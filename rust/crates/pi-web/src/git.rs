use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

use agent_store::{SessionGitConfig, SessionWorkspace, WorkspaceKind};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
#[cfg(windows)]
use process_wrap::tokio::JobObject;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::staging::{copy_named, copy_tree, open_absolute_dir_nofollow, CopyBudget, CopyLimits};

const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(4);
const GH_COMMAND_TIMEOUT: Duration = Duration::from_secs(6);
const COMMAND_OUTPUT_LIMIT: usize = 256 * 1024;
const COMMAND_ERROR_LIMIT: usize = 32 * 1024;
pub(crate) const MAX_CONCURRENT_INSPECTIONS: usize = 4;
const MAX_WORKSPACES: usize = 64;
const MAX_METADATA_ENTRIES: usize = 20_000;
const MAX_METADATA_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_METADATA_DEPTH: usize = 64;
const MAX_CONFIG_BYTES: usize = 64 * 1024;
const MAX_PARENTS: usize = 16;
const MAX_WORKSPACE_DIR_BYTES: usize = 256;
const MAX_BRANCH_BYTES: usize = 256;
const MAX_REMOTE_BYTES: usize = 1_024;
const MAX_SHA_BYTES: usize = 128;
const MAX_AUTHOR_BYTES: usize = 256;
const MAX_SUMMARY_BYTES: usize = 512;
const MAX_DATE_BYTES: usize = 64;
const MAX_PR_TITLE_BYTES: usize = 512;
const MAX_PR_URL_BYTES: usize = 2_048;
const MAX_PR_STATE_BYTES: usize = 32;

#[derive(Debug, Serialize)]
pub(crate) struct GitStatusView {
    session_id: String,
    limit: usize,
    workspaces: Vec<GitWorkspaceView>,
    workspaces_truncated: bool,
}

