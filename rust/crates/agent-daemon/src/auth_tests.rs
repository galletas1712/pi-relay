use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tokio::sync::Notify;

use super::*;

fn credentials(codex_token: &str, account: Option<&str>, anthropic_key: &str) -> Credentials {
    Credentials {
        codex_access_token: Some(codex_token.to_string()),
        codex_account_id: account.map(str::to_string),
        codex_installation_id: Some("installation-secret".to_string()),
        anthropic_api_key: Some(anthropic_key.to_string()),
    }
}

#[tokio::test]
async fn blocked_codex_refresh_does_not_block_anthropic_reload_and_both_updates_publish() {
    let loads = Arc::new(AtomicUsize::new(0));
    let loader_loads = Arc::clone(&loads);
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut refreshed = credentials(
        "refreshed-codex-secret",
        Some("refreshed-account"),
        "stale-anthropic-secret",
    );
    refreshed.codex_installation_id = Some("refreshed-installation".to_string());
    let refresher = Arc::new(BlockingRefresher {
        calls: AtomicUsize::new(0),
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
        result: refreshed,
    });
    let manager = CredentialManager::for_tests_with(
        Arc::new(move || {
            let load = loader_loads.fetch_add(1, Ordering::Relaxed);
            credentials(
                "old-codex-secret",
                Some("old-account"),
                if load == 0 {
                    "old-anthropic-secret"
                } else {
                    "refreshed-anthropic-secret"
                },
            )
        }),
        refresher.clone(),
    );
    let observed = manager.snapshot();
    let refresh_manager = manager.clone();
    let refresh_observed = observed.clone();
    let codex_refresh =
        tokio::spawn(async move { refresh_manager.refresh_codex(&refresh_observed).await });
    entered.notified().await;

    let anthropic =
        tokio::time::timeout(Duration::from_secs(1), manager.reload_anthropic(&observed))
            .await
            .expect("Anthropic reload is not blocked by Codex refresh")
            .expect("Anthropic reload succeeds")
            .expect("Anthropic key changed");
    assert_eq!(
        (
            anthropic.generation(),
            anthropic.codex_generation(),
            anthropic.anthropic_generation(),
            anthropic.credentials.codex_access_token.as_deref(),
            anthropic.credentials.anthropic_api_key.as_deref(),
        ),
        (
            2,
            1,
            2,
            Some("old-codex-secret"),
            Some("refreshed-anthropic-secret"),
        )
    );

    release.notify_one();
    let codex = codex_refresh
        .await
        .expect("Codex refresh joins")
        .expect("Codex refresh succeeds");
    assert_eq!(
        (
            codex.generation(),
            codex.codex_generation(),
            codex.anthropic_generation(),
            codex.credentials.codex_access_token.as_deref(),
            codex.credentials.codex_account_id.as_deref(),
            codex.credentials.codex_installation_id.as_deref(),
            codex.credentials.anthropic_api_key.as_deref(),
        ),
        (
            3,
            2,
            2,
            Some("refreshed-codex-secret"),
            Some("refreshed-account"),
            Some("refreshed-installation"),
            Some("refreshed-anthropic-secret"),
        )
    );
    assert_eq!(refresher.calls.load(Ordering::Relaxed), 1);
    assert_eq!(loads.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn concurrent_anthropic_reloads_load_and_publish_once() {
    let loads = Arc::new(AtomicUsize::new(0));
    let loader_loads = Arc::clone(&loads);
    let manager = CredentialManager::for_tests_with(
        Arc::new(move || {
            let load = loader_loads.fetch_add(1, Ordering::Relaxed);
            credentials(
                "codex-secret",
                Some("account"),
                if load == 0 { "old-key" } else { "new-key" },
            )
        }),
        Arc::new(CountingRefresher {
            calls: AtomicUsize::new(0),
            result: Err("not called"),
        }),
    );
    let observed = manager.snapshot();
    let (first, second) = tokio::join!(
        manager.reload_anthropic(&observed),
        manager.reload_anthropic(&observed)
    );
    let first = first.expect("first reload succeeds").expect("key changed");
    let second = second
        .expect("second reload succeeds")
        .expect("key changed");

    assert_eq!(first.anthropic_generation(), 2);
    assert_eq!(second.anthropic_generation(), 2);
    assert!(Arc::ptr_eq(&first.credentials, &second.credentials));
    assert_eq!(loads.load(Ordering::Relaxed), 2);
}

struct BlockingRefresher {
    calls: AtomicUsize,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    result: Credentials,
}

#[async_trait]
impl CodexCredentialRefresher for BlockingRefresher {
    async fn refresh(&self, _prior: &Credentials) -> Result<Credentials> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.entered.notify_one();
        self.release.notified().await;
        Ok(self.result.clone())
    }
}

fn temp_home() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("pi-relay-auth-test-{nanos}"))
}

