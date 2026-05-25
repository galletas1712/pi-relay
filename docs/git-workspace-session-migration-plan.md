# Git workspace session migration plan

This plan migrates the current daemon state from overlay-backed host checkouts to
Git-only per-session checkouts.

The new code expects:

- project workspaces: `{ "workspace_dir", "remote_url", "remote_branch" }`
- session workspaces:
  `{ "workspace_dir", "remote_url", "remote_branch", "base_sha", "local_branch" }`
- no `mount_dir`, `source_path`, overlay manager, host checkout, daemon
  `--workspace`, or `fuse-overlayfs` dependency.

Until the database is migrated, the new daemon will fail to decode existing
project/session workspace JSON.

## Current inspected state

Database: `pi_relay` on `127.0.0.1:55432`.

- `projects`: 4 rows.
- `sessions`: 70 rows.
- All 4 projects still use old workspace JSON (`mount_dir`, `source_path`).
- All 70 sessions still use old workspace JSON.
- No sessions currently use the new shape.
- All queued inputs are consumed.
- At inspection time one `tool` action was `running` in the current session; on
  daemon restart it will be marked stale like other unfinished actions.

Projects:

| project | id | sessions | archived sessions | current old workspaces |
| --- | --- | ---: | ---: | --- |
| `pi-relay` | `c1233e78-b2b3-40be-8051-786045fddb96` | 46 | 30 | `/home/schwinns/pi-relay`, `/home/schwinns/codex` |
| `Dynamo` | `41bb9290-752c-498d-94c4-5ceb1c016714` | 22 | 5 | eight Dynamo-related repos |
| `anthropic-edit-test` | `b18695a6-d2ea-48f3-82fb-61d14a12fc09` | 1 | 0 | `/home/schwinns/pi-relay` |
| `anthropic-edit-test-2` | `05f015dc-a546-44da-b270-19281afe98eb` | 1 | 0 | `/home/schwinns/pi-relay` |

Overlay state:

- Session/project state dirs exist under
  `~/.local/state/pi-relay/sessions`.
- 17 `fuse.fuse-overlayfs` mounts were present under that state root.
- Most project prompt upper dirs are empty.
- Important non-empty uppers:
  - `session_25f2133b-92ba-4ea4-a555-fff04cc8f6a4/overlays/pi-relay/upper`
    has `.git`, `PI.md`, `infra`, `packages`, `rust`; about `1.2G`. This is
    active WIP and must be preserved before unmounting or deleting overlays.
  - `session_6df6394a-fe77-4518-a0bc-5afa953877b4/overlays/pi-relay/upper`
    has `.git`; about `8K`.

## Preconditions

1. Stop the current daemon and web UI.
2. Decide whether any old sessions need to remain functionally resumable. The
   simplest migration makes old sessions transcript-reference-only and creates
   fresh Git workspaces for new sessions.
3. Export or commit any WIP from overlay mounts before unmounting.
4. Back up both Postgres and state directories.

Suggested backups:

```bash
stamp="$(date +%Y%m%d%H%M%S)"

PGPASSWORD=postgres pg_dump -Fc \
  -h 127.0.0.1 -p 55432 -U postgres -d pi_relay \
  -f "$HOME/pi_relay_pre_git_workspaces_${stamp}.dump"

tar -C "$HOME/.local/state" \
  -cpf "$HOME/pi_relay_state_pre_git_workspaces_${stamp}.tar" \
  pi-relay
```

## Preserve overlay WIP

Before unmounting, inspect mounted overlays and save patches/tars for anything
that should survive.

For the current active `pi-relay` session:

```bash
session=session_25f2133b-92ba-4ea4-a555-fff04cc8f6a4
repo="$HOME/.local/state/pi-relay/sessions/$session/cwd/pi-relay"
out="$HOME/pi-relay-${session}-overlay-wip-$(date +%Y%m%d%H%M%S)"
mkdir -p "$out"

git -C "$repo" status --short > "$out/status.txt"
git -C "$repo" diff > "$out/unstaged.patch"
git -C "$repo" diff --cached > "$out/staged.patch"
git -C "$repo" ls-files --others --exclude-standard -z \
  | tar -C "$repo" --null -T - -cpf "$out/untracked.tar"
```

