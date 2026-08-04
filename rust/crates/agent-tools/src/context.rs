use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::file_mutation::FileMutationLocks;

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub timeout: Duration,
    /// Shared with the runtime so Edit/Write serialize per path across tools.
    pub file_locks: Arc<FileMutationLocks>,
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            timeout: Duration::from_secs(30),
            file_locks: FileMutationLocks::new(),
        }
    }

    pub fn with_file_locks(mut self, file_locks: Arc<FileMutationLocks>) -> Self {
        self.file_locks = file_locks;
        self
    }
}

pub(crate) fn workspace_path(ctx: &ToolContext, path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        ctx.cwd.join(path)
    }
}
