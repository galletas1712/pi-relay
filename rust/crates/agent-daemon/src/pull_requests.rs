use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};

use agent_store::{SessionSummary, SessionWorkspace};
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{process::Command, time::timeout};

use crate::state::AppState;

const CACHE_TTL: Duration = Duration::from_secs(60);
const GIT_TIMEOUT: Duration = Duration::from_secs(5);
const GH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONCURRENT_SESSION_REFRESHES: usize = 4;
const MAX_PULL_REQUESTS_PER_BRANCH: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct PullRequestSummary {
    pub(crate) number: u64,
    pub(crate) status: PullRequestStatus,
    pub(crate) url: String,
    pub(crate) workspace_dirs: Vec<String>,
    pub(crate) source_repository: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PullRequestStatus {
    Draft,
    Open,
    Merged,
}

/// Stale-while-revalidate cache for sidebar pull-request metadata.
///
/// `session.list` is polled frequently and must never wait on Git or GitHub.
/// Callers receive the last complete value immediately while one coalesced
/// worker refreshes stale session workspaces in the background.
#[derive(Clone, Default)]
pub(crate) struct PullRequestScheduler {
    inner: Arc<StdMutex<SchedulerState>>,
}

#[derive(Default)]
struct SchedulerState {
    cache: HashMap<String, CachedPullRequests>,
    pending: BTreeMap<String, SessionWorkspaceContext>,
    in_flight: HashMap<String, SessionWorkspaceContext>,
    worker_running: bool,
}

struct CachedPullRequests {
    context: SessionWorkspaceContext,
    pull_requests: Vec<PullRequestSummary>,
    refreshed_at: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionWorkspaceContext {
    session_id: String,
    outer_cwd: PathBuf,
    workspaces: Vec<SessionWorkspace>,
}

impl SessionWorkspaceContext {
    fn from_summary(summary: &SessionSummary) -> Self {
        Self {
            session_id: summary.session_id.clone(),
            outer_cwd: PathBuf::from(&summary.outer_cwd),
            workspaces: summary.workspaces.clone(),
        }
    }

    fn has_workspace(&self) -> bool {
        !self.workspaces.is_empty()
    }
}

impl PullRequestScheduler {
    pub(crate) fn cached_and_schedule(
        &self,
        state: &AppState,
        sessions: &[SessionSummary],
    ) -> HashMap<String, Vec<PullRequestSummary>> {
        let now = Instant::now();
        let mut cached = HashMap::new();
        let should_spawn = {
            let mut scheduler = self
                .inner
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            for session in sessions {
                let context = SessionWorkspaceContext::from_summary(session);
                if !context.has_workspace() {
                    continue;
                }
                let fresh_cache = scheduler
                    .cache
                    .get(&session.session_id)
                    .filter(|entry| entry.context == context);
                if let Some(entry) = fresh_cache {
                    cached.insert(session.session_id.clone(), entry.pull_requests.clone());
                }
                let is_fresh = fresh_cache
                    .is_some_and(|entry| now.duration_since(entry.refreshed_at) < CACHE_TTL);
                let already_refreshing = scheduler
                    .in_flight
                    .get(&session.session_id)
                    .is_some_and(|current| current == &context);
                if !is_fresh && !already_refreshing {
                    scheduler
                        .pending
                        .insert(session.session_id.clone(), context);
                }
            }
            let should_spawn = !scheduler.worker_running && !scheduler.pending.is_empty();
            if should_spawn {
                scheduler.worker_running = true;
            }
            should_spawn
        };

        if should_spawn {
            self.spawn_worker(state);
        }
        cached
    }

    pub(crate) fn remove_sessions<'a>(&self, session_ids: impl IntoIterator<Item = &'a String>) {
        let mut scheduler = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for session_id in session_ids {
            scheduler.cache.remove(session_id);
            scheduler.pending.remove(session_id);
            scheduler.in_flight.remove(session_id);
        }
    }

    fn spawn_worker(&self, state: &AppState) {
        let scheduler = self.clone();
        let task_state = state.clone();
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            if start_rx.await.is_err() {
                scheduler.worker_stopped();
                return;
            }
            scheduler.run_worker().await;
        });
        if crate::runtime::register_auxiliary_task(&task_state, handle, start_tx).is_err() {
            self.worker_stopped();
        }
    }

    async fn run_worker(&self) {
        loop {
            let batch = {
                let mut scheduler = self
                    .inner
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if scheduler.pending.is_empty() {
                    scheduler.worker_running = false;
                    return;
                }
                let session_ids = scheduler
                    .pending
                    .keys()
                    .take(MAX_CONCURRENT_SESSION_REFRESHES)
                    .cloned()
                    .collect::<Vec<_>>();
                session_ids
                    .into_iter()
                    .filter_map(|session_id| {
                        let context = scheduler.pending.remove(&session_id)?;
                        scheduler.in_flight.insert(session_id, context.clone());
                        Some(context)
                    })
                    .collect::<Vec<_>>()
            };

            stream::iter(batch)
                .for_each_concurrent(MAX_CONCURRENT_SESSION_REFRESHES, |context| async move {
                    let pull_requests =
                        discover_pull_requests_with(&context, OsStr::new("git"), OsStr::new("gh"))
                            .await;
                    self.complete_refresh(context, pull_requests);
                })
                .await;
        }
    }

    fn complete_refresh(
        &self,
        context: SessionWorkspaceContext,
        pull_requests: Vec<PullRequestSummary>,
    ) {
        let mut scheduler = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !scheduler
            .in_flight
            .get(&context.session_id)
            .is_some_and(|current| current == &context)
        {
            return;
        }
        scheduler.in_flight.remove(&context.session_id);
        scheduler.cache.insert(
            context.session_id.clone(),
            CachedPullRequests {
                context,
                pull_requests,
                refreshed_at: Instant::now(),
            },
        );
    }

    fn worker_stopped(&self) {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .worker_running = false;
    }
}

async fn discover_pull_requests_with(
    context: &SessionWorkspaceContext,
    git: &OsStr,
    gh: &OsStr,
) -> Vec<PullRequestSummary> {
    let mut pull_requests = BTreeMap::new();
    let Ok(outer_cwd) = tokio::fs::canonicalize(&context.outer_cwd).await else {
        return Vec::new();
    };
    let mut workspaces = context.workspaces.iter().collect::<Vec<_>>();
    workspaces.sort_by(|left, right| left.workspace_dir.cmp(&right.workspace_dir));
    for workspace in workspaces {
        if crate::workspaces::validate_workspace_dir(&workspace.workspace_dir).is_err() {
            continue;
        }
        let Some(repository) =
            isolated_git_repository(&outer_cwd.join(&workspace.workspace_dir)).await
        else {
            continue;
        };
        if repository.work_tree.parent() != Some(outer_cwd.as_path()) {
            continue;
        }

        for source in checked_out_remote_branches(&repository, git).await {
            for mut pull_request in list_pull_requests(&repository.work_tree, &source, gh).await {
                pull_request
                    .workspace_dirs
                    .push(workspace.workspace_dir.clone());
                pull_requests
                    .entry(canonical_pull_request_url(&pull_request.url))
                    .and_modify(|existing: &mut PullRequestSummary| {
                        existing
                            .workspace_dirs
                            .extend(pull_request.workspace_dirs.iter().cloned());
                        existing.workspace_dirs.sort();
                        existing.workspace_dirs.dedup();
                    })
                    .or_insert(pull_request);
            }
        }
    }

    let mut pull_requests = pull_requests.into_values().collect::<Vec<_>>();
    pull_requests.sort_by(|left, right| {
        left.number
            .cmp(&right.number)
            .then_with(|| left.url.cmp(&right.url))
    });
    pull_requests
}

#[derive(Clone, Debug)]
struct IsolatedGitRepository {
    work_tree: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
}

async fn isolated_git_repository(workspace_root: &Path) -> Option<IsolatedGitRepository> {
    let work_tree = tokio::fs::canonicalize(workspace_root).await.ok()?;
    let git_dir = tokio::fs::canonicalize(work_tree.join(".git")).await.ok()?;
    let git_dir_metadata = tokio::fs::metadata(&git_dir).await.ok()?;
    if !git_dir_metadata.is_dir() || !git_dir.starts_with(&work_tree) {
        return None;
    };

    let commondir_file = git_dir.join("commondir");
    let common_dir = match tokio::fs::symlink_metadata(&commondir_file).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_dir.clone(),
        Err(_) => return None,
        Ok(_) => {
            let Ok(commondir_file) = tokio::fs::canonicalize(&commondir_file).await else {
                return None;
            };
            let Ok(commondir_metadata) = tokio::fs::metadata(&commondir_file).await else {
                return None;
            };
            if !commondir_metadata.is_file() || !commondir_file.starts_with(&work_tree) {
                return None;
            }
            let Ok(contents) = tokio::fs::read_to_string(commondir_file).await else {
                return None;
            };
            let common_dir = contents.strip_suffix('\n').unwrap_or(&contents);
            let common_dir = common_dir.strip_suffix('\r').unwrap_or(common_dir);
            if common_dir.is_empty() || common_dir.contains('\r') || common_dir.contains('\n') {
                return None;
            }
            let common_dir = Path::new(common_dir);
            let common_dir = if common_dir.is_absolute() {
                common_dir.to_path_buf()
            } else {
                git_dir.join(common_dir)
            };
            let Ok(common_dir) = tokio::fs::canonicalize(common_dir).await else {
                return None;
            };
            common_dir
        }
    };
    if !tokio::fs::metadata(&common_dir)
        .await
        .is_ok_and(|metadata| metadata.is_dir() && common_dir.starts_with(&work_tree))
    {
        return None;
    }
    Some(IsolatedGitRepository {
        work_tree,
        git_dir,
        common_dir,
    })
}

