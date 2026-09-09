from __future__ import annotations

import errno
import fcntl
import hashlib
import io
import json
import os
import select
import shlex
import shutil
import signal
import subprocess
import sys
import tarfile
import tempfile
import termios
import time
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
    "ln",
    "mkdir",
    "mktemp",
    "mv",
    "readlink",
    "rm",
)
FAKE_COMMANDS = {"curl"}
AGENT_SIGNALS = (
    "AI_AGENT", "CURSOR_AGENT", "GEMINI_CLI", "CODEX_SANDBOX", "CODEX_CI",
    "CODEX_THREAD_ID", "ANTIGRAVITY_AGENT", "AUGMENT_AGENT", "OPENCODE_CLIENT",
    "CLAUDECODE", "CLAUDE_CODE", "REPL_ID", "COPILOT_MODEL", "COPILOT_ALLOW_ALL",
    "COPILOT_GITHUB_TOKEN",
)
PROMPT = b"Install the ego-browser skill for your agent? (recommended) [Y/n] "


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
url=""
previous=""
for argument in "$@"; do
  case "$argument" in https://*) url="$argument" ;; esac
  if [ "$previous" = "-o" ]; then out="$argument"; fi
  previous="$argument"
done
if [ -n "$out" ]; then
  printf '%s\\n' "$out" >> "$FAKE_OUTPUT_LOG"
  if [ -n "${FAKE_URL_LOG:-}" ]; then
    printf '%s\\n' "$url" >> "$FAKE_URL_LOG"
  fi
  case "$url" in
    */ego-browser-skill.tgz)
      [ -z "${FAKE_DOWNLOAD_FAIL:-}" ] || exit 1
      cp "$FAKE_SKILL_ARCHIVE" "$out" ;;
    *) cp "$FAKE_PAYLOAD" "$out" ;;
  esac
else
  cat "$FAKE_MANIFEST"
fi
""",
        )

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

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
        env = {
            key: value for key, value in os.environ.items()
            if key not in AGENT_SIGNALS
            and not key.startswith(("CURSOR_", "XDG_", "FAKE_"))
        }
        return {
            **env,
            "HOME": str(self.root / "home"),
            "XDG_CONFIG_HOME": str(self.root / "config"),
            "XDG_DATA_HOME": str(self.root / "data"),
            "XDG_STATE_HOME": str(self.root / "state"),
            "XDG_CACHE_HOME": str(self.root / "cache"),
            "PATH": str(self.bin_dir),
            "FAKE_MANIFEST": str(manifest),
            "FAKE_PAYLOAD": str(self.payload),
            "FAKE_OUTPUT_LOG": str(self.root / "output-path"),
            "EGO_LITE_BRIDGE_INSTALL_DIR": str(self.install_dir),
            "EGO_LITE_BRIDGE_MANIFEST_URL": "https://example.invalid/latest.json",
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
            stdin=subprocess.DEVNULL,
            start_new_session=True,
            timeout=10,
            text=True,
            check=False,
        )

    def _skill_env(self, **manifest_options: object) -> dict[str, str]:
        env = self._installer_env(self.expected_sha256, **manifest_options)
        for command in ("tar", "gzip"):
            path = shutil.which(command)
            if path is None:
                self.fail(f"test host is missing {command}")
            (self.bin_dir / command).unlink(missing_ok=True)
            (self.bin_dir / command).symlink_to(path)
        archive = self.root / "skill.tgz"
        with tarfile.open(archive, "w:gz") as tar:
            for name in ("ego-browser/SKILL.md", "ego-browser/references/install.md"):
                data = b"fixture skill\n"
                member = tarfile.TarInfo(name)
                member.size = len(data)
                tar.addfile(member, io.BytesIO(data))
        manifest = json.loads(Path(env["FAKE_MANIFEST"]).read_text())
        manifest["skill_url"] = (
            "https://github.com/imleon/ego-lite-bridge/releases/download/"
            f"v{manifest['version']}/ego-browser-skill.tgz"
        )
        manifest["skill_sha256"] = hashlib.sha256(archive.read_bytes()).hexdigest().upper()
        Path(env["FAKE_MANIFEST"]).write_text(json.dumps(manifest, indent=2))
        env.update({
            "FAKE_SKILL_ARCHIVE": str(archive),
            "FAKE_CALL_LOG": str(self.root / "skill-calls.jsonl"),
            "FAKE_URL_LOG": str(self.root / "download-urls"),
        })
        # Both tools are mocks: never run node/npm or write agent directories.
        self._write_executable("node", f"""#!{sys.executable}
