//! Core service layer shared by the web API and the MCP server.

use crate::index::{self, Index};
use crate::note;
use crate::render;
use crate::vault::{self, Vault};
use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

#[derive(Debug, Clone)]
pub struct Settings {
    pub daily_dir: String,
    pub memory_dir: String,
    pub attachments_dir: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            daily_dir: "Journal".into(),
            memory_dir: "Memory".into(),
            attachments_dir: "attachments".into(),
        }
    }
}

pub struct App {
    pub vault: Vault,
    index: Mutex<Index>,
    write_lock: Mutex<()>,
    pub settings: Settings,
    pub data_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct NoteView {
    pub path: String,
    pub title: String,
    pub content: String,
    pub hash: String,
    pub mtime: i64,
    pub tags: Vec<String>,
    pub frontmatter: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct WriteResult {
    pub path: String,
    pub hash: String,
    pub created: bool,
}

/// Error type that carries an HTTP-ish category so the API can map it.
#[derive(Debug)]
pub enum AppError {
    NotFound(String),
    Conflict(String),
    BadRequest(String),
    Other(anyhow::Error),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::NotFound(s) | AppError::Conflict(s) | AppError::BadRequest(s) => {
                f.write_str(s)
            }
            AppError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError::Other(e)
    }
}

pub type AResult<T> = std::result::Result<T, AppError>;

fn bad(e: anyhow::Error) -> AppError {
    AppError::BadRequest(format!("{e:#}"))
}

impl App {
    pub fn open(
        vault_dir: &std::path::Path,
        data_dir: Option<PathBuf>,
        settings: Settings,
    ) -> Result<Self> {
        let vault = Vault::open(vault_dir)?;
        let data_dir = data_dir.unwrap_or_else(|| vault.root().join(".mindcrab"));
        std::fs::create_dir_all(&data_dir)?;
        let index = Index::open(&data_dir.join("index.db"))?;
        Ok(Self {
            vault,
            index: Mutex::new(index),
            write_lock: Mutex::new(()),
            settings,
            data_dir,
        })
    }

    pub fn idx(&self) -> MutexGuard<'_, Index> {
        self.index.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn wlock(&self) -> MutexGuard<'_, ()> {
        self.write_lock.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ---------------------------------------------------------------- sync

    /// Brings the index in line with the files on disk. Cheap when nothing changed.
    pub fn full_sync(&self) -> Result<(usize, usize)> {
        self.sync(false)
    }

    /// `force` re-reads every file even when size and mtime are unchanged.
    pub fn sync(&self, force: bool) -> Result<(usize, usize)> {
        let _w = self.wlock();
        let (notes, files, dirs) = self.vault.scan();
        let mut idx = self.idx();
        let mut changed = 0;
        let on_disk: std::collections::HashSet<&str> =
            notes.iter().map(|e| e.rel.as_str()).collect();
        for e in &notes {
            let state = idx.note_state(&e.rel)?;
            if !force && matches!(state, Some((_, s, m)) if s == e.size as i64 && m == e.mtime) {
                continue;
            }
            let content = self.vault.read(&e.rel)?;
            if idx.upsert(&e.rel, &content, e.size as i64, e.mtime, "external")? {
                changed += 1;
            }
        }
        let mut removed = 0;
        for p in idx.all_paths()? {
            if !on_disk.contains(p.as_str()) {
                idx.remove(&p, "external")?;
                removed += 1;
            }
        }
        let files_on_disk: std::collections::HashSet<&str> =
            files.iter().map(|e| e.rel.as_str()).collect();
        for p in idx.all_file_paths()? {
            if !files_on_disk.contains(p.as_str()) {
                idx.remove_file(&p)?;
            }
        }
        for f in &files {
            idx.upsert_file(&f.rel, f.size as i64, f.mtime)?;
        }
        idx.set_folders(&dirs)?;
        Ok((changed, removed))
    }

    /// Re-syncs a single path after a filesystem event.
    pub fn sync_path(&self, rel: &str) -> Result<()> {
        let _w = self.wlock();
        let abs = self.vault.abs(rel);
        let mut idx = self.idx();
        if abs.is_dir() {
            idx.add_folder(rel)?;
            return Ok(());
        }
        if vault::is_note(rel) {
            if abs.is_file() {
                let content = self.vault.read(rel)?;
                let m = self.vault.meta(rel).context("stat")?;
                idx.upsert(rel, &content, m.size as i64, m.mtime, "external")?;
            } else if idx.note_state(rel)?.is_some() {
                idx.remove(rel, "external")?;
            }
        } else if abs.is_file() {
            let m = self.vault.meta(rel).context("stat")?;
            idx.upsert_file(rel, m.size as i64, m.mtime)?;
        } else {
            idx.remove_file(rel)?;
            idx.remove_folder(rel)?;
        }
        Ok(())
    }

