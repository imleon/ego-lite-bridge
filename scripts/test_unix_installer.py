from __future__ import annotations

import hashlib
import io
import json
import os
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
INSTALLER = REPO_ROOT / "distribution" / "install.sh"
REQUIRED_COMMANDS = (
    "awk",
    "cat",
    "chmod",
    "cp",
    "curl",
    "gzip",
    "ln",
    "mkdir",
    "mktemp",
    "mv",
    "node",
    "npx",
    "readlink",
    "rm",
    "tar",
)
FAKE_COMMANDS = {"curl", "node", "npx"}


class UnixInstallerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory(prefix="ego-lite-bridge-installer-test-")
        self.root = Path(self.temp_dir.name)
        self.bin_dir = self.root / "bin"
        self.bin_dir.mkdir()
        self.install_dir = self.root / "install"
        self.payload = self.root / "payload"
        self.payload.write_bytes(b"fake-ego-lite-bridge-binary\n")
        self.expected_sha256 = hashlib.sha256(self.payload.read_bytes()).hexdigest()
        self.skill_archive = self.root / "ego-browser-skill.tgz"
        self._write_skill_archive(
            ("ego-browser/SKILL.md", b"# ego-browser\n"),
            ("ego-browser/references/install.md", b"# Install\n"),
        )

        for command in REQUIRED_COMMANDS:
            if command in FAKE_COMMANDS:
                continue
            path = shutil.which(command)
            if path is None:
                self.fail(f"test host is missing required command: {command}")
            (self.bin_dir / command).symlink_to(path)

        self._write_executable(
            "curl",
            """#!/bin/sh
out=""
previous=""
for argument in "$@"; do
  if [ "$previous" = "-o" ]; then
    out="$argument"
    break
  fi
  previous="$argument"
done
if [ -n "$out" ]; then
  printf '%s\n' "$out" >> "$FAKE_OUTPUT_LOG"
  case "$*" in
    *ego-browser-skill.tgz*) cp "$FAKE_SKILL_ARCHIVE" "$out" ;;
    *) cp "$FAKE_PAYLOAD" "$out" ;;
  esac
else
  cat "$FAKE_MANIFEST"
fi
""",
        )
        self._write_executable("node", "#!/bin/sh\nprintf '%s\\n' \"${FAKE_NODE_VERSION:-v22.20.0}\"\n")
        self._write_executable(
            "npx",
            """#!/bin/sh
printf '%s\n' "$@" > "$FAKE_NPX_LOG"
exit "${FAKE_NPX_EXIT:-0}"
""",
        )

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def _write_skill_archive(self, *files: tuple[str, bytes]) -> None:
        with tarfile.open(self.skill_archive, "w:gz") as archive:
            for name, content in files:
                info = tarfile.TarInfo(name)
                info.size = len(content)
                archive.addfile(info, io.BytesIO(content))
        self.skill_sha256 = hashlib.sha256(self.skill_archive.read_bytes()).hexdigest()

    def _write_executable(self, name: str, content: str) -> None:
        path = self.bin_dir / name
        path.write_text(content, encoding="utf-8")
        path.chmod(0o755)

    def _select_checksum_tool(self, tool: str) -> None:
        if tool == "sha256sum":
            path = shutil.which("sha256sum")
            if path is None:
                self.fail("test host is missing sha256sum")
            checksum_tool = self.bin_dir / "sha256sum"
            if not checksum_tool.exists():
                checksum_tool.symlink_to(path)
            return

        if tool == "shasum":
            sha256sum = shutil.which("sha256sum")
            if sha256sum is None:
                self.fail("test host is missing sha256sum for the shasum fixture")
            self._write_executable(
                "shasum",
                f"""#!/bin/sh
[ "$1" = "-a" ] && [ "$2" = "256" ] || exit 2
shift 2
exec {sha256sum} "$@"
""",
            )
            return

        if tool == "openssl":
            path = shutil.which("openssl")
            if path is None:
                self.fail("test host is missing openssl")
            (self.bin_dir / "openssl").symlink_to(path)
            return

        self.fail(f"unknown checksum tool fixture: {tool}")

    def _write_manifest(
        self,
        checksum: str | None,
        os_name: str,
        *,
        product: str = "ego-lite-bridge",
        available: bool = True,
        version: str | None = "9.9.9",
        asset_url: str | None = None,
        skill_url: str | None = None,
        skill_sha256: str | None = None,
    ) -> Path:
        target = f"{os_name}-x86_64"
        manifest: dict[str, object] = {
            "product": product,
            "available": available,
            "assets": {
                target: asset_url
                if asset_url is not None
                else f"https://github.com/imleon/ego-lite-bridge/releases/download/v{version}/ego-lite-bridge-{target}"
            },
        }
        if version is not None:
            manifest["version"] = version
        if checksum is not None:
            manifest["sha256"] = {target: checksum}
        if os_name == "linux":
            manifest["skill_url"] = (
                skill_url
                if skill_url is not None
                else f"https://github.com/imleon/ego-lite-bridge/releases/download/v{version}/ego-browser-skill.tgz"
            )
            manifest["skill_sha256"] = self.skill_sha256 if skill_sha256 is None else skill_sha256
        path = self.root / "latest.json"
        path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        return path

    def _installer_env(
        self,
        checksum: str | None,
        tool: str = "sha256sum",
        os_name: str = "linux",
        **manifest_options: object,
    ) -> dict[str, str]:
        self._select_checksum_tool(tool)
        manifest = self._write_manifest(checksum, os_name, **manifest_options)
        uname = "Linux" if os_name == "linux" else "Darwin"
        self._write_executable(
            "uname",
            f'''#!/bin/sh
case "$1" in
  -s) echo {uname} ;;
  -m) echo x86_64 ;;
  *) exit 1 ;;
esac
''',
        )
        return {
            **os.environ,
            "PATH": str(self.bin_dir),
            "FAKE_MANIFEST": str(manifest),
            "FAKE_PAYLOAD": str(self.payload),
            "FAKE_SKILL_ARCHIVE": str(self.skill_archive),
            "FAKE_OUTPUT_LOG": str(self.root / "output-path"),
            "FAKE_NPX_LOG": str(self.root / "npx-args"),
            "EGO_LITE_BRIDGE_INSTALL_DIR": str(self.install_dir),
            "EGO_LITE_BRIDGE_MANIFEST_URL": "https://example.invalid/latest.json",
            "TAR_OPTIONS": "",
        }

    def _run_installer(
        self,
        checksum: str | None,
        tool: str = "sha256sum",
        os_name: str = "linux",
        **manifest_options: object,
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["/bin/sh", str(INSTALLER)],
            env=self._installer_env(checksum, tool, os_name, **manifest_options),
            capture_output=True,
            text=True,
            check=False,
        )

    def test_valid_download_uses_each_supported_checksum_tool(self) -> None:
        for tool in ("sha256sum", "shasum", "openssl"):
            with self.subTest(tool=tool):
                shutil.rmtree(self.install_dir, ignore_errors=True)
                for name in ("sha256sum", "shasum", "openssl"):
                    (self.bin_dir / name).unlink(missing_ok=True)

                result = self._run_installer(self.expected_sha256.upper(), tool)

                self.assertEqual(result.returncode, 0, result.stderr)
                installed = self.install_dir / "ego-lite-bridge"
                self.assertEqual(installed.read_bytes(), self.payload.read_bytes())
                self.assertFalse(installed.is_symlink())
                self.assertEqual(os.readlink(self.install_dir / "ego-browser"), "ego-lite-bridge")

    def test_linux_installs_versioned_vendored_skill_for_all_agents(self) -> None:
        result = self._run_installer(self.expected_sha256)

        self.assertEqual(result.returncode, 0, result.stderr)
        args = (self.root / "npx-args").read_text(encoding="utf-8").splitlines()
        self.assertEqual(args[:3], ["--yes", "skills@1.5.24", "add"])
        self.assertTrue(args[3].endswith("/skill/ego-browser"), args[3])
        self.assertEqual(
            args[4:],
            ["--skill", "ego-browser", "--global", "--agent", "*", "--yes", "--copy"],
        )

    def test_ignores_inherited_tar_options(self) -> None:
        env = self._installer_env(self.expected_sha256)
        env["TAR_OPTIONS"] = "--show-transformed-names --transform=s|^ego-browser|other|"

        result = subprocess.run(
            ["/bin/sh", str(INSTALLER)],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.root / "npx-args").exists())

    def test_rejects_untrusted_skill_url_and_checksum(self) -> None:
        for options, message in (
            ({"skill_url": "https://example.invalid/ego-browser-skill.tgz"}, "skill URL does not match"),
            ({"skill_sha256": "0" * 64}, "skill checksum did not match"),
        ):
            with self.subTest(options=options):
                shutil.rmtree(self.install_dir, ignore_errors=True)
                (self.root / "npx-args").unlink(missing_ok=True)

                result = self._run_installer(self.expected_sha256, **options)

                self.assertNotEqual(result.returncode, 0)
                self.assertIn(message, result.stderr)
                self.assertFalse((self.install_dir / "ego-lite-bridge").exists())
                self.assertFalse((self.install_dir / "ego-browser").exists())
                self.assertFalse((self.root / "npx-args").exists())

    def test_rejects_unsafe_or_incomplete_skill_archive(self) -> None:
        cases = (
            ((("other/SKILL.md", b"x"),), "unsafe path"),
            ((("ego-browser/../outside", b"x"),), "unsafe path"),
            ((("ego-browser/SKILL.md", b"x"),), "missing required files"),
        )
        for files, message in cases:
            with self.subTest(files=files):
                shutil.rmtree(self.install_dir, ignore_errors=True)
                (self.root / "npx-args").unlink(missing_ok=True)
                self._write_skill_archive(*files)

                result = self._run_installer(self.expected_sha256)

                self.assertNotEqual(result.returncode, 0)
                self.assertIn(message, result.stderr)
                self.assertFalse((self.install_dir / "ego-lite-bridge").exists())
                self.assertFalse((self.install_dir / "ego-browser").exists())
                self.assertFalse((self.root / "npx-args").exists())

    def test_rejects_skill_archive_links(self) -> None:
        with tarfile.open(self.skill_archive, "w:gz") as archive:
            link = tarfile.TarInfo("ego-browser/SKILL.md")
            link.type = tarfile.SYMTYPE
            link.linkname = "/tmp/outside"
            archive.addfile(link)
            content = b"# Install\n"
            info = tarfile.TarInfo("ego-browser/references/install.md")
            info.size = len(content)
            archive.addfile(info, io.BytesIO(content))
        self.skill_sha256 = hashlib.sha256(self.skill_archive.read_bytes()).hexdigest()

        result = self._run_installer(self.expected_sha256)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("links or special files", result.stderr)
        self.assertFalse((self.install_dir / "ego-browser").exists())
        self.assertFalse((self.root / "npx-args").exists())

    def test_pipe_during_link_does_not_interrupt_commit(self) -> None:
        real_ln = os.readlink(self.bin_dir / "ln")
        (self.bin_dir / "ln").unlink()
        self._write_executable(
            "ln",
            f'#!/bin/sh\nkill -PIPE "$PPID"\nexec {real_ln} "$@"\n',
        )

        result = self._run_installer(self.expected_sha256)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            (self.install_dir / "ego-lite-bridge").read_bytes(),
            self.payload.read_bytes(),
        )
        self.assertEqual(os.readlink(self.install_dir / "ego-browser"), "ego-lite-bridge")

    def test_macos_installs_no_shim_or_skill_without_node_npx_tar(self) -> None:
        env = self._installer_env(self.expected_sha256, os_name="macos")
        for command in ("node", "npx", "tar"):
            (self.bin_dir / command).unlink()

        result = subprocess.run(
            ["/bin/sh", str(INSTALLER)],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            (self.install_dir / "ego-lite-bridge").read_bytes(),
            self.payload.read_bytes(),
        )
        self.assertFalse((self.install_dir / "ego-browser").exists())
        self.assertFalse((self.root / "npx-args").exists())

    def test_node_version_too_old_fails_before_download(self) -> None:
        env = self._installer_env(self.expected_sha256)
        env["FAKE_NODE_VERSION"] = "v22.19.9"

        result = subprocess.run(
            ["/bin/sh", str(INSTALLER)],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("requires Node.js 22.20.0 or newer", result.stderr)
        self.assertFalse((self.root / "output-path").exists())
        self.assertFalse((self.root / "npx-args").exists())

    def test_skill_install_failure_keeps_committed_binary_and_shim(self) -> None:
        self.install_dir.mkdir()
        installed = self.install_dir / "ego-lite-bridge"
        installed.write_bytes(b"existing-ego-lite-bridge\n")
        env = self._installer_env(self.expected_sha256)
        env["FAKE_NPX_EXIT"] = "1"

        result = subprocess.run(
            ["/bin/sh", str(INSTALLER)],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("bridge installed, but ego-browser skill installation failed", result.stderr)
        self.assertEqual(installed.read_bytes(), self.payload.read_bytes())
        self.assertEqual(os.readlink(self.install_dir / "ego-browser"), "ego-lite-bridge")

    def test_unavailable_release_does_not_replace_existing_binary(self) -> None:
        self.install_dir.mkdir()
        installed = self.install_dir / "ego-lite-bridge"
        installed.write_bytes(b"existing-ego-lite-bridge\n")

        result = self._run_installer(self.expected_sha256, available=False)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("release is not available yet", result.stderr)
        self.assertEqual(installed.read_bytes(), b"existing-ego-lite-bridge\n")

    def test_rejects_inherited_herdr_manifest_without_replacing_binary(self) -> None:
        self.install_dir.mkdir()
        installed = self.install_dir / "ego-lite-bridge"
        installed.write_bytes(b"existing-ego-lite-bridge\n")

        result = self._run_installer(
            self.expected_sha256,
            product="herdr",
            asset_url="https://github.com/herdrdev/herdr/releases/download/v0.8.2/herdr-linux-x86_64",
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("manifest is not for ego-lite-bridge", result.stderr)
        self.assertEqual(installed.read_bytes(), b"existing-ego-lite-bridge\n")

    def test_rejects_untrusted_asset_url_without_replacing_binary(self) -> None:
        self.install_dir.mkdir()
        installed = self.install_dir / "ego-lite-bridge"
        installed.write_bytes(b"existing-ego-lite-bridge\n")

        result = self._run_installer(
            self.expected_sha256,
            asset_url="https://example.invalid/ego-lite-bridge-linux-x86_64",
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("asset URL does not match version 9.9.9 and target linux-x86_64", result.stderr)
        self.assertEqual(installed.read_bytes(), b"existing-ego-lite-bridge\n")

    def test_missing_version_fails_without_replacing_existing_binary(self) -> None:
        self.install_dir.mkdir()
        installed = self.install_dir / "ego-lite-bridge"
        installed.write_bytes(b"existing-ego-lite-bridge\n")

        result = self._run_installer(
            self.expected_sha256,
            version=None,
            asset_url="https://github.com/imleon/ego-lite-bridge/releases/download/v9.9.9/ego-lite-bridge-linux-x86_64",
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("manifest does not include a version", result.stderr)
        self.assertEqual(installed.read_bytes(), b"existing-ego-lite-bridge\n")

    def test_asset_url_must_match_manifest_version_and_target(self) -> None:
        for asset_url in (
            "https://github.com/imleon/ego-lite-bridge/releases/download/v8.8.8/ego-lite-bridge-linux-x86_64",
            "https://github.com/imleon/ego-lite-bridge/releases/download/v9.9.9/ego-lite-bridge-macos-x86_64",
        ):
            with self.subTest(asset_url=asset_url):
                result = self._run_installer(self.expected_sha256, asset_url=asset_url)

                self.assertNotEqual(result.returncode, 0)
                self.assertIn(
                    "asset URL does not match version 9.9.9 and target linux-x86_64",
                    result.stderr,
                )

    def test_stages_download_inside_install_dir(self) -> None:
        result = self._run_installer(self.expected_sha256)

        self.assertEqual(result.returncode, 0, result.stderr)
        output_path = Path((self.root / "output-path").read_text(encoding="utf-8").splitlines()[0])
        self.assertEqual(output_path.parent.parent, self.install_dir)
        self.assertTrue(output_path.parent.name.startswith(".ego-lite-bridge."))
        self.assertFalse(output_path.parent.exists())

    def test_rejects_binary_or_shim_directory_before_download(self) -> None:
        for name in ("ego-lite-bridge", "ego-browser"):
            with self.subTest(name=name):
                shutil.rmtree(self.install_dir, ignore_errors=True)
                (self.install_dir / name).mkdir(parents=True)
                (self.root / "output-path").unlink(missing_ok=True)

                result = self._run_installer(self.expected_sha256)

                self.assertNotEqual(result.returncode, 0)
                self.assertIn("path is a directory", result.stderr)
                self.assertFalse((self.root / "output-path").exists())

    def test_rejects_nonmatching_existing_shim_before_download(self) -> None:
        for kind in ("file", "wrong-symlink", "newline-symlink"):
            with self.subTest(kind=kind):
                shutil.rmtree(self.install_dir, ignore_errors=True)
                self.install_dir.mkdir()
                shim = self.install_dir / "ego-browser"
                if kind == "file":
                    shim.write_text("not a shim\n", encoding="utf-8")
                elif kind == "wrong-symlink":
                    shim.symlink_to("other-binary")
                else:
                    shim.symlink_to("ego-lite-bridge\n")
                (self.root / "output-path").unlink(missing_ok=True)

                result = self._run_installer(self.expected_sha256)

                self.assertNotEqual(result.returncode, 0)
                self.assertIn("shim path is not a symlink to ego-lite-bridge", result.stderr)
                self.assertFalse((self.root / "output-path").exists())

    def test_link_failure_does_not_replace_binary(self) -> None:
        self.install_dir.mkdir()
        installed = self.install_dir / "ego-lite-bridge"
        installed.write_bytes(b"existing-ego-lite-bridge\n")
        (self.bin_dir / "ln").unlink()
        self._write_executable("ln", "#!/bin/sh\nexit 1\n")

        result = self._run_installer(self.expected_sha256)

        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(installed.read_bytes(), b"existing-ego-lite-bridge\n")
        self.assertFalse((self.install_dir / "ego-browser").exists())

    def test_binary_move_failure_removes_new_shim_and_rerun_recovers(self) -> None:
        real_mv = os.readlink(self.bin_dir / "mv")
        (self.bin_dir / "mv").unlink()
        self._write_executable("mv", "#!/bin/sh\nexit 1\n")

        failed = self._run_installer(self.expected_sha256)

        shim = self.install_dir / "ego-browser"
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn("failed to install ego-lite-bridge", failed.stderr)
        self.assertFalse(shim.exists())
        self.assertFalse((self.install_dir / "ego-lite-bridge").exists())

        (self.bin_dir / "mv").unlink()
        (self.bin_dir / "mv").symlink_to(real_mv)
        recovered = self._run_installer(self.expected_sha256)

        self.assertEqual(recovered.returncode, 0, recovered.stderr)
        self.assertEqual(os.readlink(shim), "ego-lite-bridge")
        self.assertEqual(
            (self.install_dir / "ego-lite-bridge").read_bytes(),
            self.payload.read_bytes(),
        )

    def test_preparation_failure_does_not_replace_existing_binary(self) -> None:
        self.install_dir.mkdir()
        installed = self.install_dir / "ego-lite-bridge"
        installed.write_bytes(b"existing-ego-lite-bridge\n")
        (self.bin_dir / "chmod").unlink()
        self._write_executable("chmod", "#!/bin/sh\nexit 1\n")

        result = self._run_installer(self.expected_sha256)

        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(installed.read_bytes(), b"existing-ego-lite-bridge\n")
        self.assertEqual(list(self.install_dir.glob(".ego-lite-bridge.*")), [])

    def test_checksum_mismatch_does_not_replace_existing_binary(self) -> None:
        self.install_dir.mkdir()
        installed = self.install_dir / "ego-lite-bridge"
        installed.write_bytes(b"existing-ego-lite-bridge\n")

        result = self._run_installer("0" * 64)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum did not match", result.stderr)
        self.assertEqual(installed.read_bytes(), b"existing-ego-lite-bridge\n")

    def test_missing_checksum_fails_without_replacing_existing_binary(self) -> None:
        self.install_dir.mkdir()
        installed = self.install_dir / "ego-lite-bridge"
        installed.write_bytes(b"existing-ego-lite-bridge\n")

        result = self._run_installer(None)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("valid SHA-256 checksum", result.stderr)
        self.assertEqual(installed.read_bytes(), b"existing-ego-lite-bridge\n")


if __name__ == "__main__":
    unittest.main()