#[derive(Clone, Debug)]
struct RepositoryRemote {
    name: String,
    url: String,
    repository: GitHubRepository,
}

#[derive(Clone, Debug)]
struct RemoteBranch {
    repository: GitHubRepository,
    branch: String,
}

#[derive(Clone, Debug)]
struct GitHubRepository {
    hostname: String,
    owner: String,
    name: String,
    name_with_owner: String,
}

async fn repository_remotes(
    git: &OsStr,
    repository: &IsolatedGitRepository,
) -> (Vec<String>, Vec<RepositoryRemote>) {
    let Ok(entries) = local_git_config_entries(git, repository).await else {
        return (Vec::new(), Vec::new());
    };
    let mut configs = BTreeMap::<String, RemoteConfig>::new();
    for (key, value) in entries {
        let Some((section, remainder)) = key.split_once('.') else {
            continue;
        };
        if !section.eq_ignore_ascii_case("remote") {
            continue;
        }
        let Some((name, field)) = remainder.rsplit_once('.') else {
            continue;
        };
        let field = if field.eq_ignore_ascii_case("pushurl") {
            RemoteUrlField::Push
        } else if field.eq_ignore_ascii_case("url") {
            RemoteUrlField::Fetch
        } else {
            continue;
        };
        if name.is_empty() || name.trim() != name {
            return (Vec::new(), Vec::new());
        }
        let config = configs.entry(name.to_string()).or_default();
        match field {
            RemoteUrlField::Push => config.push_urls.push(value),
            RemoteUrlField::Fetch => config.urls.push(value),
        }
    }

    let remote_names = configs.keys().cloned().collect::<Vec<_>>();
    let mut urls = BTreeMap::new();
    for (name, config) in configs {
        let remote_urls = if config.push_urls.is_empty() {
            config.urls
        } else {
            config.push_urls
        };
        for url in remote_urls {
            let Some(repository) = github_repository(&url) else {
                continue;
            };
            urls.entry((name.clone(), normalized_remote(&url)))
                .or_insert_with(|| RepositoryRemote {
                    name: name.clone(),
                    url,
                    repository,
                });
        }
    }
    (remote_names, urls.into_values().collect())
}

#[derive(Default)]
struct RemoteConfig {
    urls: Vec<String>,
    push_urls: Vec<String>,
}

enum RemoteUrlField {
    Fetch,
    Push,
}

async fn checked_out_remote_branches(
    repository: &IsolatedGitRepository,
    git: &OsStr,
) -> Vec<RemoteBranch> {
    let Some(checked_out_branch) = git_stdout(
        git,
        repository,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )
    .await
    else {
        return Vec::new();
    };
    if !valid_branch_name(&checked_out_branch) {
        return Vec::new();
    }

    let (remote_names, remotes) = repository_remotes(git, repository).await;
    if remote_names.is_empty() || remotes.is_empty() {
        return Vec::new();
    }

    let destinations =
        match push_destinations(git, repository, &checked_out_branch, &remote_names).await {
            Ok(destinations) => destinations,
            Err(()) => return Vec::new(),
        };

    let mut candidates = BTreeMap::new();
    for remote in remotes {
        let Some(branch) = destinations.get(&remote.name) else {
            continue;
        };
        if branch.is_empty() {
            continue;
        }
        candidates
            .entry((normalized_remote(&remote.url), branch.clone()))
            .or_insert_with(|| RemoteBranch {
                repository: remote.repository.clone(),
                branch: branch.clone(),
            });
    }

    candidates.into_values().collect()
}

async fn push_destinations(
    git: &OsStr,
    repository: &IsolatedGitRepository,
    checked_out_branch: &str,
    remote_names: &[String],
) -> Result<BTreeMap<String, String>, ()> {
    let branch_key = format!("branch.{checked_out_branch}.pushRemote");
    let explicit_remote = match local_git_config(git, repository, &branch_key).await? {
        Some(remote) => Some(remote),
        None => local_git_config(git, repository, "remote.pushDefault").await?,
    };
    if let Some(remote) = explicit_remote {
        if remote.is_empty() {
            return Err(());
        }
        let branch = match local_push_branch(git, repository, &remote, checked_out_branch).await? {
            PushRefspecResolution::Absent => checked_out_branch.to_string(),
            PushRefspecResolution::Destination(branch) => branch,
            PushRefspecResolution::Unusable => return Err(()),
        };
        return Ok(BTreeMap::from([(remote, branch)]));
    }

    let mut refspec_destinations = BTreeSet::new();
    let mut default_destinations = BTreeMap::new();
    for remote in remote_names {
        match local_push_branch(git, repository, remote, checked_out_branch).await? {
            PushRefspecResolution::Absent => {
                default_destinations.insert(remote.clone(), checked_out_branch.to_string());
            }
            PushRefspecResolution::Destination(branch) => {
                refspec_destinations.insert((remote.clone(), branch));
            }
            PushRefspecResolution::Unusable => return Err(()),
        }
    }
    if refspec_destinations.len() > 1 {
        return Err(());
    }
    if let Some(destination) = refspec_destinations.into_iter().next() {
        return Ok(BTreeMap::from([destination]));
    }
    Ok(default_destinations)
}

#[derive(Debug, PartialEq, Eq)]
enum PushRefspecResolution {
    Absent,
    Destination(String),
    Unusable,
}

async fn local_push_branch(
    git: &OsStr,
    repository: &IsolatedGitRepository,
    remote: &str,
    checked_out_branch: &str,
) -> Result<PushRefspecResolution, ()> {
    let key = format!("remote.{remote}.push");
    let refspecs = local_git_config_values(git, repository, &key).await?;
    if refspecs.is_empty() {
        return Ok(PushRefspecResolution::Absent);
    }
    let mut destinations = BTreeSet::new();
    for refspec in refspecs {
        match push_refspec_destination(&refspec, checked_out_branch) {
            RefspecMatch::Destination(destination) => {
                destinations.insert(destination);
            }
            RefspecMatch::Unrelated => {}
            RefspecMatch::Malformed => return Ok(PushRefspecResolution::Unusable),
        }
    }
    if destinations.len() > 1 {
        return Ok(PushRefspecResolution::Unusable);
    }
    Ok(destinations
        .pop_first()
        .map(PushRefspecResolution::Destination)
        .unwrap_or(PushRefspecResolution::Unusable))
}

#[derive(Debug, PartialEq, Eq)]
enum RefspecMatch {
    Destination(String),
    Unrelated,
    Malformed,
}

