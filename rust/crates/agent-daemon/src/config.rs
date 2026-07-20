use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use agent_vocab::{ProviderConfig, ProviderKind, ReasoningEffort};
use anyhow::{anyhow, Context, Result};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

const DAEMON_CONFIG_FILE: &str = "config.toml";
const DEFAULT_BIND: &str = "127.0.0.1:8787";
const DEFAULT_RUNTIME_BIND: &str = "127.0.0.1:8786";
const BOOTSTRAP_MARKER: &str = ".bootstrap-v1";
const BOOTSTRAP_STAGING_PREFIX: &str = ".bootstrap-staging-";

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) database_url: String,
    pub(crate) bind: String,
    pub(crate) runtime_bind: String,
    pub(crate) config_root: PathBuf,
    pub(crate) daemon_config: DaemonConfig,
}

/// General daemon settings stored in `$XDG_CONFIG_HOME/pi-relay/config.toml`
/// (or `$HOME/.config/pi-relay/config.toml`).
#[derive(Debug, Clone)]
pub(crate) struct DaemonConfig {
    pub(crate) default_parent_model: ProviderConfig,
    pub(crate) subagent_models: BTreeMap<String, ProviderConfig>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            default_parent_model: stable_default_provider(),
            subagent_models: BTreeMap::new(),
        }
    }
}

pub(crate) fn stable_default_provider() -> ProviderConfig {
    ProviderConfig {
        kind: ProviderKind::OpenAi,
        model: "gpt-5.6-sol".to_string(),
        reasoning_effort: ReasoningEffort::High,
        max_tokens: None,
        prompt_cache: None,
    }
}

impl Config {
    pub(crate) fn from_env_and_args() -> Result<Self> {
        Self::from_values(
            env::var_os("XDG_CONFIG_HOME"),
            env::var_os("HOME"),
            env::args().skip(1).collect(),
        )
    }

    pub(crate) fn bootstrap_catalog(&self, prompt_root: &Path) -> Result<()> {
        bootstrap_catalog(prompt_root, &self.config_root)
    }

    fn from_values(
        xdg_config_home: Option<std::ffi::OsString>,
        home: Option<std::ffi::OsString>,
        args: Vec<String>,
    ) -> Result<Self> {
        if let Some(argument) = args.first() {
            return Err(anyhow!(
                "pi-agentd accepts no arguments; configure it in {DAEMON_CONFIG_FILE} (unknown argument: {argument})"
            ));
        }

        let config_root = config_root_from_env(xdg_config_home.as_deref(), home.as_deref())?;
        let policy = load_daemon_config(&config_root.join(DAEMON_CONFIG_FILE))?;

        Ok(Self {
            database_url: policy.database_url,
            bind: policy.bind,
            runtime_bind: policy.runtime_bind,
            config_root,
            daemon_config: policy.daemon_config,
        })
    }
}

fn config_root_from_env(
    xdg_config_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Result<PathBuf> {
    if let Some(xdg_config_home) = xdg_config_home.filter(|value| !value.is_empty()) {
        let config_home = PathBuf::from(xdg_config_home);
        if !config_home.is_absolute() {
            return Err(anyhow!("XDG_CONFIG_HOME must be an absolute path"));
        }
        return Ok(config_home.join("pi-relay"));
    }
    let home = home
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("HOME is required when XDG_CONFIG_HOME is unset"))?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return Err(anyhow!("HOME must be an absolute path"));
    }
    Ok(home.join(".config").join("pi-relay"))
}

