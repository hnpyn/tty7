//! Project-file index for the prompt composer's `@` menu.
//!
//! The walk and the ranker used to live under the workbench's quick-open
//! palette. The palette is gone; the composer still completes file names from
//! the pane's directory, so the index stays here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tty7_core::host::local::LocalHost;

pub(crate) const INDEX_LIMIT: usize = 50_000;
pub(crate) const INDEX_MAX_DIRS: usize = 20_000;

const PATH_ONLY_PENALTY: i32 = 15;

#[derive(Debug)]
pub(crate) struct IndexedFile {
    pub path: PathBuf,
    pub rel: String,
    pub name_at: usize,
    lower: String,
}

impl IndexedFile {
    pub(crate) fn name(&self) -> &str {
        &self.rel[self.name_at..]
    }

    /// The directory part of `rel`, without its trailing separator. Empty for
    /// a file at the root.
    pub(crate) fn dir(&self) -> &str {
        self.rel[..self.name_at].trim_end_matches(['/', '\\'])
    }
}

#[derive(Debug, Default)]
pub(crate) struct FileIndex {
    pub files: Vec<IndexedFile>,
}

#[derive(Clone, Debug, Default)]
pub(crate) enum FileList {
    #[default]
    NoRoots,
    Failed,
    Ready(Arc<FileIndex>),
}

pub(crate) fn project_roots(resolved: Vec<PathBuf>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for root in resolved {
        let is_home = home.is_some_and(|home| root == home);
        let is_fs_root = root.parent().is_none();
        if is_home || is_fs_root || roots.contains(&root) {
            continue;
        }
        roots.push(root);
    }
    let all = roots.clone();
    roots.retain(|r| !all.iter().any(|other| other != r && r.starts_with(other)));
    roots
}

pub(crate) fn build_index(roots: &[PathBuf], files: Vec<PathBuf>) -> FileIndex {
    let many = roots.len() > 1;
    let files = files
        .into_iter()
        .filter_map(|path| {
            let text = path.to_string_lossy().into_owned();
            let (root, under) = roots
                .iter()
                .filter_map(|root| {
                    let root = root.to_string_lossy();
                    let under = text.strip_prefix(root.as_ref())?;
                    let under = under.trim_start_matches(['/', '\\']);
                    (under.len() < text.len() - root.len() || root.ends_with(['/', '\\']))
                        .then(|| (root.into_owned(), under.to_string()))
                })
                .max_by_key(|(root, _)| root.len())?;
            if under.is_empty() {
                return None;
            }
            let rel = match many {
                true => {
                    let name = root
                        .trim_end_matches(['/', '\\'])
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or_default();
                    format!("{name}/{under}")
                }
                false => under,
            };
            let name_at = rel.rfind(['/', '\\']).map_or(0, |i| i + 1);
            let lower = rel.to_lowercase();
            Some(IndexedFile {
                path,
                rel,
                name_at,
                lower,
            })
        })
        .collect();
    FileIndex { files }
}

fn is_subsequence(needle: &[char], hay: &str) -> bool {
    let mut want = needle.iter().peekable();
    for c in hay.chars() {
        match want.peek() {
            Some(w) if **w == c => {
                want.next();
            }
            Some(_) => {}
            None => break,
        }
    }
    want.peek().is_none()
}

fn file_score(needle: &str, file: &IndexedFile) -> Option<i32> {
    let on_name = fuzzy_score(needle, file.name());
    let on_path = fuzzy_score(needle, &file.rel).map(|s| s - PATH_ONLY_PENALTY);
    on_name.into_iter().chain(on_path).max()
}

pub(crate) fn rank<'a>(
    index: &'a FileIndex,
    needle: &str,
    limit: usize,
) -> Vec<(i32, &'a IndexedFile)> {
    let chars: Vec<char> = needle
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| !c.is_whitespace())
        .collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(i32, &IndexedFile)> = index
        .files
        .iter()
        .filter(|f| is_subsequence(&chars, &f.lower))
        .filter_map(|f| Some((file_score(needle, f)?, f)))
        .collect();
    hits.sort_by(|(a, fa), (b, fb)| b.cmp(a).then_with(|| fa.rel.len().cmp(&fb.rel.len())));
    hits.truncate(limit);
    hits
}

pub(crate) fn walk(h: &LocalHost, cwds: &[PathBuf], home: Option<&Path>) -> FileList {
    let resolved = cwds
        .iter()
        .map(|cwd| {
            h.repo_root(cwd)
                .ok()
                .flatten()
                .unwrap_or_else(|| cwd.clone())
        })
        .collect();
    let roots = project_roots(resolved, home);
    if roots.is_empty() {
        return FileList::NoRoots;
    }
    match h.search(&roots, "", INDEX_LIMIT, INDEX_MAX_DIRS, false) {
        Ok(hits) => {
            let files = hits
                .into_iter()
                .filter(|hit| !hit.is_dir)
                .map(|hit| hit.path)
                .collect();
            FileList::Ready(Arc::new(build_index(&roots, files)))
        }
        Err(e) => {
            log::warn!("file completion: walking {roots:?}: {e}");
            FileList::Failed
        }
    }
}

fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let needle: Vec<char> = query
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| !c.is_whitespace())
        .collect();
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    if needle.len() > hay.len() {
        return None;
    }
    let best = (0..hay.len())
        .filter(|&i| hay[i] == needle[0])
        .filter_map(|start| align(&needle, &hay, start))
        .max()?;
    let mut score = best;
    if hay == needle {
        score += 120;
    } else if hay.starts_with(&needle) {
        score += 50;
    }
    score -= (hay.len() as i32) / 6;
    Some(score)
}

fn align(needle: &[char], hay: &[char], start: usize) -> Option<i32> {
    let mut qi = 0usize;
    let mut score = 0i32;
    let mut run = 0i32;
    let mut prev_hit = false;
    for (i, ch) in hay.iter().enumerate().skip(start) {
        if qi >= needle.len() {
            break;
        }
        if *ch != needle[qi] {
            prev_hit = false;
            run = 0;
            continue;
        }
        score += 1;
        let word_start = i == 0 || !hay[i - 1].is_alphanumeric();
        if word_start {
            score += 12;
        }
        if i == 0 {
            score += 10;
        }
        if prev_hit {
            run += 1;
            score += 6 + run.min(8);
        } else {
            run = 0;
        }
        prev_hit = true;
        qi += 1;
    }
    (qi == needle.len()).then_some(score)
}
