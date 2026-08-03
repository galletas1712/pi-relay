//! Interest-scoped filesystem watching for the session files browser.
//!
//! Only directories and files in the current interest set are watched. Directory
//! interest refreshes name/presence listings; file interest refreshes open-file
//! contents. Events are coalesced before crossing the runtime→control boundary.

use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use super::fs::validate_browse_path;

const DEBOUNCE: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrowseInterest {
    pub directories: BTreeSet<String>,
    pub files: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowseFsDelta {
    pub directories: Vec<String>,
    pub files: Vec<String>,
}

pub struct BrowseWatchHub {
    inner: Arc<Mutex<HashMap<String, WorkspaceWatch>>>,
    events_tx: mpsc::UnboundedSender<(String, BrowseFsDelta)>,
}

struct WorkspaceWatch {
    cwd: PathBuf,
    interest: BrowseInterest,
    watcher: RecommendedWatcher,
    /// Absolute paths currently registered with `watcher`.
    watched_abs: BTreeSet<PathBuf>,
}

impl BrowseWatchHub {
    pub fn new(events_tx: mpsc::UnboundedSender<(String, BrowseFsDelta)>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            events_tx,
        }
    }

    pub fn set_interest(
        &self,
        workspace_id: &str,
        cwd: PathBuf,
        directories: Vec<String>,
        files: Vec<String>,
    ) -> Result<()> {
        let interest = normalize_interest(directories, files)?;
        let mut guard = self.inner.lock().expect("browse watch lock");
        if interest.directories.is_empty() && interest.files.is_empty() {
            if let Some(mut entry) = guard.remove(workspace_id) {
                clear_watches(&mut entry);
            }
            return Ok(());
        }

        if let Some(entry) = guard.get_mut(workspace_id) {
            entry.cwd = cwd;
            entry.interest = interest;
            sync_watches(entry)?;
            return Ok(());
        }

        let events_tx = self.events_tx.clone();
        let workspace_key = workspace_id.to_string();
        let hub = Arc::clone(&self.inner);
        let watcher = RecommendedWatcher::new(
            move |result: notify::Result<notify::Event>| {
                let Ok(event) = result else { return };
                let Ok(guard) = hub.lock() else { return };
                let Some(entry) = guard.get(&workspace_key) else {
                    return;
                };
                let mut dirs = BTreeSet::new();
                let mut files = BTreeSet::new();
                classify_event(&entry.cwd, &entry.interest, &event, &mut dirs, &mut files);
                if dirs.is_empty() && files.is_empty() {
                    return;
                }
                let _ = events_tx.send((
                    workspace_key.clone(),
                    BrowseFsDelta {
                        directories: dirs.into_iter().collect(),
                        files: files.into_iter().collect(),
                    },
                ));
            },
            notify::Config::default(),
        )
        .context("create filesystem watcher")?;

        let mut entry = WorkspaceWatch {
            cwd,
            interest,
            watcher,
            watched_abs: BTreeSet::new(),
        };
        sync_watches(&mut entry)?;
        guard.insert(workspace_id.to_string(), entry);
        Ok(())
    }

    pub fn stop_workspace(&self, workspace_id: &str) {
        let mut guard = self.inner.lock().expect("browse watch lock");
        if let Some(mut entry) = guard.remove(workspace_id) {
            clear_watches(&mut entry);
        }
    }
}

/// Coalesce rapid watcher bursts into one delta per workspace.
pub async fn coalesce_watch_events(
    mut rx: mpsc::UnboundedReceiver<(String, BrowseFsDelta)>,
    out: mpsc::Sender<(String, BrowseFsDelta)>,
) {
    while let Some((workspace_id, delta)) = rx.recv().await {
        let mut pending: HashMap<String, BrowseFsDelta> = HashMap::new();
        pending.insert(workspace_id, delta);
        let deadline = tokio::time::Instant::now() + DEBOUNCE;
        loop {
            tokio::select! {
                next = rx.recv() => {
                    let Some((workspace_id, delta)) = next else {
                        flush_pending(&mut pending, &out).await;
                        return;
                    };
                    let entry = pending.entry(workspace_id).or_insert_with(|| BrowseFsDelta {
                        directories: Vec::new(),
                        files: Vec::new(),
                    });
                    merge_delta(entry, delta);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    flush_pending(&mut pending, &out).await;
                    break;
                }
            }
        }
    }
}

async fn flush_pending(
    pending: &mut HashMap<String, BrowseFsDelta>,
    out: &mpsc::Sender<(String, BrowseFsDelta)>,
) {
    for (workspace_id, delta) in pending.drain() {
        if delta.directories.is_empty() && delta.files.is_empty() {
            continue;
        }
        if out.send((workspace_id, delta)).await.is_err() {
            return;
        }
    }
}

