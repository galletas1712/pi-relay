use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Mutex, OwnedMutexGuard};

/// In-process per-path locks for file mutations (pi-mono `withFileMutationQueue`).
///
/// Same path is serialized; different paths stay concurrent. Bash does not
/// participate — the same gap pi-mono accepts.
#[derive(Debug, Default)]
pub struct FileMutationLocks {
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

/// Holds acquired per-path guards for the duration of a mutating tool call.
pub struct FileMutationGuard {
    _guards: Vec<OwnedMutexGuard<()>>,
}

impl FileMutationLocks {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Lock `paths` in sorted order to avoid deadlock across multi-file patches.
    pub async fn lock_paths(
        &self,
        paths: impl IntoIterator<Item = impl AsRef<Path>>,
    ) -> FileMutationGuard {
        let mut keys = paths
            .into_iter()
            .map(|path| mutation_lock_key(path.as_ref()))
            .collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        let mut guards = Vec::with_capacity(keys.len());
        for key in keys {
            let lock = {
                let mut map = self.locks.lock().await;
                map.retain(|_, lock| Arc::strong_count(lock) > 1);
                map.entry(key)
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone()
            };
            guards.push(lock.lock_owned().await);
        }
        FileMutationGuard { _guards: guards }
    }
}

/// Resolve a stable lock key: absolute-ish normalized path, with a realpath
/// attempt when the file already exists (matches pi-mono).
pub fn mutation_lock_key(path: &Path) -> PathBuf {
    let normalized = normalize_path(path);
    match std::fs::canonicalize(&normalized) {
        Ok(real) => real,
        Err(_) => normalized,
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn same_path_serializes_different_paths_stay_concurrent() {
        let locks = FileMutationLocks::new();
        let root = temp_dir();
        let a = root.join("a.txt");
        let b = root.join("b.txt");

        let guard_a = locks.lock_paths([&a]).await;

        let locks_same = Arc::clone(&locks);
        let a_same = a.clone();
        let same = tokio::spawn(async move { locks_same.lock_paths([&a_same]).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !same.is_finished(),
            "same path must wait while the first guard is held"
        );

        let other = locks.lock_paths([&b]).await;
        drop(other);

        drop(guard_a);
        same.await.expect("same-path waiter joins after release");
        std::fs::remove_dir_all(&root).ok();
    }

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("pi-file-mutation-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).expect("temp dir");
        path
    }
}