async fn finish_read(
    task: &mut tokio::task::JoinHandle<std::io::Result<BoundedRead>>,
    program: &str,
    stream: &str,
) -> Result<BoundedRead, String> {
    match timeout(Duration::from_secs(1), &mut *task).await {
        Ok(Ok(Ok(read))) => Ok(read),
        Ok(Ok(Err(_))) | Ok(Err(_)) => Err(format!("Couldn’t capture {program} {stream}.")),
        Err(_) => {
            task.abort();
            Err(format!("{program} {stream} did not close."))
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct GitWorkspaceView {
    workspace_dir: String,
    kind: WorkspaceKind,
    status: GitWorkspaceStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    detached: bool,
    unborn: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    head_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_url: Option<String>,
    pull_request: Option<GitPullRequestView>,
    pull_request_lookup: PullRequestLookup,
    commits: Vec<GitCommitView>,
    has_more: bool,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum GitWorkspaceStatus {
    Ready,
    NotGit,
    Unavailable,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PullRequestLookup {
    Found,
    None,
    Unavailable,
    NotApplicable,
}

#[derive(Debug, Serialize)]
struct GitPullRequestView {
    number: u64,
    title: String,
    url: String,
    state: String,
    is_draft: bool,
    head_ref_name: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct GitCommitView {
    sha: String,
    parents: Vec<String>,
    author_name: String,
    authored_at: String,
    summary: String,
}

#[derive(Debug, Deserialize)]
struct GhPullRequest {
    number: u64,
    title: String,
    url: String,
    state: String,
    #[serde(rename = "isDraft")]
    is_draft: bool,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
}

struct RepositoryPaths {
    _snapshot: tempfile::TempDir,
    workspace: PathBuf,
    git_dir: PathBuf,
}

#[derive(Clone)]
pub struct GitExecutables {
    git: Option<PathBuf>,
    gh: Option<PathBuf>,
    child_path: OsString,
}

impl GitExecutables {
    /// Resolve executable names exactly once from the operator-provided startup
    /// environment. Requests only use these canonical absolute paths.
    pub fn resolve(git: Option<&Path>, gh: Option<&Path>) -> anyhow::Result<Self> {
        let untrusted_root = std::env::current_dir()
            .ok()
            .and_then(|path| fs::canonicalize(path).ok())
            .filter(|path| path.parent().is_some());
        let git = resolve_program(git, "git", untrusted_root.as_deref())?;
        let gh = resolve_program(gh, "gh", untrusted_root.as_deref())?;
        Ok(Self::from_resolved(git, gh))
    }

    fn from_resolved(git: Option<PathBuf>, gh: Option<PathBuf>) -> Self {
        // Put the resolved Git directory first so gh's internal `git` calls
        // cannot be intercepted by another executable beside gh.
        let mut directories = Vec::new();
        for directory in [&git, &gh]
            .into_iter()
            .filter_map(|program| program.as_deref().and_then(Path::parent))
        {
            if !directories.iter().any(|existing| existing == directory) {
                directories.push(directory.to_path_buf());
            }
        }
        let child_path = std::env::join_paths(directories).unwrap_or_default();
        Self {
            git,
            gh,
            child_path,
        }
    }
    fn allowed_for_session(&self, outer_cwd: &Path) -> bool {
        let outer = fs::canonicalize(outer_cwd).unwrap_or_else(|_| outer_cwd.to_path_buf());
        [&self.git, &self.gh]
            .into_iter()
            .flatten()
            .all(|program| !program.starts_with(&outer))
    }
}

#[derive(Default)]
struct RepositoryConfig {
    origin_url: Option<String>,
}

fn resolve_program(
    explicit: Option<&Path>,
    name: &str,
    untrusted_root: Option<&Path>,
) -> anyhow::Result<Option<PathBuf>> {
    if let Some(explicit) = explicit {
        if !explicit.is_absolute() {
            anyhow::bail!("{name} executable override must be an absolute path");
        }
        return validate_program(explicit, name, untrusted_root).map(Some);
    }
    for candidate in startup_path_candidates(name) {
        if let Ok(program) = validate_program(&candidate, name, untrusted_root) {
            return Ok(Some(program));
        }
    }
    Ok(None)
}

fn validate_program(
    candidate: &Path,
    name: &str,
    untrusted_root: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    let canonical = fs::canonicalize(candidate).map_err(|error| {
        anyhow::anyhow!(
            "canonicalize configured {name} executable {}: {error}",
            candidate.display()
        )
    })?;
    let metadata = fs::metadata(&canonical)?;
    if !metadata.is_file() {
        anyhow::bail!(
            "configured {name} executable is not a regular file: {}",
            canonical.display()
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            anyhow::bail!(
                "configured {name} executable is not executable: {}",
                canonical.display()
            );
        }
    }
    if untrusted_root.is_some_and(|root| canonical.starts_with(root)) {
        anyhow::bail!(
            "refusing configured {name} executable inside pi-web's startup workspace: {}",
            canonical.display()
        );
    }
    Ok(canonical)
}

fn startup_path_candidates(name: &str) -> Vec<PathBuf> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    startup_path_candidates_from(&path, name)
}

fn startup_path_candidates_from(path: &OsStr, name: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for directory in std::env::split_paths(path) {
        if !directory.is_absolute() {
            continue;
        }
        #[cfg(windows)]
        let names = {
            let mut names = vec![name.to_string()];
            if Path::new(name).extension().is_none() {
                let extensions = std::env::var_os("PATHEXT")
                    .unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
                names.extend(
                    extensions
                        .to_string_lossy()
                        .split(';')
                        .filter(|extension| !extension.is_empty())
                        .map(|extension| format!("{name}{extension}")),
                );
            }
            names
        };
        #[cfg(not(windows))]
        let names = [name.to_string()];
        for candidate_name in names {
            let candidate = directory.join(candidate_name);
            if candidate.is_file() {
                candidates.push(candidate);
            }
        }
    }
    candidates
}

pub(crate) async fn session_git_status(
    session_id: &str,
    config: SessionGitConfig,
    limit: usize,
    semaphore: std::sync::Arc<Semaphore>,
    executables: GitExecutables,
) -> GitStatusView {
    let workspaces_truncated = config.workspaces.len() > MAX_WORKSPACES;
    let mut inspections = JoinSet::new();
    for (index, workspace) in config
        .workspaces
        .into_iter()
        .take(MAX_WORKSPACES)
        .enumerate()
    {
        let outer_cwd = config.outer_cwd.clone();
        let semaphore = semaphore.clone();
        let executables = executables.clone();
        inspections.spawn(async move {
            let permit = semaphore.acquire_owned().await;
            let result = match permit {
                Ok(_permit) => {
                    inspect_workspace(Path::new(&outer_cwd), workspace, limit, &executables).await
                }
                Err(_) => unreachable!("inspection semaphore remains owned by this request"),
            };
            (index, result)
        });
    }
    let mut workspaces = Vec::new();
    while let Some(result) = inspections.join_next().await {
        if let Ok(result) = result {
            workspaces.push(result);
        }
    }
    workspaces.sort_by_key(|(index, _)| *index);

    GitStatusView {
        session_id: bounded_string(session_id, MAX_WORKSPACE_DIR_BYTES),
        limit,
        workspaces: workspaces.into_iter().map(|(_, view)| view).collect(),
        workspaces_truncated,
    }
}

async fn inspect_workspace(
    outer_cwd: &Path,
    workspace: SessionWorkspace,
    limit: usize,
    programs: &GitExecutables,
) -> GitWorkspaceView {
    inspect_workspace_with_hook(outer_cwd, workspace, limit, programs, None).await
}

async fn inspect_workspace_with_hook(
    outer_cwd: &Path,
    workspace: SessionWorkspace,
    limit: usize,
    programs: &GitExecutables,
    after_metadata_open: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
) -> GitWorkspaceView {
    let workspace_dir = bounded_string(&workspace.workspace_dir, MAX_WORKSPACE_DIR_BYTES);
    let unavailable = |message: &str| {
        unavailable_workspace(workspace_dir.clone(), workspace.kind, message.to_string())
    };
    if !valid_workspace_dir(&workspace.workspace_dir) {
        return unavailable("The workspace directory is invalid.");
    }
    if !programs.allowed_for_session(outer_cwd) {
        return unavailable("The configured Git executables are inside the session workspace.");
    }

    let outer_cwd = outer_cwd.to_path_buf();
    let workspace_name = workspace.workspace_dir.clone();
    let snapshot = tokio::task::spawn_blocking(move || {
        snapshot_repository(
            &outer_cwd,
            OsStr::new(&workspace_name),
            after_metadata_open.as_deref(),
        )
    })
    .await;
    let (repository, repository_config) = match snapshot {
        Ok(Ok(snapshot)) => snapshot,
        Ok(Err(SnapshotError::NotGit)) => return not_git_workspace(workspace_dir, workspace.kind),
        Ok(Err(SnapshotError::Unavailable(message))) => return unavailable(message),
        Err(_) => return unavailable("Couldn’t stage repository metadata."),
    };

    let branch_ref = match run_git(
        &repository,
        programs,
        ["symbolic-ref", "--quiet", "HEAD"],
        GIT_COMMAND_TIMEOUT,
    )
    .await
    {
        Ok(output) if output.status.success() => {
            let value = bounded_bytes(trim_ascii(&output.stdout), MAX_BRANCH_BYTES + 11);
            if !value.starts_with("refs/heads/")
                || value.len() <= "refs/heads/".len()
                || value.chars().any(char::is_control)
            {
                return unavailable("Git returned an invalid branch.");
            }
            Some(value)
        }
        Ok(output) if output.status.code() == Some(1) => None,
        Ok(_) => return unavailable("Couldn’t read the Git branch."),
        Err(error) => return unavailable(&error.to_string()),
    };
    let branch = branch_ref
        .as_deref()
        .map(|branch| bounded_string(&branch["refs/heads/".len()..], MAX_BRANCH_BYTES));
    let head_sha = match run_git(
        &repository,
        programs,
        ["rev-parse", "--verify", "HEAD"],
        GIT_COMMAND_TIMEOUT,
    )
    .await
    {
        Ok(output) if output.status.success() => {
            let value = bounded_bytes(trim_ascii(&output.stdout), MAX_SHA_BYTES);
            if !valid_sha(&value) {
                return unavailable("Git returned an invalid HEAD.");
            }
            Some(value)
        }
        Ok(output) if output.status.code() == Some(128) && branch_ref.is_some() => {
            let branch_ref = branch_ref.as_deref().expect("branch checked above");
            match run_git(
                &repository,
                programs,
                ["show-ref", "--verify", "--quiet", branch_ref],
                GIT_COMMAND_TIMEOUT,
            )
            .await
            {
                Ok(output) if output.status.code() == Some(1) => None,
                Ok(_) => return unavailable("The Git HEAD is unavailable."),
                Err(error) => return unavailable(&error.to_string()),
            }
        }
        Ok(_) => return unavailable("The Git HEAD is unavailable."),
        Err(error) => return unavailable(&error.to_string()),
    };
    let remote_url = repository_config.origin_url;
    let display_remote_url = remote_url.as_deref().and_then(sanitize_remote_url);
    let mut error = None;
    let (commits, has_more) = if head_sha.is_some() {
        match read_commits(&repository, programs, limit).await {
            Ok(value) => value,
            Err(GitReadError::Command(command_error)) => {
                return unavailable(&command_error.to_string())
            }
            Err(GitReadError::Data(message)) => {
                error = Some(message);
                (Vec::new(), false)
            }
        }
    } else {
        (Vec::new(), false)
    };
    let (pull_request, pull_request_lookup) = lookup_pull_request(
        &repository,
        programs,
        branch.as_deref(),
        head_sha.as_deref(),
        remote_url.as_deref(),
    )
    .await;

    GitWorkspaceView {
        workspace_dir,
        kind: workspace.kind,
        status: GitWorkspaceStatus::Ready,
        error,
        unborn: branch.is_some() && head_sha.is_none(),
        detached: branch.is_none() && head_sha.is_some(),
        branch,
        head_sha,
        remote_url: display_remote_url,
        pull_request,
        pull_request_lookup,
        commits,
        has_more,
    }
}

enum SnapshotError {
    NotGit,
    Unavailable(&'static str),
}

fn snapshot_repository(
    outer_cwd: &Path,
    workspace_name: &OsStr,
    after_metadata_open: Option<&(dyn Fn() + Send + Sync)>,
) -> Result<(RepositoryPaths, RepositoryConfig), SnapshotError> {
    let absolute_outer = if outer_cwd.is_absolute() {
        outer_cwd.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| SnapshotError::Unavailable("The session directory is unavailable."))?
            .join(outer_cwd)
    };
    let outer = open_absolute_dir_nofollow(&absolute_outer)
        .map_err(|_| SnapshotError::Unavailable("The session directory is unavailable."))?;
    let workspace = outer.open_dir_nofollow(workspace_name).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            SnapshotError::Unavailable("The workspace directory is unavailable.")
        } else {
            SnapshotError::Unavailable("The workspace directory is not a safe directory.")
        }
    })?;
    let git =
        match workspace.open_dir_nofollow(".git") {
            Ok(git) => git,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match workspace.symlink_metadata(".git") {
                    Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => {
                        return Err(SnapshotError::NotGit)
                    }
                    _ => return Err(SnapshotError::Unavailable(
                        "Git pointer files and linked worktrees are not available in the browser.",
                    )),
                }
            }
            Err(_) => {
                return Err(SnapshotError::Unavailable(
                    "The Git directory is not a safe directory.",
                ))
            }
        };
    for forbidden in ["commondir", "gitdir", "config.worktree"] {
        match git.symlink_metadata(forbidden) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(SnapshotError::Unavailable(
                    "Repository metadata uses unsupported external Git indirection.",
                ))
            }
            Err(_) => {
                return Err(SnapshotError::Unavailable(
                    "Repository metadata is unavailable.",
                ))
            }
        }
    }
    let objects = git.open_dir_nofollow("objects").map_err(|_| {
        SnapshotError::Unavailable("Repository object metadata is not a safe directory.")
    })?;
    reject_object_indirection(&objects)?;
    let refs = match git.open_dir_nofollow("refs") {
        Ok(refs) => Some(refs),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => {
            return Err(SnapshotError::Unavailable(
                "Repository reference metadata is not a safe directory.",
            ))
        }
    };

    let snapshot = tempfile::Builder::new()
        .prefix("pi-web-git-")
        .tempdir()
        .map_err(|_| SnapshotError::Unavailable("Couldn’t stage repository metadata."))?;
    fs::create_dir(snapshot.path().join("worktree"))
        .and_then(|()| fs::create_dir(snapshot.path().join("git")))
        .map_err(|_| SnapshotError::Unavailable("Couldn’t stage repository metadata."))?;
    let destination = open_absolute_dir_nofollow(&snapshot.path().join("git"))
        .map_err(|_| SnapshotError::Unavailable("Couldn’t stage repository metadata."))?;
    let mut budget = CopyBudget::new(CopyLimits {
        max_entries: MAX_METADATA_ENTRIES,
        max_bytes: MAX_METADATA_BYTES,
        max_depth: MAX_METADATA_DEPTH,
    });
    let mut include_all = |_: &Path, _: bool| true;
    copy_named(
        &git,
        &destination,
        OsStr::new("config"),
        true,
        &mut budget,
        &mut include_all,
    )
    .map_err(|_| SnapshotError::Unavailable("The repository configuration is unavailable."))?;
    let config = read_snapshot_config(&destination)?;

    copy_named(
        &git,
        &destination,
        OsStr::new("HEAD"),
        true,
        &mut budget,
        &mut include_all,
    )
    .map_err(|_| SnapshotError::Unavailable("The Git HEAD is unavailable."))?;
    for optional in ["packed-refs", "shallow"] {
        copy_named(
            &git,
            &destination,
            OsStr::new(optional),
            false,
            &mut budget,
            &mut include_all,
        )
        .map_err(|_| SnapshotError::Unavailable("Repository metadata is unavailable."))?;
    }
    if let Some(hook) = after_metadata_open {
        hook();
    }
    destination
        .create_dir("objects")
        .map_err(|_| SnapshotError::Unavailable("Couldn’t stage repository metadata."))?;
    let destination_objects = destination
        .open_dir_nofollow("objects")
        .map_err(|_| SnapshotError::Unavailable("Couldn’t stage repository metadata."))?;
    copy_tree(
        &objects,
        &destination_objects,
        &mut budget,
        &mut |relative, _| {
            !matches!(
                relative.to_str(),
                Some("info/alternates" | "info/http-alternates")
            )
        },
    )
    .map_err(|_| {
        SnapshotError::Unavailable("Repository metadata is unsafe, incomplete, or too large.")
    })?;
    if let Some(refs) = refs {
        destination
            .create_dir("refs")
            .map_err(|_| SnapshotError::Unavailable("Couldn’t stage repository metadata."))?;
        let destination_refs = destination
            .open_dir_nofollow("refs")
            .map_err(|_| SnapshotError::Unavailable("Couldn’t stage repository metadata."))?;
        copy_tree(&refs, &destination_refs, &mut budget, &mut |_, _| true).map_err(|_| {
            SnapshotError::Unavailable("Repository metadata is unsafe, incomplete, or too large.")
        })?;
    }

    let repository = RepositoryPaths {
        workspace: snapshot.path().join("worktree"),
        git_dir: snapshot.path().join("git"),
        _snapshot: snapshot,
    };
    Ok((repository, config))
}

