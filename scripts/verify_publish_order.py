#!/usr/bin/env python3
import argparse
import json
import subprocess


def workspace_packages() -> dict[str, dict]:
    output = subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        text=True,
    )
    metadata = json.loads(output)
    members = set(metadata["workspace_members"])
    return {
        package["name"]: package
        for package in metadata["packages"]
        if package["id"] in members
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("packages", nargs="+")
    args = parser.parse_args()

    positions = {name: index for index, name in enumerate(args.packages)}
    if len(positions) != len(args.packages):
        raise SystemExit("publish order contains duplicate package names")

    packages = workspace_packages()
    unknown = [name for name in args.packages if name not in packages]
    if unknown:
        raise SystemExit(f"publish order contains unknown workspace packages: {unknown}")

    errors = []
    for package_name in args.packages:
        package = packages[package_name]
        for dependency in package["dependencies"]:
            dependency_name = dependency["name"]
            if dependency_name not in positions:
                continue
            if positions[dependency_name] > positions[package_name]:
                kind = dependency.get("kind") or "normal"
                errors.append(
                    f"{package_name} has a {kind} dependency on {dependency_name}, "
                    f"so {dependency_name} must be published first"
                )

    if errors:
        raise SystemExit("\n".join(errors))
    print("crate publish order is valid")


if __name__ == "__main__":
    main()
