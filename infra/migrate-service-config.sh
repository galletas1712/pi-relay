#!/usr/bin/env bash
# One-time migration from the flat pi-relay config root to agentd/runtime
# subtrees. Stop pi-agentd and pi-runtime before running this script.
set -euo pipefail

exec python3 - "$@" <<'PY'
import filecmp
import json
import os
from pathlib import Path
import re
import shutil
import stat
import sys
import tempfile
import tomllib


if sys.argv[1:] != ["--apply"]:
    print(f"usage: {sys.argv[0]} --apply", file=sys.stderr)
    print("stop pi-agentd and pi-runtime before applying this migration", file=sys.stderr)
    raise SystemExit(2)

config_home = Path(os.environ.get("XDG_CONFIG_HOME") or Path.home() / ".config")
state_home = Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local/state")
workspace_root = Path(os.environ.get("PI_RUNTIME_ROOT") or state_home / "pi-relay")
for path in (config_home, state_home, workspace_root):
    if not path.is_absolute():
        raise SystemExit(f"configuration, state, and workspace paths must be absolute: {path}")

product_root = config_home / "pi-relay"
agentd_root = product_root / "agentd"
runtime_root = product_root / "runtime"
if product_root.is_symlink():
    raise SystemExit(f"configuration root must not be a symlink: {product_root}")
if not (product_root / "config.toml").is_file() and not (
    product_root / "agentd/config.toml"
).is_file():
    raise SystemExit(
        f"missing source configuration: {product_root / 'config.toml'} "
        f"or {product_root / 'agentd/config.toml'}"
    )


def reject_symlinks(path: Path) -> None:
    if path.is_symlink():
        raise SystemExit(f"configuration migration refuses symlink: {path}")
    if path.is_dir():
        for child in path.iterdir():
            reject_symlinks(child)


def source_is_subset(source: Path, destination: Path) -> bool:
    if source.is_dir():
        return destination.is_dir() and all(
            source_is_subset(child, destination / child.name) for child in source.iterdir()
        )
    return destination.is_file() and filecmp.cmp(source, destination, shallow=False)


def merge_existing(source: Path, destination: Path) -> None:
    if not source.exists():
        return
    reject_symlinks(source)
    if not destination.exists():
        source.rename(destination)
        return
    reject_symlinks(destination)
    if not source_is_subset(source, destination):
        raise SystemExit(f"destination conflicts with source configuration: {destination}")
    if source.is_dir():
        shutil.rmtree(source)
    else:
        source.unlink()


def toml_key(value: str) -> str:
    return value if re.fullmatch(r"[A-Za-z0-9_-]+", value) else json.dumps(value)


def toml_value(value) -> str:
    if isinstance(value, str):
        return json.dumps(value)
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(item) for item in value) + "]"
    if isinstance(value, dict):
        return "{ " + ", ".join(
            f"{toml_key(key)} = {toml_value(item)}" for key, item in value.items()
        ) + " }"
    raise SystemExit(f"unsupported role provider config value: {value!r}")


def normalized_provider(provider: dict) -> dict:
    allowed = {"kind", "model", "reasoning_effort", "max_tokens", "prompt_cache"}
    unknown = set(provider) - allowed
    if unknown:
        raise SystemExit(f"unknown role provider fields: {', '.join(sorted(unknown))}")
    if not isinstance(provider.get("kind"), str) or not isinstance(provider.get("model"), str):
        raise SystemExit("role provider requires string kind and model")
    if "prompt_cache" in provider:
        raise SystemExit("role prompt_cache cannot be represented in SKILL.md frontmatter")
    normalized = {"kind": provider["kind"], "model": provider["model"]}
    for key in ("reasoning_effort", "max_tokens"):
        if key in provider:
            normalized[key] = provider[key]
    return normalized


