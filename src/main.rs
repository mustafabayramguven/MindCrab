mod app;
mod bases;
mod embed;
mod features;
mod index;
mod mcp;
mod note;
mod render;
mod review;
mod scope;
mod seed;
mod server;
mod store;
mod tasks;
mod vault;
mod watch;
mod web;

use anyhow::Result;
use app::{App, Settings};
use clap::builder::BoolishValueParser;
use clap::{Parser, Subcommand};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "mindcrab",
    version,
    about = "Self-hosted Markdown notebook and AI memory with an MCP server"
)]
struct Cli {
    /// Folder that holds the Markdown notes (works with an Obsidian vault).
    #[arg(long, env = "MINDCRAB_VAULT", default_value = "./vault", global = true)]
    vault: PathBuf,
    /// Where the index and tokens live. Default: <vault>/.mindcrab
    #[arg(long, env = "MINDCRAB_DATA", global = true)]
    data: Option<PathBuf>,
    #[arg(long, env = "MINDCRAB_HOST", default_value = "0.0.0.0", global = true)]
    host: String,
    #[arg(long, env = "MINDCRAB_PORT", default_value_t = 3436, global = true)]
    port: u16,
    /// Token for the web UI and REST API. Generated and saved on first start if unset.
    #[arg(long, env = "MINDCRAB_TOKEN", hide_env_values = true, global = true)]
    token: Option<String>,
    /// Separate token for MCP clients, so you can revoke AI access without logging out. Defaults to the web token.
    #[arg(
        long,
        env = "MINDCRAB_MCP_TOKEN",
        hide_env_values = true,
        global = true
    )]
    mcp_token: Option<String>,
    /// Only expose read tools over MCP.
    #[arg(long, env = "MINDCRAB_MCP_READ_ONLY", global = true, value_parser = BoolishValueParser::new())]
    mcp_read_only: bool,
    /// Facts saved with `remember` over the main MCP token wait for approval in the web UI.
    #[arg(long, env = "MINDCRAB_REVIEW_REMEMBER", global = true, value_parser = BoolishValueParser::new())]
    review_remember: bool,
    /// Mark the login cookie Secure (set this when served over HTTPS).
    #[arg(long, env = "MINDCRAB_COOKIE_SECURE", global = true, value_parser = BoolishValueParser::new())]
    cookie_secure: bool,
    #[arg(
        long,
        env = "MINDCRAB_DAILY_DIR",
        default_value = "Journal",
        global = true
    )]
    daily_dir: String,
    #[arg(
        long,
        env = "MINDCRAB_MEMORY_DIR",
        default_value = "Memory",
        global = true
    )]
    memory_dir: String,
    #[arg(
        long,
        env = "MINDCRAB_TEMPLATES_DIR",
        default_value = "Templates",
        global = true
    )]
    templates_dir: String,
    /// OpenAI-compatible embeddings API base URL, enables semantic search.
    /// e.g. http://ollama:11434/v1 or https://generativelanguage.googleapis.com/v1beta/openai
    #[arg(long, env = "MINDCRAB_EMBED_URL", global = true)]
    embed_url: Option<String>,
    #[arg(
        long,
        env = "MINDCRAB_EMBED_KEY",
        hide_env_values = true,
        global = true
    )]
    embed_key: Option<String>,
    /// Embedding model name, e.g. nomic-embed-text, bge-m3, gemini-embedding-001.
    #[arg(long, env = "MINDCRAB_EMBED_MODEL", global = true)]
    embed_model: Option<String>,
    /// Text prepended to search queries (e.g. "search_query: " for nomic-embed-text).
    #[arg(
        long,
        env = "MINDCRAB_EMBED_QUERY_PREFIX",
        default_value = "",
        global = true
    )]
    embed_query_prefix: String,
    /// Text prepended to note chunks (e.g. "search_document: " for nomic-embed-text).
    #[arg(
        long,
        env = "MINDCRAB_EMBED_DOC_PREFIX",
        default_value = "",
        global = true
    )]
    embed_doc_prefix: String,
    /// Optional output dimensions (only for APIs that support "dimensions").
    #[arg(long, env = "MINDCRAB_EMBED_DIMS", global = true)]
    embed_dims: Option<usize>,
    #[arg(
        long,
        env = "MINDCRAB_EMBED_BATCH",
        default_value_t = 32,
        global = true
    )]
    embed_batch: usize,
    /// Minimum cosine similarity for semantic hits (model dependent; 0 = only relative cut-off).
    #[arg(
        long,
        env = "MINDCRAB_EMBED_MIN_SCORE",
        default_value_t = 0.0,
        global = true
    )]
    embed_min_score: f32,
    /// Don't create starter notes in an empty vault.
    #[arg(long, env = "MINDCRAB_NO_SEED", global = true, value_parser = BoolishValueParser::new())]
    no_seed: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the web server (default).
    Serve,
    /// Speak MCP over stdin/stdout (for local clients that launch a command).
    Mcp,
    /// Rebuild the search index from the files on disk.
    Reindex,
    /// Print the web and MCP tokens.
    Token,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mindcrab=info,tower_http=warn".into()),
        )
        .init();

    let settings = Settings {
        daily_dir: cli.daily_dir.clone(),
        memory_dir: cli.memory_dir.clone(),
        templates_dir: cli.templates_dir.trim_matches('/').to_string(),
        ..Default::default()
    };
    let embedder = match (
        cli.embed_url.as_deref().filter(|u| !u.trim().is_empty()),
        cli.embed_model.as_deref().filter(|m| !m.trim().is_empty()),
    ) {
        (Some(url), Some(model)) => {
            let mut c = embed::EmbedConfig::new(url, cli.embed_key.clone(), model);
            c.query_prefix = cli.embed_query_prefix.clone();
            c.doc_prefix = cli.embed_doc_prefix.clone();
            c.dims = cli.embed_dims;
            c.batch = cli.embed_batch.clamp(1, 256);
            c.min_score = cli.embed_min_score;
            Some(embed::Embedder::new(c))
        }
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!(
                "set both MINDCRAB_EMBED_URL and MINDCRAB_EMBED_MODEL to enable semantic search"
            )
        }
        _ => None,
    };
    if matches!(cli.cmd, Some(Cmd::Reindex)) {
        // Re-reads every file; revisions are kept.
        let app = App::open(&cli.vault, cli.data.clone(), settings)?.with_embedder(embedder);
        let (c, r) = app.sync(true)?;
        eprintln!("reindexed {c} notes, removed {r}");
        return Ok(());
    }

    let app = Arc::new(App::open(&cli.vault, cli.data.clone(), settings)?.with_embedder(embedder));
    if app.semantic_enabled() && !matches!(cli.cmd, Some(Cmd::Token)) {
        app.set_waker(embed::spawn_worker(app.clone()));
    }
    let (web_token, web_new) =
        app::ensure_token(&app.data_dir, cli.token.clone(), "MINDCRAB_TOKEN")?;
    let mcp_token = match cli.mcp_token.clone().filter(|t| !t.trim().is_empty()) {
        Some(t) => app::ensure_token(&app.data_dir, Some(t), "MINDCRAB_MCP_TOKEN")?.0,
        None => web_token.clone(),
    };

    match cli.cmd.unwrap_or(Cmd::Serve) {
        Cmd::Token => {
            println!("web token: {web_token}");
            println!("mcp token: {mcp_token}");
            Ok(())
        }
        Cmd::Mcp => {
            app.full_sync()?;
            stdio(app, cli.mcp_read_only, cli.review_remember)
        }
        Cmd::Serve => {
            let (changed, removed) = app.full_sync()?;
            if !cli.no_seed {
                app.seed_if_empty()?;
            }
            tracing::info!(
                "vault {} — {} notes ({changed} indexed, {removed} removed)",
                app.vault.root().display(),
                app.idx().count()?
            );
            if web_new {
                tracing::warn!(
                    "generated a login token (saved in {}/mindcrab_token): {web_token}",
                    app.data_dir.display()
                );
            }
            watch::spawn(app.clone());
            watch::spawn_periodic(app.clone(), std::time::Duration::from_secs(300));
            let cfg = server::Config {
                web_token,
                mcp_token,
                mcp_read_only: cli.mcp_read_only,
                mcp_review: cli.review_remember,
                cookie_secure: cli.cookie_secure,
            };
            let router = server::router(server::shared(app, cfg));
            let addr = format!("{}:{}", cli.host, cli.port);
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(async move {
                    let listener = tokio::net::TcpListener::bind(&addr).await?;
                    tracing::info!("MindCrab listening on http://{addr}  (MCP: /mcp)");
                    axum::serve(listener, router)
                        .with_graceful_shutdown(shutdown())
                        .await?;
                    Ok(())
                })
        }
        Cmd::Reindex => unreachable!(),
    }
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("shutting down");
}

/// Newline-delimited JSON-RPC over stdio.
fn stdio(app: Arc<App>, read_only: bool, review: bool) -> Result<()> {
    let mut session = mcp::Session::new(scope::Scope::full("stdio", read_only, review), false);
    session.client = "stdio".into();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str(&line) {
            Ok(msg) => mcp::handle(&app, &mut session, msg),
            Err(e) => Some(
                serde_json::json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":format!("parse error: {e}")}}),
            ),
        };
        if let Some(r) = reply {
            writeln!(stdout, "{r}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}
