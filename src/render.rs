//! Markdown → sanitised HTML, with Obsidian-style wikilinks, embeds, callouts and tags.

use crate::index::Resolver;
use crate::note;
use pulldown_cmark::{CowStr, Event, LinkType, Options, Parser, Tag, TagEnd, html};
use regex::Regex;
use std::borrow::Cow;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::LazyLock;

static TAG_IN_TEXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|[\s(])#([\p{L}\p{N}_][\p{L}\p{N}_/\-]*)").unwrap());

const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "avif", "bmp"];

/// Prefix the sanitizer puts on every `id` (so note content cannot clobber the app's own
/// element ids); in-page `#fragment` links get the same prefix so they still match.
const ID_PREFIX: &str = "h-";

/// Classes the web UI styles or looks for in rendered notes; any other class is dropped.
const UI_CLASSES: &[&str] = &[
    "wikilink",
    "broken",
    "embed",
    "embed-note",
    "embed-title",
    "embed-body",
    "file",
    "tag",
    "math-inline",
    "math-display",
    "base-view",
    "footnote-reference",
    "footnote-definition",
    "footnote-definition-label",
    "markdown-alert-note",
    "markdown-alert-tip",
    "markdown-alert-important",
    "markdown-alert-warning",
    "markdown-alert-caution",
];

/// Random per-process tag on the `data-task` values the renderer writes. The sanitizer
/// keeps only tagged values (and strips the tag), so raw HTML in a note cannot forge a
/// task checkbox that the preview would toggle.
static TASK_SECRET: LazyLock<String> = LazyLock::new(|| {
    let mut b = [0u8; 16];
    if getrandom::fill(&mut b).is_err() {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        b = (t ^ u128::from(std::process::id()).rotate_left(64)).to_le_bytes();
    }
    format!("mc{}", hex::encode(b))
});

/// Nesting limit for `![[Note]]` transclusions.
const MAX_EMBED_DEPTH: usize = 3;
/// Transclusions attempted per render, across all nesting levels.
const MAX_EMBEDS: usize = 64;
/// Markdown + HTML bytes that transclusions may add to one render.
const MAX_EMBED_BYTES: usize = 4 << 20;
/// Bytes of loaded notes kept in the per-render cache.
const MAX_CACHE_BYTES: usize = 16 << 20;

pub fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn split_target(dest: &str) -> (&str, Option<&str>) {
    match dest.find(['#', '^']) {
        Some(i) => (&dest[..i], Some(&dest[i + 1..])),
        None => (dest, None),
    }
}

/// Opening `<a>` for a vault file that is not a note: canvases open in the app, anything
/// else is served from `/api/file/` (keeping a fragment such as `#page=2`).
fn file_link(p: &str, frag: Option<&str>) -> String {
    if p.to_lowercase().ends_with(".canvas") {
        return format!("<a class=\"file\" href=\"#/c/{}\">", url_encode(p));
    }
    format!(
        "<a class=\"file\" href=\"/api/file/{}{}\" target=\"_blank\">",
        url_encode(p),
        frag.map(|f| format!("#{}", escape(f))).unwrap_or_default()
    )
}

/// Renders note content (frontmatter is stripped) for the note at `src`.
#[cfg(test)]
pub fn render(src: &str, content: &str, resolver: &Resolver) -> String {
    render_with(src, content, resolver, &|_| None)
}

/// Like [`render`], with a loader so `![[Note]]` / `![[Note#Heading]]` can embed other notes.
///
/// Task-list checkboxes of the note itself (not of embedded notes) carry
/// `data-task="<n>"`: the 0-based position of the task item in the note, in document order.
pub fn render_with(
    src: &str,
    content: &str,
    resolver: &Resolver,
    load: &dyn Fn(&str) -> Option<String>,
) -> String {
    let mut ctx = Ctx {
        resolver,
        load,
        top: (src, content),
        chain: vec![(src.to_string(), None)],
        embeds: 0,
        embed_bytes: 0,
        cache: HashMap::new(),
        cache_bytes: 0,
        tasks: 0,
    };
    sanitize(&render_inner(src, content, &mut ctx))
}

