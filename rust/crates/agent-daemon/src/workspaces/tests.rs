use super::*;
use agent_store::{ProjectWorkspace, SessionRelationshipFilesystemMode};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[tokio::test]
async fn materialize_session_workspaces_from_local_remote() {
    let temp = TempDir::new("workspace-manager");
    let remote = temp.path().join("remote.git");
    let seed = temp.path().join("seed");
    std::fs::create_dir_all(&seed).expect("seed dir");

    git(
        temp.path(),
        ["init", "--bare", remote.to_str().expect("remote path")],
    );
    git(&seed, ["init"]);
    git(&seed, ["config", "user.email", "pi-relay@example.test"]);
    git(&seed, ["config", "user.name", "pi relay"]);
    git(&seed, ["config", "commit.gpgsign", "false"]);
    std::fs::write(seed.join("README.md"), "hello\n").expect("seed file");
    git(&seed, ["add", "README.md"]);
    git(&seed, ["commit", "-m", "initial"]);
    git(&seed, ["branch", "-M", "main"]);
    git(
        &seed,
        [
            "remote",
            "add",
            "origin",
            remote.to_str().expect("remote path"),
        ],
    );
    git(&seed, ["push", "origin", "main"]);

    let manager = WorkspaceManager::new(temp.path().join("state"));
    let project_id = Uuid::new_v4();
    let project_workspaces = vec![ProjectWorkspace::git(
        "repo",
        remote.to_string_lossy(),
        "main",
    )];

    let (cwd, workspaces) = manager
        .materialize_session(project_id, "session-1", &project_workspaces)
        .await
        .expect("materialize session");
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0].workspace_dir, "repo");
    assert_eq!(workspaces[0].kind, WorkspaceKind::Git);
    assert_eq!(workspaces[0].remote_branch.as_deref(), Some("main"));
    assert_eq!(
        workspaces[0].local_branch.as_deref(),
        Some("pi/session/session-1/repo")
    );
    assert_eq!(
        git_stdout(
            Path::new(&cwd).join("repo").as_path(),
            ["branch", "--show-current"]
        ),
        "pi/session/session-1/repo"
    );

    assert_eq!(
        std::fs::read_to_string(Path::new(&cwd).join("repo/README.md")).expect("workspace file"),
        "hello\n"
    );

    std::fs::write(seed.join("README.md"), "updated\n").expect("update seed file");
    git(&seed, ["add", "README.md"]);
    git(&seed, ["commit", "-m", "update"]);
    git(&seed, ["push", "origin", "main"]);

    let (cwd, _) = manager
        .materialize_session(project_id, "session-2", &project_workspaces)
        .await
        .expect("materialize second session");
    assert_eq!(
        std::fs::read_to_string(Path::new(&cwd).join("repo/README.md"))
            .expect("updated workspace file"),
        "updated\n"
    );
}