import json, os, sys
from pathlib import Path
with open(os.environ['FAKE_CALL_LOG'], 'a') as log:
    log.write(json.dumps({{'tool': 'node', 'args': sys.argv[1:],
                          'stdio': [os.isatty(fd) for fd in range(3)]}}) + '\\n')
assert Path(os.environ['EGO_LITE_BRIDGE_INSTALL_DIR'], 'ego-lite-bridge').is_file()
assert sys.stdin.read() == ''
print(os.environ.get('FAKE_NODE_VERSION', 'v22.20.0'))
sys.exit(int(os.environ.get('FAKE_NODE_EXIT', '0')))
""")
        self._write_executable("npx-python", f"""#!{sys.executable}
import json, os, signal, sys
from pathlib import Path
assert signal.getsignal(signal.SIGTTIN) == signal.SIG_DFL, 'SIGTTIN leaked into npx'
source = Path(sys.argv[4])
with open(os.environ['FAKE_CALL_LOG'], 'a') as log:
    log.write(json.dumps({{'tool': 'npx', 'args': sys.argv[1:],
                          'stdio': [os.isatty(fd) for fd in range(3)],
                          'trace': os.environ.get('CURSOR_TRACE_ID'),
                          'skill': (source / 'SKILL.md').read_text()}}) + '\\n')
if os.environ.get('FAKE_NPX_READ'):
    print('NPX input: ', end='', flush=True)
    assert input() == 'from-terminal'
print('Installation cancelled' if os.environ.get('FAKE_NPX_CANCEL') else 'Mock CLI ended')
sys.exit(int(os.environ.get('FAKE_NPX_EXIT', '0')))
""")
        self._write_executable("npx", """#!/bin/sh
