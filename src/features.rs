//! Higher-level features built on the core service: AI privacy & scopes, the AI
//! activity feed, fact review, tasks, bases, memory hygiene and canvases.

use crate::app::{AResult, App, AppError, WriteResult};
use crate::scope::{Ignore, Scope};
use crate::store::{Activity, Connection, PendingFact, TaskRow};
use crate::vault::Vault;
use crate::{bases, note, review, tasks};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;

pub const IGNORE_FILE: &str = ".mcpignore";

/// What one AI connection can see right now.
pub struct AiView {
    pub scope: Scope,
    ignore: Arc<Ignore>,
    private: HashSet<String>,
}

impl AiView {
    pub fn can_read(&self, path: &str) -> bool {
        !self.private.contains(path)
            && !self.ignore.matches(path)
            && self.scope.folder_allowed(path)
    }

    /// Writable = readable, inside the scope and the connection isn't read-only.
    pub fn can_write(&self, path: &str) -> bool {
        !self.scope.read_only
            && !self.ignore.matches(path)
            && self.scope.folder_allowed(path)
            && !self.private.contains(path)
    }
}

#[derive(Debug, Serialize)]
pub struct RememberOutcome {
    pub queued: bool,
    pub path: Option<String>,
    pub pending_id: Option<i64>,
}

#[derive(Debug, Default, Clone)]
pub struct TaskFilter {
    pub include_done: bool,
    pub due_before: Option<String>,
    pub query: Option<String>,
    pub path: Option<String>,
}

fn not_found(p: &str) -> AppError {
    AppError::NotFound(format!("note not found: {p}"))
}

impl App {
    // ------------------------------------------------------------ privacy

    pub fn ai_ignore_text(&self) -> String {
        std::fs::read_to_string(self.vault.root().join(IGNORE_FILE)).unwrap_or_default()
    }

    pub fn set_ai_ignore(&self, text: &str) -> AResult<()> {
        if text.len() > 64 * 1024 {
            return Err(AppError::BadRequest("ignore file too large".into()));
        }
        std::fs::write(self.vault.root().join(IGNORE_FILE), text).map_err(anyhow::Error::from)?;
        Ok(())
    }

    /// Parsed `.mcpignore`, cached until the file changes.
    pub fn ignore(&self) -> Arc<Ignore> {
        let path = self.vault.root().join(IGNORE_FILE);
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let mut cache = self.ignore_cache.lock().unwrap_or_else(|p| p.into_inner());
        if cache.0 != mtime || cache.1.is_none() {
            let ig = Arc::new(Ignore::parse(
                &std::fs::read_to_string(&path).unwrap_or_default(),
            ));
            *cache = (mtime, Some(ig));
        }
        cache.1.clone().unwrap()
    }

    pub fn ai_view(&self, scope: Scope) -> AResult<AiView> {
        Ok(AiView {
            scope,
            ignore: self.ignore(),
            private: self.idx().private_paths()?,
        })
    }

    /// Every note hidden from AI clients (frontmatter flag or ignore file).
    pub fn ai_hidden_paths(&self) -> AResult<Vec<String>> {
        let ig = self.ignore();
        let idx = self.idx();
        let private = idx.private_paths()?;
        Ok(idx
            .all_paths()?
            .into_iter()
            .filter(|p| private.contains(p) || ig.matches(p))
            .collect())
    }

    // ------------------------------------------------------------ connections

    pub fn connections(&self) -> AResult<Vec<Connection>> {
        Ok(self.idx().connections()?)
    }

