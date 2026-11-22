//! Watches the vault for changes made outside MindCrab (Obsidian, Syncthing, git, an editor…).

use crate::app::App;
use crate::vault::Vault;
use notify::{RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::Path;
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

/// The vault path an event is about, or `None` when it can't matter: the data dir (the
/// index and its WAL are written on every request), `.trash/`, temp files and other
/// hidden paths, or anything outside the vault.
fn relevant(vault: &Vault, data_dir: &Path, p: &Path) -> Option<String> {
    if p.starts_with(data_dir) {
        return None;
    }
    vault.rel_of(p)
}

fn run(app: Arc<App>) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<String>();
    let vault = app.vault.clone();
    let data_dir = app
        .data_dir
        .canonicalize()
        .unwrap_or_else(|_| app.data_dir.clone());
    // Filter here, so ignored paths never wake the loop below.
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            for p in ev.paths {
                if let Some(rel) = relevant(&vault, &data_dir, &p) {
                    let _ = tx.send(rel);
                }
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
        let mut rels = HashSet::from([first]);
        let deadline = std::time::Instant::now() + Duration::from_millis(400);
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(r) => {
                    rels.insert(r);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(e) => return Err(e.into()),
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_paths_are_ignored() {
        let d = tempfile::tempdir().unwrap();
        let v = Vault::open(d.path()).unwrap();
        let root = v.root();
        let data = root.join(".mindcrab");
        for p in [
            data.join("index.db-wal"),
            root.join(".trash/20260101-000000.000__n.md"),
            root.join("a/.n.md.12-3.mc-tmp"),
            root.join(".obsidian/workspace.json"),
            root.to_path_buf(),
        ] {
            assert_eq!(relevant(&v, &data, &p), None, "{}", p.display());
        }
        // A data dir that is not hidden is ignored as well.
        let custom = root.join("data");
        assert_eq!(relevant(&v, &custom, &custom.join("index.db")), None);
        assert_eq!(
            relevant(&v, &data, &root.join("a/n.md")).as_deref(),
            Some("a/n.md")
        );
    }
}
