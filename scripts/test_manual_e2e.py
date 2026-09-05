from __future__ import annotations

import os
import platform
import subprocess
import time
import unittest
from pathlib import Path


BINARY_ENV = "EGO_LITE_BRIDGE_BIN"
TARGET_ENV = "EGO_LITE_BRIDGE_SSH_TARGET"
SHIM_ENV = "EGO_LITE_BRIDGE_LINUX_SHIM"


@unittest.skipUnless(
    platform.system() == "Darwin"
    and all(os.environ.get(name) for name in (BINARY_ENV, TARGET_ENV)),
    f"manual E2E requires macOS, {BINARY_ENV}, and {TARGET_ENV}",
)
class ManualE2ETest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.binary = Path(os.environ[BINARY_ENV]).expanduser().resolve()
        if not cls.binary.is_file():
            raise RuntimeError(f"{BINARY_ENV} is not a file: {cls.binary}")
        cls.target = os.environ[TARGET_ENV]
        cls.shim = os.environ.get(SHIM_ENV, "~/.local/bin/ego-browser")
        cls.name = f"manual-e2e-{os.getpid()}-{time.time_ns()}"
        cls.duplicate_name = f"{cls.name}-duplicate"
        cls.added: set[str] = set()

    @classmethod
    def tearDownClass(cls) -> None:
        for name in (cls.duplicate_name, cls.name):
            if name in cls.added:
                cls.run_bridge("remote", "remove", name, check=False)
        cls.run_bridge("stop", check=False)

    @classmethod
    def run_bridge(
        cls, *args: str, check: bool = True
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(cls.binary), *args],
            check=check,
            capture_output=True,
            text=True,
            timeout=45,
        )

    def test_remote_crud_and_linux_shim(self) -> None:
        self.run_bridge("start")

        added = self.run_bridge("remote", "add", self.name, self.target)
        self.added.add(self.name)
        self.assertIn("Active/Connected", added.stdout)

        status = self.run_bridge("status")
        self.assertIn("running (", status.stdout)
        remote_status = self.run_bridge("remote", "status", self.name)
        self.assertIn("Active/Connected", remote_status.stdout)

        shim = subprocess.run(
            ["ssh", self.target, self.shim, "--version"],
            check=False,
            capture_output=True,
            timeout=45,
        )
        self.assertEqual(shim.returncode, 0, shim.stderr.decode(errors="replace"))

        duplicate = self.run_bridge(
            "remote", "add", self.duplicate_name, self.target, check=False
        )
        if duplicate.returncode == 0:
            self.added.add(self.duplicate_name)
        self.assertNotEqual(duplicate.returncode, 0, duplicate.stdout + duplicate.stderr)
        self.assertIn("endpoint", (duplicate.stdout + duplicate.stderr).lower())

        removed = self.run_bridge("remote", "remove", self.name)
        self.added.remove(self.name)
        self.assertIn("removed ", removed.stdout)


if __name__ == "__main__":
    unittest.main()
