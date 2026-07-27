//! Recording a session so it can be resumed.
//!
//! One append-only JSONL file per session, under `~/.octane/sessions/`. The
//! first line is [`SessionMeta`]; every line after it is one
//! [`Message`](octane_protocol::Message), in the order it entered the
//! conversation.
//!
//! ```text
//! ~/.octane/sessions/2026-07-27T18-04-11-ses_01J....jsonl
//! ```
//!
//! # Why append-only, and why JSONL
//!
//! The file is written as the conversation happens, never rewritten. A session
//! that dies to a panic, a closed terminal, or a machine losing power leaves a
//! file that is still valid up to its last complete line — which is the whole
//! reason to persist at all. Rewriting a single document on each turn would put
//! the crash exactly where the data is.
//!
//! It also means a truncated final line is *expected*, not corruption: the
//! process was killed mid-write. [`read`] stops at the first line it cannot
//! parse rather than failing the whole file, so the recoverable prefix is
//! recovered. Codex reaches the same conclusion for the same reason
//! (`codex-rs/rollout`), and stores broadly the same thing.
//!
//! # What is deliberately not here
//!
//! No index, no database, no compaction. Listing sessions reads the directory
//! and parses one line per file, which is fine for the thousands of sessions a
//! person accumulates and would need rethinking at a million. Codex has an
//! SQLite index for exactly that reason; octane will need one when someone can
//! demonstrate the directory walk is slow, and not before.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::io::{BufRead, BufReader, Write};

use camino::{Utf8Path, Utf8PathBuf};
use octane_protocol::{Message, SessionId};
use serde::{Deserialize, Serialize};

/// Directory under a config root that holds recorded sessions.
pub const SESSIONS_DIR: &str = "sessions";

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{path}: not a session file: {reason}")]
    Malformed { path: String, reason: String },

    #[error("no session found matching {0:?}")]
    NotFound(String),
}

/// The first line of a session file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    /// RFC 3339, so the file sorts by name and reads without tooling.
    pub timestamp: String,
    /// Where the session ran. Resuming into a different tree is usually a
    /// mistake, so the value is kept in order to be able to say so.
    pub cwd: Utf8PathBuf,
    /// Model reference at the time, for display in a session list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub cli_version: String,
}

/// A recorded session, as listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub meta: SessionMeta,
    pub path: Utf8PathBuf,
    /// Number of messages recorded, for telling a real session from a stray
    /// one someone opened and closed.
    pub messages: usize,
    /// First thing the user actually asked, as a one-line label.
    pub preview: Option<String>,
}

/// An open session file, appended to as the conversation grows.
#[derive(Debug)]
pub struct Recorder {
    file: std::fs::File,
    path: Utf8PathBuf,
    /// How many messages have already been written, so a caller holding the
    /// whole history can append only what is new.
    written: usize,
}

impl Recorder {
    /// Start recording under `dir`, creating it if needed.
    pub fn create(dir: &Utf8Path, meta: &SessionMeta) -> Result<Self, SessionError> {
        std::fs::create_dir_all(dir).map_err(|source| SessionError::Io {
            path: dir.to_string(),
            source,
        })?;

        // Timestamp first so the directory sorts chronologically by name, and
        // the id after it so two sessions starting in the same second cannot
        // collide.
        let stamp = meta.timestamp.replace([':', '.'], "-");
        let path = dir.join(format!("{stamp}-{}.jsonl", meta.id));

        let mut file = std::fs::File::create(&path).map_err(|source| SessionError::Io {
            path: path.to_string(),
            source,
        })?;
        writeln_json(&mut file, &path, meta)?;

        Ok(Self { file, path, written: 0 })
    }

    /// Append every message in `history` that has not been written yet.
    ///
    /// Takes the whole history rather than one message because that is what the
    /// caller has: the turn runner hands back a conversation, not a delta.
    pub fn record(&mut self, history: &[Message]) -> Result<(), SessionError> {
        // A compaction replaces the history with a shorter one. Recording the
        // replacement as an append would interleave two conversations, so the
        // count is reset and the new tail written; the file keeps the original
        // messages above it, which is what makes the record an audit trail
        // rather than a mirror of memory.
        if history.len() < self.written {
            self.written = 0;
        }
        for message in &history[self.written..] {
            writeln_json(&mut self.file, &self.path, message)?;
        }
        self.written = history.len();
        Ok(())
    }