fn reject_object_indirection(objects: &Dir) -> Result<(), SnapshotError> {
    let info = match objects.open_dir_nofollow("info") {
        Ok(info) => info,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {
            return Err(SnapshotError::Unavailable(
                "Repository object metadata is not a safe directory.",
            ))
        }
    };
    for forbidden in ["alternates", "http-alternates"] {
        match info.symlink_metadata(forbidden) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(SnapshotError::Unavailable(
                    "Repository metadata uses unsupported external Git indirection.",
                ))
            }
            Err(_) => {
                return Err(SnapshotError::Unavailable(
                    "Repository metadata is unavailable.",
                ))
            }
        }
    }
    Ok(())
}

fn read_snapshot_config(git: &Dir) -> Result<RepositoryConfig, SnapshotError> {
    let metadata = git
        .symlink_metadata("config")
        .map_err(|_| SnapshotError::Unavailable("The repository configuration is unavailable."))?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES as u64 {
        return Err(SnapshotError::Unavailable(
            "The repository configuration is too large or invalid.",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = git
        .open_with("config", &options)
        .map_err(|_| SnapshotError::Unavailable("The repository configuration is unavailable."))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(MAX_CONFIG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| SnapshotError::Unavailable("The repository configuration is unavailable."))?;
    if bytes.len() as u64 != metadata.len() {
        return Err(SnapshotError::Unavailable(
            "The repository configuration changed while staging.",
        ));
    }
    validate_config(&bytes).map_err(|message| {
        let _ = message;
        SnapshotError::Unavailable("Repository configuration is outside the safe read-only policy.")
    })
}

fn validate_config(bytes: &[u8]) -> Result<RepositoryConfig, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "The repository configuration is not UTF-8.".to_string())?;
    let mut section = String::new();
    let mut subsection = None;
    let mut config = RepositoryConfig::default();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            let end = line
                .find(']')
                .ok_or_else(|| "The repository configuration is malformed.".to_string())?;
            if !line[end + 1..].trim().is_empty() {
                return Err("The repository configuration is malformed.".to_string());
            }
            (section, subsection) = parse_config_section(&line[1..end])?;
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| "The repository configuration is malformed.".to_string())?;
        let key = key.trim().to_ascii_lowercase();
        let value = parse_config_value(value.trim())?;
        match (section.as_str(), subsection.as_deref(), key.as_str()) {
            ("core", None, "repositoryformatversion") if matches!(value.as_str(), "0" | "1") => {}
            ("core", None, "filemode" | "logallrefupdates" | "ignorecase")
            | ("core", None, "precomposeunicode" | "symlinks") => validate_config_bool(&value)?,
            ("core", None, "bare") if value.eq_ignore_ascii_case("false") => {}
            ("extensions", None, "objectformat")
                if matches!(value.to_ascii_lowercase().as_str(), "sha1" | "sha256") => {}
            ("remote", Some(remote), "url" | "fetch")
                if valid_config_subsection(remote) && value.len() <= MAX_CONFIG_BYTES =>
            {
                if remote.eq_ignore_ascii_case("origin")
                    && key == "url"
                    && config.origin_url.replace(value).is_some()
                {
                    return Err("The repository has an ambiguous origin URL.".to_string());
                }
            }
            ("branch", Some(branch), "remote" | "merge")
                if valid_config_subsection(branch) && value.len() <= MAX_BRANCH_BYTES => {}
            ("user", None, "name" | "email") if value.len() <= MAX_AUTHOR_BYTES => {}
            _ => {
                return Err(
                    "Repository configuration is outside the safe read-only policy.".to_string(),
                )
            }
        }
    }
    if section.is_empty() {
        return Err("The repository configuration is malformed.".to_string());
    }
    Ok(config)
}

fn parse_config_section(value: &str) -> Result<(String, Option<String>), String> {
    let value = value.trim();
    let (section, subsection) = value
        .split_once(char::is_whitespace)
        .map_or((value, None), |(section, subsection)| {
            (section, Some(subsection.trim()))
        });
    if section.is_empty()
        || !section
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("The repository configuration is malformed.".to_string());
    }
    let subsection = subsection
        .map(parse_quoted_config_value)
        .transpose()?
        .filter(|value| !value.is_empty());
    Ok((section.to_ascii_lowercase(), subsection))
}

fn parse_config_value(value: &str) -> Result<String, String> {
    if value.starts_with('"') {
        parse_quoted_config_value(value)
    } else if value.contains(['"', '\0', '\r', '\n']) {
        Err("The repository configuration is malformed.".to_string())
    } else {
        Ok(value.to_string())
    }
}

fn parse_quoted_config_value(value: &str) -> Result<String, String> {
    let Some(value) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return Err("The repository configuration is malformed.".to_string());
    };
    if value.contains(['"', '\\', '\0', '\r', '\n']) {
        return Err("The repository configuration uses unsupported quoting.".to_string());
    }
    Ok(value.to_string())
}

fn validate_config_bool(value: &str) -> Result<(), String> {
    if matches!(
        value.to_ascii_lowercase().as_str(),
        "true" | "false" | "yes" | "no" | "on" | "off" | "1" | "0"
    ) {
        Ok(())
    } else {
        Err("The repository configuration has an invalid boolean.".to_string())
    }
}

fn valid_config_subsection(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_BRANCH_BYTES && !value.chars().any(char::is_control)
}