fn push_refspec_destination(refspec: &str, checked_out_branch: &str) -> RefspecMatch {
    let refspec = refspec.strip_prefix('+').unwrap_or(refspec);
    if refspec.is_empty() || refspec.starts_with('^') || refspec.trim() != refspec {
        return RefspecMatch::Malformed;
    }
    if refspec == ":" {
        return RefspecMatch::Destination(checked_out_branch.to_string());
    }
    let Some((source, destination)) = refspec.split_once(':') else {
        return match exact_push_source(refspec, checked_out_branch) {
            SourceMatch::Matches => RefspecMatch::Destination(checked_out_branch.to_string()),
            SourceMatch::Unrelated => RefspecMatch::Unrelated,
            SourceMatch::Malformed => RefspecMatch::Malformed,
        };
    };
    if destination.contains(':') || destination.is_empty() {
        return RefspecMatch::Malformed;
    }
    if source.is_empty() {
        return if destination_branch(destination).is_some() {
            RefspecMatch::Unrelated
        } else {
            RefspecMatch::Malformed
        };
    }

    let source_wildcard = split_wildcard(source);
    let destination_wildcard = split_wildcard(destination);
    if source.contains('*') || destination.contains('*') {
        let (Some((source_prefix, source_suffix)), Some((destination_prefix, destination_suffix))) =
            (source_wildcard, destination_wildcard)
        else {
            return RefspecMatch::Malformed;
        };
        let source_candidate = if source.starts_with("refs/") {
            format!("refs/heads/{checked_out_branch}")
        } else {
            checked_out_branch.to_string()
        };
        if !valid_refspec_pattern(source, source.starts_with("refs/"))
            || !valid_refspec_pattern(destination, destination.starts_with("refs/"))
        {
            return RefspecMatch::Malformed;
        }
        let Some(matched) = source_candidate
            .strip_prefix(source_prefix)
            .and_then(|value| value.strip_suffix(source_suffix))
        else {
            return RefspecMatch::Unrelated;
        };
        let destination = format!("{destination_prefix}{matched}{destination_suffix}");
        match destination_branch(&destination) {
            Some(branch) => RefspecMatch::Destination(branch),
            None => RefspecMatch::Malformed,
        }
    } else {
        match exact_push_source(source, checked_out_branch) {
            SourceMatch::Matches => match destination_branch(destination) {
                Some(branch) => RefspecMatch::Destination(branch),
                None => RefspecMatch::Malformed,
            },
            SourceMatch::Unrelated => {
                if destination_branch(destination).is_some() {
                    RefspecMatch::Unrelated
                } else {
                    RefspecMatch::Malformed
                }
            }
            SourceMatch::Malformed => RefspecMatch::Malformed,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SourceMatch {
    Matches,
    Unrelated,
    Malformed,
}

fn exact_push_source(source: &str, checked_out_branch: &str) -> SourceMatch {
    if matches!(source, "HEAD" | "@") {
        return SourceMatch::Matches;
    }
    if let Some(branch) = source.strip_prefix("refs/heads/") {
        return if !valid_branch_name(branch) {
            SourceMatch::Malformed
        } else if branch == checked_out_branch {
            SourceMatch::Matches
        } else {
            SourceMatch::Unrelated
        };
    }
    if source.starts_with("refs/") {
        return if valid_ref_name(source) {
            SourceMatch::Unrelated
        } else {
            SourceMatch::Malformed
        };
    }
    if !valid_branch_name(source) {
        SourceMatch::Malformed
    } else if source == checked_out_branch {
        SourceMatch::Matches
    } else {
        SourceMatch::Unrelated
    }
}

fn destination_branch(destination: &str) -> Option<String> {
    let branch = if let Some(branch) = destination.strip_prefix("refs/heads/") {
        branch
    } else if destination.starts_with("refs/") {
        return None;
    } else {
        destination
    };
    valid_branch_name(branch).then(|| branch.to_string())
}

fn valid_refspec_pattern(pattern: &str, qualified: bool) -> bool {
    let Some((prefix, suffix)) = split_wildcard(pattern) else {
        return false;
    };
    let example = format!("{prefix}wildcard{suffix}");
    if qualified {
        valid_ref_name(&example)
    } else {
        valid_branch_name(&example)
    }
}

fn valid_branch_name(branch: &str) -> bool {
    !branch.starts_with('-') && valid_ref_name(&format!("refs/heads/{branch}"))
}

fn valid_ref_name(reference: &str) -> bool {
    if reference.is_empty()
        || reference == "@"
        || reference.starts_with('/')
        || reference.ends_with('/')
        || reference.ends_with('.')
        || reference.contains("//")
        || reference.contains("..")
        || reference.contains("@{")
        || reference
            .bytes()
            .any(|byte| byte <= b' ' || byte == 0x7f || b"~^:?*[\\".contains(&byte))
    {
        return false;
    }
    reference.split('/').all(|component| {
        !component.is_empty() && !component.starts_with('.') && !component.ends_with(".lock")
    })
}

fn split_wildcard(pattern: &str) -> Option<(&str, &str)> {
    let (prefix, suffix) = pattern.split_once('*')?;
    (!suffix.contains('*')).then_some((prefix, suffix))
}

async fn local_git_config(
    git: &OsStr,
    repository: &IsolatedGitRepository,
    key: &str,
) -> Result<Option<String>, ()> {
    let mut values = local_git_config_values(git, repository, key).await?;
    if values.len() > 1 {
        return Err(());
    }
    Ok(values.pop())
}

async fn local_git_config_values(
    git: &OsStr,
    repository: &IsolatedGitRepository,
    key: &str,
) -> Result<Vec<String>, ()> {
    let output = git_command_output(
        git,
        repository,
        &[
            "config",
            "--local",
            "--no-includes",
            "--null",
            "--get-all",
            "--",
            key,
        ],
        GIT_TIMEOUT,
    )
    .await
    .ok_or(())?;
    if output.status.success() {
        let Some(values) = output.stdout.strip_suffix(b"\0") else {
            return Err(());
        };
        return values
            .split(|byte| *byte == b'\0')
            .map(|value| {
                std::str::from_utf8(value)
                    .map(str::to_string)
                    .map_err(|_| ())
            })
            .collect();
    }
    if output.status.code() == Some(1) {
        return Ok(Vec::new());
    }
    Err(())
}

async fn local_git_config_entries(
    git: &OsStr,
    repository: &IsolatedGitRepository,
) -> Result<Vec<(String, String)>, ()> {
    let output = git_command_output(
        git,
        repository,
        &["config", "--local", "--no-includes", "--null", "--list"],
        GIT_TIMEOUT,
    )
    .await
    .ok_or(())?;
    if !output.status.success() {
        return Err(());
    }
    parse_local_git_config_entries(&output.stdout)
}

fn parse_local_git_config_entries(stdout: &[u8]) -> Result<Vec<(String, String)>, ()> {
    let Some(entries) = stdout.strip_suffix(b"\0") else {
        return if stdout.is_empty() {
            Ok(Vec::new())
        } else {
            Err(())
        };
    };
    entries
        .split(|byte| *byte == b'\0')
        .map(|entry| {
            let Some(separator) = entry.iter().position(|byte| *byte == b'\n') else {
                return Err(());
            };
            let key = std::str::from_utf8(&entry[..separator]).map_err(|_| ())?;
            let value = std::str::from_utf8(&entry[separator + 1..]).map_err(|_| ())?;
            if key.is_empty() {
                return Err(());
            }
            Ok((key.to_string(), value.to_string()))
        })
        .collect()
}

async fn list_pull_requests(
    workspace_root: &Path,
    source: &RemoteBranch,
    gh: &OsStr,
) -> Vec<PullRequestSummary> {
    let limit = MAX_PULL_REQUESTS_PER_BRANCH.to_string();
    let qualified_name = format!("refs/heads/{}", source.branch);
    let owner = format!("owner={}", source.repository.owner);
    let name = format!("name={}", source.repository.name);
    let qualified_name = format!("qualifiedName={qualified_name}");
    let first = format!("first={limit}");
    let Some(stdout) = command_stdout(
        gh,
        workspace_root,
        &[
            "api",
            "graphql",
            "--hostname",
            &source.repository.hostname,
            "-f",
            &format!("query={ASSOCIATED_PULL_REQUESTS_QUERY}"),
            "-f",
            &owner,
            "-f",
            &name,
            "-f",
            &qualified_name,
            "-F",
            &first,
        ],
        GH_TIMEOUT,
    )
    .await
    else {
        return Vec::new();
    };
    parse_pull_requests(&stdout, &source.repository, &source.branch)
}

const ASSOCIATED_PULL_REQUESTS_QUERY: &str = r#"query PullRequestsForBranch($owner: String!, $name: String!, $qualifiedName: String!, $first: Int!) {
  repository(owner: $owner, name: $name) {
    ref(qualifiedName: $qualifiedName) {
      associatedPullRequests(first: $first, states: [OPEN, MERGED]) {
        nodes {
          number
          state
          isDraft
          url
          headRefName
          headRepository {
            nameWithOwner
          }
        }
      }
    }
  }
}"#;

#[derive(Deserialize)]
struct GhPullRequest {
    number: u64,
    state: String,
    #[serde(rename = "isDraft")]
    is_draft: bool,
    url: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    #[serde(rename = "headRepository")]
    head_repository: Option<GhRepository>,
}

#[derive(Deserialize)]
struct GhRepository {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

#[derive(Deserialize)]
struct GhGraphQlResponse {
    data: Option<GhGraphQlData>,
}

#[derive(Deserialize)]
struct GhGraphQlData {
    repository: Option<GhGraphQlRepository>,
}

#[derive(Deserialize)]
struct GhGraphQlRepository {
    #[serde(rename = "ref")]
    reference: Option<GhGraphQlRef>,
}

#[derive(Deserialize)]
struct GhGraphQlRef {
    #[serde(rename = "associatedPullRequests")]
    associated_pull_requests: GhPullRequestConnection,
}

#[derive(Deserialize)]
struct GhPullRequestConnection {
    nodes: Vec<GhPullRequest>,
}

fn parse_pull_requests(
    stdout: &str,
    source_repository: &GitHubRepository,
    source_branch: &str,
) -> Vec<PullRequestSummary> {
    let Ok(response) = serde_json::from_str::<GhGraphQlResponse>(stdout) else {
        return Vec::new();
    };
    let Some(pull_requests) = response
        .data
        .and_then(|data| data.repository)
        .and_then(|repository| repository.reference)
        .map(|reference| reference.associated_pull_requests.nodes)
    else {
        return Vec::new();
    };
    pull_requests
        .into_iter()
        .filter_map(|pull_request| {
            if pull_request.head_ref_name != source_branch
                || !pull_request
                    .head_repository
                    .as_ref()
                    .is_some_and(|repository| {
                        repository
                            .name_with_owner
                            .eq_ignore_ascii_case(&source_repository.name_with_owner)
                    })
            {
                return None;
            }
            let status = if pull_request.state.eq_ignore_ascii_case("MERGED") {
                PullRequestStatus::Merged
            } else if pull_request.state.eq_ignore_ascii_case("OPEN") && pull_request.is_draft {
                PullRequestStatus::Draft
            } else if pull_request.state.eq_ignore_ascii_case("OPEN") {
                PullRequestStatus::Open
            } else {
                return None;
            };
            Some(PullRequestSummary {
                number: pull_request.number,
                status,
                url: pull_request.url,
                workspace_dirs: Vec::new(),
                source_repository: source_repository.name_with_owner.clone(),
            })
        })
        .collect()
}

fn github_repository(remote_url: &str) -> Option<GitHubRepository> {
    if !has_safe_remote_transport(remote_url) {
        return None;
    }
    let normalized = normalized_remote(remote_url);
    let mut parts = normalized.split('/');
    let hostname = parts.next()?.to_string();
    let owner = parts.next()?.to_string();
    let name = parts.next()?.to_string();
    if hostname.is_empty() || owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(GitHubRepository {
        hostname,
        name_with_owner: format!("{owner}/{name}"),
        owner,
        name,
    })
}

fn has_safe_remote_transport(remote_url: &str) -> bool {
    if remote_url.is_empty()
        || remote_url.trim() != remote_url
        || remote_url
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return false;
    }
    if let Some((scheme, rest)) = remote_url.split_once("://") {
        return matches!(
            scheme.to_ascii_lowercase().as_str(),
            "http" | "https" | "ssh" | "git"
        ) && !rest.is_empty();
    }
    remote_url.split_once(':').is_some_and(|(host, path)| {
        !host.is_empty() && !host.contains('/') && !path.starts_with(':')
    })
}

fn canonical_pull_request_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_ascii_lowercase()
}

fn normalized_remote(remote_url: &str) -> String {
    let mut value = remote_url.trim().trim_end_matches('/').to_ascii_lowercase();
    if let Some(rest) = value.strip_prefix("ssh://") {
        value = rest.to_string();
    } else if let Some((_, rest)) = value.split_once("://") {
        value = rest.to_string();
    }
    if let Some((_, rest)) = value.split_once('@') {
        value = rest.to_string();
    }
    if let Some((host, path)) = value.split_once(':') {
        if !path.contains(':') {
            value = format!("{host}/{path}");
        }
    }
    value.trim_end_matches(".git").to_string()
}

async fn git_stdout(
    git: &OsStr,
    repository: &IsolatedGitRepository,
    args: &[&str],
) -> Option<String> {
    let output = git_command_output(git, repository, args, GIT_TIMEOUT).await?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn git_command_output(
    git: &OsStr,
    repository: &IsolatedGitRepository,
    args: &[&str],
    command_timeout: Duration,
) -> Option<std::process::Output> {
    let mut command = Command::new(git);
    command
        .args(args)
        .current_dir(&repository.work_tree)
        .kill_on_drop(true)
        .env("GIT_DIR", &repository.git_dir)
        .env("GIT_WORK_TREE", &repository.work_tree)
        .env("GIT_COMMON_DIR", &repository.common_dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("PAGER", "cat");
    timeout(command_timeout, command.output()).await.ok()?.ok()
}

async fn command_stdout(
    program: &OsStr,
    cwd: &Path,
    args: &[&str],
    command_timeout: Duration,
) -> Option<String> {
    let output = command_output(program, cwd, args, command_timeout).await?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn command_output(
    program: &OsStr,
    cwd: &Path,
    args: &[&str],
    command_timeout: Duration,
) -> Option<std::process::Output> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .kill_on_drop(true)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("PAGER", "cat");
    timeout(command_timeout, command.output()).await.ok()?.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn parses_supported_statuses_and_ignores_closed_pull_requests() {
        let repository =
            github_repository("https://github.com/owner/repo.git").expect("GitHub repository");
        let pull_requests = parse_pull_requests(
            r#"{"data":{"repository":{"ref":{"associatedPullRequests":{"nodes":[
                {"number":4,"state":"OPEN","isDraft":true,"url":"https://example.test/pull/4","headRefName":"feature","headRepository":{"nameWithOwner":"owner/repo"}},
                {"number":7,"state":"OPEN","isDraft":false,"url":"https://example.test/pull/7","headRefName":"feature","headRepository":{"nameWithOwner":"owner/repo"}},
                {"number":9,"state":"MERGED","isDraft":false,"url":"https://example.test/pull/9","headRefName":"feature","headRepository":{"nameWithOwner":"owner/repo"}},
                {"number":2,"state":"CLOSED","isDraft":false,"url":"https://example.test/pull/2","headRefName":"feature","headRepository":{"nameWithOwner":"owner/repo"}},
                {"number":11,"state":"OPEN","isDraft":false,"url":"https://example.test/pull/11","headRefName":"feature","headRepository":{"nameWithOwner":"fork/repo"}},
                {"number":13,"state":"OPEN","isDraft":false,"url":"https://example.test/pull/13","headRefName":"other","headRepository":{"nameWithOwner":"owner/repo"}}
            ]}}}}}"#,
            &repository,
            "feature",
        );

        assert_eq!(
            pull_requests
                .iter()
                .map(|pull_request| (pull_request.number, pull_request.status))
                .collect::<Vec<_>>(),
            vec![
                (4, PullRequestStatus::Draft),
                (7, PullRequestStatus::Open),
                (9, PullRequestStatus::Merged),
            ]
        );
    }

    #[test]
    fn normalizes_common_remote_url_conventions() {
        let expected = "github.com/owner/repo";
        assert_eq!(
            normalized_remote("https://github.com/Owner/Repo.git"),
            expected
        );
        assert_eq!(normalized_remote("git@github.com:Owner/Repo.git"), expected);
        assert_eq!(
            normalized_remote("ssh://git@github.com/Owner/Repo.git"),
            expected
        );
    }

    #[test]
    fn rejects_executable_and_local_remote_transports() {
        assert!(github_repository("ext::github.com/owner/repo.git").is_none());
        assert!(github_repository("file://github.com/owner/repo.git").is_none());
        assert!(github_repository(" https://github.com/owner/repo.git").is_none());
    }

    #[test]
    fn resolves_common_push_refspec_forms() {
        for refspec in [
            "feature:review/feature",
            "+feature:refs/heads/review/feature",
            "refs/heads/feature:refs/heads/review/feature",
            "refs/heads/*:refs/heads/review/*",
        ] {
            assert_eq!(
                push_refspec_destination(refspec, "feature"),
                RefspecMatch::Destination("review/feature".to_string()),
                "{refspec}"
            );
        }
        assert_eq!(
            push_refspec_destination("HEAD:refs/heads/review/head", "feature"),
            RefspecMatch::Destination("review/head".to_string())
        );
        for refspec in ["HEAD", "@"] {
            assert_eq!(
                push_refspec_destination(refspec, "feature"),
                RefspecMatch::Destination("feature".to_string()),
                "{refspec}"
            );
        }
        assert_eq!(
            push_refspec_destination(":", "feature"),
            RefspecMatch::Destination("feature".to_string())
        );
        assert_eq!(
            push_refspec_destination("feature", "feature"),
            RefspecMatch::Destination("feature".to_string())
        );
        assert_eq!(
            push_refspec_destination(":refs/heads/feature", "feature"),
            RefspecMatch::Unrelated,
            "a delete refspec must not be treated as a source mapping"
        );
        assert_eq!(
            push_refspec_destination("other:refs/heads/review/other", "feature"),
            RefspecMatch::Unrelated
        );
        assert_eq!(
            push_refspec_destination("feature:refs/tags/not-a-branch", "feature"),
            RefspecMatch::Malformed
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn passes_graphql_string_variables_as_raw_fields() {
        let root = temporary_directory("pr-graphql-variable-types");
        let arguments_file = root.join("gh-arguments");
        let gh = executable(
            &root,
            "fake-gh",
            &format!(
                "#!/bin/sh\nprintf '%s\\0' \"$@\" > '{}'\nprintf '%s\\n' \
                 '{{\"data\":{{\"repository\":{{\"ref\":{{\"associatedPullRequests\":\
                 {{\"nodes\":[]}}}}}}}}}}'\n",
                arguments_file.display()
            ),
        );
        let source = RemoteBranch {
            repository: GitHubRepository {
                hostname: "github.com".to_string(),
                owner: "123".to_string(),
                name: "456".to_string(),
                name_with_owner: "123/456".to_string(),
            },
            branch: "789".to_string(),
        };

        assert!(list_pull_requests(&root, &source, gh.as_os_str())
            .await
            .is_empty());

        let arguments_bytes = std::fs::read(&arguments_file).expect("read fake gh arguments");
        let arguments = arguments_bytes
            .strip_suffix(b"\0")
            .expect("arguments have a trailing delimiter")
            .split(|byte| *byte == b'\0')
            .map(|argument| std::str::from_utf8(argument).expect("UTF-8 argument"))
            .collect::<Vec<_>>();
        let query = format!("query={ASSOCIATED_PULL_REQUESTS_QUERY}");
        assert_eq!(
            arguments,
            vec![
                "api",
                "graphql",
                "--hostname",
                "github.com",
                "-f",
                query.as_str(),
                "-f",
                "owner=123",
                "-f",
                "name=456",
                "-f",
                "qualifiedName=refs/heads/789",
                "-F",
                "first=100",
            ]
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn discovers_and_deduplicates_prs_from_session_workspace_branches() {
        let root = temporary_directory("pr-discovery");
        for workspace in ["repo-a", "repo-b"] {
            std::fs::create_dir_all(root.join(workspace).join(".git"))
                .expect("create fake session workspace");
        }
        let fake_git = executable(
            &root,
            "fake-git",
            r#"#!/bin/sh
case "$1" in
  symbolic-ref) printf '%s\n' 'feature/sidebar-prs' ;;
  config)
    case " $* " in
      *" --list "*) printf 'remote.origin.url\ngit@github.com:owner/repo.git\0' ;;
      *) exit 1 ;;
    esac
    ;;
  for-each-ref) printf '\n' ;;
  *) exit 1 ;;