#[tokio::test]
async fn fork_session_from_parent_copies_current_state_without_refreshing_base() {
    let temp = TempDir::new("workspace-manager-fork");
    let remote = temp.path().join("remote.git");
    let seed = temp.path().join("seed");
    std::fs::create_dir_all(&seed).expect("seed dir");

    git(
        temp.path(),
        ["init", "--bare", remote.to_str().expect("remote path")],
    );
    git(&seed, ["init"]);
    git(&seed, ["config", "user.email", "pi-relay@example.test"]);
    git(&seed, ["config", "user.name", "pi relay"]);
    git(&seed, ["config", "commit.gpgsign", "false"]);
    std::fs::write(seed.join("README.md"), "hello\n").expect("seed file");
    git(&seed, ["add", "README.md"]);
    git(&seed, ["commit", "-m", "initial"]);
    git(&seed, ["branch", "-M", "main"]);
    git(
        &seed,
        [
            "remote",
            "add",
            "origin",
            remote.to_str().expect("remote path"),
        ],
    );
    git(&seed, ["push", "origin", "main"]);

    let manager = WorkspaceManager::new(temp.path().join("state"));
    let project_id = Uuid::new_v4();
    let project_workspaces = vec![ProjectWorkspace::git(
        "repo",
        remote.to_string_lossy(),
        "main",
    )];
    let (parent_cwd, parent_workspaces) = manager
        .materialize_session(project_id, "parent-session", &project_workspaces)
        .await
        .expect("materialize parent session");
    let parent_repo = Path::new(&parent_cwd).join("repo");
    std::fs::write(parent_repo.join("README.md"), "dirty parent\n").expect("dirty parent file");
    std::fs::write(parent_repo.join("UNTRACKED.txt"), "untracked parent\n")
        .expect("untracked parent file");
    make_symlink(Path::new("/etc/passwd"), &parent_repo.join("external-link"));

    std::fs::write(seed.join("README.md"), "remote update\n").expect("remote update");
    git(&seed, ["add", "README.md"]);
    git(&seed, ["commit", "-m", "remote update"]);
    git(&seed, ["push", "origin", "main"]);

    let fork = manager
        .fork_session_from_parent(
            "parent-session",
            &parent_cwd,
            &parent_workspaces,
            "child-session",
        )
        .await
        .expect("fork child session");
    let child_repo = Path::new(&fork.outer_cwd).join("repo");
    let baseline_repo = Path::new(&fork.baseline_cwd).join("repo");

    assert_eq!(fork.workspaces.len(), 1);
    assert!(matches!(
        fork.filesystem_mode,
        SessionRelationshipFilesystemMode::BtrfsSnapshot
            | SessionRelationshipFilesystemMode::ReflinkCopy
            | SessionRelationshipFilesystemMode::PlainCopy
    ));
    assert_eq!(
        fork.workspaces[0].local_branch.as_deref(),
        Some("pi/session/child-session/repo")
    );
    assert_eq!(
        git_stdout(&child_repo, ["branch", "--show-current"]),
        "pi/session/child-session/repo"
    );
    assert_eq!(
        git_stdout(&parent_repo, ["branch", "--show-current"]),
        "pi/session/parent-session/repo"
    );
    assert_eq!(
        std::fs::read_to_string(child_repo.join("README.md")).expect("child dirty file"),
        "dirty parent\n"
    );
    assert_eq!(
        std::fs::read_to_string(child_repo.join("UNTRACKED.txt")).expect("child untracked file"),
        "untracked parent\n"
    );
    assert_eq!(
        std::fs::read_to_string(baseline_repo.join("README.md")).expect("baseline dirty file"),
        "dirty parent\n"
    );
    assert_eq!(
        std::fs::read_to_string(baseline_repo.join("UNTRACKED.txt"))
            .expect("baseline untracked file"),
        "untracked parent\n"
    );
    assert_eq!(
        std::fs::read_link(child_repo.join("external-link")).expect("child symlink"),
        PathBuf::from("/etc/passwd")
    );
    assert_eq!(
        std::fs::read_link(baseline_repo.join("external-link")).expect("baseline symlink"),
        PathBuf::from("/etc/passwd")
    );
    assert_git_paths_inside(&child_repo);
}