    pub fn path(&self) -> &Utf8Path {
        &self.path
    }
}

fn writeln_json<T: Serialize>(
    file: &mut std::fs::File,
    path: &Utf8Path,
    value: &T,
) -> Result<(), SessionError> {
    let mut line = serde_json::to_string(value).map_err(|source| SessionError::Malformed {
        path: path.to_string(),
        reason: source.to_string(),
    })?;
    line.push('\n');
    file.write_all(line.as_bytes()).map_err(|source| SessionError::Io {
        path: path.to_string(),
        source,
    })?;
    // Flushed per line, so a session killed between turns keeps everything up
    // to the last one. Buffering would lose exactly the tail worth having.
    file.flush().map_err(|source| SessionError::Io {
        path: path.to_string(),
        source,
    })
}

/// Read a recorded session back.
///
/// A trailing line that does not parse is dropped, not an error: the process
/// was killed mid-write, and the messages before it are still good.
pub fn read(path: &Utf8Path) -> Result<(SessionMeta, Vec<Message>), SessionError> {
    let file = std::fs::File::open(path).map_err(|source| SessionError::Io {
        path: path.to_string(),
        source,
    })?;
    let mut lines = BufReader::new(file).lines();

    let first = lines
        .next()
        .transpose()
        .map_err(|source| SessionError::Io { path: path.to_string(), source })?
        .ok_or_else(|| SessionError::Malformed {
            path: path.to_string(),
            reason: "the file is empty".into(),
        })?;
    let meta: SessionMeta =
        serde_json::from_str(&first).map_err(|source| SessionError::Malformed {
            path: path.to_string(),
            reason: format!("first line is not session metadata: {source}"),
        })?;

    let mut messages = Vec::new();
    for line in lines {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str(&line) else { break };
        messages.push(message);
    }

    Ok((meta, messages))
}

/// Every session under `dir`, newest first.
pub fn list(dir: &Utf8Path) -> Vec<SessionSummary> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut sessions: Vec<SessionSummary> = entries
        .flatten()
        .filter_map(|entry| Utf8PathBuf::from_path_buf(entry.path()).ok())
        .filter(|path| path.extension() == Some("jsonl"))
        .filter_map(|path| {
            let (meta, messages) = read(&path).ok()?;
            let preview = messages
                .iter()
                .find(|message| message.role == octane_protocol::Role::User)
                .map(|message| {
                    let text = message.text_content().replace('\n', " ");
                    text.chars().take(80).collect()
                });
            Some(SessionSummary { meta, path, messages: messages.len(), preview })
        })
        .collect();

    // By recorded timestamp rather than file mtime: mtime moves when a file is
    // copied or restored from a backup, and the question being asked is "which
    // conversation was most recent", not "which file was touched last".
    sessions.sort_by(|a, b| b.meta.timestamp.cmp(&a.meta.timestamp));
    sessions
}

