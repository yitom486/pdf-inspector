import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from version import (
    PLATFORM_PACKAGES,
    check_versions,
    is_fork_version,
    is_fork_wheel_version,
    python_wheel_version,
    require_pypi_publishable,
    set_versions,
    write_cargo_wheel_version,
)


class VersionTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        (self.root / "napi").mkdir()
        (self.root / "site").mkdir()
        (self.root / "wasm").mkdir()

        self._write_manifest("Cargo.toml", "package", "0.1.0")
        self._write_manifest("pyproject.toml", "project", "0.1.0")
        self._write_manifest("napi/Cargo.toml", "package", "0.1.0")
        self._write_manifest("wasm/Cargo.toml", "package", "0.1.0")

        package = {
            "name": "@firecrawl/pdf-inspector",
            "version": "0.1.0",
            "optionalDependencies": {
                dependency: "0.1.0" for dependency in PLATFORM_PACKAGES
            },
        }
        (self.root / "napi/package.json").write_text(
            json.dumps(package), encoding="utf-8"
        )
        (self.root / "napi/bun.lock").write_text(
            "\n".join(
                f'        "{dependency}": "0.1.0",'
                for dependency in PLATFORM_PACKAGES
            )
            + "\n",
            encoding="utf-8",
        )
        (self.root / "site/index.html").write_text(
            'https://cdn.jsdelivr.net/npm/@firecrawl/pdf-inspector-wasm@0.1.0/'
            'pdf_inspector_wasm.js\n',
            encoding="utf-8",
        )
        self._write_lock(
            "napi/Cargo.lock", ("pdf-inspector", "pdf-inspector-napi")
        )
        self._write_lock(
            "wasm/Cargo.lock", ("pdf-inspector", "pdf-inspector-wasm")
        )

    def tearDown(self):
        self.temporary.cleanup()

    def _write_manifest(self, relative, section, version):
        (self.root / relative).write_text(
            f'[{section}]\nname = "fixture"\nversion = "{version}"\n',
            encoding="utf-8",
        )

    def _write_lock(self, relative, packages):
        content = "\n".join(
            f'[[package]]\nname = "{package}"\nversion = "0.1.0"\n'
            for package in packages
        )
        (self.root / relative).write_text(content, encoding="utf-8")

    def test_updates_every_version_location(self):
        set_versions("1.14.0", self.root)

        self.assertEqual(check_versions(self.root), "1.14.0")

    def test_reports_a_divergent_package(self):
        self._write_manifest("wasm/Cargo.toml", "package", "0.2.0")

        with self.assertRaisesRegex(ValueError, "WASM package: 0.2.0"):
            check_versions(self.root)

    def test_rejects_an_invalid_version(self):
        with self.assertRaisesRegex(ValueError, "Invalid semantic version"):
            set_versions("next", self.root)

    def test_rejects_numeric_prerelease_with_leading_zero(self):
        before = (self.root / "Cargo.toml").read_text(encoding="utf-8")

        with self.assertRaisesRegex(ValueError, "Invalid semantic version"):
            set_versions("1.2.3-01", self.root)

        self.assertEqual(
            (self.root / "Cargo.toml").read_text(encoding="utf-8"), before
        )

    def test_preflight_failure_does_not_partially_update(self):
        before = (self.root / "Cargo.toml").read_text(encoding="utf-8")
        (self.root / "site/index.html").write_text(
            "missing module URL\n", encoding="utf-8"
        )

        with self.assertRaisesRegex(ValueError, "Missing pinned WASM package URL"):
            set_versions("1.14.0", self.root)

        self.assertEqual(
            (self.root / "Cargo.toml").read_text(encoding="utf-8"), before
        )


class ForkWheelMappingTests(unittest.TestCase):
    def test_maps_fork_release_to_pep440_wheel(self):
        self.assertEqual(
            python_wheel_version("1.18.2-inkdown.1"), "1.18.2+inkdown.1"
        )
        self.assertEqual(
            python_wheel_version("2.0.0-inkdown.10"), "2.0.0+inkdown.10"
        )

    def test_rejects_non_fork_versions(self):
        for bad in (
            "1.18.2",
            "1.18.2+inkdown.1",
            "1.18.2-inkdown",
            "1.18.2-inkdown.x",
            "1.18.2-Inkdown.1",
            "1.18.2-other.1",
            "1.18.2-inkdown.01",
        ):
            with self.assertRaises(ValueError, msg=bad):
                python_wheel_version(bad)

    def test_classifies_fork_and_wheel_forms(self):
        self.assertTrue(is_fork_version("1.18.2-inkdown.1"))
        self.assertFalse(is_fork_version("1.18.2+inkdown.1"))
        self.assertFalse(is_fork_version("1.18.2"))
        self.assertTrue(is_fork_wheel_version("1.18.2+inkdown.1"))
        self.assertFalse(is_fork_wheel_version("1.18.2-inkdown.1"))
        self.assertFalse(is_fork_wheel_version("1.18.2"))

    def test_pypi_guard_refuses_fork_wheel_versions(self):
        with self.assertRaisesRegex(ValueError, "Refusing to publish"):
            require_pypi_publishable("1.18.2+inkdown.1")

    def test_pypi_guard_passes_through_other_versions(self):
        self.assertEqual(require_pypi_publishable("1.18.0"), "1.18.0")
        # A fork SemVer prerelease is refused later by maturin/PyPI itself;
        # the explicit guard only owns the `+` local form.
        self.assertEqual(
            require_pypi_publishable("1.18.2-inkdown.1"), "1.18.2-inkdown.1"
        )


class ForkTreeTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        (self.root / "napi").mkdir()
        (self.root / "site").mkdir()
        (self.root / "wasm").mkdir()

        self._write_manifest("Cargo.toml", "package", "1.18.2-inkdown.1")
        self._write_manifest("pyproject.toml", "project", "1.18.2+inkdown.1")
        self._write_manifest("napi/Cargo.toml", "package", "1.18.2-inkdown.1")
        self._write_manifest("wasm/Cargo.toml", "package", "1.18.2-inkdown.1")

        package = {
            "name": "@firecrawl/pdf-inspector",
            "version": "1.18.2-inkdown.1",
            "optionalDependencies": {
                dependency: "1.18.2-inkdown.1" for dependency in PLATFORM_PACKAGES
            },
        }
        (self.root / "napi/package.json").write_text(
            json.dumps(package), encoding="utf-8"
        )
        (self.root / "napi/bun.lock").write_text(
            "\n".join(
                f'        "{dependency}": "1.18.2-inkdown.1",'
                for dependency in PLATFORM_PACKAGES
            )
            + "\n",
            encoding="utf-8",
        )
        (self.root / "site/index.html").write_text(
            'https://cdn.jsdelivr.net/npm/@firecrawl/pdf-inspector-wasm@1.18.2-inkdown.1/'
            'pdf_inspector_wasm.js\n',
            encoding="utf-8",
        )
        self._write_lock(
            "napi/Cargo.lock",
            ("pdf-inspector", "pdf-inspector-napi"),
            "1.18.2-inkdown.1",
        )
        self._write_lock(
            "wasm/Cargo.lock",
            ("pdf-inspector", "pdf-inspector-wasm"),
            "1.18.2-inkdown.1",
        )

    def tearDown(self):
        self.temporary.cleanup()

    def _write_manifest(self, relative, section, version):
        (self.root / relative).write_text(
            f'[{section}]\nname = "fixture"\nversion = "{version}"\n',
            encoding="utf-8",
        )

    def _write_lock(self, relative, packages, version):
        content = "\n".join(
            f'[[package]]\nname = "{package}"\nversion = "{version}"\n'
            for package in packages
        )
        (self.root / relative).write_text(content, encoding="utf-8")

    def test_accepts_mapped_wheel_version(self):
        self.assertEqual(check_versions(self.root), "1.18.2-inkdown.1")

    def test_rejects_unmapped_wheel_version(self):
        self._write_manifest("pyproject.toml", "project", "1.18.2-inkdown.1")

        with self.assertRaisesRegex(ValueError, "Python package"):
            check_versions(self.root)

    def test_set_versions_round_trip_writes_mapped_wheel(self):
        self._write_manifest("pyproject.toml", "project", "1.18.2-inkdown.1")
        set_versions("1.18.2-inkdown.1", self.root)

        manifest = (self.root / "pyproject.toml").read_text(encoding="utf-8")
        self.assertIn('version = "1.18.2+inkdown.1"', manifest)
        cargo = (self.root / "Cargo.toml").read_text(encoding="utf-8")
        self.assertIn('version = "1.18.2-inkdown.1"', cargo)
        self.assertEqual(check_versions(self.root), "1.18.2-inkdown.1")

    def test_write_cargo_wheel_version_translates_fork_tree(self):
        written = write_cargo_wheel_version(self.root)

        self.assertEqual(written, "1.18.2+inkdown.1")
        cargo = (self.root / "Cargo.toml").read_text(encoding="utf-8")
        self.assertIn('version = "1.18.2+inkdown.1"', cargo)
        # Only Cargo.toml moves; the wheel manifest is untouched.
        manifest = (self.root / "pyproject.toml").read_text(encoding="utf-8")
        self.assertIn('version = "1.18.2+inkdown.1"', manifest)

    def test_write_cargo_wheel_version_is_noop_off_fork(self):
        self._write_manifest("Cargo.toml", "package", "1.18.0")
        before = (self.root / "Cargo.toml").read_text(encoding="utf-8")

        self.assertEqual(write_cargo_wheel_version(self.root), "1.18.0")
        self.assertEqual(
            (self.root / "Cargo.toml").read_text(encoding="utf-8"), before
        )


if __name__ == "__main__":
    unittest.main()
