//! Semantic search: chunking, an OpenAI-compatible embeddings client and a
//! background worker that keeps chunk vectors up to date.
//!
//! Works with anything that speaks `POST /embeddings` the OpenAI way:
//! Ollama (`http://ollama:11434/v1`, fully local), Gemini's OpenAI endpoint,
//! OpenAI, Jina, Voyage, LM Studio, llama.cpp server…

use crate::app::App;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

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

/// Whole indexing batches may take a while on slow local models.
const BATCH_TIMEOUT: Duration = Duration::from_secs(120);
/// Searches wait on the query embedding, so give up quickly and fall back to keywords.
const QUERY_TIMEOUT: Duration = Duration::from_secs(15);

/// The API refused this particular input (HTTP 400/413/422, or an unusable vector in
/// the answer): sending the same text again won't help.
#[derive(Debug)]
struct Rejected(String);

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Rejected {}

fn is_rejection(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Rejected>().is_some()
}

pub struct Embedder {
    pub cfg: EmbedConfig,
    agent: ureq::Agent,
    /// `scheme://host` of the endpoint: what error messages show instead of the full URL.
    endpoint: String,
}

impl Embedder {
    pub fn new(cfg: EmbedConfig) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(BATCH_TIMEOUT))
            .http_status_as_error(false)
            // Never send note text (or the key) on to wherever a redirect points.
            .max_redirects(0)
            .build()
            .into();
        let endpoint = redact_url(&cfg.url);
        Self {
            cfg,
            agent,
            endpoint,
        }
    }

    pub fn embed_docs(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let inputs: Vec<String> = texts
            .iter()
            .map(|t| format!("{}{t}", self.cfg.doc_prefix))
            .collect();
        self.embed(&inputs, BATCH_TIMEOUT)
    }

    pub fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = self.embed(&[format!("{}{text}", self.cfg.query_prefix)], QUERY_TIMEOUT)?;
        v.pop().context("embedding API returned no vector")
    }

    fn embed(&self, inputs: &[String], timeout: Duration) -> Result<Vec<Vec<f32>>> {
        let mut body = json!({"model": self.cfg.model, "input": inputs});
        if let Some(d) = self.cfg.dims {
            body["dimensions"] = json!(d);
        }
        let mut req = self
            .agent
            .post(&self.cfg.url)
            .config()
            .timeout_global(Some(timeout))
            .build()
            .header("content-type", "application/json");
        if let Some(k) = &self.cfg.key {
            req = req.header("authorization", &format!("Bearer {k}"));
        }
        let mut resp = req
            .send_json(&body)
            .map_err(|e| anyhow!("POST {}: {}", self.endpoint, self.describe(&e)))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            // Scrub before cutting, so a key straddling the cut can't survive half-masked.
            let msg = format!(
                "embedding API returned {status}: {}",
                self.scrub(&text).chars().take(300).collect::<String>()
            );
            if matches!(status.as_u16(), 400 | 413 | 422) {
                return Err(Rejected(msg).into());
            }
            bail!(msg);
        }
        let v: Value = match resp
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_json()
        {
            Ok(v) => v,
            // Invalid JSON (e.g. a bare NaN) can come from one bad input; the worker isolates it.
            Err(ureq::Error::Json(e)) => {
                return Err(Rejected(format!("unreadable embedding response: {e}")).into());
            }
            Err(e) => bail!("reading embedding response: {}", self.describe(&e)),
        };
        parse_response(&v, inputs.len())
    }

    fn describe(&self, e: &ureq::Error) -> String {
        match e {
            ureq::Error::BadUri(_) => "invalid URL".to_string(),
            e => self.scrub(&e.to_string()),
        }
    }

    /// Masks the full URL and the API key: error texts reach the UI, MCP clients and logs.
    fn scrub(&self, s: &str) -> String {
        let mut s = s.replace(&self.cfg.url, &self.endpoint);
        if let Some(k) = self.cfg.key.as_deref().filter(|k| k.len() >= 8) {
            s = s.replace(k, "***");
        }
        s
    }
}

/// `scheme://host[:port]` of `url`, without credentials, path or query string.
fn redact_url(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    if scheme.is_empty() {
        host.to_string()
    } else {
        format!("{scheme}://{host}")
    }
}

