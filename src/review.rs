//! Memory hygiene: finds facts in the memory folder that repeat each other.

use regex::Regex;
use serde::Serialize;
use std::collections::HashSet;
use std::sync::LazyLock;

/// The " — _2026-11-01, source_" stamp `remember` appends. The source name may
/// contain `_` (e.g. "claude_code"), so the stamp ends at the last underscore.
static DATE_STAMP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+—\s+_(\d{4}-\d{2}-\d{2})[^—]*_\s*$").unwrap());

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Fact {
    pub path: String,
    /// 1-based line number.
    pub line: usize,
    /// The full line as written (used to remove it safely).
    pub raw: String,
    /// Fact text without the bullet and the "— _date, source_" stamp.
    pub text: String,
    pub date: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Duplicate {
    pub a: Fact,
    pub b: Fact,
    /// 0..1 word-overlap similarity (1 = same words).
    pub score: f32,
}

/// Bullet lines of a memory note.
pub fn facts(path: &str, content: &str) -> Vec<Fact> {
    // Skip YAML frontmatter (its `  - memory` list items are not facts).
    let skip = match crate::note::split_frontmatter(content) {
        (Some(_), body) => content[..content.len() - body.len()].lines().count(),
        (None, _) => 0,
    };
    let mut out = Vec::new();
    let mut fence = false;
    for (i, l) in content.lines().enumerate().skip(skip) {
        let t = l.trim_start();
        if t.starts_with("```") {
            fence = !fence;
        }
        if fence {
            continue;
        }
        let Some(body) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) else {
            continue;
        };
        if body.starts_with("[ ]") || body.starts_with("[x]") {
            continue;
        }
        let date = DATE_STAMP.captures(body).map(|c| c[1].to_string());
        let text = DATE_STAMP.replace(body, "").trim().to_string();
        if text.is_empty() {
            continue;
        }
        out.push(Fact {
            path: path.into(),
            line: i + 1,
            raw: l.to_string(),
            text,
            date,
        });
    }
    out
}

fn words(s: &str) -> HashSet<String> {
    let folded: String = s
        .chars()
        .flat_map(char::to_lowercase)
        // 'İ' lowercases to "i\u{307}": drop combining marks instead of splitting on
        // them, so "İstanbul" stays one word (same for decomposed input).
        .filter(|c| !('\u{300}'..='\u{36f}').contains(c))
        .map(|c| match c {
            'ı' => 'i',
            'ş' => 's',
            'ğ' => 'g',
            'ü' => 'u',
            'ö' => 'o',
            'ç' => 'c',
            c if c.is_alphanumeric() => c,
            _ => ' ',
        })
        .collect();
    folded
        .split_whitespace()
        // crude stemming: compare by the first 5 letters so "sunucusu"/"sunucuda" match
        .map(|w| w.chars().take(5).collect::<String>())
        .filter(|w| w.chars().count() >= 2 || w.chars().all(|c| c.is_ascii_digit()))
        .collect()
}

#[cfg(test)]
fn similarity(a: &str, b: &str) -> f32 {
    overlap(&words(a), &words(b))
}

fn overlap(x: &HashSet<String>, y: &HashSet<String>) -> f32 {
    if x.is_empty() || y.is_empty() {
        return 0.0;
    }
    let inter = x.intersection(y).count() as f32;
    // Overlap coefficient: a short fact fully contained in a longer one counts as a repeat.
    inter / (x.len().min(y.len()) as f32).max(1.0) * 0.6 + inter / (x.union(y).count() as f32) * 0.4
}

pub fn duplicates(all: &[Fact], threshold: f32, limit: usize) -> Vec<Duplicate> {
    // Word sets once per fact, not once per pair.
    let sets: Vec<HashSet<String>> = all.iter().map(|f| words(&f.text)).collect();
    let mut out = Vec::new();
    for i in 0..all.len() {
        for j in i + 1..all.len() {
            let s = overlap(&sets[i], &sets[j]);
            if s >= threshold {
                out.push(Duplicate {
                    a: all[i].clone(),
                    b: all[j].clone(),
                    score: s,
                });
            }
        }
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_repeats() {
        let c = "# Sunucular\n\n- Oracle sunucusu 1 GB RAM — _2026-11-01, claude_\n- Oracle sunucusunda 1 GB RAM var — _2026-11-20, gemini_\n- Fish shell kullanır\n- [ ] görev\n";
        let f = facts("Memory/Sunucular.md", c);
        assert_eq!(f.len(), 3);
        assert_eq!(f[0].text, "Oracle sunucusu 1 GB RAM");
        assert_eq!(f[0].date.as_deref(), Some("2026-11-01"));
        let d = duplicates(&f, 0.6, 10);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].a.line, 3);
        assert!(similarity("Fish shell kullanır", "Oracle sunucusu") < 0.2);
    }

    #[test]
    fn turkish_dotted_capital_i() {
        assert!(similarity("İstanbul", "istanbul") > 0.99);
        assert!(similarity("İSTANBUL'da yaşar", "istanbulda yaşar") > 0.6);
        // decomposed "I\u{307}" folds the same way
        assert!(similarity("I\u{307}zmir", "izmir") > 0.99);
    }

    #[test]
    fn skips_frontmatter_and_keeps_underscored_sources() {
        let c = "---\ntags:\n  - memory\n---\n# T\n\n- Kahve sever — _2026-11-01, claude_code_\n";
        let f = facts("Memory/T.md", c);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].line, 7);
        assert_eq!(f[0].text, "Kahve sever");
        assert_eq!(f[0].date.as_deref(), Some("2026-11-01"));
        // an unclosed fence is not frontmatter
        assert_eq!(facts("x.md", "---\n- a fact\n").len(), 1);
    }
}
