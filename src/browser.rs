use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::process::Operation;

const MAX_RECURSIVE_DEPTH: usize = 16;
const MAX_RECURSIVE_FILES: usize = 10_000;
const GIT_ROOT_TIMEOUT: Duration = Duration::from_secs(2);
const GIT_LS_FILES_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub struct BrowserEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_recent: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserEntrySource {
    Recent,
    Here,
    Subdirectory,
}

impl BrowserEntrySource {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Recent => "RECENT",
            Self::Here => "HERE",
            Self::Subdirectory => "SUBDIR",
        }
    }
}

pub struct BrowserState {
    pub current_dir: PathBuf,
    pub entries: Vec<BrowserEntry>,
    pub selected: usize,
    pub scroll_offset: usize,
    pub filter: String,
    pub filtered_indices: Vec<usize>,
    root_dir: PathBuf,
    recents: Vec<PathBuf>,
    recursive_entries: Vec<BrowserEntry>,
    recursive_loaded: bool,
    recursive_rx: Option<Receiver<Vec<BrowserEntry>>>,
    recursive_op: Option<Operation>,
}

impl Drop for BrowserState {
    fn drop(&mut self) {
        if let Some(op) = self.recursive_op.take() {
            op.cancel();
        }
    }
}

impl BrowserState {
    pub fn new(dir: PathBuf) -> Self {
        let mut state = Self {
            current_dir: dir.clone(),
            entries: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            filter: String::new(),
            filtered_indices: Vec::new(),
            root_dir: dir,
            recents: Vec::new(),
            recursive_entries: Vec::new(),
            recursive_loaded: false,
            recursive_rx: None,
            recursive_op: None,
        };
        state.load_dir();
        state
    }

    /// Supplies the recently opened documents, which are listed first in the
    /// starting directory and remain available to recursive filtering.
    pub fn set_recents(&mut self, recents: Vec<PathBuf>) {
        self.recents = recents;
        self.load_dir();
    }