/// Reads an OpenAI-style embeddings response for `n` inputs into unit vectors, in input order.
fn parse_response(v: &Value, n: usize) -> Result<Vec<Vec<f32>>> {
    let data = v
        .get("data")
        .and_then(Value::as_array)
        .context("embedding response has no 'data' array")?;
    if data.len() != n {
        bail!(
            "embedding API returned {} vectors for {n} inputs",
            data.len()
        );
    }
    let mut out: Vec<Option<Vec<f32>>> = vec![None; n];
    for (pos, item) in data.iter().enumerate() {
        let i = match item.get("index") {
            None | Some(Value::Null) => pos,
            Some(x) => x
                .as_u64()
                .and_then(|i| usize::try_from(i).ok())
                .filter(|&i| i < n)
                .with_context(|| format!("embedding response has an invalid index {x}"))?,
        };
        if out[i].is_some() {
            bail!("embedding response repeats index {i}");
        }
        let raw = item
            .get("embedding")
            .and_then(Value::as_array)
            .context("embedding item without 'embedding'")?;
        let vec = unit_vector(raw).map_err(|why| Rejected(format!("input {i}: {why}")))?;
        out[i] = Some(vec);
    }
    // n distinct indexes below n: every slot is filled.
    let out: Vec<Vec<f32>> = out
        .into_iter()
        .collect::<Option<_>>()
        .context("embedding response misses an input")?;
    if let Some(d) = out.first().map(Vec::len)
        && out.iter().any(|v| v.len() != d)
    {
        bail!("embedding API returned vectors of different sizes");
    }
    Ok(out)
}

/// A unit-length copy of a raw JSON vector; refuses values no similarity search can use.
fn unit_vector(raw: &[Value]) -> std::result::Result<Vec<f32>, &'static str> {
    if raw.is_empty() {
        return Err("empty vector");
    }
    let mut v = Vec::with_capacity(raw.len());
    for x in raw {
        let f = x.as_f64().ok_or("non-numeric value in vector")? as f32;
        if !f.is_finite() {
            return Err("non-finite value in vector");
        }
        v.push(f);
    }
    // Norm in f64: squaring large f32 values could overflow.
    let norm = v
        .iter()
        .map(|&x| f64::from(x) * f64::from(x))
        .sum::<f64>()
        .sqrt();
    if !(norm.is_finite() && norm > 0.0) {
        return Err("zero vector");
    }
    Ok(v.into_iter()
        .map(|x| (f64::from(x) / norm) as f32)
        .collect())
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

/// Blocks are packed into one chunk until the next would cross this size (bytes).
const CHUNK_TARGET: usize = 900;
/// Hard limit for a whole chunk, context line included (bytes).
const CHUNK_MAX: usize = 1600;
/// Longest title or heading kept in a chunk's context line (bytes).
const CTX_PART_MAX: usize = 200;
// The context line ("title › heading\n") must leave room for a full piece of text.
const _: () = assert!(CHUNK_TARGET + 2 * CTX_PART_MAX + " › \n".len() <= CHUNK_MAX);

/// Bump whenever `chunk` output changes: stored chunks are then rebuilt once
/// (chunks whose text stays the same keep their vectors).
pub const CHUNKER_VERSION: &str = "2";

/// Splits a note into chunks of at most `CHUNK_MAX` bytes. Each chunk starts with the
/// note title and the heading it lives under, so it embeds well on its own.
pub fn chunk(title: &str, body: &str) -> Vec<String> {
    // CRLF (or lone CR) line endings would hide the blank lines between paragraphs.
    let body = body.replace("\r\n", "\n").replace('\r', "\n");
    let mut c = Chunker {
        title,
        ctx: clip(title, CTX_PART_MAX).to_string(),
        buf: String::new(),
        chunks: Vec::new(),
    };
    // Current block: a paragraph, plus any code fence it contains (blank lines and
    // `#` lines inside a fence are code, not separators or headings).
    let mut block = String::new();
    let mut fence: Option<(u8, usize)> = None;
    for line in body.split('\n') {
        if let Some((ch, n)) = fence {
            block.push('\n');
            block.push_str(line);
            if closes_fence(line, ch, n) {
                fence = None;
            }
            continue;
        }
        if line.trim().is_empty() {
            c.add(&block);
            block.clear();
            continue;
        }
        if block.is_empty()
            && let Some(h) = heading(line)
        {
            c.set_heading(h);
            continue;
        }
        if !block.is_empty() {
            block.push('\n');
        }
        block.push_str(line);
        fence = opens_fence(line);
    }
    // An unclosed fence runs to the end of the note; `add` still cuts it to size.
    c.add(&block);
    c.finish()
}

