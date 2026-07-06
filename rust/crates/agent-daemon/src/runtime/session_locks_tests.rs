use std::sync::atomic::Ordering;
use std::sync::{Arc, Barrier};
use std::time::Duration;

use tokio::sync::oneshot;

use super::*;

fn registry_len(registry: &SessionLockRegistry) -> usize {
    registry
        .inner
        .entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len()
}

fn owner_count(registry: &SessionLockRegistry, session_id: &str) -> usize {
    registry
        .inner
        .entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(session_id)
        .and_then(Weak::upgrade)
        .map_or(0, |entry| Arc::strong_count(&entry) - 1)
}

async fn wait_for_owner_count(registry: &SessionLockRegistry, session_id: &str, expected: usize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while owner_count(registry, session_id) != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("lock owner count should converge");
}

#[tokio::test]
async fn same_session_is_exclusive_and_try_acquire_is_nonblocking() {
    let registry = SessionLockRegistry::default();
    let first = registry.acquire("same".to_string()).await;

    assert!(registry.try_acquire("same".to_string()).is_none());

    let waiting_registry = registry.clone();
    let (acquired_tx, mut acquired_rx) = oneshot::channel();
    let waiter = tokio::spawn(async move {
        let guard = waiting_registry.acquire("same".to_string()).await;
        acquired_tx.send(()).expect("receiver remains");
        guard
    });
    wait_for_owner_count(&registry, "same", 2).await;
    assert!(acquired_rx.try_recv().is_err());

    drop(first);
    acquired_rx.await.expect("waiter acquires after release");
    drop(waiter.await.expect("waiter completes"));
    assert_eq!(registry_len(&registry), 0);
}

#[tokio::test]
async fn different_sessions_do_not_wait_for_each_other() {
    let registry = SessionLockRegistry::default();
    let first = registry.acquire("first".to_string()).await;

    let second = tokio::time::timeout(
        Duration::from_secs(1),
        registry.acquire("second".to_string()),
    )
    .await
    .expect("different session should acquire independently");

    drop((first, second));
    assert_eq!(registry_len(&registry), 0);
}

#[tokio::test]
async fn drop_allows_reacquire_and_removes_the_entry() {
    let registry = SessionLockRegistry::default();
    let first = registry.acquire("session".to_string()).await;
    assert_eq!(registry_len(&registry), 1);

    drop(first);
    assert_eq!(registry_len(&registry), 0);

    let second = registry
        .try_acquire("session".to_string())
        .expect("released session should be immediately acquirable");
    drop(second);
    assert_eq!(registry_len(&registry), 0);
}

#[tokio::test]
async fn cancelled_waiter_releases_its_entry_owner() {
    let registry = SessionLockRegistry::default();
    let first = registry.acquire("session".to_string()).await;
    let waiting_registry = registry.clone();
    let waiter = tokio::spawn(async move { waiting_registry.acquire("session".to_string()).await });
    wait_for_owner_count(&registry, "session", 2).await;

    waiter.abort();
    let result = waiter.await;
    assert!(result.is_err_and(|error| error.is_cancelled()));
    wait_for_owner_count(&registry, "session", 1).await;

    drop(first);
    assert_eq!(registry_len(&registry), 0);
}

#[tokio::test]
async fn panic_releases_the_guard_and_registry_entry() {
    let registry = SessionLockRegistry::default();
    let task_registry = registry.clone();
    let task = tokio::spawn(async move {
        let _guard = task_registry.acquire("session".to_string()).await;
        panic!("injected panic");
    });

    assert!(task.await.expect_err("task panics").is_panic());
    assert_eq!(registry_len(&registry), 0);
    assert!(registry.try_acquire("session".to_string()).is_some());
}

#[test]
fn stale_entry_drop_does_not_remove_a_replacement() {
    let registry = SessionLockRegistry::default();
    let old = registry.entry("session".to_string());
    let replacement = Arc::new(SessionLockEntry {
        registry: Arc::downgrade(&registry.inner),
        session_id: "session".to_string(),
        mutex: Arc::new(Mutex::new(())),
    });
    registry
        .inner
        .entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert("session".to_string(), Arc::downgrade(&replacement));
    let replacement_handle = SessionLockEntryHandle {
        entry: Some(replacement.clone()),
        drop_return_barrier: None,
    };

    drop(old);

    let stored = registry
        .inner
        .entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get("session")
        .and_then(Weak::upgrade)
        .expect("replacement remains registered");
    assert!(Arc::ptr_eq(&stored, &replacement));
    drop(stored);
    drop(replacement);
    drop(replacement_handle);
    assert_eq!(registry_len(&registry), 0);
}

#[test]
fn concurrent_final_handle_drops_remove_the_entry() {
    let registry = SessionLockRegistry::default();
    let mut first = registry.entry("session".to_string());
    let mut second = registry.entry("session".to_string());
    // Rendezvous before generated drop glue destroys fields. With the old
    // cleanup, both handles finish their map checks while both Arc fields are
    // still live, then return and leave a dead Weak behind.
    let drop_return_barrier = Arc::new(Barrier::new(2));
    first.drop_return_barrier = Some(drop_return_barrier.clone());
    second.drop_return_barrier = Some(drop_return_barrier);

    std::thread::scope(|scope| {
        scope.spawn(move || drop(first));
        scope.spawn(move || drop(second));
    });

    assert_eq!(registry_len(&registry), 0);
}

#[tokio::test]
async fn ten_thousand_live_keys_use_exact_operations_and_leave_no_stale_entries() {
    let registry = SessionLockRegistry::default();
    let mut guards = Vec::with_capacity(10_000);

    for index in 0..10_000 {
        guards.push(registry.acquire(format!("session-{index}")).await);
    }
    assert_eq!(registry_len(&registry), 10_000);
    assert_eq!(
        registry.inner.key_operations.load(Ordering::Relaxed),
        20_000
    );

    drop(guards);
    assert_eq!(
        registry.inner.key_operations.load(Ordering::Relaxed),
        40_000
    );
    assert_eq!(registry_len(&registry), 0);
}