    pub fn load_dir(&mut self) {
        if let Some(op) = self.recursive_op.take() {
            op.cancel();
        }
        self.entries.clear();
        self.filter.clear();
        self.recursive_loaded = false;
        self.recursive_rx = None;

        if let Some(parent) = self.current_dir.parent() {
            self.entries.push(BrowserEntry {
                name: "../".into(),
                path: parent.to_path_buf(),
                is_dir: true,
                is_recent: false,
            });
        }

        let Ok(read_dir) = std::fs::read_dir(&self.current_dir) else {
            self.rebuild_filter();
            return;
        };
        let mut directories = Vec::new();
        let mut files = Vec::new();
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                directories.push(BrowserEntry {
                    name: format!("{name}/"),
                    path: entry.path(),
                    is_dir: true,
                    is_recent: false,
                });
            } else if is_presentation(&entry.path()) {
                files.push(BrowserEntry {
                    name,
                    path: entry.path(),
                    is_dir: false,
                    is_recent: false,
                });
            }
        }
        directories.sort_by(|left, right| left.name.cmp(&right.name));
        files.sort_by(|left, right| left.name.cmp(&right.name));
        self.entries.extend(directories);
        self.entries.extend(files);
        self.prepend_recents();
        self.selected = 0;
        self.scroll_offset = 0;
        self.rebuild_filter();
    }

    /// Inserts recent documents that still exist at the top of the listing, but
    /// only in the directory the picker started in and only when they are not
    /// already shown there.
    fn prepend_recents(&mut self) {
        if self.current_dir != self.root_dir || self.recents.is_empty() {
            return;
        }
        let insert_at = usize::from(
            self.entries
                .first()
                .is_some_and(|entry| entry.name == "../"),
        );
        let mut recent_entries = Vec::new();
        for path in &self.recents {
            if !path.is_file() || !is_presentation(path) {
                continue;
            }
            if let Some(position) = self.entries.iter().position(|entry| &entry.path == path) {
                let mut entry = self.entries.remove(position);
                entry.is_recent = true;
                recent_entries.push(entry);
                continue;
            }
            let file_name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            recent_entries.push(BrowserEntry {
                name: file_name,
                path: path.clone(),
                is_dir: false,
                is_recent: true,
            });
        }
        for (offset, entry) in recent_entries.into_iter().enumerate() {
            self.entries.insert(insert_at + offset, entry);
        }
    }

    pub fn preload_recursive(&mut self) {
        if self.recursive_loaded || self.recursive_rx.is_some() {
            return;
        }
        let directory = self.current_dir.clone();
        let recents = self.recents.clone();
        let op = Operation::default();
        let (sender, receiver) = mpsc::channel();
        self.recursive_rx = Some(receiver);
        self.recursive_op = Some(op.clone());
        thread::spawn(move || {
            if op.check().is_err() {
                return;
            }
            let root = find_git_root(&directory, &op).unwrap_or_else(|| directory.clone());
            if op.check().is_err() {
                return;
            }
            let mut entries: Vec<_> = collect_presentation_files(&root, &op)
                .into_iter()
                .map(|(name, path)| BrowserEntry {
                    name,
                    is_recent: recents.contains(&path),
                    path,
                    is_dir: false,
                })
                .collect();
            if op.check().is_err() {
                return;
            }
            if let Ok(read_dir) = std::fs::read_dir(&directory) {
                for entry in read_dir
                    .flatten()
                    .filter(|entry| is_presentation(&entry.path()))
                {
                    if op.check().is_err() {
                        return;
                    }
                    if !entries.iter().any(|existing| existing.path == entry.path()) {
                        entries.push(BrowserEntry {
                            name: entry.file_name().to_string_lossy().into_owned(),
                            path: entry.path(),
                            is_dir: false,
                            is_recent: false,
                        });
                    }
                }
            }
            for path in recents {
                if op.check().is_err() {
                    return;
                }
                if !path.is_file() || !is_presentation(&path) {
                    continue;
                }
                if let Some(entry) = entries.iter_mut().find(|entry| entry.path == path) {
                    entry.is_recent = true;
                    continue;
                }
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string_lossy().into_owned());
                entries.push(BrowserEntry {
                    name,
                    path,
                    is_dir: false,
                    is_recent: true,
                });
            }
            entries.sort_by(|left, right| left.name.cmp(&right.name));
            let _ = sender.send(entries);
        });
    }

    pub fn poll_recursive(&mut self) -> bool {
        let Some(receiver) = self.recursive_rx.as_ref() else {
            return false;
        };
        match receiver.try_recv() {
            Ok(entries) => {
                let selected_path = self.selected_entry().map(|entry| entry.path.clone());
                self.recursive_entries = entries;
                self.recursive_loaded = true;
                self.recursive_rx = None;
                self.recursive_op = None;
                if !self.filter.is_empty() {
                    self.rebuild_filter();
                    self.restore_selection(selected_path.as_deref());
                }
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.recursive_rx = None;
                self.recursive_op = None;
                false
            }
        }
    }

    pub fn recursive_loading(&self) -> bool {
        self.recursive_rx.is_some()
    }

    pub fn rebuild_filter(&mut self) {
        if self.filter.is_empty() {
            self.filtered_indices = (0..self.entries.len()).collect();
            return;
        }

        let pattern = Pattern::parse(&self.filter, CaseMatching::Ignore, Normalization::Smart);
        let query = self.filter.trim().to_lowercase();
        let filtered_indices = {
            let source = self.active_source();
            let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
            let mut buffer = Vec::new();
            let mut scored: Vec<_> = source
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    let basename = entry
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy())
                        .unwrap_or_else(|| entry.name.as_str().into());
                    let basename_score =
                        pattern.score(Utf32Str::new(basename.as_ref(), &mut buffer), &mut matcher);
                    let path = entry.path.to_string_lossy();
                    let path_score = if entry.name == "../" {
                        None
                    } else {
                        pattern.score(Utf32Str::new(&path, &mut buffer), &mut matcher)
                    };
                    let score = basename_score.or(path_score)?;
                    let basename_lower = basename.to_lowercase();
                    let match_tier = if basename_lower == query {
                        3
                    } else if basename_lower.starts_with(&query) {
                        2
                    } else if basename_score.is_some() {
                        1
                    } else {
                        0
                    };
                    Some((index, match_tier, score, entry.is_recent))
                })
                .collect();
            scored.sort_by(|left, right| {
                right
                    .1
                    .cmp(&left.1)
                    .then_with(|| right.2.cmp(&left.2))
                    .then_with(|| right.3.cmp(&left.3))
                    .then_with(|| source[left.0].path.cmp(&source[right.0].path))
            });
            scored.into_iter().map(|(index, _, _, _)| index).collect()
        };
        self.filtered_indices = filtered_indices;
    }

    pub fn filtered_entries(&self) -> impl Iterator<Item = &BrowserEntry> {
        self.filtered_indices
            .iter()
            .filter_map(|index| self.active_source().get(*index))
    }

    /// Returns the displayed index where the recent-files section begins.
    /// Fuzzy results are ranked by match quality, so they are not sectioned.
    pub fn recent_heading_index(&self) -> Option<usize> {
        if !self.filter.is_empty() {
            return None;
        }
        self.filtered_entries().position(|entry| entry.is_recent)
    }

    pub fn match_indices(&self, value: &str) -> Vec<usize> {
        if self.filter.is_empty() {
            return Vec::new();
        }
        let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
        let mut buffer = Vec::new();
        let mut indices = Vec::new();
        for term in self.filter.split_whitespace() {
            let pattern = Pattern::parse(term, CaseMatching::Ignore, Normalization::Smart);
            let haystack = Utf32Str::new(value, &mut buffer);
            let _ = pattern.indices(haystack, &mut matcher, &mut indices);
        }
        indices.sort_unstable();
        indices.dedup();
        indices
            .into_iter()
            .filter_map(|index| usize::try_from(index).ok())
            .collect()
    }

    pub fn entry_source(&self, entry: &BrowserEntry) -> Option<BrowserEntrySource> {
        if entry.name == "../" {
            return None;
        }
        if entry.is_recent {
            return Some(BrowserEntrySource::Recent);
        }
        if entry.path.parent() == Some(self.current_dir.as_path()) {
            Some(BrowserEntrySource::Here)
        } else {
            Some(BrowserEntrySource::Subdirectory)
        }
    }

    pub fn select_down(&mut self) {
        if !self.filtered_indices.is_empty() {
            self.selected = (self.selected + 1).min(self.filtered_indices.len() - 1);
        }
    }

    pub fn select_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.filtered_indices.len().saturating_sub(1);
    }

    pub fn page_down(&mut self, page_size: usize) {
        if !self.filtered_indices.is_empty() {
            self.selected = (self.selected + page_size).min(self.filtered_indices.len() - 1);
        }
    }

    pub fn page_up(&mut self, page_size: usize) {
        self.selected = self.selected.saturating_sub(page_size);
    }

    pub fn adjust_scroll(&mut self, visible_height: usize) {
        let visible_height = visible_height.max(1);
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
            return;
        }

        let heading_index = self.recent_heading_index();
        while self.scroll_offset < self.selected {
            let entries = self.selected - self.scroll_offset + 1;
            let includes_heading = heading_index
                .is_some_and(|index| index >= self.scroll_offset && index <= self.selected);
            if entries + usize::from(includes_heading) <= visible_height {
                break;
            }
            self.scroll_offset += 1;
        }
    }

    pub fn enter_selected(&mut self) -> Option<PathBuf> {
        let real_index = *self.filtered_indices.get(self.selected)?;
        let entry = self.active_source().get(real_index)?;
        if entry.is_dir {
            self.current_dir = entry.path.clone();
            self.load_dir();
            self.preload_recursive();
            None
        } else {
            Some(entry.path.clone())
        }
    }

    fn selected_entry(&self) -> Option<&BrowserEntry> {
        let real_index = *self.filtered_indices.get(self.selected)?;
        self.active_source().get(real_index)
    }

    fn restore_selection(&mut self, path: Option<&Path>) {
        self.selected = path
            .and_then(|path| {
                self.filtered_indices.iter().position(|index| {
                    self.active_source()
                        .get(*index)
                        .is_some_and(|entry| entry.path == path)
                })
            })
            .unwrap_or(0);
    }

    fn active_source(&self) -> &[BrowserEntry] {
        if !self.filter.is_empty() && self.recursive_loaded {
            &self.recursive_entries
        } else {
            &self.entries
        }
    }
}

