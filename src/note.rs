//! Markdown note parsing: frontmatter, title, tags and links.

use regex::Regex;
use serde_json::{Map, Value};
use std::collections::HashMap;
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
    LazyLock::new(|| Regex::new(r"(!?)\[[^\]\n]*\]\(([^)\s]+\.(?i:md))(?:#[^)\s]*)?\)").unwrap());
static TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[\s(])#([\p{L}\p{N}_][\p{L}\p{N}_/\-]*)").unwrap());
/// Link destinations: inline `](dest)` and reference definitions `[label]: dest`
/// (a footnote `[^1]: text` is not a definition).
static LINK_DEST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)\]\([^)\n]*\)|^ {0,3}\[[^\]^\n][^\]\n]*\]:[ \t]*\S+").unwrap()
});

/// Split a document into (frontmatter yaml, body).
pub fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    // Editors may save a UTF-8 BOM. It is invisible and Obsidian still sees the fence,
    // so it must not hide the frontmatter (and with it `ai: false`).
    let text = content.strip_prefix('\u{FEFF}').unwrap_or(content);
    let Some((first, rest)) = text.split_once('\n') else {
        return (None, text);
    };
    if fence_line(first) != "---" {
        return (None, text);
    }
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let fence = fence_line(line);
        if fence == "---" || fence == "..." {
            let yaml = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return (Some(yaml), body);
        }
        offset += line.len();
    }
    (None, text)
}

/// A frontmatter fence line without its line ending and trailing blanks (`--- \r\n` → `---`).
fn fence_line(line: &str) -> &str {
    line.trim_end_matches([' ', '\t', '\r', '\n'])
}

pub fn parse_frontmatter(yaml: &str) -> Map<String, Value> {
    match serde_yaml::from_str::<Value>(yaml) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

/// Tracks fenced code blocks line by line. As in CommonMark, a fence is 3+ backticks or
/// tildes and closes only with the same character, at least as long, and nothing after it.
#[derive(Default)]
struct Fences {
    open: Option<(u8, usize)>,
}

impl Fences {
    /// Whether `line` is code: a fence line or a line inside a fenced block.
    fn is_code(&mut self, line: &str) -> bool {
        let t = line.trim_start();
        if let Some((ch, len)) = self.open {
            let n = t.bytes().take_while(|b| *b == ch).count();
            if n >= len && t[n..].trim().is_empty() {
                self.open = None;
            }
            return true;
        }
        let Some(&ch) = t.as_bytes().first() else {
            return false;
        };
        if ch != b'`' && ch != b'~' {
            return false;
        }
        let n = t.bytes().take_while(|b| *b == ch).count();
        // The info string of a backtick fence has no backticks (```a``` is inline code).
        if n < 3 || (ch == b'`' && t[n..].contains('`')) {
            return false;
        }
        self.open = Some((ch, n));
        true
    }
}

/// Appends `line` without its code spans: a run of N backticks up to the next run of
/// exactly N (``a ` b``). A run without such a partner is literal text.
fn push_without_code_spans(line: &str, out: &mut String) {
    if !line.contains('`') {
        out.push_str(line);
        return;
    }
    let b = line.as_bytes();
    let mut runs = Vec::new(); // (start, end) of every backtick run
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'`' {
            let s = i;
            while i < b.len() && b[i] == b'`' {
                i += 1;
            }
            runs.push((s, i));
        } else {
            i += 1;
        }
    }
    // next[k]: the following run as long as run k — keeps this linear on hostile input.
    let mut next = vec![None; runs.len()];
    let mut later: HashMap<usize, usize> = HashMap::new();
    for (k, &(s, e)) in runs.iter().enumerate().rev() {
        next[k] = later.insert(e - s, k);
    }
    let mut last = 0;
    let mut k = 0;
    while k < runs.len() {
        match next[k] {
            Some(c) => {
                out.push_str(&line[last..runs[k].0]);
                last = runs[c].1;
                k = c + 1;
            }
            None => k += 1,
        }
    }
    out.push_str(&line[last..]);
}

/// Removes fenced code blocks and inline code so we don't pick up tags/links from code.
fn strip_code(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut fences = Fences::default();
    for line in body.lines() {
        if !fences.is_code(line) {
            push_without_code_spans(line, &mut out);
        }
        out.push('\n');
    }
    out
}

/// `s` without a trailing `.md` in any case (`README.MD`).
fn strip_md(s: &str) -> &str {
    let cut = s.len().saturating_sub(3);
    match s.get(cut..) {
        Some(ext) if ext.eq_ignore_ascii_case(".md") => &s[..cut],
        _ => s,
    }
}

