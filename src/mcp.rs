//! Model Context Protocol server (JSON-RPC 2.0). Shared by the Streamable HTTP
//! endpoint (`POST /mcp`) and the stdio transport (`mindcrab mcp`).

use crate::app::{App, AppError};
use crate::index::{SNIP_END, SNIP_START};
use serde_json::{Value, json};
use std::fmt::Write as _;

pub const SUPPORTED_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "MindCrab is the user's personal Markdown notebook and long-term memory. \
At the start of a conversation where past context could help, call memory_overview. \
Search (search_notes) before writing so you update existing notes instead of creating duplicates. \
Use remember for small durable facts, append_to_note/edit_note for targeted changes, and write_note only for new notes or full rewrites (pass expected_hash). \
Link related notes with [[wikilinks]] and group with #tags. Write in the user's language. \
Never store secrets (passwords, API keys, card or ID numbers). All changes are versioned and can be restored.";

pub struct Session {
    pub client: String,
    pub read_only: bool,
}

impl Session {
    pub fn source(&self) -> String {
        format!("mcp:{}", self.client)
    }
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: &Value, code: i64, msg: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
}

/// Handles one JSON-RPC message (or batch). Returns `None` for notifications.
pub fn handle(app: &App, session: &mut Session, msg: Value) -> Option<Value> {
    if let Value::Array(items) = msg {
        let out: Vec<Value> = items
            .into_iter()
            .filter_map(|m| handle(app, session, m))
            .collect();
        return if out.is_empty() {
            None
        } else {
            Some(Value::Array(out))
        };
    }
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let Some(id) = id else {
        // Notification (initialized, cancelled, …) or a response from the client.
        return None;
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
        "tools/list" => rpc_result(&id, json!({"tools": tool_defs(session.read_only)})),
        "resources/list" => rpc_result(&id, json!({"resources": []})),
        "resources/templates/list" => rpc_result(&id, json!({"resourceTemplates": []})),
        "prompts/list" => rpc_result(&id, json!({"prompts": []})),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            if !tool_defs(session.read_only)
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

pub fn tool_defs(read_only: bool) -> Vec<Value> {
    let path = json!({"type": "string", "description": "Note path relative to the vault, e.g. 'Projects/MindCrab.md' (.md optional). A note name or title also works."});
    let hash = json!({"type": "string", "description": "Optional. The hash from read_note; the write fails if the note changed since. Use 'new' to require that the note does not exist yet."});
    let mut tools = vec![
        tool(
            "memory_overview",
            "Memory overview",
            "Start here. Returns the MEMORY.md guide/index written by the user, recently changed notes, memory topics and top tags.",
            schema(json!({}), &[]),
            true,
            false,
        ),
        tool(
            "search_notes",
            "Search notes",
            "Searches all notes by words and — when semantic search is configured — by meaning, so it also finds notes phrased differently (results marked ≈). Diacritics are ignored (seker finds şeker). Filters: tag:name, path:Folder, \"exact phrase\". Use this to recall what the user told you before.",
            schema(
                json!({
                    "query": {"type": "string", "description": "Search words and optional filters, e.g. 'docker caddy tag:server'"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 10},
                    "mode": {"type": "string", "enum": ["auto", "keyword", "semantic"], "default": "auto", "description": "auto = keywords + meaning (when semantic search is configured)"}
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
                    "recent": {"type": "boolean", "default": false, "description": "Sort by last modified, newest first"},
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
            "Shows the knowledge graph around a note: backlinks (with context), outgoing links (resolved or missing) and unlinked mentions.",
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
            "note_history",
            "Note history",
            "Lists earlier versions of a note (who changed it and when). Pass revision_id to read that version's content.",
            schema(
                json!({"path": path, "revision_id": {"type": "integer", "description": "Optional: return this revision's full content"}}),
                &["path"],
            ),
            true,
            false,
        ),
        tool(
            "daily_note",
            "Daily note",
            "Reads the journal note for a day (default today). With 'append', adds text to it (created if missing) — use for a log of what happened or was decided.",
            schema(
                json!({
                    "date": {"type": "string", "description": "YYYY-MM-DD, default today"},
                    "append": {"type": "string", "description": "Optional Markdown to append (write access required)"}
                }),
                &[],
            ),
            read_only,
            false,
        ),
    ];
    if read_only {
        return tools;
    }
    tools.extend([
        tool(
            "remember",
            "Remember a fact",
            "Saves one durable fact about the user or their world as a dated bullet in Memory/<topic>.md. Keep it to one self-contained sentence. Reuse an existing topic (see memory_overview) when one fits.",
            schema(
                json!({
                    "fact": {"type": "string", "description": "The fact, e.g. 'Prefers Fish shell over bash'"},
                    "topic": {"type": "string", "description": "Topic note name, e.g. 'Preferences', 'Servers', 'People'. Default: Inbox"},
                    "tags": {"type": "array", "items": {"type": "string"}, "description": "Optional extra tags"}
                }),
                &["fact"],
            ),
            false,
            false,
        ),
        tool(
            "write_note",
            "Write note",
            "Creates a note or replaces its whole content. Prefer edit_note/append_to_note for changes to existing notes. Folders are created automatically.",
            schema(
                json!({"path": path, "content": {"type": "string", "description": "Full Markdown content (frontmatter allowed)"}, "expected_hash": hash}),
                &["path", "content"],
            ),
            false,
            true,
        ),
        tool(
            "edit_note",
            "Edit note",
            "Replaces one exact occurrence of old_text with new_text. old_text must match exactly once (include surrounding text if needed). Empty new_text deletes.",
            schema(
                json!({
                    "path": path,
                    "old_text": {"type": "string"},
                    "new_text": {"type": "string"},
                    "expected_hash": hash
                }),
                &["path", "old_text", "new_text"],
            ),
            false,
            true,
        ),
        tool(
            "append_to_note",
            "Append to note",
            "Appends Markdown to the end of a note, or to the end of a heading's section. Creates the note (and the heading) if missing.",
            schema(
                json!({
                    "path": path,
                    "text": {"type": "string"},
                    "heading": {"type": "string", "description": "Optional heading text, e.g. 'Decisions'"}
                }),
                &["path", "text"],
            ),
            false,
            false,
        ),
        tool(
            "move_note",
            "Move / rename note",
            "Renames or moves a note — or a whole folder — and updates every [[link]] that points to the moved notes.",
            schema(json!({"from": path, "to": {"type": "string", "description": "New path"}}), &["from", "to"]),
            false,
            false,
        ),
        tool(
            "delete_note",
            "Delete note",
            "Moves a note to the vault trash. Only do this when the user asks. It can be restored from history.",
            schema(json!({"path": path}), &["path"]),
            false,
            true,
        ),
        tool(
            "restore_revision",
            "Restore revision",
            "Restores a note to an earlier version from note_history.",
            schema(json!({"revision_id": {"type": "integer"}}), &["revision_id"]),
            false,
            true,
        ),
    ]);
    tools
}

fn s<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn req<'a>(args: &'a Value, key: &str) -> Result<&'a str, AppError> {
    s(args, key).ok_or_else(|| AppError::BadRequest(format!("missing argument: {key}")))
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

pub fn call_tool(app: &App, session: &Session, name: &str, a: &Value) -> Result<String, AppError> {
    let src = session.source();
    let mut out = String::new();
    match name {
        "memory_overview" => {
            match app.read_note("MEMORY.md") {
                Ok(n) => {
                    let _ = writeln!(
                        out,
                        "# MEMORY.md (hash {})\n\n{}\n",
                        n.hash,
                        n.content.trim()
                    );
                }
                Err(_) => out.push_str(
                    "(No MEMORY.md yet. You may create one as an index for future sessions.)\n\n",
                ),
            }
            let es = app.embed_status();
            let sem = if es["enabled"] == true {
                format!(
                    "on ({}, {}/{} chunks indexed)",
                    es["model"].as_str().unwrap_or(""),
                    es["done"],
                    es["total"]
                )
            } else {
                "off (keyword search only)".to_string()
            };
            let idx = app.idx();
            let _ = writeln!(out, "Semantic search: {sem}.");
            let _ = writeln!(
                out,
                "## Vault\n{} notes. Today is {}.\n",
                idx.count()?,
                chrono::Local::now().format("%Y-%m-%d (%A)")
            );
            let mem = idx.list(Some(&app.settings.memory_dir), false, 200)?;
            if !mem.is_empty() {
                out.push_str("## Memory topics\n");
                for m in mem {
                    let _ = writeln!(out, "- {} ({})", m.path, m.title);
                }
                out.push('\n');
            }
            out.push_str("## Recently changed\n");
            for m in idx.list(None, true, 12)? {
                let _ = writeln!(out, "- {} — {} ({})", m.path, m.title, fmt_time(m.mtime));
            }
            let tags = idx.tag_counts()?;
            if !tags.is_empty() {
                out.push_str("\n## Top tags\n");
                let t: Vec<String> = tags
                    .iter()
                    .take(30)
                    .map(|(t, c)| format!("#{t} ({c})"))
                    .collect();
                out.push_str(&t.join(", "));
                out.push('\n');
            }
        }
        "search_notes" => {
            let q = req(a, "query")?;
            let limit = a
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(10)
                .clamp(1, 50) as usize;
            let mode = s(a, "mode").unwrap_or("auto");
            let res = app.search(q, limit, mode)?;
            if let Some(w) = &res.warning {
                let _ = writeln!(out, "(note: {w}; showing keyword results)");
            }
            let hits = res.hits;
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
            let path = app.find_note(req(a, "path")?)?;
            let n = app.read_note(&path)?;
            let back = app.idx().backlinks(&path)?;
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
                let b: Vec<&str> = back.iter().map(|b| b.path.as_str()).collect();
                let _ = writeln!(out, "linked from: {}", b.join(", "));
            }
            let _ = write!(out, "-----\n{}", n.content);
        }
        "list_notes" => {
            let folder = s(a, "folder");
            let recent = a.get("recent").and_then(Value::as_bool).unwrap_or(false);
            let limit = a
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(100)
                .clamp(1, 500) as usize;
            let notes = app.idx().list(folder, recent, limit)?;
            if notes.is_empty() {
                return Ok("No notes found.".into());
            }
            for m in notes {
                let _ = writeln!(out, "- {} — {} ({})", m.path, m.title, fmt_time(m.mtime));
            }
        }
        "get_links" => {
            let path = app.find_note(req(a, "path")?)?;
            let idx = app.idx();
            let _ = writeln!(out, "# Links for {path}\n\n## Backlinks");
            let back = idx.backlinks(&path)?;
            if back.is_empty() {
                out.push_str("(none)\n");
            }
            for b in back {
                let _ = writeln!(out, "- {} — {}", b.path, b.context);
            }
            out.push_str("\n## Outgoing\n");
            for l in idx.outgoing(&path)? {
                match l.path {
                    Some(p) => {
                        let _ = writeln!(out, "- [[{}]] → {p}", l.target);
                    }
                    None => {
                        let _ = writeln!(out, "- [[{}]] → (missing note)", l.target);
                    }
                }
            }
            let un = idx.unlinked_mentions(&path, 10)?;
            if !un.is_empty() {
                out.push_str("\n## Unlinked mentions\n");
                for h in un {
                    let _ = writeln!(out, "- {} — {}", h.path, clean_snippet(&h.snippet));
                }
            }
        }
        "related_notes" => {
            let path = app.find_note(req(a, "path")?)?;
            if !app.semantic_enabled() {
                return Ok("Semantic search is not configured on this server (MINDCRAB_EMBED_URL / MINDCRAB_EMBED_MODEL). Use get_links instead.".into());
            }
            let limit = a
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(8)
                .clamp(1, 30) as usize;
            let rel = app.related(&path, limit)?;
            if rel.is_empty() {
                return Ok(format!(
                    "No related notes yet for {path} (it may not be embedded yet)."
                ));
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
            let tags = app.idx().tag_counts()?;
            if tags.is_empty() {
                return Ok("No tags yet.".into());
            }
            for (t, c) in tags {
                let _ = writeln!(out, "#{t} ({c})");
            }
        }
        "note_history" => {
            if let Some(id) = a.get("revision_id").and_then(Value::as_i64) {
                let (p, content) = app
                    .idx()
                    .revision(id)?
                    .ok_or_else(|| AppError::NotFound(format!("revision {id} not found")))?;
                let _ = write!(out, "revision {id} of {p}\n-----\n{content}");
            } else {
                let path = app.find_note(req(a, "path")?).or_else(|_| {
                    crate::vault::Vault::clean_note(req(a, "path")?)
                        .map_err(|e| AppError::BadRequest(e.to_string()))
                })?;
                let revs = app.idx().revisions(&path)?;
                if revs.is_empty() {
                    return Ok(format!("No earlier versions of {path}."));
                }
                let _ = writeln!(
                    out,
                    "Earlier versions of {path} (newest first). Each is the content *before* the change by 'replaced by':"
                );
                for r in revs.iter().take(50) {
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
            let date = s(a, "date");
            match s(a, "append").filter(|t| !t.trim().is_empty()) {
                Some(text) if !session.read_only => {
                    let path = app.ensure_daily(date, &src)?;
                    let time = chrono::Local::now().format("%H:%M");
                    let text = if text.trim_start().starts_with(['-', '#', '>', '*']) {
                        text.to_string()
                    } else {
                        format!("- {time} {}", text.trim())
                    };
                    let r = app.append(&path, &text, None, &src)?;
                    let _ = writeln!(out, "Appended to {} (hash {}).", r.path, r.hash);
                }
                Some(_) => return Err(AppError::BadRequest("this connection is read-only".into())),
                None => {
                    let path = app.daily_path(date)?;
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
        "remember" => {
            let tags: Vec<String> = a
                .get("tags")
                .and_then(Value::as_array)
                .map(|v| {
                    v.iter()
                        .filter_map(|t| t.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let r = app.remember(req(a, "fact")?, s(a, "topic"), &tags, &src)?;
            let _ = write!(out, "Remembered in {}.", r.path);
        }
        "write_note" => {
            let r = app.write_note(
                req(a, "path")?,
                req(a, "content")?,
                s(a, "expected_hash"),
                &src,
            )?;
            let _ = write!(
                out,
                "{} {} (hash {}).",
                if r.created { "Created" } else { "Updated" },
                r.path,
                r.hash
            );
        }
        "edit_note" => {
            let r = app.edit_note(
                req(a, "path")?,
                req(a, "old_text")?,
                s(a, "new_text").unwrap_or(""),
                s(a, "expected_hash"),
                &src,
            )?;
            let _ = write!(out, "Edited {} (hash {}).", r.path, r.hash);
        }
        "append_to_note" => {
            let r = app.append(req(a, "path")?, req(a, "text")?, s(a, "heading"), &src)?;
            let _ = write!(
                out,
                "{} {} (hash {}).",
                if r.created { "Created" } else { "Appended to" },
                r.path,
                r.hash
            );
        }
        "move_note" => {
            let from_raw = req(a, "from")?;
            let folder = crate::vault::Vault::clean(from_raw)
                .ok()
                .filter(|f| app.vault.abs(f).is_dir());
            if let Some(f) = folder {
                let (to, moved, n) = app.rename_folder(&f, req(a, "to")?, &src)?;
                let _ = write!(
                    out,
                    "Moved folder {f} → {to} ({moved} files); updated links in {n} note(s)."
                );
            } else {
                let from = app.find_note(from_raw)?;
                let (to, n) = app.rename_note(&from, req(a, "to")?, &src)?;
                let _ = write!(out, "Moved {from} → {to}; updated links in {n} note(s).");
            }
        }
        "delete_note" => {
            let path = app.find_note(req(a, "path")?)?;
            app.delete_note(&path, &src)?;
            let _ = write!(
                out,
                "Moved {path} to trash. It can be restored with note_history / restore_revision."
            );
        }
        "restore_revision" => {
            let id = a
                .get("revision_id")
                .and_then(Value::as_i64)
                .ok_or_else(|| AppError::BadRequest("missing argument: revision_id".into()))?;
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
            Session {
                client: "mcp".into(),
                read_only,
            },
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
        assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 16);
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
        let (t, _) = call(&app, &mut s, "read_note", json!({"path":"Sunucular"}));
        assert!(t.contains("gemini-cli"), "{t}");
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
            "write_note",
            json!({"path":"x","content":"y","expected_hash":"bogus"}),
        );
        assert!(e && t.contains("changed by someone else"), "{t}");
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
