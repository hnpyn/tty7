use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::core::gitignore::GitignoreChain;
use crate::host::{Entry, SearchHit, guard_off_ui};

#[derive(Default)]
pub struct LocalHost {
    gitignore: Arc<Mutex<GitignoreChain>>,
}

impl LocalHost {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn shared() -> Arc<Self> {
        static LOCAL: OnceLock<Arc<LocalHost>> = OnceLock::new();
        LOCAL.get_or_init(|| Arc::new(LocalHost::new())).clone()
    }

    pub fn join(&self, dir: &Path, name: &str) -> PathBuf {
        dir.join(name)
    }

    pub fn read_dir(&self, dir: &Path, root: Option<&Path>) -> io::Result<Vec<Entry>> {
        guard_off_ui();
        Ok(self.list(dir, root)?.into_iter().map(|(e, _)| e).collect())
    }

    /// The real path behind `p`, in the spelling the rest of tty7 keys by.
    ///
    /// `fs::canonicalize` answers with the extended-length form on Windows —
    /// `\\?\C:\Users\x\repo` — which is a different `Prefix` component from
    /// the `C:\Users\x\repo` a shell, a pane cwd and `git` all report, and so
    /// compares unequal, hashes differently and fails `starts_with` against
    /// every one of them. `\\?\` is a Win32 API escape hatch rather than part
    /// of the path's identity, so it comes off here, at the one boundary that
    /// produces it. What the call is actually *for* — resolving a junction, a
    /// `subst` drive, an 8.3 short name or a symlink — is untouched. See
    /// [`crate::core::path_spelling`].
    pub fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
        guard_off_ui();
        Ok(crate::core::path_spelling::local_spelling_buf(
            fs::canonicalize(p)?,
        ))
    }

    pub fn search(
        &self,
        roots: &[PathBuf],
        query: &str,
        limit: usize,
        max_dirs: usize,
        show_hidden: bool,
    ) -> io::Result<Vec<SearchHit>> {
        guard_off_ui();
        let needle = query.to_lowercase();
        let mut out: Vec<SearchHit> = Vec::new();
        let mut visited = 0usize;
        for root in roots {
            let mut queue: VecDeque<PathBuf> = VecDeque::from([root.clone()]);
            while let Some(dir) = queue.pop_front() {
                if out.len() >= limit || visited >= max_dirs {
                    break;
                }
                visited += 1;
                let Ok(entries) = self.list(&dir, Some(root)) else {
                    continue;
                };
                for (e, path) in entries {
                    if !show_hidden && (e.ignored || e.name.starts_with('.')) {
                        continue;
                    }
                    if e.is_dir {
                        queue.push_back(path.clone());
                    }
                    if e.name.to_lowercase().contains(&needle) {
                        out.push(SearchHit {
                            name: e.name,
                            path,
                            is_dir: e.is_dir,
                            ignored: e.ignored,
                        });
                        if out.len() >= limit {
                            break;
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    pub fn repo_root(&self, p: &Path) -> io::Result<Option<PathBuf>> {
        guard_off_ui();
        Ok(p.ancestors()
            .find(|a| a.join(".git").exists())
            .map(Path::to_path_buf))
    }

    fn list(&self, dir: &Path, root: Option<&Path>) -> io::Result<Vec<(Entry, PathBuf)>> {
        let mut out: Vec<(Entry, PathBuf)> = Vec::new();
        for e in fs::read_dir(dir)?.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            let ft = e.file_type().ok();
            let is_symlink = ft.is_some_and(|t| t.is_symlink());
            let is_dir = if is_symlink {
                fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false)
            } else {
                ft.is_some_and(|t| t.is_dir())
            };
            out.push((
                Entry {
                    name,
                    is_dir,
                    is_symlink,
                    ignored: false,
                },
                path,
            ));
        }

        let mut chain = self.gitignore.lock().unwrap_or_else(|e| e.into_inner());
        for (entry, path) in &mut out {
            entry.ignored = entry.name == ".git"
                || root.is_some_and(|root| chain.is_ignored(path, entry.is_dir, root));
        }
        drop(chain);

        sort_entries(&mut out);
        Ok(out)
    }
}

fn sort_entries(entries: &mut [(Entry, PathBuf)]) {
    entries.sort_by(|(a, _), (b, _)| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> (LocalHost, tempfile::TempDir) {
        (LocalHost::new(), tempfile::tempdir().expect("temp dir"))
    }

    #[test]
    fn sort_matches_the_file_trees_order() {
        let mut v: Vec<(Entry, PathBuf)> = ["main.rs", "Cargo.toml", ".gitignore", "src", "Zeta"]
            .iter()
            .map(|n| {
                (
                    Entry {
                        name: (*n).to_string(),
                        is_dir: *n == "src",
                        is_symlink: false,
                        ignored: false,
                    },
                    PathBuf::from(n),
                )
            })
            .collect();
        sort_entries(&mut v);
        let names: Vec<&str> = v.iter().map(|(e, _)| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["src", ".gitignore", "Cargo.toml", "main.rs", "Zeta"]
        );
    }

    #[test]
    fn shared_is_a_singleton() {
        assert!(Arc::ptr_eq(&LocalHost::shared(), &LocalHost::shared()));
    }

    #[test]
    fn gitignore_chain_scores_the_file_tree_fixture() {
        let (h, tmp) = sandbox();
        let root = tmp.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join(".gitignore"), b"*.log\nbuild/\n").unwrap();
        fs::write(root.join("src/.gitignore"), b"!keep.log\n").unwrap();
        fs::write(root.join("drop.log"), b"").unwrap();
        fs::write(root.join("src/keep.log"), b"").unwrap();
        fs::write(root.join("src/main.rs"), b"").unwrap();

        let ignored = |entries: &[Entry], name: &str| {
            entries
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("{name} missing"))
                .ignored
        };
        let top = h.read_dir(root, Some(root)).unwrap();
        assert!(ignored(&top, "drop.log"));
        assert!(ignored(&top, ".git"));
        assert!(!ignored(&top, "src"));

        let nested = h.read_dir(&root.join("src"), Some(root)).unwrap();
        assert!(!ignored(&nested, "keep.log"), "whitelist un-ignores");
        assert!(!ignored(&nested, "main.rs"));

        let hits = h
            .search(&[root.to_path_buf()], "log", 200, 2000, false)
            .unwrap();
        let names: Vec<&str> = hits.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["keep.log"]);
    }
}
