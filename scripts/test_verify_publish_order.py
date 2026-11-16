#!/usr/bin/env python3
import unittest
from pathlib import Path

from scripts import verify_publish_order


def workspace(*packages: tuple[str, dict[str, tuple[str, ...]]]) -> dict[str, dict]:
    """Build cargo-metadata-like packages: (name, {"normal"/"build"/"dev": deps})."""
    result = {}
    for name, dependencies in packages:
        entries = []
        for kind, dependency_names in dependencies.items():
            for dependency_name in dependency_names:
                # cargo metadata reports normal dependencies with a null kind.
                entries.append(
                    {
                        "name": dependency_name,
                        "kind": None if kind == "normal" else kind,
                    }
                )
        result[name] = {"name": name, "dependencies": entries}
    return result


ORDER = ["pd-host-schema", "pd-host-function", "pd-vm", "pd-vm-nostd", "rustscript"]


class VerifyPublishOrderTests(unittest.TestCase):
    def test_dev_dependency_does_not_constrain_publish_order(self) -> None:
        packages = workspace(
            ("pd-host-schema", {}),
            ("pd-host-function", {"normal": ("pd-host-schema",), "dev": ("pd-vm",)}),
            ("pd-vm", {}),
            ("pd-vm-nostd", {"normal": ("pd-vm",)}),
            ("rustscript", {"normal": ("pd-vm",)}),
        )

        self.assertEqual(
            verify_publish_order.publish_order_errors(packages, ORDER),
            [],
        )

    def test_build_dependency_missing_from_order_is_rejected(self) -> None:
        # 0.31.2 published straight into `failed to select a version for the
        # requirement pd-host-schema = "^0.1.0"` because the order omitted the
        # build dependency that the publish tree had to rewrite.
        packages = workspace(
            ("pd-host-schema", {}),
            ("pd-host-function", {"normal": ("pd-host-schema",)}),
            ("pd-vm", {"build": ("pd-host-schema",)}),
        )

        errors = verify_publish_order.publish_order_errors(
            packages,
            ["pd-host-function", "pd-vm"],
        )

        self.assertEqual(len(errors), 2)
        self.assertIn(
            "pd-vm has a build dependency on workspace package pd-host-schema, "
            "so pd-host-schema must be published first",
            errors,
        )

    def test_normal_dependency_must_be_published_first(self) -> None:
        packages = workspace(
            ("pd-host-function", {"normal": ("pd-vm",)}),
            ("pd-vm", {}),
        )

        errors = verify_publish_order.publish_order_errors(
            packages,
            ["pd-host-function", "pd-vm"],
        )

        self.assertEqual(len(errors), 1)
        self.assertIn("pd-host-function has a normal dependency on pd-vm", errors[0])

    def test_build_dependency_must_be_published_first(self) -> None:
        packages = workspace(
            ("pd-host-function", {"build": ("pd-vm",)}),
            ("pd-vm", {}),
        )

        errors = verify_publish_order.publish_order_errors(
            packages,
            ["pd-host-function", "pd-vm"],
        )

        self.assertEqual(len(errors), 1)
        self.assertIn("pd-host-function has a build dependency on pd-vm", errors[0])

    def test_duplicate_package_names_are_rejected(self) -> None:
        packages = workspace(("pd-vm", {}))

        self.assertEqual(
            verify_publish_order.publish_order_errors(packages, ["pd-vm", "pd-vm"]),
            ["publish order contains duplicate package name: pd-vm"],
        )

    def test_unknown_package_names_are_rejected(self) -> None:
        packages = workspace(("pd-vm", {}))

        self.assertEqual(
            verify_publish_order.publish_order_errors(packages, ["pd-vm", "missing"]),
            ["publish order contains unknown workspace packages: ['missing']"],
        )


class ReleaseUploadWorkflowTests(unittest.TestCase):
    def test_release_assets_are_uploaded_to_a_draft_without_finalizing(self) -> None:
        workflow = (
            Path(__file__).parents[1] / ".github" / "workflows" / "ci.yml"
        ).read_text()

        # softprops/action-gh-release publishes the release it touches, which
        # fails once the release is immutable.
        self.assertNotIn("softprops/action-gh-release", workflow)
        self.assertEqual(workflow.count("gh release upload"), 2)
        self.assertEqual(workflow.count("--clobber"), 2)
        self.assertEqual(workflow.count("gh release create"), 2)
        self.assertNotIn("gh release edit", workflow)
        self.assertNotIn("--draft=false", workflow)


if __name__ == "__main__":
    unittest.main()
