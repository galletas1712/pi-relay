#!/usr/bin/env bash
# One-time migration of runtime-owned instructions, workflows, roles, and skills.
# Stop pi-agentd and pi-runtime before running this script.
set -euo pipefail

exec python3 - "$@" <<'PY'
import filecmp
import os
from pathlib import Path
import shutil
import sys


if sys.argv[1:] != ["--apply"]:
    print(f"usage: {sys.argv[0]} --apply", file=sys.stderr)
    print("stop pi-agentd and pi-runtime before applying this migration", file=sys.stderr)
    raise SystemExit(2)

home = Path.home()
config_home = Path(os.environ.get("XDG_CONFIG_HOME") or home / ".config")
product_root = config_home / "pi-relay"
agentd_root = product_root / "agentd"
runtime_root = product_root / "runtime"
source_root = Path(os.environ.get("PI_AGENT_CONFIG_SOURCE") or home / "agent-config")

for path in (home, config_home, product_root, runtime_root, source_root):
    if not path.is_absolute():
        raise SystemExit(f"path must be absolute: {path}")
for path in (product_root, agentd_root, runtime_root, source_root):
    if path.is_symlink():
        raise SystemExit(f"migration refuses symlink root: {path}")


def reject_symlinks(path: Path) -> None:
    if path.is_symlink():
        raise SystemExit(f"migration refuses symlink: {path}")
    if path.is_dir():
        for child in path.iterdir():
            reject_symlinks(child)


def files_equal(left: Path, right: Path) -> bool:
    if left.is_symlink() or right.is_symlink():
        raise SystemExit(f"migration refuses symlink comparison: {left} -> {right}")
    if left.is_dir():
        return right.is_dir() and all(
            files_equal(child, right / child.name) for child in left.iterdir()
        ) and all((left / child.name).exists() for child in right.iterdir())
    return right.is_file() and filecmp.cmp(left, right, shallow=False)


def copy_exact(source: Path, destination: Path) -> None:
    if not source.exists():
        return
    reject_symlinks(source)
    if destination.is_symlink():
        raise SystemExit(f"migration refuses symlink: {destination}")
    if destination.exists():
        reject_symlinks(destination)
        if not files_equal(source, destination):
            raise SystemExit(f"destination conflicts with source: {destination}")
        return
    destination.parent.mkdir(parents=True, exist_ok=True)
    if source.is_dir():
        shutil.copytree(source, destination)
    else:
        shutil.copy2(source, destination)


def copy_packages(source: Path, destination: Path) -> None:
    if source.is_symlink() or destination.is_symlink():
        raise SystemExit(f"migration refuses symlink catalog: {source} -> {destination}")
    if not source.is_dir():
        return
    for package in sorted(source.iterdir()):
        if package.name.startswith(".") or not package.is_dir():
            continue
        if not (package / "SKILL.md").is_file():
            continue
        copy_exact(package, destination / package.name)


def migrate_packages(source: Path, destination: Path) -> None:
    copy_packages(source, destination)
    if source.is_dir():
        for package in sorted(source.iterdir()):
            if (
                not package.name.startswith(".")
                and package.is_dir()
                and (package / "SKILL.md").is_file()
            ):
                if files_equal(package, destination / package.name):
                    shutil.rmtree(package)
        if not any(source.iterdir()):
            source.rmdir()


if source_root.exists():
    reject_symlinks(source_root)
runtime_root.mkdir(parents=True, exist_ok=True)
migrate_packages(agentd_root / "workflows", runtime_root / "skills")
migrate_packages(agentd_root / "subagent-roles", runtime_root / "subagent-roles")

if source_root.is_dir():
    copy_exact(source_root / "AGENTS.md", runtime_root / "AGENTS.md")
    copy_packages(source_root / "skills", home / ".agents/skills")
    projects = source_root / "projects"
    if projects.is_dir():
        for workspace in sorted(projects.iterdir()):
            if workspace.is_dir():
                packages = workspace / "skills"
                if not packages.is_dir():
                    packages = workspace
                copy_packages(
                    packages,
                    runtime_root / "projects" / workspace.name / "skills",
                )

print(f"runtime configuration: {runtime_root}")
print(f"global skills: {home / '.agents/skills'}")
print("runtime-owned catalogs migrated successfully")
PY