/// Heading text of an ATX heading line (`## Text`).
fn heading(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let rest = t.trim_start_matches('#');
    if t.starts_with('#') && rest.starts_with(' ') {
        Some(rest.trim())
    } else {
        None
    }
}

/// Opening code fence: (fence char, fence length).
fn opens_fence(line: &str) -> Option<(u8, usize)> {
    let t = line.trim_start();
    let ch = *t.as_bytes().first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let n = t.bytes().take_while(|&b| b == ch).count();
    // A backtick fence's info string can't hold backticks (that is inline code).
    (n >= 3 && !(ch == b'`' && t[n..].contains('`'))).then_some((ch, n))
}

/// Whether `line` closes a fence of `n` × `ch`: same char, at least as long, nothing else.
fn closes_fence(line: &str, ch: u8, n: usize) -> bool {
    let t = line.trim();
    t.len() >= n && t.bytes().all(|b| b == ch)
}

/// Longest prefix of `s` of at most `max` bytes that doesn't split a character.
fn clip(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

struct Chunker<'a> {
    title: &'a str,
    /// First line of every chunk: the title and the current heading.
    ctx: String,
    buf: String,
    chunks: Vec<String>,
}

impl Chunker<'_> {
    /// Bytes left for text once the context line is in.
    fn budget(&self) -> usize {
        CHUNK_MAX - self.ctx.len() - 1
    }

    fn set_heading(&mut self, h: &str) {
        self.flush();
        let title = clip(self.title, CTX_PART_MAX);
        self.ctx = if h.is_empty() || h == self.title {
            title.to_string()
        } else {
            format!("{title} › {}", clip(h, CTX_PART_MAX))
        };
    }

    fn add(&mut self, block: &str) {
        if block.trim().is_empty() {
            return;
        }
        if !self.buf.is_empty() && self.buf.len() + block.len() > CHUNK_TARGET {
            self.flush();
        }
        if block.len() > self.budget() {
            // Too long for one chunk (huge paragraph or code block): cut it up.
            self.flush();
            for piece in split_long(block, CHUNK_TARGET) {
                self.buf.push_str(piece);
                self.flush();
            }
            return;
        }
        self.buf.push_str(block);
        self.buf.push_str("\n\n");
    }

    fn flush(&mut self) {
        let text = self.buf.trim();
        if !text.is_empty() {
            self.chunks.push(format!("{}\n{text}", self.ctx));
        }
        self.buf.clear();
    }

    fn finish(mut self) -> Vec<String> {
        self.flush();
        if self.chunks.is_empty() {
            self.chunks.push(clip(self.title, CHUNK_MAX).to_string());
        }
        self.chunks
    }
}