esac
"#,
        );
        let gh = executable(
            &root,
            "fake-gh",
            r#"#!/bin/sh
printf '%s\n' '{"data":{"repository":{"ref":{"associatedPullRequests":{"nodes":[
  {"number":7,"state":"OPEN","isDraft":false,"url":"https://github.com/owner/repo/pull/7","headRefName":"feature/sidebar-prs","headRepository":{"nameWithOwner":"owner/repo"}},
  {"number":7,"state":"MERGED","isDraft":false,"url":"https://github.com/another/base/pull/7","headRefName":"feature/sidebar-prs","headRepository":{"nameWithOwner":"owner/repo"}},
  {"number":4,"state":"OPEN","isDraft":true,"url":"https://github.com/owner/repo/pull/4","headRefName":"feature/sidebar-prs","headRepository":{"nameWithOwner":"owner/repo"}},
  {"number":8,"state":"OPEN","isDraft":false,"url":"https://github.com/fork/repo/pull/8","headRefName":"feature/sidebar-prs","headRepository":{"nameWithOwner":"fork/repo"}}
]}}}}}'
"#,
        );
        let context = SessionWorkspaceContext {
            session_id: "session".to_string(),
            outer_cwd: root.clone(),
            workspaces: vec![
                SessionWorkspace::git(
                    "repo-a",
                    "git@github.com:owner/repo.git",
                    "main",
                    "abc123",
                    "session-a",
                ),
                SessionWorkspace::git(
                    "repo-b",
                    "git@github.com:owner/repo.git",
                    "main",
                    "abc123",
                    "session-b",
                ),
            ],
        };

        let pull_requests =
            discover_pull_requests_with(&context, fake_git.as_os_str(), gh.as_os_str()).await;

        assert_eq!(
            pull_requests
                .iter()
                .map(|pull_request| {
                    (
                        pull_request.number,
                        pull_request.status,
                        pull_request.url.as_str(),
                        pull_request.workspace_dirs.as_slice(),
                        pull_request.source_repository.as_str(),
                    )
                })
                .collect::<Vec<_>>(),
            vec![
                (
                    4,
                    PullRequestStatus::Draft,
                    "https://github.com/owner/repo/pull/4",
                    &["repo-a".to_string(), "repo-b".to_string()][..],
                    "owner/repo",
                ),
                (
                    7,
                    PullRequestStatus::Merged,
                    "https://github.com/another/base/pull/7",
                    &["repo-a".to_string(), "repo-b".to_string()][..],
                    "owner/repo",
                ),
                (
                    7,
                    PullRequestStatus::Open,
                    "https://github.com/owner/repo/pull/7",
                    &["repo-a".to_string(), "repo-b".to_string()][..],
                    "owner/repo",
                ),
            ]
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uses_the_checked_out_branch_for_a_colonless_head_push_refspec() {
        assert_colonless_head_push_refspec("HEAD", 71).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uses_the_checked_out_branch_for_a_colonless_at_push_refspec() {
        assert_colonless_head_push_refspec("@", 72).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uses_a_real_git_matching_push_refspec() {
        let root = temporary_directory("pr-matching-push-refspec");
        let workspace_root = initialized_git_repository(&root, "repo", "feature");
        git_success(
            &workspace_root,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/owner/repo.git",
            ],
        );
        git_success(
            &workspace_root,
            &["config", "--local", "branch.feature.pushRemote", "origin"],
        );
        git_success(
            &workspace_root,
            &["config", "--local", "remote.origin.push", ":"],
        );

        let repository = isolated_git_repository(&workspace_root)
            .await
            .expect("isolated Git repository");
        let sources = checked_out_remote_branches(&repository, OsStr::new("git")).await;

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].repository.name_with_owner, "owner/repo");
        assert_eq!(sources[0].branch, "feature");
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn discovers_a_git_repo_inside_a_local_folder_workspace() {
        let root = temporary_directory("pr-local-folder-git");
        std::fs::create_dir_all(root.join("repo").join(".git"))
            .expect("create fake local-folder git workspace");
        let git = executable(
            &root,
            "fake-git",
            r#"#!/bin/sh
case "$1" in
  symbolic-ref) printf '%s\n' 'feature/local-folder' ;;
  config)
    case " $* " in
      *" --list "*) printf 'remote.origin.url\nhttps://github.com/owner/repo.git\0' ;;
      *) exit 1 ;;
    esac
    ;;
  for-each-ref) printf '\n' ;;
  *) exit 1 ;;