def inject_role_provider(skill_path: Path, provider: dict) -> None:
    provider = normalized_provider(provider)
    contents = skill_path.read_text()
    if not contents.startswith("---\n"):
        raise SystemExit(f"role skill has no frontmatter: {skill_path}")
    end = contents.find("\n---", 4)
    if end < 0:
        raise SystemExit(f"role skill has unterminated frontmatter: {skill_path}")
    frontmatter = contents[4:end]
    existing = {}
    for line in frontmatter.splitlines():
        if ":" not in line:
            continue
        key, value = line.split(":", 1)
        existing[key.strip()] = value.strip().strip("\"'")
    configured = {
        key: existing[key]
        for key in ("kind", "model", "reasoning_effort", "max_tokens")
        if key in existing
    }
    expected = {key: str(value) for key, value in provider.items()}
    if configured:
        if configured != expected:
            raise SystemExit(f"role frontmatter conflicts with migrated model policy: {skill_path}")
        return
    additions = "\n".join(
        f"{key}: {json.dumps(value) if isinstance(value, str) else value}"
        for key, value in provider.items()
    )
    updated = contents[:end] + "\n" + additions + contents[end:]
    mode = stat.S_IMODE(skill_path.stat().st_mode)
    with tempfile.NamedTemporaryFile(
        mode="w", dir=skill_path.parent, prefix=".skill-migration-", delete=False
    ) as temporary:
        temporary.write(updated)
        temporary_path = Path(temporary.name)
    temporary_path.chmod(mode)
    temporary_path.replace(skill_path)


def strip_subagent_model_tables(contents: str) -> str:
    output = []
    skipping = False
    found = False
    for line in contents.splitlines(keepends=True):
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            if re.match(r"^\[subagent_models(?:\.|\])", stripped):
                skipping = True
                found = True
                continue
            skipping = False
        if not skipping:
            output.append(line)
    if not found:
        return contents
    cleaned = "".join(output)
    if "subagent_models" in tomllib.loads(cleaned):
        raise SystemExit("could not remove subagent_models from daemon config")
    return cleaned


reject_symlinks(product_root)
agentd_root.mkdir(mode=0o700, exist_ok=True)
runtime_root.mkdir(mode=0o700, exist_ok=True)

merge_existing(product_root / "config.toml", agentd_root / "config.toml")
merge_existing(product_root / "subagent-roles", agentd_root / "subagent-roles")
merge_existing(product_root / "workflows", agentd_root / "workflows")
merge_existing(product_root / "mcp.toml", runtime_root / "mcp.toml")

agentd_config = agentd_root / "config.toml"
contents = agentd_config.read_text()
parsed = tomllib.loads(contents)
models = parsed.get("subagent_models", {})
if not isinstance(models, dict):
    raise SystemExit("subagent_models must be a table")
for role, provider in models.items():
    if not isinstance(role, str) or Path(role).name != role or role in {".", ".."}:
        raise SystemExit(f"subagent model key is not a global role name: {role}")
    role_dir = agentd_root / "subagent-roles" / role
    skill_path = role_dir / "SKILL.md"
    if not skill_path.is_file():
        raise SystemExit(f"subagent model has no matching role skill: {role}")
    inject_role_provider(skill_path, provider)

cleaned = strip_subagent_model_tables(contents)
if cleaned != contents:
    mode = stat.S_IMODE(agentd_config.stat().st_mode)
    with tempfile.NamedTemporaryFile(
        mode="w", dir=agentd_root, prefix=".config-migration-", delete=False
    ) as temporary:
        temporary.write(cleaned)
        temporary_path = Path(temporary.name)
    temporary_path.chmod(mode)
    temporary_path.replace(agentd_config)

runtime_config = runtime_root / "config.toml"
expected_runtime = {
    "runtime_id": os.environ.get("PI_RUNTIME_ID", "runtime-local"),
    "name": os.environ.get("PI_RUNTIME_NAME", "Local runtime"),
    "control_addr": os.environ.get("PI_RUNTIME_CONTROL_ADDR", "127.0.0.1:8786"),
    "workspace_root": str(workspace_root),
}
rendered_runtime = "".join(
    f"{key} = {toml_value(value)}\n" for key, value in expected_runtime.items()
)
if runtime_config.exists():
    if tomllib.loads(runtime_config.read_text()) != expected_runtime:
        raise SystemExit(f"runtime config conflicts with migration values: {runtime_config}")
else:
    runtime_config.write_text(rendered_runtime)
    runtime_config.chmod(0o600)

for generated in [product_root / ".bootstrap-v1", *product_root.glob(".bootstrap-staging-*")]:
    if generated.exists():
        if generated.is_symlink() or not generated.is_file():
            raise SystemExit(f"unexpected bootstrap artifact: {generated}")
        generated.unlink()

print(f"agentd configuration: {agentd_root}")
print(f"runtime configuration: {runtime_root}")
print(f"runtime workspace state remains at: {workspace_root}")
PY