    /// Creates a connection and returns its token (shown once).
    pub fn create_connection(
        &self,
        name: &str,
        read_only: bool,
        review: bool,
        folders: &[String],
    ) -> AResult<(i64, String)> {
        let name = clean_name(name)?;
        let folders = clean_folders(folders)?;
        let mut buf = [0u8; 24];
        getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("random: {e}"))?;
        let token = format!("mcc_{}", hex::encode(buf));
        let id = self
            .idx()
            .add_connection(&name, &token, read_only, review, &folders)?;
        Ok((id, token))
    }

    pub fn update_connection(
        &self,
        id: i64,
        name: &str,
        read_only: bool,
        review: bool,
        folders: &[String],
    ) -> AResult<()> {
        let ok = self.idx().update_connection(
            id,
            &clean_name(name)?,
            read_only,
            review,
            &clean_folders(folders)?,
        )?;
        if ok {
            Ok(())
        } else {
            Err(AppError::NotFound("connection not found".into()))
        }
    }

    pub fn delete_connection(&self, id: i64) -> AResult<()> {
        if self.idx().delete_connection(id)? {
            Ok(())
        } else {
            Err(AppError::NotFound("connection not found".into()))
        }
    }

    // ------------------------------------------------------------ activity

    pub fn activity(
        &self,
        ai_only: bool,
        before: Option<i64>,
        limit: usize,
    ) -> AResult<Vec<Activity>> {
        Ok(self.idx().activity(ai_only, before, limit.clamp(1, 200))?)
    }

    /// (before, after) contents around one change.
    pub fn activity_diff(&self, id: i64) -> AResult<Value> {
        let idx = self.idx();
        let a = idx
            .activity_get(id)?
            .ok_or_else(|| AppError::NotFound("activity not found".into()))?;
        let before = match a.rev {
            Some(r) => idx.revision(r)?.map(|(_, c)| c).unwrap_or_default(),
            None => String::new(),
        };
        let after = if a.kind == "delete" {
            String::new()
        } else {
            match idx.content_after(&a.path, a.floor)? {
                Some(c) => c,
                None => self.vault.read(&a.path).unwrap_or_default(),
            }
        };
        let before = if a.kind == "move" {
            after.clone()
        } else {
            before
        };
        Ok(json!({"activity": a, "before": before, "after": after}))
    }

    /// Reverts one change: restores the previous version, removes a created note,
    /// brings back a deleted one or moves a note back.
    pub fn undo_activity(&self, id: i64, source: &str) -> AResult<String> {
        let a = self
            .idx()
            .activity_get(id)?
            .ok_or_else(|| AppError::NotFound("activity not found".into()))?;
        let src = format!("{source} (undo #{id})");
        match a.kind.as_str() {
            "update" => {
                let rev = a
                    .rev
                    .ok_or_else(|| AppError::BadRequest("nothing to restore".into()))?;
                Ok(self.restore_revision(rev, &src)?.path)
            }
            "create" => {
                if self.vault.exists(&a.path) {
                    self.delete_note(&a.path, &src)?;
                }
                Ok(a.path)
            }
            "delete" => {
                let rev = a
                    .rev
                    .ok_or_else(|| AppError::BadRequest("nothing to restore".into()))?;
                let (_, content) = self
                    .idx()
                    .revision(rev)?
                    .ok_or_else(|| AppError::NotFound("revision gone".into()))?;
                Ok(self.write_note(&a.path, &content, None, &src)?.path)
            }
            "move" => {
                let from = a.detail.clone().unwrap_or_default();
                Ok(self.rename_note(&a.path, &from, &src)?.0)
            }
            k => Err(AppError::BadRequest(format!("cannot undo '{k}'"))),
        }
    }

    // ------------------------------------------------------------ fact review

    /// `remember`, or park the fact for the user's approval when `review` is on.
    pub fn remember_or_queue(
        &self,
        fact: &str,
        topic: Option<&str>,
        tags: &[String],
        source: &str,
        review: bool,
    ) -> AResult<RememberOutcome> {
        if fact.trim().is_empty() {
            return Err(AppError::BadRequest("fact must not be empty".into()));
        }
        if review {
            let id = self.idx().add_pending(source, topic, fact.trim(), tags)?;
            return Ok(RememberOutcome {
                queued: true,
                path: None,
                pending_id: Some(id),
            });
        }
        let r = self.remember(fact, topic, tags, source)?;
        Ok(RememberOutcome {
            queued: false,
            path: Some(r.path),
            pending_id: None,
        })
    }

    pub fn pending_facts(&self) -> AResult<Vec<PendingFact>> {
        Ok(self.idx().pending()?)
    }

    pub fn approve_fact(
        &self,
        id: i64,
        edited: Option<&str>,
        topic: Option<&str>,
    ) -> AResult<WriteResult> {
        let f = self
            .idx()
            .take_pending(id)?
            .ok_or_else(|| AppError::NotFound("pending fact not found".into()))?;
        let text = edited
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .unwrap_or(&f.fact)
            .to_string();
        let topic = topic
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(String::from)
            .or(f.topic.clone());
        match self.remember(&text, topic.as_deref(), &f.tags, &f.source) {
            Ok(r) => Ok(r),
            Err(e) => {
                // Put it back so nothing is lost.
                let _ = self
                    .idx()
                    .add_pending(&f.source, f.topic.as_deref(), &f.fact, &f.tags);
                Err(e)
            }
        }
    }

    pub fn reject_fact(&self, id: i64) -> AResult<()> {
        self.idx()
            .take_pending(id)?
            .ok_or_else(|| AppError::NotFound("pending fact not found".into()))?;
        Ok(())
    }

    // ------------------------------------------------------------ tasks

    pub fn tasks(&self, f: &TaskFilter, view: Option<&AiView>) -> AResult<Vec<TaskRow>> {
        let ig = self.ignore();
        let q = f.query.as_deref().map(str::to_lowercase);
        Ok(self
            .idx()
            .tasks(f.include_done)?
            .into_iter()
            .filter(|t| {
                view.is_none_or(|v| v.can_read(&t.path))
                    && (view.is_none() || !(t.private || ig.matches(&t.path)))
            })
            .filter(|t| {
                f.path.as_deref().is_none_or(|p| {
                    t.path == p || t.path.starts_with(&format!("{}/", p.trim_matches('/')))
                })
            })
            .filter(
                |t| match (&f.due_before, t.due.as_ref().or(t.scheduled.as_ref())) {
                    (Some(b), Some(d)) => d.as_str() <= b.as_str(),
                    (Some(_), None) => false,
                    _ => true,
                },
            )
            .filter(|t| q.as_ref().is_none_or(|q| t.text.to_lowercase().contains(q)))
            .collect())
    }

    pub fn toggle_task(
        &self,
        path: &str,
        line: usize,
        raw: &str,
        source: &str,
    ) -> AResult<WriteResult> {
        let path = Vault::clean_note(path).map_err(|e| AppError::BadRequest(e.to_string()))?;
        if !self.vault.exists(&path) {
            return Err(not_found(&path));
        }
        let content = self.vault.read(&path)?;
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let new = tasks::toggle(&content, line, raw, &today).ok_or_else(|| {
            AppError::Conflict("the task changed meanwhile — reload and try again".into())
        })?;
        self.write_note(&path, &new, Some(&crate::index::hash(&content)), source)
    }

    /// Adds `- [ ] text 📅 due` to a note (default: today's journal, under "Tasks").
    pub fn add_task(
        &self,
        text: &str,
        due: Option<&str>,
        path: Option<&str>,
        source: &str,
    ) -> AResult<WriteResult> {
        let text = text.trim().replace('\n', " ");
        if text.is_empty() {
            return Err(AppError::BadRequest("task text must not be empty".into()));
        }
        if let Some(d) = due.filter(|d| !d.is_empty()) {
            chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d")
                .map_err(|_| AppError::BadRequest("due must be YYYY-MM-DD".into()))?;
        }
        let line = match due.filter(|d| !d.is_empty()) {
            Some(d) => format!("- [ ] {text} 📅 {d}"),
            None => format!("- [ ] {text}"),
        };
        match path.map(str::trim).filter(|p| !p.is_empty()) {
            Some(p) => self.append(p, &line, None, source),
            None => {
                let daily = self.ensure_daily(None, source)?;
                self.append(&daily, &line, Some("Tasks"), source)
            }
        }
    }

    // ------------------------------------------------------------ bases

    pub fn base_query(
        &self,
        spec: &bases::Spec,
        view: Option<&AiView>,
    ) -> AResult<bases::BaseResult> {
        let (metas, tags) = {
            let idx = self.idx();
            (idx.all_meta()?, idx.tags_by_path()?)
        };
        let ig = self.ignore();
        let parsed: Vec<(String, String, serde_json::Map<String, Value>, i64)> = metas
            .into_iter()
            .filter(|(p, _, _, _, private)| {
                view.is_none() || !(*private || ig.matches(p)) && view.unwrap().can_read(p)
            })
            .map(|(p, t, fm, m, _)| (p, t, serde_json::from_str(&fm).unwrap_or_default(), m))
            .collect();
        let empty = Vec::new();
        let rows: Vec<bases::Row> = parsed
            .iter()
            .map(|(p, t, fm, m)| bases::Row {
                path: p,
                title: t,
                mtime: *m,
                tags: tags.get(p).unwrap_or(&empty),
                fm,
            })
            .collect();
        bases::run(spec, &rows).map_err(AppError::BadRequest)
    }

    // ------------------------------------------------------------ memory hygiene

    pub fn memory_review(&self, view: Option<&AiView>) -> AResult<Value> {
        let notes = self
            .idx()
            .list(Some(&self.settings.memory_dir), false, 10_000)?;
        let mut all = Vec::new();
        for n in notes {
            if view.is_some_and(|v| !v.can_read(&n.path)) {
                continue;
            }
            if let Ok(c) = self.vault.read(&n.path) {
                all.extend(review::facts(&n.path, &c));
            }
        }
        let dups = review::duplicates(&all, 0.6, 50);
        let cutoff = (chrono::Local::now() - chrono::Duration::days(180))
            .format("%Y-%m-%d")
            .to_string();
        let old: Vec<&review::Fact> = all
            .iter()
            .filter(|f| f.date.as_deref().is_some_and(|d| d < cutoff.as_str()))
            .take(50)
            .collect();
        Ok(json!({"facts": all.len(), "duplicates": dups, "old": old}))
    }

    /// Removes one exact line (e.g. a duplicate fact) from a note.
    pub fn remove_line(
        &self,
        path: &str,
        line: usize,
        raw: &str,
        source: &str,
    ) -> AResult<WriteResult> {
        let path = Vault::clean_note(path).map_err(|e| AppError::BadRequest(e.to_string()))?;
        let content = self.vault.read(&path).map_err(|_| not_found(&path))?;
        let mut lines: Vec<&str> = content.split('\n').collect();
        let i = line.checked_sub(1).filter(|i| {
            lines.get(*i).map(|l| l.trim_end_matches('\r')) == Some(raw.trim_end_matches('\r'))
        });
        let Some(i) = i else {
            return Err(AppError::Conflict(
                "the line changed meanwhile — reload and try again".into(),
            ));
        };
        lines.remove(i);
        self.write_note(
            &path,
            &lines.join("\n"),
            Some(&crate::index::hash(&content)),
            source,
        )
    }

    // ------------------------------------------------------------ previews & canvases

    pub fn preview(&self, path: &str, anchor: Option<&str>) -> AResult<String> {
        let path = self.find_note(path)?;
        let content = self.vault.read(&path)?;
        let md = match anchor.filter(|a| !a.is_empty()) {
            Some(a) => crate::render::section(&content, a).unwrap_or(content),
            None => content,
        };
        // Keep hover cards small.
        let short: String = md.chars().take(4000).collect();
        self.render(&path, &short)
    }

    pub fn read_canvas(&self, path: &str) -> AResult<String> {
        let p = canvas_path(path)?;
        if !self.vault.abs(&p).is_file() {
            return Ok(r#"{"nodes":[],"edges":[]}"#.into());
        }
        Ok(self.vault.read(&p)?)
    }

    pub fn write_canvas(&self, path: &str, content: &str, source: &str) -> AResult<String> {
        let p = canvas_path(path)?;
        let v: Value = serde_json::from_str(content)
            .map_err(|e| AppError::BadRequest(format!("invalid canvas JSON: {e}")))?;
        if !v.get("nodes").is_some_and(Value::is_array) {
            return Err(AppError::BadRequest("canvas needs a 'nodes' array".into()));
        }
        if content.len() > crate::vault::MAX_NOTE_BYTES {
            return Err(AppError::BadRequest("canvas too large".into()));
        }
        let _w = self.wlock();
        let pretty = serde_json::to_string_pretty(&v).map_err(anyhow::Error::from)?;
        self.vault.write(&p, pretty.as_bytes())?;
        if let Some(m) = self.vault.meta(&p) {
            let idx = self.idx();
            idx.upsert_file(&p, m.size as i64, m.mtime)?;
            if let Some((d, _)) = p.rsplit_once('/') {
                idx.add_folder(d)?;
            }
            idx.log_activity(source, "update", &p, None, None)?;
        }
        Ok(p)
    }

    pub fn canvases(&self) -> AResult<Vec<String>> {
        Ok(self
            .idx()
            .all_file_paths()?
            .into_iter()
            .filter(|p| p.to_lowercase().ends_with(".canvas"))
            .collect())
    }

    /// Small shared helper for the MCP layer.
    pub fn title_of(&self, path: &str) -> String {
        self.idx()
            .title_mtime_tags(path)
            .ok()
            .flatten()
            .map(|t| t.0)
            .unwrap_or_else(|| note::stem(path).to_string())
    }
}