fn find_git_root(start: &Path, op: &Operation) -> Option<PathBuf> {
    let mut cmd = Command::new("git");
    cmd.args(["rev-parse", "--show-toplevel"])
        .current_dir(start);
    let output = crate::process::output(&mut cmd, &op.limited(GIT_ROOT_TIMEOUT)).ok()?;
    output
        .status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string()))
}

fn collect_presentation_files(root: &Path, op: &Operation) -> Vec<(String, PathBuf)> {
    if op.check().is_err() {
        return Vec::new();
    }
    let mut cmd = Command::new("git");
    cmd.args([
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ])
    .current_dir(root);
    let git_files = crate::process::output(&mut cmd, &op.limited(GIT_LS_FILES_TIMEOUT));
    match git_files {
        Ok(output) if output.status.success() => {
            let mut files: Vec<_> = output
                .stdout
                .split(|byte| *byte == 0)
                .filter(|relative| !relative.is_empty())
                .take_while(|_| op.check().is_ok())
                .map(|relative| Path::new(OsStr::from_bytes(relative)))
                .filter(|relative| is_presentation(relative))
                .map(|relative| (relative, root.join(relative)))
                .filter(|(_, path)| path.is_file())
                .take(MAX_RECURSIVE_FILES)
                .map(|(relative, path)| (relative.to_string_lossy().into_owned(), path))
                .collect();
            files.sort_by(|left, right| left.0.cmp(&right.0));
            return files;
        }
        _ => {}
    }

    let mut files = Vec::new();
    collect_presentation_files_recursive(root, root, 0, op, &mut files);
    files
}

