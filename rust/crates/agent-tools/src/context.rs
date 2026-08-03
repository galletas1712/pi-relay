use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cap_std::fs::Dir;

#[derive(Debug, Clone)]
pub struct ToolContext {
    cwd: PathBuf,
    timeout: Duration,
    workspace_dir: Arc<Dir>,
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>, workspace_dir: Dir) -> Self {
        Self {
            cwd: cwd.into(),
            timeout: Duration::from_secs(30),
            workspace_dir: Arc::new(workspace_dir),
        }
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub(crate) fn workspace_dir(&self) -> Arc<Dir> {
        Arc::clone(&self.workspace_dir)
    }
}

pub(crate) fn workspace_path(ctx: &ToolContext, path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        ctx.cwd().join(path)
    }
}
