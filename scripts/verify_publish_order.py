#!/usr/bin/env python3
import argparse
import json
import subprocess

# Only these dependency kinds constrain the crate publish order. Dev
# dependencies are not part of the published package, so a crate may depend on
# a crate that appears only in its dev-dependencies even when it is published
# first (pd-host-function dev-depends on pd-vm).
ORDERING_DEPENDENCY_KINDS = frozenset({"normal", "build"})


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


def dependency_kind(dependency: dict) -> str:
    # cargo metadata reports normal dependencies with a null kind.
    return dependency.get("kind") or "normal"


def publish_order_errors(packages: dict[str, dict], order: list[str]) -> list[str]:
    positions: dict[str, int] = {}
    for index, name in enumerate(order):
        if name in positions:
            return [f"publish order contains duplicate package name: {name}"]
        positions[name] = index

    unknown = [name for name in order if name not in packages]
    if unknown:
        return [f"publish order contains unknown workspace packages: {unknown}"]

    errors = []
    for package_name in order:
        package = packages[package_name]
        for dependency in package.get("dependencies", []):
            kind = dependency_kind(dependency)
            if kind not in ORDERING_DEPENDENCY_KINDS:
                continue
            dependency_name = dependency["name"]
            if dependency_name not in positions:
                continue
            if positions[dependency_name] > positions[package_name]:
                errors.append(
                    f"{package_name} has a {kind} dependency on {dependency_name}, "
                    f"so {dependency_name} must be published first"
                )
    return errors


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("packages", nargs="+")
    args = parser.parse_args()

    errors = publish_order_errors(workspace_packages(), args.packages)
    if errors:
        raise SystemExit("\n".join(errors))
    print("crate publish order is valid")


if __name__ == "__main__":
    main()
