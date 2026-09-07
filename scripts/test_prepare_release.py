from __future__ import annotations

import hashlib
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("prepare_release.py")
NAMES = (
    "ego-lite-bridge-linux-x86_64",
    "ego-lite-bridge-macos-aarch64",
)


class PrepareReleaseTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory(prefix="prepare-release-test-")
        self.root = Path(self.temp_dir.name)
        self.assets = []
        for index, name in enumerate(NAMES):
            path = self.root / name
            path.write_bytes(f"asset-{index}\n".encode())
            self.assets.append(path)

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def run_script(
        self, version: str = "0.1.0", assets: list[Path] | None = None
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(SCRIPT), version, *(str(path) for path in assets or self.assets)],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_generates_deterministic_installer_manifest(self) -> None:
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        manifest = json.loads(result.stdout)
        self.assertEqual(
            manifest,
            {
                "product": "ego-lite-bridge",
                "available": False,
                "version": "0.1.0",
                "assets": {
                    target: "https://github.com/imleon/ego-lite-bridge/releases/download/v0.1.0/"
                    + name
                    for name, target in zip(NAMES, ("linux-x86_64", "macos-aarch64"))
                },
                "sha256": {
                    target: hashlib.sha256(path.read_bytes()).hexdigest()
                    for path, target in zip(self.assets, ("linux-x86_64", "macos-aarch64"))
                },
            },
        )
        self.assertEqual(result.stdout, self.run_script(assets=self.assets[::-1]).stdout)

    def test_rejects_invalid_inputs(self) -> None:
        cases: list[tuple[str, list[Path], str]] = [
            ("v0.1.0", self.assets, "invalid semantic version"),
            ("01.1.0", self.assets, "invalid semantic version"),
            ("0.1", self.assets, "invalid semantic version"),
            ("1٢.2.3", self.assets, "invalid semantic version"),
            ("1.2.3-1٢", self.assets, "invalid semantic version"),
            ("1.2.3-a.1٢", self.assets, "invalid semantic version"),
            ("0.1.0", [self.assets[0]], "the following arguments are required"),
            ("0.1.0", [self.assets[0], self.assets[0]], "duplicate asset"),
        ]

        unknown = self.root / "unknown"
        unknown.write_bytes(b"unknown")
        cases.append(("0.1.0", [self.assets[0], unknown], "unknown asset name"))

        empty = self.root / NAMES[1]
        empty.write_bytes(b"")
        cases.append(("0.1.0", self.assets, "asset is empty"))

        for version, assets, error in cases:
            with self.subTest(version=version, assets=assets, error=error):
                result = self.run_script(version, assets)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(error, result.stderr)

    def test_rejects_symlink_and_non_file(self) -> None:
        directory = self.assets[1]
        directory.unlink()
        directory.mkdir()
        result = self.run_script(assets=[self.assets[0], directory])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not a regular file", result.stderr)

        directory.rmdir()
        directory.symlink_to(self.assets[0])
        result = self.run_script(assets=[self.assets[0], directory])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must not be a symlink", result.stderr)


if __name__ == "__main__":
    unittest.main()