fn load_daemon_config(path: &Path) -> Result<DaemonStartupPolicy> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow!(
                "daemon configuration {} is required",
                path.display()
            ))
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read daemon config {}", path.display()))
        }
    };
    let parsed: DaemonConfigFile = toml::from_str(
        std::str::from_utf8(&bytes)
            .with_context(|| format!("parse daemon config {}", path.display()))?,
    )
    .with_context(|| format!("parse daemon config {}", path.display()))?;
    parsed
        .try_into()
        .with_context(|| format!("validate daemon config {}", path.display()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DaemonConfigFile {
    database_url: Option<String>,
    #[serde(default)]
    bind: Option<String>,
    #[serde(default)]
    runtime_bind: Option<String>,
    #[serde(default)]
    default_parent_model: Option<StrictProviderConfig>,
    #[serde(default)]
    subagent_models: BTreeMap<String, StrictProviderConfig>,
}

#[derive(Debug)]
struct DaemonStartupPolicy {
    database_url: String,
    bind: String,
    runtime_bind: String,
    daemon_config: DaemonConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictProviderConfig {
    kind: ProviderKind,
    model: String,
    #[serde(default)]
    reasoning_effort: ReasoningEffort,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    prompt_cache: Option<Value>,
}

impl TryFrom<DaemonConfigFile> for DaemonStartupPolicy {
    type Error = anyhow::Error;

    fn try_from(value: DaemonConfigFile) -> Result<Self> {
        let database_url = value
            .database_url
            .ok_or_else(|| anyhow!("database_url is required"))?;
        if database_url.trim().is_empty() {
            return Err(anyhow!("database_url must not be blank"));
        }

        let bind = value.bind.unwrap_or_else(|| DEFAULT_BIND.to_string());
        if bind.trim().is_empty() {
            return Err(anyhow!("bind must not be blank"));
        }
        let runtime_bind = value
            .runtime_bind
            .unwrap_or_else(|| DEFAULT_RUNTIME_BIND.to_string());
        if runtime_bind.trim().is_empty() {
            return Err(anyhow!("runtime_bind must not be blank"));
        }

        let default_parent_model = match value.default_parent_model {
            Some(provider) => {
                validate_model("default_parent_model", &provider.model)?;
                provider_from_strict(provider)
            }
            None => stable_default_provider(),
        };
        let mut subagent_models = BTreeMap::new();
        for (role, provider) in value.subagent_models {
            validate_model(&format!("subagent_models.{role}"), &provider.model)?;
            subagent_models.insert(role, provider_from_strict(provider));
        }
        Ok(Self {
            database_url,
            bind,
            runtime_bind,
            daemon_config: DaemonConfig {
                default_parent_model,
                subagent_models,
            },
        })
    }
}

fn validate_model(field: &str, model: &str) -> Result<()> {
    if model.trim().is_empty() {
        return Err(anyhow!("{field}.model must not be blank"));
    }
    Ok(())
}

fn provider_from_strict(provider: StrictProviderConfig) -> ProviderConfig {
    ProviderConfig {
        kind: provider.kind,
        model: provider.model,
        reasoning_effort: provider.reasoning_effort,
        max_tokens: provider.max_tokens,
        prompt_cache: provider.prompt_cache,
    }
}

/// Copy bundled catalogs once. The marker deliberately makes later deletions
/// user-owned: upgrades do not repopulate a role or workflow a user removed.
fn bootstrap_catalog(prompt_root: &Path, config_root: &Path) -> Result<()> {
    let mut no_hook = |_| {};
    let mut no_staging_hook = |_write: BootstrapWrite| Ok::<(), std::io::Error>(());
    bootstrap_catalog_with_hooks(prompt_root, config_root, &mut no_hook, &mut no_staging_hook)
}

/// Directory handles remain valid when their pathname is renamed. The callback
/// is only used by tests to deterministically exercise that property.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BootstrapDirectory {
    ConfigRoot,
    Catalog,
    Asset,
}

/// The test-only staging callback distinguishes assets from the completion
/// marker so tests can inject failures after a staging leaf is created.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BootstrapWrite {
    Asset,
    Marker,
}

#[cfg(test)]
fn bootstrap_catalog_with_hook(
    prompt_root: &Path,
    config_root: &Path,
    after_open: &mut dyn FnMut(BootstrapDirectory),
) -> Result<()> {
    let mut no_staging_hook = |_write: BootstrapWrite| Ok::<(), std::io::Error>(());
    bootstrap_catalog_with_hooks(prompt_root, config_root, after_open, &mut no_staging_hook)
}

fn bootstrap_catalog_with_hooks(
    prompt_root: &Path,
    config_root: &Path,
    after_open: &mut dyn FnMut(BootstrapDirectory),
    after_staging: &mut dyn FnMut(BootstrapWrite) -> std::io::Result<()>,
) -> Result<()> {
    let config_root_dir = open_config_root(config_root)?;
    after_open(BootstrapDirectory::ConfigRoot);

    if existing_non_symlink(
        &config_root_dir,
        BOOTSTRAP_MARKER,
        &config_root.join(BOOTSTRAP_MARKER),
    )? {
        return Ok(());
    }

    for catalog in ["subagent-roles", "workflows"] {
        let catalog_path = config_root.join(catalog);
        let catalog_dir = open_or_create_dir(
            &config_root_dir,
            std::ffi::OsStr::new(catalog),
            &catalog_path,
        )?;
        after_open(BootstrapDirectory::Catalog);
        copy_missing_catalog_files(
            &prompt_root.join(catalog),
            &config_root_dir,
            &catalog_dir,
            &catalog_path,
            after_open,
            after_staging,
        )?;
    }
    write_missing_file(
        &config_root_dir,
        &config_root_dir,
        BOOTSTRAP_MARKER,
        b"v1\n",
        &config_root.join(BOOTSTRAP_MARKER),
        BootstrapWrite::Marker,
        after_staging,
    )?;
    Ok(())
}

fn open_config_root(config_root: &Path) -> Result<Dir> {
    let config_home = config_root
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("configuration root has no configuration-home parent"))?;
    let root_name = match config_root.components().next_back() {
        Some(Component::Normal(name)) => name,
        _ => return Err(anyhow!("configuration root has no normal final component")),
    };
    let config_home_dir = open_or_create_config_home(config_home)?;
    open_or_create_dir(&config_home_dir, root_name, config_root)
}

/// Opens a configuration home from its filesystem root, creating only missing
/// normal components through stable no-follow directory handles. Relative and
/// parent-directory paths are deliberately unsupported: an XDG configuration
/// home must identify an absolute location independent of the daemon's cwd.
fn open_or_create_config_home(config_home: &Path) -> Result<Dir> {
    if !config_home.is_absolute() {
        return Err(anyhow!(
            "configuration home must be an absolute path: {}",
            config_home.display()
        ));
    }

    let anchor = config_home
        .ancestors()
        .last()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("configuration home has no filesystem root"))?;
    let relative_path = config_home
        .strip_prefix(anchor)
        .expect("an ancestor is always a path prefix");
    for component in relative_path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(anyhow!(
                "configuration home contains unsupported path component: {}",
                component.as_os_str().to_string_lossy()
            ));
        }
    }
    let mut directory = Dir::open_ambient_dir(anchor, ambient_authority()).with_context(|| {
        format!(
            "open configuration-home filesystem root {}",
            anchor.display()
        )
    })?;
    let mut logical_path = anchor.to_path_buf();

    for component in relative_path.components() {
        let Component::Normal(name) = component else {
            unreachable!("configuration-home components were validated above")
        };
        logical_path.push(name);
        directory = open_or_create_dir(&directory, name, &logical_path)?;
    }
    Ok(directory)
}

fn copy_missing_catalog_files(
    source_root: &Path,
    staging_root: &Dir,
    destination_root: &Dir,
    destination_root_path: &Path,
    after_open: &mut dyn FnMut(BootstrapDirectory),
    after_staging: &mut dyn FnMut(BootstrapWrite) -> std::io::Result<()>,
) -> Result<()> {
    let entries = fs::read_dir(source_root)
        .with_context(|| format!("read packaged catalog {}", source_root.display()))?;
    let mut entries = entries
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("enumerate packaged catalog {}", source_root.display()))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let source_file = entry.path().join("SKILL.md");
        if !entry.file_type()?.is_dir() || !source_file.is_file() {
            continue;
        }
        let asset_name = entry.file_name();
        let asset_path = destination_root_path.join(&asset_name);
        let destination_dir = open_or_create_dir(destination_root, &asset_name, &asset_path)?;
        after_open(BootstrapDirectory::Asset);
        let destination = asset_path.join("SKILL.md");
        if existing_non_symlink(&destination_dir, "SKILL.md", &destination)? {
            continue;
        }
        let content = fs::read(&source_file)
            .with_context(|| format!("read packaged catalog file {}", source_file.display()))?;
        if !write_missing_file(
            staging_root,
            &destination_dir,
            "SKILL.md",
            &content,
            &destination,
            BootstrapWrite::Asset,
            after_staging,
        )? {
            continue;
        }
    }
    Ok(())
}