fn normalize_interest(directories: Vec<String>, files: Vec<String>) -> Result<BrowseInterest> {
    let mut interest = BrowseInterest::default();
    for path in directories {
        interest.directories.insert(validate_browse_path(&path)?);
    }
    for path in files {
        let normalized = validate_browse_path(&path)?;
        if normalized.is_empty() {
            anyhow::bail!("watched file path must not be empty");
        }
        interest.files.insert(normalized);
    }
    Ok(interest)
}

fn watch_targets(cwd: &Path, interest: &BrowseInterest) -> BTreeSet<PathBuf> {
    let mut targets = BTreeSet::new();
    for dir in &interest.directories {
        let abs = if dir.is_empty() {
            cwd.to_path_buf()
        } else {
            cwd.join(dir)
        };
        targets.insert(abs);
    }
    for file in &interest.files {
        let abs = cwd.join(file);
        if let Some(parent) = abs.parent() {
            targets.insert(parent.to_path_buf());
        }
    }
    targets
}

fn sync_watches(entry: &mut WorkspaceWatch) -> Result<()> {
    let desired = watch_targets(&entry.cwd, &entry.interest);
    let to_remove: Vec<_> = entry.watched_abs.difference(&desired).cloned().collect();
    for path in to_remove {
        let _ = entry.watcher.unwatch(&path);
        entry.watched_abs.remove(&path);
    }
    let to_add: Vec<_> = desired.difference(&entry.watched_abs).cloned().collect();
    for path in to_add {
        if !path.is_dir() {
            // Parent may not exist yet; skip until a later interest refresh.
            continue;
        }
        entry
            .watcher
            .watch(&path, RecursiveMode::NonRecursive)
            .with_context(|| format!("watch {}", path.display()))?;
        entry.watched_abs.insert(path);
    }
    Ok(())
}

fn clear_watches(entry: &mut WorkspaceWatch) {
    let watched: Vec<_> = entry.watched_abs.iter().cloned().collect();
    entry.watched_abs.clear();
    for path in watched {
        let _ = entry.watcher.unwatch(&path);
    }
}

fn classify_event(
    cwd: &Path,
    interest: &BrowseInterest,
    event: &notify::Event,
    dirs: &mut BTreeSet<String>,
    files: &mut BTreeSet<String>,
) {
    if matches!(event.kind, EventKind::Access(_)) {
        return;
    }
    for path in &event.paths {
        let Ok(rel) = path.strip_prefix(cwd) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() {
            if interest.directories.contains("") {
                dirs.insert(String::new());
            }
            continue;
        }
        if interest.files.contains(&rel) {
            files.insert(rel.clone());
        }
        let parent = match rel.rfind('/') {
            Some(idx) => rel[..idx].to_string(),
            None => String::new(),
        };
        if interest.directories.contains(&parent) {
            dirs.insert(parent);
        }
        // A watched directory node itself changed.
        if interest.directories.contains(&rel) {
            dirs.insert(rel);
        }
    }
}

fn merge_delta(into: &mut BrowseFsDelta, from: BrowseFsDelta) {
    let mut dirs: BTreeSet<_> = into.directories.drain(..).collect();
    let mut files: BTreeSet<_> = into.files.drain(..).collect();
    dirs.extend(from.directories);
    files.extend(from.files);
    into.directories = dirs.into_iter().collect();
    into.files = files.into_iter().collect();
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::Event;

    #[test]
    fn classifies_child_create_against_parent_interest() {
        let cwd = PathBuf::from("/tmp/cwd");
        let interest = BrowseInterest {
            directories: BTreeSet::from([String::new(), "src".into()]),
            files: BTreeSet::from(["src/main.rs".into()]),
        };
        let mut dirs = BTreeSet::new();
        let mut files = BTreeSet::new();
        classify_event(
            &cwd,
            &interest,
            &Event {
                kind: EventKind::Create(notify::event::CreateKind::File),
                paths: vec![cwd.join("src/lib.rs")],
                attrs: Default::default(),
            },
            &mut dirs,
            &mut files,
        );
        assert_eq!(dirs, BTreeSet::from(["src".into()]));
        assert!(files.is_empty());
    }

    #[test]
    fn classifies_open_file_modify() {
        let cwd = PathBuf::from("/tmp/cwd");
        let interest = BrowseInterest {
            directories: BTreeSet::new(),
            files: BTreeSet::from(["README.md".into()]),
        };
        let mut dirs = BTreeSet::new();
        let mut files = BTreeSet::new();
        classify_event(
            &cwd,
            &interest,
            &Event {
                kind: EventKind::Modify(notify::event::ModifyKind::Data(
                    notify::event::DataChange::Any,
                )),
                paths: vec![cwd.join("README.md")],
                attrs: Default::default(),
            },
            &mut dirs,
            &mut files,
        );
        assert!(dirs.is_empty());
        assert_eq!(files, BTreeSet::from(["README.md".into()]));
    }
}
