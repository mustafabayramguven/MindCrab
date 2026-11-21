//! Semantic search: chunking, an OpenAI-compatible embeddings client and a
//! background worker that keeps chunk vectors up to date.
//!
//! Works with anything that speaks `POST /embeddings` the OpenAI way:
//! Ollama (`http://ollama:11434/v1`, fully local), Gemini's OpenAI endpoint,
//! OpenAI, Jina, Voyage, LM Studio, llama.cpp server…

use crate::app::App;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct EmbedConfig {
    /// Full URL of the embeddings endpoint.
    pub url: String,
    pub key: Option<String>,
    pub model: String,
    pub query_prefix: String,
    pub doc_prefix: String,
    pub batch: usize,
    pub dims: Option<usize>,
    /// Absolute similarity floor for semantic hits (model-dependent, default 0).
    pub min_score: f32,
}

impl EmbedConfig {
    pub fn new(base: &str, key: Option<String>, model: &str) -> Self {
        let base = base.trim().trim_end_matches('/');
        let url = if base.ends_with("/embeddings") {
            base.to_string()
        } else {
            format!("{base}/embeddings")
        };
        Self {
            url,
            key: key.filter(|k| !k.trim().is_empty()),
            model: model.trim().to_string(),
            query_prefix: String::new(),
            doc_prefix: String::new(),
            batch: 32,
            dims: None,
            min_score: 0.0,
        }
    }

    /// Stable identity of the vector space: changing any of these invalidates stored vectors.
    pub fn fingerprint(&self) -> String {
        format!(
            "{}|{}|{}|{:?}",
            self.model, self.doc_prefix, self.url, self.dims
        )
    }
}

pub struct Embedder {
    pub cfg: EmbedConfig,
    agent: ureq::Agent,
}

impl Embedder {
    pub fn new(cfg: EmbedConfig) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(120)))
            .http_status_as_error(false)
            .build()
            .into();
        Self { cfg, agent }
    }

    pub fn embed_docs(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let inputs: Vec<String> = texts
            .iter()
            .map(|t| format!("{}{t}", self.cfg.doc_prefix))
            .collect();
        self.embed(&inputs)
    }

    pub fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = self.embed(&[format!("{}{text}", self.cfg.query_prefix)])?;
        v.pop().context("embedding API returned no vector")
    }

    fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut body = json!({"model": self.cfg.model, "input": inputs});
        if let Some(d) = self.cfg.dims {
            body["dimensions"] = json!(d);
        }
        let mut req = self
            .agent
            .post(&self.cfg.url)
            .header("content-type", "application/json");
        if let Some(k) = &self.cfg.key {
            req = req.header("authorization", &format!("Bearer {k}"));
        }
        let mut resp = req
            .send_json(&body)
            .with_context(|| format!("POST {}", self.cfg.url))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            bail!(
                "embedding API returned {status}: {}",
                text.chars().take(300).collect::<String>()
            );
        }
        let v: Value = resp
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_json()?;
        let data = v
            .get("data")
            .and_then(Value::as_array)
            .context("embedding response has no 'data' array")?;
        let mut out: Vec<(usize, Vec<f32>)> = Vec::with_capacity(data.len());
        for (i, item) in data.iter().enumerate() {
            let idx = item
                .get("index")
                .and_then(Value::as_u64)
                .map(|x| x as usize)
                .unwrap_or(i);
            let emb = item
                .get("embedding")
                .and_then(Value::as_array)
                .context("embedding item without 'embedding'")?
                .iter()
                .map(|x| x.as_f64().unwrap_or(0.0) as f32)
                .collect::<Vec<f32>>();
            out.push((idx, normalize(emb)));
        }
        out.sort_by_key(|(i, _)| *i);
        if out.len() != inputs.len() {
            bail!(
                "embedding API returned {} vectors for {} inputs",
                out.len(),
                inputs.len()
            );
        }
        Ok(out.into_iter().map(|(_, v)| v).collect())
    }
}

pub fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

pub fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

