//! SQLite index: note metadata, full-text search (FTS5), links, tags, revisions.
//! The Markdown files on disk are the source of truth; this index can always be
//! rebuilt from them (`mindcrab reindex`). Revisions live only here.

use crate::note::{self, Parsed};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub const SNIP_START: char = '\u{E000}';
pub const SNIP_END: char = '\u{E001}';
const MAX_REVISIONS_PER_NOTE: i64 = 100;

pub struct Index {
    conn: Connection,
}

#[derive(Debug, Clone, Serialize)]
pub struct NoteMeta {
    pub path: String,
    pub title: String,
    pub mtime: i64,
    pub size: i64,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub path: String,
    pub title: String,
    /// Snippet with matches wrapped in U+E000 / U+E001 (escape, then replace on the client).
    pub snippet: String,
    pub tags: Vec<String>,
    pub mtime: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Revision {
    pub id: i64,
    pub path: String,
    pub hash: String,
    pub created_at: i64,
    pub source: String,
    pub size: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutLink {
    pub target: String,
    pub path: Option<String>,
    pub embed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BackLink {
    pub path: String,
    pub title: String,
    pub context: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphNode {
    pub id: String,
    pub title: String,
    pub tags: Vec<String>,
    pub degree: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Graph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<(String, String)>,
}

pub fn hash(content: &str) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(content.as_bytes());
    hex::encode(&d[..10])
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS notes(
                path TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                hash TEXT NOT NULL,
                size INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                frontmatter TEXT NOT NULL,
                content TEXT NOT NULL
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(
                path UNINDEXED, title, body, tags,
                tokenize = 'unicode61 remove_diacritics 2'
            );
            CREATE TABLE IF NOT EXISTS links(
                src TEXT NOT NULL REFERENCES notes(path) ON DELETE CASCADE,
                key TEXT NOT NULL,
                raw TEXT NOT NULL,
                embed INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS links_src ON links(src);
            CREATE INDEX IF NOT EXISTS links_key ON links(key);
            CREATE TABLE IF NOT EXISTS tags(
                path TEXT NOT NULL REFERENCES notes(path) ON DELETE CASCADE,
                tag TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS tags_tag ON tags(tag);
            CREATE INDEX IF NOT EXISTS tags_path ON tags(path);
            CREATE TABLE IF NOT EXISTS files(
                path TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                size INTEGER NOT NULL,
                mtime INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS files_name ON files(name);
            CREATE TABLE IF NOT EXISTS folders(path TEXT PRIMARY KEY);
            CREATE TABLE IF NOT EXISTS revisions(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL,
                content TEXT NOT NULL,
                hash TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                source TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS revisions_path ON revisions(path, id);
            "#,
        )?;
        Ok(Self { conn })
    }

    pub fn note_state(&self, path: &str) -> Result<Option<(String, i64, i64)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT hash, size, mtime FROM notes WHERE path=?1",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }

    /// Indexes a note. If an older version exists with a different hash, it is
    /// kept as a revision attributed to `source`. Returns true if anything changed.
    pub fn upsert(
        &mut self,
        path: &str,
        content: &str,
        size: i64,
        mtime: i64,
        source: &str,
    ) -> Result<bool> {
        let new_hash = hash(content);
        let tx = self.conn.transaction()?;
        let old: Option<(String, String)> = tx
            .query_row(
                "SELECT hash, content FROM notes WHERE path=?1",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((old_hash, old_content)) = &old {
            if *old_hash == new_hash {
                tx.execute(
                    "UPDATE notes SET size=?2, mtime=?3 WHERE path=?1",
                    params![path, size, mtime],
                )?;
                tx.commit()?;
                return Ok(false);
            }
            if !old_content.is_empty() {
                tx.execute(
                    "INSERT INTO revisions(path, content, hash, created_at, source) VALUES(?1,?2,?3,?4,?5)",
                    params![path, old_content, old_hash, now_ms(), source],
                )?;
                prune_revisions(&tx, path)?;
            }
        }
        let p: Parsed = note::parse(path, content);
        let fm = serde_json::to_string(&p.frontmatter)?;
        tx.execute(
            "INSERT INTO notes(path,title,hash,size,mtime,frontmatter,content) VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(path) DO UPDATE SET title=?2,hash=?3,size=?4,mtime=?5,frontmatter=?6,content=?7",
            params![path, p.title, new_hash, size, mtime, fm, content],
        )?;
        tx.execute("DELETE FROM notes_fts WHERE path=?1", [path])?;
        tx.execute(
            "INSERT INTO notes_fts(path,title,body,tags) VALUES(?1,?2,?3,?4)",
            params![path, p.title, p.body, p.tags.join(" ")],
        )?;
        tx.execute("DELETE FROM links WHERE src=?1", [path])?;
        tx.execute("DELETE FROM tags WHERE path=?1", [path])?;
        {
            let mut ins = tx.prepare("INSERT INTO links(src,key,raw,embed) VALUES(?1,?2,?3,?4)")?;
            for l in &p.links {
                ins.execute(params![path, l.key, l.raw, l.embed])?;
            }
            let mut ins = tx.prepare("INSERT INTO tags(path,tag) VALUES(?1,?2)")?;
            for t in &p.tags {
                ins.execute(params![path, t])?;
            }
        }
        tx.commit()?;
        Ok(true)
    }

    /// Removes a note from the index, keeping its last content as a revision.
    pub fn remove(&mut self, path: &str, source: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        let old: Option<(String, String)> = tx
            .query_row(
                "SELECT hash, content FROM notes WHERE path=?1",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((h, c)) = old {
            tx.execute(
                "INSERT INTO revisions(path, content, hash, created_at, source) VALUES(?1,?2,?3,?4,?5)",
                params![path, c, h, now_ms(), format!("{source} (deleted)")],
            )?;
        }
        tx.execute("DELETE FROM notes_fts WHERE path=?1", [path])?;
        tx.execute("DELETE FROM notes WHERE path=?1", [path])?;
        tx.commit()?;
        Ok(())
    }

    /// Moves index rows (including history) to a new path.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE revisions SET path=?2 WHERE path=?1",
            params![from, to],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_file(&self, path: &str, size: i64, mtime: i64) -> Result<()> {
        let name = path.rsplit('/').next().unwrap_or(path).to_lowercase();
        self.conn.execute(
            "INSERT INTO files(path,name,size,mtime) VALUES(?1,?2,?3,?4)
             ON CONFLICT(path) DO UPDATE SET size=?3, mtime=?4",
            params![path, name, size, mtime],
        )?;
        Ok(())
    }

    pub fn remove_file(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM files WHERE path=?1", [path])?;
        Ok(())
    }

    pub fn set_folders(&mut self, dirs: &[String]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM folders", [])?;
        for d in dirs {
            tx.execute("INSERT OR IGNORE INTO folders(path) VALUES(?1)", [d])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn add_folder(&self, path: &str) -> Result<()> {
        // Register the folder and all its parents.
        let mut acc = String::new();
        for seg in path.split('/') {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(seg);
            self.conn
                .execute("INSERT OR IGNORE INTO folders(path) VALUES(?1)", [&acc])?;
        }
        Ok(())
    }

    pub fn remove_folder(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM folders WHERE path=?1", [path])?;
        Ok(())
    }

    pub fn folders(&self) -> Result<Vec<String>> {
        let mut st = self
            .conn
            .prepare("SELECT path FROM folders ORDER BY path")?;
        let rows = st
            .query_map([], |r| r.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(rows)
    }

    pub fn all_paths(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT path FROM notes")?;
        Ok(st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?)
    }

    pub fn all_file_paths(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT path FROM files")?;
        Ok(st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?)
    }

    fn tags_map(&self) -> Result<HashMap<String, Vec<String>>> {
        let mut st = self
            .conn
            .prepare("SELECT path, tag FROM tags ORDER BY tag")?;
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (p, t) = row?;
            map.entry(p).or_default().push(t);
        }
        Ok(map)
    }

    pub fn list(
        &self,
        folder: Option<&str>,
        order_recent: bool,
        limit: usize,
    ) -> Result<Vec<NoteMeta>> {
        let order = if order_recent {
            "mtime DESC"
        } else {
            "path COLLATE NOCASE"
        };
        let sql = format!(
            "SELECT path,title,mtime,size FROM notes WHERE (?1 = '' OR path LIKE ?1 || '/%') ORDER BY {order} LIMIT ?2"
        );
        let mut st = self.conn.prepare(&sql)?;
        let tags = self.tags_map()?;
        let folder = folder.unwrap_or("").trim_matches('/');
        let rows = st
            .query_map(params![folder, limit as i64], |r| {
                Ok(NoteMeta {
                    path: r.get(0)?,
                    title: r.get(1)?,
                    mtime: r.get(2)?,
                    size: r.get(3)?,
                    tags: vec![],
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .map(|mut m| {
                m.tags = tags.get(&m.path).cloned().unwrap_or_default();
                m
            })
            .collect())
    }

    pub fn count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))?)
    }

    /// Full-text search. Supports `tag:x`, `path:folder` and `"exact phrase"`.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let parsed = parse_query(query);
        let tags = self.tags_map()?;
        let mut hits = Vec::new();

        if parsed.fts.is_empty() {
            // Filter-only query: list matching notes by recency.
            for m in self.list(parsed.folder.as_deref(), true, 10_000)? {
                if parsed.tags.iter().all(|t| {
                    m.tags
                        .iter()
                        .any(|x| x == t || x.starts_with(&format!("{t}/")))
                }) {
                    hits.push(SearchHit {
                        path: m.path,
                        title: m.title,
                        snippet: String::new(),
                        tags: m.tags,
                        mtime: m.mtime,
                    });
                    if hits.len() >= limit {
                        break;
                    }
                }
            }
            return Ok(hits);
        }

        let sql = format!(
            "SELECT f.path, n.title, snippet(notes_fts, 2, '{SNIP_START}', '{SNIP_END}', '…', 16), n.mtime
             FROM notes_fts f JOIN notes n ON n.path = f.path
             WHERE notes_fts MATCH ?1 AND (?2 = '' OR f.path LIKE ?2 || '/%')
             ORDER BY bm25(notes_fts, 0.0, 8.0, 1.0, 4.0) LIMIT ?3"
        );
        let mut st = self.conn.prepare(&sql)?;
        let folder = parsed.folder.clone().unwrap_or_default();
        let rows = st.query_map(params![parsed.fts, folder, (limit * 4) as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows {
            let (path, title, snippet, mtime) = row?;
            let t = tags.get(&path).cloned().unwrap_or_default();
            if !parsed
                .tags
                .iter()
                .all(|q| t.iter().any(|x| x == q || x.starts_with(&format!("{q}/"))))
            {
                continue;
            }
            hits.push(SearchHit {
                path,
                title,
                snippet,
                tags: t,
                mtime,
            });
            if hits.len() >= limit {
                break;
            }
        }
        Ok(hits)
    }

    pub fn tag_counts(&self) -> Result<Vec<(String, i64)>> {
        let mut st = self
            .conn
            .prepare("SELECT tag, COUNT(*) c FROM tags GROUP BY tag ORDER BY c DESC, tag")?;
        Ok(st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?)
    }

    /// Lookup tables for link resolution: full key -> path, stem -> paths.
    fn resolver(&self) -> Result<Resolver> {
        let mut r = Resolver::default();
        for p in self.all_paths()? {
            r.by_key.insert(note::normalize_key(&p), p.clone());
            r.by_stem
                .entry(note::stem(&p).to_lowercase())
                .or_default()
                .push(p);
        }
        let mut st = self.conn.prepare("SELECT path, name FROM files")?;
        for row in st.query_map([], |x| Ok((x.get::<_, String>(0)?, x.get::<_, String>(1)?)))? {
            let (p, name) = row?;
            r.files_by_key.insert(p.to_lowercase(), p.clone());
            r.files_by_name.entry(name).or_default().push(p);
        }
        Ok(r)
    }

    pub fn resolve_link(&self, src: &str, target: &str) -> Result<Option<String>> {
        Ok(self.resolver()?.resolve(src, target))
    }

    pub fn resolver_snapshot(&self) -> Result<Resolver> {
        self.resolver()
    }

    pub fn outgoing(&self, path: &str) -> Result<Vec<OutLink>> {
        let r = self.resolver()?;
        let mut st = self
            .conn
            .prepare("SELECT raw, embed FROM links WHERE src=?1")?;
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for row in st.query_map([path], |x| {
            Ok((x.get::<_, String>(0)?, x.get::<_, bool>(1)?))
        })? {
            let (raw, embed) = row?;
            if !seen.insert(raw.to_lowercase()) {
                continue;
            }
            out.push(OutLink {
                path: r.resolve(path, &raw),
                target: raw,
                embed,
            });
        }
        Ok(out)
    }

    pub fn backlinks(&self, path: &str) -> Result<Vec<BackLink>> {
        let r = self.resolver()?;
        let full = note::normalize_key(path);
        let stem = note::stem(path).to_lowercase();
        let mut st = self.conn.prepare(
            "SELECT DISTINCT l.src, l.raw, n.title, n.content FROM links l JOIN notes n ON n.path = l.src
             WHERE (l.key = ?1 OR l.key = ?2 OR l.key LIKE '%/' || ?2) AND l.src != ?3",
        )?;
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for row in st.query_map(params![full, stem, path], |x| {
            Ok((
                x.get::<_, String>(0)?,
                x.get::<_, String>(1)?,
                x.get::<_, String>(2)?,
                x.get::<_, String>(3)?,
            ))
        })? {
            let (src, raw, title, content) = row?;
            if r.resolve(&src, &raw).as_deref() != Some(path) || !seen.insert(src.clone()) {
                continue;
            }
            out.push(BackLink {
                context: context_line(&content, &raw),
                path: src,
                title,
            });
        }
        Ok(out)
    }

    /// Notes that mention this note's title/stem in plain text without linking it.
    pub fn unlinked_mentions(&self, path: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let stem = note::stem(path);
        if stem.chars().count() < 3 {
            return Ok(vec![]);
        }
        let linked: HashSet<String> = self.backlinks(path)?.into_iter().map(|b| b.path).collect();
        let q = format!("\"{}\"", stem.replace('"', ""));
        Ok(self
            .search(&q, limit + linked.len() + 1)?
            .into_iter()
            .filter(|h| h.path != path && !linked.contains(&h.path))
            .take(limit)
            .collect())
    }

    pub fn graph(&self) -> Result<Graph> {
        let r = self.resolver()?;
        let tags = self.tags_map()?;
        let mut nodes: Vec<GraphNode> = Vec::new();
        let mut idx: HashMap<String, usize> = HashMap::new();
        let mut st = self.conn.prepare("SELECT path, title FROM notes")?;
        for row in st.query_map([], |x| Ok((x.get::<_, String>(0)?, x.get::<_, String>(1)?)))? {
            let (p, t) = row?;
            idx.insert(p.clone(), nodes.len());
            nodes.push(GraphNode {
                tags: tags.get(&p).cloned().unwrap_or_default(),
                id: p,
                title: t,
                degree: 0,
            });
        }
        let mut edges = HashSet::new();
        let mut st = self
            .conn
            .prepare("SELECT src, raw FROM links WHERE embed = 0")?;
        for row in st.query_map([], |x| Ok((x.get::<_, String>(0)?, x.get::<_, String>(1)?)))? {
            let (src, raw) = row?;
            if let Some(dst) = r.resolve(&src, &raw)
                && dst != src
                && idx.contains_key(&dst)
                && edges.insert((src.clone(), dst.clone()))
            {
                nodes[idx[&src]].degree += 1;
                nodes[idx[&dst]].degree += 1;
            }
        }
        Ok(Graph {
            nodes,
            edges: edges.into_iter().collect(),
        })
    }

    pub fn revisions(&self, path: &str) -> Result<Vec<Revision>> {
        let mut st = self.conn.prepare(
            "SELECT id, path, hash, created_at, source, length(content) FROM revisions WHERE path=?1 ORDER BY id DESC",
        )?;
        Ok(st
            .query_map([path], |r| {
                Ok(Revision {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    hash: r.get(2)?,
                    created_at: r.get(3)?,
                    source: r.get(4)?,
                    size: r.get(5)?,
                })
            })?
            .collect::<Result<_, _>>()?)
    }

    pub fn revision(&self, id: i64) -> Result<Option<(String, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT path, content FROM revisions WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }
}

fn prune_revisions(tx: &rusqlite::Transaction, path: &str) -> Result<()> {
    tx.execute(
        "DELETE FROM revisions WHERE path=?1 AND id NOT IN (SELECT id FROM revisions WHERE path=?1 ORDER BY id DESC LIMIT ?2)",
        params![path, MAX_REVISIONS_PER_NOTE],
    )?;
    Ok(())
}

fn context_line(content: &str, raw: &str) -> String {
    let needle = raw.to_lowercase();
    let line = content
        .lines()
        .find(|l| l.to_lowercase().contains(&needle))
        .unwrap_or("")
        .trim();
    let mut s: String = line.chars().take(200).collect();
    if line.chars().count() > 200 {
        s.push('…');
    }
    s
}

#[derive(Default)]
pub struct Resolver {
    by_key: HashMap<String, String>,
    by_stem: HashMap<String, Vec<String>>,
    files_by_key: HashMap<String, String>,
    files_by_name: HashMap<String, Vec<String>>,
}

impl Resolver {
    /// Resolves a link target the way Obsidian does: exact path first, then
    /// by file name (preferring the linking note's folder, then the shortest path).
    pub fn resolve(&self, src: &str, target: &str) -> Option<String> {
        let joined;
        let mut target = target.trim();
        if target.starts_with("./") || target.starts_with("../") {
            joined = note::join_relative(src, target);
            target = &joined;
        }
        let lower = target.to_lowercase();
        let ext = lower
            .rsplit_once('.')
            .map(|(_, e)| e)
            .filter(|e| *e != "md" && e.len() <= 5);
        let src_dir = note::parent_dir(src);

        if ext.is_some() {
            let k = lower.trim_start_matches("./").trim_start_matches('/');
            if let Some(p) = self.files_by_key.get(k) {
                return Some(p.clone());
            }
            let rel = if src_dir.is_empty() {
                k.to_string()
            } else {
                format!("{}/{k}", src_dir.to_lowercase())
            };
            if let Some(p) = self.files_by_key.get(&rel) {
                return Some(p.clone());
            }
            let name = k.rsplit('/').next().unwrap_or(k);
            return pick(self.files_by_name.get(name)?, src_dir);
        }

        let key = note::normalize_key(target);
        if let Some(p) = self.by_key.get(&key) {
            return Some(p.clone());
        }
        if !src_dir.is_empty()
            && let Some(p) = self
                .by_key
                .get(&format!("{}/{key}", src_dir.to_lowercase()))
        {
            return Some(p.clone());
        }
        let stem = key.rsplit('/').next().unwrap_or(&key);
        let cands = self.by_stem.get(stem)?;
        let cands: Vec<String> = if key.contains('/') {
            cands
                .iter()
                .filter(|p| note::normalize_key(p).ends_with(&key))
                .cloned()
                .collect()
        } else {
            cands.clone()
        };
        pick(&cands, src_dir)
    }
}

fn pick(cands: &[String], src_dir: &str) -> Option<String> {
    if cands.is_empty() {
        return None;
    }
    if let Some(p) = cands.iter().find(|p| note::parent_dir(p) == src_dir) {
        return Some(p.clone());
    }
    cands
        .iter()
        .min_by_key(|p| (p.len(), (*p).clone()))
        .cloned()
}

#[derive(Debug, Default)]
struct ParsedQuery {
    fts: String,
    tags: Vec<String>,
    folder: Option<String>,
}

/// Turns free text into a safe FTS5 expression (each term quoted, prefix match on the last).
fn parse_query(q: &str) -> ParsedQuery {
    let mut out = ParsedQuery::default();
    let mut terms: Vec<String> = Vec::new();
    let mut chars = q.chars().peekable();
    let mut buf = String::new();
    let flush = |buf: &mut String, out: &mut ParsedQuery, terms: &mut Vec<String>, phrase: bool| {
        let t = std::mem::take(buf);
        let t = t.trim();
        if t.is_empty() {
            return;
        }
        if !phrase {
            if let Some(tag) = t.strip_prefix("tag:").or_else(|| t.strip_prefix('#')) {
                if !tag.is_empty() {
                    out.tags.push(tag.to_lowercase());
                }
                return;
            }
            if let Some(f) = t.strip_prefix("path:") {
                out.folder = Some(f.trim_matches('/').to_string());
                return;
            }
        }
        let clean: String = t.chars().filter(|c| *c != '"').collect();
        if clean.is_empty() {
            return;
        }
        terms.push(if phrase {
            format!("\"{clean}\"")
        } else {
            format!("\"{clean}\"*")
        });
    };
    while let Some(c) = chars.next() {
        if c == '"' {
            flush(&mut buf, &mut out, &mut terms, false);
            for c2 in chars.by_ref() {
                if c2 == '"' {
                    break;
                }
                buf.push(c2);
            }
            flush(&mut buf, &mut out, &mut terms, true);
        } else if c.is_whitespace() {
            flush(&mut buf, &mut out, &mut terms, false);
        } else {
            buf.push(c);
        }
    }
    flush(&mut buf, &mut out, &mut terms, false);
    out.fts = terms.join(" ");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx() -> (tempfile::TempDir, Index) {
        let d = tempfile::tempdir().unwrap();
        let i = Index::open(&d.path().join("i.db")).unwrap();
        (d, i)
    }

    #[test]
    fn query_parsing() {
        let q = parse_query(r#"rust tag:Proje "tam ifade" path:Notlar/ ""#);
        assert_eq!(q.fts, r#""rust"* "tam ifade""#);
        assert_eq!(q.tags, vec!["proje"]);
        assert_eq!(q.folder.as_deref(), Some("Notlar"));
    }

    #[test]
    fn hostile_queries_do_not_error() {
        let (_d, mut i) = idx();
        i.upsert("A.md", "hello world", 1, 1, "t").unwrap();
        for q in [
            "a\"b OR ) (",
            "*",
            "NEAR(",
            "-x",
            "^",
            "\"",
            "tag:",
            "path:",
            "a:b",
        ] {
            assert!(i.search(q, 10).is_ok(), "query failed: {q}");
        }
    }

    #[test]
    fn upsert_search_links_revisions() {
        let (_d, mut i) = idx();
        i.upsert("A.md", "# Alpha\nlinks to [[B]] #proje", 1, 1, "test")
            .unwrap();
        i.upsert("dir/B.md", "Bravo şeker text", 1, 1, "test")
            .unwrap();
        assert_eq!(i.search("seker", 10).unwrap()[0].path, "dir/B.md");
        assert_eq!(i.search("alp", 10).unwrap()[0].title, "Alpha");
        assert_eq!(i.search("tag:proje", 10).unwrap().len(), 1);
        let bl = i.backlinks("dir/B.md").unwrap();
        assert_eq!(bl.len(), 1);
        assert_eq!(bl[0].path, "A.md");
        assert_eq!(
            i.outgoing("A.md").unwrap()[0].path.as_deref(),
            Some("dir/B.md")
        );
        assert_eq!(i.graph().unwrap().edges.len(), 1);

        assert!(
            !i.upsert("A.md", "# Alpha\nlinks to [[B]] #proje", 1, 2, "test")
                .unwrap()
        );
        assert!(i.upsert("A.md", "changed", 1, 3, "mcp:test").unwrap());
        let revs = i.revisions("A.md").unwrap();
        assert_eq!(revs.len(), 1);
        assert_eq!(revs[0].source, "mcp:test");
        assert!(i.revision(revs[0].id).unwrap().unwrap().1.contains("Alpha"));
        i.remove("A.md", "web").unwrap();
        assert_eq!(i.revisions("A.md").unwrap().len(), 2);
        assert!(i.backlinks("dir/B.md").unwrap().is_empty());
    }

    #[test]
    fn resolver_prefers_same_folder() {
        let (_d, mut i) = idx();
        i.upsert("x/N.md", "", 0, 0, "t").unwrap();
        i.upsert("y/N.md", "", 0, 0, "t").unwrap();
        i.upsert("y/src.md", "[[N]]", 0, 0, "t").unwrap();
        assert_eq!(
            i.resolve_link("y/src.md", "N").unwrap().as_deref(),
            Some("y/N.md")
        );
        assert_eq!(
            i.resolve_link("top.md", "x/N").unwrap().as_deref(),
            Some("x/N.md")
        );
        i.upsert_file("attachments/pic.png", 1, 1).unwrap();
        assert_eq!(
            i.resolve_link("y/src.md", "pic.png").unwrap().as_deref(),
            Some("attachments/pic.png")
        );
    }
}
