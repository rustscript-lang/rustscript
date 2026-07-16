#!/usr/bin/env python3
import tempfile
import unittest
from pathlib import Path

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

    def test_publish_script_does_not_force_registry_lock_versions(self) -> None:
        source = (Path(__file__).parent / "publish_crates.py").read_text()
        self.assertNotIn("refresh_registry_lock", source)
        self.assertNotIn("refresh_publish_lock.py", source)

    def test_package_versions_keep_host_macro_on_compatible_line(self) -> None:
        self.assertEqual(
            publish_crates.package_versions("0.23.1", "0.22.7"),
            {
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
            self.assertIn('vm = { package = "pd-vm", path = "..", version = "0.23.1" }', nostd_manifest)
            self.assertIn('version = "0.23.1"', alias_manifest)

    def test_yank_command_targets_requested_host_version(self) -> None:
        self.assertEqual(
            publish_crates.yank_command("pd-host-function", "0.23.0"),
            ["cargo", "yank", "--vers", "0.23.0", "pd-host-function"],
        )

    def test_publish_plan_uses_dependency_order_and_per_package_versions(self) -> None:
        self.assertEqual(
            publish_crates.publish_plan("0.23.1", "0.22.7"),
            [
                ("pd-host-function", "0.22.7"),
                ("pd-vm", "0.23.1"),
                ("pd-vm-nostd", "0.23.1"),
                ("rustscript", "0.23.1"),
            ],
        )


if __name__ == "__main__":
    unittest.main()