/// Per-render state shared by a note and everything it transcludes. The budgets keep a
/// note that embeds notes that embed notes (a DAG, not a cycle) from fanning out into
/// millions of reads and gigabytes of HTML.
struct Ctx<'a> {
    resolver: &'a Resolver,
    load: &'a dyn Fn(&str) -> Option<String>,
    /// The note being rendered and its (possibly unsaved) text, used for self-embeds.
    top: (&'a str, &'a str),
    /// Notes (and sections) from the top-level note down to the one being rendered.
    chain: Vec<(String, Option<String>)>,
    /// Transclusions attempted so far, and the Markdown + HTML bytes they added.
    embeds: usize,
    embed_bytes: usize,
    /// Notes loaded during this render, so a note embedded many times is read once.
    cache: HashMap<String, Option<Rc<str>>>,
    cache_bytes: usize,
    /// Task-list items of the top-level note seen so far.
    tasks: usize,
}

impl Ctx<'_> {
    /// A whole note may not embed itself or an ancestor; a section may not embed itself.
    fn can_embed(&self, p: &str, sel: Option<&str>) -> bool {
        self.chain.len() <= MAX_EMBED_DEPTH
            && self.embeds < MAX_EMBEDS
            && self.embed_bytes < MAX_EMBED_BYTES
            && !self
                .chain
                .iter()
                .any(|(cp, cs)| cp == p && (sel.is_none() || cs.as_deref() == sel))
    }

    fn note_text(&mut self, p: &str) -> Option<Rc<str>> {
        if let Some(c) = self.cache.get(p) {
            return c.clone();
        }
        let text: Option<Rc<str>> = if p == self.top.0 {
            Some(Rc::from(self.top.1))
        } else {
            (self.load)(p).map(Rc::from)
        };
        let n = text.as_ref().map_or(0, |t| t.len());
        if self.cache_bytes + n <= MAX_CACHE_BYTES {
            self.cache_bytes += n;
            self.cache.insert(p.to_string(), text.clone());
        }
        text
    }
}

/// Extracts the part of a note under a heading (until the next heading of the same or a
/// higher level), or the paragraph carrying a `^block-id`.
pub fn section(content: &str, frag: &str) -> Option<String> {
    let (_, body) = note::split_frontmatter(content);
    let lines: Vec<&str> = body.lines().collect();
    if let Some(id) = frag.strip_prefix('^') {
        let marker = format!("^{id}");
        let l = lines.iter().find(|l| l.trim_end().ends_with(&marker))?;
        return Some(
            l.trim_end()
                .trim_end_matches(&marker)
                .trim_end()
                .to_string(),
        );
    }
    let want = frag.trim().to_lowercase();
    let level_of = |l: &str| {
        let t = l.trim_start();
        let n = t.chars().take_while(|c| *c == '#').count();
        ((1..=6).contains(&n) && t[n..].starts_with(' ')).then_some(n)
    };
    let start = lines.iter().position(|l| {
        level_of(l).is_some()
            && l.trim_start().trim_start_matches('#').trim().to_lowercase() == want
    })?;
    let lvl = level_of(lines[start]).unwrap();
    let end = lines[start + 1..]
        .iter()
        .position(|l| level_of(l).is_some_and(|n| n <= lvl))
        .map(|i| start + 1 + i)
        .unwrap_or(lines.len());
    Some(lines[start + 1..end].join("\n"))
}

