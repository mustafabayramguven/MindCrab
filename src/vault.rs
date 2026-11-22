//! Filesystem access to the vault. All paths that come from users or agents go
//! through [`Vault::resolve`], which rejects traversal and hidden folders.

use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

pub const MAX_NOTE_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_FILE_BYTES: usize = 25 * 1024 * 1024;

/// Tells temp files of concurrent writes apart (together with the process id).
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct Vault {
    root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub rel: String,
    pub size: u64,
    pub mtime: i64,
}

impl Vault {
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)
            .with_context(|| format!("creating vault dir {}", root.display()))?;
        let root = root.canonicalize()?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Normalises a user-supplied relative path ("a/b.md") and checks it is safe.
    pub fn clean(rel: &str) -> Result<String> {
        let rel = rel.trim().replace('\\', "/");
        let rel = rel.trim_matches('/');
        if rel.is_empty() {
            bail!("empty path");
        }
        let mut parts = Vec::new();
        for c in Path::new(rel).components() {
            match c {
                Component::Normal(s) => {
                    let s = s.to_str().context("path is not valid UTF-8")?;
                    if s.starts_with('.') {
                        bail!("hidden files and folders are not allowed: {s}");
                    }
                    if s.chars().any(|ch| {
                        ch.is_control() || matches!(ch, ':' | '*' | '?' | '"' | '<' | '>' | '|')
                    }) {
                        bail!("invalid character in path segment: {s}");
                    }
                    parts.push(s.to_string());
                }
                Component::CurDir => {}
                _ => bail!("path must be relative and must not contain '..'"),
            }
        }
        if parts.is_empty() {
            bail!("empty path");
        }
        Ok(parts.join("/"))
    }

    /// Like [`clean`] but also appends `.md` when missing.
    pub fn clean_note(rel: &str) -> Result<String> {
        let c = Self::clean(rel)?;
        Ok(if c.to_lowercase().ends_with(".md") {
            c
        } else {
            format!("{c}.md")
        })
    }

    pub fn abs(&self, clean_rel: &str) -> PathBuf {
        self.root.join(clean_rel)
    }

    /// [`Self::abs`] for file I/O. Rejects paths that leave the vault through a symlink:
    /// the deepest part of the path that exists is resolved and must stay under the
    /// root (so files and folders that don't exist yet can still be created).
    fn io_path(&self, rel: &str) -> Result<PathBuf> {
        let p = self.abs(rel);
        let mut cur = p.as_path();
        loop {
            if fs::symlink_metadata(cur).is_ok() {
                // Exists, possibly as a dangling link (which canonicalize rejects).
                let inside = cur
                    .canonicalize()
                    .is_ok_and(|real| real.starts_with(&self.root));
                if !inside {
                    bail!("path leaves the vault: {rel}");
                }
                return Ok(p);
            }
            cur = match cur.parent() {
                Some(dir) if dir.starts_with(&self.root) => dir,
                _ => bail!("path leaves the vault: {rel}"),
            };
        }
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.io_path(rel).is_ok_and(|p| p.is_file())
    }

    pub fn read(&self, rel: &str) -> Result<String> {
        let p = self.io_path(rel)?;
        let bytes = fs::read(&p).with_context(|| format!("note not found: {rel}"))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn read_bytes(&self, rel: &str) -> Result<Vec<u8>> {
        Ok(fs::read(self.io_path(rel)?)?)
    }

    /// Atomic write: temp file in the same dir, then rename.
    pub fn write(&self, rel: &str, data: &[u8]) -> Result<()> {
        let p = self.io_path(rel)?;
        if let Some(dir) = p.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = temp_path(&p);
        if let Err(e) = fs::write(&tmp, data).and_then(|()| fs::rename(&tmp, &p)) {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    pub fn mkdir(&self, rel: &str) -> Result<()> {
        Ok(fs::create_dir_all(self.io_path(rel)?)?)
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let src = self.io_path(from)?;
        let dst = self.io_path(to)?;
        if dst.exists() {
            bail!("target already exists: {to}");
        }
        if let Some(dir) = dst.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::rename(src, dst)?;
        Ok(())
    }

    /// Moves a file into `.trash/` instead of deleting it.
    pub fn trash(&self, rel: &str) -> Result<()> {
        let src = self.io_path(rel)?;
        let dir = self.root.join(".trash");
        fs::create_dir_all(&dir)?;
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S%.3f");
        let name = rel.replace('/', "__");
        let mut dst = dir.join(format!("{stamp}__{name}"));
        // Never replace an earlier copy (the same name deleted twice within a millisecond).
        let mut n = 1;
        while fs::symlink_metadata(&dst).is_ok() {
            dst = dir.join(format!("{stamp}-{n}__{name}"));
            n += 1;
        }
        fs::rename(src, dst)?;
        Ok(())
    }

    /// Removes a folder only if it is empty.
    pub fn rmdir(&self, rel: &str) -> Result<()> {
        let p = self.io_path(rel)?;
        fs::remove_dir(p).with_context(|| format!("folder is not empty: {rel}"))
    }

    pub fn meta(&self, rel: &str) -> Option<Entry> {
        let m = fs::metadata(self.io_path(rel).ok()?).ok()?;
        Some(Entry {
            rel: rel.to_string(),
            size: m.len(),
            mtime: mtime(&m),
        })
    }

    /// Converts an absolute path under the vault back to a clean relative path.
    /// Returns `None` for hidden or outside paths.
    pub fn rel_of(&self, abs: &Path) -> Option<String> {
        let rel = abs.strip_prefix(&self.root).ok()?;
        let s = rel.to_str()?.replace('\\', "/");
        if s.is_empty() || s.split('/').any(|seg| seg.starts_with('.')) {
            return None;
        }
        Some(s)
    }

    /// Walks the vault and returns (markdown files, other files, folders).
    pub fn scan(&self) -> (Vec<Entry>, Vec<Entry>, Vec<String>) {
        let mut notes = Vec::new();
        let mut files = Vec::new();
        let mut dirs = Vec::new();
        let walker = WalkDir::new(&self.root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                e.depth() == 0 || !e.file_name().to_str().is_some_and(|n| n.starts_with('.'))
            });
        for e in walker.flatten() {
            if e.depth() == 0 {
                continue;
            }
            let Some(rel) = self.rel_of(e.path()) else {
                continue;
            };
            if e.file_type().is_dir() {
                dirs.push(rel);
                continue;
            }
            if !e.file_type().is_file() {
                continue;
            }
            let Ok(m) = e.metadata() else { continue };
            let entry = Entry {
                rel,
                size: m.len(),
                mtime: mtime(&m),
            };
            if is_note(&entry.rel) {
                notes.push(entry);
            } else {
                files.push(entry);
            }
        }
        (notes, files, dirs)
    }
}

pub fn is_note(rel: &str) -> bool {
    rel.to_lowercase().ends_with(".md")
}

/// Hidden temp file next to `p`, unique per write (process id + counter), so concurrent
/// writes of one file never share a temp file.
fn temp_path(p: &Path) -> PathBuf {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("note");
    // Stay within the usual 255-byte file name limit.
    let mut cut = name.len().min(200);
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    p.with_file_name(format!(
        ".{}.{}-{}.mc-tmp",
        &name[..cut],
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

fn mtime(m: &fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_paths() {
        assert_eq!(Vault::clean("a/b.md").unwrap(), "a/b.md");
        assert_eq!(Vault::clean("/a//./b.md/").unwrap(), "a/b.md");
        assert_eq!(
            Vault::clean_note("Notlar/Fikir").unwrap(),
            "Notlar/Fikir.md"
        );
        assert!(Vault::clean("../etc/passwd").is_err());
        assert!(Vault::clean("a/../../b").is_err());
        assert!(Vault::clean(".mindcrab/index.db").is_err());
        assert!(Vault::clean("a/.git/config").is_err());
        assert!(Vault::clean("").is_err());
    }

    #[test]
    fn write_read_trash() {
        let dir = tempfile::tempdir().unwrap();
        let v = Vault::open(dir.path()).unwrap();
        v.write("x/y.md", b"hi").unwrap();
        assert_eq!(v.read("x/y.md").unwrap(), "hi");
        let (n, _, d) = v.scan();
        assert_eq!(n.len(), 1);
        assert_eq!(d, vec!["x"]);
        v.trash("x/y.md").unwrap();
        assert!(!v.exists("x/y.md"));
        assert_eq!(v.scan().0.len(), 0);
    }

    #[test]
    fn temp_names_are_unique_and_hidden() {
        let p = Path::new("/v/a/n.md");
        let (a, b) = (temp_path(p), temp_path(p));
        assert_ne!(a, b);
        assert_eq!(a.parent(), p.parent());
        let name = a.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with(".n.md.") && name.ends_with(".mc-tmp"),
            "{name}"
        );
        let long = Path::new("/v").join("ş".repeat(120) + ".md");
        assert!(temp_path(&long).file_name().unwrap().len() <= 255);
    }

    #[test]
    fn concurrent_writes_leave_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let v = Vault::open(dir.path()).unwrap();
        std::thread::scope(|s| {
            for t in 0..8 {
                let v = &v;
                s.spawn(move || {
                    for i in 0..20 {
                        v.write("n.md", format!("{t}-{i}").as_bytes()).unwrap();
                    }
                });
            }
        });
        assert!(v.read("n.md").unwrap().ends_with("-19"));
        let left: Vec<_> = fs::read_dir(v.root())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .filter(|n| n != "n.md")
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn trash_keeps_every_copy() {
        let dir = tempfile::tempdir().unwrap();
        let v = Vault::open(dir.path()).unwrap();
        for body in ["one", "two", "three"] {
            v.write("a/n.md", body.as_bytes()).unwrap();
            v.trash("a/n.md").unwrap();
        }
        let mut kept: Vec<String> = fs::read_dir(v.root().join(".trash"))
            .unwrap()
            .flatten()
            .map(|e| fs::read_to_string(e.path()).unwrap())
            .collect();
        kept.sort();
        assert_eq!(kept, ["one", "three", "two"]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_leave_the_vault() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.md"), "s").unwrap();
        let v = Vault::open(&dir.path().join("vault")).unwrap();
        symlink(outside.path(), v.root().join("out")).unwrap();
        symlink(outside.path().join("secret.md"), v.root().join("s.md")).unwrap();
        symlink(outside.path().join("new.md"), v.root().join("dangling.md")).unwrap();

        assert!(v.read("out/secret.md").is_err());
        assert!(!v.exists("out/secret.md"));
        assert!(v.meta("out/secret.md").is_none());
        assert!(v.read("s.md").is_err());
        assert!(v.read_bytes("s.md").is_err());
        assert!(v.write("out/new.md", b"x").is_err());
        assert!(v.write("out/sub/new.md", b"x").is_err());
        assert!(v.mkdir("out/sub").is_err());
        assert!(v.write("dangling.md", b"x").is_err());
        assert!(v.trash("s.md").is_err());
        assert!(v.rename("s.md", "t.md").is_err());
        assert!(!outside.path().join("new.md").exists());
        assert!(!outside.path().join("sub").exists());
        assert!(outside.path().join("secret.md").exists());

        // Links that stay inside the vault keep working.
        v.write("real/n.md", b"ok").unwrap();
        symlink(v.root().join("real"), v.root().join("alias")).unwrap();
        assert_eq!(v.read("alias/n.md").unwrap(), "ok");
        v.write("alias/m.md", b"m").unwrap();
        assert!(v.exists("real/m.md"));
    }
}
