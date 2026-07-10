use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use pretty_assertions::assert_eq;

use super::*;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("pi-relay-oauth-store-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn credential(access_token: &str) -> StoredOAuthCredential {
    StoredOAuthCredential {
        server_id: "server".to_string(),
        server_url: "https://mcp.example.test/mcp?route=one".to_string(),
        configured_client_id: None,
        resource: Some("https://api.example.test".to_string()),
        client_id: "dynamic-client".to_string(),
        access_token: access_token.to_string(),
        refresh_token: Some("refresh-token".to_string()),
        expires_at_millis: Some(4_000_000_000_000),
        granted_scopes: vec!["read".to_string(), "write".to_string()],
    }
}

#[tokio::test]
async fn missing_file_is_empty_and_roundtrip_is_exact() {
    let temp = TempDir::new();
    let path = temp.path().join("credentials.json");
    let repository = OAuthCredentialRepository::open_file(path.clone()).expect("missing is empty");
    assert_eq!(
        repository
            .get("server", "https://mcp.example.test/mcp?route=one")
            .await
            .expect("store is available"),
        None
    );

    let expected = credential("access-token");
    repository
        .save(expected.clone())
        .await
        .expect("credential saves");
    drop(repository);

    let reopened = OAuthCredentialRepository::open_file(path).expect("credential file reopens");
    assert_eq!(
        reopened
            .get("server", "https://mcp.example.test/mcp?route=one")
            .await
            .expect("store is available"),
        Some(expected)
    );
}

#[tokio::test]
async fn replacement_is_atomic_and_temp_file_is_removed() {
    let temp = TempDir::new();
    let path = temp.path().join("credentials.json");
    let repository = OAuthCredentialRepository::open_file(path.clone()).expect("repository opens");
    repository
        .save(credential("first-access-token"))
        .await
        .expect("first save");
    repository
        .save(credential("second-access-token"))
        .await
        .expect("replacement save");

    assert_eq!(
        fs::read_dir(temp.path())
            .expect("read temp directory")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect::<Vec<_>>(),
        vec![path
            .file_name()
            .expect("credential file name")
            .to_os_string()]
    );
    let serialized = fs::read_to_string(path).expect("read credential file");
    assert!(serialized.contains("second-access-token"));
    assert!(!serialized.contains("first-access-token"));
}

#[tokio::test]
async fn more_than_sixty_four_bounded_credentials_roundtrip_and_accept_another_login() {
    let temp = TempDir::new();
    let path = temp.path().join("credentials.json");
    let repository = OAuthCredentialRepository::open_file(path.clone()).expect("repository opens");
    for index in 0..80 {
        let mut entry = credential(&format!("access-token-{index}"));
        entry.server_id = format!("server-{index}");
        entry.server_url = format!("https://mcp.example.test/mcp?route={index}");
        repository.save(entry).await.expect("credential saves");
    }
    let mut newest = credential("new-login-access-token");
    newest.server_id = "new-login".to_string();
    newest.server_url = "https://mcp.example.test/new-login".to_string();
    repository
        .save(newest.clone())
        .await
        .expect("new login saves after long history");
    drop(repository);

    let reopened = OAuthCredentialRepository::open_file(path).expect("credential file reopens");
    for index in 0..80 {
        assert!(reopened
            .get(
                &format!("server-{index}"),
                &format!("https://mcp.example.test/mcp?route={index}"),
            )
            .await
            .expect("store is available")
            .is_some());
    }
    assert_eq!(
        reopened
            .get("new-login", "https://mcp.example.test/new-login")
            .await
            .expect("store is available"),
        Some(newest)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn file_and_parent_permissions_are_restrictive() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new();
    let parent = temp.path().join("private");
    let path = parent.join("credentials.json");
    let repository = OAuthCredentialRepository::open_file(path.clone()).expect("repository opens");
    repository
        .save(credential("access-token"))
        .await
        .expect("credential saves");

    assert_eq!(
        fs::metadata(parent)
            .expect("parent metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(path)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn empty_corrupt_and_oversized_files_fail_with_sanitized_errors() {
    let temp = TempDir::new();
    let path = temp.path().join("credentials.json");

    fs::write(&path, []).expect("write empty");
    assert_eq!(
        OAuthCredentialRepository::open_file(path.clone())
            .err()
            .expect("empty fails"),
        OAuthCredentialStoreError::Empty
    );
    fs::write(&path, br#"{"credentials":"access-token"}"#).expect("write corrupt");
    let corrupt = OAuthCredentialRepository::open_file(path.clone())
        .err()
        .expect("corrupt fails");
    assert_eq!(corrupt, OAuthCredentialStoreError::Corrupt);
    assert!(!format!("{corrupt:?} {corrupt}").contains("access-token"));
    let file = fs::File::create(&path).expect("create oversized");
    file.set_len(MAX_FILE_BYTES + 1).expect("extend oversized");
    assert_eq!(
        OAuthCredentialRepository::open_file(path)
            .err()
            .expect("oversized fails"),
        OAuthCredentialStoreError::Oversized
    );
}

#[test]
fn credential_debug_redacts_all_sensitive_values() {
    let credential = credential("access-token");
    let debug = format!("{credential:?}");
    for sensitive in [
        "access-token",
        "refresh-token",
        "dynamic-client",
        "mcp.example.test",
        "api.example.test",
    ] {
        assert!(!debug.contains(sensitive), "{sensitive}");
    }
}
