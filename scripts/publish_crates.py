#!/usr/bin/env python3
import argparse
import json
import os
import re
import subprocess
import time
import tomllib
import urllib.error
import urllib.request
from pathlib import Path

# Every publishable workspace crate in dependency order. A crate must appear
# before every crate that declares a normal or build dependency on it.
PACKAGE_ORDER = ("pd-host-schema", "pd-host-function", "pd-vm", "pd-vm-nostd", "rustscript")

# The proc-macro crate tracks its own compatibility line instead of the release
# version, because published crates depend on the macro through `^0.22`.
HOST_PACKAGE = "pd-host-function"

SKIPPED_MANIFEST_DIRECTORIES = frozenset({".git", ".ci-tmp", "node_modules", "target"})

SECTION_NAME_RE = re.compile(r"^\[([^\]]+)\]$")
DEPENDENCY_ENTRY_RE = re.compile(r"^(\s*([A-Za-z0-9_-]+)\s*=\s*\{)(.*)(\}\s*)$")
VERSION_KEY_RE = re.compile(r'version\s*=\s*"([^"]*)"')
PACKAGE_KEY_RE = re.compile(r'package\s*=\s*"([^"]+)"')


def package_versions(release_version: str, host_version: str) -> dict[str, str]:
    """Version each publishable workspace crate ships with in this release.

    This map is the single source of truth for both `publish_plan` and the path
    dependency rewrite, so adding a workspace crate to `PACKAGE_ORDER` cannot
    leave a stale requirement behind in the publish tree.
    """
    versions = {package: release_version for package in PACKAGE_ORDER}
    versions[HOST_PACKAGE] = host_version
    return versions


def publish_plan(release_version: str, host_version: str) -> list[tuple[str, str]]:
    versions = package_versions(release_version, host_version)
    missing = set(PACKAGE_ORDER) - set(versions)
    if missing:
        raise SystemExit(f"publish order entries without a version: {sorted(missing)}")
    return [(package, versions[package]) for package in PACKAGE_ORDER]


def _compatibility_line(version: str) -> tuple[int, ...]:
    match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", version)
    if not match:
        raise ValueError(f"unsupported package version: {version}")
    major, minor, _patch = map(int, match.groups())
    return (major,) if major else (major, minor)


def compatible_registry_lock_versions(
    lock_path: Path,
    package: str,
    target_version: str,
) -> list[str]:
    if not lock_path.exists():
        return []
    target_line = _compatibility_line(target_version)
    lock = tomllib.loads(lock_path.read_text())
    versions = {
        entry["version"]
        for entry in lock.get("package", [])
        if entry.get("name") == package
        and str(entry.get("source", "")).startswith("registry+")
        and entry.get("version") != target_version
        and _compatibility_line(entry["version"]) == target_line
    }
    return sorted(versions, key=lambda version: tuple(map(int, version.split("."))))