/// Cuts `text` into pieces of at most `max` bytes, at a line break, sentence end or
/// space when there is one near the end of the window, never inside a character.
fn split_long(text: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    while rest.len() > max {
        let mut cut = break_point(clip(rest, max));
        if cut == 0 {
            // `max` is smaller than the first character: take that character whole.
            cut = rest.chars().next().map_or(rest.len(), char::len_utf8);
        }
        let (piece, tail) = rest.split_at(cut);
        out.push(piece);
        rest = tail.trim_start();
    }
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

/// Where to end a piece cut from window `w`: after its last line break, else after its
/// last sentence end, else at its last space (only in the second half, so pieces stay
/// long), else at the end of `w`.
fn break_point(w: &str) -> usize {
    let half = w.len() / 2;
    let (mut line, mut sentence, mut space) = (None, None, None);
    let mut prev = '\0';
    for (i, c) in w.char_indices() {
        if i >= half {
            if c == '\n' {
                line = Some(i + 1);
            } else if c.is_whitespace() {
                if matches!(prev, '.' | '!' | '?') {
                    sentence = Some(i);
                }
                space = Some(i);
            } else if matches!(c, '。' | '！' | '？') {
                sentence = Some(i + c.len_utf8());
            }
        }
        prev = c;
    }
    line.or(sentence).or(space).unwrap_or(w.len())
}

/// One input's outcome: its vector, or why the API refuses it for good.
type Outcome = std::result::Result<Vec<f32>, String>;

/// Harmless text used to tell "the API refuses this input" from "the API refuses everything".
const PROBE: &str = "test";

/// Embeds `texts`, isolating inputs the API rejects on their own (too long for the model,
/// unusable vector…) so one bad chunk can't block the rest of the vault: a rejected batch
/// is bisected until each rejected input stands alone. Before blaming the input, a probe
/// checks the API accepts anything at all; if not (bad key, wrong model), that's an error
/// to retry later, not a reason to skip every chunk.
fn embed_isolating<F>(texts: &[String], embed: &mut F) -> Result<Vec<Outcome>>
where
    F: FnMut(&[String]) -> Result<Vec<Vec<f32>>>,
{
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let err = match embed(texts) {
        Ok(v) => return Ok(checked(v, texts.len())?.into_iter().map(Ok).collect()),
        Err(e) if is_rejection(&e) => e,
        Err(e) => return Err(e),
    };
    if embed(&[PROBE.to_string()]).is_err() {
        return Err(err);
    }
    if texts.len() == 1 {
        return Ok(vec![Err(format!("{err:#}"))]);
    }
    let mut out = Vec::with_capacity(texts.len());
    bisect(texts, embed, &mut out)?;
    Ok(out)
}

/// `texts` (two or more) was rejected as a batch: embeds its halves separately, recursively.
fn bisect<F>(texts: &[String], embed: &mut F, out: &mut Vec<Outcome>) -> Result<()>
where
    F: FnMut(&[String]) -> Result<Vec<Vec<f32>>>,
{
    let mid = texts.len() / 2;
    for half in [&texts[..mid], &texts[mid..]] {
        match embed(half) {
            Ok(v) => out.extend(checked(v, half.len())?.into_iter().map(Ok)),
            Err(e) if is_rejection(&e) && half.len() == 1 => out.push(Err(format!("{e:#}"))),
            Err(e) if is_rejection(&e) => bisect(half, embed, out)?,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn checked(v: Vec<Vec<f32>>, n: usize) -> Result<Vec<Vec<f32>>> {
    if v.len() != n {
        bail!("embedding API returned {} vectors for {n} inputs", v.len());
    }
    Ok(v)
}

/// Waits out `d` even if notes change meanwhile (wake-ups are swallowed), so a failing
/// API isn't hit again on every save. Returns false once the app is gone.
fn sleep_through(rx: &Receiver<()>, d: Duration) -> bool {
    let until = Instant::now() + d;
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        match rx.recv_timeout(left) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => return true,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
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

const BACKOFF_MIN: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(600);

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
    // Only a success resets the backoff; edits during an outage don't shorten it.
    let mut backoff = BACKOFF_MIN;
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
        let stored =
            embed_isolating(&texts, &mut |t: &[String]| emb.embed_docs(t)).and_then(|vecs| {
                let rows: Vec<_> = pending
                    .iter()
                    .zip(vecs)
                    .map(|(p, v)| {
                        // An empty vector marks the chunk as skipped until its text changes.
                        let v = v.unwrap_or_else(|why| {
                            tracing::warn!("embedder: skipping chunk {} of {}: {why}", p.1, p.0);
                            Vec::new()
                        });
                        (p.0.clone(), p.1, p.2.clone(), v)
                    })
                    .collect();
                app.idx()
                    .store_vectors(&rows)
                    .context("storing vectors failed")
            });
        match stored {
            Ok(()) => {
                backoff = BACKOFF_MIN;
                app.set_embed_error(None);
                std::thread::sleep(Duration::from_millis(150));
            }
            Err(e) => {
                tracing::warn!("embedder: {e:#} — retrying in {}s", backoff.as_secs());
                app.set_embed_error(Some(format!("{e:#}")));
                if !sleep_through(&rx, backoff) {
                    return;
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
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

    /// Text of a chunk without its context line.
    fn text(c: &str) -> &str {
        c.split_once('\n').map_or("", |(_, t)| t)
    }

    #[test]
    fn crlf_paragraphs_split_like_lf() {
        let lf = "intro\n\n## Sunucu\n\nOracle\n\n```\na\n\nb\n```\n\nson\n";
        let crlf = lf.replace('\n', "\r\n");
        assert_eq!(chunk("Not", &crlf), chunk("Not", lf));
        assert!(chunk("Not", &crlf).iter().all(|c| !c.contains('\r')));
        // Many CRLF paragraphs must not end up in one oversized chunk.
        let many = "paragraph text here.\r\n\r\n".repeat(500);
        let c = chunk("Not", &many);
        assert!(c.len() > 5, "{}", c.len());
        assert!(c.iter().all(|c| c.len() <= CHUNK_MAX));
    }

    #[test]
    fn long_text_under_a_heading_respects_the_max() {
        let body = format!("## Başlık\n{}", "kelime ".repeat(100_000));
        let c = chunk("Not", &body);
        assert!(c.len() > 100);
        assert!(c.iter().all(|c| c.len() <= CHUNK_MAX), "chunk too big");
        assert!(c.iter().all(|c| c.starts_with("Not › Başlık\n")));
        // Cut at spaces: no word is lost or split.
        let words: usize = c.iter().map(|c| text(c).split_whitespace().count()).sum();
        assert_eq!(words, 100_000);
        assert!(
            c.iter()
                .all(|c| text(c).split_whitespace().all(|w| w == "kelime"))
        );
        // Huge headings and titles are clipped in the context line too.
        let c = chunk(
            &"T".repeat(5000),
            &format!("# {}\n{}", "H".repeat(5000), "x ".repeat(5000)),
        );
        assert!(c.iter().all(|c| c.len() <= CHUNK_MAX));
    }

    #[test]
    fn multibyte_text_is_measured_in_bytes() {
        for ch in ["ş", "語", "😀"] {
            let body = ch.repeat(5000);
            let c = chunk("L", &body);
            assert!(c.iter().all(|c| c.len() <= CHUNK_MAX));
            assert!(c.iter().all(|c| text(c).len() <= CHUNK_TARGET));
            let joined: String = c.iter().map(|c| text(c)).collect();
            assert_eq!(joined, body, "nothing lost, no character split");
        }
        // Sentences are kept whole when possible.
        let body = "Bu bir cümle. ".repeat(400);
        for c in chunk("L", &body) {
            assert!(text(&c).ends_with('.'), "{c}");
        }
    }

    #[test]
    fn fences_need_the_same_char_and_length() {
        // `~~~` does not close a ``` fence: the `#` line stays code, not a heading.
        let c = chunk("N", "```\ncode\n~~~\n\n# not a heading\n```\n\nafter");
        assert!(c.iter().all(|c| c.starts_with("N\n")), "{c:?}");
        assert!(c[0].contains("# not a heading"), "{c:?}");
        // A shorter run does not close a longer fence; a longer one does.
        let c = chunk("N", "````\n```\n\n## inside\n`````\n\n## After\n\ntext");
        assert!(c[0].contains("## inside"), "{c:?}");
        assert_eq!(c.last().unwrap(), "N › After\ntext");
        // Inline triple backticks don't open a fence.
        let c = chunk("N", "```x``` y\n\n## H\n\nz");
        assert_eq!(c.last().unwrap(), "N › H\nz");
    }

    #[test]
    fn huge_or_unclosed_fences_are_still_cut() {
        let code = "let x = 1;\n\n".repeat(2000);
        for body in [format!("```\n{code}"), format!("```rust\n{code}```\n\nend")] {
            let c = chunk("N", &body);
            assert!(c.len() > 10, "{}", c.len());
            assert!(c.iter().all(|c| c.len() <= CHUNK_MAX));
        }
    }

    #[test]
    fn response_validation() {
        let ok = json!({"data": [
            {"index": 1, "embedding": [0.0, 2.0]},
            {"index": 0, "embedding": [3.0, 4.0]},
        ]});
        let v = parse_response(&ok, 2).unwrap();
        assert!((v[0][0] - 0.6).abs() < 1e-6 && (v[1][1] - 1.0).abs() < 1e-6);
        let no_index = json!({"data": [{"embedding": [1.0]}, {"embedding": [2.0]}]});
        assert_eq!(parse_response(&no_index, 2).unwrap().len(), 2);

        // Broken answers as a whole: an error to retry, not a verdict on the input.
        for bad in [
            json!({"data": [{"index": 0, "embedding": [1.0]}, {"index": 0, "embedding": [1.0]}]}),
            json!({"data": [{"index": 0, "embedding": [1.0]}, {"index": 2, "embedding": [1.0]}]}),
            json!({"data": [{"index": -1, "embedding": [1.0]}, {"index": 1, "embedding": [1.0]}]}),
            json!({"data": [{"embedding": [1.0]}, {"embedding": [1.0, 2.0]}]}),
            json!({"data": [{"embedding": [1.0]}]}),
            json!({"error": "x"}),
        ] {
            let e = parse_response(&bad, 2).unwrap_err();
            assert!(!is_rejection(&e), "{bad}: {e:#}");
        }
        // Unusable vectors: the input is rejected, so the worker can isolate it.
        for bad in [
            json!({"data": [{"embedding": []}]}),
            json!({"data": [{"embedding": [1.0, null]}]}),
            json!({"data": [{"embedding": [1.0, "x"]}]}),
            json!({"data": [{"embedding": [0.0, 0.0]}]}),
            json!({"data": [{"embedding": [1.0e39, 1.0]}]}),
        ] {
            let e = parse_response(&bad, 1).unwrap_err();
            assert!(is_rejection(&e), "{bad}: {e:#}");
        }
        // Huge but finite values must not overflow into a zero/NaN vector.
        let big = json!({"data": [{"embedding": [3.0e38, 3.0e38]}]});
        let v = parse_response(&big, 1).unwrap();
        assert!(v[0].iter().all(|x| x.is_finite() && *x > 0.7));
    }

    /// A fake API: rejects batches containing "poison", fails everything when `down`.
    fn fake(down: bool, calls: &mut usize) -> impl FnMut(&[String]) -> Result<Vec<Vec<f32>>> {
        move |t: &[String]| {
            *calls += 1;
            if down {
                return Err(Rejected("400: API key not valid".into()).into());
            }
            if t.iter().any(|x| x == "poison") {
                return Err(Rejected("400: input too long".into()).into());
            }
            Ok(t.iter().map(|_| vec![1.0]).collect())
        }
    }

    #[test]
    fn poison_chunks_are_isolated() {
        let texts: Vec<String> = (0..32)
            .map(|i| {
                if i == 7 {
                    "poison".into()
                } else {
                    format!("t{i}")
                }
            })
            .collect();
        let mut calls = 0;
        let out = embed_isolating(&texts, &mut fake(false, &mut calls)).unwrap();
        assert_eq!(out.len(), 32);
        assert!(out[7].as_ref().unwrap_err().contains("input too long"));
        assert!(out.iter().enumerate().all(|(i, o)| (i == 7) == o.is_err()));
        assert!(
            calls <= 2 + 2 * 5,
            "bisection, not one call per item: {calls}"
        );

        let one = ["poison".to_string()];
        let mut calls = 0;
        let out = embed_isolating(&one, &mut fake(false, &mut calls)).unwrap();
        assert!(out[0].is_err());

        // When the API refuses everything, nothing is marked as skipped.
        let mut calls = 0;
        let e = embed_isolating(&texts, &mut fake(true, &mut calls)).unwrap_err();
        assert!(format!("{e:#}").contains("API key"));
        assert_eq!(calls, 2, "one batch + one probe");

        // Transient errors are passed on for a retry with backoff.
        let mut flaky = |_: &[String]| -> Result<Vec<Vec<f32>>> { bail!("503") };
        assert!(embed_isolating(&texts, &mut flaky).is_err());
    }

    #[test]
    fn backoff_ignores_wakeups_and_stops_when_disconnected() {
        let (tx, rx) = mpsc::channel();
        let t = Instant::now();
        tx.send(()).unwrap();
        tx.send(()).unwrap();
        assert!(sleep_through(&rx, Duration::from_millis(150)));
        assert!(t.elapsed() >= Duration::from_millis(150));
        drop(tx);
        let t = Instant::now();
        assert!(!sleep_through(&rx, Duration::from_secs(30)));
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn errors_hide_the_url_and_key() {
        assert_eq!(
            redact_url("https://user:pw@api.example.com:8443/v1/embeddings?key=s3cret"),
            "https://api.example.com:8443"
        );
        assert_eq!(
            redact_url("http://ollama:11434/v1/embeddings"),
            "http://ollama:11434"
        );
        let mut cfg = EmbedConfig::new("https://h.example/v1?key=s3cret", None, "m");
        cfg.key = Some("sk-verysecretkey".into());
        let e = Embedder::new(cfg);
        let s = e.scrub(&format!("failed {} with sk-verysecretkey", e.cfg.url));
        assert!(!s.contains("s3cret") && !s.contains("verysecret"), "{s}");
        assert!(s.contains("https://h.example"), "{s}");
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