fn collect_presentation_files_recursive(
    directory: &Path,
    root: &Path,
    depth: usize,
    op: &Operation,
    files: &mut Vec<(String, PathBuf)>,
) {
    if depth > MAX_RECURSIVE_DEPTH || files.len() >= MAX_RECURSIVE_FILES || op.check().is_err() {
        return;
    }
    let Ok(read_dir) = std::fs::read_dir(directory) else {
        return;
    };
    let mut entries: Vec<_> = read_dir.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if files.len() >= MAX_RECURSIVE_FILES || op.check().is_err() {
            return;
        }
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_presentation_files_recursive(&entry.path(), root, depth + 1, op, files);
        } else if is_presentation(&entry.path()) {
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap_or(&path);
            files.push((relative.to_string_lossy().into_owned(), path));
        }
    }
}

fn is_presentation(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "pptx"
            | "ppsx"
            | "potx"
            | "pptm"
            | "ppsm"
            | "potm"
            | "docx"
            | "docm"
            | "dotx"
            | "dotm"
            | "doc"
            | "dot"
            | "rtf"
            | "odt"
            | "xlsx"
            | "xlsm"
            | "xltx"
            | "xltm"
            | "xls"
            | "xlt"
            | "xlsb"
            | "ods"
            | "csv"
            | "tsv"
    )
}

#[cfg(test)]
mod tests {
    use super::BrowserState;
    use std::fs;
    use std::thread;
    use std::time::{Duration, Instant};