fn canvas_path(path: &str) -> AResult<String> {
    let mut p = Vault::clean(path).map_err(|e| AppError::BadRequest(e.to_string()))?;
    if !p.to_lowercase().ends_with(".canvas") {
        p.push_str(".canvas");
    }
    Ok(p)
}

fn clean_name(name: &str) -> AResult<String> {
    let n: String = name
        .trim()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ' ' | '.'))
        .take(40)
        .collect();
    if n.trim().is_empty() {
        return Err(AppError::BadRequest("name must not be empty".into()));
    }
    Ok(n.trim().replace(' ', "-").to_lowercase())
}

fn clean_folders(folders: &[String]) -> AResult<Vec<String>> {
    folders
        .iter()
        .map(|f| f.trim().trim_matches('/'))
        .filter(|f| !f.is_empty())
        .map(|f| Vault::clean(f).map_err(|e| AppError::BadRequest(e.to_string())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Settings;

    fn app() -> (tempfile::TempDir, App) {
        let d = tempfile::tempdir().unwrap();
        let a = App::open(d.path(), None, Settings::default()).unwrap();
        (d, a)
    }

    #[test]
    fn privacy_view() {
        let (_d, a) = app();
        a.write_note("Açık.md", "x", None, "t").unwrap();
        a.write_note("Gizli.md", "---\nai: false\n---\nsır", None, "t")
            .unwrap();
        a.write_note("Özel/Not.md", "y", None, "t").unwrap();
        a.write_note("Projeler/P.md", "z", None, "t").unwrap();
        a.set_ai_ignore("Özel/\n").unwrap();
        let v = a.ai_view(Scope::full("x", false, false)).unwrap();
        assert!(v.can_read("Açık.md"));
        assert!(!v.can_read("Gizli.md"));
        assert!(!v.can_read("Özel/Not.md"));
        let mut hidden = a.ai_hidden_paths().unwrap();
        hidden.sort();
        assert_eq!(hidden, vec!["Gizli.md", "Özel/Not.md"]);
        let s = Scope {
            name: "g".into(),
            read_only: true,
            review: false,
            folders: vec!["Projeler".into()],
        };
        let v = a.ai_view(s).unwrap();
        assert!(v.can_read("Projeler/P.md") && !v.can_write("Projeler/P.md"));
        assert!(!v.can_read("Açık.md"));
    }

    #[test]
    fn activity_diff_and_undo() {
        let (_d, a) = app();
        a.write_note("N.md", "v1", None, "web").unwrap();
        a.write_note("N.md", "v2", None, "mcp:claude").unwrap();
        a.write_note("N.md", "v3", None, "web").unwrap();
        let acts = a.activity(true, None, 10).unwrap();
        assert_eq!(acts.len(), 1);
        let d = a.activity_diff(acts[0].id).unwrap();
        assert_eq!(d["before"], "v1");
        assert_eq!(d["after"], "v2");
        a.undo_activity(acts[0].id, "web").unwrap();
        assert_eq!(a.read_note("N.md").unwrap().content, "v1");

        a.write_note("Yeni.md", "ai yazdı", None, "mcp:gemini")
            .unwrap();
        let c = a.activity(true, None, 1).unwrap()[0].clone();
        assert_eq!(c.kind, "create");
        assert_eq!(a.activity_diff(c.id).unwrap()["after"], "ai yazdı");
        a.undo_activity(c.id, "web").unwrap();
        assert!(!a.vault.exists("Yeni.md"));

        a.delete_note("N.md", "mcp:claude").unwrap();
        let del = a.activity(true, None, 1).unwrap()[0].clone();
        assert_eq!(del.kind, "delete");
        a.undo_activity(del.id, "web").unwrap();
        assert_eq!(a.read_note("N.md").unwrap().content, "v1");

        a.rename_note("N.md", "Taşındı.md", "mcp:claude").unwrap();
        let mv = a.activity(true, None, 1).unwrap()[0].clone();
        assert_eq!(mv.kind, "move");
        a.undo_activity(mv.id, "web").unwrap();
        assert!(a.vault.exists("N.md"));
    }

    #[test]
    fn review_queue_tasks_bases() {
        let (_d, a) = app();
        let o = a
            .remember_or_queue("Kahve sever", Some("Tercihler"), &[], "mcp:x", true)
            .unwrap();
        assert!(o.queued);
        assert!(!a.vault.exists("Memory/Tercihler.md"));
        a.approve_fact(o.pending_id.unwrap(), Some("Şekersiz kahve sever"), None)
            .unwrap();
        assert!(
            a.read_note("Memory/Tercihler.md")
                .unwrap()
                .content
                .contains("Şekersiz kahve sever")
        );
        assert!(a.pending_facts().unwrap().is_empty());

        a.add_task("Fatura öde", Some("2026-12-01"), None, "web")
            .unwrap();
        let t = a.tasks(&TaskFilter::default(), None).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].due.as_deref(), Some("2026-12-01"));
        a.toggle_task(&t[0].path, t[0].line as usize, &t[0].raw, "web")
            .unwrap();
        assert!(a.tasks(&TaskFilter::default(), None).unwrap().is_empty());
        assert_eq!(
            a.tasks(
                &TaskFilter {
                    include_done: true,
                    ..Default::default()
                },
                None
            )
            .unwrap()
            .len(),
            1
        );

        a.write_note("Kitap/A.md", "---\npuan: 5\n---\n", None, "t")
            .unwrap();
        a.write_note("Kitap/B.md", "---\npuan: 2\nai: false\n---\n", None, "t")
            .unwrap();
        let spec = bases::parse_spec("from: Kitap\nsort: -puan").unwrap();
        assert_eq!(a.base_query(&spec, None).unwrap().total, 2);
        let v = a.ai_view(Scope::full("x", false, false)).unwrap();
        assert_eq!(a.base_query(&spec, Some(&v)).unwrap().total, 1);
    }

    #[test]
    fn memory_cleanup_and_canvas() {
        let (_d, a) = app();
        a.remember("Oracle sunucusu 1 GB RAM", Some("Sunucular"), &[], "mcp:a")
            .unwrap();
        a.remember(
            "Oracle sunucusunda 1 GB RAM var",
            Some("Sunucular"),
            &[],
            "mcp:b",
        )
        .unwrap();
        let r = a.memory_review(None).unwrap();
        assert_eq!(r["duplicates"].as_array().unwrap().len(), 1);
        let dup = &r["duplicates"][0]["b"];
        a.remove_line(
            dup["path"].as_str().unwrap(),
            dup["line"].as_u64().unwrap() as usize,
            dup["raw"].as_str().unwrap(),
            "web",
        )
        .unwrap();
        assert_eq!(a.memory_review(None).unwrap()["facts"], 1);

        let p = a.write_canvas("Pano", r#"{"nodes":[{"id":"1","type":"text","text":"hi","x":0,"y":0,"width":100,"height":50}],"edges":[]}"#, "web").unwrap();
        assert_eq!(p, "Pano.canvas");
        assert!(a.read_canvas("Pano.canvas").unwrap().contains("\"hi\""));
        assert!(a.write_canvas("X", "{}", "web").is_err());
        assert_eq!(a.canvases().unwrap(), vec!["Pano.canvas"]);
    }
}
