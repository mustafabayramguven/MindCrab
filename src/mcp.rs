//! Model Context Protocol server (JSON-RPC 2.0). Shared by the Streamable HTTP
//! endpoint (`POST /mcp`) and the stdio transport (`mindcrab mcp`).
//!
//! Every tool goes through an [`AiView`]: notes marked `ai: false`, paths in
//! `.mcpignore` and folders outside the connection's scope are invisible —
//! they behave exactly like notes that don't exist.

use crate::app::{AResult, App, AppError};
use crate::features::{AiView, TaskFilter};
use crate::index::{SNIP_END, SNIP_START};
use crate::scope::Scope;
use crate::vault::Vault;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::sync::LazyLock;

pub const SUPPORTED_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "MindCrab is the user's personal Markdown notebook and long-term memory. \
At the start of a conversation where past context could help, call memory_overview. \
Search (search_notes) before writing so you update existing notes instead of creating duplicates. \
Use remember for small durable facts, append_to_note/edit_note for targeted changes, and write_note only for new notes or full rewrites (pass expected_hash). \
Tasks: list_tasks / add_task / complete_task. Structured data in frontmatter can be queried with query_notes. \
Link related notes with [[wikilinks]] and group with #tags. Write in the user's language. \
Some notes are private to the user and are not visible to you — don't ask about them. \
Never store secrets (passwords, API keys, card or ID numbers). All changes are versioned and can be restored.";

pub struct Session {
    /// Name from the client's `initialize` (e.g. "claude-code").
    pub client: String,
    pub scope: Scope,
    /// True when the scope comes from a named connection (its name is used in history).
    pub named: bool,
}

impl Session {
    pub fn new(scope: Scope, named: bool) -> Self {
        Self {
            client: "mcp".into(),
            scope,
            named,
        }
    }

    pub fn source(&self) -> String {
        if self.named {
            format!("mcp:{}", self.scope.name)
        } else {
            format!("mcp:{}", self.client)
        }
    }
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: &Value, code: i64, msg: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
}

/// Most messages accepted in one JSON-RPC batch.
pub const MAX_BATCH: usize = 32;

/// Handles one JSON-RPC message (or batch). Returns `None` for notifications.
pub fn handle(app: &App, session: &mut Session, msg: Value) -> Option<Value> {
    let Value::Array(items) = msg else {
        return handle_one(app, session, msg);
    };
    if items.is_empty() {
        return Some(rpc_error(
            &Value::Null,
            -32600,
            "invalid request: empty batch",
        ));
    }
    if items.len() > MAX_BATCH {
        return Some(rpc_error(
            &Value::Null,
            -32600,
            &format!("invalid request: batch too large (at most {MAX_BATCH} messages)"),
        ));
    }
    let out: Vec<Value> = items
        .into_iter()
        .filter_map(|m| match m {
            Value::Array(_) => Some(rpc_error(
                &Value::Null,
                -32600,
                "invalid request: nested batch",
            )),
            m => handle_one(app, session, m),
        })
        .collect();
    if out.is_empty() {
        None
    } else {
        Some(Value::Array(out))
    }
}

fn handle_one(app: &App, session: &mut Session, msg: Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let Some(id) = id else {
        return None; // notification or client response
    };
    let resp = match method {
        "initialize" => {
            if let Some(name) = params.pointer("/clientInfo/name").and_then(Value::as_str) {
                session.client = sanitize_client(name);
            }
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("");
            let version = if SUPPORTED_VERSIONS.contains(&requested) {
                requested
            } else {
                SUPPORTED_VERSIONS[0]
            };
            rpc_result(
                &id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "mindcrab", "title": "MindCrab", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS,
                }),
            )
        }
        "ping" => rpc_result(&id, json!({})),
        "tools/list" => rpc_result(&id, json!({"tools": tool_defs(session.scope.read_only)})),
        "resources/list" => rpc_result(&id, json!({"resources": []})),
        "resources/templates/list" => rpc_result(&id, json!({"resourceTemplates": []})),
        "prompts/list" => rpc_result(&id, json!({"prompts": []})),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            if !tool_defs(session.scope.read_only)
                .iter()
                .any(|t| t["name"] == name)
            {
                return Some(rpc_error(&id, -32602, &format!("unknown tool: {name}")));
            }
            let (text, is_error) = match call_tool(app, session, name, &args) {
                Ok(t) => (t, false),
                Err(e) => (format!("Error: {e}"), true),
            };
            rpc_result(
                &id,
                json!({"content": [{"type": "text", "text": text}], "isError": is_error}),
            )
        }
        _ => rpc_error(&id, -32601, &format!("method not found: {method}")),
    };
    Some(resp)
}

fn sanitize_client(name: &str) -> String {
    let s: String = name
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '))
        .take(40)
        .collect();
    let s = s.trim().replace(' ', "-").to_lowercase();
    if s.is_empty() { "client".into() } else { s }
}

// ---------------------------------------------------------------- tool definitions

fn schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

fn tool(
    name: &str,
    title: &str,
    desc: &str,
    input: Value,
    read_only: bool,
    destructive: bool,
) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": desc,
        "inputSchema": input,
        "annotations": {
            "title": title,
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "idempotentHint": read_only,
            "openWorldHint": false
        }
    })
}

static TOOLS_READ_WRITE: LazyLock<Vec<Value>> = LazyLock::new(|| build_tool_defs(false));
static TOOLS_READ_ONLY: LazyLock<Vec<Value>> = LazyLock::new(|| build_tool_defs(true));

/// Tool definitions, built once per kind of connection.
pub fn tool_defs(read_only: bool) -> &'static [Value] {
    if read_only {
        &TOOLS_READ_ONLY
    } else {
        &TOOLS_READ_WRITE
    }
}

