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

/// Bump when derived data changes shape; forces a full re-read of the vault.
const SCHEMA_VERSION: i64 = 5;

/// Web autosaves within this window share one revision (the state before the editing session).
pub const COALESCE_MS: i64 = 5 * 60 * 1000;

/// Everything derived from one note's text — computed without touching the database,
/// so many notes can be prepared in parallel.
pub struct Prepared {
    pub path: String,
    pub content: String,
    pub hash: String,
    pub size: i64,
    pub mtime: i64,
    parsed: Parsed,
    fm_json: String,
    private: bool,
    tasks: Vec<crate::tasks::Task>,
    chunks: Option<Vec<String>>,
}

pub fn prepare(path: &str, content: String, size: i64, mtime: i64, chunking: bool) -> Prepared {
    let parsed = note::parse(path, &content);
    let fm_json = serde_json::to_string(&parsed.frontmatter).unwrap_or_else(|_| "{}".into());
    let private = crate::scope::frontmatter_private(&parsed.frontmatter);
    let tasks = crate::tasks::parse(&content);
    let chunks = chunking.then(|| crate::embed::chunk(&parsed.title, &parsed.body));
    Prepared {
        path: path.to_string(),
        hash: hash(&content),
        content,
        size,
        mtime,
        parsed,
        fm_json,
        private,
        tasks,
        chunks,
    }
}

