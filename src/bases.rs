//! "Bases": table/card views over note frontmatter, written as a fenced block:
//!
//! ````markdown
//! ```base
//! from: Kitaplar            # folder (optional)
//! tag: kitap                # tag filter (optional)
//! where:                    # all must hold
//!   - puan >= 4
//!   - durum != bırakıldı
//!   - has yazar
//! columns: [title, yazar, puan, bitiş]
//! sort: -puan               # "-" = descending
//! limit: 50
//! view: table               # table | cards | list
//! ```
//! ````
//!
//! Built-in fields: `title`, `path`, `folder`, `name`, `tags`, `modified`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::cmp::Ordering;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Spec {
    pub from: Option<String>,
    pub tag: Option<String>,
    #[serde(rename = "where", deserialize_with = "one_or_many")]
    pub filters: Vec<String>,
    pub columns: Vec<String>,
    pub sort: Option<String>,
    pub limit: Option<usize>,
    pub view: Option<String>,
}

fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(match v {
        Value::String(s) => vec![s],
        Value::Array(a) => a
            .into_iter()
            .map(|x| {
                x.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| x.to_string())
            })
            .collect(),
        Value::Null => vec![],
        other => vec![other.to_string()],
    })
}

pub fn parse_spec(yaml: &str) -> Result<Spec, String> {
    if yaml.trim().is_empty() {
        return Ok(Spec::default());
    }
    serde_yaml::from_str::<Spec>(yaml).map_err(|e| format!("invalid base definition: {e}"))
}

/// One note as seen by a base query.
pub struct Row<'a> {
    pub path: &'a str,
    pub title: &'a str,
    pub mtime: i64,
    pub tags: &'a [String],
    pub fm: &'a Map<String, Value>,
}

impl Row<'_> {
    pub fn get(&self, field: &str) -> Value {
        match field {
            "title" => json!(self.title),
            "path" => json!(self.path),
            "name" => json!(crate::note::stem(self.path)),
            "folder" => json!(crate::note::parent_dir(self.path)),
            "tags" => json!(self.tags),
            "modified" => chrono::DateTime::from_timestamp_millis(self.mtime)
                .map(|d| json!(d.format("%Y-%m-%d").to_string()))
                .unwrap_or(Value::Null),
            f => self
                .fm
                .get(f)
                .or_else(|| {
                    let want = fold(f);
                    self.fm
                        .iter()
                        .find(|(k, _)| fold(k) == want)
                        .map(|(_, v)| v)
                })
                .cloned()
                .unwrap_or(Value::Null),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Op {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    Contains,
    Has,
    Missing,
}

#[derive(Debug, Clone)]
struct Cond {
    field: String,
    op: Op,
    value: String,
}

fn parse_cond(s: &str) -> Result<Cond, String> {
    let s = s.trim();
    if let Some(f) = s.strip_prefix("has ") {
        return Ok(Cond {
            field: f.trim().into(),
            op: Op::Has,
            value: String::new(),
        });
    }
    if let Some(f) = s
        .strip_prefix("missing ")
        .or_else(|| s.strip_prefix("!has "))
    {
        return Ok(Cond {
            field: f.trim().into(),
            op: Op::Missing,
            value: String::new(),
        });
    }
    // Longest token first, so ">=" wins over ">" at the same position.
    let ops = [
        (" contains ", Op::Contains),
        (">=", Op::Ge),
        ("<=", Op::Le),
        ("!=", Op::Ne),
        ("==", Op::Eq),
        ("=", Op::Eq),
        (">", Op::Gt),
        ("<", Op::Lt),
    ];
    // Split at the first operator after the field name; operators inside the value
    // ("url = a?b=c", "x = a>=b") stay part of the value.
    for (i, _) in s.char_indices() {
        let rest = &s[i..];
        let Some((tok, op)) = ops.iter().find(|(tok, _)| rest.starts_with(tok)) else {
            continue;
        };
        let field = s[..i].trim();
        if field.is_empty() {
            break;
        }
        let v = rest[tok.len()..]
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .to_string();
        return Ok(Cond {
            field: field.into(),
            op: op.clone(),
            value: v,
        });
    }
    Err(format!(
        "cannot understand condition '{s}' (use e.g. 'puan >= 4', 'has yazar', 'tags contains kitap')"
    ))
}

fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Array(a) => a.iter().map(as_text).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

/// Case folding for case-insensitive comparisons: full Unicode lowercase, with the
/// Turkish dotted/dotless I folded together ('İ', 'I', 'ı' → 'i') so that
/// "İstanbul", "ISTANBUL" and "istanbul" (or "BIRAKILDI" and "bırakıldı") match.
fn fold(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            'İ' | 'I' | 'ı' => 'i'.to_lowercase(),
            c => c.to_lowercase(),
        })
        // combining dot above (decomposed 'İ')
        .filter(|&c| c != '\u{307}')
        .collect()
}