esac
"#,
        );
        let gh = executable(
            &root,
            "fake-gh",
            r#"#!/bin/sh
printf '%s\n' '{"data":{"repository":{"ref":{"associatedPullRequests":{"nodes":[
  {"number":12,"state":"MERGED","isDraft":false,"url":"https://github.com/owner/repo/pull/12","headRefName":"feature/local-folder","headRepository":{"nameWithOwner":"owner/repo"}}
]}}}}}'
"#,
        );
        let context = SessionWorkspaceContext {
            session_id: "session".to_string(),
            outer_cwd: root.clone(),
            workspaces: vec![SessionWorkspace::local("repo", "/source/repo")],
        };

        let pull_requests =
            discover_pull_requests_with(&context, git.as_os_str(), gh.as_os_str()).await;

        assert_eq!(
            pull_requests
                .iter()
                .map(|pull_request| (pull_request.number, pull_request.status))
                .collect::<Vec<_>>(),
            vec![(12, PullRequestStatus::Merged)]
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn skips_a_merged_pull_request_when_its_source_branch_was_deleted() {
        let root = temporary_directory("pr-deleted-remote-branch");
        std::fs::create_dir_all(root.join("repo").join(".git"))
            .expect("create fake session workspace");
        let gh_marker = root.join("gh-was-called");
        let git = executable(
            &root,
            "fake-git",
            r#"#!/bin/sh
case "$1" in
  symbolic-ref) printf '%s\n' 'pi/session/local-only' ;;
  config)
    case " $* " in
      *" --list "*) printf 'remote.origin.url\nhttps://github.com/owner/repo.git\0' ;;
      *) exit 1 ;;
    esac
    ;;
  for-each-ref) printf '\n' ;;
  *) exit 1 ;;
esac
"#,
        );
        let gh = executable(
            &root,
            "fake-gh",
            &format!(
                "#!/bin/sh\nprintf called > '{}'\nprintf '%s\\n' \
                 '{{\"data\":{{\"repository\":{{\"ref\":null}}}}}}'\n",
                gh_marker.display()
            ),
        );
        let context = SessionWorkspaceContext {
            session_id: "session".to_string(),
            outer_cwd: root.clone(),
            workspaces: vec![SessionWorkspace::git(
                "repo",
                "https://github.com/owner/repo.git",
                "main",
                "abc123",
                "pi/session/local-only",
            )],
        };

        assert!(
            discover_pull_requests_with(&context, git.as_os_str(), gh.as_os_str())
                .await
                .is_empty()
        );
        assert!(
            gh_marker.exists(),
            "the exact GraphQL ref lookup must determine that the source branch is deleted"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn does_not_treat_a_pull_upstream_as_the_checked_out_remote_branch() {
        let root = temporary_directory("pr-pull-upstream");
        std::fs::create_dir_all(root.join("repo").join(".git"))
            .expect("create fake session workspace");
        let wrong_ref_marker = root.join("wrong-ref-was-queried");
        let git = executable(
            &root,
            "fake-git",
            r#"#!/bin/sh
case "$1" in
  symbolic-ref) printf '%s\n' 'feature' ;;
  config)
    case " $* " in
      *" --list "*) printf 'remote.origin.url\nhttps://github.com/owner/repo.git\0' ;;
      *) exit 1 ;;
    esac
    ;;
  for-each-ref)
    case "$3" in
      '--format=%(push:remotename)') printf '%s\n' 'origin' ;;
      '--format=%(push:remoteref)') printf '\n' ;;
      *) exit 1 ;;
    esac
    ;;
  *) exit 1 ;;