fn build_tool_defs(read_only: bool) -> Vec<Value> {
    let path = json!({"type": "string", "description": "Note path relative to the vault, e.g. 'Projects/MindCrab.md' (.md optional). A note name or title also works."});
    let hash = json!({"type": "string", "description": "Optional. The hash from read_note; the write fails if the note changed since. Use 'new' to require that the note does not exist yet."});
    let date = json!({"type": "string", "description": "YYYY-MM-DD"});
    let mut tools = vec![
        tool(
            "memory_overview",
            "Memory overview",
            "Start here. Returns the MEMORY.md guide/index written by the user, memory topics, recently changed notes, top tags and open tasks due soon.",
            schema(json!({}), &[]),
            true,
            false,
        ),
        tool(
            "search_notes",
            "Search notes",
            "Searches all notes by words and — when semantic search is configured — by meaning (results marked ≈). Diacritics are ignored (seker finds şeker). Filters: tag:name, path:Folder, \"exact phrase\". Use this to recall what the user told you before.",
            schema(
                json!({
                    "query": {"type": "string", "description": "Search words and optional filters, e.g. 'docker caddy tag:server'"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 10},
                    "mode": {"type": "string", "enum": ["auto", "keyword", "semantic"], "default": "auto"}
                }),
                &["query"],
            ),
            true,
            false,
        ),
        tool(
            "read_note",
            "Read note",
            "Returns the full Markdown of a note with its hash (for safe edits), tags and the notes linking to it.",
            schema(json!({"path": path}), &["path"]),
            true,
            false,
        ),
        tool(
            "list_notes",
            "List notes",
            "Lists notes, optionally inside a folder, alphabetically or by most recently changed.",
            schema(
                json!({
                    "folder": {"type": "string", "description": "Optional folder, e.g. 'Memory'"},
                    "recent": {"type": "boolean", "default": false},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 100}
                }),
                &[],
            ),
            true,
            false,
        ),
        tool(
            "get_links",
            "Get links",
            "The knowledge graph around a note: backlinks (with context), outgoing links and unlinked mentions.",
            schema(json!({"path": path}), &["path"]),
            true,
            false,
        ),
        tool(
            "related_notes",
            "Related notes",
            "Notes closest in meaning to the given note (needs semantic search). Good for finding context the user never linked.",
            schema(
                json!({"path": path, "limit": {"type": "integer", "minimum": 1, "maximum": 30, "default": 8}}),
                &["path"],
            ),
            true,
            false,
        ),
        tool(
            "list_tags",
            "List tags",
            "All tags with note counts.",
            schema(json!({}), &[]),
            true,
            false,
        ),
        tool(
            "query_notes",
            "Query notes (bases)",
            "Structured query over note frontmatter, like a database: filter by folder/tag/conditions, pick columns, sort. Conditions look like 'rating >= 4', 'status != done', 'has author', 'tags contains book'. Built-in fields: title, path, folder, name, tags, modified.",
            schema(
                json!({
                    "from": {"type": "string", "description": "Folder"},
                    "tag": {"type": "string"},
                    "where": {"type": "array", "items": {"type": "string"}},
                    "columns": {"type": "array", "items": {"type": "string"}},
                    "sort": {"type": "string", "description": "Field, '-' prefix for descending, e.g. '-rating'"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 500, "default": 50}
                }),
                &[],
            ),
            true,
            false,
        ),
        tool(
            "list_tasks",
            "List tasks",
            "Checkbox tasks from all notes (Obsidian Tasks syntax: 📅 due, ⏳ scheduled, ✅ done). Sorted by due date.",
            schema(
                json!({
                    "status": {"type": "string", "enum": ["open", "done", "all"], "default": "open"},
                    "due_before": date,
                    "query": {"type": "string", "description": "Text filter"},
                    "folder": {"type": "string"}
                }),
                &[],
            ),
            true,
            false,
        ),
        tool(
            "note_history",
            "Note history",
            "Earlier versions of a note (who changed it and when). Pass revision_id to read that version's content.",
            schema(
                json!({"path": path, "revision_id": {"type": "integer"}}),
                &["path"],
            ),
            true,
            false,
        ),
        tool(
            "daily_note",
            "Journal note",
            "Reads the journal note for a day (default today), an ISO week or a month. With 'append', adds text to it (created if missing) — use for a log of what happened or was decided.",
            schema(
                json!({
                    "date": {"type": "string", "description": "YYYY-MM-DD, default today"},
                    "period": {"type": "string", "enum": ["day", "week", "month"], "default": "day"},
                    "append": {"type": "string", "description": "Optional Markdown to append (write access required)"}
                }),
                &[],
            ),
            read_only,
            false,
        ),
        tool(
            "memory_review",
            "Review memory",
            "Finds facts in the memory folder that repeat each other and facts older than six months, so you can merge or refresh them with edit_note (ask the user when unsure).",
            schema(json!({}), &[]),
            true,
            false,
        ),
    ];
    if read_only {
        return tools;
    }
    tools.extend([
        tool("remember", "Remember a fact",
            "Saves one durable fact about the user or their world as a dated bullet in Memory/<topic>.md. Keep it to one self-contained sentence. Reuse an existing topic (see memory_overview). On some connections facts wait for the user's approval first.",
            schema(json!({
                "fact": {"type": "string"},
                "topic": {"type": "string", "description": "Topic note name, e.g. 'Preferences', 'Servers', 'People'. Default: Inbox"},
                "tags": {"type": "array", "items": {"type": "string"}}
            }), &["fact"]), false, false),
        tool("write_note", "Write note",
            "Creates a note or replaces its whole content. Prefer edit_note/append_to_note for changes to existing notes. Folders are created automatically.",
            schema(json!({"path": path, "content": {"type": "string"}, "expected_hash": hash}), &["path", "content"]), false, true),
        tool("edit_note", "Edit note",
            "Replaces one exact occurrence of old_text with new_text. old_text must match exactly once. Empty new_text deletes.",
            schema(json!({"path": path, "old_text": {"type": "string"}, "new_text": {"type": "string"}, "expected_hash": hash}), &["path", "old_text", "new_text"]), false, true),
        tool("append_to_note", "Append to note",
            "Appends Markdown to the end of a note, or to the end of a heading's section. Creates the note (and the heading) if missing.",
            schema(json!({"path": path, "text": {"type": "string"}, "heading": {"type": "string"}}), &["path", "text"]), false, false),
        tool("add_task", "Add task",
            "Adds a checkbox task (with optional due date) to a note — by default under 'Tasks' in today's journal.",
            schema(json!({"text": {"type": "string"}, "due": date, "path": {"type": "string", "description": "Optional note to add it to"}}), &["text"]), false, false),
        tool("complete_task", "Complete / reopen task",
            "Toggles a task's checkbox. Identify it by path plus line (from list_tasks) or by a unique piece of its text.",
            schema(json!({"path": path, "line": {"type": "integer"}, "text": {"type": "string"}}), &["path"]), false, false),
        tool("move_note", "Move / rename note",
            "Renames or moves a note — or a whole folder — and updates every [[link]] that points to the moved notes.",
            schema(json!({"from": path, "to": {"type": "string"}}), &["from", "to"]), false, false),
        tool("delete_note", "Delete note",
            "Moves a note to the vault trash. Only do this when the user asks. It can be restored from history.",
            schema(json!({"path": path}), &["path"]), false, true),
        tool("restore_revision", "Restore revision",
            "Restores a note to an earlier version from note_history.",
            schema(json!({"revision_id": {"type": "integer"}}), &["revision_id"]), false, true),
    ]);
    tools
}

// ---------------------------------------------------------------- helpers

fn s<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
}

fn req<'a>(args: &'a Value, key: &str) -> Result<&'a str, AppError> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest(format!("missing argument: {key}")))
}

fn clean_snippet(sn: &str) -> String {
    sn.replace([SNIP_START, SNIP_END], "**").replace('\n', " ")
}