    /// Creates starter notes in an empty vault.
    pub fn seed_if_empty(&self) -> Result<()> {
        if self.idx().count()? > 0 {
            return Ok(());
        }
        let mem = &self.settings.memory_dir;
        self.write_note(
            "MEMORY.md",
            &crate::seed::memory_md(mem, &self.settings.daily_dir),
            None,
            "seed",
        )
        .map_err(|e| anyhow!("{e}"))?;
        self.write_note("Welcome.md", crate::seed::WELCOME, None, "seed")
            .map_err(|e| anyhow!("{e}"))?;
        self.write_note(
            &format!("{mem}/Inbox.md"),
            &crate::seed::inbox_md(),
            None,
            "seed",
        )
        .map_err(|e| anyhow!("{e}"))?;
        Ok(())
    }

    // ---------------------------------------------------------------- reads

    pub fn read_note(&self, path: &str) -> AResult<NoteView> {
        let path = vault::Vault::clean_note(path).map_err(bad)?;
        if !self.vault.exists(&path) {
            return Err(AppError::NotFound(format!("note not found: {path}")));
        }
        let content = self.vault.read(&path)?;
        let p = note::parse(&path, &content);
        let mtime = self.vault.meta(&path).map(|m| m.mtime).unwrap_or(0);
        Ok(NoteView {
            hash: index::hash(&content),
            title: p.title,
            tags: p.tags,
            frontmatter: Value::Object(p.frontmatter),
            path,
            content,
            mtime,
        })
    }

    /// Finds a note by exact path, or by name/title like a wikilink would.
    pub fn find_note(&self, name: &str) -> AResult<String> {
        if let Ok(p) = Vault::clean_note(name)
            && self.vault.exists(&p)
        {
            return Ok(p);
        }
        if let Some(p) = self.idx().resolve_link("", name)?
            && note::is_note_path(&p)
        {
            return Ok(p);
        }
        Err(AppError::NotFound(format!(
            "no note matches '{name}' — use search_notes to find it"
        )))
    }

    pub fn render(&self, path: &str, content: &str) -> AResult<String> {
        let resolver = self.idx().resolver_snapshot()?;
        Ok(render::render(path, content, &resolver))
    }

    // ---------------------------------------------------------------- writes

    fn check_hash(&self, path: &str, expected: Option<&str>) -> AResult<Option<String>> {
        let current = if self.vault.exists(path) {
            Some(self.vault.read(path)?)
        } else {
            None
        };
        if let Some(exp) = expected.filter(|e| !e.is_empty()) {
            let cur_hash = current.as_deref().map(index::hash);
            let ok = match (&cur_hash, exp) {
                (None, "new") => true,
                (Some(h), e) => h == e,
                (None, _) => false,
            };
            if !ok {
                return Err(AppError::Conflict(format!(
                    "{path} was changed by someone else (expected hash {exp}, current {}). Re-read it and try again.",
                    cur_hash.as_deref().unwrap_or("<missing>")
                )));
            }
        }
        Ok(current)
    }

    fn persist(&self, path: &str, content: &str, source: &str) -> AResult<String> {
        if content.len() > vault::MAX_NOTE_BYTES {
            return Err(AppError::BadRequest(format!(
                "note too large (max {} MB)",
                vault::MAX_NOTE_BYTES >> 20
            )));
        }
        self.vault.write(path, content.as_bytes())?;
        let m = self.vault.meta(path).context("stat after write")?;
        let mut idx = self.idx();
        idx.upsert(path, content, m.size as i64, m.mtime, source)?;
        if let Some(dir) = path.rsplit_once('/').map(|(d, _)| d) {
            idx.add_folder(dir)?;
        }
        Ok(index::hash(content))
    }

    pub fn write_note(
        &self,
        path: &str,
        content: &str,
        expected_hash: Option<&str>,
        source: &str,
    ) -> AResult<WriteResult> {
        let path = Vault::clean_note(path).map_err(bad)?;
        let _w = self.wlock();
        let current = self.check_hash(&path, expected_hash)?;
        let hash = self.persist(&path, content, source)?;
        Ok(WriteResult {
            path,
            hash,
            created: current.is_none(),
        })
    }

