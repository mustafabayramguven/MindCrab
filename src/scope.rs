//! What an AI connection may see and change.
//!
//! A note is hidden from every AI client when its frontmatter has `ai: false`
//! (or `mcp: false` / `private: true`), or when its path matches a pattern in
//! `<vault>/.mcpignore` (gitignore-style: `Özel/`, `Sağlık/**`, `*.secret.md`,
//! `!Özel/paylaşılabilir.md`). On top of that every connection can be limited to
//! some folders and/or be read-only.

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Frontmatter keys that hide a note from AI clients.
pub fn frontmatter_private(fm: &serde_json::Map<String, Value>) -> bool {
    let word = |k: &str| match fm.get(k) {
        Some(Value::Bool(b)) => Some(b.to_string()),
        Some(Value::String(s)) => Some(scalar_word(s)),
        _ => None,
    };
    let falsy = |k: &str| word(k).is_some_and(|w| is_falsy(&w));
    let truthy = |k: &str| word(k).is_some_and(|w| is_truthy(&w));
    falsy("ai") || falsy("mcp") || truthy("private")
}

/// True when a note's text hides it from AI clients through its frontmatter.
/// Also catches the flag in a frontmatter block that is not valid YAML, so a
/// typo elsewhere in the block never exposes the note.
pub fn content_private(content: &str) -> bool {
    let Some(yaml) = crate::note::split_frontmatter(content).0 else {
        return false;
    };
    let fm = crate::note::parse_frontmatter(yaml);
    if !fm.is_empty() {
        return frontmatter_private(&fm);
    }
    // Unparsable block: look for the flag line by line.
    yaml.lines().any(|line| {
        if line.starts_with([' ', '\t']) {
            return false; // nested key
        }
        let Some((key, value)) = line.split_once(':') else {
            return false;
        };
        let w = scalar_word(value);
        match key.trim().trim_matches(['"', '\'']) {
            "ai" | "mcp" => is_falsy(&w),
            "private" => is_truthy(&w),
            _ => false,
        }
    })
}

fn is_falsy(w: &str) -> bool {
    matches!(w, "false" | "no" | "off")
}

fn is_truthy(w: &str) -> bool {
    matches!(w, "true" | "yes" | "on")
}

/// Lower-cased plain value of a YAML scalar. Strings written back by an editor
/// may still carry their quotes or an inline comment (`"false # gizli"`,
/// `"'false'"`); those are removed so the flag keeps its meaning.
fn scalar_word(s: &str) -> String {
    let s = s.trim();
    let s = match s.chars().next() {
        Some(q @ ('"' | '\'')) => s[1..].split(q).next().unwrap_or(""),
        _ => {
            // `#` starts a comment at the start or after whitespace (not in `a#b`).
            let mut end = s.len();
            let mut after_space = true;
            for (i, c) in s.char_indices() {
                if c == '#' && after_space {
                    end = i;
                    break;
                }
                after_space = c.is_whitespace();
            }
            &s[..end]
        }
    };
    s.trim().to_ascii_lowercase()
}

#[derive(Debug, Default, Clone)]
pub struct Ignore {
    rules: Vec<(Regex, bool)>, // (pattern, negated)
}

impl Ignore {
    pub fn parse(text: &str) -> Self {
        let mut rules = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (neg, pat) = match line.strip_prefix('!') {
                Some(p) => (true, p.trim()),
                None => (false, line),
            };
            if let Some(re) = glob_to_regex(pat) {
                rules.push((re, neg));
            }
        }
        Self { rules }
    }

    /// Last matching rule wins (like .gitignore).
    pub fn matches(&self, path: &str) -> bool {
        let mut hit = false;
        for (re, neg) in &self.rules {
            if re.is_match(path) {
                hit = !neg;
            }
        }
        hit
    }
}

fn glob_to_regex(pat: &str) -> Option<Regex> {
    let anchored = pat.starts_with('/') || pat.trim_end_matches('/').contains('/');
    let dir = pat.ends_with('/');
    let p = pat.trim_matches('/');
    if p.is_empty() {
        return None;
    }
    let mut re = String::new();
    let chars: Vec<char> = p.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                re.push_str(".*");
                i += 1;
                if chars.get(i + 1) == Some(&'/') {
                    i += 1;
                    re.push_str("/?");
                }
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    let prefix = if anchored { "^" } else { "(?:^|.*/)" };
    // A folder pattern (or any pattern) also covers everything below it.
    let suffix = if dir { "/.*$" } else { "(?:/.*)?$" };
    Regex::new(&format!("(?i){prefix}{re}{suffix}")).ok()
}

