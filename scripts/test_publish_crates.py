#!/usr/bin/env python3
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import publish_crates


class PublishCratesTests(unittest.TestCase):
    def test_workflow_supports_unyank_and_explicit_recovery_publish_tags(self) -> None:
        workflow = (
            Path(__file__).parents[1] / ".github" / "workflows" / "publish-crates.yml"
        ).read_text()
        self.assertIn("'unyank-pd-host-function-*'", workflow)
        self.assertIn("cargo yank --undo --vers", workflow)
        self.assertIn("'publish-crates-*-host-*'", workflow)
        self.assertIn('publish_spec="${GITHUB_REF_NAME#publish-crates-}"', workflow)

    def test_compatible_registry_lock_versions_only_selects_same_compatibility_line(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            lock_path = Path(directory) / "Cargo.lock"
            lock_path.write_text(
                """version = 4

[[package]]
name = "pd-host-function"
version = "0.22.2"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "old"

[[package]]
name = "pd-host-function"
version = "0.21.9"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "older-line"

[[package]]
name = "pd-host-function"
version = "0.22.7"
"""
            )

            self.assertEqual(
                publish_crates.compatible_registry_lock_versions(
                    lock_path,
                    "pd-host-function",
                    "0.22.7",
                ),
                ["0.22.2"],
            )

    def test_package_versions_keep_host_macro_on_compatible_line(self) -> None:
        self.assertEqual(
            publish_crates.package_versions("0.23.1", "0.22.7"),
            {
                "pd-host-schema": "0.23.1",
                "pd-host-function": "0.22.7",
                "pd-vm": "0.23.1",
                "pd-vm-nostd": "0.23.1",
                "rustscript": "0.23.1",
            },
        )

    def test_rewrite_manifests_applies_release_and_dependency_versions(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "pd-host-function").mkdir()
            (root / "pd-vm-nostd").mkdir()
            (root / "crates" / "rustscript").mkdir(parents=True)
            (root / "Cargo.toml").write_text(
                """[workspace]\nmembers = [\".\", \"pd-host-function\", \"pd-vm-nostd\", \"crates/rustscript\"]\n\n[workspace.package]\nversion = \"0.1.0\"\n\n[package]\nname = \"pd-vm\"\nversion.workspace = true\n\n[dependencies]\npd-host-function = { path = \"./pd-host-function\", version = \"0.1.0\" }\n"""
            )
            (root / "pd-host-function" / "Cargo.toml").write_text(
                """[package]\nname = \"pd-host-function\"\nversion.workspace = true\n"""
            )
            (root / "pd-vm-nostd" / "Cargo.toml").write_text(
                """[package]\nname = \"pd-vm-nostd\"\nversion.workspace = true\n\n[dev-dependencies]\nvm = { package = \"pd-vm\", path = \"..\" }\n"""
            )
            (root / "crates" / "rustscript" / "Cargo.toml").write_text(
                """[package]\nname = \"rustscript\"\nversion.workspace = true\n\n[dependencies]\npd_vm_crate = { package = \"pd-vm\", path = \"../..\", version = \">=0.1.0, <1.0.0\" }\n"""
            )

            publish_crates.rewrite_manifests(root, "0.23.1", "0.22.7")

            root_manifest = (root / "Cargo.toml").read_text()
            host_manifest = (root / "pd-host-function" / "Cargo.toml").read_text()
            nostd_manifest = (root / "pd-vm-nostd" / "Cargo.toml").read_text()
            alias_manifest = (root / "crates" / "rustscript" / "Cargo.toml").read_text()
            self.assertIn('version = "0.23.1"', root_manifest)
            self.assertIn('version = "0.22.7"', host_manifest)
            self.assertNotIn("version.workspace = true", host_manifest)
            self.assertIn('pd-host-function = { path = "./pd-host-function", version = "0.22.7" }', root_manifest)
            # A versioned path dev-dependency stays in the published manifest and
            # then needs an already published version, so it is left path-only.
            self.assertIn('vm = { package = "pd-vm", path = ".." }', nostd_manifest)
            self.assertIn('version = "0.23.1"', alias_manifest)
            self.assertEqual(
                publish_crates.stale_path_dependencies(root, publish_crates.package_versions("0.23.1", "0.22.7")),
                [],
            )

    def _write_publish_tree_shaped_like_0_31_2(self, root: Path) -> None:
        (root / "pd-host-function").mkdir()
        (root / "crates" / "pd-host-schema").mkdir(parents=True)
        (root / "Cargo.toml").write_text(
            """[workspace]
members = [".", "pd-host-function", "crates/pd-host-schema"]

[workspace.package]
version = "0.1.0"

[package]
name = "pd-vm"
version.workspace = true

[dependencies]
pd-host-function = { path = "./pd-host-function", version = "0.1.0" }

[build-dependencies]
pd-host-schema = { path = "./crates/pd-host-schema", version = "0.1.0" }

[dev-dependencies]
pd-host-schema = { path = "./crates/pd-host-schema", version = "0.1.0" }

[target.'cfg(not(target_family = "wasm"))'.dev-dependencies]
tokio = { version = "1", features = ["macros"] }
"""
        )
        (root / "pd-host-function" / "Cargo.toml").write_text(
            """[package]
name = "pd-host-function"
version.workspace = true

[dependencies]
pd-host-schema = { path = "../crates/pd-host-schema", version = "0.1.0" }
proc-macro2 = "1"

[dev-dependencies]
vm = { package = "pd-vm", path = ".." }
"""
        )
        (root / "crates" / "pd-host-schema" / "Cargo.toml").write_text(
            """[package]
name = "pd-host-schema"
version.workspace = true

[dependencies]
syn = { version = "2", features = ["full"] }
"""
        )

    def test_publish_tree_rewrites_every_workspace_path_dependency(self) -> None:
        # 0.31.2 failed with `failed to select a version for the requirement
        # pd-host-schema = "^0.1.0"` because the workspace version bump moved
        # pd-host-schema to 0.31.2 while its path dependents kept `^0.1.0`.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self._write_publish_tree_shaped_like_0_31_2(root)

            publish_crates.rewrite_manifests(root, "0.31.2", "0.22.7")

            root_manifest = (root / "Cargo.toml").read_text()
            host_manifest = (root / "pd-host-function" / "Cargo.toml").read_text()
            schema_manifest = (root / "crates" / "pd-host-schema" / "Cargo.toml").read_text()
            self.assertIn('version = "0.31.2"', root_manifest)
            self.assertIn(
                'pd-host-function = { path = "./pd-host-function", version = "0.22.7" }',
                root_manifest,
            )
            self.assertIn(
                'pd-host-schema = { path = "./crates/pd-host-schema", version = "0.31.2" }',
                root_manifest,
            )
            self.assertIn(
                'pd-host-schema = { path = "../crates/pd-host-schema", version = "0.31.2" }',
                host_manifest,
            )
            self.assertIn('version = "0.22.7"', host_manifest)
            self.assertIn('version = "0.31.2"', schema_manifest)
            self.assertNotIn('"0.1.0"', root_manifest + host_manifest)
            self.assertEqual(
                publish_crates.stale_path_dependencies(
                    root, publish_crates.package_versions("0.31.2", "0.22.7")
                ),
                [],
            )

    def test_dev_dependency_path_requirement_is_dropped(self) -> None:
        # A versioned path dev-dependency must resolve on crates.io at package
        # time, which blocks crates published before their dev-dependency.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self._write_publish_tree_shaped_like_0_31_2(root)

            publish_crates.rewrite_manifests(root, "0.31.2", "0.22.7")

            root_manifest = (root / "Cargo.toml").read_text()
            dev_section = root_manifest.split("[dev-dependencies]", 1)[1].split("[", 1)[0]
            self.assertIn(
                'pd-host-schema = { path = "./crates/pd-host-schema" }',
                dev_section,
            )
            self.assertNotIn("version", dev_section)

    def test_stale_path_dependencies_reports_unrewritten_requirement(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self._write_publish_tree_shaped_like_0_31_2(root)
            # Simulate the 0.31.2 publish tree: workspace version bumped, path
            # requirement left behind.
            manifest = root / "Cargo.toml"
            manifest.write_text(
                manifest.read_text().replace(
                    'version = "0.1.0"\n\n[package]', 'version = "0.31.2"\n\n[package]'
                )
            )

            problems = publish_crates.stale_path_dependencies(
                root, publish_crates.package_versions("0.31.2", "0.22.7")
            )

            self.assertEqual(len(problems), 4)
            self.assertTrue(
                all("pd-host-schema" in problem or "pd-host-function" in problem for problem in problems)
            )
            self.assertTrue(
                any("'0.1.0', expected '0.31.2'" in problem for problem in problems)
            )

    def test_refresh_compatible_registry_lock_leaves_current_lock_alone(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.lock").write_text(
                """version = 4

[[package]]
name = "pd-host-function"
version = "0.22.7"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "current"
"""
            )
            with mock.patch.object(
                publish_crates.subprocess, "run", side_effect=AssertionError("cargo must not run")
            ):
                publish_crates.refresh_compatible_registry_lock(
                    root, "pd-host-function", "0.22.7"
                )

    def test_workflow_package_order_matches_publish_plan(self) -> None:
        workflow = (
            Path(__file__).parents[1] / ".github" / "workflows" / "publish-crates.yml"
        ).read_text()
        order_line = next(
            line for line in workflow.splitlines() if "PACKAGE_ORDER:" in line
        )

        self.assertEqual(
            tuple(order_line.split("PACKAGE_ORDER:", 1)[1].split()),
            publish_crates.PACKAGE_ORDER,
        )

    def test_yank_command_targets_requested_host_version(self) -> None:
        self.assertEqual(
            publish_crates.yank_command("pd-host-function", "0.23.0"),
            ["cargo", "yank", "--vers", "0.23.0", "pd-host-function"],
        )

    def test_publish_plan_uses_dependency_order_and_per_package_versions(self) -> None:
        self.assertEqual(
            publish_crates.publish_plan("0.23.1", "0.22.7"),
            [
                ("pd-host-schema", "0.23.1"),
                ("pd-host-function", "0.22.7"),
                ("pd-vm", "0.23.1"),
                ("pd-vm-nostd", "0.23.1"),
                ("rustscript", "0.23.1"),
            ],
        )


if __name__ == "__main__":
    unittest.main()