sh -c 'trap - PIPE; kill -PIPE "$$"; exit 99' 2>/dev/null
[ "$?" -ne 99 ] || { echo 'SIGPIPE leaked into npx' >&2; exit 1; }
exec npx-python "$@"
""")
        return env

    def _run_pty(
        self, env: dict[str, str], exchanges: list[tuple[bytes, bytes]],
        *, script: str | None = None,
    ) -> subprocess.CompletedProcess[str]:
        master, slave = os.openpty()

        def attach_terminal() -> None:
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

        # Capture bridge output separately: the optional CLI must explicitly use
        # /dev/tty even when stdin is a script and stdout/stderr are redirected.
        with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
            process = subprocess.Popen(
                ["/bin/sh"] if script is not None else ["/bin/sh", str(INSTALLER)],
                stdin=subprocess.PIPE if script is not None else slave,
                stdout=stdout, stderr=stderr, env=env,
                preexec_fn=attach_terminal, pass_fds=(slave,),
            )
            os.close(slave)
            output = bytearray()
            pending = list(exchanges)
            consumed = 0
            deadline = time.monotonic() + 10
            try:
                if script is not None:
                    process.stdin.write(script.encode())
                    process.stdin.close()
                while True:
                    if time.monotonic() >= deadline:
                        self.fail(f"installer PTY timed out: {output.decode(errors='replace')}")
                    if select.select([master], [], [], 0.05)[0]:
                        try:
                            chunk = os.read(master, 65536)
                        except OSError as error:
                            if error.errno != errno.EIO:
                                raise
                            break
                        if not chunk:
                            break
                        output.extend(chunk)
                        if pending:
                            expected, reply = pending[0]
                            index = output.find(expected, consumed)
                            if index >= 0:
                                consumed = index + len(expected)
                                os.write(master, reply)
                                pending.pop(0)
                    elif process.poll() is not None:
                        break
                process.wait(timeout=max(0.1, deadline - time.monotonic()))
            finally:
                # Reap the whole session even on timeout/assertion failure.
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait(timeout=5)
                os.close(master)
            stdout.seek(0)
            stderr.seek(0)
            result = subprocess.CompletedProcess(
                process.args, process.returncode,
                output.decode(errors="replace") + stdout.read().decode(),
                stderr.read().decode(),
            )
            self.assertFalse(pending, result.stdout + result.stderr)
            return result

    def _assert_bridge_installed(self) -> None:
        self.assertEqual((self.install_dir / "ego-lite-bridge").read_bytes(), self.payload.read_bytes())
        self.assertEqual(os.readlink(self.install_dir / "ego-browser"), "ego-lite-bridge")
        self.assertEqual(list(self.install_dir.glob(".ego-lite-bridge.*")), [])

    def test_skill_confirmation_and_native_cli_stdio(self) -> None:
        for answer, piped in ((b"\n", False), (b"y\n", True), (b"YeS\n", False)):
            with self.subTest(answer=answer, piped=piped):
                env = self._skill_env(version="8.7.6")
                calls_path = Path(env["FAKE_CALL_LOG"])
                calls_path.unlink(missing_ok=True)
                env["FAKE_NPX_READ"] = "1"
                script = INSTALLER.read_text() + "\nprintf 'SCRIPT_INTACT\\n'\n" if piped else None
                result = self._run_pty(env, [(PROMPT, answer), (b"NPX input: ", b"from-terminal\n")], script=script)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self._assert_bridge_installed()
                self.assertEqual(Path(env["FAKE_URL_LOG"]).read_text().splitlines()[-2:], [
                    "https://github.com/imleon/ego-lite-bridge/releases/download/v8.7.6/ego-lite-bridge-linux-x86_64",
                    "https://github.com/imleon/ego-lite-bridge/releases/download/v8.7.6/ego-browser-skill.tgz",
                ])
                calls = [json.loads(line) for line in calls_path.read_text().splitlines()]
                self.assertEqual([call["tool"] for call in calls], ["node", "npx"])
                self.assertEqual(calls[0]["args"], ["--version"])
                self.assertEqual(calls[0]["stdio"], [False, False, False])
                call = calls[1]
                args = call["args"]
                self.assertEqual(args[:3], ["--yes", "skills@1.5.24", "add"])
                self.assertEqual(args[4:], ["--skill", "ego-browser", "--global", "--copy"])
                self.assertEqual(call["stdio"], [True, True, True])
                self.assertEqual(call["skill"], "fixture skill\n")
                self.assertFalse(Path(args[3]).exists())
                if piped:
                    self.assertIn("SCRIPT_INTACT", result.stdout)

    def test_non_tty_does_not_run_present_skill_tooling(self) -> None:
        env = self._skill_env()
        result = subprocess.run(
            ["/bin/sh", str(INSTALLER)], env=env, stdin=subprocess.DEVNULL,
            start_new_session=True, capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(Path(env["FAKE_CALL_LOG"]).exists())
        self.assertEqual(len((self.root / "output-path").read_text().splitlines()), 1)
        self.assertIn("#optional-agent-skill-installation", result.stdout)
        self._assert_bridge_installed()

    def test_background_tty_skips_without_stopping_or_skill_tooling(self) -> None:
        env = self._skill_env()
        # Keep a live foreground parent in the same session, so the background
        # job (including confirm_skill's subshell) is not an orphaned group.
        supervisor = f"""
import os, signal, sys