pub struct Index {
    pub(crate) conn: Connection,
    /// Store text chunks for semantic search (only when an embedder is configured).
    pub chunking: bool,
    /// Bumped when the set of note/file paths changes (invalidates the link resolver cache).
    paths_gen: std::cell::Cell<u64>,
    resolver_cache: std::cell::RefCell<Option<(u64, std::sync::Arc<Resolver>)>>,
    /// Set when the on-disk schema is older than this build (full reindex needed).
    pub needs_reindex: bool,
    /// Bumped whenever stored vectors may have changed (for the in-memory vector cache).
    pub vec_gen: u64,
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
    /// Found by meaning (vector search) rather than by words.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub semantic: bool,
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
        conn.pragma_update(None, "cache_size", -16000)?; // ~16 MB page cache
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.pragma_update(None, "mmap_size", 64 * 1024 * 1024)?;
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
            CREATE TABLE IF NOT EXISTS chunks(
                path TEXT NOT NULL REFERENCES notes(path) ON DELETE CASCADE,
                idx INTEGER NOT NULL,
                hash TEXT NOT NULL,
                text TEXT NOT NULL,
                vec BLOB,
                PRIMARY KEY(path, idx)
            );
            CREATE INDEX IF NOT EXISTS chunks_pending ON chunks(path) WHERE vec IS NULL;
            CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS tasks(
                path TEXT NOT NULL REFERENCES notes(path) ON DELETE CASCADE,
                line INTEGER NOT NULL,
                text TEXT NOT NULL,
                raw TEXT NOT NULL,
                done INTEGER NOT NULL,
                due TEXT,
                scheduled TEXT,
                done_date TEXT,
                priority INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS tasks_path ON tasks(path);
            CREATE TABLE IF NOT EXISTS activity(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL,
                source TEXT NOT NULL,
                kind TEXT NOT NULL,
                path TEXT NOT NULL,
                rev INTEGER,
                floor INTEGER NOT NULL,
                detail TEXT
            );
            CREATE TABLE IF NOT EXISTS pending_facts(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL,
                source TEXT NOT NULL,
                topic TEXT,
                fact TEXT NOT NULL,
                tags TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS connections(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                token_hash TEXT NOT NULL UNIQUE,
                token_hint TEXT NOT NULL,
                read_only INTEGER NOT NULL,
                review INTEGER NOT NULL,
                folders TEXT NOT NULL,
                created INTEGER NOT NULL,
                last_used INTEGER
            );
            "#,
        )?;
        // Migrations for databases created by older versions.
        let has_col = |table: &str, col: &str| -> Result<bool> {
            let mut st = conn.prepare(&format!("PRAGMA table_info({table})"))?;
            let cols = st
                .query_map([], |r| r.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(cols.iter().any(|c| c == col))
        };
        if !has_col("notes", "private")? {
            conn.execute(
                "ALTER TABLE notes ADD COLUMN private INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        if !has_col("links", "stem")? {
            conn.execute(
                "ALTER TABLE links ADD COLUMN stem TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS links_stem ON links(stem);
             -- Listings read only metadata; without these they would page through every note body.
             CREATE INDEX IF NOT EXISTS notes_meta ON notes(path, title, mtime, size, private);
             CREATE INDEX IF NOT EXISTS notes_recent ON notes(mtime DESC, path, title, size);
             CREATE INDEX IF NOT EXISTS notes_private ON notes(path) WHERE private = 1;
             CREATE INDEX IF NOT EXISTS tags_path_tag ON tags(path, tag);
             DROP INDEX IF EXISTS tags_path;",
        )?;
        let ver: i64 = conn
            .query_row("SELECT value FROM meta WHERE key='schema'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let empty: i64 = conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))?;
        let mut me = Self {
            conn,
            chunking: false,
            paths_gen: std::cell::Cell::new(0),
            resolver_cache: std::cell::RefCell::new(None),
            vec_gen: 0,
            needs_reindex: ver < SCHEMA_VERSION,
        };
        if empty == 0 && me.needs_reindex {
            me.mark_reindexed()?; // fresh database: nothing to migrate
        }
        me.ensure_store_indexes()?;
        Ok(me)
    }

    pub fn mark_reindexed(&mut self) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key,value) VALUES('schema',?1) ON CONFLICT(key) DO UPDATE SET value=?1",
            [SCHEMA_VERSION.to_string()],
        )?;
        self.needs_reindex = false;
        Ok(())
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

    /// (size, mtime) of every indexed note, so a rescan can spot changes with one query.
    pub fn all_note_states(&self) -> Result<HashMap<String, (i64, i64)>> {
        let mut st = self.conn.prepare("SELECT path, size, mtime FROM notes")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?),
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// (size, mtime) of every indexed attachment.
    pub fn all_file_states(&self) -> Result<HashMap<String, (i64, i64)>> {
        let mut st = self.conn.prepare("SELECT path, size, mtime FROM files")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?),
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Applies attachment changes found by a rescan in one transaction: `upsert` holds
    /// (path, size, mtime) of new or changed files, `remove` the paths that are gone.
    pub fn apply_file_changes(
        &mut self,
        upsert: &[(String, i64, i64)],
        remove: &[String],
    ) -> Result<()> {
        if upsert.is_empty() && remove.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        let mut new_paths = false;
        {
            let mut ins = tx.prepare_cached(
                "INSERT OR IGNORE INTO files(path,name,size,mtime) VALUES(?1,?2,?3,?4)",
            )?;
            let mut upd = tx.prepare_cached("UPDATE files SET size=?2, mtime=?3 WHERE path=?1")?;
            for (path, size, mtime) in upsert {
                let name = path
                    .rsplit('/')
                    .next()
                    .unwrap_or(path.as_str())
                    .to_lowercase();
                if ins.execute(params![path, name, size, mtime])? > 0 {
                    new_paths = true;
                } else {
                    upd.execute(params![path, size, mtime])?;
                }
            }
            let mut del = tx.prepare_cached("DELETE FROM files WHERE path=?1")?;
            for p in remove {
                new_paths |= del.execute([p])? > 0;
            }
        }
        tx.commit()?;
        if new_paths {
            self.paths_gen.set(self.paths_gen.get() + 1);
        }
        Ok(())
    }

    /// Moves a note's rows to `p.path` without losing its embedding vectors (indexing it
    /// afresh under the new name would re-embed the whole note). Title, links, tags, tasks
    /// and search text are rebuilt for the new path; history moves with [`Index::rename`].
    pub fn move_note(&mut self, from: &str, p: &Prepared, source: &str) -> Result<()> {
        let to = p.path.as_str();
        let tx = self.conn.transaction()?;
        // A leftover row for the target (its file is gone) would block the move.
        tx.execute("DELETE FROM notes_fts WHERE path=?1", [to])?;
        tx.execute("DELETE FROM notes WHERE path=?1", [to])?;
        tx.execute(
            "INSERT INTO notes(path,title,hash,size,mtime,frontmatter,content,private)
             SELECT ?2,title,hash,size,mtime,frontmatter,content,private FROM notes WHERE path=?1",
            params![from, to],
        )?;
        tx.execute("UPDATE chunks SET path=?2 WHERE path=?1", params![from, to])?;
        tx.execute("DELETE FROM notes_fts WHERE path=?1", [from])?;
        tx.execute("DELETE FROM notes WHERE path=?1", [from])?;
        // Forced so everything derived from the path is rebuilt; chunks whose text did not
        // change keep their vectors.
        Self::apply(&tx, p, source, true)?;
        tx.commit()?;
        self.after_apply(true, true);
        Ok(())
    }

    /// Notes that carry every tag in `tags`, where a tag also covers its nested tags
    /// (`a` matches `a/b`) — the same rule as the `tag:` search filter.
    pub fn paths_with_tags(&self, tags: &[String]) -> Result<HashSet<String>> {
        let mut st = self.conn.prepare_cached(
            "SELECT DISTINCT path FROM tags WHERE tag = ?1 OR (tag >= ?1 || '/' AND tag < ?1 || '0')",
        )?;
        let mut out: Option<HashSet<String>> = None;
        for t in tags {
            let found = st
                .query_map([t], |r| r.get::<_, String>(0))?
                .collect::<Result<HashSet<_>, _>>()?;
            out = Some(match out {
                None => found,
                Some(mut prev) => {
                    prev.retain(|p| found.contains(p));
                    prev
                }
            });
        }
        Ok(out.unwrap_or_default())
    }

    /// Paths of notes whose indexed text passes `keep` (one pass over all notes).
    pub fn note_paths_where(&self, mut keep: impl FnMut(&str) -> bool) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT path, content FROM notes")?;
        let mut rows = st.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            if keep(r.get_ref(1)?.as_str()?) {
                out.push(r.get(0)?);
            }
        }
        Ok(out)
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
        // Cheap no-op check before parsing anything.
        if !self.needs_reindex {
            let old: Option<String> = self
                .conn
                .query_row("SELECT hash FROM notes WHERE path=?1", [path], |r| r.get(0))
                .optional()?;
            if old.as_deref() == Some(hash(content).as_str()) {
                self.conn.execute(
                    "UPDATE notes SET size=?2, mtime=?3 WHERE path=?1",
                    params![path, size, mtime],
                )?;
                return Ok(false);
            }
        }
        let p = prepare(path, content.to_string(), size, mtime, self.chunking);
        let tx = self.conn.transaction()?;
        let (changed, new_path) = Self::apply(&tx, &p, source, self.needs_reindex)?;
        tx.commit()?;
        self.after_apply(changed, new_path);
        Ok(changed)
    }

