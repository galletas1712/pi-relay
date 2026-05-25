use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub timeout: Duration,
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            timeout: Duration::from_secs(30),
        }
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
