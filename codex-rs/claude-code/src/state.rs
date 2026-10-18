//! Per-thread files under the Codex-owned state directory.
//!
//! Nothing here contains credentials: only the Claude Code session id and history sync markers,
//! plus the system prompt passed to `--system-prompt-file`.

use serde::Deserialize;
use serde::Serialize;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use tracing::debug;

const SESSIONS_DIR: &str = "sessions";
const PROMPTS_DIR: &str = "system-prompts";

/// What is needed to resume a thread's Claude Code session after a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PersistedSession {
    pub(crate) session_id: String,
    pub(crate) synced_len: usize,
    pub(crate) prefix_hash: String,
    pub(crate) model: String,
}

/// Restricts a thread id to characters that are safe in a file name.
pub(crate) fn file_stem(thread_id: &str) -> String {
    let stem: String = thread_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if stem.is_empty() {
        "thread".to_string()
    } else {
        stem
    }
}

pub(crate) fn session_file(state_dir: &Path, thread_id: &str) -> PathBuf {
    state_dir
        .join(SESSIONS_DIR)
        .join(format!("{}.json", file_stem(thread_id)))
}

pub(crate) fn system_prompt_file(state_dir: &Path, thread_id: &str) -> PathBuf {
    state_dir
        .join(PROMPTS_DIR)
        .join(format!("{}.md", file_stem(thread_id)))
}

pub(crate) fn load_session(state_dir: &Path, thread_id: &str) -> Option<PersistedSession> {
    let path = session_file(state_dir, thread_id);
    let contents = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&contents) {
        Ok(session) => Some(session),
        Err(err) => {
            debug!(path = %path.display(), error = %err, "ignoring unreadable Claude Code session state");
            None
        }
    }
}

pub(crate) fn save_session(
    state_dir: &Path,
    thread_id: &str,
    session: &PersistedSession,
) -> std::io::Result<()> {
    let contents = serde_json::to_vec_pretty(session)?;
    write_private_file(&session_file(state_dir, thread_id), &contents)
}

/// Writes `contents` atomically with owner-only permissions, creating owner-only parents.
pub(crate) fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        create_private_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
    {
        let mut file = private_open_options().open(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(unix)]
fn private_open_options() -> std::fs::OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    options
}

#[cfg(not(unix))]
fn private_open_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    options
}

#[cfg(unix)]
fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

#[cfg(not(unix))]
fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