esac
"#,
        );
        let gh = executable(
            &root,
            "fake-gh",
            &format!(
                "#!/bin/sh\ncase \" $* \" in\n\
                 *\" qualifiedName=refs/heads/feature \"*) \
                 printf '%s\\n' '{{\"data\":{{\"repository\":{{\"ref\":null}}}}}}' ;;\n\
                 *) printf called > '{}'; exit 1 ;;\n\
                 esac\n",
                wrong_ref_marker.display()
            ),
        );
        let context = SessionWorkspaceContext {
            session_id: "session".to_string(),
            outer_cwd: root.clone(),
            workspaces: vec![SessionWorkspace::git(
                "repo",
                "https://github.com/owner/repo.git",
                "main",
                "abc123",
                "feature",
            )],
        };
        assert!(
            discover_pull_requests_with(&context, git.as_os_str(), gh.as_os_str())
                .await
                .is_empty()
        );
        assert!(
            !wrong_ref_marker.exists(),
            "a PR for upstream main must not be attached to local feature"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn discovers_a_fork_branch_pr_into_an_upstream_repository() {
        let root = temporary_directory("pr-fork");
        let workspace_root = initialized_git_repository(&root, "repo", "feature/fork-pr");
        git_success(
            &workspace_root,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/upstream/repo.git",
            ],
        );
        git_success(
            &workspace_root,
            &[
                "remote",
                "add",
                "fork",
                "git@github.com:contributor/repo.git",
            ],
        );
        git_success(
            &workspace_root,
            &[
                "config",
                "--local",
                "branch.feature/fork-pr.remote",
                "origin",
            ],
        );
        git_success(
            &workspace_root,
            &[
                "config",
                "--local",
                "branch.feature/fork-pr.merge",
                "refs/heads/main",
            ],
        );
        git_success(
            &workspace_root,
            &[
                "config",
                "--local",
                "branch.feature/fork-pr.pushRemote",
                "fork",
            ],
        );
        git_success(
            &workspace_root,
            &["config", "--local", "push.default", "current"],
        );
        assert_eq!(
            git_stdout_sync(
                &workspace_root,
                &[
                    "for-each-ref",
                    "--count=1",
                    "--format=%(push:remotename)",
                    "refs/heads/feature/fork-pr",
                ],
            ),
            "fork"
        );
        assert_eq!(
            git_stdout_sync(
                &workspace_root,
                &[
                    "for-each-ref",
                    "--count=1",
                    "--format=%(push:remoteref)",
                    "refs/heads/feature/fork-pr",
                ],
            ),
            "",
            "real Git leaves push:remoteref empty for push.default=current"
        );
        let gh = executable(
            &root,
            "fake-gh",
            r#"#!/bin/sh
case " $* " in
  *" --hostname github.com "*" owner=contributor "*" name=repo "*" qualifiedName=refs/heads/feature/fork-pr "*)
    printf '%s\n' '{"data":{"repository":{"ref":{"associatedPullRequests":{"nodes":[
      {"number":41,"state":"OPEN","isDraft":false,"url":"https://github.com/upstream/repo/pull/41","headRefName":"feature/fork-pr","headRepository":{"nameWithOwner":"someone-else/repo"}},
      {"number":42,"state":"OPEN","isDraft":false,"url":"https://github.com/upstream/repo/pull/42","headRefName":"feature/fork-pr","headRepository":{"nameWithOwner":"contributor/repo"}},
      {"number":43,"state":"OPEN","isDraft":false,"url":"https://github.com/upstream/repo/pull/43","headRefName":"other","headRepository":{"nameWithOwner":"contributor/repo"}}
    ]}}}}}'
    ;;
  *) exit 1 ;;
esac
"#,
        );
        let context = SessionWorkspaceContext {
            session_id: "session".to_string(),
            outer_cwd: root.clone(),
            // Persisted project metadata points at upstream; the checked-out branch's
            // explicit push remote identifies the fork as its actual source.
            workspaces: vec![SessionWorkspace::git(
                "repo",
                "https://github.com/upstream/repo.git",
                "main",
                "abc123",
                "feature/fork-pr",
            )],
        };

        let pull_requests =
            discover_pull_requests_with(&context, OsStr::new("git"), gh.as_os_str()).await;

        assert_eq!(
            pull_requests
                .iter()
                .map(|pull_request| (pull_request.number, pull_request.status))
                .collect::<Vec<_>>(),
            vec![(42, PullRequestStatus::Open)]
        );
        assert_eq!(
            pull_requests[0].url,
            "https://github.com/upstream/repo/pull/42"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uses_an_explicit_push_destination_instead_of_pull_tracking_metadata() {
        let root = temporary_directory("pr-push-destination");
        let workspace_root = initialized_git_repository(&root, "repo", "feature");
        git_success(
            &workspace_root,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/owner/repo.git",
            ],
        );
        git_success(
            &workspace_root,
            &["config", "--local", "branch.feature.remote", "origin"],
        );
        git_success(
            &workspace_root,
            &[
                "config",
                "--local",
                "branch.feature.merge",
                "refs/heads/main",
            ],
        );
        git_success(
            &workspace_root,
            &[
                "config",
                "--local",
                "remote.origin.push",
                "refs/heads/feature:refs/heads/review/feature",
            ],
        );
        assert_eq!(
            git_stdout_sync(
                &workspace_root,
                &[
                    "for-each-ref",
                    "--count=1",
                    "--format=%(push:remotename)",
                    "refs/heads/feature",
                ],
            ),
            "origin"
        );
        assert_eq!(
            git_stdout_sync(
                &workspace_root,
                &[
                    "for-each-ref",
                    "--count=1",
                    "--format=%(push:remoteref)",
                    "refs/heads/feature",
                ],
            ),
            "refs/heads/review/feature"
        );
        let gh = executable(
            &root,
            "fake-gh",
            r#"#!/bin/sh
case " $* " in
  *" qualifiedName=refs/heads/review/feature "*)
    printf '%s\n' '{"data":{"repository":{"ref":{"associatedPullRequests":{"nodes":[
      {"number":51,"state":"OPEN","isDraft":true,"url":"https://github.com/owner/repo/pull/51","headRefName":"review/feature","headRepository":{"nameWithOwner":"owner/repo"}}
    ]}}}}}'
    ;;
  *) exit 1 ;;
