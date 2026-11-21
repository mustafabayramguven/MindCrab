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
                    self.fm
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(f))
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
    for (tok, op) in [
        (">=", Op::Ge),
        ("<=", Op::Le),
        ("!=", Op::Ne),
        ("==", Op::Eq),
        (" contains ", Op::Contains),
        ("=", Op::Eq),
        (">", Op::Gt),
        ("<", Op::Lt),
    ] {
        if let Some((f, v)) = s.split_once(tok) {
            let v = v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
            return Ok(Cond {
                field: f.trim().into(),
                op,
                value: v,
            });
        }
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

fn cmp_values(a: &Value, b: &str) -> Option<Ordering> {
    if let (Some(x), Ok(y)) = (
        a.as_f64().or_else(|| as_text(a).parse().ok()),
        b.parse::<f64>(),
    ) {
        return x.partial_cmp(&y);
    }
    Some(as_text(a).to_lowercase().cmp(&b.to_lowercase()))
}

fn check(c: &Cond, row: &Row) -> bool {
    let v = row.get(&c.field);
    let present = !matches!(v, Value::Null) && !(v.is_string() && v.as_str() == Some(""));
    match c.op {
        Op::Has => present,
        Op::Missing => !present,
        Op::Contains => match &v {
            Value::Array(a) => a.iter().any(|x| as_text(x).eq_ignore_ascii_case(&c.value)),
            other => as_text(other)
                .to_lowercase()
                .contains(&c.value.to_lowercase()),
        },
        Op::Eq => match &v {
            Value::Array(a) => a.iter().any(|x| as_text(x).eq_ignore_ascii_case(&c.value)),
            Value::Bool(b) => b.to_string() == c.value.to_lowercase(),
            other => cmp_values(other, &c.value) == Some(Ordering::Equal),
        },
        Op::Ne => present && cmp_values(&v, &c.value) != Some(Ordering::Equal) || !present,
        Op::Gt => present && cmp_values(&v, &c.value) == Some(Ordering::Greater),
        Op::Ge => {
            present
                && matches!(
                    cmp_values(&v, &c.value),
                    Some(Ordering::Greater | Ordering::Equal)
                )
        }
        Op::Lt => present && cmp_values(&v, &c.value) == Some(Ordering::Less),
        Op::Le => {
            present
                && matches!(
                    cmp_values(&v, &c.value),
                    Some(Ordering::Less | Ordering::Equal)
                )
        }
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
        hit.sort_by(|a, b| {
            let (x, y) = (a.get(field), b.get(field));
            let o = match (x.as_f64(), y.as_f64()) {
                (Some(p), Some(q)) => p.partial_cmp(&q).unwrap_or(Ordering::Equal),
                _ => match (x.is_null(), y.is_null()) {
                    (true, false) => Ordering::Greater, // empty values last
                    (false, true) => Ordering::Less,
                    _ => as_text(&x).to_lowercase().cmp(&as_text(&y).to_lowercase()),
                },
            };
            if desc && !(x.is_null() ^ y.is_null()) {
                o.reverse()
            } else {
                o
            }
        });
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
}