    fn wait_for_recursive_scan(browser: &mut BrowserState) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !browser.poll_recursive() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(browser.recursive_loaded, "recursive scan timed out");
    }

    #[test]
    fn browser_lists_directories_and_presentation_files_only() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::create_dir(directory.path().join("docs")).expect("directory");
        fs::write(directory.path().join("one.pptx"), b"synthetic").expect("pptx");
        fs::write(directory.path().join("two.ppsx"), b"synthetic").expect("ppsx");
        fs::write(directory.path().join("notes.txt"), b"synthetic").expect("text file");

        let browser = BrowserState::new(directory.path().to_path_buf());
        let names: Vec<_> = browser
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();

        assert!(names.contains(&"docs/"));
        assert!(names.contains(&"one.pptx"));
        assert!(names.contains(&"two.ppsx"));
        assert!(!names.contains(&"notes.txt"));
    }

    #[test]
    fn recents_from_other_directories_are_listed_first() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::write(directory.path().join("b.pptx"), b"synthetic").expect("pptx");
        let other = tempfile::tempdir().expect("other directory");
        let recent = other.path().join("z-recent.pptx");
        fs::write(&recent, b"synthetic").expect("recent pptx");

        let mut browser = BrowserState::new(directory.path().to_path_buf());
        browser.set_recents(vec![recent.clone()]);

        let recent_position = browser
            .entries
            .iter()
            .position(|entry| entry.name == "z-recent.pptx" && entry.is_recent)
            .expect("recent entry present");
        let listed = browser
            .entries
            .iter()
            .position(|entry| entry.name == "b.pptx")
            .expect("directory entry present");
        assert!(recent_position < listed);
    }

    #[test]
    fn recents_already_in_the_directory_are_not_duplicated() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("here.pptx");
        fs::write(&path, b"synthetic").expect("pptx");

        let mut browser = BrowserState::new(directory.path().to_path_buf());
        browser.set_recents(vec![path]);

        let count = browser
            .entries
            .iter()
            .filter(|entry| entry.name.contains("here.pptx"))
            .count();
        assert_eq!(count, 1);
        let entry = browser
            .entries
            .iter()
            .find(|entry| entry.name == "here.pptx")
            .expect("recent entry");
        assert!(entry.is_recent);
    }

    #[test]
    fn recursive_filter_keeps_recents_outside_the_search_root() {
        let directory = tempfile::tempdir().expect("temporary directory");
        fs::write(directory.path().join("local.pptx"), b"synthetic").expect("local pptx");
        let other = tempfile::tempdir().expect("other directory");
        let recent = other.path().join("remote-report.pptx");
        fs::write(&recent, b"synthetic").expect("recent pptx");

        let mut browser = BrowserState::new(directory.path().to_path_buf());
        browser.set_recents(vec![recent.clone()]);
        browser.preload_recursive();
        wait_for_recursive_scan(&mut browser);

        browser.filter = "remote report".into();
        browser.rebuild_filter();
        let matches: Vec<_> = browser.filtered_entries().collect();

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, recent);
        assert!(matches[0].is_recent);
    }

    #[test]
    fn fuzzy_match_indices_identify_highlighted_characters() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut browser = BrowserState::new(directory.path().to_path_buf());
        browser.filter = "gss".to_string();

        assert_eq!(browser.match_indices("genesis.pptx"), vec![0, 4, 6]);
    }

    #[test]
    fn git_collection_excludes_missing_and_nonfile_documents() {
        use super::{Operation, collect_presentation_files};
        use std::os::unix::fs::symlink;
        use std::process::Command;

        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(root)
                .status()
                .expect("initialize repository")
                .success()
        );
        for name in ["kept.docx", "deleted.docx", "directory.docx"] {
            fs::write(root.join(name), b"synthetic").expect("write document");
        }
        symlink("kept.docx", root.join("linked.docx")).expect("document symlink");
        symlink("missing.docx", root.join("broken.docx")).expect("broken symlink");
        assert!(
            Command::new("git")
                .args(["add", "--", "."])
                .current_dir(root)
                .status()
                .expect("stage documents")
                .success()
        );
        fs::remove_file(root.join("deleted.docx")).expect("delete indexed document");
        fs::remove_file(root.join("directory.docx")).expect("remove indexed document");
        fs::create_dir(root.join("directory.docx")).expect("replace document with directory");

        let files = collect_presentation_files(root, &Operation::default());
        let names: Vec<_> = files.iter().map(|(name, _)| name.as_str()).collect();

        assert_eq!(names, ["kept.docx", "linked.docx"]);
        assert!(files.iter().all(|(_, path)| path.is_file()));
    }

    #[test]
    fn recursive_collection_honors_depth_limit_and_cancellation() {
        use super::{MAX_RECURSIVE_DEPTH, Operation, collect_presentation_files_recursive};

        let directory = tempfile::tempdir().expect("temporary directory");
        let mut current = directory.path().to_path_buf();
        for depth in 0..=(MAX_RECURSIVE_DEPTH + 3) {
            fs::write(current.join(format!("deck-{depth}.pptx")), b"synthetic")
                .expect("write pptx");
            current = current.join(format!("sub-{depth}"));
            fs::create_dir(&current).expect("create subdir");
        }

        let op = Operation::default();
        let mut files = Vec::new();
        collect_presentation_files_recursive(
            directory.path(),
            directory.path(),
            0,
            &op,
            &mut files,
        );
        assert_eq!(files.len(), MAX_RECURSIVE_DEPTH + 1);
        assert!(
            files
                .iter()
                .any(|(name, _)| name.ends_with(&format!("deck-{MAX_RECURSIVE_DEPTH}.pptx")))
        );
        assert!(
            !files
                .iter()
                .any(|(name, _)| name.ends_with(&format!("deck-{}.pptx", MAX_RECURSIVE_DEPTH + 1)))
        );

        // Pre-cancelled operation collects nothing.
        let cancelled_op = Operation::default();
        cancelled_op.cancel();
        let mut cancelled_files = Vec::new();
        collect_presentation_files_recursive(
            directory.path(),
            directory.path(),
            0,
            &cancelled_op,
            &mut cancelled_files,
        );
        assert!(cancelled_files.is_empty());
    }

    #[test]
    fn load_dir_and_drop_cancel_inflight_recursive_operation() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut browser = BrowserState::new(directory.path().to_path_buf());
        browser.preload_recursive();
        let op_on_reload = browser
            .recursive_op
            .as_ref()
            .expect("recursive_op should be set")
            .clone();
        browser.load_dir();
        assert!(op_on_reload.is_cancelled());

        browser.preload_recursive();
        let op_on_drop = browser
            .recursive_op
            .as_ref()
            .expect("recursive_op should be set")
            .clone();
        drop(browser);
        assert!(op_on_drop.is_cancelled());
    }
}
