use super::PersistedSession;
use super::file_stem;
use super::load_session;
use super::save_session;
use super::session_file;
use pretty_assertions::assert_eq;

#[test]
fn file_stem_sanitizes_thread_ids() {
    assert_eq!(file_stem("0199-abc_DEF"), "0199-abc_DEF");
    assert_eq!(file_stem("../etc/passwd"), "___etc_passwd");
    assert_eq!(file_stem(""), "thread");
}

#[test]
fn session_round_trips_with_private_permissions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session = PersistedSession {
        session_id: "6f2c1c56-8f0e-4d55-9a53-3f4fd1e0f0aa".to_string(),
        synced_len: 7,
        prefix_hash: "abc".to_string(),
        model: "claude-sonnet-5".to_string(),
    };
    save_session(dir.path(), "thread-1", &session).expect("save");
    assert_eq!(load_session(dir.path(), "thread-1"), Some(session));
    assert_eq!(load_session(dir.path(), "thread-2"), None);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(session_file(dir.path(), "thread-1"))
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
