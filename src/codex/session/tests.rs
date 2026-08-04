use super::*;
use std::fs;
use tempfile::TempDir;

fn store(temp: &TempDir) -> PathBuf {
    temp.path().join("store/artifacts.db")
}

#[test]
fn records_and_recalls_the_intent_of_a_session() {
    let temp = TempDir::new().expect("temp");
    let store = store(&temp);
    fs::create_dir_all(store.parent().expect("parent")).expect("store directory");
    assert!(record(
        &store,
        "session-a",
        "  why does the budget argument fail to parse  "
    ));
    assert_eq!(
        recall(&store, "session-a").as_deref(),
        Some("why does the budget argument fail to parse")
    );
    assert_eq!(recall(&store, "session-b"), None);
}

#[test]
fn a_later_prompt_replaces_the_intent_of_the_same_session() {
    let temp = TempDir::new().expect("temp");
    let store = store(&temp);
    fs::create_dir_all(store.parent().expect("parent")).expect("store directory");
    assert!(record(&store, "session-a", "first question"));
    assert!(record(&store, "session-a", "second question"));
    assert_eq!(
        recall(&store, "session-a").as_deref(),
        Some("second question")
    );
}

#[test]
fn an_empty_or_whitespace_prompt_records_nothing() {
    let temp = TempDir::new().expect("temp");
    let store = store(&temp);
    fs::create_dir_all(store.parent().expect("parent")).expect("store directory");
    assert!(!record(&store, "session-a", "   \n\t "));
    assert_eq!(recall(&store, "session-a"), None);
}

#[test]
fn an_oversized_prompt_is_bounded_on_a_utf8_boundary() {
    let temp = TempDir::new().expect("temp");
    let store = store(&temp);
    fs::create_dir_all(store.parent().expect("parent")).expect("store directory");
    let prompt = format!("{}é", "a".repeat(MAX_FOCUS_BYTES - 1));
    assert!(record(&store, "session-a", &prompt));
    let recalled = recall(&store, "session-a").expect("recalled");
    assert_eq!(recalled.len(), MAX_FOCUS_BYTES - 1);
    assert!(recalled.is_char_boundary(recalled.len()));
}

#[test]
fn a_session_identifier_never_reaches_the_file_system() {
    let temp = TempDir::new().expect("temp");
    let store = store(&temp);
    fs::create_dir_all(store.parent().expect("parent")).expect("store directory");
    assert!(record(&store, "../../escape", "intent"));
    let directory = directory(&store).expect("focus directory");
    let entries: Vec<_> = fs::read_dir(&directory)
        .expect("entries")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].len() == 64 && entries[0].chars().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(recall(&store, "../../escape").as_deref(), Some("intent"));
}

#[test]
fn an_expired_record_yields_no_focus_and_is_reclaimed() {
    let temp = TempDir::new().expect("temp");
    let store = store(&temp);
    fs::create_dir_all(store.parent().expect("parent")).expect("store directory");
    assert!(record(&store, "session-a", "stale intent"));
    let directory = directory(&store).expect("focus directory");
    let path = directory.join(digest("session-a"));
    let stale = SystemTime::now() - SESSION_FOCUS_TTL - Duration::from_secs(60);
    let file = fs::File::options().write(true).open(&path).expect("open");
    file.set_modified(stale).expect("backdate");
    assert_eq!(recall(&store, "session-a"), None);
    assert!(record(&store, "session-b", "fresh intent"));
    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn the_record_and_its_directory_stay_private() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new().expect("temp");
    let store = store(&temp);
    fs::create_dir_all(store.parent().expect("parent")).expect("store directory");
    assert!(record(&store, "session-a", "intent"));
    let directory = directory(&store).expect("focus directory");
    assert_eq!(
        fs::metadata(&directory)
            .expect("directory")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(directory.join(digest("session-a")))
            .expect("record")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn an_unwritable_store_records_nothing_without_failing() {
    let temp = TempDir::new().expect("temp");
    let store = temp.path().join("missing/store/artifacts.db");
    assert!(!record(&store, "session-a", "intent"));
    assert_eq!(recall(&store, "session-a"), None);
}