/// Permissions of one MCP connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scope {
    /// Shown in history as `mcp:<name>`.
    pub name: String,
    pub read_only: bool,
    /// `remember` goes to the review queue instead of straight into notes.
    pub review: bool,
    /// Allowed folders (empty = whole vault).
    pub folders: Vec<String>,
}

impl Scope {
    pub fn full(name: &str, read_only: bool, review: bool) -> Self {
        Self {
            name: name.into(),
            read_only,
            review,
            folders: Vec::new(),
        }
    }

    pub fn folder_allowed(&self, path: &str) -> bool {
        self.folders.is_empty()
            || self.folders.iter().any(|f| {
                let f = f.trim_matches('/');
                f.is_empty() || path == f || path.starts_with(&format!("{f}/"))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_patterns() {
        let ig =
            Ignore::parse("# yorum\nÖzel/\nSağlık/**\n*.secret.md\n!Özel/paylaş.md\n/Kök.md\n");
        assert!(ig.matches("Özel/a.md"));
        assert!(ig.matches("Özel/alt/b.md"));
        assert!(ig.matches("özel/a.md"), "case-insensitive");
        assert!(!ig.matches("Özel/paylaş.md"), "negation");
        assert!(ig.matches("Sağlık/x.md"));
        assert!(ig.matches("Notlar/şifre.secret.md"));
        assert!(ig.matches("Kök.md"));
        assert!(!ig.matches("Alt/Kök.md"));
        assert!(!ig.matches("ÖzelOlmayan/a.md"));
        assert!(!ig.matches("Notlar/a.md"));
    }

    #[test]
    fn frontmatter_flags() {
        let fm = |y: &str| crate::note::parse_frontmatter(y);
        assert!(frontmatter_private(&fm("ai: false")));
        assert!(frontmatter_private(&fm("mcp: \"false\"")));
        assert!(frontmatter_private(&fm("private: true")));
        assert!(!frontmatter_private(&fm("ai: true\ntags: [x]")));
    }

    #[test]
    fn frontmatter_flags_survive_comments_and_quotes() {
        let fm = |y: &str| crate::note::parse_frontmatter(y);
        for y in [
            "ai: false # gizli",
            "ai: \"false # gizli\"",
            "ai: 'false # gizli'",
            "ai: \"false\"",
            "ai: 'false'",
            "ai: \"'false' # x\"",
            "ai: \"\\\"false\\\" # x\"",
            "ai: FALSE",
            "ai: no",
            "ai: \"No\"",
            "ai: off",
            "ai: \"off # yok\"",
            "mcp: no",
            "private: yes",
            "private: \"true # gizli\"",
            "private: on",
            "tags: [a]\nai: \"  false  \"",
        ] {
            assert!(frontmatter_private(&fm(y)), "should be private: {y}");
        }
        for y in [
            "ai: true",
            "ai: \"true # false\"",
            "ai: \"x # false\"",
            "ai: \"falsey\"",
            "ai: \"a#false\"",
            "ai: \"# false\"",
            "ai: yes",
            "private: false",
            "private: \"no # true\"",
            "title: \"ai: false\"",
        ] {
            assert!(!frontmatter_private(&fm(y)), "should be visible: {y}");
        }
    }

    #[test]
    fn content_flags() {
        assert!(content_private("---\nai: false # gizli\n---\nsır"));
        assert!(content_private("---\nai: \"false # gizli\"\n---\nsır"));
        // Broken YAML elsewhere in the block must not expose the note.
        assert!(content_private("---\nai: false\ntags: [açık\n---\nsır"));
        assert!(content_private("---\ntags: [x\nprivate: \"yes\"\n---\n"));
        assert!(!content_private("---\nai: true\n---\nx"));
        assert!(!content_private("---\nmeta:\n  ai: false\n---\nx"));
        assert!(!content_private("ai: false\n\nno frontmatter here"));
        assert!(!content_private(""));
    }

    #[test]
    fn folder_scope() {
        let s = Scope {
            name: "g".into(),
            read_only: false,
            review: false,
            folders: vec!["Projeler".into()],
        };
        assert!(s.folder_allowed("Projeler/a.md"));
        assert!(!s.folder_allowed("Projeler2/a.md"));
        assert!(!s.folder_allowed("a.md"));
    }
}