pub fn from_blob(b: &[u8]) -> Vec<f32> {
    let (chunks, _) = b.as_chunks::<4>();
    chunks.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

const CHUNK_TARGET: usize = 900;
const CHUNK_MAX: usize = 1600;

/// Splits a note into overlapping-context chunks. Each chunk starts with the
/// note title and the heading it lives under, so it embeds well on its own.
pub fn chunk(title: &str, body: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut heading = String::new();
    let mut buf = String::new();
    let mut in_fence = false;
    let flush = |buf: &mut String, heading: &str, chunks: &mut Vec<String>| {
        let text = buf.trim();
        if !text.is_empty() {
            let ctx = if heading.is_empty() || heading == title {
                title.to_string()
            } else {
                format!("{title} › {heading}")
            };
            chunks.push(format!("{ctx}\n{text}"));
        }
        buf.clear();
    };
    for para in body.split("\n\n") {
        let p = para.trim_matches('\n');
        if p.trim().is_empty() {
            continue;
        }
        let fences = p
            .lines()
            .filter(|l| l.trim_start().starts_with("```") || l.trim_start().starts_with("~~~"))
            .count();
        let first = p.lines().next().unwrap_or("").trim_start();
        let is_heading =
            !in_fence && first.starts_with('#') && first.trim_start_matches('#').starts_with(' ');
        if fences % 2 == 1 {
            in_fence = !in_fence;
        }
        if is_heading {
            flush(&mut buf, &heading, &mut chunks);
            heading = first.trim_start_matches('#').trim().to_string();
            let rest: String = p.lines().skip(1).collect::<Vec<_>>().join("\n");
            if !rest.trim().is_empty() {
                buf.push_str(&rest);
                buf.push_str("\n\n");
            }
            continue;
        }
        if buf.len() + p.len() > CHUNK_TARGET && !buf.is_empty() && !in_fence {
            flush(&mut buf, &heading, &mut chunks);
        }
        if p.len() > CHUNK_MAX {
            // Hard-split very long paragraphs on char boundaries.
            let mut start = 0;
            let chars: Vec<char> = p.chars().collect();
            while start < chars.len() {
                let end = (start + CHUNK_TARGET).min(chars.len());
                buf.push_str(&chars[start..end].iter().collect::<String>());
                flush(&mut buf, &heading, &mut chunks);
                start = end;
            }
            continue;
        }
        buf.push_str(p);
        buf.push_str("\n\n");
    }
    flush(&mut buf, &heading, &mut chunks);
    if chunks.is_empty() {
        chunks.push(title.to_string());
    }
    chunks
}

/// Handle used by the app to wake the worker when notes change.
#[derive(Clone)]
pub struct Waker(Option<Sender<()>>);

impl Waker {
    pub fn none() -> Self {
        Self(None)
    }
    pub fn wake(&self) {
        if let Some(tx) = &self.0 {
            let _ = tx.send(());
        }
    }
}

/// Starts the background embedding worker; returns a waker.
pub fn spawn_worker(app: Arc<App>) -> Waker {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("embedder".into())
        .spawn(move || worker(app, rx))
        .expect("spawn embedder");
    Waker(Some(tx))
}

fn worker(app: Arc<App>, rx: Receiver<()>) {
    let Some(emb) = app.embedder.as_ref() else {
        return;
    };
    let fp = emb.cfg.fingerprint();
    match app.idx().ensure_vector_space(&fp) {
        Ok(true) => tracing::info!(
            "embedding model changed — all notes will be re-embedded with {}",
            emb.cfg.model
        ),
        Ok(false) => {}
        Err(e) => tracing::warn!("embedding setup failed: {e:#}"),
    }
    match app.idx().rechunk_missing() {
        Ok(0) => {}
        Ok(n) => tracing::info!("prepared {n} notes for semantic search"),
        Err(e) => tracing::warn!("chunking failed: {e:#}"),
    }
    let mut backoff = Duration::from_secs(5);
    loop {
        let pending = match app.idx().pending_chunks(emb.cfg.batch) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("embedder: {e:#}");
                Vec::new()
            }
        };
        if pending.is_empty() {
            app.set_embed_error(None);
            // Sleep until something changes (or a periodic check).
            match rx.recv_timeout(Duration::from_secs(60)) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {
                    std::thread::sleep(Duration::from_millis(800)); // let bursts of edits settle
                    while rx.try_recv().is_ok() {}
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        let texts: Vec<String> = pending.iter().map(|p| p.3.clone()).collect();
        match emb.embed_docs(&texts) {
            Ok(vecs) => {
                backoff = Duration::from_secs(5);
                app.set_embed_error(None);
                let rows: Vec<_> = pending
                    .iter()
                    .zip(vecs)
                    .map(|(p, v)| (p.0.clone(), p.1, p.2.clone(), v))
                    .collect();
                if let Err(e) = app.idx().store_vectors(&rows) {
                    tracing::warn!("embedder: storing vectors failed: {e:#}");
                }
                std::thread::sleep(Duration::from_millis(150));
            }
            Err(e) => {
                tracing::warn!("embedder: {e:#} — retrying in {}s", backoff.as_secs());
                app.set_embed_error(Some(format!("{e:#}")));
                let _ = rx.recv_timeout(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(600));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunking_keeps_headings_as_context() {
        let body = "intro text\n\n## Sunucu\n\nOracle 1 GB\n\n```\ncode\n\nmore code\n```\n\n## Diğer\n\nson";
        let c = chunk("Not", body);
        assert_eq!(c[0], "Not\nintro text");
        assert!(c[1].starts_with("Not › Sunucu\nOracle 1 GB"), "{c:?}");
        assert!(c[1].contains("more code"), "{c:?}");
        assert_eq!(c.last().unwrap(), "Not › Diğer\nson");
        assert_eq!(chunk("Boş", ""), vec!["Boş"]);
        let long = "ş".repeat(5000);
        assert!(chunk("L", &long).len() >= 5);
    }

    #[test]
    fn vectors_roundtrip() {
        let v = normalize(vec![3.0, 4.0]);
        assert!((v[0] - 0.6).abs() < 1e-6);
        assert_eq!(from_blob(&to_blob(&v)), v);
        assert!((dot(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn url_building() {
        assert_eq!(
            EmbedConfig::new("http://x:11434/v1/", None, "m").url,
            "http://x:11434/v1/embeddings"
        );
        assert_eq!(
            EmbedConfig::new("https://a/b/embeddings", Some("".into()), "m").key,
            None
        );
    }
}