    pub fn create_note(&self, path: &str, content: &str, source: &str) -> AResult<WriteResult> {
        let path = Vault::clean_note(path).map_err(bad)?;
        let _w = self.wlock();
        if self.vault.exists(&path) {
            return Err(AppError::Conflict(format!("{path} already exists")));
        }
        let hash = self.persist(&path, content, source)?;
        Ok(WriteResult {
            path,
            hash,
            created: true,
        })
    }

    /// Replaces exactly one occurrence of `old` with `new`.
    pub fn edit_note(
        &self,
        path: &str,
        old: &str,
        new: &str,
        expected_hash: Option<&str>,
        source: &str,
    ) -> AResult<WriteResult> {
        let path = self.find_note(path)?;
        let _w = self.wlock();
        let current = self.check_hash(&path, expected_hash)?.unwrap_or_default();
        if old.is_empty() {
            return Err(AppError::BadRequest("old_text must not be empty".into()));
        }
        let n = current.matches(old).count();
        if n == 0 {
            return Err(AppError::BadRequest(
                "old_text was not found in the note (it must match exactly)".into(),
            ));
        }
        if n > 1 {
            return Err(AppError::BadRequest(format!(
                "old_text appears {n} times; include more surrounding text"
            )));
        }
        let updated = current.replacen(old, new, 1);
        let hash = self.persist(&path, &updated, source)?;
        Ok(WriteResult {
            path,
            hash,
            created: false,
        })
    }

    /// Appends text to the end of a note, or to the end of a heading's section.
    pub fn append(
        &self,
        path: &str,
        text: &str,
        heading: Option<&str>,
        source: &str,
    ) -> AResult<WriteResult> {
        let path = match self.find_note(path) {
            Ok(p) => p,
            Err(_) => Vault::clean_note(path).map_err(bad)?,
        };
        let _w = self.wlock();
        let existing = if self.vault.exists(&path) {
            Some(self.vault.read(&path)?)
        } else {
            None
        };
        let created = existing.is_none();
        let current = existing.unwrap_or_else(|| format!("# {}\n", note::stem(&path)));
        let text = text.trim_end_matches('\n');
        let updated = match heading.map(str::trim).filter(|h| !h.is_empty()) {
            Some(h) => insert_under_heading(&current, h, text),
            None => {
                let mut s = current.trim_end_matches('\n').to_string();
                s.push_str("\n\n");
                s.push_str(text);
                s.push('\n');
                s
            }
        };
        let hash = self.persist(&path, &updated, source)?;
        Ok(WriteResult {
            path,
            hash,
            created,
        })
    }

    pub fn delete_note(&self, path: &str, source: &str) -> AResult<()> {
        let path = Vault::clean_note(path).map_err(bad)?;
        let _w = self.wlock();
        if !self.vault.exists(&path) {
            return Err(AppError::NotFound(format!("note not found: {path}")));
        }
        self.vault.trash(&path)?;
        self.idx().remove(&path, source)?;
        Ok(())
    }