esac
"#,
        );
        let context = SessionWorkspaceContext {
            session_id: "session".to_string(),
            outer_cwd: root.clone(),
            workspaces: vec![SessionWorkspace::git(
                "repo",
                "https://github.com/owner/repo.git",
                "main",
                "abc123",
                "feature",
            )],
        };

        let pull_requests =
            discover_pull_requests_with(&context, OsStr::new("git"), gh.as_os_str()).await;

        assert_eq!(
            pull_requests
                .iter()
                .map(|pull_request| (pull_request.number, pull_request.status))
                .collect::<Vec<_>>(),
            vec![(51, PullRequestStatus::Draft)]
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn does_not_fall_back_when_an_explicit_push_destination_is_not_github() {
        let root = temporary_directory("pr-non-github-push-destination");
        let workspace_root = initialized_git_repository(&root, "repo", "feature");
        git_success(
            &workspace_root,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/owner/repo.git",
            ],
        );
        git_success(
            &workspace_root,
            &["remote", "add", "review", "/srv/git/review.git"],
        );
        git_success(
            &workspace_root,
            &["config", "--local", "branch.feature.pushRemote", "review"],
        );
        git_success(
            &workspace_root,
            &["config", "--local", "push.default", "current"],
        );
        assert_eq!(
            git_stdout_sync(
                &workspace_root,
                &[
                    "for-each-ref",
                    "--count=1",
                    "--format=%(push:remotename)",
                    "refs/heads/feature",
                ],
            ),
            "review"
        );
        assert_eq!(
            git_stdout_sync(
                &workspace_root,
                &[
                    "for-each-ref",
                    "--count=1",
                    "--format=%(push:remoteref)",
                    "refs/heads/feature",
                ],
            ),
            "",
            "real Git leaves push:remoteref empty for push.default=current"
        );
        let gh_marker = root.join("gh-was-called");
        let gh = marker_executable(&root, "fake-gh", &gh_marker);
        let context = SessionWorkspaceContext {
            session_id: "session".to_string(),
            outer_cwd: root.clone(),
            workspaces: vec![SessionWorkspace::git(
                "repo",
                "https://github.com/owner/repo.git",
                "main",
                "abc123",
                "feature",
            )],
        };

        assert!(
            discover_pull_requests_with(&context, OsStr::new("git"), gh.as_os_str())
                .await
                .is_empty()
        );
        assert!(
            !gh_marker.exists(),
            "an explicit non-GitHub push destination must not fall back to origin"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uses_local_remote_push_default_when_branch_push_remote_is_absent() {
        let root = temporary_directory("pr-remote-push-default");
        let workspace_root = initialized_git_repository(&root, "repo", "feature");
        git_success(
            &workspace_root,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/owner/repo.git",
            ],
        );
        git_success(
            &workspace_root,
            &[
                "remote",
                "add",
                "fork",
                "https://github.com/contributor/repo.git",
            ],
        );
        git_success(
            &workspace_root,
            &["config", "--local", "remote.pushDefault", "fork"],
        );

        let repository = isolated_git_repository(&workspace_root)
            .await
            .expect("isolated Git repository");
        let sources = checked_out_remote_branches(&repository, OsStr::new("git")).await;

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].repository.name_with_owner, "contributor/repo");
        assert_eq!(sources[0].branch, "feature");
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ignores_included_push_destination_config() {
        let root = temporary_directory("pr-no-included-push-config");
        let workspace_root = initialized_git_repository(&root, "repo", "feature");
        let included_config = root.join("included-config");
        std::fs::write(&included_config, "[remote]\n\tpushDefault = fork\n")
            .expect("write included Git config");
        git_success(
            &workspace_root,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/owner/repo.git",
            ],
        );
        git_success(
            &workspace_root,
            &[
                "remote",
                "add",
                "fork",
                "https://github.com/contributor/repo.git",
            ],
        );
        git_success(
            &workspace_root,
            &[
                "config",
                "--local",
                "include.path",
                included_config.to_str().expect("UTF-8 config path"),
            ],
        );

        let repository = isolated_git_repository(&workspace_root)
            .await
            .expect("isolated Git repository");
        let sources = checked_out_remote_branches(&repository, OsStr::new("git")).await;

        assert_eq!(
            sources
                .iter()
                .map(|source| source.repository.name_with_owner.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["contributor/repo", "owner/repo"])
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn enumerates_only_raw_local_remote_urls_without_rewrites() {
        let root = temporary_directory("pr-local-remotes-only");
        let workspace_root = initialized_git_repository(&root, "repo", "feature");
        let included_config = root.join("included-config");
        std::fs::write(
            &included_config,
            concat!(
                "[remote \"included\"]\n",
                "\turl = https://github.com/included/repo.git\n",
                "[url \"https://github.com/rewritten/\"]\n",
                "\tinsteadOf = alias:\n"
            ),
        )
        .expect("write included Git config");
        git_success(
            &workspace_root,
            &[
                "config",
                "--local",
                "include.path",
                included_config.to_str().expect("UTF-8 config path"),
            ],
        );
        git_success(
            &workspace_root,
            &["remote", "add", "origin", "alias:owner/repo.git"],
        );
        git_success(
            &workspace_root,
            &[
                "config",
                "--local",
                "remote.origin.pushurl",
                "https://github.com/local/push-repo.git",
            ],
        );
        git_success(
            &workspace_root,
            &[
                "remote",
                "add",
                "review.remote",
                "https://github.com/local/review-fallback.git",
            ],
        );
        git_success(
            &workspace_root,
            &[
                "config",
                "--local",
                "--add",
                "remote.review.remote.pushurl",
                "https://github.com/local/review-a.git",
            ],
        );
        git_success(
            &workspace_root,
            &[
                "config",
                "--local",
                "--add",
                "remote.review.remote.pushurl",
                "git@github.com:local/review-b.git",
            ],
        );

        let repository = isolated_git_repository(&workspace_root)
            .await
            .expect("isolated Git repository");
        let (remote_names, remotes) = repository_remotes(OsStr::new("git"), &repository).await;

        assert_eq!(remote_names, vec!["origin", "review.remote"]);
        assert_eq!(remotes.len(), 3);
        assert_eq!(remotes[0].name, "origin");
        assert_eq!(remotes[0].url, "https://github.com/local/push-repo.git");
        assert_eq!(remotes[0].repository.name_with_owner, "local/push-repo");
        assert_eq!(
            remotes[1..]
                .iter()
                .map(|remote| (
                    remote.name.as_str(),
                    remote.repository.name_with_owner.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("review.remote", "local/review-a"),
                ("review.remote", "local/review-b"),
            ]
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn explicit_empty_push_remote_settings_fail_closed() {
        for key in ["branch.feature.pushRemote", "remote.pushDefault"] {
            let root = temporary_directory("pr-empty-push-remote");
            let workspace_root = initialized_git_repository(&root, "repo", "feature");
            git_success(
                &workspace_root,
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://github.com/owner/repo.git",
                ],
            );
            git_success(
                &workspace_root,
                &[
                    "remote",
                    "add",
                    "fork",
                    "https://github.com/contributor/repo.git",
                ],
            );
            git_success(&workspace_root, &["config", "--local", key, ""]);
            if key.starts_with("branch.") {
                git_success(
                    &workspace_root,
                    &["config", "--local", "remote.pushDefault", "fork"],
                );
            }

            assert!(
                checked_out_remote_branches(
                    &isolated_git_repository(&workspace_root)
                        .await
                        .expect("isolated Git repository"),
                    OsStr::new("git"),
                )
                .await
                .is_empty(),
                "{key} must remain present-but-empty"
            );
            std::fs::remove_dir_all(root).ok();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uses_a_real_git_shorthand_push_rename() {
        let root = temporary_directory("pr-shorthand-push-rename");
        let workspace_root = initialized_git_repository(&root, "repo", "feature");
        git_success(
            &workspace_root,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/owner/repo.git",
            ],
        );
        git_success(
            &workspace_root,
            &["config", "--local", "branch.feature.pushRemote", "origin"],
        );
        git_success(
            &workspace_root,
            &[
                "config",
                "--local",
                "remote.origin.push",
                "feature:review/feature",
            ],
        );
        let gh = executable(
            &root,
            "fake-gh",
            r#"#!/bin/sh
case " $* " in
  *" qualifiedName=refs/heads/review/feature "*)
    printf '%s\n' '{"data":{"repository":{"ref":{"associatedPullRequests":{"nodes":[
      {"number":61,"state":"OPEN","isDraft":false,"url":"https://github.com/owner/repo/pull/61","headRefName":"review/feature","headRepository":{"nameWithOwner":"owner/repo"}}
    ]}}}}}'
    ;;
  *) exit 1 ;;