/// Open one direct child of an already-open configuration directory.
/// `open_dir_nofollow` atomically rejects a symlink at the component being
/// opened; after opening, all descendants are accessed relative to the
/// returned stable handle.
fn open_or_create_dir(parent: &Dir, name: &std::ffi::OsStr, logical_path: &Path) -> Result<Dir> {
    match parent.open_dir_nofollow(name) {
        Ok(dir) => Ok(dir),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match parent.create_dir(name) {
                Ok(()) => {}
                Err(ref error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("create bootstrap directory {}", logical_path.display())
                    })
                }
            }
            open_existing_dir_nofollow(parent, name, logical_path)
        }
        Err(_) => open_existing_dir_nofollow(parent, name, logical_path),
    }
}

fn open_existing_dir_nofollow(
    parent: &Dir,
    name: &std::ffi::OsStr,
    logical_path: &Path,
) -> Result<Dir> {
    match parent.open_dir_nofollow(name) {
        Ok(dir) => Ok(dir),
        Err(error) => {
            reject_symlink(parent, name, logical_path, "directory")?;
            Err(error)
                .with_context(|| format!("open bootstrap directory {}", logical_path.display()))
        }
    }
}

/// Returns whether an existing non-symlink entry is present. Existing user
/// files are deliberately left untouched, but bootstrap-owned leaves may not
/// be symlinks.
fn existing_non_symlink(parent: &Dir, name: &str, logical_path: &Path) -> Result<bool> {
    match parent.symlink_metadata(name) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(anyhow!(
            "refusing symlinked bootstrap file {}",
            logical_path.display()
        )),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("inspect bootstrap file {}", logical_path.display()))
        }
    }
}

fn reject_symlink(
    parent: &Dir,
    name: &std::ffi::OsStr,
    logical_path: &Path,
    entry_kind: &str,
) -> Result<()> {
    match parent.symlink_metadata(name) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(anyhow!(
            "refusing symlinked bootstrap {entry_kind} {}",
            logical_path.display()
        )),
        Ok(_) | Err(_) => Ok(()),
    }
}

/// Writes a unique, hidden staging leaf through the stable configuration-root
/// handle. It is deliberately never removed: after an error, a pathname no
/// longer proves that the leaf is still bootstrap-owned, so deleting it could
/// remove a raced user file.
fn write_staging_file(
    staging_root: &Dir,
    content: &[u8],
    after_staging: &mut dyn FnMut() -> std::io::Result<()>,
) -> std::io::Result<String> {
    for _ in 0..16 {
        let staging_name = format!("{BOOTSTRAP_STAGING_PREFIX}{}", Uuid::new_v4());
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let mut file = match staging_root.open_with(&staging_name, &options) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        after_staging()?;
        file.write_all(content)?;
        file.sync_all()?;
        return Ok(staging_name);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "unable to allocate a unique bootstrap staging leaf",
    ))
}

