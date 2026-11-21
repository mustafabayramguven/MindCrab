//! Task extraction, compatible with the Obsidian Tasks plugin syntax:
//! `- [ ] Pay rent 📅 2026-12-01 ⏳ 2026-11-28 ⏫` / `- [x] done ✅ 2026-11-21`.
//! Also accepts `due:2026-12-01` and dataview-style `[due:: 2026-12-01]`.

use regex::Regex;
use serde::Serialize;
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Task {
    /// 1-based line number in the full file (frontmatter included).
    pub line: usize,
    /// Task text without the checkbox and without date/priority markers.
    pub text: String,
    /// The full original line (used to verify edits).
    pub raw: String,
    pub done: bool,
    pub due: Option<String>,
    pub scheduled: Option<String>,
    pub done_date: Option<String>,
    /// 3 highest … 0 none … -1 low
    pub priority: i32,
}

static TASK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:[-*+]|\d+[.)])\s+\[([ xX/-])\]\s+(.*)$").unwrap());
static DATE: &str = r"(\d{4}-\d{2}-\d{2})";
static DUE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?:📅|🗓️?|\bdue:|\[due::)\s*{DATE}\]?")).unwrap());
static SCHED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"(?:⏳|\bscheduled:|\[scheduled::)\s*{DATE}\]?")).unwrap()
});
static DONE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?:✅|\bdone:|\[completion::)\s*{DATE}\]?")).unwrap());
static START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?:🛫|➕)\s*{DATE}")).unwrap());
static RECUR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"🔁[^📅⏳✅🛫➕⏫🔼🔽🔺⏬]*").unwrap());

pub fn parse(content: &str) -> Vec<Task> {
    let mut out = Vec::new();
    let mut fence = false;
    let mut in_fm = content.starts_with("---\n") || content.starts_with("---\r\n");
    for (i, line) in content.lines().enumerate() {
        let t = line.trim_start();
        if in_fm {
            if i > 0 && (t.trim_end() == "---" || t.trim_end() == "...") {
                in_fm = false;
            }
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if fence {
            continue;
        }
        let Some(c) = TASK.captures(line) else {
            continue;
        };
        let body = c[2].to_string();
        let grab = |re: &Regex| re.captures(&body).map(|m| m[1].to_string());
        let priority = if body.contains('🔺') {
            3
        } else if body.contains('⏫') {
            2
        } else if body.contains('🔼') {
            1
        } else if body.contains('🔽') || body.contains('⏬') {
            -1
        } else {
            0
        };
        let mut text = body.clone();
        for re in [&*DUE, &*SCHED, &*DONE, &*START, &*RECUR] {
            text = re.replace_all(&text, "").into_owned();
        }
        let text = text
            .replace(['🔺', '⏫', '🔼', '🔽', '⏬'], "")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        out.push(Task {
            line: i + 1,
            done: matches!(&c[1], "x" | "X" | "-"),
            due: grab(&DUE),
            scheduled: grab(&SCHED),
            done_date: grab(&DONE),
            priority,
            text,
            raw: line.to_string(),
        });
    }
    out
}

/// Toggles the task on `line` (1-based) if that line still equals `raw`.
/// Adds/removes a `✅ date` stamp like the Tasks plugin. Returns the new content.
pub fn toggle(content: &str, line: usize, raw: &str, today: &str) -> Option<String> {
    let mut lines: Vec<String> = content.split('\n').map(str::to_string).collect();
    let idx = line.checked_sub(1)?;
    let cur = lines.get(idx)?.trim_end_matches('\r').to_string();
    if cur != raw.trim_end_matches('\r') {
        return None;
    }
    let c = TASK.captures(&cur)?;
    let mark = c.get(1)?;
    let done = matches!(mark.as_str(), "x" | "X" | "-");
    let mut new = format!(
        "{}{}{}",
        &cur[..mark.start()],
        if done { " " } else { "x" },
        &cur[mark.end()..]
    );
    if done {
        new = DONE.replace_all(&new, "").trim_end().to_string();
    } else if !DONE.is_match(&new) {
        new = format!("{} ✅ {today}", new.trim_end());
    }
    let cr = if lines[idx].ends_with('\r') { "\r" } else { "" };
    lines[idx] = format!("{new}{cr}");
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tasks_plugin_syntax() {
        let c = "---\ntags: [x]\n---\n- [ ] Kira öde 📅 2026-12-01 ⏫\n  - [x] alt görev ✅ 2026-11-20\n```\n- [ ] kod\n```\n* [ ] due:2026-11-30 başka [scheduled:: 2026-11-29]\nnormal satır";
        let t = parse(c);
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].line, 4);
        assert_eq!(t[0].text, "Kira öde");
        assert_eq!(t[0].due.as_deref(), Some("2026-12-01"));
        assert_eq!(t[0].priority, 2);
        assert!(t[1].done);
        assert_eq!(t[1].done_date.as_deref(), Some("2026-11-20"));
        assert_eq!(t[2].due.as_deref(), Some("2026-11-30"));
        assert_eq!(t[2].scheduled.as_deref(), Some("2026-11-29"));
        assert_eq!(t[2].text, "başka");
    }

    #[test]
    fn toggles_with_done_stamp() {
        let c = "# T\n- [ ] a 📅 2026-12-01\n- [x] b ✅ 2026-11-01";
        let on = toggle(c, 2, "- [ ] a 📅 2026-12-01", "2026-11-21").unwrap();
        assert_eq!(
            on.lines().nth(1).unwrap(),
            "- [x] a 📅 2026-12-01 ✅ 2026-11-21"
        );
        let off = toggle(c, 3, "- [x] b ✅ 2026-11-01", "2026-11-21").unwrap();
        assert_eq!(off.lines().nth(2).unwrap(), "- [ ] b");
        assert!(toggle(c, 2, "- [ ] changed", "x").is_none());
    }
}