/// A finite number; "NaN", "inf", "infinity" and the like stay text.
fn parse_num(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|x| x.is_finite())
}

fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .filter(|x| x.is_finite())
        .or_else(|| parse_num(&as_text(v)))
}

/// Null, "" and [] all mean "no value".
fn is_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        _ => false,
    }
}

fn cmp_values(a: &Value, b: &str) -> Ordering {
    if let (Some(x), Some(y)) = (number(a), parse_num(b)) {
        // Both are finite, so this never falls back.
        return x.partial_cmp(&y).unwrap_or(Ordering::Equal);
    }
    fold(&as_text(a)).cmp(&fold(b))
}

/// `field = value`; a list "equals" a value when it contains it.
fn equals(v: &Value, want: &str) -> bool {
    match v {
        Value::Array(a) => a.iter().any(|x| cmp_values(x, want) == Ordering::Equal),
        Value::Bool(b) => b.to_string() == fold(want),
        other => cmp_values(other, want) == Ordering::Equal,
    }
}

fn check(c: &Cond, row: &Row) -> bool {
    let v = row.get(&c.field);
    let present = !is_empty(&v);
    match c.op {
        Op::Has => present,
        Op::Missing => !present,
        Op::Contains => match &v {
            Value::Array(a) => {
                let want = fold(&c.value);
                a.iter().any(|x| fold(&as_text(x)) == want)
            }
            other => fold(&as_text(other)).contains(&fold(&c.value)),
        },
        Op::Eq => equals(&v, &c.value),
        // Exact negation of `=`, so `x = a` and `x != a` never both hold.
        Op::Ne => !equals(&v, &c.value),
        Op::Gt => present && cmp_values(&v, &c.value) == Ordering::Greater,
        Op::Ge => present && cmp_values(&v, &c.value) != Ordering::Less,
        Op::Lt => present && cmp_values(&v, &c.value) == Ordering::Less,
        Op::Le => present && cmp_values(&v, &c.value) != Ordering::Greater,
    }
}

/// Sort key with a total order: numbers (also numeric text) < text < empty.
#[derive(Debug)]
enum SortKey {
    Num(f64),
    Text(String),
    Empty,
}

impl SortKey {
    fn of(v: &Value) -> SortKey {
        if is_empty(v) {
            SortKey::Empty
        } else if let Some(n) = number(v) {
            SortKey::Num(n)
        } else {
            SortKey::Text(fold(&as_text(v)))
        }
    }

    /// Empty values stay last in both directions; the rest is reversed as a whole
    /// when descending, which keeps the order total.
    fn order(&self, other: &SortKey, desc: bool) -> Ordering {
        use SortKey::*;
        let o = match (self, other) {
            (Empty, Empty) => return Ordering::Equal,
            (Empty, _) => return Ordering::Greater,
            (_, Empty) => return Ordering::Less,
            (Num(x), Num(y)) => x.total_cmp(y),
            (Num(_), Text(_)) => Ordering::Less,
            (Text(_), Num(_)) => Ordering::Greater,
            (Text(x), Text(y)) => x.cmp(y),
        };
        if desc { o.reverse() } else { o }
    }
}