async fn read_commits(
    repository: &RepositoryPaths,
    programs: &GitExecutables,
    limit: usize,
) -> Result<(Vec<GitCommitView>, bool), GitReadError> {
    let max_count = limit.saturating_add(1).to_string();
    let output = run_git(
        repository,
        programs,
        [
            "log",
            "-z",
            "--no-show-signature",
            "--no-textconv",
            "--no-ext-diff",
            "--date=iso-strict",
            "--format=%H%x00%P%x00%an%x00%aI%x00%s",
            "--max-count",
            &max_count,
            "HEAD",
        ],
        GIT_COMMAND_TIMEOUT,
    )
    .await
    .map_err(GitReadError::Command)?;
    if !output.status.success() {
        return Err(GitReadError::Data(
            "Couldn’t read commit history.".to_string(),
        ));
    }
    let mut commits = parse_git_log(&output.stdout).map_err(GitReadError::Data)?;
    let has_more = commits.len() > limit;
    commits.truncate(limit);
    Ok((commits, has_more))
}

fn parse_git_log(output: &[u8]) -> Result<Vec<GitCommitView>, String> {
    let mut fields = output.split(|byte| *byte == 0).collect::<Vec<_>>();
    if fields.last().is_some_and(|field| field.is_empty()) {
        fields.pop();
    }
    if fields.len() % 5 != 0 {
        return Err("Git returned malformed commit history.".to_string());
    }
    let mut commits = Vec::with_capacity(fields.len() / 5);
    for field in fields.chunks_exact(5) {
        let sha = bounded_bytes(field[0], MAX_SHA_BYTES);
        if !valid_sha(&sha) {
            return Err("Git returned an invalid commit identifier.".to_string());
        }
        let parents = String::from_utf8_lossy(field[1])
            .split_whitespace()
            .take(MAX_PARENTS + 1)
            .map(|parent| bounded_string(parent, MAX_SHA_BYTES))
            .collect::<Vec<_>>();
        if parents.len() > MAX_PARENTS || parents.iter().any(|parent| !valid_sha(parent)) {
            return Err("Git returned invalid commit lineage.".to_string());
        }
        commits.push(GitCommitView {
            sha,
            parents,
            author_name: bounded_bytes(field[2], MAX_AUTHOR_BYTES),
            authored_at: bounded_bytes(field[3], MAX_DATE_BYTES),
            summary: bounded_bytes(field[4], MAX_SUMMARY_BYTES),
        });
    }
    Ok(commits)
}

async fn lookup_pull_request(
    repository: &RepositoryPaths,
    programs: &GitExecutables,
    branch: Option<&str>,
    head_sha: Option<&str>,
    remote_url: Option<&str>,
) -> (Option<GitPullRequestView>, PullRequestLookup) {
    let Some(repository_slug) = remote_url.and_then(remote_repository_slug) else {
        return (None, PullRequestLookup::NotApplicable);
    };
    let Some(branch) = branch.filter(|branch| !branch.trim().is_empty()) else {
        return (None, PullRequestLookup::NotApplicable);
    };
    let Some(head_sha) = head_sha else {
        return (None, PullRequestLookup::NotApplicable);
    };
    let branch = bounded_string(branch, MAX_BRANCH_BYTES);
    let output = match run_command(
        programs.gh.as_deref(),
        &programs.child_path,
        "gh",
        [
            OsString::from("pr"),
            OsString::from("list"),
            OsString::from("--repo"),
            OsString::from(repository_slug),
            OsString::from("--head"),
            OsString::from(&branch),
            OsString::from("--state"),
            OsString::from("open"),
            OsString::from("--limit"),
            OsString::from("20"),
            OsString::from("--json"),
            OsString::from("number,title,url,state,isDraft,headRefName,headRefOid"),
        ],
        &repository.workspace,
        GH_COMMAND_TIMEOUT,
        CommandEnvironment::Gh,
    )
    .await
    {
        Ok(output) if output.status.success() => output,
        _ => return (None, PullRequestLookup::Unavailable),
    };
    let rows = match serde_json::from_slice::<Vec<GhPullRequest>>(&output.stdout) {
        Ok(rows) => rows,
        Err(_) => return (None, PullRequestLookup::Unavailable),
    };
    let row = match select_pull_request(&rows, &branch, head_sha) {
        Ok(Some(row)) => row,
        Ok(None) => return (None, PullRequestLookup::None),
        Err(()) => return (None, PullRequestLookup::Unavailable),
    };
    if !safe_https_url(&row.url) {
        return (None, PullRequestLookup::Unavailable);
    }
    (
        Some(GitPullRequestView {
            number: row.number,
            title: bounded_string(&row.title, MAX_PR_TITLE_BYTES),
            url: bounded_string(&row.url, MAX_PR_URL_BYTES),
            state: bounded_string(&row.state.to_ascii_lowercase(), MAX_PR_STATE_BYTES),
            is_draft: row.is_draft,
            head_ref_name: bounded_string(&row.head_ref_name, MAX_BRANCH_BYTES),
        }),
        PullRequestLookup::Found,
    )
}

fn select_pull_request<'a>(
    rows: &'a [GhPullRequest],
    branch: &str,
    head_sha: &str,
) -> Result<Option<&'a GhPullRequest>, ()> {
    let mut matching = rows.iter().filter(|row| {
        row.head_ref_name == branch
            && row.head_ref_oid.eq_ignore_ascii_case(head_sha)
            && valid_sha(&row.head_ref_oid)
            && row.state.eq_ignore_ascii_case("open")
    });
    let first = matching.next();
    if matching.next().is_some() {
        return Err(());
    }
    Ok(first)
}

fn sanitize_remote_url(remote: &str) -> Option<String> {
    let remote = remote.trim();
    if remote.is_empty() || remote.chars().any(char::is_control) {
        return None;
    }
    let remote = remote.split(['?', '#']).next().unwrap_or(remote);
    if let Some((scheme, rest)) = remote.split_once("://") {
        if !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "http" | "https" | "ssh" | "git"
        ) {
            return Some("local repository".to_string());
        }
        let authority_end = rest.find('/').unwrap_or(rest.len());
        let (authority, path) = rest.split_at(authority_end);
        let host_port = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        if host_port.is_empty() || host_port.parse::<axum::http::uri::Authority>().is_err() {
            return None;
        }
        return bounded_remote(format!("{scheme}://{host_port}{path}"));
    }
    if let Some((authority, path)) = remote.split_once(':') {
        if path.is_empty() || path.contains('\\') {
            return Some("local repository".to_string());
        }
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        if !valid_remote_component(host) {
            return None;
        }
        return bounded_remote(format!("{host}:{path}"));
    }
    Some("local repository".to_string())
}

fn bounded_remote(remote: String) -> Option<String> {
    (remote.len() <= MAX_REMOTE_BYTES).then_some(remote)
}

fn remote_repository_slug(remote: &str) -> Option<String> {
    let remote = remote.split(['?', '#']).next()?.trim();
    let (host, path) = if let Some((scheme, rest)) = remote.split_once("://") {
        if !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "http" | "https" | "ssh" | "git"
        ) {
            return None;
        }
        let slash = rest.find('/')?;
        let (authority, path) = rest.split_at(slash);
        (
            authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host),
            path.trim_start_matches('/'),
        )
    } else {
        let (authority, path) = remote.split_once(':')?;
        (
            authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host),
            path,
        )
    };
    let host = host.split_once(':').map_or(host, |(host, _)| host);
    let mut segments = path.trim_end_matches('/').split('/');
    let owner = segments.next()?;
    let repository = segments.next()?.trim_end_matches(".git");
    if segments.next().is_some()
        || !valid_remote_component(host)
        || !valid_remote_component(owner)
        || !valid_remote_component(repository)
    {
        return None;
    }
    Some(format!("{host}/{owner}/{repository}"))
}

fn valid_remote_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn safe_https_url(value: &str) -> bool {
    value.len() <= MAX_PR_URL_BYTES
        && value.starts_with("https://")
        && !value.chars().any(char::is_control)
}