def timeout(signum, frame):
    raise TimeoutError('background installer timed out')

tty = os.open('/dev/tty', os.O_RDWR)
assert os.tcgetpgrp(tty) == os.getpgrp()
signal.signal(signal.SIGTTIN, signal.SIG_DFL)
signal.pthread_sigmask(signal.SIG_UNBLOCK, {{signal.SIGTTIN}})
job = os.fork()
if job == 0:
    os.setpgid(0, 0)
    assert os.tcgetpgrp(tty) != os.getpgrp()
    assert os.getsid(0) == os.getsid(os.getppid())
    os.close(tty)
    os.execv('/bin/sh', ['/bin/sh', {str(INSTALLER)!r}])
os.close(tty)
reaped = False
try:
    signal.signal(signal.SIGALRM, timeout)
    signal.alarm(5)
    _, status = os.waitpid(job, os.WUNTRACED)
    if os.WIFSTOPPED(status):
        raise RuntimeError('background installer stopped by ' + signal.Signals(os.WSTOPSIG(status)).name)
    reaped = True
    sys.exit(os.waitstatus_to_exitcode(status))
finally:
    signal.alarm(0)
    try:
        os.killpg(job, signal.SIGKILL)
    except ProcessLookupError:
        pass
    if not reaped:
        os.waitpid(job, 0)
