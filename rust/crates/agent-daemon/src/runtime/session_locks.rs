use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, Weak};

use tokio::sync::{Mutex, OwnedMutexGuard};

#[derive(Clone, Default)]
pub(crate) struct SessionLockRegistry {
    inner: Arc<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    entries: StdMutex<HashMap<String, Weak<SessionLockEntry>>>,
    #[cfg(test)]
    key_operations: std::sync::atomic::AtomicUsize,
}

struct SessionLockEntry {
    registry: Weak<RegistryInner>,
    session_id: String,
    mutex: Arc<Mutex<()>>,
}

struct SessionLockEntryHandle {
    entry: Option<Arc<SessionLockEntry>>,
    #[cfg(test)]
    drop_return_barrier: Option<Arc<std::sync::Barrier>>,
}

#[cfg(test)]
struct DropReturnBarrier(Option<Arc<std::sync::Barrier>>);

pub(super) struct SessionLockGuard {
    guard: Option<OwnedMutexGuard<()>>,
    _entry: SessionLockEntryHandle,
}

impl SessionLockRegistry {
    pub(super) async fn acquire(&self, session_id: String) -> SessionLockGuard {
        let entry = self.entry(session_id);
        let guard = entry
            .entry
            .as_ref()
            .expect("session lock entry handle is live")
            .mutex
            .clone()
            .lock_owned()
            .await;
        SessionLockGuard {
            guard: Some(guard),
            _entry: entry,
        }
    }

    pub(super) fn try_acquire(&self, session_id: String) -> Option<SessionLockGuard> {
        let entry = self.entry(session_id);
        let guard = entry
            .entry
            .as_ref()
            .expect("session lock entry handle is live")
            .mutex
            .clone()
            .try_lock_owned()
            .ok()?;
        Some(SessionLockGuard {
            guard: Some(guard),
            _entry: entry,
        })
    }

    fn entry(&self, session_id: String) -> SessionLockEntryHandle {
        let mut entries = self
            .inner
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = entries.get(&session_id).and_then(Weak::upgrade);
        #[cfg(test)]
        self.inner
            .key_operations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let entry = entry.unwrap_or_else(|| {
            let entry = Arc::new(SessionLockEntry {
                registry: Arc::downgrade(&self.inner),
                session_id: session_id.clone(),
                mutex: Arc::new(Mutex::new(())),
            });
            entries.insert(session_id, Arc::downgrade(&entry));
            #[cfg(test)]
            self.inner
                .key_operations
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            entry
        });
        SessionLockEntryHandle {
            entry: Some(entry),
            #[cfg(test)]
            drop_return_barrier: None,
        }
    }
}

impl Drop for SessionLockEntryHandle {
    fn drop(&mut self) {
        #[cfg(test)]
        let _drop_return_barrier = DropReturnBarrier(self.drop_return_barrier.take());
        let live_entry = self
            .entry
            .as_ref()
            .expect("session lock entry handle is live");
        let Some(registry) = live_entry.registry.upgrade() else {
            return;
        };
        let session_id = live_entry.session_id.clone();
        let mut entries = registry
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = self
            .entry
            .take()
            .expect("session lock entry handle is dropped once");
        let weak_entry = Arc::downgrade(&entry);
        drop(entry);
        if weak_entry.strong_count() == 0 {
            let matches = entries
                .get(&session_id)
                .is_some_and(|entry| entry.ptr_eq(&weak_entry));
            #[cfg(test)]
            registry
                .key_operations
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if matches {
                entries.remove(&session_id);
                #[cfg(test)]
                registry
                    .key_operations
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        drop(entries);
    }
}

#[cfg(test)]
impl Drop for DropReturnBarrier {
    fn drop(&mut self) {
        if let Some(barrier) = self.0.take() {
            barrier.wait();
        }
    }
}

impl Drop for SessionLockGuard {
    fn drop(&mut self) {
        drop(self.guard.take());
    }
}

#[cfg(test)]
#[path = "session_locks_tests.rs"]
mod tests;