/// Transcludes note `p` (or one section/block of it); falls back to a plain link when the
/// embed would loop, nest too deep or exceed the per-render budget.
fn embed_note(p: &str, dest: &str, frag: Option<&str>, ctx: &mut Ctx) -> String {
    let label = format!(
        "{}{}",
        escape(note::stem(p)),
        frag.map(|f| format!(" › {}", escape(f.trim_start_matches('^'))))
            .unwrap_or_default()
    );
    // `Note^id` and `Note#^id` select a block, `Note#Heading` a section.
    let block = dest.contains('^') && !dest.contains("#^");
    let sel = frag.map(|f| format!("{}{f}", if block { "^" } else { "" }));
    let key = sel.as_ref().map(|s| s.trim().to_lowercase());
    let mut inner = None;
    if ctx.can_embed(p, key.as_deref()) {
        // Counted before loading, so failed lookups cannot be repeated without limit.
        ctx.embeds += 1;
        let budget = MAX_EMBED_BYTES - ctx.embed_bytes;
        inner = ctx
            .note_text(p)
            .and_then(|c| match &sel {
                Some(s) => section(&c, s),
                None => Some(note::split_frontmatter(&c).1.to_string()),
            })
            .filter(|md| md.len() <= budget);
    }
    match inner {
        Some(md) => {
            ctx.embed_bytes += md.len();
            ctx.chain.push((p.to_string(), key));
            let html = render_inner(p, &md, ctx);
            ctx.chain.pop();
            ctx.embed_bytes += html.len();
            format!(
                "<div class=\"embed-note\"><a class=\"embed-title wikilink\" data-note=\"{}\" href=\"#/n/{}\">{label}</a><div class=\"embed-body\">{html}</div></div>",
                escape(p),
                url_encode(p)
            )
        }
        None => format!(
            "<a class=\"embed wikilink\" data-note=\"{}\" href=\"#/n/{}\">{label}</a>",
            escape(p),
            url_encode(p)
        ),
    }
}

