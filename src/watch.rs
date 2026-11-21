//! Watches the vault for changes made outside MindCrab (Obsidian, Syncthing, git, an editor…).

use crate::app::App;
use notify::{RecursiveMode, Watcher};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

pub fn spawn(app: Arc<App>) {
    std::thread::Builder::new()
        .name("vault-watch".into())
        .spawn(move || {
            if let Err(e) = run(app) {
                tracing::warn!("file watcher stopped: {e:#} — external edits will be picked up by the periodic rescan");
            }
        })
        .expect("spawn watcher thread");
}

fn run(app: Arc<App>) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            for p in ev.paths {
                let _ = tx.send(p);
            }
        }
    })?;
    watcher.watch(app.vault.root(), RecursiveMode::Recursive)?;
    tracing::info!(
        "watching {} for external changes",
        app.vault.root().display()
    );

    loop {
        // Block for the first event, then collect a short burst (debounce).
        let first = rx.recv()?;
        let mut batch = vec![first];
        let deadline = std::time::Instant::now() + Duration::from_millis(400);
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(p) => batch.push(p),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(e) => return Err(e.into()),
            }
        }
        let rels: HashSet<String> = batch.iter().filter_map(|p| app.vault.rel_of(p)).collect();
        if rels.is_empty() {
            continue;
        }
        // Folder moves/deletes arrive as a single path; rescan in that case.
        let needs_full = rels.iter().any(|r| {
            let abs = app.vault.abs(r);
            abs.is_dir() || (!abs.exists() && !r.contains('.'))
        });
        let res = if needs_full {
            app.full_sync().map(|_| ())
        } else {
            rels.iter().try_for_each(|r| app.sync_path(r))
        };
        if let Err(e) = res {
            tracing::warn!("sync after file change failed: {e:#}");
        }
    }
}

/// Safety net for filesystems where inotify does not work (some network mounts).
pub fn spawn_periodic(app: Arc<App>, every: Duration) {
    std::thread::Builder::new()
        .name("vault-rescan".into())
        .spawn(move || {
            loop {
                std::thread::sleep(every);
                match app.full_sync() {
                    Ok((c, r)) if c + r > 0 => tracing::info!("rescan: {c} changed, {r} removed"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!("rescan failed: {e:#}"),
                }
            }
        })
        .expect("spawn rescan thread");
}