#[tokio::test]
async fn materialize_session_workspaces_from_local_folder() {
    let temp = TempDir::new("workspace-manager-local");
    let source = temp.path().join("source");
    std::fs::create_dir_all(source.join("nested")).expect("source dirs");
    std::fs::write(source.join("README.md"), "hello\n").expect("source file");
    std::fs::write(source.join("nested/data.txt"), "nested\n").expect("nested source file");
    make_symlink(Path::new("README.md"), &source.join("readme-link"));
    make_symlink(Path::new("/etc/passwd"), &source.join("external-link"));

    let manager = WorkspaceManager::new(temp.path().join("state"));
    let project_id = Uuid::new_v4();
    let project_workspaces = vec![ProjectWorkspace::local(
        "local-repo",
        source.to_string_lossy(),
    )];

    let (cwd, workspaces) = manager
        .materialize_session(project_id, "session-local", &project_workspaces)
        .await
        .expect("materialize local session");
    let target = Path::new(&cwd).join("local-repo");
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0].workspace_dir, "local-repo");
    assert_eq!(workspaces[0].kind, WorkspaceKind::Local);
    assert_eq!(
        std::fs::read_to_string(target.join("README.md")).expect("target file"),
        "hello\n"
    );
    assert_eq!(
        std::fs::read_to_string(target.join("nested/data.txt")).expect("nested target file"),
        "nested\n"
    );
    assert_eq!(
        std::fs::read_link(target.join("readme-link")).expect("safe symlink"),
        PathBuf::from("README.md")
    );
    assert!(std::fs::read_to_string(target.join("external-link"))
        .expect("external symlink marker")
        .contains("skipped external symlink target: /etc/passwd"));

    std::fs::write(source.join("README.md"), "updated\n").expect("update source file");
    std::fs::remove_file(source.join("nested/data.txt")).expect("remove deleted source file");
    std::fs::write(source.join("new.txt"), "new\n").expect("new source file");

    let (cwd, _) = manager
        .materialize_session(project_id, "session-local-2", &project_workspaces)
        .await
        .expect("materialize refreshed local session");
    let refreshed = Path::new(&cwd).join("local-repo");
    assert_eq!(
        std::fs::read_to_string(refreshed.join("README.md")).expect("updated target file"),
        "updated\n"
    );
    assert!(
        !refreshed.join("nested/data.txt").exists(),
        "destructive base refresh should delete files removed from the source"
    );
    assert_eq!(
        std::fs::read_to_string(refreshed.join("new.txt")).expect("new target file"),
        "new\n"
    );
}

#[tokio::test]
async fn materialize_session_snapshots_managed_btrfs_base_when_available() {
    let Some(temp) = TempDir::new_under_home("workspace-manager-btrfs") else {
        return;
    };
    let source = temp.path().join("source");
    std::fs::create_dir_all(&source).expect("source dir");
    std::fs::write(source.join("large.bin"), vec![b'x'; 1024 * 1024]).expect("source file");
    make_symlink(Path::new("/etc/passwd"), &source.join("external-link"));

    let manager = WorkspaceManager::new(temp.path().join("state"));
    let project_id = Uuid::new_v4();
    let project_workspaces = vec![ProjectWorkspace::local(
        "local-repo",
        source.to_string_lossy(),
    )];

    let (cwd, _) = manager
        .materialize_session(project_id, "session-btrfs", &project_workspaces)
        .await
        .expect("materialize btrfs session");
    let target = Path::new(&cwd).join("local-repo");
    let base = manager
        .workspace_base_slot(project_id, "local-repo")
        .join(WORKSPACE_BASE_DIR);

    if !is_btrfs_subvolume(&base) || !is_btrfs_subvolume(&target) {
        return;
    }
    assert!(
        btrfs_files_have_shared_extents(&base.join("large.bin"), &target.join("large.bin")),
        "expected the session workspace to be a Btrfs snapshot of the managed base"
    );
    assert!(std::fs::read_to_string(target.join("external-link"))
        .expect("external symlink marker")
        .contains("skipped external symlink target: /etc/passwd"));
}

#[tokio::test]
async fn workspace_base_config_changes_recreate_base() {
    let temp = TempDir::new("workspace-manager-base-recreate");
    let manager = WorkspaceManager::new(temp.path().join("state"));
    let project_id = Uuid::new_v4();
    let source_a = temp.path().join("source-a");
    let source_b = temp.path().join("source-b");
    std::fs::create_dir_all(&source_a).expect("source a");
    std::fs::create_dir_all(&source_b).expect("source b");
    std::fs::write(source_a.join("only-a.txt"), "a\n").expect("source a file");
    std::fs::write(source_b.join("only-b.txt"), "b\n").expect("source b file");

    let workspace_a = vec![ProjectWorkspace::local(
        "local-repo",
        source_a.to_string_lossy(),
    )];
    manager
        .materialize_session(project_id, "session-a", &workspace_a)
        .await
        .expect("materialize source a");

    let workspace_b = vec![ProjectWorkspace::local(
        "local-repo",
        source_b.to_string_lossy(),
    )];
    let (cwd, _) = manager
        .materialize_session(project_id, "session-b", &workspace_b)
        .await
        .expect("materialize source b");
    let target = Path::new(&cwd).join("local-repo");
    assert!(!target.join("only-a.txt").exists());
    assert_eq!(
        std::fs::read_to_string(target.join("only-b.txt")).expect("source b file"),
        "b\n"
    );
}