fn render_inner(src: &str, content: &str, ctx: &mut Ctx) -> String {
    let resolver = ctx.resolver;
    let top = ctx.chain.len() == 1;
    // Footnote labels of embedded notes get a per-embed prefix so their ids stay unique.
    let ns = (!top).then(|| format!("e{}-", ctx.embeds));
    let (_, body) = note::split_frontmatter(content);
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_MATH
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM
        | Options::ENABLE_WIKILINKS
        | Options::ENABLE_HEADING_ATTRIBUTES;

    let mut events: Vec<Event> = Vec::new();
    let mut in_code = false;
    // When an embed or wikilink is rewritten to raw HTML, we skip events until its end tag.
    let mut skip_depth: Option<usize> = None;
    let mut depth = 0usize;
    let mut link_stack: Vec<bool> = Vec::new();
    // Collects the text of a ```base block.
    let mut base_block: Option<String> = None;

    for ev in Parser::new_ext(body, opts) {
        if let Some(buf) = base_block.as_mut() {
            match ev {
                Event::Text(t) => buf.push_str(&t),
                Event::End(TagEnd::CodeBlock) => {
                    depth -= 1;
                    in_code = false;
                    let spec = base_block.take().unwrap_or_default();
                    events.push(Event::Html(CowStr::from(format!(
                        "<div class=\"base-view\" data-spec=\"{}\"></div>\n",
                        escape(&spec)
                    ))));
                }
                _ => {}
            }
            continue;
        }
        if let Some(d) = skip_depth {
            match &ev {
                Event::Start(_) => depth += 1,
                Event::End(_) => {
                    depth -= 1;
                    if depth == d {
                        skip_depth = None;
                    }
                }
                _ => {}
            }
            continue;
        }
        match ev {
            Event::Start(Tag::CodeBlock(pulldown_cmark::CodeBlockKind::Fenced(lang)))
                if lang.trim() == "base" =>
            {
                in_code = true;
                depth += 1;
                base_block = Some(String::new());
            }
            Event::Start(Tag::CodeBlock(k)) => {
                in_code = true;
                depth += 1;
                events.push(Event::Start(Tag::CodeBlock(k)));
            }
            Event::InlineMath(m) => {
                events.push(Event::InlineHtml(CowStr::from(format!(
                    "<span class=\"math-inline\">{}</span>",
                    escape(&m)
                ))));
            }
            Event::DisplayMath(m) => {
                events.push(Event::InlineHtml(CowStr::from(format!(
                    "<span class=\"math-display\">{}</span>",
                    escape(&m)
                ))));
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code = false;
                depth -= 1;
                events.push(Event::End(TagEnd::CodeBlock));
            }
            Event::TaskListMarker(done) if top => {
                events.push(Event::InlineHtml(CowStr::from(format!(
                    "<input type=\"checkbox\" disabled{} data-task=\"{}-{}\">\n",
                    if done { " checked" } else { "" },
                    TASK_SECRET.as_str(),
                    ctx.tasks
                ))));
                ctx.tasks += 1;
            }
            Event::FootnoteReference(label) => {
                let label = match &ns {
                    Some(n) => CowStr::from(format!("{n}{label}")),
                    None => label,
                };
                events.push(Event::FootnoteReference(label));
            }
            Event::Start(Tag::FootnoteDefinition(label)) => {
                let label = match &ns {
                    Some(n) => CowStr::from(format!("{n}{label}")),
                    None => label,
                };
                depth += 1;
                events.push(Event::Start(Tag::FootnoteDefinition(label)));
            }
            Event::Start(Tag::Image {
                link_type: LinkType::WikiLink { .. },
                dest_url,
                ..
            }) => {
                let (target, frag) = split_target(&dest_url);
                // `![[#Heading]]` embeds a section of the note itself.
                let resolved = if target.is_empty() {
                    Some(src.to_string())
                } else {
                    resolver.resolve(src, target)
                };
                let ext = target
                    .rsplit_once('.')
                    .map(|(_, e)| e.to_lowercase())
                    .unwrap_or_default();
                let html = match (&resolved, IMAGE_EXT.contains(&ext.as_str())) {
                    (Some(p), true) => format!(
                        "<img src=\"/api/file/{}\" alt=\"{}\" loading=\"lazy\">",
                        url_encode(p),
                        escape(target)
                    ),
                    (Some(p), false) if note::is_note_path(p) => {
                        embed_note(p, &dest_url, frag, ctx)
                    }
                    (Some(p), false) => format!("{}{}</a>", file_link(p, None), escape(target)),
                    (None, _) => {
                        format!("<span class=\"wikilink broken\">{}</span>", escape(target))
                    }
                };
                events.push(Event::InlineHtml(CowStr::from(html)));
                skip_depth = Some(depth);
                depth += 1;
            }
            Event::Start(Tag::Link {
                link_type: LinkType::WikiLink { .. },
                dest_url,
                ..
            }) => {
                let (target, frag) = split_target(&dest_url);
                let resolved = if target.is_empty() {
                    Some(src.to_string())
                } else {
                    resolver.resolve(src, target)
                };
                let html = match resolved {
                    Some(p) if note::is_note_path(&p) => format!(
                        "<a class=\"wikilink\" data-note=\"{}\" data-anchor=\"{}\" href=\"#/n/{}\">",
                        escape(&p),
                        escape(frag.unwrap_or("")),
                        url_encode(&p)
                    ),
                    // `[[report.pdf]]` and other attachments link to the file itself.
                    Some(p) => file_link(&p, None),
                    // `[[Missing#Heading]]` offers to create "Missing"; the heading is not
                    // part of the new note's name.
                    None => format!(
                        "<a class=\"wikilink broken\" data-create=\"{}\" href=\"#/new/{}\">",
                        escape(target),
                        url_encode(target)
                    ),
                };
                link_stack.push(true);
                depth += 1;
                events.push(Event::InlineHtml(CowStr::from(html)));
            }
            Event::Start(t @ Tag::Link { .. }) => {
                // Relative links to other notes become in-app links.
                if let Tag::Link { dest_url, .. } = &t {
                    let d = dest_url.to_string();
                    if !d.contains("://") && !d.starts_with('#') && !d.starts_with("mailto:") {
                        // Only `#` starts the fragment of a Markdown link (blocks are `#^id`).
                        let (target, frag) = match d.split_once('#') {
                            Some((t, f)) => (t, Some(f)),
                            None => (d.as_str(), None),
                        };
                        let decoded = note::percent_decode(target);
                        if let Some(p) = resolver.resolve(src, &decoded) {
                            let html = if note::is_note_path(&p) {
                                let anchor = frag
                                    .map(|f| {
                                        format!(
                                            " data-anchor=\"{}\"",
                                            escape(&note::percent_decode(f))
                                        )
                                    })
                                    .unwrap_or_default();
                                format!(
                                    "<a class=\"wikilink\" data-note=\"{}\"{anchor} href=\"#/n/{}\">",
                                    escape(&p),
                                    url_encode(&p)
                                )
                            } else {
                                file_link(&p, frag)
                            };
                            link_stack.push(true);
                            depth += 1;
                            events.push(Event::InlineHtml(CowStr::from(html)));
                            continue;
                        }
                    }
                }
                link_stack.push(false);
                depth += 1;
                events.push(Event::Start(t));
            }
            Event::End(TagEnd::Link) => {
                depth -= 1;
                if link_stack.pop().unwrap_or(false) {
                    events.push(Event::InlineHtml(CowStr::from("</a>")));
                } else {
                    events.push(Event::End(TagEnd::Link));
                }
            }
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                // Local image paths are served from the vault.
                let d = dest_url.to_string();
                let dest = if !d.contains("://") && !d.starts_with("data:") {
                    match resolver.resolve(src, &note::percent_decode(&d)) {
                        Some(p) => CowStr::from(format!("/api/file/{}", url_encode(&p))),
                        None => dest_url,
                    }
                } else {
                    dest_url
                };
                depth += 1;
                events.push(Event::Start(Tag::Image {
                    link_type,
                    dest_url: dest,
                    title,
                    id,
                }));
            }
            Event::Text(t) if !in_code && link_stack.is_empty() && TAG_IN_TEXT.is_match(&t) => {
                let mut last = 0;
                let mut html = String::new();
                for cap in TAG_IN_TEXT.captures_iter(&t) {
                    let m = cap.get(0).unwrap();
                    let tag = cap.get(2).unwrap().as_str();
                    if tag.chars().all(|c| c.is_ascii_digit()) {
                        continue;
                    }
                    html.push_str(&escape(&t[last..m.start()]));
                    html.push_str(&escape(&cap[1]));
                    html.push_str(&format!(
                        "<a class=\"tag\" data-tag=\"{}\" href=\"#/tag/{}\">#{}</a>",
                        escape(&tag.to_lowercase()),
                        url_encode(&tag.to_lowercase()),
                        escape(tag)
                    ));
                    last = m.end();
                }
                html.push_str(&escape(&t[last..]));
                events.push(Event::InlineHtml(CowStr::from(html)));
            }
            e @ Event::Start(_) => {
                depth += 1;
                events.push(e);
            }
            e @ Event::End(_) => {
                depth = depth.saturating_sub(1);
                events.push(e);
            }
            e => events.push(e),
        }
    }

    let mut out = String::new();
    html::push_html(&mut out, events.into_iter());
    out
}

