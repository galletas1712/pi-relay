# Workflow package publication

The workflow skills loaded by `LoadSkill` are host-owned runtime packages and
are not modified by this repository. Before using concurrent delegations,
update and republish these packages on every runtime host:

- `workflow-explore`
- `workflow-implement-review`
- `workflow-implement-review-test`
- `workflow-kubernetes-e2e`

Each workflow must key state by delegation ID rather than a singleton current
delegation. It may launch one writer and independent read-only research
together, and may launch later read-only rounds. No workflow may poll.
Implementation → review → test gates remain sequential. Kubernetes and MCP
workflows must continue treating read-only agents as capable of remote side
effects and keep their existing safety policy.

Existing sessions keep the persisted prompt they were rendered with, so the
upgrade runbook in [`../migrations/README.md`](../migrations/README.md) removes
the pre-concurrency single-delegation instructions from stored top-level
prompts. It does not regenerate whole prompts; it only guarantees that no
stored prompt contradicts the concurrent-capable tool schemas that are rebuilt
on every model request.

## Publication procedure

Do not edit shared host package directories from a development session. Stage
the revised packages in an operator-owned directory with one
`<workflow>/SKILL.md` subtree per package. Then run this explicit publication
step on each runtime host as the owning user:

```sh
stage="$HOME/pi-relay-workflow-stage"
runtime_config="${XDG_CONFIG_HOME:-$HOME/.config}/pi-relay/runtime"
for workflow in \
  workflow-explore \
  workflow-implement-review \
  workflow-implement-review-test \
  workflow-kubernetes-e2e
do
  install -d -m 0755 "$runtime_config/skills/$workflow"
  install -m 0644 \
    "$stage/$workflow/SKILL.md" \
    "$runtime_config/skills/$workflow/SKILL.md"
done
```

The installed destination is:

```text
$XDG_CONFIG_HOME/pi-relay/runtime/skills/<workflow>/SKILL.md
# or, when XDG_CONFIG_HOME is unset:
$HOME/.config/pi-relay/runtime/skills/<workflow>/SKILL.md
```

Before starting new concurrent sessions, verify every installed package and
reject the obsolete single-delegation wording:

```sh
runtime_config="${XDG_CONFIG_HOME:-$HOME/.config}/pi-relay/runtime"
for workflow in \
  workflow-explore \
  workflow-implement-review \
  workflow-implement-review-test \
  workflow-kubernetes-e2e
do
  file="$runtime_config/skills/$workflow/SKILL.md"
  test -s "$file"
  sha256sum "$file"
  ! grep -Fq 'one delegation per turn' "$file"
  grep -Fq 'delegation ID' "$file"
done
```

Finally start a disposable parent session and smoke-test one full launch beside
one read-only fan-out in the same response. Confirm distinct delegation IDs,
normal completion wakeups, and no polling instructions.