/// Find one session by id, or by a unique prefix of it.
///
/// A prefix, because the ids are long and nobody types one in full.
pub fn find(dir: &Utf8Path, id: &str) -> Result<SessionSummary, SessionError> {
    let mut matches: Vec<SessionSummary> = list(dir)
        .into_iter()
        .filter(|session| session.meta.id.as_str().starts_with(id))
        .collect();

    match matches.len() {
        0 => Err(SessionError::NotFound(id.to_string())),
        1 => Ok(matches.remove(0)),
        // Refused rather than resolved to the newest: resuming the wrong
        // conversation is silent, and the user is one character away from
        // saying which they meant.
        _ => Err(SessionError::Malformed {
            path: dir.to_string(),
            reason: format!("{id:?} matches {} sessions; give more of the id", matches.len()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octane_protocol::Role;

    fn temp() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        (dir, path)
    }

    fn meta() -> SessionMeta {
        SessionMeta {
            id: SessionId::new(),
            timestamp: "2026-07-27T18:04:11Z".into(),
            cwd: "/repo".into(),
            model: Some("anthropic/sonnet".into()),
            cli_version: "0.0.0".into(),
        }
    }

    #[test]
    fn a_recorded_session_reads_back_as_it_was_written() {
        let (_guard, dir) = temp();
        let meta = meta();
        let history = vec![
            Message::text(Role::User, "hello"),
            Message::text(Role::Assistant, "hi"),
        ];

        let mut recorder = Recorder::create(&dir, &meta).unwrap();
        recorder.record(&history).unwrap();

        let (read_meta, read_history) = read(recorder.path()).unwrap();
        assert_eq!(read_meta, meta);
        assert_eq!(read_history, history);
    }

    /// The caller holds a growing history and hands the whole thing over each
    /// turn. Re-appending what was already written would duplicate every
    /// message in the file, and a resumed session would say everything twice.
    #[test]
    fn appending_a_grown_history_writes_only_what_is_new() {
        let (_guard, dir) = temp();
        let mut recorder = Recorder::create(&dir, &meta()).unwrap();

        let mut history = vec![Message::text(Role::User, "one")];
        recorder.record(&history).unwrap();
        history.push(Message::text(Role::Assistant, "two"));
        recorder.record(&history).unwrap();
        recorder.record(&history).unwrap(); // a turn that added nothing

        let (_, read_history) = read(recorder.path()).unwrap();
        assert_eq!(read_history, history);
    }

    /// The reason for append-only. A process killed mid-write leaves a partial
    /// final line, and everything before it is still a usable conversation.
    #[test]
    fn a_truncated_final_line_costs_only_that_line() {
        let (_guard, dir) = temp();
        let mut recorder = Recorder::create(&dir, &meta()).unwrap();
        recorder.record(&[Message::text(Role::User, "kept")]).unwrap();

        let path = recorder.path().to_owned();
        drop(recorder);
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{\"role\":\"assis");  // killed mid-write
        std::fs::write(&path, raw).unwrap();

        let (_, history) = read(&path).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].text_content(), "kept");
    }

    /// Compaction replaces the history with a shorter one. Treating that as an
    /// append would slice from a stale offset and interleave two conversations.
    #[test]
    fn a_history_that_shrank_is_recorded_from_the_start_again() {
        let (_guard, dir) = temp();
        let mut recorder = Recorder::create(&dir, &meta()).unwrap();

        recorder
            .record(&[
                Message::text(Role::User, "one"),
                Message::text(Role::Assistant, "two"),
                Message::text(Role::User, "three"),
            ])
            .unwrap();
        // What compaction hands back: shorter, and not a prefix of the old one.
        recorder.record(&[Message::text(Role::Developer, "summary")]).unwrap();

        let (_, history) = read(recorder.path()).unwrap();
        assert_eq!(history.len(), 4, "the original messages stay as an audit trail");
        assert_eq!(history[3].text_content(), "summary");
    }

    #[test]
    fn sessions_are_listed_newest_first_with_a_preview() {
        let (_guard, dir) = temp();
        for (index, stamp) in ["2026-07-01T00:00:00Z", "2026-07-27T00:00:00Z"].iter().enumerate() {
            let meta = SessionMeta { timestamp: (*stamp).to_string(), ..meta() };
            let mut recorder = Recorder::create(&dir, &meta).unwrap();
            recorder
                .record(&[Message::text(Role::User, format!("question {index}"))])
                .unwrap();
        }

        let sessions = list(&dir);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].meta.timestamp, "2026-07-27T00:00:00Z");
        assert_eq!(sessions[0].preview.as_deref(), Some("question 1"));
        assert_eq!(sessions[0].messages, 1);
    }

    /// Resuming the wrong conversation is silent, so an ambiguous prefix is
    /// refused rather than resolved to the newest match.
    #[test]
    fn an_ambiguous_id_prefix_is_refused_rather_than_guessed() {
        let (_guard, dir) = temp();
        let first = meta();
        let second = meta();
        Recorder::create(&dir, &first).unwrap();
        Recorder::create(&dir, &second).unwrap();

        assert!(find(&dir, first.id.as_str()).is_ok(), "a full id is unambiguous");
        // Every id shares the type prefix, so that alone matches both.
        assert!(find(&dir, "ses").is_err());
        assert!(find(&dir, "nothing-like-this").is_err());
    }
}
