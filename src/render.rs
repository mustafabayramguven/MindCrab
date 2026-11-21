//! Markdown → sanitised HTML, with Obsidian-style wikilinks, embeds, callouts and tags.

use crate::index::Resolver;
use crate::note;
use pulldown_cmark::{CowStr, Event, LinkType, Options, Parser, Tag, TagEnd, html};
use regex::Regex;
use std::sync::LazyLock;

static TAG_IN_TEXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|[\s(])#([\p{L}\p{N}_][\p{L}\p{N}_/\-]*)").unwrap());

const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "avif", "bmp"];

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

/// Renders note content (frontmatter is stripped) for the note at `src`.
pub fn render(src: &str, content: &str, resolver: &Resolver) -> String {
    let (_, body) = note::split_frontmatter(content);
    let opts = Options::ENABLE_TABLES
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

    for ev in Parser::new_ext(body, opts) {
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
            Event::Start(Tag::CodeBlock(k)) => {
                in_code = true;
                depth += 1;
                events.push(Event::Start(Tag::CodeBlock(k)));
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code = false;
                depth -= 1;
                events.push(Event::End(TagEnd::CodeBlock));
            }
            Event::Start(Tag::Image {
                link_type: LinkType::WikiLink { .. },
                dest_url,
                title,
                id,
            }) => {
                let (target, frag) = split_target(&dest_url);
                let resolved = resolver.resolve(src, target);
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
                    (Some(p), false) if note::is_note_path(p) => format!(
                        "<a class=\"embed wikilink\" data-note=\"{}\" href=\"#/n/{}\">{}{}</a>",
                        escape(p),
                        url_encode(p),
                        escape(target),
                        frag.map(|f| format!(" › {}", escape(f)))
                            .unwrap_or_default()
                    ),
                    (Some(p), false) => {
                        format!(
                            "<a class=\"file\" href=\"/api/file/{}\" target=\"_blank\">{}</a>",
                            url_encode(p),
                            escape(target)
                        )
                    }
                    (None, _) => {
                        format!("<span class=\"wikilink broken\">{}</span>", escape(target))
                    }
                };
                let _ = (title, id);
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
                let anchor = frag
                    .map(|f| format!("#{}", url_encode(f)))
                    .unwrap_or_default();
                let html = match resolved {
                    Some(p) => format!(
                        "<a class=\"wikilink\" data-note=\"{}\" data-anchor=\"{}\" href=\"#/n/{}\">",
                        escape(&p),
                        escape(frag.unwrap_or("")),
                        url_encode(&p)
                    ),
                    None => format!(
                        "<a class=\"wikilink broken\" data-create=\"{}\" href=\"#/new/{}{anchor}\">",
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
                        let (target, _) = split_target(&d);
                        let decoded = note::percent_decode(target);
                        if let Some(p) = resolver.resolve(src, &decoded) {
                            let html = if note::is_note_path(&p) {
                                format!(
                                    "<a class=\"wikilink\" data-note=\"{}\" href=\"#/n/{}\">",
                                    escape(&p),
                                    url_encode(&p)
                                )
                            } else {
                                format!(
                                    "<a href=\"/api/file/{}\" target=\"_blank\">",
                                    url_encode(&p)
                                )
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
    sanitize(&out)
}

pub fn sanitize(html: &str) -> String {
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
        .add_tags(["input"])
        .add_tag_attributes("input", ["type", "checked", "disabled"])
        .add_tag_attributes("code", ["class"])
        .add_tag_attributes("ol", ["start"])
        .link_rel(Some("noopener noreferrer"))
        .id_prefix(Some("h-"));
    b.clean(html).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;

    fn resolver() -> (tempfile::TempDir, Resolver) {
        let d = tempfile::tempdir().unwrap();
        let mut i = Index::open(&d.path().join("i.db")).unwrap();
        i.upsert("Notes/Target.md", "x", 1, 1, "t").unwrap();
        i.upsert_file("attachments/cat.png", 1, 1).unwrap();
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
}