#[tokio::test]
async fn workspace_name_change_removes_old_base() {
    let temp = TempDir::new("workspace-manager-base-rename");
    let manager = WorkspaceManager::new(temp.path().join("state"));
    let project_id = Uuid::new_v4();
    let source = temp.path().join("source");
    std::fs::create_dir_all(&source).expect("source");
    std::fs::write(source.join("README.md"), "hello\n").expect("source file");

    let old_workspace = vec![ProjectWorkspace::local(
        "old-name",
        source.to_string_lossy(),
    )];
    manager
        .materialize_session(project_id, "session-old", &old_workspace)
        .await
        .expect("materialize old workspace");
    assert!(manager.workspace_base_slot(project_id, "old-name").exists());

    let new_workspace = vec![ProjectWorkspace::local(
        "new-name",
        source.to_string_lossy(),
    )];
    manager
        .materialize_session(project_id, "session-new", &new_workspace)
        .await
        .expect("materialize renamed workspace");
    assert!(!manager.workspace_base_slot(project_id, "old-name").exists());
    assert!(manager.workspace_base_slot(project_id, "new-name").exists());
}

#[test]
fn workspace_dir_validation_rejects_paths_and_hidden_dirs() {
    assert!(validate_workspace_dir("repo").is_ok());
    assert!(validate_workspace_dir("repo_1").is_ok());
    assert!(validate_workspace_dir(".repo").is_err());
    assert!(validate_workspace_dir("nested/repo").is_err());
    assert!(validate_workspace_dir("../repo").is_err());
    assert!(validate_workspace_dir("repo.name").is_err());
}

fn make_symlink(target: &Path, link: &Path) {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).expect("create symlink");
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link).expect("create symlink");
    }
}

fn is_btrfs_subvolume(path: &Path) -> bool {
    std::process::Command::new("btrfs")
        .args(["subvolume", "show"])
        .arg(path)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn btrfs_files_have_shared_extents(left: &Path, right: &Path) -> bool {
    let output = std::process::Command::new("filefrag")
        .arg("-v")
        .arg(left)
        .arg(right)
        .output()
        .expect("run filefrag");
    assert!(
        output.status.success(),
        "filefrag failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).contains("shared")
}

fn git<const N: usize>(cwd: &Path, args: [&str; N]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git failed in {}: {}",
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout<const N: usize>(cwd: &Path, args: [&str; N]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git failed in {}: {}",
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn assert_git_paths_inside(workspace: &Path) {
    let root = workspace.canonicalize().expect("canonical workspace");
    for args in [
        ["rev-parse", "--git-dir"],
        ["rev-parse", "--git-common-dir"],
    ] {
        let output = git_stdout(workspace, args);
        let path = Path::new(&output);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        let path = path.canonicalize().expect("canonical git path");
        assert!(
            path.starts_with(&root),
            "git path {} should be inside {}",
            path.display(),
            root.display()
        );
    }
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        Self::new_in(std::env::temp_dir(), prefix)
    }

    fn new_under_home(prefix: &str) -> Option<Self> {
        let home = std::env::var_os("HOME").filter(|value| !value.is_empty())?;
        let base = PathBuf::from(home).join(".local/state/pi-relay/test-tmp");
        std::fs::create_dir_all(&base).ok()?;
        let temp = Self::new_in(base, prefix);
        if can_create_btrfs_subvolume(temp.path()) {
            Some(temp)
        } else {
            None
        }
    }

    fn new_in(base: PathBuf, prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path = base.join(format!("pi-relay-{prefix}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).expect("temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

fn can_create_btrfs_subvolume(parent: &Path) -> bool {
    let probe = parent.join("probe-subvol");
    let created = std::process::Command::new("btrfs")
        .args(["subvolume", "create"])
        .arg(&probe)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if created {
        let _ = std::fs::remove_dir_all(&probe);
    }
    created
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
