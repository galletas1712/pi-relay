use std::path::{Path, PathBuf};

use agent_store::{SessionConfig, WorkspaceMount};
use anyhow::{anyhow, bail, Context, Result};
use tokio::process::Command;

#[derive(Clone)]
pub(crate) struct OverlayManager {
    state_root: PathBuf,
}

impl OverlayManager {
    pub(crate) fn from_default_state_dir() -> Result<Self> {
        let state_home = match std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty())
        {
            Some(value) => PathBuf::from(value),
            None => {
                let home = std::env::var_os("HOME")
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| anyhow!("HOME is required when XDG_STATE_HOME is unset"))?;
                PathBuf::from(home).join(".local/state")
            }
        };
        Ok(Self {
            state_root: state_home.join("pi-relay"),
        })
    }

    pub(crate) fn session_cwd(&self, session_id: &str) -> String {
        self.session_root(session_id)
            .join("cwd")
            .to_string_lossy()
            .into_owned()
    }

    pub(crate) async fn ensure_session(
        &self,
        session_id: &str,
        config: &SessionConfig,
    ) -> Result<()> {
        let root = self.session_root(session_id);
        let cwd = PathBuf::from(&config.outer_cwd);
        if cwd != root.join("cwd") {
            bail!(
                "session outer_cwd does not match overlay state root: expected {}, got {}",
                root.join("cwd").display(),
                cwd.display()
            );
        }
        tokio::fs::create_dir_all(root.join("overlays")).await?;
        tokio::fs::create_dir_all(&cwd).await?;
        for workspace in &config.workspaces {
            self.ensure_workspace_overlay(&root, &cwd, workspace)
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn remove_session_dir(&self, session_id: &str) -> Result<()> {
        let root = self.session_root(session_id);
        if !root.exists() {
            return Ok(());
        }
        for mountpoint in mounted_paths_under(&root)?.into_iter().rev() {
            let status = Command::new("fusermount3")
                .arg("-u")
                .arg(&mountpoint)
                .status()
                .await
                .with_context(|| format!("unmount {}", mountpoint.display()))?;
            if !status.success() {
                bail!("failed to unmount {}", mountpoint.display());
            }
        }
        tokio::fs::remove_dir_all(&root).await?;
        Ok(())
    }

    fn session_root(&self, session_id: &str) -> PathBuf {
        self.state_root
            .join("sessions")
            .join(path_component(session_id))
    }

    async fn ensure_workspace_overlay(
        &self,
        root: &Path,
        cwd: &Path,
        workspace: &WorkspaceMount,
    ) -> Result<()> {
        validate_mount_dir(&workspace.mount_dir)?;
        let source = PathBuf::from(&workspace.source_path);
        if !source.is_dir() {
            bail!("workspace source is not a directory: {}", source.display());
        }
        let target = if workspace.mount_dir == "." {
            cwd.to_path_buf()
        } else {
            cwd.join(&workspace.mount_dir)
        };
        if is_mounted(&target)? {
            return Ok(());
        }
        ensure_empty_mountpoint(&target)?;
        require_fuse_overlayfs().await?;
        let overlay_root = root
            .join("overlays")
            .join(path_component(&workspace.mount_dir));
        let upper = overlay_root.join("upper");
        let work = overlay_root.join("work");
        tokio::fs::create_dir_all(&upper).await?;
        tokio::fs::create_dir_all(&work).await?;
        let status = Command::new("fuse-overlayfs")
            .arg("-o")
            .arg(format!(
                "lowerdir={},upperdir={},workdir={}",
                source.display(),
                upper.display(),
                work.display()
            ))
            .arg(&target)
            .status()
            .await
            .with_context(|| format!("mount overlay at {}", target.display()))?;
        if !status.success() {
            bail!("fuse-overlayfs failed for {}", target.display());
        }
        Ok(())
    }
}

async fn require_fuse_overlayfs() -> Result<()> {
    match Command::new("fuse-overlayfs")
        .arg("--version")
        .output()
        .await
    {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => bail!(
            "fuse-overlayfs is required for session overlays but --version exited with {}",
            output.status
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("fuse-overlayfs is required for session overlays but was not found on PATH")
        }
        Err(error) => Err(error).context("check fuse-overlayfs"),
    }
}

fn validate_mount_dir(mount_dir: &str) -> Result<()> {
    if mount_dir == "." {
        return Ok(());
    }
    let path = Path::new(mount_dir);
    let mut components = path.components();
    let valid_single_component = matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none();
    if path.is_absolute() || !valid_single_component {
        bail!("workspace mount_dir must be '.' or a direct child name: {mount_dir}");
    }
    Ok(())
}

fn ensure_empty_mountpoint(target: &Path) -> Result<()> {
    std::fs::create_dir_all(target)?;
    if std::fs::read_dir(target)?.next().is_some() {
        bail!("overlay mountpoint is not empty: {}", target.display());
    }
    Ok(())
}

fn is_mounted(path: &Path) -> Result<bool> {
    let path = normalize_path(path);
    Ok(mountinfo_mountpoints()?
        .into_iter()
        .any(|mountpoint| normalize_path(&mountpoint) == path))
}

fn mounted_paths_under(path: &Path) -> Result<Vec<PathBuf>> {
    let path = normalize_path(path);
    let mut mounted = mountinfo_mountpoints()?
        .into_iter()
        .filter(|mountpoint| normalize_path(mountpoint).starts_with(&path))
        .collect::<Vec<_>>();
    mounted.sort_by_key(|mountpoint| mountpoint.components().count());
    Ok(mounted)
}

fn mountinfo_mountpoints() -> Result<Vec<PathBuf>> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")?;
    let mut mountpoints = Vec::new();
    for line in mountinfo.lines() {
        let Some(before_separator) = line.split(" - ").next() else {
            continue;
        };
        let fields = before_separator.split_whitespace().collect::<Vec<_>>();
        if let Some(mountpoint) = fields.get(4) {
            mountpoints.push(PathBuf::from(unescape_mountinfo(mountpoint)));
        }
    }
    Ok(mountpoints)
}

fn unescape_mountinfo(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            if let Ok(text) = std::str::from_utf8(&bytes[index + 1..index + 4]) {
                if let Ok(value) = u8::from_str_radix(text, 8) {
                    output.push(value);
                    index += 4;
                    continue;
                }
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn path_component(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02x}").chars().collect(),
        })
        .collect()
}