    /// Renames/moves a note and rewrites links pointing at it.
    pub fn rename_note(&self, from: &str, to: &str, source: &str) -> AResult<(String, usize)> {
        let from = Vault::clean_note(from).map_err(bad)?;
        let to = Vault::clean_note(to).map_err(bad)?;
        if from == to {
            return Ok((to, 0));
        }
        let _w = self.wlock();
        if !self.vault.exists(&from) {
            return Err(AppError::NotFound(format!("note not found: {from}")));
        }
        if self.vault.exists(&to) && from.to_lowercase() != to.to_lowercase() {
            return Err(AppError::Conflict(format!("{to} already exists")));
        }
        // Collect (src, raw) link texts that currently resolve to `from`.
        let referrers: Vec<(String, Vec<String>)> = {
            let idx = self.idx();
            let mut out = Vec::new();
            for b in idx.backlinks(&from)? {
                let raws: Vec<String> = idx
                    .outgoing(&b.path)?
                    .into_iter()
                    .filter(|l| l.path.as_deref() == Some(from.as_str()))
                    .map(|l| l.target)
                    .collect();
                out.push((b.path, raws));
            }
            out
        };
        let content = self.vault.read(&from)?;
        if from.to_lowercase() == to.to_lowercase() {
            // Case-only rename: go through a temp name for case-insensitive filesystems.
            let tmp = format!("{from}.mc-rename");
            self.vault.rename(&from, &tmp)?;
            self.vault.rename(&tmp, &to)?;
        } else {
            self.vault.rename(&from, &to)?;
        }
        {
            let mut idx = self.idx();
            idx.remove(&from, &format!("{source} (renamed to {to})"))?;
            idx.rename(&from, &to)?;
        }
        self.persist(&to, &content, source)?;

        let new_stem = note::stem(&to).to_string();
        let stem_unique = self.idx().resolve_link("", &new_stem)?.as_deref() == Some(to.as_str());
        let new_target = if stem_unique {
            new_stem
        } else {
            to.trim_end_matches(".md").to_string()
        };
        let mut updated = 0;
        for (src, raws) in referrers {
            let src = if src == from { to.clone() } else { src };
            let Ok(mut text) = self.vault.read(&src) else {
                continue;
            };
            let before = text.clone();
            for raw in raws {
                if raw.to_lowercase().ends_with(".md") || raw.contains("](") {
                    // Markdown-style relative link: replace the exact target text.
                    let rel = relative_link(&src, &to);
                    text = text.replace(
                        &format!("]({raw})"),
                        &format!("]({})", render::url_encode(&rel)),
                    );
                    text = text.replace(
                        &format!("]({raw}#"),
                        &format!("]({}#", render::url_encode(&rel)),
                    );
                } else {
                    let re = Regex::new(&format!(r"\[\[{}([\]|#^])", regex::escape(&raw))).unwrap();
                    text = re
                        .replace_all(&text, |c: &regex::Captures| {
                            format!("[[{new_target}{}", &c[1])
                        })
                        .into_owned();
                }
            }
            if text != before {
                self.persist(&src, &text, &format!("{source} (link update)"))?;
                updated += 1;
            }
        }
        Ok((to, updated))
    }

    pub fn create_folder(&self, path: &str) -> AResult<String> {
        let path = Vault::clean(path).map_err(bad)?;
        self.vault.mkdir(&path)?;
        self.idx().add_folder(&path)?;
        Ok(path)
    }

    pub fn delete_folder(&self, path: &str) -> AResult<()> {
        let path = Vault::clean(path).map_err(bad)?;
        self.vault
            .rmdir(&path)
            .map_err(|e| AppError::Conflict(format!("{e:#}")))?;
        self.idx().remove_folder(&path)?;
        Ok(())
    }

    pub fn restore_revision(&self, id: i64, source: &str) -> AResult<WriteResult> {
        let (path, content) = self
            .idx()
            .revision(id)?
            .ok_or_else(|| AppError::NotFound(format!("revision {id} not found")))?;
        self.write_note(&path, &content, None, &format!("{source} (restored #{id})"))
    }

    pub fn daily_path(&self, date: Option<&str>) -> AResult<String> {
        let date = match date.map(str::trim).filter(|d| !d.is_empty()) {
            Some(d) => chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d")
                .map_err(|_| AppError::BadRequest("date must be YYYY-MM-DD".into()))?,
            None => chrono::Local::now().date_naive(),
        };
        Ok(format!(
            "{}/{}.md",
            self.settings.daily_dir,
            date.format("%Y-%m-%d")
        ))
    }