/// Publishes a fully written bootstrap leaf only when its final name is
/// absent. `Dir::hard_link` is cap-std's capability-relative wrapper around
/// the OS no-replace link primitive, unlike rename which can replace a user
/// entry. On a concurrent creation, inspect through the same stable parent
/// handle so a symlink still produces a clear refusal rather than a generic
/// create error.
fn write_missing_file(
    staging_root: &Dir,
    parent: &Dir,
    name: &str,
    content: &[u8],
    logical_path: &Path,
    write: BootstrapWrite,
    after_staging: &mut dyn FnMut(BootstrapWrite) -> std::io::Result<()>,
) -> Result<bool> {
    let staging_name = write_staging_file(staging_root, content, &mut || after_staging(write))
        .with_context(|| format!("write bootstrap file {}", logical_path.display()))?;

    match staging_root.hard_link(&staging_name, parent, name) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if existing_non_symlink(parent, name, logical_path)? {
                Ok(false)
            } else {
                Err(error)
                    .with_context(|| format!("write bootstrap file {}", logical_path.display()))
            }
        }
        Err(error) => {
            reject_symlink(parent, std::ffi::OsStr::new(name), logical_path, "file")?;
            Err(error).with_context(|| format!("write bootstrap file {}", logical_path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    #[cfg(unix)]
    use std::{
        os::unix::fs::{symlink, PermissionsExt},
        path::Path,
    };

    #[test]
    fn xdg_config_root_does_not_nest_dot_config() {
        let root = config_root_from_env(Some("/tmp/xdg".as_ref()), Some("/home/test".as_ref()))
            .expect("config root");
        assert_eq!(root, PathBuf::from("/tmp/xdg/pi-relay"));
    }

    #[test]
    fn config_root_falls_back_to_home_dot_config() {
        let root = config_root_from_env(None, Some("/home/test".as_ref())).expect("config root");
        assert_eq!(root, PathBuf::from("/home/test/.config/pi-relay"));
    }

    #[test]
    fn config_root_rejects_relative_environment_paths() {
        assert!(
            config_root_from_env(Some("relative/xdg".as_ref()), None).is_err(),
            "relative XDG_CONFIG_HOME is rejected"
        );
        assert!(
            config_root_from_env(None, Some("relative-home".as_ref())).is_err(),
            "relative HOME is rejected"
        );
    }

    #[test]
    fn configuration_home_rejects_parent_component_before_creating_directories() {
        let temp = make_temp_dir("parent-component");
        let config_home = temp.join("not-created/../xdg");

        let error = open_or_create_config_home(&config_home)
            .expect_err("parent-directory configuration-home component is rejected");

        assert!(format!("{error:#}").contains("unsupported path component"));
        assert!(!temp.join("not-created").exists());
        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn bootstrap_creates_missing_xdg_config_home_components() {
        let temp = make_temp_dir("missing-xdg-home");
        let config_root = temp.join("missing/xdg/pi-relay");
        let prompt_root = make_temp_dir("missing-xdg-source");
        write_catalog_fixture(&prompt_root);

        bootstrap_catalog(&prompt_root, &config_root).expect("bootstrap");

        assert_catalog_and_marker_created(&config_root);
        fs::remove_dir_all(temp).ok();
        fs::remove_dir_all(prompt_root).ok();
    }

    #[test]
    fn bootstrap_creates_missing_home_dot_config_components() {
        let temp = make_temp_dir("missing-home-config");
        let config_root = temp.join("missing-home/.config/pi-relay");
        let prompt_root = make_temp_dir("missing-home-source");
        write_catalog_fixture(&prompt_root);

        bootstrap_catalog(&prompt_root, &config_root).expect("bootstrap");

        assert_catalog_and_marker_created(&config_root);
        fs::remove_dir_all(temp).ok();
        fs::remove_dir_all(prompt_root).ok();
    }

    #[test]
    fn config_is_strict_and_preserves_full_provider_configuration() {
        let root = make_temp_dir("strict-config");
        fs::create_dir_all(&root).expect("config root");
        fs::write(
            root.join(DAEMON_CONFIG_FILE),
            r#"
database_url = "postgres://example"
bind = "127.0.0.1:9999"

[default_parent_model]
kind = "openai"
model = "parent"
reasoning_effort = "high"
max_tokens = 123
prompt_cache = { key = "parent-cache" }

[subagent_models.reviewer]
kind = "claude"
model = "review"
reasoning_effort = "low"
max_tokens = 456
prompt_cache = { key = "review-cache" }
"#,
        )
        .expect("write config");

        let config = load_daemon_config(&root.join(DAEMON_CONFIG_FILE)).expect("parse config");
        let reviewer = config
            .daemon_config
            .subagent_models
            .get("reviewer")
            .expect("reviewer");
        assert_eq!(config.database_url, "postgres://example");
        assert_eq!(config.bind, "127.0.0.1:9999");
        assert_eq!(config.daemon_config.default_parent_model.model, "parent");
        assert_eq!(
            config.daemon_config.default_parent_model.max_tokens,
            Some(123)
        );
        assert_eq!(
            config.daemon_config.default_parent_model.prompt_cache,
            Some(serde_json::json!({"key": "parent-cache"}))
        );
        assert_eq!(reviewer.kind, ProviderKind::Claude);
        assert_eq!(reviewer.max_tokens, Some(456));
        assert_eq!(
            reviewer.prompt_cache,
            Some(serde_json::json!({"key": "review-cache"}))
        );

        fs::write(
            root.join(DAEMON_CONFIG_FILE),
            r#"
database_url = "postgres://example"

[default_parent_model]
kind = "openai"
model = "x"
unexpected = true
"#,
        )
        .expect("write invalid config");
        let error = load_daemon_config(&root.join(DAEMON_CONFIG_FILE))
            .expect_err("unknown nested provider field is rejected");
        assert!(format!("{error:#}").contains("unknown field"));

        fs::write(
            root.join(DAEMON_CONFIG_FILE),
            r#"
database_url = "postgres://example"

[default_parent_model]
kind = "openai"
model = "x"

unexpected = true
"#,
        )
        .expect("write invalid config");
        let error = load_daemon_config(&root.join(DAEMON_CONFIG_FILE))
            .expect_err("unknown root field is rejected");
        assert!(format!("{error:#}").contains("unknown field"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn config_requires_database_url_defaults_bind_and_defaults_parent_model() {
        let root = make_temp_dir("default-config");
        let error = load_daemon_config(&root.join(DAEMON_CONFIG_FILE))
            .expect_err("missing config is rejected");
        assert!(format!("{error:#}").contains("is required"));

        fs::write(root.join(DAEMON_CONFIG_FILE), "").expect("write missing database URL config");
        let error = load_daemon_config(&root.join(DAEMON_CONFIG_FILE))
            .expect_err("missing database URL is rejected");
        assert!(format!("{error:#}").contains("database_url is required"));

        fs::write(
            root.join(DAEMON_CONFIG_FILE),
            r#"
database_url = "postgres://example"
"#,
        )
        .expect("write default policy");
        let config = load_daemon_config(&root.join(DAEMON_CONFIG_FILE)).expect("default policy");
        assert_eq!(config.database_url, "postgres://example");
        assert_eq!(config.bind, DEFAULT_BIND);
        assert_eq!(
            config.daemon_config.default_parent_model.kind,
            ProviderKind::OpenAi
        );
        assert_eq!(
            config.daemon_config.default_parent_model.model,
            "gpt-5.6-sol"
        );
        assert_eq!(
            config.daemon_config.default_parent_model.reasoning_effort,
            ReasoningEffort::High
        );

        fs::write(
            root.join(DAEMON_CONFIG_FILE),
            r#"
database_url = "   "
"#,
        )
        .expect("write blank database URL config");
        let error = load_daemon_config(&root.join(DAEMON_CONFIG_FILE))
            .expect_err("blank database URL rejected");
        assert!(format!("{error:#}").contains("database_url must not be blank"));

        fs::write(
            root.join(DAEMON_CONFIG_FILE),
            r#"
database_url = "postgres://example"
bind = "   "
"#,
        )
        .expect("write blank bind config");
        let error =
            load_daemon_config(&root.join(DAEMON_CONFIG_FILE)).expect_err("blank bind rejected");
        assert!(format!("{error:#}").contains("bind must not be blank"));

        fs::write(
            root.join(DAEMON_CONFIG_FILE),
            r#"
database_url = "postgres://example"

[default_parent_model]
kind = "openai"
model = "   "
"#,
        )
        .expect("write blank config");
        let error =
            load_daemon_config(&root.join(DAEMON_CONFIG_FILE)).expect_err("blank model rejected");
        assert!(format!("{error:#}").contains("must not be blank"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn daemon_policy_uses_toml_and_ignores_legacy_json() {
        let root = make_temp_dir("toml-only-config");
        let config_root = root.join("pi-relay");
        fs::create_dir_all(&config_root).expect("config root");
        fs::write(config_root.join("config.json"), b"not valid JSON").expect("legacy config");

        let error = config_from_values(&root, Vec::new())
            .expect_err("legacy JSON is ignored and cannot satisfy required TOML policy");
        assert!(format!("{error:#}").contains("is required"));

        fs::write(
            config_root.join(DAEMON_CONFIG_FILE),
            r#"
database_url = "postgres://toml"

[default_parent_model]
kind = "claude"
model = "toml-parent"
"#,
        )
        .expect("TOML config");

        let present = config_from_values(&root, Vec::new()).expect("TOML config wins");
        assert_eq!(present.database_url, "postgres://toml");
        assert_eq!(
            present.daemon_config.default_parent_model.kind,
            ProviderKind::Claude
        );
        assert_eq!(
            present.daemon_config.default_parent_model.model,
            "toml-parent"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn daemon_accepts_no_configuration_arguments() {
        let root = make_temp_dir("no-arguments");
        let config_root = root.join("pi-relay");
        fs::create_dir_all(&config_root).expect("config root");
        fs::write(
            config_root.join(DAEMON_CONFIG_FILE),
            r#"database_url = "postgres://test""#,
        )
        .expect("daemon config");

        for argument in ["--database-url", "--bind", "--mcp-config"] {
            let error = config_from_values(&root, vec![argument.to_string()])
                .expect_err("former configuration argument is rejected");
            let message = format!("{error:#}");
            assert!(message.contains("accepts no arguments"), "{message}");
            assert!(message.contains(argument), "{message}");
        }

        let error = config_from_values(&root, vec!["unexpected".to_string()])
            .expect_err("unknown argument is rejected");
        assert!(format!("{error:#}").contains("unknown argument: unexpected"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn bootstrap_is_idempotent_non_overwriting_and_does_not_restore_deletions() {
        let prompt_root = make_temp_dir("bootstrap-source");
        let config_root = make_temp_dir("bootstrap-config");
        write_skill(
            &prompt_root.join("subagent-roles/reviewer/SKILL.md"),
            "reviewer",
            "Bundled reviewer",
        );
        write_skill(
            &prompt_root.join("workflows/workflow-review/SKILL.md"),
            "workflow-review",
            "Bundled workflow",
        );
        let existing = config_root.join("subagent-roles/reviewer/SKILL.md");
        write_skill(&existing, "reviewer", "User reviewer");
        let manual_mcp = config_root.join("mcp.toml");
        let mcp_bytes = b"# manually managed MCP configuration\n";
        fs::write(&manual_mcp, mcp_bytes).expect("manual MCP");

        bootstrap_catalog(&prompt_root, &config_root).expect("bootstrap");
        assert!(config_root.join(BOOTSTRAP_MARKER).is_file());
        let existing_bytes = fs::read(&existing).expect("existing role");
        assert_eq!(
            existing_bytes,
            b"---\nname: reviewer\ndescription: User reviewer\n---\n\nBody.\n"
        );
        let workflow = config_root.join("workflows/workflow-review/SKILL.md");
        assert!(workflow.is_file());
        assert_eq!(fs::read(&manual_mcp).expect("MCP untouched"), mcp_bytes);

        fs::remove_file(&workflow).expect("delete workflow intentionally");
        bootstrap_catalog(&prompt_root, &config_root).expect("repeat bootstrap");
        assert!(!workflow.exists());
        assert_eq!(fs::read(&manual_mcp).expect("MCP untouched"), mcp_bytes);

        let no_mcp_root = make_temp_dir("bootstrap-no-mcp");
        bootstrap_catalog(&prompt_root, &no_mcp_root).expect("bootstrap without MCP");
        assert!(!no_mcp_root.join("mcp.toml").exists());

        fs::remove_dir_all(prompt_root).ok();
        fs::remove_dir_all(config_root).ok();
        fs::remove_dir_all(no_mcp_root).ok();
    }

    #[test]
    fn bootstrap_preserves_preexisting_config_artifacts_and_catalog_permissions() {
        let prompt_root = make_temp_dir("preserve-source");
        let config_root = make_temp_dir("preserve-config");
        write_skill(
            &prompt_root.join("subagent-roles/reviewer/SKILL.md"),
            "reviewer",
            "Bundled reviewer",
        );
        write_skill(
            &prompt_root.join("workflows/review/SKILL.md"),
            "review",
            "Bundled review workflow",
        );

        let config = config_root.join(DAEMON_CONFIG_FILE);
        let mcp = config_root.join("mcp.toml");
        let sentinel = config_root.join("unrelated-sentinel");
        let role = config_root.join("subagent-roles/reviewer/SKILL.md");
        let workflow = config_root.join("workflows/review/SKILL.md");
        fs::write(&config, b"config fixture bytes").expect("config fixture");
        fs::write(&mcp, b"mcp fixture bytes").expect("MCP fixture");
        fs::write(&sentinel, b"sentinel fixture bytes").expect("sentinel fixture");
        write_skill(&role, "reviewer", "User reviewer");
        write_skill(&workflow, "review", "User review workflow");

        let catalog_dirs = [
            config_root.join("subagent-roles"),
            config_root.join("subagent-roles/reviewer"),
            config_root.join("workflows"),
            config_root.join("workflows/review"),
        ];
        #[cfg(unix)]
        for (index, path) in catalog_dirs.iter().enumerate() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o750 - index as u32))
                .expect("set catalog permissions");
        }

        let files = [&config, &mcp, &sentinel, &role, &workflow];
        let before = files
            .iter()
            .map(|path| (path, fs::read(path).expect("read existing artifact")))
            .collect::<Vec<_>>();
        #[cfg(unix)]
        let directory_modes = catalog_dirs
            .iter()
            .map(|path| (path, file_mode(path)))
            .collect::<Vec<_>>();
        #[cfg(unix)]
        let file_modes = files
            .iter()
            .map(|path| (*path, file_mode(path)))
            .collect::<Vec<_>>();

        bootstrap_catalog(&prompt_root, &config_root).expect("bootstrap");

        for (path, bytes) in before {
            assert_eq!(fs::read(path).expect("read preserved artifact"), bytes);
        }
        #[cfg(unix)]
        {
            for (path, mode) in directory_modes {
                assert_eq!(
                    file_mode(path),
                    mode,
                    "{} permissions changed",
                    path.display()
                );
            }
            for (path, mode) in file_modes {
                assert_eq!(
                    file_mode(path),
                    mode,
                    "{} permissions changed",
                    path.display()
                );
            }
        }

        fs::remove_dir_all(prompt_root).ok();
        fs::remove_dir_all(config_root).ok();
    }

    #[test]
    fn existing_bootstrap_marker_is_preserved_and_prevents_catalog_restoration() {
        let prompt_root = make_temp_dir("marker-source");
        let config_root = make_temp_dir("marker-config");
        write_skill(
            &prompt_root.join("subagent-roles/reviewer/SKILL.md"),
            "reviewer",
            "Bundled reviewer",
        );
        write_skill(
            &prompt_root.join("workflows/review/SKILL.md"),
            "review",
            "Bundled review workflow",
        );
        let marker = config_root.join(BOOTSTRAP_MARKER);
        let marker_bytes = b"operator-owned marker";
        fs::write(&marker, marker_bytes).expect("marker");
        #[cfg(unix)]
        let marker_mode = file_mode(&marker);

        bootstrap_catalog(&prompt_root, &config_root).expect("existing marker skips bootstrap");

        assert_eq!(fs::read(&marker).expect("marker"), marker_bytes);
        #[cfg(unix)]
        assert_eq!(file_mode(&marker), marker_mode);
        assert!(!config_root
            .join("subagent-roles/reviewer/SKILL.md")
            .exists());
        assert!(!config_root.join("workflows/review/SKILL.md").exists());

        fs::remove_dir_all(prompt_root).ok();
        fs::remove_dir_all(config_root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn bootstrap_rejects_preexisting_symlinked_components() {
        for component in ["config-root", "catalog", "asset"] {
            let prompt_root = make_temp_dir(&format!("{component}-symlink-source"));
            let config_home = make_temp_dir(&format!("{component}-symlink-config-home"));
            let config_root = config_home.join("pi-relay");
            let outside = make_temp_dir(&format!("{component}-symlink-outside"));
            write_catalog_fixture(&prompt_root);

            match component {
                "config-root" => {
                    symlink(&outside, &config_root).expect("config-root symlink");
                }
                "catalog" => {
                    fs::create_dir_all(&config_root).expect("config root");
                    symlink(&outside, config_root.join("subagent-roles")).expect("catalog symlink");
                }
                "asset" => {
                    fs::create_dir_all(config_root.join("subagent-roles")).expect("catalog root");
                    symlink(&outside, config_root.join("subagent-roles/reviewer"))
                        .expect("asset symlink");
                }
                _ => unreachable!("test components are fixed"),
            }

            let error = bootstrap_catalog(&prompt_root, &config_root)
                .expect_err("symlinked bootstrap component is rejected");
            assert!(
                format!("{error:#}").contains("refusing symlinked bootstrap directory"),
                "{error:#}"
            );
            assert!(!outside.join("SKILL.md").exists());
            assert!(!outside.join(BOOTSTRAP_MARKER).exists());

            fs::remove_dir_all(prompt_root).ok();
            fs::remove_dir_all(config_home).ok();
            fs::remove_dir_all(outside).ok();
        }
    }

    #[cfg(unix)]
    #[test]
    fn bootstrap_rejects_symlinked_configuration_home_component() {
        let temp = make_temp_dir("config-home-symlink-parent");
        let prompt_root = make_temp_dir("config-home-symlink-source");
        let outside = make_temp_dir("config-home-symlink-outside");
        let config_root = temp.join("missing/xdg/pi-relay");
        write_catalog_fixture(&prompt_root);
        symlink(&outside, temp.join("missing")).expect("configuration-home symlink");

        let error = bootstrap_catalog(&prompt_root, &config_root)
            .expect_err("symlinked configuration-home component is rejected");
        assert!(
            format!("{error:#}").contains("refusing symlinked bootstrap directory"),
            "{error:#}"
        );
        assert!(!outside
            .join("xdg/pi-relay/subagent-roles/reviewer/SKILL.md")
            .exists());
        assert!(!outside
            .join("xdg/pi-relay/workflows/review/SKILL.md")
            .exists());
        assert!(!outside.join("xdg/pi-relay").join(BOOTSTRAP_MARKER).exists());

        fs::remove_dir_all(temp).ok();
        fs::remove_dir_all(prompt_root).ok();
        fs::remove_dir_all(outside).ok();
    }

    #[cfg(unix)]
    #[test]
    fn bootstrap_rejects_preexisting_symlinked_leaves() {
        for leaf in [BOOTSTRAP_MARKER, "SKILL.md"] {
            let prompt_root = make_temp_dir(&format!("{leaf}-symlink-source"));
            let config_root = make_temp_dir(&format!("{leaf}-symlink-config"));
            let outside = make_temp_dir(&format!("{leaf}-symlink-outside"));
            write_catalog_fixture(&prompt_root);

            let link_path = if leaf == BOOTSTRAP_MARKER {
                config_root.join(BOOTSTRAP_MARKER)
            } else {
                config_root.join("subagent-roles/reviewer/SKILL.md")
            };
            fs::create_dir_all(link_path.parent().expect("leaf parent")).expect("leaf parent");
            symlink(outside.join("target"), &link_path).expect("bootstrap leaf symlink");

            let error =
                bootstrap_catalog(&prompt_root, &config_root).expect_err("symlinked leaf rejected");
            assert!(
                format!("{error:#}").contains("refusing symlinked bootstrap file"),
                "{error:#}"
            );
            assert!(!outside.join("target").exists());

            fs::remove_dir_all(prompt_root).ok();
            fs::remove_dir_all(config_root).ok();
            fs::remove_dir_all(outside).ok();
        }
    }

    #[cfg(unix)]
    #[test]
    fn bootstrap_keeps_writes_in_opened_config_root_after_path_swap() {
        let prompt_root = make_temp_dir("root-race-source");
        let config_home = make_temp_dir("root-race-config-home");
        let config_root = config_home.join("pi-relay");
        let retained_root = config_home.join("retained-pi-relay");
        let outside = make_temp_dir("root-race-outside");
        fs::create_dir_all(&config_root).expect("config root");
        write_catalog_fixture(&prompt_root);

        let mut swapped = false;
        bootstrap_catalog_with_hook(&prompt_root, &config_root, &mut |opened| {
            if !swapped && opened == BootstrapDirectory::ConfigRoot {
                fs::rename(&config_root, &retained_root).expect("rename opened config root");
                symlink(&outside, &config_root).expect("replace config root with symlink");
                swapped = true;
            }
        })
        .expect("bootstrap through retained config-root handle");

        assert!(!outside.join("subagent-roles/reviewer/SKILL.md").exists());
        assert!(!outside.join("workflows/review/SKILL.md").exists());
        assert!(!outside.join(BOOTSTRAP_MARKER).exists());
        assert!(retained_root
            .join("subagent-roles/reviewer/SKILL.md")
            .is_file());
        assert!(retained_root.join("workflows/review/SKILL.md").is_file());
        assert!(retained_root.join(BOOTSTRAP_MARKER).is_file());

        fs::remove_dir_all(prompt_root).ok();
        fs::remove_dir_all(config_home).ok();
        fs::remove_dir_all(outside).ok();
    }

    #[cfg(unix)]
    #[test]
    fn bootstrap_keeps_writes_in_opened_catalog_after_path_swap() {
        let prompt_root = make_temp_dir("catalog-race-source");
        let config_root = make_temp_dir("catalog-race-config");
        let catalog_root = config_root.join("subagent-roles");
        let retained_catalog = config_root.join("retained-subagent-roles");
        let outside = make_temp_dir("catalog-race-outside");
        write_catalog_fixture(&prompt_root);

        let mut swapped = false;
        bootstrap_catalog_with_hook(&prompt_root, &config_root, &mut |opened| {
            if !swapped && opened == BootstrapDirectory::Catalog {
                fs::rename(&catalog_root, &retained_catalog).expect("rename opened catalog");
                symlink(&outside, &catalog_root).expect("replace catalog with symlink");
                swapped = true;
            }
        })
        .expect("bootstrap through retained catalog handle");

        assert!(!outside.join("reviewer/SKILL.md").exists());
        assert!(!outside.join(BOOTSTRAP_MARKER).exists());
        assert!(retained_catalog.join("reviewer/SKILL.md").is_file());
        assert!(config_root.join("workflows/review/SKILL.md").is_file());
        assert!(config_root.join(BOOTSTRAP_MARKER).is_file());

        fs::remove_dir_all(prompt_root).ok();
        fs::remove_dir_all(config_root).ok();
        fs::remove_dir_all(outside).ok();
    }

    #[cfg(unix)]
    #[test]
    fn bootstrap_keeps_writes_in_opened_asset_after_path_swap() {
        let prompt_root = make_temp_dir("asset-race-source");
        let config_root = make_temp_dir("asset-race-config");
        let asset_dir = config_root.join("subagent-roles/reviewer");
        let retained_asset = config_root.join("subagent-roles/retained-reviewer");
        let outside = make_temp_dir("asset-race-outside");
        write_catalog_fixture(&prompt_root);

        let mut swapped = false;
        bootstrap_catalog_with_hook(&prompt_root, &config_root, &mut |opened| {
            if !swapped && opened == BootstrapDirectory::Asset {
                fs::rename(&asset_dir, &retained_asset).expect("rename opened asset directory");
                symlink(&outside, &asset_dir).expect("replace asset with symlink");
                swapped = true;
            }
        })
        .expect("bootstrap through retained asset handle");

        assert!(!outside.join("SKILL.md").exists());
        assert!(!outside.join(BOOTSTRAP_MARKER).exists());
        assert!(retained_asset.join("SKILL.md").is_file());
        assert!(config_root.join("workflows/review/SKILL.md").is_file());
        assert!(config_root.join(BOOTSTRAP_MARKER).is_file());

        fs::remove_dir_all(prompt_root).ok();
        fs::remove_dir_all(config_root).ok();
        fs::remove_dir_all(outside).ok();
    }

    #[test]
    fn failed_bootstrap_never_marks_catalog_complete() {
        let prompt_root = make_temp_dir("partial-bootstrap-source");
        let config_root = make_temp_dir("partial-bootstrap-config");
        write_skill(
            &prompt_root.join("subagent-roles/reviewer/SKILL.md"),
            "reviewer",
            "Bundled reviewer",
        );

        assert!(bootstrap_catalog(&prompt_root, &config_root).is_err());
        assert!(!config_root.join(BOOTSTRAP_MARKER).exists());
        assert!(config_root
            .join("subagent-roles/reviewer/SKILL.md")
            .is_file());

        write_skill(
            &prompt_root.join("workflows/review/SKILL.md"),
            "review",
            "Bundled workflow",
        );
        bootstrap_catalog(&prompt_root, &config_root).expect("repaired bootstrap");
        assert!(config_root.join(BOOTSTRAP_MARKER).is_file());
        assert!(config_root.join("workflows/review/SKILL.md").is_file());

        fs::remove_dir_all(prompt_root).ok();
        fs::remove_dir_all(config_root).ok();
    }

    #[test]
    fn asset_staging_failure_publishes_no_partial_skill_and_retry_completes() {
        let prompt_root = make_temp_dir("asset-staging-failure-source");
        let config_root = make_temp_dir("asset-staging-failure-config");
        write_catalog_fixture(&prompt_root);
        let role = config_root.join("subagent-roles/reviewer/SKILL.md");

        let mut no_open_hook = |_| {};
        let mut failed = false;
        let error = bootstrap_catalog_with_hooks(
            &prompt_root,
            &config_root,
            &mut no_open_hook,
            &mut |write| {
                if write == BootstrapWrite::Asset && !failed {
                    failed = true;
                    return Err(std::io::Error::other("injected asset staging failure"));
                }
                Ok(())
            },
        )
        .expect_err("asset staging failure");

        assert!(format!("{error:#}").contains("write bootstrap file"));
        assert!(!role.exists(), "failed staging must not publish SKILL.md");
        assert!(
            !config_root.join(BOOTSTRAP_MARKER).exists(),
            "a failed asset must not publish the completion marker"
        );

        bootstrap_catalog(&prompt_root, &config_root).expect("retry bootstrap");
        assert_eq!(
            fs::read(&role).expect("retry role"),
            fs::read(prompt_root.join("subagent-roles/reviewer/SKILL.md")).expect("source role")
        );
        assert_eq!(
            fs::read(config_root.join(BOOTSTRAP_MARKER)).expect("retry marker"),
            b"v1\n"
        );

        fs::remove_dir_all(prompt_root).ok();
        fs::remove_dir_all(config_root).ok();
    }

    #[test]
    fn marker_staging_failure_leaves_no_marker_and_retry_completes() {
        let prompt_root = make_temp_dir("marker-staging-failure-source");
        let config_root = make_temp_dir("marker-staging-failure-config");
        write_catalog_fixture(&prompt_root);
        let role = config_root.join("subagent-roles/reviewer/SKILL.md");
        let source_role = prompt_root.join("subagent-roles/reviewer/SKILL.md");

        let mut no_open_hook = |_| {};
        let mut failed = false;
        let error = bootstrap_catalog_with_hooks(
            &prompt_root,
            &config_root,
            &mut no_open_hook,
            &mut |write| {
                if write == BootstrapWrite::Marker && !failed {
                    failed = true;
                    return Err(std::io::Error::other("injected marker staging failure"));
                }
                Ok(())
            },
        )
        .expect_err("marker staging failure");

        assert!(format!("{error:#}").contains("write bootstrap file"));
        assert_eq!(
            fs::read(&role).expect("published asset"),
            fs::read(&source_role).expect("source role")
        );
        assert!(
            !config_root.join(BOOTSTRAP_MARKER).exists(),
            "a failed marker staging write must not mark bootstrap complete"
        );

        bootstrap_catalog(&prompt_root, &config_root).expect("retry bootstrap");
        assert_eq!(
            fs::read(config_root.join(BOOTSTRAP_MARKER)).expect("retry marker"),
            b"v1\n"
        );

        fs::remove_dir_all(prompt_root).ok();
        fs::remove_dir_all(config_root).ok();
    }

    #[test]
    fn user_skill_created_between_staging_and_publish_wins_unchanged() {
        let prompt_root = make_temp_dir("concurrent-user-skill-source");
        let config_root = make_temp_dir("concurrent-user-skill-config");
        write_catalog_fixture(&prompt_root);
        let role = config_root.join("subagent-roles/reviewer/SKILL.md");
        let user_bytes = b"user-created between staging and publication";

        let mut no_open_hook = |_| {};
        let mut user_created = false;
        bootstrap_catalog_with_hooks(
            &prompt_root,
            &config_root,
            &mut no_open_hook,
            &mut |write| {
                if write == BootstrapWrite::Asset && !user_created {
                    fs::write(&role, user_bytes).expect("concurrent user skill");
                    user_created = true;
                }
                Ok(())
            },
        )
        .expect("bootstrap preserves concurrent user skill");

        assert_eq!(fs::read(&role).expect("user skill"), user_bytes);
        assert_eq!(
            fs::read(config_root.join("workflows/review/SKILL.md")).expect("workflow"),
            fs::read(prompt_root.join("workflows/review/SKILL.md")).expect("source workflow")
        );
        assert_eq!(
            fs::read(config_root.join(BOOTSTRAP_MARKER)).expect("marker"),
            b"v1\n"
        );

        fs::remove_dir_all(prompt_root).ok();
        fs::remove_dir_all(config_root).ok();
    }

    fn config_from_values(root: &Path, args: Vec<String>) -> Result<Config> {
        Config::from_values(
            Some(root.as_os_str().to_os_string()),
            Some("/unused-home".into()),
            args,
        )
    }

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "pi-relay-config-{prefix}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp dir");
        path
    }

    fn write_skill(path: &Path, name: &str, description: &str) {
        fs::create_dir_all(path.parent().expect("skill parent")).expect("skill parent");
        fs::write(
            path,
            format!("---\nname: {name}\ndescription: {description}\n---\n\nBody.\n"),
        )
        .expect("skill");
    }

    fn write_catalog_fixture(prompt_root: &Path) {
        write_skill(
            &prompt_root.join("subagent-roles/reviewer/SKILL.md"),
            "reviewer",
            "Bundled reviewer",
        );
        write_skill(
            &prompt_root.join("workflows/review/SKILL.md"),
            "review",
            "Bundled workflow",
        );
    }

    fn assert_catalog_and_marker_created(config_root: &Path) {
        assert!(config_root
            .join("subagent-roles/reviewer/SKILL.md")
            .is_file());
        assert!(config_root.join("workflows/review/SKILL.md").is_file());
        assert!(config_root.join(BOOTSTRAP_MARKER).is_file());
    }

    #[cfg(unix)]
    fn file_mode(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o7777
    }
}