If the overlay has commits that are not on a remote, either push them to a
temporary migration branch or bundle them:

```bash
git -C "$repo" bundle create "$out/repo.bundle" --all
```

Repeat for any other non-empty overlay upper that matters.

## Choose project branch mappings

Project rows must be updated before new sessions can be created.

Current host checkouts suggest the following mappings, but the user should
choose the desired staging branch explicitly; do not blindly trust the dirty
host checkout branch.

| workspace_dir | remote_url | suggested remote_branch |
| --- | --- | --- |
| `pi-relay` | `https://github.com/galletas1712/pi-relay.git` | `pi-mono-rust-port` |
| `codex` | `https://github.com/galletas1712/codex` | `main` |
| `dynamo` | `https://github.com/ai-dynamo/dynamo.git` | `main` |
| `nixl` | `https://github.com/ai-dynamo/nixl` | `checkpoint-restore-ep` |
| `vllm` | `https://github.com/vllm-project/vllm.git` | `main` |
| `pytorch` | `https://github.com/pytorch/pytorch.git` | `main` |
| `nccl` | `https://gitlab-master.nvidia.com/nccl/nccl.git` | `master` |
| `TensorRT-LLM` | `https://github.com/NVIDIA/TensorRT-LLM.git` | `main` |
| `sglang` | `https://github.com/sgl-project/sglang.git` | `main` |
| `criu` | `https://github.com/galletas1712/criu.git` | `criu-dev` |

Verify each remote branch is fetchable from the daemon host:

```bash
git ls-remote --heads <remote_url> <remote_branch>
```

## Database migration

### Detect old rows

```sql
select count(*) from projects
where jsonb_path_exists(workspaces, '$[*].mount_dir');

select count(*) from sessions
where jsonb_path_exists(workspaces, '$[*].mount_dir');
```

Both counts must become zero before starting the new daemon.

### Update project rows

Example project updates:

```sql
update projects
set workspaces = '[
  {
    "workspace_dir": "pi-relay",
    "remote_url": "https://github.com/galletas1712/pi-relay.git",
    "remote_branch": "pi-mono-rust-port"
  },
  {
    "workspace_dir": "codex",
    "remote_url": "https://github.com/galletas1712/codex",
    "remote_branch": "main"
  }
]'::jsonb,
updated_at = now()
where id = 'c1233e78-b2b3-40be-8051-786045fddb96';

update projects
set workspaces = '[
  {
    "workspace_dir": "dynamo",
    "remote_url": "https://github.com/ai-dynamo/dynamo.git",
    "remote_branch": "main"
  },
  {
    "workspace_dir": "nixl",
    "remote_url": "https://github.com/ai-dynamo/nixl",
    "remote_branch": "checkpoint-restore-ep"
  },
  {
    "workspace_dir": "vllm",
    "remote_url": "https://github.com/vllm-project/vllm.git",
    "remote_branch": "main"
  },
  {
    "workspace_dir": "pytorch",
    "remote_url": "https://github.com/pytorch/pytorch.git",
    "remote_branch": "main"
  },
  {
    "workspace_dir": "nccl",
    "remote_url": "https://gitlab-master.nvidia.com/nccl/nccl.git",
    "remote_branch": "master"
  },
  {
    "workspace_dir": "TensorRT-LLM",
    "remote_url": "https://github.com/NVIDIA/TensorRT-LLM.git",
    "remote_branch": "main"
  },
  {
    "workspace_dir": "sglang",
    "remote_url": "https://github.com/sgl-project/sglang.git",
    "remote_branch": "main"
  },
  {
    "workspace_dir": "criu",
    "remote_url": "https://github.com/galletas1712/criu.git",
    "remote_branch": "criu-dev"
  }
]'::jsonb,
updated_at = now()
where id = '41bb9290-752c-498d-94c4-5ceb1c016714';
```

Hidden test projects can either be deleted if empty enough for policy, or
updated to the same `pi-relay` mapping.

### Migrate old sessions

All session rows must have either:

- `workspaces = '[]'::jsonb`, or
- valid new `SessionWorkspace` objects with `base_sha` and `local_branch`.