    /// Returns today's (or the given day's) journal note, creating it if needed.
    pub fn ensure_daily(&self, date: Option<&str>, source: &str) -> AResult<String> {
        let path = self.daily_path(date)?;
        if !self.vault.exists(&path) {
            let d = note::stem(&path).to_string();
            let body = format!("---\ntags: [journal]\n---\n# {d}\n\n");
            match self.create_note(&path, &body, source) {
                Ok(_) | Err(AppError::Conflict(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(path)
    }

    /// Stores a fact in the memory folder (`Memory/<topic>.md`).
    pub fn remember(
        &self,
        fact: &str,
        topic: Option<&str>,
        tags: &[String],
        source: &str,
    ) -> AResult<WriteResult> {
        let fact = fact.trim();
        if fact.is_empty() {
            return Err(AppError::BadRequest("fact must not be empty".into()));
        }
        let topic = topic
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .unwrap_or("Inbox");
        let topic_clean: String = topic
            .chars()
            .map(|c| {
                if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                    '-'
                } else {
                    c
                }
            })
            .collect();
        let path = format!(
            "{}/{}.md",
            self.settings.memory_dir,
            topic_clean.trim_start_matches('.')
        );
        if !self.vault.exists(&path) {
            let body = format!("---\ntags: [memory]\n---\n# {topic}\n\n");
            let _ = self.create_note(&path, &body, source);
        }
        let date = chrono::Local::now().format("%Y-%m-%d");
        let who = source.strip_prefix("mcp:").unwrap_or(source);
        let tag_str: String = tags
            .iter()
            .map(|t| t.trim().trim_start_matches('#'))
            .filter(|t| !t.is_empty())
            .map(|t| format!(" #{}", t.replace(' ', "-")))
            .collect();
        let line = format!("- {}{tag_str} — _{date}, {who}_", fact.replace('\n', " "));
        self.append(&path, &line, None, source)
    }

    pub fn upload(&self, name: &str, bytes: &[u8]) -> AResult<String> {
        if bytes.len() > vault::MAX_FILE_BYTES {
            return Err(AppError::BadRequest(format!(
                "file too large (max {} MB)",
                vault::MAX_FILE_BYTES >> 20
            )));
        }
        let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
        let base =
            Vault::clean(&format!("{}/{name}", self.settings.attachments_dir)).map_err(bad)?;
        let (stem, ext) = match base.rsplit_once('.') {
            Some((s, e)) if !s.ends_with('/') => (s.to_string(), format!(".{e}")),
            _ => (base.clone(), String::new()),
        };
        let mut path = base.clone();
        let mut n = 1;
        while self.vault.abs(&path).exists() {
            path = format!("{stem} {n}{ext}");
            n += 1;
        }
        self.vault.write(&path, bytes)?;
        let m = self.vault.meta(&path).context("stat")?;
        let idx = self.idx();
        idx.upsert_file(&path, m.size as i64, m.mtime)?;
        idx.add_folder(&self.settings.attachments_dir)?;
        Ok(path)
    }

    pub fn read_file(&self, path: &str) -> AResult<Vec<u8>> {
        let path = Vault::clean(path).map_err(bad)?;
        if !self.vault.abs(&path).is_file() {
            return Err(AppError::NotFound(format!("file not found: {path}")));
        }
        Ok(self.vault.read_bytes(&path)?)
    }
}

/// Inserts `text` at the end of the section that starts with heading `h`
/// (matched case-insensitively, with or without `#`). Creates the section if missing.
fn insert_under_heading(content: &str, h: &str, text: &str) -> String {
    let want = h.trim_start_matches('#').trim().to_lowercase();
    let lines: Vec<&str> = content.lines().collect();
    let mut start = None;
    let mut level = 0;
    let mut in_fence = false;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
        }
        if in_fence {
            continue;
        }
        let hashes = t.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
            if let Some(_s) = start {
                if hashes <= level {
                    return splice(&lines, i, text);
                }
            } else if t[hashes..].trim().to_lowercase() == want {
                start = Some(i);
                level = hashes;
            }
        }
    }
    match start {
        Some(_) => splice(&lines, lines.len(), text),
        None => {
            let mut s = content.trim_end_matches('\n').to_string();
            s.push_str(&format!(
                "\n\n## {}\n\n{text}\n",
                h.trim_start_matches('#').trim()
            ));
            s
        }
    }
}

fn splice(lines: &[&str], at: usize, text: &str) -> String {
    // Insert before `at`, after trimming blank lines at the end of the section.
    let mut end = at;
    while end > 0 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    let mut out: Vec<String> = lines[..end].iter().map(|s| s.to_string()).collect();
    out.push(text.to_string());
    if at < lines.len() {
        out.push(String::new());
        out.extend(lines[at..].iter().map(|s| s.to_string()));
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

fn relative_link(src: &str, dst: &str) -> String {
    let sd: Vec<&str> = note::parent_dir(src)
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let dd: Vec<&str> = dst.split('/').collect();
    let common = sd.iter().zip(dd.iter()).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = std::iter::repeat_n("..".to_string(), sd.len() - common).collect();
    parts.extend(dd[common..].iter().map(|s| s.to_string()));
    parts.join("/")
}

pub fn ensure_token(
    data_dir: &std::path::Path,
    env_token: Option<String>,
    name: &str,
) -> Result<(String, bool)> {
    if let Some(t) = env_token.filter(|t| !t.trim().is_empty()) {
        if t.trim().len() < 16 {
            bail!("{name} must be at least 16 characters");
        }
        return Ok((t.trim().to_string(), false));
    }
    let file = data_dir.join(name.to_lowercase());
    if let Ok(t) = std::fs::read_to_string(&file)
        && t.trim().len() >= 16
    {
        return Ok((t.trim().to_string(), false));
    }
    let mut buf = [0u8; 24];
    getrandom::fill(&mut buf).map_err(|e| anyhow!("random: {e}"))?;
    let t = format!("mc_{}", hex::encode(buf));
    std::fs::write(&file, &t)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok((t, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> (tempfile::TempDir, App) {
        let d = tempfile::tempdir().unwrap();
        let a = App::open(d.path(), None, Settings::default()).unwrap();
        (d, a)
    }

    #[test]
    fn heading_insert() {
        let c = "# T\n\n## A\n- a1\n\n## B\n- b1\n";
        assert_eq!(
            insert_under_heading(c, "A", "- a2"),
            "# T\n\n## A\n- a1\n- a2\n\n## B\n- b1\n"
        );
        assert_eq!(
            insert_under_heading(c, "## b", "- b2"),
            "# T\n\n## A\n- a1\n\n## B\n- b1\n- b2\n"
        );
        assert!(insert_under_heading(c, "C", "x").ends_with("## C\n\nx\n"));
    }

    #[test]
    fn relative() {
        assert_eq!(relative_link("a/b/c.md", "a/d.md"), "../d.md");
        assert_eq!(relative_link("x.md", "a/d.md"), "a/d.md");
    }

    #[test]
    fn write_edit_conflict_history() {
        let (_d, a) = app();
        let w = a.write_note("N", "hello world", None, "t").unwrap();
        assert!(w.created);
        assert_eq!(w.path, "N.md");
        assert!(matches!(
            a.write_note("N", "x", Some("deadbeef"), "t"),
            Err(AppError::Conflict(_))
        ));
        let e = a
            .edit_note("N", "world", "dünya", Some(&w.hash), "t")
            .unwrap();
        assert_eq!(a.read_note("N.md").unwrap().content, "hello dünya");
        assert!(a.edit_note("N", "nope", "x", None, "t").is_err());
        let revs = a.idx().revisions("N.md").unwrap();
        assert_eq!(revs.len(), 1);
        a.restore_revision(revs[0].id, "t").unwrap();
        assert_eq!(a.read_note("N.md").unwrap().content, "hello world");
        assert_ne!(e.hash, a.read_note("N.md").unwrap().hash);
    }

    #[test]
    fn rename_updates_links() {
        let (_d, a) = app();
        a.write_note("Old.md", "# Old", None, "t").unwrap();
        a.write_note(
            "dir/Ref.md",
            "see [[Old]] and [[Old|alias]] and [[Old#h]] and [md](../Old.md)",
            None,
            "t",
        )
        .unwrap();
        let (to, n) = a.rename_note("Old.md", "sub/New.md", "t").unwrap();
        assert_eq!(to, "sub/New.md");
        assert_eq!(n, 1);
        let r = a.read_note("dir/Ref.md").unwrap().content;
        assert_eq!(
            r,
            "see [[New]] and [[New|alias]] and [[New#h]] and [md](../sub/New.md)"
        );
        assert_eq!(a.idx().backlinks("sub/New.md").unwrap().len(), 1);
    }

    #[test]
    fn remember_and_daily() {
        let (_d, a) = app();
        let r = a
            .remember(
                "Kahveyi şekersiz içer",
                Some("Tercihler"),
                &["yemek".into()],
                "mcp:claude",
            )
            .unwrap();
        assert_eq!(r.path, "Memory/Tercihler.md");
        let c = a.read_note(&r.path).unwrap().content;
        assert!(c.contains("- Kahveyi şekersiz içer #yemek — _"), "{c}");
        assert!(c.contains(", claude_"));
        assert_eq!(a.idx().search("sekersiz", 5).unwrap().len(), 1);
        let d = a.ensure_daily(Some("2026-01-02"), "t").unwrap();
        assert_eq!(d, "Journal/2026-01-02.md");
        assert!(a.ensure_daily(Some("2026-13-02"), "t").is_err());
    }

    #[test]
    fn sync_picks_up_external_files() {
        let (d, a) = app();
        std::fs::write(d.path().join("ext.md"), "external [[x]]").unwrap();
        std::fs::create_dir_all(d.path().join(".obsidian")).unwrap();
        std::fs::write(d.path().join(".obsidian/hidden.md"), "nope").unwrap();
        assert_eq!(a.full_sync().unwrap(), (1, 0));
        assert_eq!(a.idx().count().unwrap(), 1);
        std::fs::remove_file(d.path().join("ext.md")).unwrap();
        assert_eq!(a.full_sync().unwrap(), (0, 1));
    }
}