struct CountingRefresher {
    calls: AtomicUsize,
    result: std::result::Result<Credentials, &'static str>,
}

#[async_trait]
impl CodexCredentialRefresher for CountingRefresher {
    async fn refresh(&self, _prior: &Credentials) -> Result<Credentials> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.result.clone().map_err(|message| anyhow!(message))
    }
}

#[test]
fn startup_loads_once_and_hot_snapshots_share_backing_credentials() {
    let loads = Arc::new(AtomicUsize::new(0));
    let loader_loads = Arc::clone(&loads);
    let manager = CredentialManager::for_tests_with(
        Arc::new(move || {
            loader_loads.fetch_add(1, Ordering::Relaxed);
            credentials("codex-secret", Some("account-secret"), "anthropic-secret")
        }),
        Arc::new(CountingRefresher {
            calls: AtomicUsize::new(0),
            result: Err("not called"),
        }),
    );

    let first = manager.snapshot();
    for _ in 0..32 {
        let snapshot = manager.snapshot();
        assert_eq!(snapshot.generation(), first.generation());
        assert!(Arc::ptr_eq(&snapshot.credentials, &first.credentials));
    }
    assert_eq!(loads.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn successful_and_failed_refresh_publication_is_atomic_and_redacted() {
    let refreshed = credentials(
        "refreshed-codex-secret",
        Some("account-secret"),
        "anthropic-secret",
    );
    let manager = CredentialManager::for_tests_with(
        Arc::new(|| {
            credentials(
                "old-codex-secret",
                Some("account-secret"),
                "anthropic-secret",
            )
        }),
        Arc::new(CountingRefresher {
            calls: AtomicUsize::new(0),
            result: Ok(refreshed),
        }),
    );
    let before = manager.snapshot();
    let after = manager
        .refresh_codex(&before)
        .await
        .expect("refresh succeeds");

    assert_eq!(after.generation(), before.generation() + 1);
    assert!(!Arc::ptr_eq(&after.credentials, &before.credentials));
    assert_eq!(manager.snapshot().generation(), after.generation());
    let debug = format!("{manager:?} {after:?}");
    for secret in [
        "refreshed-codex-secret",
        "account-secret",
        "installation-secret",
        "anthropic-secret",
    ] {
        assert!(!debug.contains(secret));
    }

    let failure = CredentialManager::for_tests_with(
        Arc::new(|| credentials("stable-secret", Some("stable-account"), "stable-anthropic")),
        Arc::new(CountingRefresher {
            calls: AtomicUsize::new(0),
            result: Err("refresh failed"),
        }),
    );
    let before_failure = failure.snapshot();
    failure
        .refresh_codex(&before_failure)
        .await
        .expect_err("refresh fails");
    let after_failure = failure.snapshot();
    assert_eq!(after_failure.generation(), before_failure.generation());
    assert!(Arc::ptr_eq(
        &after_failure.credentials,
        &before_failure.credentials
    ));
}

#[test]
fn reads_claude_code_config_primary_api_key() {
    let home = temp_home();
    let claude_dir = home.join(".claude");
    std::fs::create_dir_all(&claude_dir).expect("create claude dir");
    std::fs::write(
        claude_dir.join("config.json"),
        r#"{"primaryApiKey":"sk-ant-test-config"}"#,
    )
    .expect("write config");

    let key = read_claude_code_config_api_key_from_home(&home);

    assert_eq!(key.as_deref(), Some("sk-ant-test-config"));
    std::fs::remove_dir_all(home).expect("remove temp home");
}

#[test]
fn falls_back_to_root_claude_json() {
    let home = temp_home();
    std::fs::create_dir_all(&home).expect("create temp home");
    std::fs::write(
        home.join(".claude.json"),
        r#"{"primaryApiKey":"sk-ant-test-root"}"#,
    )
    .expect("write claude json");

    let key = read_claude_code_config_api_key_from_home(&home);

    assert_eq!(key.as_deref(), Some("sk-ant-test-root"));
    std::fs::remove_dir_all(home).expect("remove temp home");
}

#[test]
fn ignores_non_anthropic_primary_key() {
    let home = temp_home();
    let claude_dir = home.join(".claude");
    std::fs::create_dir_all(&claude_dir).expect("create claude dir");
    std::fs::write(
        claude_dir.join("config.json"),
        r#"{"primaryApiKey":"not-an-anthropic-key"}"#,
    )
    .expect("write config");

    let key = read_claude_code_config_api_key_from_home(&home);

    assert_eq!(key, None);
    std::fs::remove_dir_all(home).expect("remove temp home");
}
