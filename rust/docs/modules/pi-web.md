# pi-web

`pi-web` is the loopback-first host web/control service. It serves the built
React application from `packages/web/dist` (including SPA fallback) and owns
the read-only endpoint:

```text
GET /api/sessions/{session_id}/git?limit=12
```

The endpoint accepts no filesystem paths. It resolves the durable session
outer directory and workspace list through `agent-store`, then independently
validates the live filesystem before invoking `git` or, best-effort, `gh`.
One workspace failure is represented on that workspace and does not fail the
whole response. History is newest-first and bounded to 100 commits.

## Trust and binding

The service defaults to `127.0.0.1:8788`. Its HTTP API has no application
login because the deployment trust boundary is the local host plus Tailscale:

- local access is loopback;
- remote access is expected to enter through authenticated `tailscale serve`;
- `infra/serve.sh` routes `/ws` directly to loopback `pi-agentd` and `/`
  (including `/api`) to loopback `pi-web`;
- CORS is intentionally absent and the browser calls `/api` on its own origin.

`pi-web` validates the HTTP `Host` header. Add the Tailscale DNS name through
`PI_WEB_ALLOWED_HOSTS` (comma-separated) or repeated `--allowed-host` options.
Entries use the same strict authority parser as requests: a hostname or
bracketed IPv6 literal, optionally followed by a numeric port. Matching ignores
the port, ASCII hostname case, and one trailing DNS dot; malformed authorities
make startup fail.
A non-loopback bind is rejected unless
`PI_WEB_ALLOW_NON_LOOPBACK=1`/`--allow-non-loopback` is explicitly supplied;
only use that override behind an equivalent trusted access layer.

## Configuration

```text
DATABASE_URL                 required PostgreSQL connection string
PI_WEB_BIND                  default 127.0.0.1:8788
PI_WEB_DIST_DIR              default packages/web/dist
PI_WEB_ALLOWED_HOSTS         comma-separated additional Host names
PI_WEB_ALLOW_NON_LOOPBACK    explicit unsafe-bind acknowledgement
PI_WEB_GIT_BIN               absolute trusted git executable override
PI_WEB_GH_BIN                absolute trusted optional gh executable override
```

Equivalent command-line options are shown by `pi-web --help`. `GET /healthz`
is available for process readiness. SIGINT and SIGTERM trigger graceful HTTP
shutdown and close the database pool.

At startup, `pi-web` canonicalizes `PI_WEB_GIT_BIN`/`--git-bin` and
`PI_WEB_GH_BIN`/`--gh-bin`. Without an override it searches the startup
`PATH` once, skipping candidates inside pi-web's current source/workspace
directory. The operator is responsible for treating that startup `PATH`, or
the parent directories of explicit overrides, as trusted executable input.
This supports Nix store paths as well as conventional Unix and Windows paths;
there is no `/usr` allowlist. Request handling never searches `PATH` again.
An executable later found inside a session `outer_cwd` disables inspection for
that session. Child `PATH` contains only the trusted resolved executable
directories, with resolved Git first, so `gh` cannot discover a
repository-controlled Git binary. Missing Git makes a workspace unavailable;
missing `gh` only makes PR lookup unavailable.

`pi-agentd` is the sole schema migration owner. Start it first and wait until
its listener is ready before starting `pi-web`; the web service only verifies
that its session projection is readable and never runs migrations. Its database
role therefore needs only connection plus `SELECT` access to the `sessions`
table. `infra/dev.sh` and `infra/dev-web.sh` enforce this startup ordering.

## Repository inspection policy

Only an expected direct child of the session `outer_cwd` is read. Every
`outer_cwd`, workspace, `.git`, objects, refs, directory, and file component is
opened through stable capability-relative no-follow handles. Git pointer files,
linked worktrees, common-directory/object-alternate files, symlinked metadata,
config includes, per-worktree config, and every repository configuration entry
outside a small inert allowlist are rejected.

Git never receives a live workspace path. Each inspection copies the opened
metadata capability into a private service-owned temporary snapshot (up to
20,000 entries, 1 GiB, and 64 levels), using block reflinks when the platform
and filesystem support them and a bounded copy otherwise. Git runs only
against that immutable snapshot, which is removed after inspection. Renaming
or replacing the live workspace, `.git`, objects, refs, or config after it was
opened therefore cannot redirect Git outside the original capability.
Capability/no-follow support comes from `cap-std` on conventional Unix and
Windows; a platform that cannot provide the operation fails inspection closed.
This intentionally trades linked worktrees, alternates, repositories larger
than the stated bound, and unusual repository config for an auditable
confinement policy.

The frontend bundle is similarly loaded once through no-follow handles into an
immutable in-memory asset map (up to 10,000 entries, 256 MiB, and 32 levels).
Any startup symlink fails startup; source-tree mutation after startup cannot
change or escape the served assets. SPA, asset MIME, and missing-asset behavior
operate only on that staged map.

Subprocesses receive argument vectors without a shell, a scrubbed environment,
explicit no-signature/no-textconv/no-ext-diff/no-pager/no-replacement options,
short timeouts, bounded output capture, and process-tree kill/reap handling.
Workspace inspection concurrency and serialized fields are bounded. Remote
credentials and URL query/fragment data are removed before serialization.
Unix children run in a process group; remaining members are killed after every
direct-child result, including success. A deliberately executed helper could
escape with `setsid(2)`, which is why only startup-resolved trusted executables
can run and repository configuration cannot select helpers. Windows uses a Job
Object and terminates the complete job.

PR lookup is conservative: `gh` results must match both the currently checked
out branch name and current HEAD object ID, and only open PRs are requested.
The durable session
`remote_branch` is not substituted for an internal or later-renamed branch,
and zero or ambiguous exact matches return no PR rather than a potentially
stale association.
