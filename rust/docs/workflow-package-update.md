# Host-owned package updates

`pi-runtime` reads two package catalogs that this repository does not own: the
workflow skills reached through `LoadSkill`, and the subagent roles named by
`delegation.start_full` and `delegation.start_readonly_fanout`.

```text
${XDG_CONFIG_HOME:-$HOME/.config}/pi-relay/runtime/
├── skills/<package>/SKILL.md
└── subagent-roles/<role>/SKILL.md
```

Both catalog directories are symlinks into a git checkout of the operator's
agent-config repository, so the tracked tree *is* the live tree:

```text
skills         -> ~/agent-config/pi-relay/runtime/skills
subagent-roles -> ~/agent-config/pi-relay/runtime/subagent-roles
```

## Update procedure

Edit the packages in the checkout, then publish them:

```sh
git -C ~/agent-config add pi-relay/runtime
git -C ~/agent-config commit -m 'Update runtime packages'
git -C ~/agent-config push
```

On every other runtime host:

```sh
git -C ~/agent-config pull --ff-only
```

No daemon or runtime restart is required: the runtime re-reads both catalogs
from disk on every runtime-context read. A session started after the change
renders the updated skill and role index into its prompt; a session started
before it keeps the index it was rendered with, while `LoadSkill` still
resolves against the current files.

Tool descriptions and JSON schemas are rebuilt from the registry on every model
request, so they always match the deployed binaries rather than the session's
stored prompt.

## Symlink the catalog directory, never an individual package

Discovery runs through `collect_skill_dir`
(`rust/crates/agent-runtime/src/workspaces/mod.rs`), which skips every entry
that fails `entry.file_type().await?.is_dir()`. Rust's `DirEntry::file_type()`
does not follow symlinks, so a symlinked *package* directory reports `symlink`
rather than `dir` and is dropped silently — no error, no log line, the package
simply never appears in the catalog.

Linking the *catalog* directory works because `read_dir` resolves the final
component of the path it is given, and the packages inside the checkout are
real directories.

## Why host symlinks work here

`pi-runtime` is deliberately not dockerized (see the note in
`infra/docker-compose.yml`): it needs full host filesystem and toolchain
access, runs as the host login user, and therefore resolves host symlinks
normally.

The control plane is containerized, and bind mounts do not follow host
symlinks, so `$XDG_CONFIG_HOME/pi-relay/agentd/config.toml` must stay a real
file. Machine-specific and credential-bearing runtime files stay untracked and
outside the checkout for the same reason they are not shareable:
`runtime/config.toml` carries host paths and listen addresses, and
`runtime/mcp.toml` carries MCP server URLs and OAuth client ids.

## Verification

Confirm the links point into the checkout and that every package is reachable
through them:

```sh
runtime="${XDG_CONFIG_HOME:-$HOME/.config}/pi-relay/runtime"
for catalog in skills subagent-roles; do
  test -L "$runtime/$catalog"
  test "$(readlink -f "$runtime/$catalog")" = "$HOME/agent-config/pi-relay/runtime/$catalog"
  for package in "$runtime/$catalog"/*/; do
    package="${package%/}"
    # A symlinked package would be skipped silently; it must be a real directory.
    test ! -L "$package"
    test -s "$package/SKILL.md"
    echo "$package"
  done
done
```

Then sanity-check the workflow packages against the contract they must express.
Each workflow keys its state by delegation ID rather than a singleton current
delegation, may launch one writer alongside independent read-only research and
further read-only rounds, and never polls; implementation → review → test gates
stay sequential, and the Kubernetes and MCP workflows keep treating read-only
agents as capable of remote side effects.

```sh
for workflow in "$runtime"/skills/workflow-*/SKILL.md; do
  sha256sum "$workflow"
  grep -Fq 'delegation ID' "$workflow"
  ! grep -Fq 'one delegation per turn' "$workflow"
done
```

Finally start a disposable parent session and smoke-test one full launch beside
one read-only fan-out in the same response. Confirm that the intended roles and
workflows resolve, that delegation IDs are distinct, and that completion
wakeups arrive without polling.
