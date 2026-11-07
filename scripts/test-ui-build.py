#!/usr/bin/env python3
"""Exercise the Rust UI validation script without Cargo, Node, or real UI assets."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


class UiBuildTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.compiler_dir = tempfile.TemporaryDirectory(prefix="jellyswarrm-build-script-")
        cls.binary = Path(cls.compiler_dir.name) / "ui-build-check"
        source = Path(__file__).resolve().parents[1] / "crates/jellyswarrm-proxy/build.rs"
        subprocess.run(["rustc", str(source), "-o", str(cls.binary)], check=True)

    @classmethod
    def tearDownClass(cls):
        cls.compiler_dir.cleanup()

    def setUp(self):
        self.fixture = tempfile.TemporaryDirectory(prefix="jellyswarrm-ui-fixture-")
        self.addCleanup(self.fixture.cleanup)
        self.manifest = Path(self.fixture.name)
        self.index = self.manifest / "static/index.html"

    def run_build(self, profile="debug", allow_placeholder=False):
        env = os.environ.copy()
        env["CARGO_MANIFEST_DIR"] = str(self.manifest)
        env["PROFILE"] = profile
        env.pop("JELLYSWARRM_ALLOW_PLACEHOLDER_UI", None)
        if allow_placeholder:
            env["JELLYSWARRM_ALLOW_PLACEHOLDER_UI"] = "1"
        # A build must not need git, npm, or any other external command.
        env["PATH"] = ""
        return subprocess.run([self.binary], env=env, text=True, capture_output=True)

    def test_fresh_debug_build_creates_placeholder_and_tracks_inputs(self):
        result = self.run_build()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("just ui", self.index.read_text())
        self.assertIn("cargo:warning=", result.stdout)
        for directive in (
            "cargo:rerun-if-changed=migrations",
            "cargo:rerun-if-changed=static",
            "cargo:rerun-if-env-changed=JELLYSWARRM_ALLOW_PLACEHOLDER_UI",
        ):
            self.assertIn(directive, result.stdout)

    def test_existing_placeholder_is_rejected_in_release(self):
        self.assertEqual(self.run_build().returncode, 0)
        result = self.run_build(profile="release")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Release builds require the web client", result.stderr)

    def test_fresh_release_build_is_rejected(self):
        self.assertNotEqual(self.run_build(profile="release").returncode, 0)

    def test_placeholder_release_requires_explicit_opt_in(self):
        result = self.run_build(profile="release", allow_placeholder=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("cargo:warning=", result.stdout)

    def test_real_ui_replaces_placeholder_and_is_not_modified(self):
        self.assertEqual(self.run_build().returncode, 0)
        html = "<!doctype html><html><body>Built UI</body></html>"
        self.index.write_text(html)
        result = self.run_build(profile="release")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.index.read_text(), html)
        self.assertNotIn("cargo:warning=", result.stdout)

    def test_empty_index_is_rejected_in_release(self):
        self.index.parent.mkdir()
        self.index.write_text("\n")
        self.assertNotEqual(self.run_build(profile="release").returncode, 0)

    def test_static_directory_without_index_is_repaired_for_debug(self):
        self.index.parent.mkdir()
        result = self.run_build()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("just ui", self.index.read_text())


if __name__ == "__main__":
    unittest.main()
