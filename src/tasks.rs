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
/// `<emoji/key> date` or dataview `[key:: date]`. Only the dataview form consumes a
/// closing `]`, so a following `]]` of a wikilink is left alone. The date is in
/// group 1 or 2 (see [`date_of`]).
fn marker(prefixes: &str, dataview: &str) -> Regex {
    Regex::new(&format!(
        r"(?:{prefixes})\s*{DATE}|\[{dataview}::\s*{DATE}\s*\]?"
    ))
    .unwrap()
}
static DUE: LazyLock<Regex> = LazyLock::new(|| marker(r"📅|🗓️?|\bdue:", "due"));
static SCHED: LazyLock<Regex> = LazyLock::new(|| marker(r"⏳|\bscheduled:", "scheduled"));
static DONE: LazyLock<Regex> = LazyLock::new(|| marker(r"✅|\bdone:", "completion"));
static START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?:🛫|➕)\s*{DATE}")).unwrap());
/// Recurrence rule like Obsidian Tasks: `🔁 every week on Monday, when done`.
/// Limited to the rule's own characters so text, tags or links after it survive.
static RECUR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"🔁[a-zA-Z0-9, !]*").unwrap());

fn date_of(re: &Regex, s: &str) -> Option<String> {
    let c = re.captures(s)?;
    c.get(1)
        .or_else(|| c.get(2))
        .map(|m| m.as_str().to_string())
}

pub fn parse(content: &str) -> Vec<Task> {
    let mut out = Vec::new();
    let mut fence = false;
    // Frontmatter only when it is closed; an unclosed `---` must not hide every task.
    let skip = match crate::note::split_frontmatter(content) {
        (Some(_), body) => content[..content.len() - body.len()].lines().count(),
        (None, _) => 0,
    };
    for (i, line) in content.lines().enumerate().skip(skip) {
        let t = line.trim_start();
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
        // An empty checkbox (e.g. a template placeholder `- [ ] `) is not a task.
        if body.trim().is_empty() {
            continue;
        }
        let grab = |re: &Regex| date_of(re, &body);
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

    #[test]
    fn toggle_keeps_wikilink_brackets() {
        let c = "- [x] review [[Log ✅ 2026-11-20]] ✅ 2026-11-21";
        let off = toggle(c, 1, c, "2026-11-22").unwrap();
        assert_eq!(off, "- [ ] review [[Log ]]");
        let t = parse("- [ ] plan [[Journal/2026-12-01|📅 2026-12-01]]");
        assert_eq!(t[0].due.as_deref(), Some("2026-12-01"));
        assert_eq!(t[0].text, "plan [[Journal/2026-12-01|]]");
    }

    #[test]
    fn unclosed_frontmatter_and_empty_boxes() {
        let t = parse("---\n- [ ] a\n- [ ] b");
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].line, 2);
        let t = parse("## Plan\n\n- [ ] \n- [ ]   \n- [x] \n- [ ] real\n");
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].text, "real");
        assert_eq!(t[0].line, 6);
    }

    #[test]
    fn recurrence_keeps_the_rest_of_the_line() {
        let t = parse("- [ ] Water plants 🔁 every week on Monday 📅 2026-12-01 #home [[Garden]]");
        assert_eq!(t[0].text, "Water plants #home [[Garden]]");
        assert_eq!(t[0].due.as_deref(), Some("2026-12-01"));
        let t = parse("- [ ] Pay rent 🔁 every month when done [due:: 2026-12-01] ⏫");
        assert_eq!(t[0].text, "Pay rent");
        assert_eq!(t[0].due.as_deref(), Some("2026-12-01"));
        assert_eq!(t[0].priority, 2);
    }
}