/// Built once: rebuilding the allow-lists on every render was measurable.
static SANITIZER: LazyLock<ammonia::Builder<'static>> = LazyLock::new(|| {
    let mut b = ammonia::Builder::default();
    b.add_generic_attributes(["class", "id"])
        .add_tag_attributes(
            "a",
            [
                "data-note",
                "data-anchor",
                "data-create",
                "data-tag",
                "target",
            ],
        )
        .add_tag_attributes("img", ["loading"])
        .add_tag_attributes("div", ["data-spec"])
        .add_tags(["input"])
        .add_tag_attributes("input", ["checked", "data-task"])
        // Whatever a note's raw HTML asks for, an <input> is only ever an inert checkbox.
        .set_tag_attribute_value("input", "type", "checkbox")
        .set_tag_attribute_value("input", "disabled", "")
        .add_tag_attributes("ol", ["start"])
        // `data:` passes the scheme check only to reach `filter_attribute`, which keeps
        // it for raster `<img src>` alone.
        .add_url_schemes(["data"])
        .attribute_filter(filter_attribute)
        .link_rel(Some("noopener noreferrer"))
        .id_prefix(Some(ID_PREFIX));
    b
});

pub fn sanitize(html: &str) -> String {
    SANITIZER.clean(html).to_string()
}