Recommended simple path:

1. Archive old overlay sessions.
2. Set their `workspaces` to `[]`.
3. Leave transcripts in the DB for reference.
4. Create new sessions after the project rows have been migrated.

```sql
update sessions
set metadata = jsonb_set(
      coalesce(metadata, '{}'::jsonb),
      '{archived}',
      'true'::jsonb,
      true
    ),
    workspaces = '[]'::jsonb,
    updated_at = now()
where jsonb_path_exists(workspaces, '$[*].mount_dir');
```

If a selected old session must remain functionally resumable:

1. Stop daemon and export overlay WIP for that session.
2. Move the old state root aside after unmounting, for example:
   `mv ~/.local/state/pi-relay/sessions/<session> ~/.local/state/pi-relay/sessions/<session>.overlay-backup`.
3. Recreate `~/.local/state/pi-relay/sessions/<session>/cwd/<workspace_dir>`.
4. For each workspace:
   - `git init`
   - `git remote add origin <remote_url>`
   - `git fetch origin +refs/heads/<remote_branch>:refs/remotes/origin/<remote_branch>`
   - `base_sha=$(git rev-parse refs/remotes/origin/<remote_branch>)`
   - `git switch -c pi/session/<session>/<workspace_dir> "$base_sha"`
   - `git branch --set-upstream-to origin/<remote_branch>`
5. Apply saved patches/untracked files or cherry-pick/pull the migration branch.
6. Update that session row with the new `outer_cwd` and new workspace JSON.

Old `mount_dir: "."` maps to the primary project workspace:

- pi-relay sessions: `workspace_dir = "pi-relay"`
- Dynamo sessions: `workspace_dir = "dynamo"`

The two-workspace session
`session_6df6394a-fe77-4518-a0bc-5afa953877b4` maps to `pi-relay` and
`codex`.

### Verify DB shape

```sql
select count(*) as bad_projects
from projects
where jsonb_path_exists(workspaces, '$[*].mount_dir')
   or not jsonb_path_exists(workspaces, '$[*].workspace_dir');

select count(*) as bad_sessions
from sessions
where workspaces <> '[]'::jsonb
  and (
    jsonb_path_exists(workspaces, '$[*].mount_dir')
    or not jsonb_path_exists(workspaces, '$[*].base_sha')
  );
```

Both counts should be zero.

## Unmount and remove overlay state

Only do this after backups and WIP export.

```bash
state="$HOME/.local/state/pi-relay/sessions"

awk -v root="$state" '$5 ~ "^"root && $9 ~ /fuse-overlayfs/ {print $5}' \
  /proc/self/mountinfo \
  | sort -r \
  | while read -r mountpoint; do
      fusermount3 -u "$mountpoint" || umount "$mountpoint"
    done

awk -v root="$state" '$5 ~ "^"root && $9 ~ /fuse-overlayfs/ {print $5}' \
  /proc/self/mountinfo
```

After verifying no `fuse-overlayfs` mounts remain and all WIP is preserved,
remove or archive old `overlays` directories and `project_prompt_*` state dirs.

`fuse-overlayfs`/FUSE packages can then be uninstalled if no other local tooling
needs them.

## Deploy new daemon

1. Update the service/launcher to remove `--workspace`.
2. Start the new daemon.
3. Validate:

```bash
# from a websocket client or UI:
project.list
session.list
system.prompt for a migrated project
session.start for each visible project
```

For each fresh project session, verify:

```bash
git -C ~/.local/state/pi-relay/sessions/<session>/cwd/<workspace_dir> status
git -C ~/.local/state/pi-relay/sessions/<session>/cwd/<workspace_dir> branch --show-current
git -C ~/.local/state/pi-relay/sessions/<session>/cwd/<workspace_dir> rev-parse --abbrev-ref --symbolic-full-name @{u}
```

Expected branch shape:

```text
pi/session/<session_id>/<workspace_dir>
```

No `fuse-overlayfs` mounts should remain under the pi-relay state directory.

## Rollback

1. Stop the new daemon.
2. Restore the Postgres dump.
3. Restore the old state tar.
4. Reinstall/use the old daemon and `fuse-overlayfs` if needed.
