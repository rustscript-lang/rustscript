#!/usr/bin/env python3
import argparse
import subprocess
import tomllib
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("package")
    parser.add_argument("version")
    parser.add_argument("--lockfile", type=Path, default=Path("Cargo.lock"))
    args = parser.parse_args()

    lock = tomllib.loads(args.lockfile.read_text())
    registry_versions = {
        package["version"]
        for package in lock.get("package", [])
        if package.get("name") == args.package
        and str(package.get("source", "")).startswith("registry+")
    }
    stale_versions = sorted(registry_versions - {args.version})
    if not stale_versions:
        print(f"registry lock for {args.package} already uses {args.version}")
        return

    for old_version in stale_versions:
        print(f"updating registry lock for {args.package}: {old_version} -> {args.version}")
        subprocess.run(
            [
                "cargo",
                "update",
                "-p",
                f"{args.package}@{old_version}",
                "--precise",
                args.version,
            ],
            check=True,
        )


if __name__ == "__main__":
    main()
