//! Extra index tables: AI activity log, pending (unreviewed) facts, MCP
//! connections and the task list.

use crate::index::{Index, now_ms};
use crate::scope::Scope;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

const MAX_ACTIVITY: i64 = 5000;

#[derive(Debug, Clone, Serialize)]
pub struct Activity {
    pub id: i64,
    pub ts: i64,
    pub source: String,
    /// create | update | delete | move
    pub kind: String,
    pub path: String,
    /// Revision holding the content *before* this change (update/delete).
    pub rev: Option<i64>,
    /// Revisions table high-water mark right after the change.
    #[serde(skip)]
    pub floor: i64,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingFact {
    pub id: i64,
    pub ts: i64,
    pub source: String,
    pub topic: Option<String>,
    pub fact: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Connection {
    pub id: i64,
    pub name: String,
    pub token_hint: String,
    pub read_only: bool,
    pub review: bool,
    pub folders: Vec<String>,
    pub created: i64,
    pub last_used: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskRow {
    pub path: String,
    pub title: String,
    pub line: i64,
    pub text: String,
    pub raw: String,
    pub done: bool,
    pub due: Option<String>,
    pub scheduled: Option<String>,
    pub done_date: Option<String>,
    pub priority: i64,
    pub private: bool,
}

/// (path, title, frontmatter json, mtime, private)
pub type NoteRow = (String, String, String, i64, bool);

pub fn token_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(token.as_bytes()))
}

impl Index {
    pub fn max_rev(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(id),0) FROM revisions", [], |r| {
                r.get(0)
            })?)
    }

    pub fn last_rev_for(&self, path: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT MAX(id) FROM revisions WHERE path=?1", [path], |r| {
                r.get(0)
            })?)
    }

    /// Paths whose frontmatter hides them from AI clients.
    pub fn private_paths(&self) -> Result<std::collections::HashSet<String>> {
        let mut st = self
            .conn
            .prepare("SELECT path FROM notes WHERE private=1")?;
        Ok(st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?)
    }

    // ------------------------------------------------------------ activity

    pub fn log_activity(
        &self,
        source: &str,
        kind: &str,
        path: &str,
        rev: Option<i64>,
        detail: Option<&str>,
    ) -> Result<()> {
        if source == "web" && kind == "update" {
            // Autosaves of one editing session collapse into a single entry.
            let last: Option<(i64, String, String, i64)> = self
                .conn
                .query_row(
                    "SELECT id, source, kind, ts FROM activity WHERE path=?1 ORDER BY id DESC LIMIT 1",
                    [path],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            if let Some((id, s, k, ts)) = last
                && s == "web"
                && (k == "update" || k == "create")
                && now_ms() - ts < crate::index::COALESCE_MS
            {
                self.conn.execute(
                    "UPDATE activity SET ts=?2 WHERE id=?1",
                    params![id, now_ms()],
                )?;
                return Ok(());
            }
        }
        let floor = self.max_rev()?;
        self.conn.execute(
            "INSERT INTO activity(ts,source,kind,path,rev,floor,detail) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![now_ms(), source, kind, path, rev, floor, detail],
        )?;
        self.conn.execute(
            "DELETE FROM activity WHERE id <= (SELECT MAX(id) FROM activity) - ?1",
            [MAX_ACTIVITY],
        )?;
        Ok(())
    }

    fn row_activity(r: &rusqlite::Row) -> rusqlite::Result<Activity> {
        Ok(Activity {
            id: r.get(0)?,
            ts: r.get(1)?,
            source: r.get(2)?,
            kind: r.get(3)?,
            path: r.get(4)?,
            rev: r.get(5)?,
            floor: r.get(6)?,
            detail: r.get(7)?,
        })
    }

    /// Newest first. `ai_only` keeps changes made over MCP.
    pub fn activity(
        &self,
        ai_only: bool,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Activity>> {
        let mut st = self.conn.prepare(
            "SELECT id,ts,source,kind,path,rev,floor,detail FROM activity
             WHERE (?1 = 0 OR source LIKE 'mcp:%') AND (?2 IS NULL OR id < ?2)
             ORDER BY id DESC LIMIT ?3",
        )?;
        Ok(st
            .query_map(
                params![ai_only as i64, before, limit as i64],
                Self::row_activity,
            )?
            .collect::<Result<_, _>>()?)
    }

    pub fn activity_get(&self, id: i64) -> Result<Option<Activity>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id,ts,source,kind,path,rev,floor,detail FROM activity WHERE id=?1",
                [id],
                Self::row_activity,
            )
            .optional()?)
    }

    /// Content of `path` as it was after the activity: the first revision taken
    /// later than the activity's floor (or `None` = still the current file).
    pub fn content_after(&self, path: &str, floor: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT content FROM revisions WHERE path=?1 AND id > ?2 ORDER BY id LIMIT 1",
                params![path, floor],
                |r| r.get(0),
            )
            .optional()?)
    }

    // ------------------------------------------------------------ pending facts

    pub fn add_pending(
        &self,
        source: &str,
        topic: Option<&str>,
        fact: &str,
        tags: &[String],
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO pending_facts(ts,source,topic,fact,tags) VALUES(?1,?2,?3,?4,?5)",
            params![now_ms(), source, topic, fact, serde_json::to_string(tags)?],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn pending(&self) -> Result<Vec<PendingFact>> {
        let mut st = self
            .conn
            .prepare("SELECT id,ts,source,topic,fact,tags FROM pending_facts ORDER BY id")?;
        Ok(st
            .query_map([], |r| {
                let tags: String = r.get(5)?;
                Ok(PendingFact {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    source: r.get(2)?,
                    topic: r.get(3)?,
                    fact: r.get(4)?,
                    tags: serde_json::from_str(&tags).unwrap_or_default(),
                })
            })?
            .collect::<Result<_, _>>()?)
    }

    pub fn take_pending(&self, id: i64) -> Result<Option<PendingFact>> {
        let f = self.pending()?.into_iter().find(|p| p.id == id);
        self.conn
            .execute("DELETE FROM pending_facts WHERE id=?1", [id])?;
        Ok(f)
    }

    pub fn pending_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM pending_facts", [], |r| r.get(0))?)
    }

    // ------------------------------------------------------------ connections

    pub fn add_connection(
        &self,
        name: &str,
        token: &str,
        read_only: bool,
        review: bool,
        folders: &[String],
    ) -> Result<i64> {
        let hint = format!("…{}", &token[token.len().saturating_sub(4)..]);
        self.conn.execute(
            "INSERT INTO connections(name,token_hash,token_hint,read_only,review,folders,created) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![name, token_hash(token), hint, read_only, review, serde_json::to_string(folders)?, now_ms()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_connection(
        &self,
        id: i64,
        name: &str,
        read_only: bool,
        review: bool,
        folders: &[String],
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE connections SET name=?2, read_only=?3, review=?4, folders=?5 WHERE id=?1",
            params![id, name, read_only, review, serde_json::to_string(folders)?],
        )? > 0)
    }

    pub fn delete_connection(&self, id: i64) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM connections WHERE id=?1", [id])?
            > 0)
    }

    fn row_conn(r: &rusqlite::Row) -> rusqlite::Result<Connection> {
        let folders: String = r.get(6)?;
        Ok(Connection {
            id: r.get(0)?,
            name: r.get(1)?,
            token_hint: r.get(2)?,
            read_only: r.get(3)?,
            review: r.get(4)?,
            folders: serde_json::from_str(&folders).unwrap_or_default(),
            created: r.get(7)?,
            last_used: r.get(8)?,
        })
    }

    pub fn connections(&self) -> Result<Vec<Connection>> {
        let mut st = self.conn.prepare(
            "SELECT id,name,token_hint,read_only,review,token_hash,folders,created,last_used FROM connections ORDER BY id",
        )?;
        Ok(st
            .query_map([], Self::row_conn)?
            .collect::<Result<_, _>>()?)
    }

    /// Looks up a connection by its token and records the use.
    pub fn connection_scope(&self, token: &str) -> Result<Option<Scope>> {
        let c = self
            .conn
            .query_row(
                "SELECT id,name,token_hint,read_only,review,token_hash,folders,created,last_used FROM connections WHERE token_hash=?1",
                [token_hash(token)],
                Self::row_conn,
            )
            .optional()?;
        if let Some(c) = &c {
            self.conn.execute(
                "UPDATE connections SET last_used=?2 WHERE id=?1",
                params![c.id, now_ms()],
            )?;
        }
        Ok(c.map(|c| Scope {
            name: c.name,
            read_only: c.read_only,
            review: c.review,
            folders: c.folders,
        }))
    }

    // ------------------------------------------------------------ tasks

    pub fn count_due_tasks(&self, day: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM tasks WHERE done = 0 AND COALESCE(due, scheduled) <= ?1",
            [day],
            |r| r.get(0),
        )?)
    }

    pub fn tasks(&self, include_done: bool) -> Result<Vec<TaskRow>> {
        let mut st = self.conn.prepare(
            "SELECT t.path, n.title, t.line, t.text, t.raw, t.done, t.due, t.scheduled, t.done_date, t.priority, n.private
             FROM tasks t JOIN notes n ON n.path = t.path
             WHERE (?1 = 1 OR t.done = 0)
             ORDER BY t.done, COALESCE(t.due, t.scheduled, '9999') , t.priority DESC, t.path, t.line",
        )?;
        Ok(st
            .query_map([include_done as i64], |r| {
                Ok(TaskRow {
                    path: r.get(0)?,
                    title: r.get(1)?,
                    line: r.get(2)?,
                    text: r.get(3)?,
                    raw: r.get(4)?,
                    done: r.get(5)?,
                    due: r.get(6)?,
                    scheduled: r.get(7)?,
                    done_date: r.get(8)?,
                    priority: r.get(9)?,
                    private: r.get(10)?,
                })
            })?
            .collect::<Result<_, _>>()?)
    }

    /// (path, title, frontmatter json, tags, mtime, private) for every note — used by base queries.
    pub fn all_meta(&self) -> Result<Vec<NoteRow>> {
        let mut st = self
            .conn
            .prepare("SELECT path,title,frontmatter,mtime,private FROM notes")?;
        Ok(st
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<Result<_, _>>()?)
    }

    pub fn tags_by_path(&self) -> Result<std::collections::HashMap<String, Vec<String>>> {
        let mut st = self
            .conn
            .prepare("SELECT path, tag FROM tags ORDER BY tag")?;
        let mut map: std::collections::HashMap<String, Vec<String>> = Default::default();
        for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (p, t) = row?;
            map.entry(p).or_default().push(t);
        }
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connections_and_pending() {
        let d = tempfile::tempdir().unwrap();
        let i = Index::open(&d.path().join("i.db")).unwrap();
        let id = i
            .add_connection(
                "gemini",
                "mc_secret_token_1234",
                true,
                true,
                &["Projeler".into()],
            )
            .unwrap();
        let s = i.connection_scope("mc_secret_token_1234").unwrap().unwrap();
        assert_eq!(s.name, "gemini");
        assert!(s.read_only && s.review);
        assert!(i.connection_scope("wrong").unwrap().is_none());
        assert!(i.connections().unwrap()[0].last_used.is_some());
        assert_eq!(i.connections().unwrap()[0].token_hint, "…1234");
        assert!(i.delete_connection(id).unwrap());

        let pid = i
            .add_pending("mcp:x", Some("Konu"), "bir bilgi", &["a".into()])
            .unwrap();
        assert_eq!(i.pending_count().unwrap(), 1);
        assert_eq!(i.take_pending(pid).unwrap().unwrap().fact, "bir bilgi");
        assert_eq!(i.pending_count().unwrap(), 0);
    }
}
