#!/usr/bin/env python3
import argparse
import json
import os
import re
import subprocess
import sys
import time
import tomllib
import urllib.error
import urllib.request
from pathlib import Path

PACKAGE_ORDER = ("pd-host-function", "pd-vm", "pd-vm-nostd", "rustscript")


def package_versions(release_version: str, host_version: str) -> dict[str, str]:
    return {
        "pd-host-function": host_version,
        "pd-vm": release_version,
        "pd-vm-nostd": release_version,
        "rustscript": release_version,
    }


def publish_plan(release_version: str, host_version: str) -> list[tuple[str, str]]:
    versions = package_versions(release_version, host_version)
    return [(package, versions[package]) for package in PACKAGE_ORDER]


def yank_command(package: str, version: str) -> list[str]:
    return ["cargo", "yank", "--vers", version, package]


def _rewrite_dependency_versions(text: str, versions: dict[str, str]) -> str:
    dependency_re = re.compile(r"^(\s*([A-Za-z0-9_-]+)\s*=\s*\{)(.*)(\}\s*)$")
    output = []
    for line in text.splitlines():
        match = dependency_re.match(line)
        if match and "path" in match.group(3):
            prefix, key, body, suffix = match.groups()
            package_match = re.search(r'package\s*=\s*"([^"]+)"', body)
            dependency = package_match.group(1) if package_match else key
            version = versions.get(dependency)
            if version:
                if re.search(r'version\s*=\s*"[^"]*"', body):
                    body = re.sub(
                        r'version\s*=\s*"[^"]*"',
                        f'version = "{version}"',
                        body,
                    )
                else:
                    body = body.rstrip()
                    if body and not body.endswith(","):
                        body += ","
                    body += f' version = "{version}" '
                line = prefix + body + suffix
        output.append(line)
    return "\n".join(output) + "\n"


def _rewrite_manifest(
    path: Path,
    release_version: str,
    host_version: str,
    dependency_versions: dict[str, str],
) -> None:
    text = path.read_text()
    manifest = tomllib.loads(text)
    package_name = manifest.get("package", {}).get("name")
    output = []
    section = ""
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            section = stripped
        if section == "[workspace.package]" and stripped.startswith("version = "):
            line = re.sub(
                r'version\s*=\s*"[^"]*"',
                f'version = "{release_version}"',
                line,
            )
        elif section == "[package]" and package_name == "pd-host-function":
            if stripped == "version.workspace = true":
                indent = line[: len(line) - len(line.lstrip())]
                line = f'{indent}version = "{host_version}"'
            elif stripped.startswith("version = "):
                line = re.sub(
                    r'version\s*=\s*"[^"]*"',
                    f'version = "{host_version}"',
                    line,
                )
        output.append(line)
    path.write_text(
        _rewrite_dependency_versions("\n".join(output) + "\n", dependency_versions)
    )


def rewrite_manifests(root: Path, release_version: str, host_version: str) -> None:
    dependency_versions = {
        "pd-host-function": host_version,
        "pd-vm": release_version,
    }
    for path in root.rglob("Cargo.toml"):
        if ".git" in path.parts or "target" in path.parts:
            continue
        _rewrite_manifest(path, release_version, host_version, dependency_versions)


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


def refresh_registry_lock(root: Path, package: str, version: str) -> None:
    subprocess.run(
        [sys.executable, "scripts/refresh_publish_lock.py", package, version],
        cwd=root,
        check=True,
    )


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
    plan = publish_plan(args.version, args.host_version)
    print(json.dumps({"publish_plan": plan}))
    if args.prepare_only:
        return
    if not os.environ.get("CARGO_REGISTRY_TOKEN"):
        raise SystemExit("CARGO_REGISTRY_TOKEN is required")

    if args.yank_host_version:
        subprocess.run(
            yank_command("pd-host-function", args.yank_host_version),
            cwd=root,
            check=True,
        )

    for package, version in plan:
        if crate_exists(package, version):
            print(f"{package} {version} already published; skipping", flush=True)
            continue
        if package == "pd-vm":
            refresh_registry_lock(root, "pd-host-function", args.host_version)
        publish_package(root, package, version)


if __name__ == "__main__":
    main()