/// Final per-attribute pass of the sanitizer (runs after the allow-lists).
fn filter_attribute<'u>(el: &str, attr: &str, value: &'u str) -> Option<Cow<'u, str>> {
    match attr {
        "class" => {
            let keep: Vec<&str> = value
                .split_ascii_whitespace()
                .filter(|c| allowed_class(el, c))
                .collect();
            (!keep.is_empty()).then(|| Cow::Owned(keep.join(" ")))
        }
        "data-task" => value
            .strip_prefix(TASK_SECRET.as_str())
            .and_then(|v| v.strip_prefix('-'))
            .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .map(Cow::Borrowed),
        "href" | "src" if is_data_url(value) => {
            (el == "img" && attr == "src" && is_raster_data_url(value))
                .then_some(Cow::Borrowed(value))
        }
        // Footnotes and `[x](#id)` point at ids the sanitizer prefixed.
        "href"
            if value.len() > 1
                && value.starts_with('#')
                && !value.starts_with("#/")
                && !value[1..].starts_with(ID_PREFIX) =>
        {
            Some(Cow::Owned(format!("#{ID_PREFIX}{}", &value[1..])))
        }
        _ => Some(Cow::Borrowed(value)),
    }
}

fn allowed_class(el: &str, class: &str) -> bool {
    UI_CLASSES.contains(&class)
        || (el == "code"
            && class.strip_prefix("language-").is_some_and(|l| {
                !l.is_empty()
                    && l.len() <= 32
                    && l.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"+#._-".contains(&b))
            }))
}

/// The URL with ASCII tabs/newlines removed and leading spaces/controls skipped, the
/// way browsers read it.
fn url_chars(v: &str) -> impl Iterator<Item = char> + '_ {
    v.trim_start_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
}

fn is_data_url(v: &str) -> bool {
    let mut head = url_chars(v);
    "data:"
        .chars()
        .all(|want| head.next().is_some_and(|c| c.eq_ignore_ascii_case(&want)))
}

