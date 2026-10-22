use std::env;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

const MAX_RECENTS: usize = 50;

/// Loads the recently opened documents, most-recent first. Returns an empty
/// list when no history file exists or it cannot be read.
pub fn load() -> Vec<PathBuf> {
    let Some(path) = recent_path() else {
        return Vec::new();
    };
    load_from(&path)
}

fn load_from(store: &Path) -> Vec<PathBuf> {
    let Ok(contents) = fs::read(store) else {
        return Vec::new();
    };
    serde_json::from_slice::<Vec<Vec<u8>>>(&contents)
        .unwrap_or_default()
        .into_iter()
        .map(|bytes| PathBuf::from(OsString::from_vec(bytes)))
        .collect()
}

/// Records `path` as the most recently opened document, promoting it to the
/// front of the history and capping the list. Failures are ignored so history
/// never blocks opening a document.
pub fn record(path: &Path) {
    let Some(store) = recent_path() else {
        return;
    };
    record_to(&store, path);
}

fn record_to(store: &Path, path: &Path) {
    let entries = promote(load_from(store), path, MAX_RECENTS);
    if let Some(parent) = store.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let entries: Vec<_> = entries
        .iter()
        .map(|path| path.as_os_str().as_bytes())
        .collect();
    if let Ok(contents) = serde_json::to_vec(&entries) {
        let _ = fs::write(store, contents);
    }
}

/// Moves `path` to the front of `entries`, removing any earlier occurrence and
/// truncating to `max` items.
fn promote(mut entries: Vec<PathBuf>, path: &Path, max: usize) -> Vec<PathBuf> {
    entries.retain(|existing| existing != path);
    entries.insert(0, path.to_path_buf());
    entries.truncate(max);
    entries
}

fn recent_path() -> Option<PathBuf> {
    let base = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("officeterm").join("recent"))
}

#[cfg(test)]
mod tests {
    use super::{load_from, promote, record_to};
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    #[test]
    fn history_roundtrips_newlines_without_splitting_paths() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = directory.path().join("cache").join("recent");
        let plain = directory.path().join("plain.docx");
        let newline = directory.path().join("report\nwith\nnewlines.docx");

        record_to(&store, &plain);
        record_to(&store, &newline);

        assert_eq!(load_from(&store), vec![newline.clone(), plain.clone()]);
        record_to(&store, &plain);
        assert_eq!(load_from(&store), vec![plain, newline]);
    }

    #[test]
    fn history_roundtrips_non_utf8_paths_without_lossy_collisions() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = directory.path().join("recent");
        let invalid = directory
            .path()
            .join(OsString::from_vec(b"report-\xff.docx".to_vec()));
        let replacement = directory.path().join("report-\u{fffd}.docx");

        record_to(&store, &invalid);
        record_to(&store, &replacement);

        assert_eq!(
            load_from(&store),
            vec![replacement.clone(), invalid.clone()]
        );
        record_to(&store, &invalid);
        assert_eq!(load_from(&store), vec![invalid, replacement]);
    }

    #[test]
    fn promote_moves_existing_entry_to_front() {
        let entries = vec![PathBuf::from("/a.pptx"), PathBuf::from("/b.pptx")];

        let promoted = promote(entries, &PathBuf::from("/b.pptx"), 50);

        assert_eq!(
            promoted,
            vec![PathBuf::from("/b.pptx"), PathBuf::from("/a.pptx")]
        );
    }

    #[test]
    fn promote_caps_the_history_length() {
        let entries = vec![PathBuf::from("/a.pptx"), PathBuf::from("/b.pptx")];

        let promoted = promote(entries, &PathBuf::from("/c.pptx"), 2);

        assert_eq!(
            promoted,
            vec![PathBuf::from("/c.pptx"), PathBuf::from("/a.pptx")]
        );
    }
}