    /// Indexes many prepared notes in one transaction (initial scan, rescans).
    pub fn upsert_many(&mut self, items: &[Prepared], source: &str) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut changed = 0;
        let mut new_paths = false;
        for p in items {
            let (c, n) = Self::apply(&tx, p, source, self.needs_reindex)?;
            changed += c as usize;
            new_paths |= n;
        }
        tx.commit()?;
        self.after_apply(changed > 0, new_paths);
        Ok(changed)
    }

    fn after_apply(&mut self, changed: bool, new_path: bool) {
        if changed {
            self.vec_gen += 1;
        }
        if new_path {
            self.paths_gen.set(self.paths_gen.get() + 1);
        }
    }

    /// Writes one prepared note. Returns (changed, path was new).
    fn apply(
        tx: &rusqlite::Transaction,
        p: &Prepared,
        source: &str,
        force: bool,
    ) -> Result<(bool, bool)> {
        let path = p.path.as_str();
        let old: Option<(String, String)> = tx
            .query_row(
                "SELECT hash, content FROM notes WHERE path=?1",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((old_hash, old_content)) = &old {
            if *old_hash == p.hash && !force {
                tx.execute(
                    "UPDATE notes SET size=?2, mtime=?3 WHERE path=?1",
                    params![path, p.size, p.mtime],
                )?;
                return Ok((false, false));
            }
            if !old_content.is_empty() && *old_hash != p.hash && !coalesce(tx, path, source)? {
                tx.execute(
                    "INSERT INTO revisions(path, content, hash, created_at, source) VALUES(?1,?2,?3,?4,?5)",
                    params![path, old_content, old_hash, now_ms(), source],
                )?;
                prune_revisions(tx, path)?;
            }
        }
        let pa = &p.parsed;
        tx.execute(
            "INSERT INTO notes(path,title,hash,size,mtime,frontmatter,content,private) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(path) DO UPDATE SET title=?2,hash=?3,size=?4,mtime=?5,frontmatter=?6,content=?7,private=?8",
            params![path, pa.title, p.hash, p.size, p.mtime, p.fm_json, p.content, p.private],
        )?;
        if old.is_some() {
            tx.execute("DELETE FROM tasks WHERE path=?1", [path])?;
            tx.execute("DELETE FROM notes_fts WHERE path=?1", [path])?;
            tx.execute("DELETE FROM links WHERE src=?1", [path])?;
            tx.execute("DELETE FROM tags WHERE path=?1", [path])?;
        }
        tx.execute(
            "INSERT INTO notes_fts(path,title,body,tags) VALUES(?1,?2,?3,?4)",
            params![path, pa.title, pa.body, pa.tags.join(" ")],
        )?;
        {
            let mut ins = tx.prepare_cached(
                "INSERT INTO tasks(path,line,text,raw,done,due,scheduled,done_date,priority) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            )?;
            for t in &p.tasks {
                ins.execute(params![
                    path,
                    t.line as i64,
                    t.text,
                    t.raw,
                    t.done,
                    t.due,
                    t.scheduled,
                    t.done_date,
                    t.priority
                ])?;
            }
            let mut ins = tx.prepare_cached(
                "INSERT INTO links(src,key,raw,embed,stem) VALUES(?1,?2,?3,?4,?5)",
            )?;
            for l in &pa.links {
                let stem = l.key.rsplit('/').next().unwrap_or(&l.key);
                ins.execute(params![path, l.key, l.raw, l.embed, stem])?;
            }
            let mut ins = tx.prepare_cached("INSERT INTO tags(path,tag) VALUES(?1,?2)")?;
            for t in &pa.tags {
                ins.execute(params![path, t])?;
            }
        }
        if let Some(chunks) = &p.chunks {
            write_chunks(tx, path, chunks)?;
        }
        Ok((true, old.is_none()))
    }

    /// Creates chunks for notes that have none (semantic search was just turned on). After a
    /// chunker change, or a run without an embedder (notes may have changed meanwhile), every
    /// note is re-chunked once instead; chunks whose text is unchanged keep their vectors.
    pub fn rechunk_missing(&mut self) -> Result<usize> {
        let version: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key='chunker'", [], |r| {
                r.get(0)
            })
            .optional()?;
        let all = version.as_deref() != Some(crate::embed::CHUNKER_VERSION);
        let rows: Vec<(String, String)> = {
            let mut st = self.conn.prepare(if all {
                "SELECT path, content FROM notes"
            } else {
                "SELECT path, content FROM notes WHERE path NOT IN (SELECT DISTINCT path FROM chunks)"
            })?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?
        };
        let tx = self.conn.transaction()?;
        for (path, content) in &rows {
            let p = note::parse(path, content);
            write_chunks(&tx, path, &crate::embed::chunk(&p.title, &p.body))?;
        }
        tx.execute(
            "INSERT INTO meta(key,value) VALUES('chunker',?1) ON CONFLICT(key) DO UPDATE SET value=?1",
            [crate::embed::CHUNKER_VERSION],
        )?;
        tx.commit()?;
        self.vec_gen += 1;
        Ok(rows.len())
    }

    /// Without an embedder, chunks still waiting for a vector are dead weight; embedded ones
    /// are kept for when semantic search comes back. Chunks aren't kept up to date meanwhile,
    /// so the next run with an embedder re-chunks every note (see `rechunk_missing`).
    pub fn drop_pending_chunks(&self) -> Result<usize> {
        self.conn
            .execute("DELETE FROM meta WHERE key='chunker'", [])?;
        Ok(self
            .conn
            .execute("DELETE FROM chunks WHERE vec IS NULL", [])?)
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
        self.vec_gen += 1;
        self.paths_gen.set(self.paths_gen.get() + 1);
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
        let added = self.conn.execute(
            "INSERT OR IGNORE INTO files(path,name,size,mtime) VALUES(?1,?2,?3,?4)",
            params![path, name, size, mtime],
        )?;
        if added > 0 {
            self.paths_gen.set(self.paths_gen.get() + 1);
        } else {
            self.conn.execute(
                "UPDATE files SET size=?2, mtime=?3 WHERE path=?1 AND (size!=?2 OR mtime!=?3)",
                params![path, size, mtime],
            )?;
        }
        Ok(())
    }

    pub fn remove_file(&self, path: &str) -> Result<()> {
        if self
            .conn
            .execute("DELETE FROM files WHERE path=?1", [path])?
            > 0
        {
            self.paths_gen.set(self.paths_gen.get() + 1);
        }
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
        let mut st = self.conn.prepare("SELECT path FROM notes ORDER BY path")?;
        Ok(st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?)
    }

    pub fn all_file_paths(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT path FROM files ORDER BY path")?;
        Ok(st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?)
    }

    fn tags_map(&self) -> Result<HashMap<String, Vec<String>>> {
        let mut st = self
            .conn
            .prepare("SELECT path, tag FROM tags ORDER BY path, tag")?;
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
            "SELECT path,title,mtime,size,{TAGS_SQL} FROM notes n WHERE (?1 = '' OR path LIKE ?1 ESCAPE '\\') ORDER BY {order} LIMIT ?2"
        );
        let big = limit > 500;
        let sql = if big {
            sql.replace(&format!(",{TAGS_SQL}"), ",NULL")
        } else {
            sql
        };
        let mut st = self.conn.prepare(&sql)?;
        let folder = folder_pattern(folder.unwrap_or(""));
        let rows = st
            .query_map(params![folder, limit as i64], |r| {
                Ok(NoteMeta {
                    path: r.get(0)?,
                    title: r.get(1)?,
                    mtime: r.get(2)?,
                    size: r.get(3)?,
                    tags: split_tags(r.get(4)?),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if !big {
            return Ok(rows);
        }
        // One pass over the tag table beats thousands of correlated lookups.
        let mut tags = self.tags_map()?;
        Ok(rows
            .into_iter()
            .map(|mut m| {
                m.tags = tags.remove(&m.path).unwrap_or_default();
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
        let folder = folder_pattern(parsed.folder.as_deref().unwrap_or(""));
        let limit = limit as i64;
        // Tag filters run in SQL, so LIMIT never cuts off the notes carrying a rare tag.
        let mut args: Vec<&dyn rusqlite::ToSql> = Vec::new();
        let sql = if parsed.fts.is_empty() {
            // Filter-only query: list matching notes by recency.
            args.push(&folder);
            args.push(&limit);
            format!(
                "SELECT n.path, n.title, '', n.mtime, {TAGS_SQL} FROM notes n
                 WHERE (?1 = '' OR n.path LIKE ?1 ESCAPE '\\'){}
                 ORDER BY n.mtime DESC LIMIT ?2",
                tag_filter_sql(3, parsed.tags.len())
            )
        } else {
            args.push(&parsed.fts);
            args.push(&folder);
            args.push(&limit);
            format!(
                "SELECT f.path, n.title, snippet(notes_fts, 2, '{SNIP_START}', '{SNIP_END}', '…', 16), n.mtime, {TAGS_SQL}
                 FROM notes_fts f JOIN notes n ON n.path = f.path
                 WHERE notes_fts MATCH ?1 AND (?2 = '' OR f.path LIKE ?2 ESCAPE '\\'){}
                 ORDER BY bm25(notes_fts, 0.0, 8.0, 1.0, 4.0) LIMIT ?3",
                tag_filter_sql(4, parsed.tags.len())
            )
        };
        for t in &parsed.tags {
            args.push(t);
        }
        let mut st = self.conn.prepare(&sql)?;
        let hits = st
            .query_map(args.as_slice(), |r| {
                Ok(SearchHit {
                    path: r.get(0)?,
                    title: r.get(1)?,
                    snippet: r.get(2)?,
                    tags: split_tags(r.get(4)?),
                    mtime: r.get(3)?,
                    semantic: false,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
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
    /// Link resolver, rebuilt only when the set of paths changed.
    fn resolver(&self) -> Result<std::sync::Arc<Resolver>> {
        let cur = self.paths_gen.get();
        if let Some((g, r)) = &*self.resolver_cache.borrow()
            && *g == cur
        {
            return Ok(r.clone());
        }
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
        let r = std::sync::Arc::new(r);
        *self.resolver_cache.borrow_mut() = Some((cur, r.clone()));
        Ok(r)
    }

    pub fn resolve_link(&self, src: &str, target: &str) -> Result<Option<String>> {
        Ok(self.resolver()?.resolve(src, target))
    }

    pub fn resolver_snapshot(&self) -> Result<std::sync::Arc<Resolver>> {
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
        let stem = note::stem(path).to_lowercase();
        // Every link that could resolve here ends in this note's file name (indexed column).
        let mut st = self.conn.prepare(
            "SELECT DISTINCT l.src, l.raw, n.title, n.content FROM links l JOIN notes n ON n.path = l.src
             WHERE l.stem = ?1 AND l.src != ?2",
        )?;
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for row in st.query_map(params![stem, path], |x| {
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
        let mut edges: Vec<_> = edges.into_iter().collect();
        edges.sort_unstable(); // stable output → stable ETag
        Ok(Graph { nodes, edges })
    }

    // ------------------------------------------------------------ vectors

    /// Records which embedding model the stored vectors belong to; wipes them if it changed.
    pub fn ensure_vector_space(&mut self, fingerprint: &str) -> Result<bool> {
        let cur: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key='vector_space'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if cur.as_deref() == Some(fingerprint) {
            return Ok(false);
        }
        self.conn.execute("UPDATE chunks SET vec=NULL", [])?;
        self.conn.execute(
            "INSERT INTO meta(key,value) VALUES('vector_space',?1) ON CONFLICT(key) DO UPDATE SET value=?1",
            [fingerprint],
        )?;
        self.vec_gen += 1;
        Ok(cur.is_some())
    }

    /// Chunks that still need a vector: (path, idx, hash, text). Chunks the API rejected
    /// hold an empty vector, so they aren't retried until their text changes.
    pub fn pending_chunks(&self, limit: usize) -> Result<Vec<(String, i64, String, String)>> {
        let mut st = self.conn.prepare("SELECT path, idx, hash, text FROM chunks WHERE vec IS NULL ORDER BY path, idx LIMIT ?1")?;
        Ok(st
            .query_map([limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<Result<_, _>>()?)
    }

    /// Stores vectors; rows whose chunk changed meanwhile (hash mismatch) are skipped.
    /// An empty vector marks a chunk the API rejects: no longer pending, never searched.
    pub fn store_vectors(&mut self, rows: &[(String, i64, String, Vec<f32>)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut st =
                tx.prepare("UPDATE chunks SET vec=?4 WHERE path=?1 AND idx=?2 AND hash=?3")?;
            for (p, i, h, v) in rows {
                st.execute(params![p, i, h, crate::embed::to_blob(v)])?;
            }
        }
        tx.commit()?;
        self.vec_gen += 1;
        Ok(())
    }

    /// (chunks done — embedded, or skipped as rejected —, all chunks)
    pub fn embed_stats(&self) -> Result<(i64, i64)> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(vec), COUNT(*) FROM chunks", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?)
    }

    pub fn load_vectors(&self) -> Result<Vec<(String, i64, Vec<f32>)>> {
        let mut st = self
            .conn
            .prepare("SELECT path, idx, vec FROM chunks WHERE length(vec) > 0")?;
        Ok(st
            .query_map([], |r| {
                let b: Vec<u8> = r.get(2)?;
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    crate::embed::from_blob(&b),
                ))
            })?
            .collect::<Result<_, _>>()?)
    }

    pub fn chunk_text(&self, path: &str, idx: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT text FROM chunks WHERE path=?1 AND idx=?2",
                params![path, idx],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn title_mtime_tags(&self, path: &str) -> Result<Option<(String, i64, Vec<String>)>> {
        let row: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT title, mtime FROM notes WHERE path=?1",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((t, m)) = row else { return Ok(None) };
        let mut st = self
            .conn
            .prepare("SELECT tag FROM tags WHERE path=?1 ORDER BY tag")?;
        let tags = st
            .query_map([path], |r| r.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(Some((t, m, tags)))
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

/// True when the latest revision of `path` came from the same web editing session,
/// so another autosave doesn't need its own snapshot.
fn coalesce(tx: &rusqlite::Transaction, path: &str, source: &str) -> Result<bool> {
    if source != "web" {
        return Ok(false);
    }
    let last: Option<(String, i64)> = tx
        .query_row(
            "SELECT source, created_at FROM revisions WHERE path=?1 ORDER BY id DESC LIMIT 1",
            [path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(matches!(last, Some((s, t)) if s == "web" && now_ms() - t < COALESCE_MS))
}

/// Stores chunks, keeping the vectors of chunks whose text didn't change — wherever they
/// moved, so text inserted near the top doesn't re-embed everything below it.
fn write_chunks(tx: &rusqlite::Transaction, path: &str, chunks: &[String]) -> Result<()> {
    let mut old_hashes: HashMap<i64, String> = HashMap::new();
    let mut old_vecs: HashMap<String, Vec<u8>> = HashMap::new();
    {
        let mut q = tx.prepare_cached("SELECT idx, hash, vec FROM chunks WHERE path=?1")?;
        for row in q.query_map([path], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<Vec<u8>>>(2)?,
            ))
        })? {
            let (i, h, v) = row?;
            if let Some(v) = v {
                old_vecs.insert(h.clone(), v);
            }
            old_hashes.insert(i, h);
        }
    }
    let mut put = tx.prepare_cached(
        "INSERT INTO chunks(path,idx,hash,text,vec) VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(path,idx) DO UPDATE SET hash=?3, text=?4, vec=?5",
    )?;
    for (i, c) in chunks.iter().enumerate() {
        let h = hash(c);
        if old_hashes.get(&(i as i64)) != Some(&h) {
            put.execute(params![path, i as i64, h, c, old_vecs.get(&h)])?;
        }
    }
    tx.execute(
        "DELETE FROM chunks WHERE path=?1 AND idx>=?2",
        params![path, chunks.len() as i64],
    )?;
    Ok(())
}

/// Sorted tags of note `n`, joined with U+001F.
const TAGS_SQL: &str = "(SELECT group_concat(tag, char(31)) FROM (SELECT tag FROM tags t WHERE t.path = n.path ORDER BY tag))";

fn split_tags(s: Option<String>) -> Vec<String> {
    s.map(|s| s.split('\u{1f}').map(str::to_string).collect())
        .unwrap_or_default()
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
        let src_dir = note::parent_dir(src);

        // A dotted name is an attachment (`pic.png`) or a note (`v1.2`, `Node.js`):
        // like Obsidian, try the file first, then fall back to notes.
        let name = lower.rsplit('/').next().unwrap_or(&lower);
        if name.rsplit_once('.').is_some_and(|(_, e)| e != "md")
            && let Some(p) = self.resolve_file(&lower, src_dir)
        {
            return Some(p);
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
                .filter(|p| is_path_suffix(&note::normalize_key(p), &key))
                .cloned()
                .collect()
        } else {
            cands.clone()
        };
        pick(&cands, src_dir)
    }

    /// Attachment lookup for a lowercased target with an extension.
    fn resolve_file(&self, lower: &str, src_dir: &str) -> Option<String> {
        let k = lower.trim_start_matches("./").trim_start_matches('/');
        if let Some(p) = self.files_by_key.get(k) {
            return Some(p.clone());
        }
        if !src_dir.is_empty()
            && let Some(p) = self
                .files_by_key
                .get(&format!("{}/{k}", src_dir.to_lowercase()))
        {
            return Some(p.clone());
        }
        let name = k.rsplit('/').next().unwrap_or(k);
        let cands = self.files_by_name.get(name)?;
        if !k.contains('/') {
            return pick(cands, src_dir);
        }
        let cands: Vec<String> = cands
            .iter()
            .filter(|p| is_path_suffix(&p.to_lowercase(), k))
            .cloned()
            .collect();
        pick(&cands, src_dir)
    }
}

/// Whether `path` is `key` or ends with `/key`: `[[ar/Not]]` must not match `bar/Not.md`.
fn is_path_suffix(path: &str, key: &str) -> bool {
    path.strip_suffix(key)
        .is_some_and(|rest| rest.is_empty() || rest.ends_with('/'))
}

/// LIKE pattern for the paths inside `folder` ("" for no folder), with `%`, `_` and `\`
/// in the name taken literally (use with `ESCAPE '\'`).
fn folder_pattern(folder: &str) -> String {
    let folder = folder.trim_matches('/');
    if folder.is_empty() {
        return String::new();
    }
    let mut p = String::with_capacity(folder.len() + 2);
    for c in folder.chars() {
        if matches!(c, '%' | '_' | '\\') {
            p.push('\\');
        }
        p.push(c);
    }
    p.push_str("/%");
    p
}

/// SQL conditions requiring note `n` to carry each tag bound at `?first`, `?first+1`, …
/// A tag also matches its nested tags (`proj` → `proj/a`), which sort from `proj/` up to
/// `proj0` (`0` follows `/` in ASCII); a range keeps the `tags(path, tag)` index usable.
fn tag_filter_sql(first: usize, count: usize) -> String {
    let mut sql = String::new();
    for i in first..first + count {
        sql.push_str(&format!(
            " AND EXISTS(SELECT 1 FROM tags t WHERE t.path = n.path \
             AND (t.tag = ?{i} OR (t.tag >= ?{i} || '/' AND t.tag < ?{i} || '0')))"
        ));
    }
    sql
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

/// Splits a search string into (free text, tag filters, folder filter).
pub fn split_filters(q: &str) -> (String, Vec<String>, Option<String>) {
    let p = parse_query(q);
    let text = p.fts.replace("\"*", " ").replace('"', " ");
    (
        text.split_whitespace().collect::<Vec<_>>().join(" "),
        p.tags,
        p.folder,
    )
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
    fn web_autosaves_share_a_revision() {
        let (_d, mut i) = idx();
        i.upsert("a.md", "v1", 1, 1, "web").unwrap();
        for v in ["v2", "v3", "v4"] {
            i.upsert("a.md", v, 1, 1, "web").unwrap();
        }
        assert_eq!(
            i.revisions("a.md").unwrap().len(),
            1,
            "one snapshot per editing session"
        );
        i.upsert("a.md", "v5", 1, 1, "mcp:claude").unwrap();
        i.upsert("a.md", "v6", 1, 1, "web").unwrap();
        let revs = i.revisions("a.md").unwrap();
        assert_eq!(
            revs.len(),
            3,
            "other sources always snapshot, and break the session"
        );
    }

    #[test]
    fn resolver_cache_follows_new_paths() {
        let (_d, mut i) = idx();
        i.upsert("src.md", "see [[Later]]", 1, 1, "t").unwrap();
        assert_eq!(i.resolve_link("src.md", "Later").unwrap(), None);
        i.upsert("deep/Later.md", "x", 1, 1, "t").unwrap();
        assert_eq!(
            i.resolve_link("src.md", "Later").unwrap().as_deref(),
            Some("deep/Later.md")
        );
        assert_eq!(i.backlinks("deep/Later.md").unwrap().len(), 1);
        i.remove("deep/Later.md", "t").unwrap();
        assert_eq!(i.resolve_link("src.md", "Later").unwrap(), None);
    }

    #[test]
    fn batch_indexing_matches_single() {
        let (_d, mut i) = idx();
        let items: Vec<_> = (0..50)
            .map(|n| {
                prepare(
                    &format!("n{n}.md"),
                    format!("# N{n}\n#t{} [[n{}]] - [ ] task", n % 3, (n + 1) % 50),
                    1,
                    1,
                    false,
                )
            })
            .collect();
        assert_eq!(i.upsert_many(&items, "external").unwrap(), 50);
        assert_eq!(i.count().unwrap(), 50);
        assert_eq!(i.backlinks("n7.md").unwrap().len(), 1);
        assert_eq!(i.search("tag:t1", 100).unwrap().len(), 17);
        assert_eq!(
            i.list(None, false, 3).unwrap()[0].tags,
            vec!["t0".to_string()]
        );
        assert_eq!(i.count_due_tasks("2099-01-01").unwrap(), 0);
        assert_eq!(
            i.upsert_many(&items, "external").unwrap(),
            0,
            "unchanged notes are skipped"
        );
    }

    #[test]
    fn chunk_vectors_follow_their_text() {
        let (_d, mut i) = idx();
        i.chunking = true;
        let section = |n: usize| {
            let sentence = format!("Section {n} text. ");
            format!("## S{n}\n\n{}", sentence.repeat(30))
        };
        let body = (0..4).map(section).collect::<Vec<_>>().join("\n\n");
        i.upsert("a.md", &body, 1, 1, "t").unwrap();
        // Fake vectors: [index at embedding time].
        let rows: Vec<_> = i
            .pending_chunks(100)
            .unwrap()
            .into_iter()
            .map(|(p, n, h, _)| (p, n, h, vec![n as f32]))
            .collect();
        assert_eq!(rows.len(), 4);
        i.store_vectors(&rows).unwrap();
        assert_eq!(
            i.rechunk_missing().unwrap(),
            1,
            "first run checks every note"
        );
        assert_eq!(i.rechunk_missing().unwrap(), 0);
        assert_eq!(
            i.load_vectors().unwrap().len(),
            4,
            "re-chunking keeps vectors"
        );

        // A new section at the top shifts every chunk down; their vectors move along.
        let shifted = format!("## New\n\nFresh text.\n\n{body}");
        i.upsert("a.md", &shifted, 1, 2, "t").unwrap();
        let pending = i.pending_chunks(100).unwrap();
        assert_eq!(pending.len(), 1, "only the new chunk needs a vector");
        assert_eq!(pending[0].1, 0);
        let vecs = i.load_vectors().unwrap();
        assert_eq!(vecs.len(), 4);
        assert!(vecs.iter().all(|(_, n, v)| *v == vec![(*n - 1) as f32]));

        // A chunk the API rejects is skipped: not pending, not searched — until it changes.
        let p = &pending[0];
        i.store_vectors(&[(p.0.clone(), p.1, p.2.clone(), Vec::new())])
            .unwrap();
        assert!(i.pending_chunks(100).unwrap().is_empty());
        assert_eq!(i.load_vectors().unwrap().len(), 4);
        assert_eq!(i.embed_stats().unwrap(), (5, 5));
        let edited = shifted.replace("Fresh text.", "Fresher text.");
        i.upsert("a.md", &edited, 1, 3, "t").unwrap();
        assert_eq!(i.pending_chunks(100).unwrap().len(), 1);

        // Without an embedder only the waiting chunk goes; embedded ones stay.
        assert_eq!(i.drop_pending_chunks().unwrap(), 1);
        assert_eq!(i.embed_stats().unwrap(), (4, 4));
        // Edited while semantic search is off: chunks aren't updated…
        i.chunking = false;
        let edited = edited.replace("Section 3 text.", "Section three.");
        i.upsert("a.md", &edited, 1, 4, "t").unwrap();
        assert_eq!(i.embed_stats().unwrap(), (4, 4));
        // …so turning it back on re-chunks every note, keeping what still matches.
        i.chunking = true;
        assert_eq!(i.rechunk_missing().unwrap(), 1);
        let pending: Vec<i64> = i
            .pending_chunks(100)
            .unwrap()
            .into_iter()
            .map(|p| p.1)
            .collect();
        assert_eq!(pending, vec![0, 4]);
        assert_eq!(i.load_vectors().unwrap().len(), 3);
        assert_eq!(i.rechunk_missing().unwrap(), 0);
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

    #[test]
    fn resolver_dotted_names_and_path_boundaries() {
        let (_d, mut i) = idx();
        for p in [
            "v1.2.md",
            "Tech/Node.js.md",
            "Günlük/2026.11.md",
            "bar/Not.md",
            "README.MD",
        ] {
            i.upsert(p, "", 0, 0, "t").unwrap();
        }
        i.upsert_file("docs/report.pdf", 1, 1).unwrap();
        i.upsert_file("x/assets/pic.png", 1, 1).unwrap();
        let r = |t: &str| i.resolve_link("top.md", t).unwrap();
        assert_eq!(r("v1.2").as_deref(), Some("v1.2.md"));
        assert_eq!(r("Node.js").as_deref(), Some("Tech/Node.js.md"));
        assert_eq!(r("2026.11").as_deref(), Some("Günlük/2026.11.md"));
        assert_eq!(r("report.pdf").as_deref(), Some("docs/report.pdf"));
        assert_eq!(r("missing.v2"), None);
        assert_eq!(r("readme").as_deref(), Some("README.MD"));
        assert_eq!(r("README.md").as_deref(), Some("README.MD"));
        assert_eq!(r("bar/Not").as_deref(), Some("bar/Not.md"));
        assert_eq!(
            r("ar/Not"),
            None,
            "a suffix must start at a folder boundary"
        );
        assert_eq!(r("assets/pic.png").as_deref(), Some("x/assets/pic.png"));
        assert_eq!(r("sets/pic.png"), None);
        i.upsert("a.md", "[[readme]] [[v1.2]]", 0, 0, "t").unwrap();
        assert_eq!(i.backlinks("README.MD").unwrap().len(), 1);
        assert_eq!(i.backlinks("v1.2.md").unwrap().len(), 1);
    }

    #[test]
    fn folder_filters_are_literal() {
        let (_d, mut i) = idx();
        for p in [
            "my_notes/a.md",
            "myXnotes/b.md",
            "my_notes2/e.md",
            "100%/c.md",
            "100x/d.md",
        ] {
            i.upsert(p, "hello", 1, 1, "t").unwrap();
        }
        let paths = |v: Vec<SearchHit>| v.into_iter().map(|h| h.path).collect::<Vec<_>>();
        assert_eq!(
            paths(i.search("hello path:my_notes", 10).unwrap()),
            vec!["my_notes/a.md"]
        );
        assert_eq!(
            paths(i.search("path:my_notes", 10).unwrap()),
            vec!["my_notes/a.md"]
        );
        assert_eq!(
            paths(i.search("hello path:100%", 10).unwrap()),
            vec!["100%/c.md"]
        );
        let listed: Vec<_> = i
            .list(Some("my_notes/"), false, 10)
            .unwrap()
            .into_iter()
            .map(|m| m.path)
            .collect();
        assert_eq!(listed, vec!["my_notes/a.md"]);
    }

    #[test]
    fn rare_tags_survive_the_limit() {
        let (_d, mut i) = idx();
        for n in 0..20 {
            i.upsert(&format!("n{n}.md"), "# word\nword word #common", 1, n, "t")
                .unwrap();
        }
        i.upsert("z/rare.md", "a word #rare #proj/a", 1, 0, "t")
            .unwrap();
        i.upsert("z/other.md", "a word #project", 1, 0, "t")
            .unwrap();
        let hits = i.search("word tag:rare", 1).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "z/rare.md");
        assert_eq!(hits[0].tags, vec!["proj/a", "rare"]);
        assert_eq!(i.search("tag:rare", 1).unwrap()[0].path, "z/rare.md");
        let proj: Vec<_> = i
            .search("word tag:proj", 10)
            .unwrap()
            .into_iter()
            .map(|h| h.path)
            .collect();
        assert_eq!(
            proj,
            vec!["z/rare.md"],
            "nested tags match, prefixes do not"
        );
        assert!(i.search("tag:common tag:rare", 10).unwrap().is_empty());
        assert_eq!(i.search("tag:common", 100).unwrap().len(), 20);
    }
}