/// `data:image/png|jpeg|gif|webp` — no SVG, which can carry markup.
fn is_raster_data_url(v: &str) -> bool {
    let mime: String = url_chars(v)
        .skip(5)
        .take_while(|c| !matches!(c, ';' | ','))
        .take(64)
        .collect();
    matches!(
        mime.trim().to_ascii_lowercase().as_str(),
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;

    fn resolver() -> (tempfile::TempDir, std::sync::Arc<Resolver>) {
        let d = tempfile::tempdir().unwrap();
        let mut i = Index::open(&d.path().join("i.db")).unwrap();
        i.upsert("Notes/Target.md", "x", 1, 1, "t").unwrap();
        i.upsert_file("attachments/cat.png", 1, 1).unwrap();
        i.upsert_file("attachments/report.pdf", 1, 1).unwrap();
        for n in ["Notes/B.md", "Notes/C.md", "Notes/D.md"] {
            i.upsert(n, "x", 1, 1, "t").unwrap();
        }
        let r = i.resolver_snapshot().unwrap();
        (d, r)
    }

    #[test]
    fn wikilinks_tags_embeds() {
        let (_d, r) = resolver();
        let h = render(
            "a.md",
            "---\nx: 1\n---\n[[Target]] [[Missing|Yok]] #etiket ![[cat.png]] `#nocode`",
            &r,
        );
        assert!(h.contains("data-note=\"Notes/Target.md\""), "{h}");
        assert!(h.contains("wikilink broken"), "{h}");
        assert!(h.contains(">Yok</a>"), "{h}");
        assert!(h.contains("class=\"tag\""), "{h}");
        assert!(h.contains("/api/file/attachments/cat.png"), "{h}");
        assert!(!h.contains("x: 1"), "{h}");
        assert!(h.contains("<code>#nocode</code>"), "{h}");
    }

    #[test]
    fn sanitizes_scripts() {
        let (_d, r) = resolver();
        let h = render(
            "a.md",
            "<script>alert(1)</script><img src=x onerror=alert(1)>\n\n[x](javascript:alert) [[a\"><b onclick=x>]]",
            &r,
        );
        assert!(!h.contains("<script"), "{h}");
        assert!(!h.contains("onerror"), "{h}");
        assert!(!h.contains("javascript:"), "{h}");
        assert!(!h.contains("<b onclick"), "{h}");
    }

    #[test]
    fn transclusion_math_and_bases() {
        let (_d, r) = resolver();
        let load = |p: &str| {
            (p == "Notes/Target.md").then(|| {
                "# T\n\n## Bölüm\nİçerik burada ![[Target]]\n\n## Diğer\nyok\n\nsatır ^abc"
                    .to_string()
            })
        };
        let h = render_with(
            "a.md",
            "![[Target#Bölüm]]\n\n![[Target#^abc]]\n\n$e=mc^2$\n\n```base\nfrom: X\n```\n",
            &r,
            &load,
        );
        assert!(h.contains("embed-note"), "{h}");
        assert!(h.contains("İçerik burada"), "{h}");
        assert!(!h.contains(">yok<"), "{h}");
        assert!(h.contains("satır"), "{h}");
        assert!(h.contains("math-inline"), "{h}");
        assert!(h.contains("data-spec=\"from: X"), "{h}");
        assert_eq!(
            section("# A\n## B\nx\n### C\ny\n## D\nz", "b").unwrap(),
            "x\n### C\ny"
        );
    }

    #[test]
    fn callouts_and_tasks() {
        let (_d, r) = resolver();
        let h = render(
            "a.md",
            "> [!WARNING]\n> dikkat\n\n- [x] done\n- [ ] todo",
            &r,
        );
        assert!(h.contains("markdown-alert-warning"), "{h}");
        assert!(h.contains("checkbox"), "{h}");
    }

    #[test]
    fn embed_fan_out_is_bounded_and_cached() {
        let (_d, r) = resolver();
        let loads = std::cell::Cell::new(0);
        // a → 10×B → 10×C → 10×D would be 1110 transclusions without a budget.
        let load = |p: &str| {
            loads.set(loads.get() + 1);
            let next = match p {
                "Notes/B.md" => "C",
                "Notes/C.md" => "D",
                "Notes/D.md" => return Some("leaf".to_string()),
                _ => return None,
            };
            Some(format!("![[{next}]]\n\n").repeat(10))
        };
        let h = render_with("a.md", &"![[B]]\n\n".repeat(10), &r, &load);
        let embedded = h.matches("class=\"embed-note\"").count();
        assert!((3..=MAX_EMBEDS).contains(&embedded), "{embedded}");
        assert!(h.contains("leaf"), "{h}");
        assert_eq!(loads.get(), 3, "each note is read once");
        // Over budget, embeds degrade to plain links.
        assert!(h.contains("class=\"embed wikilink\""), "{h}");
    }

    #[test]
    fn footnote_and_anchor_ids_match() {
        let (_d, r) = resolver();
        let h = render(
            "a.md",
            "Text[^1] and [jump](#sec)\n\n## Sec {#sec}\n\n[^1]: The note.",
            &r,
        );
        assert!(
            h.contains("href=\"#h-1\"") && h.contains("id=\"h-1\""),
            "{h}"
        );
        assert!(
            h.contains("href=\"#h-sec\"") && h.contains("id=\"h-sec\""),
            "{h}"
        );
        // Embedded footnotes get their own ids.
        let load = |p: &str| (p == "Notes/Target.md").then(|| "x[^1]\n\n[^1]: inner".to_string());
        let h = render_with("a.md", "y[^1]\n\n[^1]: outer\n\n![[Target]]", &r, &load);
        assert_eq!(h.matches("id=\"h-1\"").count(), 1, "{h}");
        assert!(
            h.contains("id=\"h-e1-1\"") && h.contains("href=\"#h-e1-1\""),
            "{h}"
        );
    }

    #[test]
    fn task_checkboxes_carry_their_ordinal() {
        let (_d, r) = resolver();
        let load = |p: &str| (p == "Notes/Target.md").then(|| "- [ ] inner".to_string());
        let h = render_with(
            "a.md",
            "- [ ] one\n- [x] two\n\n```\n- [ ] code\n```\n\n> - [ ] quoted\n\n![[Target]]\n\n<input type=\"text\" data-task=\"7\" value=\"v\" onclick=\"x()\">\n",
            &r,
            &load,
        );
        for n in 0..3 {
            assert!(h.contains(&format!("data-task=\"{n}\"")), "{h}");
        }
        assert_eq!(h.matches("data-task=").count(), 3, "{h}");
        assert_eq!(h.matches("type=\"checkbox\"").count(), 5, "{h}");
        assert_eq!(h.matches("checked").count(), 1, "{h}");
        let embed = &h[h.find("embed-body").unwrap()..];
        assert!(
            embed.contains("checkbox")
                && !embed[..embed.find("</div>").unwrap()].contains("data-task"),
            "{h}"
        );
        assert!(!h.contains("type=\"text\""), "{h}");
        assert!(!h.contains("onclick") && !h.contains("value="), "{h}");
        assert!(!h.contains(TASK_SECRET.as_str()), "{h}");
    }

    #[test]
    fn sanitizer_allow_lists() {
        let (_d, r) = resolver();
        let h = render(
            "a.md",
            "![p](data:image/png;base64,iVBORw0KGgo=) ![s](data:image/svg+xml;base64,PHN2Zz4=)\n\n<a href=\"data:text/html,x\">d</a> <span class=\"evil wikilink\">c</span> <img src=\" DATA:text/html,y\">\n\n```rust\nfn x() {}\n```\n\n## H {.custom}",
            &r,
        );
        assert!(
            h.contains("src=\"data:image/png;base64,iVBORw0KGgo=\""),
            "{h}"
        );
        assert!(!h.contains("svg+xml") && !h.contains("text/html"), "{h}");
        assert!(
            h.contains("<span class=\"wikilink\">") && !h.contains("evil"),
            "{h}"
        );
        assert!(h.contains("class=\"language-rust\""), "{h}");
        assert!(!h.contains("custom"), "{h}");
    }

    #[test]
    fn link_targets() {
        let (_d, r) = resolver();
        let h = render(
            "a.md",
            "[[report.pdf]] [[Missing#Başlık]] [t](Notes/Target.md#B%C3%B6l%C3%BCm) [p](attachments/report.pdf#page=2)",
            &r,
        );
        assert!(
            h.contains("href=\"/api/file/attachments/report.pdf\""),
            "{h}"
        );
        assert!(
            h.contains("href=\"/api/file/attachments/report.pdf#page=2\""),
            "{h}"
        );
        assert!(h.contains("data-create=\"Missing\""), "{h}");
        assert!(h.contains("href=\"#/new/Missing\""), "{h}");
        assert!(h.contains("data-anchor=\"Bölüm\""), "{h}");
        // `![[#Heading]]` embeds a section of the note being rendered (unsaved text).
        let h = render(
            "a.md",
            "## Bölüm\nself text ![[#Bölüm]]\n\n## Other\n![[#Bölüm]]",
            &r,
        );
        assert!(h.contains("embed-note"), "{h}");
        assert_eq!(h.matches("self text").count(), 3, "{h}");
    }
}
