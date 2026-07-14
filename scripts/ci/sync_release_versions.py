#!/usr/bin/env python3
"""Sync version references that release-plz does not manage into the release PR.

Run from CI after ``release-plz release-pr`` reports that it created a PR. The
following environment variables are expected:

    PR_NUMBER   the release PR to amend
    APP_SLUG    slug of the GitHub App used to author the commit
    GH_TOKEN    token the ``gh`` CLI authenticates with
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path

# .github/scripts/sync_release_versions.py -> repository root.
REPO_ROOT = Path(__file__).resolve().parents[2]


def run(
    *args: str,
    check: bool = True,
    capture: bool = False,
    env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    """Run a command from the repository root."""
    return subprocess.run(
        args,
        cwd=REPO_ROOT,
        text=True,
        check=check,
        capture_output=capture,
        env=env,
    )


def crate_version() -> str:
    """Return the version release-plz just wrote to the root Cargo.toml."""
    text = (REPO_ROOT / "Cargo.toml").read_text()
    match = re.search(r'(?m)^version = "([^"]+)"', text)
    if not match:
        raise SystemExit("could not find the package version in Cargo.toml")
    return match.group(1)


def sync_readme(version: str) -> None:
    """Pin the README install snippet to the released major.minor version."""
    major_minor = ".".join(version.split(".")[:2])
    readme = REPO_ROOT / "README.md"
    updated = re.sub(
        r'(?m)^x39-sacn = "[^"]*"',
        f'x39-sacn = "{major_minor}"',
        readme.read_text(),
    )
    readme.write_text(updated)


def sync_embassy_lockfile(version: str) -> None:
    """Relock the nested examples/embassy workspace against the new version."""
    run(
        "cargo",
        "update",
        "--manifest-path",
        "examples/embassy/Cargo.toml",
        "--package",
        "x39-sacn",
    )


SYNC_STEPS = (sync_readme, sync_embassy_lockfile)
SYNCED_PATHS = ("README.md", "examples/embassy/Cargo.lock")


def bot_identity(app_slug: str) -> tuple[str, str]:
    """Resolve the GitHub App's bot name and no-reply email for the commit."""
    result = run("gh", "api", f"/users/{app_slug}[bot]", "--jq", ".id", capture=True)
    user_id = result.stdout.strip()
    name = f"{app_slug}[bot]"
    email = f"{user_id}+{app_slug}[bot]@users.noreply.github.com"
    return name, email


def working_tree_dirty() -> bool:
    return run("git", "diff", "--quiet", check=False).returncode != 0


def main() -> int:
    pr_number = os.environ["PR_NUMBER"]
    app_slug = os.environ["APP_SLUG"]

    run("gh", "pr", "checkout", pr_number)

    version = crate_version()
    for step in SYNC_STEPS:
        step(version)

    if not working_tree_dirty():
        print("README and embassy lockfile are already in sync.")
        return 0

    name, email = bot_identity(app_slug)
    commit_env = {
        **os.environ,
        "GIT_AUTHOR_NAME": name,
        "GIT_AUTHOR_EMAIL": email,
        "GIT_COMMITTER_NAME": name,
        "GIT_COMMITTER_EMAIL": email,
    }
    run("git", "add", *SYNCED_PATHS)
    run(
        "git",
        "commit",
        "-m",
        f"chore: update repo files for v{version}",
        env=commit_env,
    )
    run("git", "push")
    return 0


if __name__ == "__main__":
    sys.exit(main())
