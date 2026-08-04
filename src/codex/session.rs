//! Session intent captured from the Codex `UserPromptSubmit` event.
//!
//! `PostToolUse` is the one event that carries an observation, and the one
//! event that cannot say why it exists: its `tool_input` names the command, not
//! the question. `UserPromptSubmit` carries the question and no observation.
//! Bridging them is what gives the hook a focus that states intent, and the
//! qualification measured the difference: on the same corpus, a focus derived
//! from the command line retains 17 answer lines of 26, an intent focus 25.
//!
//! Nothing here may fail a projection. A missing, unreadable, or expired record
//! yields no focus, and the hook proceeds exactly as it did before.
//!
//! The stored prompt is untrusted inert data. It is bounded to the contract's
//! focus limit, never evaluated, never logged, and written beside the artifact
//! store under the same private permissions the store itself requires.
use distill::MAX_FOCUS_BYTES;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// Session intent older than this is a different task. The window is long
/// enough to span one working session and short enough that a stale prompt
/// cannot silently steer an unrelated observation.
const SESSION_FOCUS_TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// The number of expired records one write may reclaim. Cleanup is opportunistic
/// so a session directory cannot grow without bound, and bounded so a hook
/// invocation cannot turn into a directory walk.
const CLEANUP_BUDGET: usize = 64;

/// Records the intent of a turn, replacing whatever the session recorded before.
///
/// Returns whether the record was written. The caller reports that as a
/// diagnostic and never as a failure.
pub(super) fn record(store_path: &Path, session_id: &str, prompt: &str) -> bool {
    let Some(bounded) = bounded_prompt(prompt) else {
        return false;
    };
    let Some(directory) = directory(store_path) else {
        return false;
    };
    if create_private_directory(&directory).is_err() {
        return false;
    }
    cleanup(&directory);
    let path = directory.join(digest(session_id));
    write_private(&path, bounded.as_bytes()).is_ok()
}

/// Recalls the intent of the current session, if a live record exists.
pub(super) fn recall(store_path: &Path, session_id: &str) -> Option<String> {
    let path = directory(store_path)?.join(digest(session_id));
    if expired(&path) {
        return None;
    }
    let bytes = fs::read(&path).ok()?;
    bounded_prompt(std::str::from_utf8(&bytes).ok()?)
}

fn directory(store_path: &Path) -> Option<PathBuf> {
    store_path.parent().map(|parent| parent.join("focus"))
}

/// The session identifier is host-supplied text, so it never reaches the file
/// system: its digest does. Traversal, absolute paths, and separators cannot
/// survive hex encoding.
fn digest(session_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_id.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn bounded_prompt(prompt: &str) -> Option<String> {
    let mut bounded = prompt.trim();
    if bounded.len() > MAX_FOCUS_BYTES {
        let mut boundary = MAX_FOCUS_BYTES;
        while boundary > 0 && !bounded.is_char_boundary(boundary) {
            boundary -= 1;
        }
        bounded = &bounded[..boundary];
    }
    (!bounded.is_empty()).then(|| bounded.to_owned())
}

fn expired(path: &Path) -> bool {
    let Ok(modified) = fs::metadata(path).and_then(|metadata| metadata.modified()) else {
        return true;
    };
    SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age > SESSION_FOCUS_TTL)
}

fn cleanup(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten().take(CLEANUP_BUDGET) {
        let path = entry.path();
        if expired(&path) {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(unix)]
fn create_private_directory(directory: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match fs::DirBuilder::new().mode(0o700).create(directory) {
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

#[cfg(not(unix))]
fn create_private_directory(directory: &Path) -> std::io::Result<()> {
    match fs::create_dir(directory) {
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

/// Writes through a temporary file so a concurrent reader observes either the
/// previous record or the new one, never a torn prompt.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension("tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)
}

#[cfg(test)]
#[path = "session/tests.rs"]
mod tests;
