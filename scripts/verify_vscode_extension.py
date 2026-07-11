#!/usr/bin/env python3
import argparse
import json
import zipfile
from pathlib import Path


def load_json(path: Path):
    with path.open(encoding="utf-8") as handle:
        return json.load(handle)


def verify_manifest(manifest, grammar, label: str, expected_version: str) -> None:
    languages = manifest["contributes"]["languages"]
    language = next(item for item in languages if item.get("id") == "rustscript")
    if ".rss" not in language.get("extensions", []):
        raise SystemExit(f"{label}: RustScript does not own the .rss extension")
    if "rss" not in language.get("aliases", []):
        raise SystemExit(f"{label}: RustScript is missing the legacy rss alias")

    contributions = manifest["contributes"]["grammars"]
    rustscript = next(
        item for item in contributions if item.get("language") == "rustscript"
    )
    manifest_scope = rustscript["scopeName"]
    grammar_scope = grammar["scopeName"]
    if manifest_scope != grammar_scope:
        raise SystemExit(
            f"{label}: manifest scope {manifest_scope!r} does not match "
            f"grammar scope {grammar_scope!r}"
        )
    if manifest_scope != "source.rss":
        raise SystemExit(f"{label}: unexpected RustScript scope {manifest_scope!r}")

    if manifest["version"] != expected_version:
        raise SystemExit(
            f"{label}: version {manifest['version']!r} does not match "
            f"source version {expected_version!r}"
        )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--vsix", type=Path)
    args = parser.parse_args()

    root = Path(__file__).resolve().parents[1]
    extension = root / "vscode-rustscript"
    manifest = load_json(extension / "package.json")
    grammar = load_json(extension / "syntaxes" / "rustscript.tmLanguage.json")
    source_version = manifest["version"]
    verify_manifest(manifest, grammar, "source", source_version)

    if args.vsix:
        with zipfile.ZipFile(args.vsix) as archive:
            packaged_manifest = json.loads(
                archive.read("extension/package.json").decode("utf-8")
            )
            packaged_grammar = json.loads(
                archive.read(
                    "extension/syntaxes/rustscript.tmLanguage.json"
                ).decode("utf-8")
            )
        verify_manifest(packaged_manifest, packaged_grammar, "VSIX", source_version)

    print("VS Code grammar registration is valid")


if __name__ == "__main__":
    main()
