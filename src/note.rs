//! Markdown note parsing: frontmatter, title, tags and links.

use regex::Regex;
use serde_json::{Map, Value};
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Normalised lookup key: lowercase, no `.md`, no leading `./` or `/`.
    pub key: String,
    /// The text as written by the user (without alias / heading).
    pub raw: String,
    pub embed: bool,
}

#[derive(Debug, Clone)]
pub struct Parsed {
    pub title: String,
    pub frontmatter: Map<String, Value>,
    /// Content without the frontmatter block.
    pub body: String,
    pub tags: Vec<String>,
    pub links: Vec<Link>,
}

static WIKILINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(!?)\[\[([^\[\]|#\^\n]*)(?:[#\^][^\[\]|\n]*)?(?:\|[^\[\]\n]*)?\]\]").unwrap()
});
static MDLINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(!?)\[[^\]\n]*\]\(([^)\s]+\.md)(?:#[^)\s]*)?\)").unwrap());
static TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[\s(])#([\p{L}\p{N}_][\p{L}\p{N}_/\-]*)").unwrap());
static INLINE_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`[^`\n]*`").unwrap());

/// Split a document into (frontmatter yaml, body).
pub fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    let rest = if let Some(r) = content.strip_prefix("---\n") {
        r
    } else if let Some(r) = content.strip_prefix("---\r\n") {
        r
    } else {
        return (None, content);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" || trimmed == "..." {
            let yaml = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return (Some(yaml), body);
        }
        offset += line.len();
    }
    (None, content)
}

pub fn parse_frontmatter(yaml: &str) -> Map<String, Value> {
    match serde_yaml::from_str::<Value>(yaml) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

/// Removes fenced code blocks and inline code so we don't pick up tags/links from code.
fn strip_code(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut fence: Option<String> = None;
    for line in body.lines() {
        let t = line.trim_start();
        if let Some(f) = &fence {
            if t.starts_with(f.as_str()) {
                fence = None;
            }
            out.push('\n');
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = Some(t[..3].to_string());
            out.push('\n');
            continue;
        }
        out.push_str(&INLINE_CODE.replace_all(line, ""));
        out.push('\n');
    }
    out
}

pub fn normalize_key(target: &str) -> String {
    let t = target.trim().replace('\\', "/");
    let t = t.trim_start_matches("./").trim_start_matches('/');
    let t = t.strip_suffix(".md").unwrap_or(t);
    t.to_lowercase()
}

pub fn is_note_path(p: &str) -> bool {
    p.to_lowercase().ends_with(".md")
}

pub fn stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".md").unwrap_or(name)
}

pub fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// Joins a relative markdown link to the directory of `src`.
pub fn join_relative(src: &str, target: &str) -> String {
    let target = percent_decode(target);
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut parts: Vec<&str> = parent_dir(src)
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(b) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn tags_from_value(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            for t in s.split([',', ' ']) {
                let t = t.trim().trim_start_matches('#');
                if !t.is_empty() {
                    out.push(t.to_lowercase());
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| tags_from_value(x, out)),
        _ => {}
    }
}

pub fn parse(path: &str, content: &str) -> Parsed {
    let (yaml, body) = split_frontmatter(content);
    let frontmatter = yaml.map(parse_frontmatter).unwrap_or_default();

    let mut tags = Vec::new();
    for key in ["tags", "tag"] {
        if let Some(v) = frontmatter.get(key) {
            tags_from_value(v, &mut tags);
        }
    }

    let clean = strip_code(body);
    for cap in TAG.captures_iter(&clean) {
        let t = cap[1].trim_end_matches(['/', '-']);
        // Skip pure numbers like #1 (issue refs) — same rule as Obsidian.
        if t.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        tags.push(t.to_lowercase());
    }
    tags.sort();
    tags.dedup();

    let mut links = Vec::new();
    for cap in WIKILINK.captures_iter(&clean) {
        let raw = cap[2].trim();
        if raw.is_empty() {
            continue;
        }
        links.push(Link {
            key: normalize_key(raw),
            raw: raw.to_string(),
            embed: &cap[1] == "!",
        });
    }
    for cap in MDLINK.captures_iter(&clean) {
        let target = &cap[2];
        if target.contains("://") {
            continue;
        }
        let joined = join_relative(path, target);
        links.push(Link {
            key: normalize_key(&joined),
            raw: target.to_string(),
            embed: &cap[1] == "!",
        });
    }

    let title = frontmatter
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            body.lines()
                .find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_string()))
                .filter(|t| !t.is_empty())
        })
        .unwrap_or_else(|| stem(path).to_string());

    Parsed {
        title,
        frontmatter,
        body: body.to_string(),
        tags,
        links,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_and_title() {
        let p = parse(
            "a/b.md",
            "---\ntitle: Hello\ntags: [x, Y]\n---\n# Ignored\nbody #inline",
        );
        assert_eq!(p.title, "Hello");
        assert_eq!(p.tags, vec!["inline", "x", "y"]);
        assert_eq!(p.body, "# Ignored\nbody #inline");
    }

    #[test]
    fn title_from_heading_then_stem() {
        assert_eq!(parse("n.md", "# Başlık\n").title, "Başlık");
        assert_eq!(parse("dir/My Note.md", "no heading").title, "My Note");
    }

    #[test]
    fn links_and_code_ignored() {
        let p = parse(
            "x/y.md",
            "see [[Foo]] and [[Bar/Baz|alias]] ![[img.png]] [[Foo#h]]\n`[[nope]]` #tag-1 #123\n```\n[[nope2]] #nocode\n```\n[m](../z.md)",
        );
        let keys: Vec<_> = p.links.iter().map(|l| l.key.as_str()).collect();
        assert_eq!(keys, vec!["foo", "bar/baz", "img.png", "foo", "z"]);
        assert!(p.links[2].embed);
        assert_eq!(p.tags, vec!["tag-1"]);
    }

    #[test]
    fn turkish_tags() {
        let p = parse("t.md", "#günlük #şapka/ekip");
        assert_eq!(p.tags, vec!["günlük", "şapka/ekip"]);
    }

    #[test]
    fn heading_is_not_tag() {
        assert!(parse("t.md", "# Heading\n## Sub").tags.is_empty());
    }
}
