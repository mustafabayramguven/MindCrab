//! Filesystem access to the vault. All paths that come from users or agents go
//! through [`Vault::resolve`], which rejects traversal and hidden folders.

use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

pub const MAX_NOTE_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_FILE_BYTES: usize = 25 * 1024 * 1024;

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

    pub fn exists(&self, rel: &str) -> bool {
        self.abs(rel).is_file()
    }

    pub fn read(&self, rel: &str) -> Result<String> {
        let p = self.abs(rel);
        let bytes = fs::read(&p).with_context(|| format!("note not found: {rel}"))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn read_bytes(&self, rel: &str) -> Result<Vec<u8>> {
        Ok(fs::read(self.abs(rel))?)
    }

    /// Atomic write: temp file in the same dir, then rename.
    pub fn write(&self, rel: &str, data: &[u8]) -> Result<()> {
        let p = self.abs(rel);
        if let Some(dir) = p.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = p.with_file_name(format!(
            ".{}.mc-tmp",
            p.file_name().and_then(|n| n.to_str()).unwrap_or("note")
        ));
        fs::write(&tmp, data)?;
        fs::rename(&tmp, &p)?;
        Ok(())
    }

    pub fn mkdir(&self, rel: &str) -> Result<()> {
        Ok(fs::create_dir_all(self.abs(rel))?)
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let dst = self.abs(to);
        if dst.exists() {
            bail!("target already exists: {to}");
        }
        if let Some(dir) = dst.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::rename(self.abs(from), dst)?;
        Ok(())
    }

    /// Moves a file into `.trash/` instead of deleting it.
    pub fn trash(&self, rel: &str) -> Result<()> {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let dst = self
            .root
            .join(".trash")
            .join(format!("{stamp}__{}", rel.replace('/', "__")));
        fs::create_dir_all(dst.parent().unwrap())?;
        fs::rename(self.abs(rel), dst)?;
        Ok(())
    }

    /// Removes a folder only if it is empty.
    pub fn rmdir(&self, rel: &str) -> Result<()> {
        fs::remove_dir(self.abs(rel)).with_context(|| format!("folder is not empty: {rel}"))
    }

    pub fn meta(&self, rel: &str) -> Option<Entry> {
        let m = fs::metadata(self.abs(rel)).ok()?;
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
}