async fn run_git<const N: usize>(
    repository: &RepositoryPaths,
    programs: &GitExecutables,
    args: [&str; N],
    command_timeout: Duration,
) -> Result<CapturedOutput, CommandError> {
    let mut all_args = vec![
        OsString::from("--no-optional-locks"),
        OsString::from("--no-pager"),
        OsString::from("--no-replace-objects"),
        OsString::from("-c"),
        OsString::from("log.showSignature=false"),
        OsString::from("-c"),
        OsString::from("core.fsmonitor=false"),
        OsString::from("-c"),
        OsString::from("core.hooksPath="),
        OsString::from("-c"),
        OsString::from("credential.helper="),
        OsString::from("-c"),
        OsString::from("diff.external="),
        OsString::from("-c"),
        OsString::from("gpg.program=false"),
        OsString::from("--git-dir"),
        repository.git_dir.as_os_str().to_os_string(),
        OsString::from("--work-tree"),
        repository.workspace.as_os_str().to_os_string(),
    ];
    all_args.extend(args.into_iter().map(OsString::from));
    run_command(
        programs.git.as_deref(),
        &programs.child_path,
        "git",
        all_args,
        &repository.workspace,
        command_timeout,
        CommandEnvironment::Git,
    )
    .await
}

enum CommandEnvironment {
    Git,
    Gh,
}

#[derive(Debug)]
struct CapturedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
}

enum GitReadError {
    Command(CommandError),
    Data(String),
}

#[derive(Debug)]
enum CommandError {
    Unavailable(&'static str),
    Failed(&'static str),
    TimedOut(&'static str),
    Capture(&'static str),
    OutputLimit(&'static str),
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(program) => write!(formatter, "{program} is not available."),
            Self::Failed(program) => write!(formatter, "Couldn’t run {program}."),
            Self::TimedOut(program) => write!(formatter, "{program} timed out."),
            Self::Capture(program) => write!(formatter, "Couldn’t capture {program} output."),
            Self::OutputLimit(program) => {
                write!(formatter, "{program} output exceeded the safety limit.")
            }
        }
    }
}

async fn run_command(
    executable: Option<&Path>,
    child_path: &OsStr,
    program: &'static str,
    args: impl IntoIterator<Item = OsString>,
    cwd: &Path,
    command_timeout: Duration,
    environment: CommandEnvironment,
) -> Result<CapturedOutput, CommandError> {
    let executable = executable.ok_or(CommandError::Unavailable(program))?;
    let mut command = Command::new(executable);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env_clear();
    command
        .env("PATH", child_path)
        .env("LC_ALL", "C.UTF-8")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", null_device())
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_PAGER", "")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "");
    match environment {
        CommandEnvironment::Git => {
            command.env("HOME", non_existent_home());
            command.env("XDG_CONFIG_HOME", non_existent_home());
        }
        CommandEnvironment::Gh => {
            copy_env(&mut command, "HOME");
            copy_env(&mut command, "XDG_CONFIG_HOME");
            command
                // `--repo` is always explicit. Prevent any internal Git call
                // from discovering the untrusted workspace repository.
                .env("GIT_DIR", null_device())
                .env("GH_PROMPT_DISABLED", "1")
                .env("GH_NO_UPDATE_NOTIFIER", "1")
                .env("NO_COLOR", "1");
        }
    }
    #[cfg(unix)]
    command.process_group(0);

    let mut command = CommandWrap::from(command);
    #[cfg(windows)]
    command.wrap(JobObject);
    command.wrap(KillOnDrop);
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CommandError::Unavailable(program)
        } else {
            CommandError::Failed(program)
        }
    })?;
    #[cfg(unix)]
    let process_group = child.id().ok_or(CommandError::Failed(program))?;
    #[cfg(windows)]
    let process_group = ();
    let stdout = match child.stdout().take() {
        Some(stdout) => stdout,
        None => {
            terminate_and_reap(&mut child, process_group).await;
            return Err(CommandError::Capture(program));
        }
    };
    let stderr = match child.stderr().take() {
        Some(stderr) => stderr,
        None => {
            terminate_and_reap(&mut child, process_group).await;
            return Err(CommandError::Capture(program));
        }
    };
    let stdout_task = tokio::spawn(read_bounded(stdout, COMMAND_OUTPUT_LIMIT));
    let stderr_task = tokio::spawn(read_bounded(stderr, COMMAND_ERROR_LIMIT));

    let status = match timeout(command_timeout, wait_for_direct_child(&mut child)).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => {
            terminate_and_reap(&mut child, process_group).await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(CommandError::Failed(program));
        }
        Err(_) => {
            terminate_and_reap(&mut child, process_group).await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(CommandError::TimedOut(program));
        }
    };
    // A successful direct-child exit does not imply that its process group is
    // empty. Explicitly kill remaining same-group descendants on every path.
    // A malicious descendant can escape with setsid(2); executable trust and
    // the inert repository-config policy ensure repositories cannot choose an
    // arbitrary helper that attempts that escape.
    terminate_and_reap(&mut child, process_group).await;
    let mut stdout_task = stdout_task;
    let mut stderr_task = stderr_task;
    let (stdout, stderr) = tokio::join!(
        finish_read(&mut stdout_task, program, "output"),
        finish_read(&mut stderr_task, program, "error output"),
    );
    let (stdout, stderr) = match (stdout, stderr) {
        (Ok(stdout), Ok(stderr)) => (stdout, stderr),
        _ => {
            terminate_and_reap(&mut child, process_group).await;
            return Err(CommandError::Capture(program));
        }
    };
    if stdout.exceeded || stderr.exceeded {
        terminate_and_reap(&mut child, process_group).await;
        return Err(CommandError::OutputLimit(program));
    }
    Ok(CapturedOutput {
        status,
        stdout: stdout.bytes,
    })
}

#[cfg(unix)]
async fn terminate_and_reap(child: &mut Box<dyn ChildWrapper>, process_group: u32) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;

    if let Ok(process_group) = i32::try_from(process_group) {
        let _ = killpg(Pid::from_raw(process_group), Signal::SIGKILL);
    }
    let _ = child.wait().await;
}

#[cfg(windows)]
async fn terminate_and_reap(child: &mut Box<dyn ChildWrapper>, _process_group: ()) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

async fn wait_for_direct_child(child: &mut Box<dyn ChildWrapper>) -> std::io::Result<ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

struct BoundedRead {
    bytes: Vec<u8>,
    exceeded: bool,
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<BoundedRead> {
    let mut bytes = Vec::new();
    let mut exceeded = false;
    let mut buffer = [0u8; 8 * 1024];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..count.min(remaining)]);
        exceeded |= count > remaining;
    }
    Ok(BoundedRead { bytes, exceeded })
}

fn copy_env(command: &mut Command, name: &str) {
    if let Some(value) = std::env::var_os(name) {
        command.env(name, value);
    }
}

#[cfg(unix)]
fn null_device() -> &'static OsStr {
    OsStr::new("/dev/null")
}

#[cfg(windows)]
fn null_device() -> &'static OsStr {
    OsStr::new("NUL")
}

#[cfg(unix)]
fn non_existent_home() -> &'static OsStr {
    OsStr::new("/nonexistent/pi-web")
}

#[cfg(windows)]
fn non_existent_home() -> &'static OsStr {
    OsStr::new("C:\\nonexistent\\pi-web")
}

fn valid_workspace_dir(value: &str) -> bool {
    value.len() <= MAX_WORKSPACE_DIR_BYTES
        && !value.is_empty()
        && Path::new(value).components().count() == 1
        && matches!(
            Path::new(value).components().next(),
            Some(Component::Normal(_))
        )
}

fn valid_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn bounded_string(value: &str, max_bytes: usize) -> String {
    bounded_bytes(value.as_bytes(), max_bytes)
}