pub fn normalize_key(target: &str) -> String {
    let t = target.trim().replace('\\', "/");
    let t = t.trim_start_matches("./").trim_start_matches('/');
    strip_md(t).to_lowercase()
}

pub fn is_note_path(p: &str) -> bool {
    p.to_lowercase().ends_with(".md")
}

pub fn stem(path: &str) -> &str {
    strip_md(path.rsplit('/').next().unwrap_or(path))
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
    // A `#` in a link destination is an anchor (`[Bölüm](#genel)`), not a tag.
    let prose = LINK_DEST.replace_all(
        &clean,
        |c: &regex::Captures| {
            if c[0].starts_with(']') { "]" } else { "" }
        },
    );
    for cap in TAG.captures_iter(&prose) {
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
            // A `# comment` in a fenced shell/python block is not a heading.
            let mut fences = Fences::default();
            body.lines()
                .filter(|l| !fences.is_code(l))
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

    #[test]
    fn frontmatter_with_bom_and_trailing_blanks_stays_private() {
        for doc in [
            "\u{FEFF}---\nai: false\n---\nsır",
            "--- \nai: false\n---\t\nsır",
            "---\r\nai: false\r\n---  \r\nsır",
            "\u{FEFF}--- \t\r\nai: false\r\n... \r\nsır",
            "---\nai: false\n---   ",
        ] {
            let p = parse("g.md", doc);
            assert!(
                crate::scope::frontmatter_private(&p.frontmatter),
                "not private: {doc:?}"
            );
            assert!(!p.body.contains("ai:"), "{doc:?}");
        }
        assert_eq!(split_frontmatter("\u{FEFF}# Başlık").0, None);
        assert_eq!(parse("t.md", "\u{FEFF}# Başlık\n").title, "Başlık");
        assert_eq!(split_frontmatter("--- x\na: 1\n---\n").0, None);
        assert_eq!(split_frontmatter(" ---\na: 1\n---\n").0, None);
    }

    #[test]
    fn uppercase_md_extension() {
        assert_eq!(stem("dir/README.MD"), "README");
        assert_eq!(stem("Not.Md"), "Not");
        assert_eq!(stem("md"), "md");
        assert_eq!(normalize_key("./Dir/README.MD"), "dir/readme");
        assert_eq!(parse("dir/README.MD", "x").title, "README");
        let p = parse("a/x.md", "[r](../README.MD#kurulum)");
        assert_eq!(p.links[0].key, "readme");
    }

    #[test]
    fn link_destinations_are_not_tags() {
        let p = parse(
            "t.md",
            "[Bölüm](#genel) ![r](#resim) (#gerçek) [b](x.md#baslik)\n[ref]: #yok\n[^1]: #dipnot",
        );
        assert_eq!(p.tags, vec!["dipnot", "gerçek"]);
    }

    #[test]
    fn multi_backtick_code_spans() {
        let p = parse("t.md", "``a ` [[nope]] #x`` [[yes]] `` ` `` #tag");
        let keys: Vec<_> = p.links.iter().map(|l| l.key.as_str()).collect();
        assert_eq!(keys, vec!["yes"]);
        assert_eq!(p.tags, vec!["tag"]);
        // Unmatched backticks are literal text.
        let p = parse("t.md", "`` open ` #t [[l]]");
        assert_eq!(p.tags, vec!["t"]);
        assert_eq!(p.links[0].key, "l");
        // ```a``` is inline code, not a fence.
        assert_eq!(parse("t.md", "```a``` [[k]]").links[0].key, "k");
    }

    #[test]
    fn long_and_tilde_fences() {
        let p = parse(
            "t.md",
            "````md\n```\n[[inner]] #inner\n```\n````\n~~~\n```\n[[t]]\n~~~\n```\n```js\n[[in]]\n  ```\n[[after]] #out",
        );
        let keys: Vec<_> = p.links.iter().map(|l| l.key.as_str()).collect();
        assert_eq!(keys, vec!["after"]);
        assert_eq!(p.tags, vec!["out"]);
    }

    #[test]
    fn title_ignores_comments_in_code() {
        assert_eq!(
            parse("t.md", "```bash\n# yorum\n```\n# Gerçek\n").title,
            "Gerçek"
        );
        assert_eq!(
            parse("d/Betik.md", "~~~~python\n# only a comment\n~~~~\ntext").title,
            "Betik"
        );
    }
}