"""
        script = f"exec {shlex.quote(sys.executable)} -c {shlex.quote(supervisor)}\n"
        result = self._run_pty(env, [], script=script)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("ego-browser skill was not installed automatically", result.stdout)
        self.assertIn("#optional-agent-skill-installation", result.stdout)
        self.assertFalse(Path(env["FAKE_CALL_LOG"]).exists())
        self.assertEqual(Path(env["FAKE_URL_LOG"]).read_text().splitlines(), [
            "https://github.com/imleon/ego-lite-bridge/releases/download/v9.9.9/ego-lite-bridge-linux-x86_64",
        ])
        self._assert_bridge_installed()

    def test_no_and_eof_skip_without_skill_tooling(self) -> None:
        for reply in (b"n\n", b"NO\n", b"\x04"):
            with self.subTest(reply=reply):
                env = self._installer_env(self.expected_sha256)
                (self.root / "output-path").unlink(missing_ok=True)
                result = self._run_pty(env, [(PROMPT, reply)])
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self._assert_bridge_installed()
                self.assertIn("#optional-agent-skill-installation", result.stdout)
                self.assertEqual(len((self.root / "output-path").read_text().splitlines()), 1)
                self.assertFalse((self.root / "home").exists())

    def test_invalid_answer_reprompts(self) -> None:
        env = self._installer_env(self.expected_sha256)
        result = self._run_pty(env, [(PROMPT, b"maybe\n"), (PROMPT, b"n\n")])
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Please answer yes or no", result.stdout)
        self.assertEqual(result.stdout.count(PROMPT.decode()), 2)
        self._assert_bridge_installed()

    def test_agent_guard_skips_before_prompt_or_dependencies(self) -> None:
        cases = [{name: "0"} for name in AGENT_SIGNALS]
        cases += [
            {"CURSOR_EXTENSION_HOST_ROLE": "agent-exec"},
            {"AI_AGENT": "unknown", "CURSOR_AGENT": "1", "CURSOR_TRACE_ID": "trace"},
        ]
        for signals in cases:
            with self.subTest(signals=signals):
                env = self._installer_env(self.expected_sha256)
                env.update(signals)
                (self.root / "output-path").unlink(missing_ok=True)
                result = self._run_pty(env, [])
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertNotIn(PROMPT.decode(), result.stdout)
                self.assertIn("normal terminal", result.stdout)
                self.assertIn("#optional-agent-skill-installation", result.stdout)
                self.assertEqual(len((self.root / "output-path").read_text().splitlines()), 1)
                self._assert_bridge_installed()

    def test_devin_guard_without_creating_global_directory(self) -> None:
        env = self._installer_env(self.expected_sha256)
        devin = self.root / ".devin"
        devin.mkdir()
        script = INSTALLER.read_text().replace("[ -e /opt/.devin ]", f'[ -e "{devin}" ]')
        self.assertNotEqual(script, INSTALLER.read_text())
        result = self._run_pty(env, [], script=script)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("normal terminal", result.stdout)
        self.assertNotIn(PROMPT.decode(), result.stdout)
        self.assertIn("#optional-agent-skill-installation", result.stdout)

    def test_trace_only_does_not_block_or_clear_environment(self) -> None:
        env = self._skill_env()
        env.update({name: "" for name in AGENT_SIGNALS})
        env.update({"CURSOR_TRACE_ID": "trace-only", "CURSOR_EXTENSION_HOST_ROLE": "worker"})
        result = self._run_pty(env, [(PROMPT, b"\n")])
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        calls = [json.loads(line) for line in Path(env["FAKE_CALL_LOG"]).read_text().splitlines()]
        self.assertEqual(calls[-1]["trace"], "trace-only")

    def test_skill_dependency_and_cli_errors_keep_bridge(self) -> None:
        for case, message in (
            ("missing-node", "requires 'node'"), ("missing-npx", "requires 'npx'"),
            ("missing-tar", "requires 'tar'"), ("old-node", "requires Node.js 22.20.0"),
            ("invalid-node", "requires Node.js 22.20.0"), ("node-error", "requires Node.js 22.20.0"),
            ("npx-error", "skills CLI failed"),
        ):
            with self.subTest(case=case):
                env = self._skill_env()
                calls_path = Path(env["FAKE_CALL_LOG"])
                calls_path.unlink(missing_ok=True)
                if case.startswith("missing-"):
                    (self.bin_dir / case.removeprefix("missing-")).unlink()
                elif case == "old-node":
                    env["FAKE_NODE_VERSION"] = "v22.19.9"
                elif case == "invalid-node":
                    env["FAKE_NODE_VERSION"] = "nonsense"
                elif case == "node-error":
                    env["FAKE_NODE_EXIT"] = "1"
                else:
                    env["FAKE_NPX_EXIT"] = "7"
                result = self._run_pty(env, [(PROMPT, b"yes\n")])
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn(message, result.stdout)
                self.assertIn("bridge installed; optional ego-browser skill installation incomplete", result.stdout)
                self.assertIn("not rolled back", result.stdout)
                self._assert_bridge_installed()
                if case != "npx-error" and calls_path.exists():
                    self.assertNotIn('"tool": "npx"', calls_path.read_text())

    def test_upstream_cancel_zero_is_not_reported_as_success(self) -> None:
        env = self._skill_env()
        env["FAKE_NPX_CANCEL"] = "1"
        result = self._run_pty(env, [(PROMPT, b"\n")])
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Installation cancelled", result.stdout)
        self.assertIn("skill interactive flow ended; refer to the skills CLI output", result.stdout)
        self.assertNotIn("skill installed", result.stdout)
        self._assert_bridge_installed()

    def test_skill_manifest_download_and_archive_failures(self) -> None:
        cases = (
            "wrong-version", "untrusted-url", "missing-sha", "invalid-sha", "mismatch",
            "download", "corrupt", "empty", "absolute", "traversal", "wrong-root",
            "symlink", "hardlink", "fifo", "missing-files", "mkdir", "extract", "checksum-error",
        )
        for case in cases:
            with self.subTest(case=case):
                shutil.rmtree(self.install_dir, ignore_errors=True)
                env = self._skill_env()
                archive = Path(env["FAKE_SKILL_ARCHIVE"])
                calls_path = Path(env["FAKE_CALL_LOG"])
                calls_path.unlink(missing_ok=True)
                manifest_path = Path(env["FAKE_MANIFEST"])
                manifest = json.loads(manifest_path.read_text())
                if case == "wrong-version":
                    manifest["skill_url"] = manifest["skill_url"].replace("v9.9.9", "v8.8.8")
                elif case == "untrusted-url":
                    manifest["skill_url"] = "https://example.invalid/ego-browser-skill.tgz"
                elif case == "missing-sha":
                    del manifest["skill_sha256"]
                elif case == "invalid-sha":
                    manifest["skill_sha256"] = "z" * 64
                elif case == "mismatch":
                    manifest["skill_sha256"] = "0" * 64
                elif case == "download":
                    env["FAKE_DOWNLOAD_FAIL"] = "1"
                elif case in ("mkdir", "extract", "checksum-error"):
                    command = {"mkdir": "mkdir", "extract": "tar", "checksum-error": "sha256sum"}[case]
                    real_command = os.readlink(self.bin_dir / command)
                    (self.bin_dir / command).unlink()
                    # Only fail the optional phase, never bridge installation.
                    condition = {"mkdir": '[ "$1" != "-p" ]', "extract": '[ "$1" = "-xzf" ]',
                                 "checksum-error": '[ -e "$EGO_LITE_BRIDGE_INSTALL_DIR/ego-lite-bridge" ]'}[case]
                    failed_output = f'{real_command} "$@"; ' if case == "checksum-error" else ""
                    self._write_executable(command, f'#!/bin/sh\nif {condition}; then {failed_output}exit 1; fi\nexec {real_command} "$@"\n')
                else:
                    if case == "corrupt":
                        archive.write_bytes(b"not a tar file")
                    else:
                        with tarfile.open(archive, "w:gz") as tar:
                            if case != "empty":
                                member = tarfile.TarInfo({
                                    "absolute": "/ego-browser/SKILL.md",
                                    "traversal": "ego-browser/../escape",
                                    "wrong-root": "other/SKILL.md",
                                }.get(case, "ego-browser/SKILL.md"))
                                member.type = {"symlink": tarfile.SYMTYPE, "hardlink": tarfile.LNKTYPE,
                                               "fifo": tarfile.FIFOTYPE}.get(case, tarfile.REGTYPE)
                                member.linkname = "../../escape" if case in ("symlink", "hardlink") else ""
                                tar.addfile(member, io.BytesIO(b""))
                    manifest["skill_sha256"] = hashlib.sha256(archive.read_bytes()).hexdigest()
                manifest_path.write_text(json.dumps(manifest, indent=2))
                result = self._run_pty(env, [(PROMPT, b"\n")])
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("bridge installed; optional ego-browser skill installation incomplete", result.stdout)
                self._assert_bridge_installed()
                self.assertNotIn('"tool": "npx"', calls_path.read_text())
                if case in ("mkdir", "extract", "checksum-error"):
                    (self.bin_dir / command).unlink()
                    (self.bin_dir / command).symlink_to(real_command)

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

    def test_linux_installs_without_skill_tooling(self) -> None:
        result = self._run_installer(self.expected_sha256)

        self.assertEqual(result.returncode, 0, result.stderr)
        downloads = (self.root / "output-path").read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(downloads), 1)
        self.assertTrue(downloads[0].endswith("/ego-lite-bridge"), downloads[0])
        self.assertIn("ego-browser skill was not installed automatically", result.stdout)
        self.assertIn(
            "https://github.com/imleon/ego-lite-bridge#optional-agent-skill-installation",
            result.stdout,
        )

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

    def test_macos_installs_no_shim_or_linux_skill_notice(self) -> None:
        env = self._installer_env(self.expected_sha256, os_name="macos")

        result = self._run_pty(env, [])
        self.assertNotIn(PROMPT.decode(), result.stdout)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            (self.install_dir / "ego-lite-bridge").read_bytes(),
            self.payload.read_bytes(),
        )
        self.assertFalse((self.install_dir / "ego-browser").exists())
        self.assertNotIn("ego-browser skill was not installed automatically", result.stdout)

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