fn bounded_bytes(value: &[u8], max_bytes: usize) -> String {
    let value = String::from_utf8_lossy(value);
    if value.len() <= max_bytes {
        return value.into_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn trim_ascii(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &value[start..end]
}

fn not_git_workspace(workspace_dir: String, kind: WorkspaceKind) -> GitWorkspaceView {
    empty_workspace(workspace_dir, kind, GitWorkspaceStatus::NotGit, None)
}

fn unavailable_workspace(
    workspace_dir: String,
    kind: WorkspaceKind,
    error: String,
) -> GitWorkspaceView {
    empty_workspace(
        workspace_dir,
        kind,
        GitWorkspaceStatus::Unavailable,
        Some(error),
    )
}

fn empty_workspace(
    workspace_dir: String,
    kind: WorkspaceKind,
    status: GitWorkspaceStatus,
    error: Option<String>,
) -> GitWorkspaceView {
    GitWorkspaceView {
        workspace_dir,
        kind,
        status,
        error,
        branch: None,
        detached: false,
        unborn: false,
        head_sha: None,
        remote_url: None,
        pull_request: None,
        pull_request_lookup: PullRequestLookup::NotApplicable,
        commits: Vec::new(),
        has_more: false,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command as StdCommand;

    use tempfile::TempDir;

    use super::*;

    fn test_programs() -> GitExecutables {
        GitExecutables::resolve(None, None).expect("resolve test executables")
    }

    fn resolved_test_program(name: &str) -> PathBuf {
        startup_path_candidates(name)
            .into_iter()
            .find_map(|candidate| fs::canonicalize(candidate).ok())
            .unwrap_or_else(|| panic!("{name} executable"))
    }

    #[test]
    fn parses_commit_records_and_bounds_fields_and_parents() {
        let sha = "a".repeat(40);
        let parent = "b".repeat(40);
        let output = format!(
            "{sha}\0{parent}\0{}\0{}\0{}\0",
            "A".repeat(MAX_AUTHOR_BYTES + 20),
            "2024-01-02T03:04:05Z",
            "S".repeat(MAX_SUMMARY_BYTES + 20),
        );
        let commits = parse_git_log(output.as_bytes()).expect("commit log");
        assert_eq!(commits[0].author_name.len(), MAX_AUTHOR_BYTES);
        assert_eq!(commits[0].summary.len(), MAX_SUMMARY_BYTES);
        assert_eq!(commits[0].parents, vec![parent]);

        let too_many = std::iter::repeat_n("b".repeat(40), MAX_PARENTS + 1)
            .collect::<Vec<_>>()
            .join(" ");
        let output = format!("{sha}\0{too_many}\0Ada\0date\0summary\0");
        assert!(parse_git_log(output.as_bytes()).is_err());
    }

    #[test]
    fn remote_urls_drop_credentials_queries_and_scp_users() {
        assert_eq!(
            sanitize_remote_url("https://token@example.test/owner/repo.git?token=secret"),
            Some("https://example.test/owner/repo.git".to_string())
        );
        assert_eq!(
            sanitize_remote_url("secret@github.com:owner/repo.git"),
            Some("github.com:owner/repo.git".to_string())
        );
        assert_eq!(
            sanitize_remote_url("/srv/private/repo"),
            Some("local repository".to_string())
        );
        assert_eq!(
            sanitize_remote_url("C:\\private\\repo"),
            Some("local repository".to_string())
        );
        assert_eq!(sanitize_remote_url("bad\nremote"), None);
        assert_eq!(
            remote_repository_slug("https://token@github.com/owner/repo.git?token=secret"),
            Some("github.com/owner/repo".to_string())
        );
        assert_eq!(
            remote_repository_slug("git@github.com:owner/repo.git"),
            Some("github.com/owner/repo".to_string())
        );
        assert_eq!(remote_repository_slug("ext::sh -c attack"), None);

        let secret = "credential-secret-".repeat(80);
        let remote = format!("https://{secret}@github.com/owner/repo.git?token={secret}");
        let sanitized = sanitize_remote_url(&remote).expect("long credentials are stripped");
        assert_eq!(sanitized, "https://github.com/owner/repo.git");
        assert!(!sanitized.contains(&secret));

        let overlong_path = format!("https://github.com/owner/{}", "x".repeat(MAX_REMOTE_BYTES));
        assert_eq!(sanitize_remote_url(&overlong_path), None);
    }

    #[test]
    fn accepts_only_inert_repository_config() {
        assert!(validate_config(b"[include]\npath = /tmp/outside\n").is_err());
        assert!(validate_config(b"[includeIf \"gitdir:/tmp\"]\npath = x\n").is_err());
        assert!(validate_config(b"[core]\nhooksPath = ../../outside\n").is_err());
        assert!(validate_config(b"[credential]\nhelper = !echo secret\n").is_err());
        assert!(validate_config(b"[log]\nshowSignature = true\n").is_err());
        assert!(validate_config(b"[gpg]\nprogram = /tmp/attack\n").is_err());
        assert!(validate_config(b"[diff \"owned\"]\ntextconv = /tmp/attack\n").is_err());
        assert!(validate_config(b"[filter \"owned\"]\nsmudge = /tmp/attack\n").is_err());
        assert!(validate_config(b"[core]\nfsmonitor = /tmp/attack\n").is_err());
        assert!(validate_config(b"[core]\nsshCommand = /tmp/attack\n").is_err());
        assert!(validate_config(b"[core]\nrepositoryformatversion = 0\n").is_ok());
    }

    #[test]
    fn serialized_strings_are_strictly_byte_bounded_at_utf8_boundaries() {
        let bounded = bounded_string("ééé", 5);
        assert_eq!(bounded, "éé");
        assert!(bounded.len() <= 5);
        let lossy = bounded_bytes(&[0xff; 10], 5);
        assert!(lossy.len() <= 5);
        assert!(lossy.is_char_boundary(lossy.len()));
    }

    #[cfg(unix)]
    #[test]
    fn startup_path_skips_repository_controlled_git_and_gh() {
        use std::os::unix::fs::PermissionsExt;

        let repository = tempfile::Builder::new()
            .prefix(".pi-web-path-")
            .tempdir_in(std::env::current_dir().expect("cwd"))
            .expect("repository temp");
        for name in ["git", "gh"] {
            let path = repository.path().join(name);
            fs::write(&path, "#!/bin/sh\nexit 99\n").expect("fake executable");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("permissions");
        }
        let system_git = resolved_test_program("git");
        let system_gh = startup_path_candidates("gh")
            .into_iter()
            .find_map(|candidate| fs::canonicalize(candidate).ok());
        let mut path_entries = vec![repository.path().to_path_buf()];
        path_entries.push(system_git.parent().expect("git parent").to_path_buf());
        if let Some(gh) = &system_gh {
            path_entries.push(gh.parent().expect("gh parent").to_path_buf());
        }
        let path = std::env::join_paths(path_entries).expect("path");
        let untrusted = fs::canonicalize(std::env::current_dir().expect("cwd")).expect("root");
        let resolved = |name| {
            startup_path_candidates_from(&path, name)
                .into_iter()
                .find_map(|candidate| validate_program(&candidate, name, Some(&untrusted)).ok())
        };
        assert_eq!(resolved("git").as_deref(), Some(system_git.as_path()));
        assert_ne!(
            resolved("gh").as_deref(),
            Some(
                fs::canonicalize(repository.path().join("gh"))
                    .unwrap()
                    .as_path()
            )
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_path_executables_are_disabled_and_gh_uses_only_trusted_git() {
        use std::os::unix::fs::PermissionsExt;

        let outer = TempDir::new().expect("outer");
        let repository = outer.path().join("repo");
        let fake_bin = outer.path().join("fake-bin");
        fs::create_dir(&repository).expect("repository");
        fs::create_dir(&fake_bin).expect("fake bin");
        let fake_git_marker = outer.path().join("fake-git-marker");
        let fake_gh_marker = outer.path().join("fake-gh-marker");
        for (name, marker) in [("git", &fake_git_marker), ("gh", &fake_gh_marker)] {
            let executable = fake_bin.join(name);
            fs::write(
                &executable,
                format!("#!/bin/sh\nprintf ran >'{}'\nexit 99\n", marker.display()),
            )
            .expect("fake executable");
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
                .expect("permissions");
        }
        let system_git = resolved_test_program("git");
        let prepended_path =
            std::env::join_paths([fake_bin.as_path(), system_git.parent().expect("git parent")])
                .expect("path");
        let selected_git = startup_path_candidates_from(&prepended_path, "git")
            .into_iter()
            .next()
            .expect("fake git selected by raw PATH");
        let selected_gh = startup_path_candidates_from(&prepended_path, "gh")
            .into_iter()
            .next()
            .expect("fake gh selected by raw PATH");
        let blocked = inspect_workspace(
            outer.path(),
            SessionWorkspace::local("repo", "unused"),
            12,
            &GitExecutables::from_resolved(
                Some(fs::canonicalize(selected_git).expect("fake git")),
                Some(fs::canonicalize(selected_gh).expect("fake gh")),
            ),
        )
        .await;
        assert_eq!(blocked.status, GitWorkspaceStatus::Unavailable);
        assert!(!fake_git_marker.exists());
        assert!(!fake_gh_marker.exists());

        git(&repository, ["init", "-b", "main"]);
        git(&repository, ["config", "user.name", "Test Author"]);
        git(
            &repository,
            ["config", "user.email", "test@example.invalid"],
        );
        git(
            &repository,
            [
                "remote",
                "add",
                "origin",
                "https://github.com/owner/repo.git",
            ],
        );
        fs::write(repository.join("file"), "safe").expect("file");
        git(&repository, ["add", "."]);
        git(&repository, ["commit", "-m", "Safe commit"]);

        let helper_dir = TempDir::new().expect("trusted helper");
        let trusted_gh = helper_dir.path().join("gh");
        let trusted_gh_marker = helper_dir.path().join("trusted-gh-marker");
        fs::write(
            &trusted_gh,
            format!(
                "#!/bin/sh\ngit --version >/dev/null || exit 91\nprintf ran >'{}'\nprintf '[]'\n",
                trusted_gh_marker.display()
            ),
        )
        .expect("trusted gh");
        fs::set_permissions(&trusted_gh, fs::Permissions::from_mode(0o755)).expect("permissions");
        let view = inspect_workspace(
            outer.path(),
            SessionWorkspace::git("repo", "unused", "main", "unused", "main"),
            12,
            &GitExecutables::from_resolved(
                Some(system_git),
                Some(fs::canonicalize(&trusted_gh).expect("trusted gh")),
            ),
        )
        .await;
        assert_eq!(view.status, GitWorkspaceStatus::Ready);
        assert_eq!(view.pull_request_lookup, PullRequestLookup::None);
        assert!(trusted_gh_marker.exists());
        assert!(!fake_git_marker.exists());
        assert!(!fake_gh_marker.exists());
    }

    #[test]
    fn pull_request_matching_requires_current_branch_and_head_oid() {
        let current = "a".repeat(40);
        let stale = "b".repeat(40);
        let rows = vec![
            GhPullRequest {
                number: 1,
                title: "wrong branch".to_string(),
                url: "https://example.test/1".to_string(),
                state: "OPEN".to_string(),
                is_draft: false,
                head_ref_name: "old-source".to_string(),
                head_ref_oid: current.clone(),
            },
            GhPullRequest {
                number: 2,
                title: "stale commit".to_string(),
                url: "https://example.test/2".to_string(),
                state: "CLOSED".to_string(),
                is_draft: false,
                head_ref_name: "current".to_string(),
                head_ref_oid: stale,
            },
            GhPullRequest {
                number: 3,
                title: "exact".to_string(),
                url: "https://example.test/3".to_string(),
                state: "OPEN".to_string(),
                is_draft: false,
                head_ref_name: "current".to_string(),
                head_ref_oid: current.clone(),
            },
        ];
        assert_eq!(
            select_pull_request(&rows, "current", &current)
                .expect("unambiguous")
                .map(|row| row.number),
            Some(3)
        );
        assert!(select_pull_request(&rows, "pi/session/internal", &current)
            .expect("no internal match")
            .is_none());
        let closed_exact = [GhPullRequest {
            number: 4,
            title: "closed exact".to_string(),
            url: "https://example.test/4".to_string(),
            state: "CLOSED".to_string(),
            is_draft: false,
            head_ref_name: "current".to_string(),
            head_ref_oid: current.clone(),
        }];
        assert!(select_pull_request(&closed_exact, "current", &current)
            .expect("closed PR must not match")
            .is_none());
        let mut ambiguous = rows;
        ambiguous.push(GhPullRequest {
            number: 5,
            title: "duplicate exact".to_string(),
            url: "https://example.test/5".to_string(),
            state: "OPEN".to_string(),
            is_draft: false,
            head_ref_name: "current".to_string(),
            head_ref_oid: current.clone(),
        });
        assert!(select_pull_request(&ambiguous, "current", &current).is_err());
    }

    #[tokio::test]
    async fn rejects_external_git_pointer_without_exposing_history() {
        let outer = TempDir::new().expect("outer");
        let external = TempDir::new().expect("external");
        let workspace = outer.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        git(external.path(), ["init", "-b", "main"]);
        git(external.path(), ["config", "user.name", "Outside Author"]);
        git(
            external.path(),
            ["config", "user.email", "outside@example.invalid"],
        );
        std::fs::write(external.path().join("secret"), "outside").expect("secret");
        git(external.path(), ["add", "."]);
        git(external.path(), ["commit", "-m", "External secret commit"]);
        std::fs::write(
            workspace.join(".git"),
            format!("gitdir: {}\n", external.path().join(".git").display()),
        )
        .expect("pointer");

        let view = inspect_workspace(
            outer.path(),
            SessionWorkspace::local("workspace", "unused"),
            12,
            &test_programs(),
        )
        .await;
        assert_eq!(view.status, GitWorkspaceStatus::Unavailable);
        assert!(view.commits.is_empty());
        assert!(!serde_json::to_string(&view)
            .expect("view")
            .contains("External secret commit"));
    }

    #[tokio::test]
    async fn inspects_branch_unborn_detached_history_and_non_git_workspace() {
        let outer = TempDir::new().expect("outer");
        let repository = outer.path().join("repo");
        let plain = outer.path().join("plain");
        std::fs::create_dir(&repository).expect("repository");
        std::fs::create_dir(&plain).expect("plain");
        git(&repository, ["init", "-b", "main"]);

        let unborn = inspect_workspace(
            outer.path(),
            SessionWorkspace::git("repo", "unused", "main", "unused", "main"),
            12,
            &test_programs(),
        )
        .await;
        assert_eq!(unborn.status, GitWorkspaceStatus::Ready);
        assert!(unborn.unborn);
        assert_eq!(unborn.branch.as_deref(), Some("main"));

        git(&repository, ["config", "user.name", "Test Author"]);
        git(
            &repository,
            ["config", "user.email", "test@example.invalid"],
        );
        std::fs::write(repository.join("one"), "one").expect("file");
        git(&repository, ["add", "."]);
        git(&repository, ["commit", "-m", "First"]);
        std::fs::write(repository.join("two"), "two").expect("file");
        git(&repository, ["add", "."]);
        git(&repository, ["commit", "-m", "Second"]);

        let ready = inspect_workspace(
            outer.path(),
            SessionWorkspace::git("repo", "unused", "main", "unused", "main"),
            1,
            &test_programs(),
        )
        .await;
        assert_eq!(ready.status, GitWorkspaceStatus::Ready);
        assert_eq!(ready.commits.len(), 1);
        assert_eq!(ready.commits[0].summary, "Second");
        assert!(ready.has_more);

        git(&repository, ["checkout", "--detach"]);
        let detached = inspect_workspace(
            outer.path(),
            SessionWorkspace::git("repo", "unused", "main", "unused", "main"),
            12,
            &test_programs(),
        )
        .await;
        assert!(detached.detached);
        assert!(!detached.unborn);

        let plain = inspect_workspace(
            outer.path(),
            SessionWorkspace::local("plain", "unused"),
            12,
            &test_programs(),
        )
        .await;
        assert_eq!(plain.status, GitWorkspaceStatus::NotGit);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn object_directory_swap_after_capability_open_never_exposes_external_history() {
        use std::os::unix::fs::symlink;

        let outer = TempDir::new().expect("outer");
        let repository = outer.path().join("repo");
        let external = TempDir::new().expect("external");
        fs::create_dir(&repository).expect("repository");
        for (path, summary) in [
            (repository.as_path(), "Internal safe commit"),
            (external.path(), "External secret commit"),
        ] {
            git(path, ["init", "-b", "main"]);
            git(path, ["config", "user.name", "Test Author"]);
            git(path, ["config", "user.email", "test@example.invalid"]);
            fs::write(path.join("file"), summary).expect("file");
            git(path, ["add", "."]);
            git(path, ["commit", "-m", summary]);
        }

        let git_dir = repository.join(".git");
        let original_objects = git_dir.join("objects");
        let moved_objects = git_dir.join("objects-opened-by-pi-web");
        let external_objects = external.path().join(".git/objects");
        let hook = std::sync::Arc::new(move || {
            fs::rename(&original_objects, &moved_objects).expect("move opened objects");
            symlink(&external_objects, &original_objects).expect("replace objects with symlink");
        });
        let view = inspect_workspace_with_hook(
            outer.path(),
            SessionWorkspace::git("repo", "unused", "main", "unused", "main"),
            12,
            &test_programs(),
            Some(hook),
        )
        .await;
        let json = serde_json::to_string(&view).expect("view");
        assert_eq!(view.status, GitWorkspaceStatus::Ready);
        assert!(json.contains("Internal safe commit"));
        assert!(!json.contains("External secret commit"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn malicious_signature_config_never_executes_through_inspection() {
        use std::os::unix::fs::PermissionsExt;

        let outer = TempDir::new().expect("outer");
        let repository = outer.path().join("repo");
        std::fs::create_dir(&repository).expect("repository");
        git(&repository, ["init", "-b", "main"]);
        write_signed_commit(&repository, "Safe signed commit");

        let marker = outer.path().join("signature-marker");
        let helper = outer.path().join("malicious-gpg");
        std::fs::write(
            &helper,
            format!("#!/bin/sh\nprintf owned >'{}'\nexit 1\n", marker.display()),
        )
        .expect("helper");
        let mut permissions = std::fs::metadata(&helper)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&helper, permissions).expect("helper permissions");
        git(
            &repository,
            [
                "config",
                "gpg.program",
                helper.to_str().expect("utf8 helper"),
            ],
        );
        git(&repository, ["config", "log.showSignature", "true"]);
        git(&repository, ["log", "-1", "--format=%s"]);
        assert!(marker.exists(), "test config did not invoke its helper");
        std::fs::remove_file(&marker).expect("clear proof marker");

        let view = inspect_workspace(
            outer.path(),
            SessionWorkspace::git("repo", "unused", "main", "unused", "main"),
            12,
            &test_programs(),
        )
        .await;

        assert_eq!(view.status, GitWorkspaceStatus::Unavailable);
        assert!(
            !marker.exists(),
            "repository-controlled signature helper executed"
        );
    }

    #[tokio::test]
    async fn missing_git_executable_makes_repository_unavailable() {
        let outer = TempDir::new().expect("outer");
        let repository = outer.path().join("repo");
        std::fs::create_dir(&repository).expect("repository");
        git(&repository, ["init", "-b", "main"]);

        let view = inspect_workspace(
            outer.path(),
            SessionWorkspace::git("repo", "unused", "main", "unused", "main"),
            12,
            &GitExecutables::from_resolved(
                Some(std::env::temp_dir().join("definitely-missing-pi-web-git")),
                None,
            ),
        )
        .await;

        assert_eq!(view.status, GitWorkspaceStatus::Unavailable);
        assert_eq!(view.error.as_deref(), Some("git is not available."));
        assert!(view.commits.is_empty());
    }

    #[tokio::test]
    async fn serialized_status_never_contains_long_remote_credentials() {
        let outer = TempDir::new().expect("outer");
        let repository = outer.path().join("repo");
        std::fs::create_dir(&repository).expect("repository");
        git(&repository, ["init", "-b", "main"]);
        let secret = "serialized-secret-".repeat(80);
        let remote = format!("https://{secret}@github.com/owner/repo.git?token={secret}");
        git(
            &repository,
            ["config", "remote.origin.url", remote.as_str()],
        );

        let response = session_git_status(
            "session",
            SessionGitConfig {
                outer_cwd: outer.path().display().to_string(),
                workspaces: vec![SessionWorkspace::git(
                    "repo", "unused", "main", "unused", "main",
                )],
            },
            12,
            std::sync::Arc::new(Semaphore::new(1)),
            test_programs(),
        )
        .await;
        let json = serde_json::to_string(&response).expect("serialize status");
        assert!(!json.contains("serialized-secret-"));
        assert!(json.contains("https://github.com/owner/repo.git"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_terminates_command_descendants() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("temp");
        let sleep = startup_path_candidates("sleep")
            .into_iter()
            .find_map(|path| fs::canonicalize(path).ok())
            .expect("sleep executable");
        let child_path =
            std::env::join_paths([sleep.parent().expect("sleep parent")]).expect("child path");
        let timeout_error = run_command(
            Some(&sleep),
            &child_path,
            "git",
            [OsString::from("30")],
            temp.path(),
            Duration::from_millis(100),
            CommandEnvironment::Git,
        )
        .await
        .expect_err("sleep must time out");
        assert!(matches!(timeout_error, CommandError::TimedOut("git")));

        let child_started = temp.path().join("child-started");
        let child_marker = temp.path().join("child-finished");
        let helper = temp.path().join("spawner");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\n(printf started >'{}'; sleep 2; printf survived >'{}') &\n\
                 while [ ! -f '{}' ]; do sleep 0.01; done\nsleep 30\n",
                child_started.display(),
                child_marker.display(),
                child_started.display(),
            ),
        )
        .expect("helper");
        let mut permissions = std::fs::metadata(&helper)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&helper, permissions).expect("helper permissions");

        let error = run_command(
            Some(&helper),
            &child_path,
            "git",
            std::iter::empty(),
            temp.path(),
            Duration::from_millis(500),
            CommandEnvironment::Git,
        )
        .await
        .expect_err("helper must be cleaned up");
        assert!(
            child_started.exists(),
            "descendant never started before cleanup: {error:?}"
        );
        assert!(
            matches!(
                error,
                CommandError::TimedOut("git") | CommandError::Failed("git")
            ),
            "expected timeout/wait cleanup, received {error:?}"
        );
        tokio::time::sleep(Duration::from_millis(2_300)).await;
        assert!(
            !child_marker.exists(),
            "descendant survived process-group cleanup"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_command_exit_terminates_redirected_descendant() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("temp");
        let marker = temp.path().join("descendant-marker");
        let helper = temp.path().join("successful-spawner");
        fs::write(
            &helper,
            format!(
                "#!/bin/sh\n(sleep 1; printf survived >'{}') </dev/null >/dev/null 2>&1 &\nexit 0\n",
                marker.display()
            ),
        )
        .expect("helper");
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o644)).expect("permissions");
        let shell = resolved_test_program("sh");
        let sleep = resolved_test_program("sleep");
        let child_path = std::env::join_paths([
            shell.parent().expect("shell parent"),
            sleep.parent().expect("sleep parent"),
        ])
        .expect("path");

        let output = run_command(
            Some(&shell),
            &child_path,
            "git",
            [helper.into_os_string()],
            temp.path(),
            Duration::from_secs(2),
            CommandEnvironment::Git,
        )
        .await
        .expect("successful direct child");
        assert!(output.status.success());
        tokio::time::sleep(Duration::from_millis(1_300)).await;
        assert!(
            !marker.exists(),
            "same-group descendant survived successful child cleanup"
        );
    }

    fn write_signed_commit(repository: &Path, summary: &str) {
        let tree = git_stdout(repository, ["mktree"], Some(b""));
        let timestamp = "1700000000 +0000";
        let commit = format!(
            "tree {tree}\nauthor Test <test@example.invalid> {timestamp}\n\
             committer Test <test@example.invalid> {timestamp}\n\
             gpgsig -----BEGIN PGP SIGNATURE-----\n \n fake\n \
             -----END PGP SIGNATURE-----\n\n{summary}\n"
        );
        let sha = git_stdout(
            repository,
            ["hash-object", "-t", "commit", "-w", "--stdin"],
            Some(commit.as_bytes()),
        );
        git(repository, ["update-ref", "HEAD", &sha]);
    }

    fn git_stdout<const N: usize>(cwd: &Path, args: [&str; N], stdin: Option<&[u8]>) -> String {
        use std::io::Write;
        use std::process::Stdio;

        let mut child = StdCommand::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("git");
        if let Some(stdin) = stdin {
            child
                .stdin
                .take()
                .expect("git stdin")
                .write_all(stdin)
                .expect("write git stdin");
        }
        let output = child.wait_with_output().expect("git output");
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git utf8")
            .trim()
            .to_string()
    }

    fn git<const N: usize>(cwd: &Path, args: [&str; N]) {
        let output = StdCommand::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
