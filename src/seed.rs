//! Starter notes written into an empty vault.

pub fn memory_md(memory_dir: &str, daily_dir: &str) -> String {
    format!(
        r#"---
title: MEMORY
tags: [memory, system]
---
# MEMORY

This vault is the user's long-term memory. AI assistants connected over MCP
read this note first (`memory_overview`) — keep it short and current.
The user can edit it at any time; their edits win.

## How to use this vault (for AI assistants)

- **Search before you write.** Use `search_notes` (or `list_notes` for a folder)
  so you update an existing note instead of creating a duplicate.
- **Small facts → `remember`.** They land in `{memory_dir}/<Topic>.md` as dated
  bullets. Pick an existing topic when one fits.
- **Bigger things → a note.** One subject per note, a clear title, `[[wikilinks]]`
  to related notes, `#tags` for grouping.
- **Edit surgically.** Prefer `edit_note` (exact replace) or `append_to_note`
  over rewriting a whole note. Pass `expected_hash` from `read_note` when you
  overwrite, so you never clobber the user's changes.
- **Daily log → `daily_note`** (`{daily_dir}/YYYY-MM-DD.md`).
- Write in the user's language. Never store passwords, tokens, card or ID numbers.
- Every change is versioned (`note_history`), so mistakes can be undone.

## Index

- [[Welcome]]
- [[{memory_dir}/Inbox|Inbox]] — unsorted facts
"#
    )
}

pub fn inbox_md() -> String {
    "---\ntags: [memory]\n---\n# Inbox\n\nFacts that don't belong to a topic yet.\n".to_string()
}

pub const WELCOME: &str = r#"# Welcome to MindCrab 🦀

MindCrab is a self-hosted, Obsidian-style Markdown notebook that your AI
assistants can use as long-term memory over **MCP**.

Hoş geldin! Notların düz `.md` dosyaları olarak diskte durur; Obsidian ile de açabilirsin.

## Basics

- `[[Note name]]` links notes; `[[Note|label]]` changes the label; `[[Note#Heading]]` jumps to a heading.
- `#tag` or `tags:` in frontmatter groups notes. Nested tags work: `#project/mindcrab`.
- `![[image.png]]` embeds an image — paste or drop images into the editor.
- Notes in `Templates/` are templates: pick one when creating a note, or insert it with the toolbar. `{{title}}`, `{{date}}`, `{{time}}` and `{{date:DD.MM.YYYY}}` are filled in. `Templates/Daily.md` shapes the daily note.
- Callouts:

> [!TIP]
> Press **Ctrl+K** to search, **Ctrl+S** to save, **Ctrl+E** to switch edit / preview.

- Tasks: `- [ ] todo` / `- [x] done`

## More

- **Tasks** and **Calendar** in the sidebar collect every `- [ ] task 📅 2026-12-01` from all notes.
- A ```` ```base ```` block turns frontmatter into a table — press the table button in the toolbar.
- `$E=mc^2$` renders as math, ```` ```mermaid ```` blocks as diagrams, and **Canvas** boards live next to your notes.
- Add `ai: false` to a note's frontmatter to hide it from every AI.

## Memory

[[MEMORY]] is the entry point AI assistants read first. Facts they save with
`remember` go into the `Memory/` folder, and the daily log lives in `Journal/`.
Every change — by you or by an AI — is kept in the note's **history**.

#mindcrab
"#;

pub const TEMPLATE_DAILY: &str =
    "---\ntags: [journal]\n---\n# {{title}}\n\n## Plan\n\n- [ ] \n\n## Log\n\n";

pub const TEMPLATE_MEETING: &str = "---\ntags: [meeting]\ndate: {{date}}\n---\n# {{title}}\n\n**When:** {{date:dddd, D MMMM YYYY}} {{time}}\n**With:** \n\n## Notes\n\n## Decisions\n\n## Action items\n\n- [ ] \n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_have_no_ghost_tasks() {
        // The empty `- [ ] ` placeholders are for typing into, not tasks.
        for t in [TEMPLATE_DAILY, TEMPLATE_MEETING] {
            assert!(t.contains("- [ ] \n"));
            assert!(crate::tasks::parse(t).is_empty(), "{t}");
        }
    }

    #[test]
    fn memory_guide_names_existing_tools() {
        // `recall` is not an MCP tool; search_notes is.
        let m = memory_md("Memory", "Journal");
        assert!(!m.contains("`recall`"));
        assert!(m.contains("`search_notes`"));
    }
}