esac
"#,
        );
        let context = SessionWorkspaceContext {
            session_id: "session".to_string(),
            outer_cwd: root.clone(),
            workspaces: vec![SessionWorkspace::local("repo", "/source/repo")],
        };

        let pull_requests =
            discover_pull_requests_with(&context, OsStr::new("git"), gh.as_os_str()).await;

        assert_eq!(
            pull_requests
                .iter()
                .map(|pull_request| (pull_request.number, pull_request.workspace_dirs.as_slice()))
                .collect::<Vec<_>>(),
            vec![(61, &["repo".to_string()][..])]
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unrelated_only_real_git_push_mapping_fails_closed() {
        let root = temporary_directory("pr-unrelated-push-refspec");
        let workspace_root = initialized_git_repository(&root, "repo", "feature");
        git_success(
            &workspace_root,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/owner/repo.git",
            ],
        );
        git_success(
            &workspace_root,
            &["config", "--local", "branch.feature.pushRemote", "origin"],
        );
        git_success(
            &workspace_root,
            &[
                "config",
                "--local",
                "remote.origin.push",
                "other:review/other",
            ],
        );
        let gh_marker = root.join("gh-was-called");
        let gh = marker_executable(&root, "fake-gh", &gh_marker);
        let context = SessionWorkspaceContext {
            session_id: "session".to_string(),
            outer_cwd: root.clone(),
            workspaces: vec![SessionWorkspace::local("repo", "/source/repo")],
        };

        assert!(
            discover_pull_requests_with(&context, OsStr::new("git"), gh.as_os_str())
                .await
                .is_empty()
        );
        assert!(
            !gh_marker.exists(),
            "an unrelated explicit refspec must not fall back to the checked-out branch"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn never_executes_repository_configured_git_transport_helpers() {
        let root = temporary_directory("pr-no-git-transport");
        let workspace_root = initialized_git_repository(&root, "repo", "feature");
        let ssh_marker = root.join("ssh-helper-was-called");
        let proxy_marker = root.join("proxy-helper-was-called");
        let ssh_helper = marker_executable(&root, "ssh-helper", &ssh_marker);
        let proxy_helper = marker_executable(&root, "proxy-helper", &proxy_marker);
        git_success(
            &workspace_root,
            &[
                "remote",
                "add",
                "ssh-origin",
                "git@github.com:owner/ssh-repo.git",
            ],
        );
        git_success(
            &workspace_root,
            &[
                "remote",
                "add",
                "git-origin",
                "git://github.com/owner/git-repo.git",
            ],
        );
        git_success(
            &workspace_root,
            &[
                "config",
                "--local",
                "core.sshCommand",
                ssh_helper.to_str().expect("UTF-8 helper path"),
            ],
        );
        git_success(
            &workspace_root,
            &[
                "config",
                "--local",
                "core.gitProxy",
                proxy_helper.to_str().expect("UTF-8 helper path"),
            ],
        );
        let gh = executable(
            &root,
            "fake-gh",
            r#"#!/bin/sh
printf '%s\n' '{"data":{"repository":{"ref":null}}}'
"#,
        );
        let context = SessionWorkspaceContext {
            session_id: "session".to_string(),
            outer_cwd: root.clone(),
            workspaces: vec![SessionWorkspace::local("repo", "/source/repo")],
        };

        assert!(
            discover_pull_requests_with(&context, OsStr::new("git"), gh.as_os_str())
                .await
                .is_empty()
        );
        assert!(
            !ssh_marker.exists() && !proxy_marker.exists(),
            "PR discovery must not invoke Git network transport"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn skips_a_gitfile_before_invoking_git_or_github() {
        let root = temporary_directory("pr-external-gitfile");
        let workspace_root = root.join("repo");
        let external_git_dir = temporary_directory("pr-external-git-dir");
        std::fs::create_dir_all(&workspace_root).expect("create fake session workspace");
        std::fs::write(
            workspace_root.join(".git"),
            format!("gitdir: {}\n", external_git_dir.display()),
        )
        .expect("write external gitfile");
        let git_marker = root.join("git-was-called");
        let gh_marker = root.join("gh-was-called");
        let git = marker_executable(&root, "fake-git", &git_marker);
        let gh = marker_executable(&root, "fake-gh", &gh_marker);
        let context = SessionWorkspaceContext {
            session_id: "session".to_string(),
            outer_cwd: root.clone(),
            workspaces: vec![SessionWorkspace::local("repo", "/source/repo")],
        };

        assert!(
            discover_pull_requests_with(&context, git.as_os_str(), gh.as_os_str())
                .await
                .is_empty()
        );
        assert!(
            !git_marker.exists() && !gh_marker.exists(),
            "gitfiles must be rejected before repository commands run"
        );
        std::fs::remove_dir_all(root).ok();
        std::fs::remove_dir_all(external_git_dir).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn skips_an_external_common_git_directory_before_invoking_git_or_github() {
        let root = temporary_directory("pr-external-common-dir");
        let git_dir = root.join("repo").join(".git");
        let external_common_dir = temporary_directory("pr-external-common-git-dir");
        std::fs::create_dir_all(&git_dir).expect("create fake session git directory");
        std::fs::write(
            git_dir.join("commondir"),
            format!("{}\n", external_common_dir.display()),
        )
        .expect("write external common directory");
        let git_marker = root.join("git-was-called");
        let gh_marker = root.join("gh-was-called");
        let git = marker_executable(&root, "fake-git", &git_marker);
        let gh = marker_executable(&root, "fake-gh", &gh_marker);
        let context = SessionWorkspaceContext {
            session_id: "session".to_string(),
            outer_cwd: root.clone(),
            workspaces: vec![SessionWorkspace::local("repo", "/source/repo")],
        };

        assert!(
            discover_pull_requests_with(&context, git.as_os_str(), gh.as_os_str())
                .await
                .is_empty()
        );
        assert!(
            !git_marker.exists() && !gh_marker.exists(),
            "an external common directory must be rejected before repository commands run"
        );
        std::fs::remove_dir_all(root).ok();
        std::fs::remove_dir_all(external_common_dir).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn empty_child_git_directory_never_falls_back_to_ambient_parent_repository() {
        let root = temporary_directory("pr-empty-child-git-dir");
        git_success(&root, &["init", "--quiet"]);
        git_success(&root, &["branch", "-M", "ambient-parent"]);
        git_success(
            &root,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/ambient/parent.git",
            ],
        );
        std::fs::create_dir_all(root.join("repo").join(".git"))
            .expect("create empty child Git directory");
        let gh_marker = root.join("gh-was-called");
        let gh = marker_executable(&root, "fake-gh", &gh_marker);
        let context = SessionWorkspaceContext {
            session_id: "session".to_string(),
            outer_cwd: root.clone(),
            workspaces: vec![SessionWorkspace::local("repo", "/source/repo")],
        };

        assert!(
            discover_pull_requests_with(&context, OsStr::new("git"), gh.as_os_str())
                .await
                .is_empty()
        );
        assert!(
            !gh_marker.exists(),
            "an invalid exact child repository must not read its parent's branch or remote"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    fn executable(root: &Path, name: &str, contents: &str) -> PathBuf {
        let path = root.join(name);
        std::fs::write(&path, contents).expect("write fake executable");
        let mut permissions = std::fs::metadata(&path)
            .expect("fake executable metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("make fake executable executable");
        path
    }

    #[cfg(unix)]
    fn marker_executable(root: &Path, name: &str, marker: &Path) -> PathBuf {
        executable(
            root,
            name,
            &format!(
                "#!/bin/sh\nprintf called > '{}'\nprintf '%s\\n' '{{\"data\":null}}'\n",
                marker.display()
            ),
        )
    }

    #[cfg(unix)]
    fn initialized_git_repository(root: &Path, name: &str, branch: &str) -> PathBuf {
        let workspace_root = root.join(name);
        std::fs::create_dir_all(&workspace_root).expect("create Git workspace");
        git_success(&workspace_root, &["init", "--quiet"]);
        git_success(
            &workspace_root,
            &["config", "--local", "user.name", "pi-relay test"],
        );
        git_success(
            &workspace_root,
            &[
                "config",
                "--local",
                "user.email",
                "pi-relay@example.invalid",
            ],
        );
        git_success(
            &workspace_root,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        git_success(&workspace_root, &["branch", "-M", branch]);
        workspace_root
    }

    #[cfg(unix)]
    async fn assert_colonless_head_push_refspec(refspec: &str, pull_request_number: u64) {
        let root = temporary_directory("pr-colonless-head-push-refspec");
        let workspace_root = initialized_git_repository(&root, "repo", "feature/colonless-push");
        let bare_remote = root.join("remote.git");
        let bare_remote = bare_remote.to_str().expect("UTF-8 temporary path");
        git_success(&workspace_root, &["init", "--quiet", "--bare", bare_remote]);
        git_success(&workspace_root, &["remote", "add", "origin", bare_remote]);
        git_success(
            &workspace_root,
            &[
                "config",
                "--local",
                "branch.feature/colonless-push.pushRemote",
                "origin",
            ],
        );
        git_success(
            &workspace_root,
            &["config", "--local", "remote.origin.push", refspec],
        );
        assert!(
            git_stdout_sync(
                &workspace_root,
                &["push", "--dry-run", "--porcelain", "origin"]
            )
            .contains("HEAD:refs/heads/feature/colonless-push"),
            "real Git must map {refspec} to the checked-out branch"
        );
        git_success(
            &workspace_root,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/owner/repo.git",
            ],
        );

        let gh = executable(
            &root,
            "fake-gh",
            &format!(
                r#"#!/bin/sh
case " $* " in
  *" qualifiedName=refs/heads/feature/colonless-push "*)
    printf '%s\n' '{{"data":{{"repository":{{"ref":{{"associatedPullRequests":{{"nodes":[
      {{"number":{pull_request_number},"state":"OPEN","isDraft":false,"url":"https://github.com/owner/repo/pull/{pull_request_number}","headRefName":"feature/colonless-push","headRepository":{{"nameWithOwner":"owner/repo"}}}}
    ]}}}}}}}}}}'
    ;;
  *) exit 1 ;;
esac
"#
            ),
        );
        let context = SessionWorkspaceContext {
            session_id: "session".to_string(),
            outer_cwd: root.clone(),
            workspaces: vec![SessionWorkspace::local("repo", "/source/repo")],
        };

        let pull_requests =
            discover_pull_requests_with(&context, OsStr::new("git"), gh.as_os_str()).await;

        assert_eq!(
            pull_requests
                .iter()
                .map(|pull_request| (pull_request.number, pull_request.source_repository.as_str()))
                .collect::<Vec<_>>(),
            vec![(pull_request_number, "owner/repo")]
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    fn git_success(workspace_root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(workspace_root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run Git test setup");
        assert!(
            output.status.success(),
            "Git test setup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    fn git_stdout_sync(workspace_root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(workspace_root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .expect("run Git test query");
        assert!(
            output.status.success(),
            "Git test query failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[cfg(unix)]
    fn temporary_directory(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("pi-relay-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create temporary test directory");
        path
    }
}