fn fmt_time(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn hidden(p: &str) -> AppError {
    AppError::NotFound(format!(
        "no note matches '{p}' — use search_notes to find it"
    ))
}

/// Resolves a note the AI may read (hidden notes look like missing ones).
fn readable(app: &App, v: &AiView, name: &str) -> AResult<String> {
    let p = app.find_note(name).map_err(|_| hidden(name))?;
    if v.can_read(&p) {
        Ok(p)
    } else {
        Err(hidden(name))
    }
}

fn bad_path(e: anyhow::Error) -> AppError {
    AppError::BadRequest(e.to_string())
}

/// Checks that the AI may create/change `path` (which may not exist yet).
fn writable(app: &App, v: &AiView, path: &str) -> AResult<String> {
    let p = match app.find_note(path) {
        Ok(p) => p,
        Err(_) => Vault::clean_note(path).map_err(bad_path)?,
    };
    check_writable(app, v, &p, path)?;
    Ok(p)
}

/// Like [`writable`] for a path that is used as given, without a lookup by
/// name — normalised exactly as the write itself will do it (`Vault::clean_note`).
fn writable_exact(app: &App, v: &AiView, path: &str) -> AResult<String> {
    let p = Vault::clean_note(path).map_err(bad_path)?;
    check_writable(app, v, &p, path)?;
    Ok(p)
}

/// `p` is a clean note path; `asked` is what the AI passed.
fn check_writable(app: &App, v: &AiView, p: &str, asked: &str) -> AResult<()> {
    if !v.can_read(p) && app.vault.exists(p) {
        return Err(hidden(asked));
    }
    if !v.can_write(p) {
        return Err(AppError::BadRequest(format!(
            "this connection may not change '{p}' (read-only or outside its folders: {})",
            if v.scope.folders.is_empty() {
                "whole vault".to_string()
            } else {
                v.scope.folders.join(", ")
            }
        )));
    }
    Ok(())
}

/// A revision the AI may see: its note is visible to this connection and
/// neither it nor a later version of the note is private (this also covers
/// private notes that were deleted). Anything else looks missing.
fn visible_revision(app: &App, v: &AiView, id: i64) -> AResult<(String, String)> {
    let missing = || AppError::NotFound(format!("revision {id} not found"));
    let (p, content) = app.idx().ai_revision(id)?.ok_or_else(missing)?;
    if !v.can_read(&p) {
        return Err(missing());
    }
    Ok((p, content))
}

fn task_line(t: &crate::store::TaskRow) -> String {
    let mut s = format!("- [{}] {}", if t.done { "x" } else { " " }, t.text);
    if let Some(d) = &t.due {
        let _ = write!(s, " 📅 {d}");
    }
    if let Some(d) = &t.scheduled {
        let _ = write!(s, " ⏳ {d}");
    }
    if t.priority > 0 {
        s.push_str(if t.priority >= 2 { " ⏫" } else { " 🔼" });
    }
    let _ = write!(s, " — {}:{}", t.path, t.line);
    s
}

// ---------------------------------------------------------------- tools

pub fn call_tool(app: &App, session: &Session, name: &str, a: &Value) -> Result<String, AppError> {
    let src = session.source();
    let v = app.ai_view(session.scope.clone())?;
    let mut out = String::new();
    match name {
        "memory_overview" => {
            match app
                .read_note("MEMORY.md")
                .ok()
                .filter(|_| v.can_read("MEMORY.md"))
            {
                Some(n) => {
                    let _ = writeln!(
                        out,
                        "# MEMORY.md (hash {})\n\n{}\n",
                        n.hash,
                        n.content.trim()
                    );
                }
                None => out.push_str(
                    "(No MEMORY.md yet. You may create one as an index for future sessions.)\n\n",
                ),
            }
            let es = app.embed_status();
            let sem = if es["enabled"] == true {
                "on (search finds notes by meaning too)"
            } else {
                "off (keyword search only)"
            };
            let all = app.idx().list(None, true, 1_000_000)?;
            let visible: Vec<_> = all.into_iter().filter(|m| v.can_read(&m.path)).collect();
            let _ = writeln!(
                out,
                "## Vault\n{} notes visible to you. Today is {}. Semantic search: {sem}.",
                visible.len(),
                chrono::Local::now().format("%Y-%m-%d (%A)")
            );
            if v.scope.read_only {
                out.push_str("This connection is read-only.\n");
            }
            if !v.scope.folders.is_empty() {
                let _ = writeln!(
                    out,
                    "This connection can only see: {}.",
                    v.scope.folders.join(", ")
                );
            }
            let pending = app.idx().pending_count()?;
            if session.scope.review && pending > 0 {
                let _ = writeln!(
                    out,
                    "{pending} remembered fact(s) are waiting for the user's approval."
                );
            }
            let mem_prefix = format!("{}/", app.settings.memory_dir);
            let topics: Vec<_> = visible
                .iter()
                .filter(|m| m.path.starts_with(&mem_prefix))
                .collect();
            if !topics.is_empty() {
                out.push_str("\n## Memory topics\n");
                for m in topics {
                    let _ = writeln!(out, "- {} ({})", m.path, m.title);
                }
            }
            out.push_str("\n## Recently changed\n");
            for m in visible.iter().take(12) {
                let _ = writeln!(out, "- {} — {} ({})", m.path, m.title, fmt_time(m.mtime));
            }
            let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
            for m in &visible {
                for t in &m.tags {
                    *counts.entry(t.as_str()).or_default() += 1;
                }
            }
            let mut tags: Vec<_> = counts.into_iter().collect();
            tags.sort_by_key(|x| std::cmp::Reverse(x.1));
            if !tags.is_empty() {
                out.push_str("\n## Top tags\n");
                out.push_str(
                    &tags
                        .iter()
                        .take(30)
                        .map(|(t, c)| format!("#{t} ({c})"))
                        .collect::<Vec<_>>()
                        .join(", "),
                );
                out.push('\n');
            }
            let soon = (chrono::Local::now() + chrono::Duration::days(7))
                .format("%Y-%m-%d")
                .to_string();
            let due = app.tasks(
                &TaskFilter {
                    due_before: Some(soon),
                    ..Default::default()
                },
                Some(&v),
            )?;
            if !due.is_empty() {
                out.push_str("\n## Open tasks due within a week\n");
                for t in due.iter().take(15) {
                    let _ = writeln!(out, "{}", task_line(t));
                }
            }
        }
        "search_notes" => {
            let q = req(a, "query")?;
            let limit = a
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(10)
                .clamp(1, 50) as usize;
            let res = app.search(q, limit * 3 + 10, s(a, "mode").unwrap_or("auto"))?;
            if let Some(w) = &res.warning {
                let _ = writeln!(out, "(note: {w}; showing keyword results)");
            }
            let hits: Vec<_> = res
                .hits
                .into_iter()
                .filter(|h| v.can_read(&h.path))
                .take(limit)
                .collect();
            if hits.is_empty() {
                return Ok(format!(
                    "No notes match '{q}'. Try fewer or different words."
                ));
            }
            for h in hits {
                let tags = if h.tags.is_empty() {
                    String::new()
                } else {
                    format!(" [#{}]", h.tags.join(" #"))
                };
                let mark = if h.semantic { " ≈" } else { "" };
                let _ = writeln!(out, "- **{}**{mark} — {}{tags}", h.path, h.title);
                if !h.snippet.is_empty() {
                    let _ = writeln!(out, "  {}", clean_snippet(&h.snippet));
                }
            }
        }
        "read_note" => {
            let path = readable(app, &v, req(a, "path")?)?;
            let n = app.read_note(&path)?;
            let back: Vec<String> = app
                .idx()
                .backlinks(&path)?
                .into_iter()
                .map(|b| b.path)
                .filter(|p| v.can_read(p))
                .collect();
            let _ = writeln!(
                out,
                "path: {}\nhash: {}\nmodified: {}",
                n.path,
                n.hash,
                fmt_time(n.mtime)
            );
            if !n.tags.is_empty() {
                let _ = writeln!(out, "tags: #{}", n.tags.join(" #"));
            }
            if !back.is_empty() {
                let _ = writeln!(out, "linked from: {}", back.join(", "));
            }
            if !v.can_write(&path) {
                out.push_str("access: read-only\n");
            }
            let _ = write!(out, "-----\n{}", n.content);
        }
        "list_notes" => {
            let recent = a.get("recent").and_then(Value::as_bool).unwrap_or(false);
            let limit = a
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(100)
                .clamp(1, 500) as usize;
            // Fetch a little more than asked (hidden notes are skipped) and grow
            // the window only while hidden notes keep it from filling up.
            let mut fetch = limit + 32;
            let notes: Vec<_> = loop {
                let rows = app.idx().list(s(a, "folder"), recent, fetch)?;
                let complete = rows.len() < fetch;
                let visible: Vec<_> = rows
                    .into_iter()
                    .filter(|m| v.can_read(&m.path))
                    .take(limit)
                    .collect();
                if complete || visible.len() >= limit {
                    break visible;
                }
                fetch = fetch.saturating_mul(4);
            };
            if notes.is_empty() {
                return Ok("No notes found.".into());
            }
            for m in notes {
                let _ = writeln!(out, "- {} — {} ({})", m.path, m.title, fmt_time(m.mtime));
            }
        }
        "get_links" => {
            let path = readable(app, &v, req(a, "path")?)?;
            let idx = app.idx();
            let _ = writeln!(out, "# Links for {path}\n\n## Backlinks");
            let back: Vec<_> = idx
                .backlinks(&path)?
                .into_iter()
                .filter(|b| v.can_read(&b.path))
                .collect();
            if back.is_empty() {
                out.push_str("(none)\n");
            }
            for b in back {
                let _ = writeln!(out, "- {} — {}", b.path, b.context);
            }
            out.push_str("\n## Outgoing\n");
            for l in idx.outgoing(&path)? {
                match l.path.filter(|p| v.can_read(p)) {
                    Some(p) => {
                        let _ = writeln!(out, "- [[{}]] → {p}", l.target);
                    }
                    None => {
                        let _ = writeln!(out, "- [[{}]] → (missing note)", l.target);
                    }
                }
            }
            let un: Vec<_> = idx
                .unlinked_mentions(&path, 20)?
                .into_iter()
                .filter(|h| v.can_read(&h.path))
                .take(10)
                .collect();
            if !un.is_empty() {
                out.push_str("\n## Unlinked mentions\n");
                for h in un {
                    let _ = writeln!(out, "- {} — {}", h.path, clean_snippet(&h.snippet));
                }
            }
        }
        "related_notes" => {
            let path = readable(app, &v, req(a, "path")?)?;
            if !app.semantic_enabled() {
                return Ok(
                    "Semantic search is not configured on this server. Use get_links instead."
                        .into(),
                );
            }
            let limit = a
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(8)
                .clamp(1, 30) as usize;
            let rel: Vec<_> = app
                .related(&path, limit * 2)?
                .into_iter()
                .filter(|r| v.can_read(&r.path))
                .take(limit)
                .collect();
            if rel.is_empty() {
                return Ok(format!("No related notes yet for {path}."));
            }
            for r in rel {
                let _ = writeln!(
                    out,
                    "- **{}** — {} (similarity {:.2})\n  {}",
                    r.path, r.title, r.score, r.snippet
                );
            }
        }
        "list_tags" => {
            let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
            for m in app
                .idx()
                .list(None, false, 1_000_000)?
                .into_iter()
                .filter(|m| v.can_read(&m.path))
            {
                for t in m.tags {
                    *counts.entry(t).or_default() += 1;
                }
            }
            if counts.is_empty() {
                return Ok("No tags yet.".into());
            }
            let mut tags: Vec<_> = counts.into_iter().collect();
            tags.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
            for (t, c) in tags {
                let _ = writeln!(out, "#{t} ({c})");
            }
        }
        "query_notes" => {
            let list = |k: &str| -> Vec<String> {
                a.get(k)
                    .and_then(Value::as_array)
                    .map(|x| {
                        x.iter()
                            .filter_map(|y| y.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let spec = crate::bases::Spec {
                from: s(a, "from").map(String::from),
                tag: s(a, "tag").map(String::from),
                filters: list("where"),
                columns: list("columns"),
                sort: s(a, "sort").map(String::from),
                limit: Some(
                    a.get("limit")
                        .and_then(Value::as_u64)
                        .unwrap_or(50)
                        .clamp(1, 500) as usize,
                ),
                view: None,
            };
            let r = app.base_query(&spec, Some(&v))?;
            if r.rows.is_empty() {
                return Ok("No notes match.".into());
            }
            let cell = |x: &Value| match x {
                Value::Null => String::new(),
                Value::String(s) => s.replace('|', "\\|"),
                Value::Array(a) => a
                    .iter()
                    .map(|y| {
                        y.as_str()
                            .map(String::from)
                            .unwrap_or_else(|| y.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                o => o.to_string(),
            };
            let _ = writeln!(out, "| path | {} |", r.columns.join(" | "));
            let _ = writeln!(out, "|---|{}", "---|".repeat(r.columns.len()));
            for row in &r.rows {
                let _ = writeln!(
                    out,
                    "| {} | {} |",
                    row.path,
                    row.values.iter().map(cell).collect::<Vec<_>>().join(" | ")
                );
            }
            if r.total > r.rows.len() {
                let _ = writeln!(out, "\n({} of {} shown)", r.rows.len(), r.total);
            }
        }
        "list_tasks" => {
            let status = s(a, "status").unwrap_or("open");
            let f = TaskFilter {
                include_done: status != "open",
                due_before: s(a, "due_before").map(String::from),
                query: s(a, "query").map(String::from),
                path: s(a, "folder").map(String::from),
            };
            let tasks: Vec<_> = app
                .tasks(&f, Some(&v))?
                .into_iter()
                .filter(|t| status != "done" || t.done)
                .take(200)
                .collect();
            if tasks.is_empty() {
                return Ok("No tasks found.".into());
            }
            for t in tasks {
                let _ = writeln!(out, "{}", task_line(&t));
            }
        }
        "note_history" => {
            if let Some(id) = a.get("revision_id").and_then(Value::as_i64) {
                let (p, content) = visible_revision(app, &v, id)?;
                let _ = write!(out, "revision {id} of {p}\n-----\n{content}");
            } else {
                let raw = req(a, "path")?;
                let path = app
                    .find_note(raw)
                    .or_else(|_| Vault::clean_note(raw).map_err(bad_path))?;
                if !v.can_read(&path) {
                    return Err(hidden(raw));
                }
                // Private versions (and what came before them) are left out.
                let revs = app.idx().ai_revisions(&path, 50)?;
                if revs.is_empty() {
                    return Ok(format!("No earlier versions of {path}."));
                }
                let _ = writeln!(
                    out,
                    "Earlier versions of {path} (newest first). Each is the content *before* the change by 'replaced by':"
                );
                for r in &revs {
                    let _ = writeln!(
                        out,
                        "- revision_id {} — {} — replaced by {} — {} bytes",
                        r.id,
                        fmt_time(r.created_at),
                        r.source,
                        r.size
                    );
                }
            }
        }
        "daily_note" => {
            let period = s(a, "period").unwrap_or("day");
            let path = app.period_path(period, s(a, "date"))?;
            match s(a, "append").filter(|t| !t.trim().is_empty()) {
                Some(text) => {
                    if session.scope.read_only {
                        return Err(AppError::BadRequest("this connection is read-only".into()));
                    }
                    writable(app, &v, &path)?;
                    let path = app.ensure_period(period, s(a, "date"), &src)?;
                    let time = chrono::Local::now().format("%H:%M");
                    let text = if text.trim_start().starts_with(['-', '#', '>', '*']) {
                        text.to_string()
                    } else {
                        format!("- {time} {}", text.trim())
                    };
                    let r = app.append(&path, &text, None, &src)?;
                    let _ = writeln!(out, "Appended to {} (hash {}).", r.path, r.hash);
                }
                None => {
                    if !v.can_read(&path) {
                        return Err(hidden(&path));
                    }
                    match app.read_note(&path) {
                        Ok(n) => {
                            let _ = write!(
                                out,
                                "path: {}\nhash: {}\n-----\n{}",
                                n.path, n.hash, n.content
                            );
                        }
                        Err(_) => out = format!("No journal note for {path} yet."),
                    }
                }
            }
        }
        "memory_review" => {
            let r = app.memory_review(Some(&v))?;
            let _ = writeln!(out, "{} facts in the memory folder.", r["facts"]);
            let dups = r["duplicates"].as_array().cloned().unwrap_or_default();
            if dups.is_empty() {
                out.push_str("No repeated facts found.\n");
            } else {
                out.push_str("\n## Possible repeats (merge with edit_note, keep the newer/more precise one)\n");
                for d in dups.iter().take(25) {
                    let _ = writeln!(
                        out,
                        "- {}:{} «{}»\n  {}:{} «{}» (similarity {:.2})",
                        d["a"]["path"].as_str().unwrap_or(""),
                        d["a"]["line"],
                        d["a"]["text"].as_str().unwrap_or(""),
                        d["b"]["path"].as_str().unwrap_or(""),
                        d["b"]["line"],
                        d["b"]["text"].as_str().unwrap_or(""),
                        d["score"].as_f64().unwrap_or(0.0)
                    );
                }
            }
            let old = r["old"].as_array().cloned().unwrap_or_default();
            if !old.is_empty() {
                out.push_str("\n## Older than 6 months (may be outdated — confirm with the user before changing)\n");
                for f in old.iter().take(25) {
                    let _ = writeln!(
                        out,
                        "- {}:{} «{}» ({})",
                        f["path"].as_str().unwrap_or(""),
                        f["line"],
                        f["text"].as_str().unwrap_or(""),
                        f["date"].as_str().unwrap_or("")
                    );
                }
            }
        }
        "remember" => {
            let tags: Vec<String> = a
                .get("tags")
                .and_then(Value::as_array)
                .map(|x| {
                    x.iter()
                        .filter_map(|t| t.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let topic = s(a, "topic");
            // Check the note `remember` will really write to (its topic clean-up
            // turns `/` into `-` and drops leading dots).
            writable_exact(app, &v, &app.memory_topic_path(topic))?;
            let o =
                app.remember_or_queue(req(a, "fact")?, topic, &tags, &src, session.scope.review)?;
            if o.queued {
                out.push_str(
                    "Saved for the user's approval — it will appear in memory once they accept it.",
                );
            } else {
                let _ = write!(out, "Remembered in {}.", o.path.unwrap_or_default());
            }
        }
        "write_note" => {
            let p = writable(app, &v, req(a, "path")?)?;
            let r = app.write_note(&p, req(a, "content")?, s(a, "expected_hash"), &src)?;
            let _ = write!(
                out,
                "{} {} (hash {}).",
                if r.created { "Created" } else { "Updated" },
                r.path,
                r.hash
            );
        }
        "edit_note" => {
            let p = writable(app, &v, req(a, "path")?)?;
            let r = app.edit_note(
                &p,
                req(a, "old_text")?,
                a.get("new_text").and_then(Value::as_str).unwrap_or(""),
                s(a, "expected_hash"),
                &src,
            )?;
            let _ = write!(out, "Edited {} (hash {}).", r.path, r.hash);
        }
        "append_to_note" => {
            let p = writable(app, &v, req(a, "path")?)?;
            let r = app.append(&p, req(a, "text")?, s(a, "heading"), &src)?;
            let _ = write!(
                out,
                "{} {} (hash {}).",
                if r.created { "Created" } else { "Appended to" },
                r.path,
                r.hash
            );
        }
        "add_task" => {
            let target = match s(a, "path") {
                Some(p) => Some(writable(app, &v, p)?),
                None => {
                    writable(app, &v, &app.daily_path(None)?)?;
                    None
                }
            };
            let r = app.add_task(req(a, "text")?, s(a, "due"), target.as_deref(), &src)?;
            let _ = write!(out, "Added task to {}.", r.path);
        }
        "complete_task" => {
            let p = writable(app, &v, req(a, "path")?)?;
            let all = app.tasks(
                &TaskFilter {
                    include_done: true,
                    ..Default::default()
                },
                Some(&v),
            )?;
            let mine: Vec<_> = all.into_iter().filter(|t| t.path == p).collect();
            let t = match (a.get("line").and_then(Value::as_u64), s(a, "text")) {
                (Some(l), _) => mine.into_iter().find(|t| t.line as u64 == l),
                (None, Some(q)) => {
                    let q = q.to_lowercase();
                    let m: Vec<_> = mine
                        .into_iter()
                        .filter(|t| t.text.to_lowercase().contains(&q))
                        .collect();
                    if m.len() > 1 {
                        return Err(AppError::BadRequest(format!(
                            "{} tasks match '{q}'; pass line instead",
                            m.len()
                        )));
                    }
                    m.into_iter().next()
                }
                _ => return Err(AppError::BadRequest("pass line or text".into())),
            }
            .ok_or_else(|| AppError::NotFound("no such task in that note".into()))?;
            app.toggle_task(&p, t.line as usize, &t.raw, &src)?;
            let _ = write!(
                out,
                "{} task «{}» in {p}.",
                if t.done { "Reopened" } else { "Completed" },
                t.text
            );
        }
        "move_note" => {
            let from_raw = req(a, "from")?;
            let to_raw = req(a, "to")?;
            let folder = Vault::clean(from_raw)
                .ok()
                .filter(|f| app.vault.abs(f).is_dir());
            if let Some(f) = folder {
                if !v.can_read(&format!("{f}/x.md")) {
                    return Err(hidden(from_raw));
                }
                // Check the destination as rename_folder will normalise it.
                let to = Vault::clean(to_raw).map_err(bad_path)?;
                let prefix = format!("{f}/");
                let inside: Vec<_> = app.idx().list(Some(&f), false, 1_000_000)?;
                // Every moved note must be writable where it is and where it lands.
                let lands_ok = |p: &str| {
                    p.strip_prefix(&prefix)
                        .is_none_or(|rest| v.can_write(&format!("{to}/{rest}")))
                };
                if !v.can_write(&format!("{prefix}x.md"))
                    || !v.can_write(&format!("{to}/x.md"))
                    || inside
                        .iter()
                        .any(|m| !v.can_write(&m.path) || !lands_ok(&m.path))
                {
                    return Err(AppError::BadRequest(
                        "this connection may not move that folder".into(),
                    ));
                }
                let (to, moved, n) = app.rename_folder(&f, &to, &src)?;
                let _ = write!(
                    out,
                    "Moved folder {f} → {to} ({moved} files); updated links in {n} note(s)."
                );
            } else {
                let from = writable(app, &v, from_raw)?;
                if !app.vault.exists(&from) {
                    return Err(hidden(from_raw));
                }
                // rename_note uses the destination as given (no lookup by name),
                // so check exactly that path.
                let to = writable_exact(app, &v, to_raw)?;
                let (to, n) = app.rename_note(&from, &to, &src)?;
                let _ = write!(out, "Moved {from} → {to}; updated links in {n} note(s).");
            }
        }
        "delete_note" => {
            let p = writable(app, &v, req(a, "path")?)?;
            if !app.vault.exists(&p) {
                return Err(hidden(&p));
            }
            app.delete_note(&p, &src)?;
            let _ = write!(
                out,
                "Moved {p} to trash. It can be restored with note_history / restore_revision."
            );
        }
        "restore_revision" => {
            let id = a
                .get("revision_id")
                .and_then(Value::as_i64)
                .ok_or_else(|| AppError::BadRequest("missing argument: revision_id".into()))?;
            let (p, _) = visible_revision(app, &v, id)?;
            // The revision is written back to its own path; no lookup by name.
            writable_exact(app, &v, &p)?;
            let r = app.restore_revision(id, &src)?;
            let _ = write!(
                out,
                "Restored {} from revision {id} (hash {}).",
                r.path, r.hash
            );
        }
        other => return Err(AppError::BadRequest(format!("unknown tool: {other}"))),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Settings;

    fn setup(read_only: bool) -> (tempfile::TempDir, App, Session) {
        let d = tempfile::tempdir().unwrap();
        let app = App::open(d.path(), None, Settings::default()).unwrap();
        app.seed_if_empty().unwrap();
        (
            d,
            app,
            Session::new(Scope::full("default", read_only, false), false),
        )
    }

    fn call(app: &App, s: &mut Session, name: &str, args: Value) -> (String, bool) {
        let r = handle(app, s, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}})).unwrap();
        let res = &r["result"];
        (
            res["content"][0]["text"].as_str().unwrap().to_string(),
            res["isError"].as_bool().unwrap(),
        )
    }

    #[test]
    fn initialize_and_list() {
        let (_d, app, mut s) = setup(false);
        let r = handle(&app, &mut s, json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"Claude Code"}}})).unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(s.client, "claude-code");
        assert_eq!(s.source(), "mcp:claude-code");
        assert!(
            handle(
                &app,
                &mut s,
                json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            )
            .is_none()
        );
        let r = handle(
            &app,
            &mut s,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .unwrap();
        assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 21);
        let r = handle(
            &app,
            &mut s,
            json!({"jsonrpc":"2.0","id":3,"method":"nope"}),
        )
        .unwrap();
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn memory_roundtrip() {
        let (_d, app, mut s) = setup(false);
        s.client = "gemini-cli".into();
        let (t, e) = call(&app, &mut s, "memory_overview", json!({}));
        assert!(
            !e && t.contains("MEMORY.md") && t.contains("Memory/Inbox.md"),
            "{t}"
        );
        let (t, e) = call(
            &app,
            &mut s,
            "remember",
            json!({"fact":"Sunucusu Oracle Cloud'da","topic":"Sunucular","tags":["infra"]}),
        );
        assert!(!e, "{t}");
        let (t, _) = call(&app, &mut s, "search_notes", json!({"query":"oracle"}));
        assert!(t.contains("Memory/Sunucular.md"), "{t}");
        let (t, e) = call(
            &app,
            &mut s,
            "edit_note",
            json!({"path":"Sunucular","old_text":"Oracle","new_text":"OCI"}),
        );
        assert!(!e, "{t}");
        let (t, _) = call(
            &app,
            &mut s,
            "note_history",
            json!({"path":"Memory/Sunucular.md"}),
        );
        assert!(t.contains("replaced by mcp:gemini-cli"), "{t}");
        let (t, e) = call(
            &app,
            &mut s,
            "daily_note",
            json!({"append":"MindCrab kuruldu"}),
        );
        assert!(!e, "{t}");
        let (t, _) = call(&app, &mut s, "daily_note", json!({}));
        assert!(t.contains("MindCrab kuruldu"), "{t}");
        let (t, e) = call(
            &app,
            &mut s,
            "daily_note",
            json!({"period":"week","append":"hafta"}),
        );
        assert!(!e && t.contains("-W"), "{t}");
        let (t, e) = call(
            &app,
            &mut s,
            "write_note",
            json!({"path":"x","content":"y","expected_hash":"bogus"}),
        );
        assert!(e && t.contains("changed by someone else"), "{t}");
    }

    #[test]
    fn hidden_notes_are_invisible() {
        let (_d, app, mut s) = setup(false);
        app.write_note("Sağlık/Tahlil.md", "kan değerleri gizli", None, "web")
            .unwrap();
        app.write_note(
            "Günlük.md",
            "---\nai: false\n---\nözel düşünceler [[Açık]]",
            None,
            "web",
        )
        .unwrap();
        app.write_note("Açık.md", "herkese açık", None, "web")
            .unwrap();
        app.set_ai_ignore("Sağlık/\n").unwrap();
        for (tool, args) in [
            ("read_note", json!({"path":"Sağlık/Tahlil.md"})),
            ("read_note", json!({"path":"Günlük"})),
            ("write_note", json!({"path":"Günlük.md","content":"x"})),
            ("delete_note", json!({"path":"Sağlık/Tahlil.md"})),
            ("note_history", json!({"path":"Günlük.md"})),
        ] {
            let (t, e) = call(&app, &mut s, tool, args.clone());
            assert!(e && t.contains("no note matches"), "{tool} {args}: {t}");
        }
        let (t, _) = call(&app, &mut s, "search_notes", json!({"query":"gizli"}));
        assert!(!t.contains("Tahlil"), "{t}");
        let (t, _) = call(&app, &mut s, "search_notes", json!({"query":"özel"}));
        assert!(!t.contains("Günlük"), "{t}");
        let (t, _) = call(&app, &mut s, "get_links", json!({"path":"Açık"}));
        assert!(!t.contains("Günlük"), "{t}");
        let (t, _) = call(&app, &mut s, "list_notes", json!({}));
        assert!(!t.contains("Sağlık") && !t.contains("Günlük"), "{t}");
        let (t, e) = call(
            &app,
            &mut s,
            "write_note",
            json!({"path":"Sağlık/Yeni.md","content":"x"}),
        );
        assert!(e, "{t}");
    }

    #[test]
    fn scoped_connection() {
        let (_d, app, _) = setup(false);
        app.write_note("Projeler/MindCrab.md", "# MindCrab", None, "web")
            .unwrap();
        let scope = Scope {
            name: "gemini".into(),
            read_only: false,
            review: true,
            folders: vec!["Projeler".into(), "Memory".into()],
        };
        let mut s = Session::new(scope, true);
        let (t, _) = call(&app, &mut s, "list_notes", json!({}));
        assert!(
            t.contains("Projeler/MindCrab.md") && !t.contains("Welcome"),
            "{t}"
        );
        let (t, e) = call(&app, &mut s, "read_note", json!({"path":"Welcome"}));
        assert!(e, "{t}");
        let (t, e) = call(
            &app,
            &mut s,
            "write_note",
            json!({"path":"Başka.md","content":"x"}),
        );
        assert!(e && t.contains("may not change"), "{t}");
        let (t, e) = call(
            &app,
            &mut s,
            "append_to_note",
            json!({"path":"Projeler/MindCrab.md","text":"ek"}),
        );
        assert!(!e, "{t}");
        let (t, _) = call(&app, &mut s, "remember", json!({"fact":"Kediyi sever"}));
        assert!(t.contains("approval"), "{t}");
        assert_eq!(app.pending_facts().unwrap()[0].source, "mcp:gemini");
        let hist = app.activity(true, None, 5).unwrap();
        assert_eq!(hist[0].source, "mcp:gemini");
    }

    #[test]
    fn tasks_and_queries() {
        let (_d, app, mut s) = setup(false);
        let (t, e) = call(
            &app,
            &mut s,
            "add_task",
            json!({"text":"Faturayı öde","due":"2026-12-01"}),
        );
        assert!(!e, "{t}");
        let (t, _) = call(&app, &mut s, "list_tasks", json!({}));
        assert!(t.contains("Faturayı öde 📅 2026-12-01"), "{t}");
        let path = t
            .split(" — ")
            .nth(1)
            .unwrap()
            .split(':')
            .next()
            .unwrap()
            .to_string();
        let (t, e) = call(
            &app,
            &mut s,
            "complete_task",
            json!({"path": path, "text":"fatura"}),
        );
        assert!(!e && t.contains("Completed"), "{t}");
        let (t, _) = call(&app, &mut s, "list_tasks", json!({"status":"done"}));
        assert!(t.contains("[x]"), "{t}");
        app.write_note(
            "Kitap/1984.md",
            "---\nyazar: Orwell\npuan: 5\n---\n",
            None,
            "web",
        )
        .unwrap();
        let (t, _) = call(
            &app,
            &mut s,
            "query_notes",
            json!({"from":"Kitap","where":["puan >= 4"],"columns":["title","yazar"]}),
        );
        assert!(t.contains("| Kitap/1984.md | 1984 | Orwell |"), "{t}");
        let (t, _) = call(&app, &mut s, "memory_review", json!({}));
        assert!(t.contains("facts in the memory folder"), "{t}");
    }

    #[test]
    fn move_note_stays_in_scope() {
        let (_d, app, _) = setup(false);
        app.write_note("Projeler/A.md", "a", None, "web").unwrap();
        app.write_note("Projeler/Hedef.md", "h", None, "web")
            .unwrap();
        let scope = Scope {
            name: "g".into(),
            read_only: false,
            review: false,
            folders: vec!["Projeler".into()],
        };
        let mut s = Session::new(scope, true);
        // "Hedef" resolves to Projeler/Hedef.md by name, but the move itself
        // would use Hedef.md at the vault root.
        for to in ["Hedef", "Kök.md", "Başka\\A.md", " /Başka/A "] {
            let (t, e) = call(
                &app,
                &mut s,
                "move_note",
                json!({"from":"Projeler/A.md","to":to}),
            );
            assert!(e && t.contains("may not change"), "{to}: {t}");
        }
        assert!(app.vault.exists("Projeler/A.md"));
        assert!(!app.vault.exists("Hedef.md") && !app.vault.exists("Başka/A.md"));
        let (t, e) = call(
            &app,
            &mut s,
            "move_note",
            json!({"from":"A","to":"Projeler\\Alt\\B"}),
        );
        assert!(!e, "{t}");
        assert!(app.vault.exists("Projeler/Alt/B.md"));
    }

    #[test]
    fn folder_move_checks_normalised_target() {
        let (_d, app, mut s) = setup(false);
        app.write_note("Taşı/n.md", "x", None, "web").unwrap();
        app.write_note("Gizli/var.md", "y", None, "web").unwrap();
        app.set_ai_ignore("Gizli/\n").unwrap();
        for to in ["Gizli\\alt", "Gizli/alt", "/Gizli/alt/", " gizli\\alt "] {
            let (t, e) = call(&app, &mut s, "move_note", json!({"from":"Taşı","to":to}));
            assert!(e && t.contains("may not move"), "{to}: {t}");
        }
        assert!(app.vault.exists("Taşı/n.md"));
        assert!(!app.vault.abs("Gizli/alt").exists());
        // A hidden folder looks missing as a source.
        let (t, e) = call(
            &app,
            &mut s,
            "move_note",
            json!({"from":"Gizli","to":"Açık"}),
        );
        assert!(e && t.contains("no note matches"), "{t}");
        assert!(app.vault.exists("Gizli/var.md"));
        let (t, e) = call(
            &app,
            &mut s,
            "move_note",
            json!({"from":"Taşı","to":"Yeni\\Yer"}),
        );
        assert!(!e, "{t}");
        assert!(app.vault.exists("Yeni/Yer/n.md"));
    }

    #[test]
    fn private_history_stays_hidden() {
        let (_d, app, mut s) = setup(false);
        // Made private, then deleted.
        app.write_note("Sır.md", "ilk sürüm: hassas", None, "t")
            .unwrap();
        app.write_note("Sır.md", "---\nai: false\n---\nhassas", None, "t")
            .unwrap();
        app.delete_note("Sır.md", "web").unwrap();
        // Private once (flag with an inline comment), public again now.
        app.write_note(
            "Dönen.md",
            "---\nai: \"false # gizli\"\n---\neski sır",
            None,
            "t",
        )
        .unwrap();
        app.write_note("Dönen.md", "artık açık", None, "t").unwrap();
        // Deleted, in a folder that is ignored now.
        app.write_note("Özel/n.md", "özel", None, "t").unwrap();
        app.delete_note("Özel/n.md", "web").unwrap();
        app.set_ai_ignore("Özel/\n").unwrap();

        let ids: Vec<i64> = ["Sır.md", "Dönen.md", "Özel/n.md"]
            .iter()
            .flat_map(|p| app.idx().revisions(p).unwrap())
            .map(|r| r.id)
            .collect();
        assert_eq!(ids.len(), 4);
        for id in &ids {
            for tool in ["note_history", "restore_revision"] {
                let (t, e) = call(&app, &mut s, tool, json!({"revision_id": id}));
                assert!(e && t.contains("not found"), "{tool} {id}: {t}");
            }
        }
        assert!(!app.vault.exists("Sır.md") && !app.vault.exists("Özel/n.md"));
        assert_eq!(app.read_note("Dönen.md").unwrap().content, "artık açık");
        for p in ["Sır.md", "Sır", "Dönen.md"] {
            let (t, e) = call(&app, &mut s, "note_history", json!({"path": p}));
            assert!(!e && t.contains("No earlier versions"), "{p}: {t}");
        }
        let (t, e) = call(&app, &mut s, "note_history", json!({"path":"Özel/n.md"}));
        assert!(e && t.contains("no note matches"), "{t}");

        // An ordinary deleted note can still be found and brought back.
        app.write_note("Silinen.md", "geri gelecek", None, "t")
            .unwrap();
        let (t, e) = call(&app, &mut s, "delete_note", json!({"path":"Silinen.md"}));
        assert!(!e, "{t}");
        let (t, e) = call(&app, &mut s, "note_history", json!({"path":"Silinen.md"}));
        assert!(!e && t.contains("revision_id"), "{t}");
        let id = app.idx().revisions("Silinen.md").unwrap()[0].id;
        let (t, e) = call(&app, &mut s, "note_history", json!({"revision_id": id}));
        assert!(!e && t.contains("geri gelecek"), "{t}");
        let (t, e) = call(&app, &mut s, "restore_revision", json!({"revision_id": id}));
        assert!(!e, "{t}");
        assert_eq!(app.read_note("Silinen.md").unwrap().content, "geri gelecek");
    }

    #[test]
    fn remember_checks_the_real_topic_note() {
        let (_d, app, mut s) = setup(false);
        app.set_ai_ignore("Memory/Gizli-*\n").unwrap();
        // `/` becomes `-`, so this would land in the ignored Memory/Gizli-Konu.md.
        let (t, e) = call(
            &app,
            &mut s,
            "remember",
            json!({"fact":"x","topic":"Gizli/Konu"}),
        );
        assert!(e && t.contains("may not change"), "{t}");
        assert!(!app.vault.exists("Memory/Gizli-Konu.md"));
        let (t, e) = call(
            &app,
            &mut s,
            "remember",
            json!({"fact":"y","topic":"..Açık"}),
        );
        assert!(!e && t.contains("Memory/Açık.md"), "{t}");
        let (t, e) = call(&app, &mut s, "remember", json!({"fact":"z","topic":"   "}));
        assert!(!e && t.contains("Memory/Inbox.md"), "{t}");
    }

    #[test]
    fn batch_limits() {
        let (_d, app, mut s) = setup(false);
        let ping = |id: usize| json!({"jsonrpc":"2.0","id":id,"method":"ping"});
        let r = handle(
            &app,
            &mut s,
            Value::Array((0..MAX_BATCH).map(ping).collect()),
        )
        .unwrap();
        assert_eq!(r.as_array().unwrap().len(), MAX_BATCH);
        let r = handle(
            &app,
            &mut s,
            Value::Array((0..=MAX_BATCH).map(ping).collect()),
        )
        .unwrap();
        assert_eq!(r["error"]["code"], -32600);
        assert!(r["id"].is_null());
        let r = handle(&app, &mut s, json!([])).unwrap();
        assert_eq!(r["error"]["code"], -32600);
        let r = handle(&app, &mut s, json!([[ping(1)], ping(2)])).unwrap();
        assert_eq!(r[0]["error"]["code"], -32600);
        assert_eq!(r[1]["id"], 2);
        assert!(
            handle(
                &app,
                &mut s,
                json!([{"jsonrpc":"2.0","method":"notifications/initialized"}])
            )
            .is_none()
        );
    }

    #[test]
    fn list_notes_pages_past_hidden_notes() {
        let (_d, app, mut s) = setup(false);
        for i in 0..80 {
            app.write_note(&format!("A/{i:02}.md"), "gizli", None, "t")
                .unwrap();
        }
        app.write_note("B/x.md", "x", None, "t").unwrap();
        app.write_note("B/y.md", "y", None, "t").unwrap();
        app.set_ai_ignore("A/\n").unwrap();
        let (t, e) = call(&app, &mut s, "list_notes", json!({"limit": 2}));
        assert!(!e, "{t}");
        assert_eq!(t.lines().count(), 2, "{t}");
        assert!(!t.contains("A/"), "{t}");
        let (t, _) = call(&app, &mut s, "list_notes", json!({"folder":"B","limit": 5}));
        assert!(t.contains("B/x.md") && t.contains("B/y.md"), "{t}");
    }

    #[test]
    fn read_only_hides_writes() {
        let (_d, app, mut s) = setup(true);
        let r = handle(
            &app,
            &mut s,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .unwrap();
        let names: Vec<_> = r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(!names.contains(&"write_note".to_string()));
        let r = handle(&app, &mut s, json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"write_note","arguments":{"path":"a","content":"b"}}})).unwrap();
        assert_eq!(r["error"]["code"], -32602);
        let (_, e) = call(&app, &mut s, "daily_note", json!({"append":"x"}));
        assert!(e);
    }
}