def refresh_compatible_registry_lock(
    root: Path,
    package: str,
    target_version: str,
) -> None:
    """Move stale registry entries of `package` onto `target_version`.

    Cargo can only resolve the publish tree when every path dependency already
    points at its publish version; otherwise `cargo update` aborts with a
    version selection error before it reaches the registry entry.
    """
    lock_path = root / "Cargo.lock"
    for _attempt in range(16):
        stale_versions = compatible_registry_lock_versions(
            lock_path,
            package,
            target_version,
        )
        if not stale_versions:
            return
        process = subprocess.run(
            [
                "cargo",
                "update",
                "-p",
                f"{package}@{stale_versions[0]}",
                "--precise",
                target_version,
            ],
            cwd=root,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        print(process.stdout, flush=True)
        if process.returncode != 0:
            raise SystemExit(
                f"failed to update {package} {stale_versions[0]} to "
                f"{target_version} in the publish lock; every path dependency in "
                "the publish tree must already require its publish version"
            )
    raise SystemExit(f"publish lock did not converge on {package} {target_version}")


def yank_command(package: str, version: str) -> list[str]:
    return ["cargo", "yank", "--vers", version, package]


def dependency_section_kind(section: str) -> str | None:
    """Classify a manifest section as a normal, build or dev dependency table."""
    tail = section.rsplit(".", 1)[-1].strip()
    if tail == "dependencies":
        return "normal"
    if tail == "build-dependencies":
        return "build"
    if tail == "dev-dependencies":
        return "dev"
    return None


def dependency_name(key: str, body: str) -> str:
    package_match = PACKAGE_KEY_RE.search(body)
    return package_match.group(1) if package_match else key


def _with_version(body: str, version: str) -> str:
    if VERSION_KEY_RE.search(body):
        return VERSION_KEY_RE.sub(f'version = "{version}"', body, count=1)
    body = body.rstrip()
    if body and not body.endswith(","):
        body += ","
    return f'{body} version = "{version}" '


def _without_version(body: str) -> str:
    """Drop the version requirement so cargo strips the dep when packaging.

    A versioned path dev-dependency is kept in the published manifest and must
    exist on crates.io, which blocks publishing crates that come earlier in the
    order (pd-host-function dev-depends on pd-vm but is published first).
    """
    inner = body.strip()
    inner = re.sub(r'(?:^|,)\s*version\s*=\s*"[^"]*"', "", inner)
    inner = re.sub(r",\s*,", ",", inner).strip().strip(",").strip()
    return f" {inner} " if inner else " "


def _rewrite_dependency_versions(text: str, versions: dict[str, str]) -> str:
    output = []
    section = ""
    for line in text.splitlines():
        stripped = line.strip()
        header = SECTION_NAME_RE.match(stripped)
        if header:
            section = header.group(1)
        kind = dependency_section_kind(section)
        match = DEPENDENCY_ENTRY_RE.match(line) if kind else None
        if match and "path" in match.group(3):
            prefix, key, body, suffix = match.groups()
            version = versions.get(dependency_name(key, body))
            if version:
                if kind == "dev":
                    line = prefix + _without_version(body) + suffix
                else:
                    line = prefix + _with_version(body, version) + suffix
        output.append(line)
    return "\n".join(output) + "\n"


def _rewrite_manifest(
    path: Path,
    release_version: str,
    versions: dict[str, str],
) -> None:
    text = path.read_text()
    manifest = tomllib.loads(text)
    package = manifest.get("package", {})
    package_name = package.get("name")
    package_version = versions.get(package_name)
    output = []
    section = ""
    for line in text.splitlines():
        stripped = line.strip()
        header = SECTION_NAME_RE.match(stripped)
        if header:
            section = header.group(1)
        if section == "workspace.package" and stripped.startswith("version = "):
            line = VERSION_KEY_RE.sub(f'version = "{release_version}"', line, count=1)
        elif section == "package" and package_version and stripped.startswith(
            ("version = ", "version.workspace")
        ):
            indent = line[: len(line) - len(line.lstrip())]
            line = f'{indent}version = "{package_version}"'
        output.append(line)
    path.write_text(
        _rewrite_dependency_versions("\n".join(output) + "\n", versions)
    )


def manifest_paths(root: Path) -> list[Path]:
    return [
        path
        for path in sorted(root.rglob("Cargo.toml"))
        if not SKIPPED_MANIFEST_DIRECTORIES.intersection(path.relative_to(root).parts)
    ]


def stale_path_dependencies(root: Path, versions: dict[str, str]) -> list[str]:
    """Report path dependencies that do not match the publish version.

    Cargo aborts the publish tree with `failed to select a version` when a path
    dependency keeps an old requirement (`pd-host-schema = "^0.1.0"` while the
    crate itself is `0.31.2`), so this is checked before publishing.
    """
    problems = []
    for path in manifest_paths(root):
        section = ""
        for number, line in enumerate(path.read_text().splitlines(), start=1):
            header = SECTION_NAME_RE.match(line.strip())
            if header:
                section = header.group(1)
            kind = dependency_section_kind(section)
            match = DEPENDENCY_ENTRY_RE.match(line) if kind else None
            if not match or "path" not in match.group(3):
                continue
            dependency = dependency_name(match.group(2), match.group(3))
            expected = versions.get(dependency)
            if expected is None:
                continue
            requirement = VERSION_KEY_RE.search(match.group(3))
            if kind == "dev":
                if requirement:
                    problems.append(
                        f"{path}:{number}: dev-dependency {dependency} keeps "
                        f"version {requirement.group(1)!r}, which must be dropped"
                    )
            elif requirement is None:
                problems.append(
                    f"{path}:{number}: {dependency} has no version requirement, "
                    f"expected {expected!r}"
                )
            elif requirement.group(1) != expected:
                problems.append(
                    f"{path}:{number}: {dependency} requires "
                    f"{requirement.group(1)!r}, expected {expected!r}"
                )
    return problems


def rewrite_manifests(root: Path, release_version: str, host_version: str) -> None:
    versions = package_versions(release_version, host_version)
    for path in manifest_paths(root):
        _rewrite_manifest(path, release_version, versions)
    problems = stale_path_dependencies(root, versions)
    if problems:
        raise SystemExit(
            "publish tree still has path dependencies that cannot resolve:\n"
            + "\n".join(problems)
        )


def crate_exists(package: str, version: str) -> bool:
    request = urllib.request.Request(
        f"https://crates.io/api/v1/crates/{package}/{version}",
        headers={"User-Agent": "rustscript-publish-workflow"},
    )
    try:
        urllib.request.urlopen(request, timeout=20).read()
        return True
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return False
        raise


def publish_package(root: Path, package: str, version: str) -> None:
    for attempt in range(1, 19):
        print(f"publishing {package} {version}, attempt {attempt}", flush=True)
        process = subprocess.run(
            [
                "cargo",
                "publish",
                "-p",
                package,
                "--no-verify",
                "--allow-dirty",
            ],
            cwd=root,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        print(process.stdout, flush=True)
        if process.returncode == 0 or crate_exists(package, version):
            return
        if attempt == 18:
            raise SystemExit(process.returncode)
        time.sleep(20)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--host-version", default="0.22.7")
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("--yank-host-version")
    args = parser.parse_args()

    root = args.root.resolve()
    rewrite_manifests(root, args.version, args.host_version)
    refresh_compatible_registry_lock(root, HOST_PACKAGE, args.host_version)
    plan = publish_plan(args.version, args.host_version)
    print(json.dumps({"publish_plan": plan}))
    if args.prepare_only:
        return
    if not os.environ.get("CARGO_REGISTRY_TOKEN"):
        raise SystemExit("CARGO_REGISTRY_TOKEN is required")

    if args.yank_host_version:
        subprocess.run(
            yank_command(HOST_PACKAGE, args.yank_host_version),
            cwd=root,
            check=True,
        )

    for package, version in plan:
        if crate_exists(package, version):
            print(f"{package} {version} already published; skipping", flush=True)
            continue
        publish_package(root, package, version)


if __name__ == "__main__":
    main()
