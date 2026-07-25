//! Read-before-write tracking.
//!
//! Two failure modes this exists to prevent, both of which silently destroy work:
//!
//! **Blind overwrite.** A model that writes a file it never read replaces content
//! it does not know about. Requiring a prior read costs one tool call and makes
//! the model's assumptions checkable.
//!
//! **Stale edit.** The model reads a file, the user edits it in their editor, and
//! the model then writes based on what it saw a minute ago. The user's change
//! disappears with no error anywhere. Comparing the file's mtime against the read
//! time catches it.
//!
//! Both checks are *recoverable* errors: the model is told to re-read and it
//! proceeds. That is the difference between a guardrail and an obstacle.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::SystemTime;

use camino::{Utf8Path, Utf8PathBuf};

/// Per-session record of which files have been read and when.
///
/// Shared by the `read`, `write`, and `edit` tools — construct one per session and
/// hand each tool an `Arc` of it.
#[derive(Debug, Default)]
pub struct FileTracker {
    reads: Mutex<HashMap<Utf8PathBuf, SystemTime>>,
}

/// Why a write was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteCheck {
    Ok,
    /// The file exists but was never read in this session.
    NeverRead,
    /// The file changed on disk after the model last read it.
    ModifiedSinceRead,
}

impl WriteCheck {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }

    /// The message the model sees.
    ///
    /// Phrased as an instruction rather than a complaint, because the model's next
    /// action should be obvious from it.
    pub fn message(&self, path: &str) -> String {
        match self {
            Self::Ok => String::new(),
            Self::NeverRead => format!(
                "{path} exists but has not been read in this session. \
                 Read it first so the existing contents are not overwritten blindly."
            ),
            Self::ModifiedSinceRead => format!(
                "{path} has been modified since it was last read. \
                 Read it again before writing, or the changes made in the meantime \
                 will be lost."
            ),
        }
    }
}

impl FileTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `path` was read just now.
    ///
    /// The file's own mtime is stored rather than the wall clock. Using
    /// "now" would race: a write landing in the same second as the read would
    /// compare equal and the staleness check would miss it.
    pub fn record_read(&self, path: &Utf8Path) {
        let mtime = mtime_of(path).unwrap_or(SystemTime::UNIX_EPOCH);
        self.reads
            .lock()
            .expect("file tracker mutex")
            .insert(path.to_owned(), mtime);
    }

    /// Record a write we performed, so a subsequent edit is not called stale
    /// against our own change.
    pub fn record_write(&self, path: &Utf8Path) {
        self.record_read(path);
    }

    /// Whether writing to `path` is safe.
    ///
    /// A file that does not exist is always safe to write — there is nothing to
    /// clobber, and demanding a read of a file the model is creating would be
    /// nonsense.
    pub fn check_write(&self, path: &Utf8Path) -> WriteCheck {
        let Some(current_mtime) = mtime_of(path) else {
            return WriteCheck::Ok;
        };

        let reads = self.reads.lock().expect("file tracker mutex");
        let Some(read_mtime) = reads.get(path) else {
            return WriteCheck::NeverRead;
        };

        if current_mtime > *read_mtime {
            return WriteCheck::ModifiedSinceRead;
        }
        WriteCheck::Ok
    }

    pub fn has_read(&self, path: &Utf8Path) -> bool {
        self.reads.lock().expect("file tracker mutex").contains_key(path)
    }
}

fn mtime_of(path: &Utf8Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(
            std::fs::canonicalize(dir.path()).expect("canonicalize"),
        )
        .expect("utf8");
        (dir, path)
    }

    /// Force a strictly later mtime. Filesystem timestamp granularity is coarse
    /// enough on some platforms that two quick writes compare equal.
    fn touch_later(path: &Utf8Path, contents: &str) {
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn creating_a_new_file_needs_no_prior_read() {
        let (_guard, root) = workspace();
        let tracker = FileTracker::new();
        assert_eq!(tracker.check_write(&root.join("new.txt")), WriteCheck::Ok);
    }

    #[test]
    fn overwriting_an_unread_file_is_refused() {
        let (_guard, root) = workspace();
        let path = root.join("existing.txt");
        std::fs::write(&path, "important").unwrap();

        let tracker = FileTracker::new();
        assert_eq!(tracker.check_write(&path), WriteCheck::NeverRead);
    }

    #[test]
    fn reading_first_permits_the_write() {
        let (_guard, root) = workspace();
        let path = root.join("existing.txt");
        std::fs::write(&path, "important").unwrap();

        let tracker = FileTracker::new();
        tracker.record_read(&path);
        assert_eq!(tracker.check_write(&path), WriteCheck::Ok);
    }

    #[test]
    fn an_external_edit_after_the_read_is_caught() {
        let (_guard, root) = workspace();
        let path = root.join("existing.txt");
        std::fs::write(&path, "v1").unwrap();

        let tracker = FileTracker::new();
        tracker.record_read(&path);

        // The user edits it in their editor.
        touch_later(&path, "v2 — the user's work");

        assert_eq!(
            tracker.check_write(&path),
            WriteCheck::ModifiedSinceRead,
            "a write here would silently destroy the user's edit"
        );
    }

    #[test]
    fn our_own_write_does_not_make_the_next_edit_stale() {
        let (_guard, root) = workspace();
        let path = root.join("existing.txt");
        std::fs::write(&path, "v1").unwrap();

        let tracker = FileTracker::new();
        tracker.record_read(&path);
        touch_later(&path, "v2");
        tracker.record_write(&path);

        // A follow-up edit in the same turn must not be blocked by our own change.
        assert_eq!(tracker.check_write(&path), WriteCheck::Ok);
    }

    #[test]
    fn messages_tell_the_model_what_to_do_next() {
        assert!(WriteCheck::NeverRead.message("a.rs").contains("Read it first"));
        assert!(WriteCheck::ModifiedSinceRead.message("a.rs").contains("Read it again"));
        assert!(WriteCheck::Ok.message("a.rs").is_empty());
    }

    #[test]
    fn tracking_is_per_path() {
        let (_guard, root) = workspace();
        let a = root.join("a.txt");
        let b = root.join("b.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();

        let tracker = FileTracker::new();
        tracker.record_read(&a);

        assert!(tracker.has_read(&a));
        assert!(!tracker.has_read(&b));
        assert_eq!(tracker.check_write(&b), WriteCheck::NeverRead);
    }
}