#[derive(Debug, Serialize)]
pub struct BaseResult {
    pub columns: Vec<String>,
    pub rows: Vec<ResultRow>,
    pub total: usize,
    pub view: String,
}

#[derive(Debug, Serialize)]
pub struct ResultRow {
    pub path: String,
    pub values: Vec<Value>,
}

pub fn run(spec: &Spec, rows: &[Row]) -> Result<BaseResult, String> {
    let conds = spec
        .filters
        .iter()
        .map(|s| parse_cond(s))
        .collect::<Result<Vec<_>, _>>()?;
    let folder = spec
        .from
        .as_deref()
        .map(|f| f.trim_matches('/').to_string())
        .filter(|f| !f.is_empty());
    let tag = spec
        .tag
        .as_deref()
        .map(|t| t.trim_start_matches('#').to_lowercase());
    let mut hit: Vec<&Row> = rows
        .iter()
        .filter(|r| {
            folder
                .as_ref()
                .is_none_or(|f| r.path.starts_with(&format!("{f}/")))
        })
        .filter(|r| {
            tag.as_ref().is_none_or(|t| {
                r.tags
                    .iter()
                    .any(|x| x == t || x.starts_with(&format!("{t}/")))
            })
        })
        .filter(|r| conds.iter().all(|c| check(c, r)))
        .collect();
    if let Some(sort) = spec
        .sort
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let (desc, field) = match sort.strip_prefix('-') {
            Some(f) => (true, f.trim()),
            None => (false, sort.trim_start_matches('+').trim()),
        };
        // One key per row up front: comparing raw values pairwise mixed numeric and
        // text comparison, which is not a total order (sort_by may panic on that).
        let mut keyed: Vec<(SortKey, &Row)> = hit
            .into_iter()
            .map(|r| (SortKey::of(&r.get(field)), r))
            .collect();
        keyed.sort_by(|a, b| a.0.order(&b.0, desc));
        hit = keyed.into_iter().map(|(_, r)| r).collect();
    } else {
        hit.sort_by_key(|r| r.title.to_lowercase());
    }
    let total = hit.len();
    hit.truncate(spec.limit.unwrap_or(200).min(1000));
    let mut columns = spec.columns.clone();
    if columns.is_empty() {
        // title + every frontmatter key seen (first 6), like an automatic table.
        columns.push("title".into());
        for r in &hit {
            for k in r.fm.keys() {
                if !columns.contains(k) && k != "title" && columns.len() < 7 {
                    columns.push(k.clone());
                }
            }
        }
    }
    let rows = hit
        .iter()
        .map(|r| ResultRow {
            path: r.path.to_string(),
            values: columns.iter().map(|c| r.get(c)).collect(),
        })
        .collect();
    Ok(BaseResult {
        columns,
        rows,
        total,
        view: spec.view.clone().unwrap_or_else(|| "table".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_frontmatter() {
        let fm1 = crate::note::parse_frontmatter("yazar: Orwell\npuan: 5\ndurum: bitti");
        let fm2 = crate::note::parse_frontmatter("yazar: Le Guin\npuan: 3");
        let fm3 = crate::note::parse_frontmatter("puan: 4\ndurum: bırakıldı");
        let t: Vec<String> = vec!["kitap".into()];
        let rows = vec![
            Row {
                path: "Kitaplar/1984.md",
                title: "1984",
                mtime: 0,
                tags: &t,
                fm: &fm1,
            },
            Row {
                path: "Kitaplar/Mülksüzler.md",
                title: "Mülksüzler",
                mtime: 0,
                tags: &t,
                fm: &fm2,
            },
            Row {
                path: "Kitaplar/X.md",
                title: "X",
                mtime: 0,
                tags: &[],
                fm: &fm3,
            },
            Row {
                path: "Diğer/Y.md",
                title: "Y",
                mtime: 0,
                tags: &t,
                fm: &fm1,
            },
        ];
        let spec = parse_spec("from: Kitaplar\nwhere:\n  - puan >= 3\n  - durum != bırakıldı\ncolumns: [title, yazar, puan]\nsort: -puan").unwrap();
        let r = run(&spec, &rows).unwrap();
        let titles: Vec<_> = r
            .rows
            .iter()
            .map(|x| x.values[0].as_str().unwrap())
            .collect();
        assert_eq!(titles, vec!["1984", "Mülksüzler"]);
        let r = run(&parse_spec("tag: kitap\nwhere: has yazar").unwrap(), &rows).unwrap();
        assert_eq!(r.total, 3);
        assert!(r.columns.contains(&"yazar".to_string()));
        assert!(run(&parse_spec("where: ???").unwrap(), &rows).is_err());
        let r = run(
            &parse_spec("where: tags contains kitap\nsort: name").unwrap(),
            &rows,
        )
        .unwrap();
        assert_eq!(r.total, 3);
    }

    fn row<'a>(path: &'a str, tags: &'a [String], fm: &'a Map<String, Value>) -> Row<'a> {
        Row {
            path,
            title: path,
            mtime: 0,
            tags,
            fm,
        }
    }

    /// Numbers, numeric strings, text, NaN/Inf strings and missing values mixed.
    fn mixed_values() -> Vec<Value> {
        (0..600u64)
            .map(|i| match i % 6 {
                0 | 3 => json!(i * 7 % 37),
                1 | 4 => json!((i * 13 % 41).to_string()),
                2 => json!(format!("t{}", i % 11)),
                _ => match i / 6 % 4 {
                    0 => Value::Null,
                    1 => json!((i % 29) as f64 + 0.5),
                    2 => {
                        let s = ["NaN", "inf", "Infinity"][(i % 3) as usize];
                        json!(s)
                    }
                    _ => json!(""),
                },
            })
            .collect()
    }

    #[test]
    fn sort_mixed_numbers_and_text_is_total() {
        // Pairwise comparison of raw values (2 < 10 as numbers, but 10 < "15" < 2
        // as text) was cyclic, and sort_by panics on such orders.
        let fms: Vec<Map<String, Value>> = mixed_values()
            .into_iter()
            .map(|v| {
                let mut m = Map::new();
                if !v.is_null() {
                    m.insert("v".into(), v);
                }
                m
            })
            .collect();
        let paths: Vec<String> = (0..fms.len()).map(|i| format!("n{i}.md")).collect();
        let rows: Vec<Row> = fms
            .iter()
            .zip(&paths)
            .map(|(f, p)| row(p, &[], f))
            .collect();
        for desc in [false, true] {
            let sort = if desc { "-v" } else { "v" };
            let spec = parse_spec(&format!("columns: [v]\nsort: {sort}\nlimit: 1000")).unwrap();
            let r = run(&spec, &rows).unwrap();
            assert_eq!(r.rows.len(), 600);
            // (class, number, text): 0 = number or numeric text, 1 = text, 2 = empty
            let keys: Vec<(u8, f64, String)> = r
                .rows
                .iter()
                .map(|x| match &x.values[0] {
                    Value::Null => (2, 0.0, String::new()),
                    Value::String(s) if s.is_empty() => (2, 0.0, String::new()),
                    Value::Number(n) => (0, n.as_f64().unwrap(), String::new()),
                    Value::String(s) => match s.parse::<f64>() {
                        Ok(n) if n.is_finite() => (0, n, String::new()),
                        _ => (1, 0.0, s.to_lowercase()),
                    },
                    other => panic!("unexpected {other}"),
                })
                .collect();
            let wrong = if desc {
                Ordering::Less
            } else {
                Ordering::Greater
            };
            for w in keys.windows(2) {
                let (a, b) = (&w[0], &w[1]);
                if a.0 == 2 || b.0 == 2 {
                    // empty values last, in both directions
                    assert!(b.0 == 2, "{a:?} before {b:?}");
                    continue;
                }
                let o = a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)).then(a.2.cmp(&b.2));
                assert_ne!(o, wrong, "{a:?} before {b:?} (desc: {desc})");
            }
        }
    }

    #[test]
    fn where_semantics() {
        let none: Vec<String> = vec![];
        let some: Vec<String> = vec!["a".into(), "iş".into()];
        let f1 = json!({"şehir": "İstanbul", "durum": "bırakıldı", "x": "NaN", "puan": 10});
        let f2 = json!({"şehir": "Izmir", "durum": "BİTTİ", "x": "Inf", "puan": "9"});
        let (f1, f2) = (f1.as_object().unwrap(), f2.as_object().unwrap());
        let rows = vec![row("A.md", &none, f1), row("B.md", &some, f2)];
        let paths = |w: &str| -> Vec<String> {
            let spec = Spec {
                filters: vec![w.into()],
                ..Default::default()
            };
            run(&spec, &rows)
                .unwrap()
                .rows
                .into_iter()
                .map(|r| r.path)
                .collect()
        };
        // An empty list is not "has".
        assert_eq!(paths("has tags"), ["B.md"]);
        assert_eq!(paths("missing tags"), ["A.md"]);
        // On lists `=` means contains and `!=` does not contain: never both.
        assert_eq!(paths("tags = a"), ["B.md"]);
        assert_eq!(paths("tags != a"), ["A.md"]);
        for v in ["a", "b", "İŞ"] {
            let mut all = paths(&format!("tags = {v}"));
            all.extend(paths(&format!("tags != {v}")));
            all.sort();
            assert_eq!(all, ["A.md", "B.md"], "{v}");
        }
        // Unicode / Turkish case folding, for values and field names.
        assert_eq!(paths("şehir = istanbul"), ["A.md"]);
        assert_eq!(paths("ŞEHİR = İZMİR"), ["B.md"]);
        assert_eq!(paths("durum = BIRAKILDI"), ["A.md"]);
        assert_eq!(paths("durum = bitti"), ["B.md"]);
        assert_eq!(paths("tags contains İş"), ["B.md"]);
        // "NaN" / "Inf" are text, not floats.
        assert_eq!(paths("x = nan"), ["A.md"]);
        assert_eq!(paths("x != nan"), ["B.md"]);
        assert_eq!(paths("x = INF"), ["B.md"]);
        assert!(parse_num("NaN").is_none() && parse_num("-inf").is_none());
        assert!(parse_num("Infinity").is_none() && parse_num("1e400").is_none());
        // Numeric text still compares as a number.
        assert_eq!(paths("puan > 9.5"), ["A.md"]);
        assert_eq!(paths("puan >= 9"), ["A.md", "B.md"]);
    }

    #[test]
    fn parse_cond_splits_at_first_operator() {
        for (s, field, op, value) in [
            (
                "url = https://x.org/?a=b&c>=d",
                "url",
                Op::Eq,
                "https://x.org/?a=b&c>=d",
            ),
            ("başlık = a contains b", "başlık", Op::Eq, "a contains b"),
            ("not != a=b", "not", Op::Ne, "a=b"),
            ("puan>=4", "puan", Op::Ge, "4"),
            ("tags contains c=d", "tags", Op::Contains, "c=d"),
            ("x == 'a<b'", "x", Op::Eq, "a<b"),
            ("x < 5", "x", Op::Lt, "5"),
        ] {
            let c = parse_cond(s).unwrap();
            assert_eq!(
                (c.field.as_str(), c.op, c.value.as_str()),
                (field, op, value),
                "{s}"
            );
        }
        assert!(parse_cond("= x").is_err());
    }
}
