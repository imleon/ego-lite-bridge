from __future__ import annotations

import os
import platform
import subprocess
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
        cls.added: set[str] = set()

    @classmethod
    def tearDownClass(cls) -> None:
        for config_id in cls.added:
            cls.run_bridge("remote", "remove", config_id, check=False)
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

    @staticmethod
    def config_snapshot() -> tuple[bytes, tuple[int, int, int, int, int, int]]:
        path = Path.home() / "Library/Application Support/ego-lite-bridge/config.json"
        stat = path.stat()
        return path.read_bytes(), (
            stat.st_mode,
            stat.st_uid,
            stat.st_gid,
            stat.st_ino,
            stat.st_size,
            stat.st_mtime_ns,
        )

    def test_remote_crud_and_linux_shim(self) -> None:
        self.run_bridge("start")

        added = self.run_bridge("remote", "add", self.target)
        fields = added.stdout.rstrip("\n").split("\t")
        self.assertEqual(len(fields), 3, added.stdout)
        config_id = fields[0]
        self.added.add(config_id)
        self.assertRegex(config_id, r"^[0-9a-f]{32}$")
        self.assertEqual(
            fields[1:],
            [self.target, "desired=active observed=connected"],
        )

        status = self.run_bridge("status")
        status_lines = status.stdout.splitlines()
        self.assertRegex(status_lines[0], r"^daemon=running remotes=[1-9][0-9]*$")
        self.assertIn(
            f"{config_id} desired=active observed=connected", status_lines[1:]
        )
        remote_list = self.run_bridge("remote", "list")
        self.assertIn(added.stdout.rstrip("\n"), remote_list.stdout.splitlines())

        remote_status = self.run_bridge("remote", "status", config_id)
        details = dict(line.split(": ", 1) for line in remote_status.stdout.splitlines())
        self.assertNotIn("name", details)
        self.assertEqual(
            list(details),
            [
                "config-id",
                "target",
                "desired",
                "observed",
                "state-changed-unix-ms",
                "last-error",
                "protocol-version",
                "capabilities",
                "reconnect-attempt",
                "reconnect-at-unix-ms",
                "active-requests",
            ],
        )
        self.assertEqual(details["config-id"], config_id)
        self.assertEqual(details["target"], self.target)
        self.assertEqual(details["desired"], "active")
        self.assertEqual(details["observed"], "connected")
        self.assertEqual(details["last-error"], "unknown")
        self.assertEqual(details["protocol-version"], "2")
        self.assertRegex(details["capabilities"], r"^0x[0-9a-f]+$")
        self.assertEqual(details["reconnect-attempt"], "unknown")
        self.assertEqual(details["reconnect-at-unix-ms"], "unknown")
        self.assertRegex(details["active-requests"], r"^0/[1-9][0-9]*$")

        before_doctor = self.config_snapshot()
        for selector in ((), (config_id,)):
            doctor = self.run_bridge("doctor", *selector)
            lines = doctor.stdout.splitlines()
            self.assertIn("PASS mac.launchd: loaded", lines)
            self.assertIn("PASS mac.daemon: running", lines)
            self.assertTrue(
                any(
                    line.startswith(
                        "PASS mac.browser: valid configured executable "
                    )
                    for line in lines
                )
            )
            self.assertIn(
                f"PASS remote.{config_id}.state: daemon snapshot desired=active observed=connected",
                lines,
            )
            self.assertIn(
                f"PASS remote.{config_id}.configured_identity: present", lines
            )
            self.assertIn(
                f"PASS remote.{config_id}.handshake: currently known v2 "
                f"capabilities={details['capabilities']}",
                lines,
            )
            self.assertIn(f"PASS remote.{config_id}.capacity: 0/8 active", lines)
            self.assertIn(
                f"NOT CHECKED remote.{config_id}.live_endpoint: no new SSH, socket permission check, or end-to-end probe",
                lines,
            )
            self.assertFalse(any(line.startswith("FAIL ") for line in lines), doctor.stdout)
        self.assertEqual(self.config_snapshot(), before_doctor, "doctor changed config")

        shim = subprocess.run(
            ["ssh", self.target, self.shim, "--version"],
            check=False,
            capture_output=True,
            timeout=45,
        )
        self.assertEqual(shim.returncode, 0, shim.stderr.decode(errors="replace"))

        duplicate = self.run_bridge("remote", "add", self.target, check=False)
        if duplicate.returncode == 0:
            duplicate_id = duplicate.stdout.split("\t", 1)[0]
            self.added.add(duplicate_id)
        self.assertNotEqual(duplicate.returncode, 0, duplicate.stdout + duplicate.stderr)
        self.assertIn(config_id, duplicate.stdout + duplicate.stderr)
        self.assertIn("endpoint", (duplicate.stdout + duplicate.stderr).lower())

        removed = self.run_bridge("remote", "remove", config_id)
        self.added.remove(config_id)
        self.assertIn("removed ", removed.stdout)


if __name__ == "__main__":
    unittest.main()
