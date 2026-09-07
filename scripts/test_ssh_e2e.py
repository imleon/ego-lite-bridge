#!/usr/bin/env python3
"""Destructive, real Mac -> SSH -> Linux end-to-end gate.

Stop the normal Mac daemon before running through ``just ssh-e2e``. The test
daemon uses an isolated temporary HOME; the harness owns only objects it creates
and refuses cleanup when an owned Linux install target has changed underneath it.
"""

from __future__ import annotations

import os
import platform
import shlex
import signal
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import unittest
from pathlib import Path


TARGET_ENV = "EGO_LITE_BRIDGE_SSH_TARGET"
ALIAS_ENV = "EGO_LITE_BRIDGE_SSH_ALIAS"
ROOT = Path(__file__).resolve().parents[1]
DEADLINE = 45
FAKE = r'''#!/usr/bin/env python3
import os, signal, sys, time

args = sys.argv[1:]
mode = args.pop(0) if args else "echo"
if mode == "echo":
    for arg in args:
        data = os.fsencode(arg)
        sys.stdout.buffer.write(len(data).to_bytes(4, "big") + data)
    sys.stdout.buffer.write(sys.stdin.buffer.read())
elif mode == "streams":
    data = sys.stdin.buffer.read()
    sys.stdout.buffer.write(b"OUT\x00\xff" + data)
    sys.stderr.buffer.write(b"ERR\x00\xfe" + data[::-1])
elif mode == "exit":
    sys.exit(int(args[0]))
elif mode == "signal":
    os.kill(os.getpid(), int(args[0]))
elif mode == "block":
    ready, release, pidfile = map(os.fsencode, args)
    with open(pidfile, "w") as f:
        f.write(str(os.getpid()))
    open(ready, "wb").close()
    while not os.path.exists(release):
        time.sleep(.02)
elif mode == "slow-read":
    ready = os.fsencode(args[0])
    open(ready, "wb").close()
    while sys.stdin.buffer.read(4096):
        time.sleep(.002)
elif mode == "flood":
    size = int(args[0])
    chunk = b"o" * 65536
    err = b"e" * 65536
    while size:
        take = min(size, len(chunk))
        sys.stdout.buffer.write(chunk[:take])
        sys.stderr.buffer.write(err[:take])
        size -= take
else:
    raise SystemExit("unknown fake mode")
'''

REMOTE_INSTALL = r'''
set -eu
TOKEN=$1
BUILD=$2
BIN=$HOME/.local/bin/ego-lite-bridge
SHIM=$HOME/.local/bin/ego-browser
STATE=$BUILD/install-state
[ ! -e "$STATE" ]
if [ -d "$HOME/.local/bin" ] && [ ! -L "$HOME/.local/bin" ]; then
  printf '1\n' > "$BUILD/bin-dir.existed"
elif [ ! -e "$HOME/.local/bin" ] && [ ! -L "$HOME/.local/bin" ]; then
  printf '0\n' > "$BUILD/bin-dir.existed"
else
  exit 69
fi
printf '%s\n' "$TOKEN" > "$STATE"
mkdir -p "$HOME/.local/bin"
for pair in "binary:$BIN" "shim:$SHIM"; do
  name=${pair%%:*}; path=${pair#*:}
  if [ -d "$path" ]; then
    exit 68
  elif [ -e "$path" ] || [ -L "$path" ]; then
    cp -a "$path" "$BUILD/$name.backup"
    printf '1\n' > "$BUILD/$name.existed"
  else
    printf '0\n' > "$BUILD/$name.existed"
  fi
done
BIN_TMP=$BIN.tmp-$TOKEN
SHIM_TMP=$SHIM.tmp-$TOKEN
rm -f "$BIN_TMP" "$SHIM_TMP"
cp "$BUILD/source/target/release/ego-lite-bridge" "$BIN_TMP"
chmod 755 "$BIN_TMP"
ln -s ego-lite-bridge "$SHIM_TMP"
stat -c '%d:%i' "$BIN_TMP" > "$BUILD/binary.identity"
stat -c '%d:%i' "$SHIM_TMP" > "$BUILD/shim.identity"
printf '1\n' > "$BUILD/binary.installed"
mv -fT "$BIN_TMP" "$BIN"
printf '1\n' > "$BUILD/shim.installed"
mv -fT "$SHIM_TMP" "$SHIM"
'''

REMOTE_RESTORE = r'''
set -u
TOKEN=$1
BUILD=$2
EXPECTED=$3
BIN=$HOME/.local/bin/ego-lite-bridge
SHIM=$HOME/.local/bin/ego-browser
[ "$(stat -c '%d:%i:%f:%u:%a' "$BUILD" 2>/dev/null)" = "$EXPECTED" ] || { echo 'build directory changed' >&2; exit 69; }
if [ ! -e "$BUILD/install-state" ] && [ ! -L "$BUILD/install-state" ]; then
  rm -rf "$BUILD"
  exit
fi
[ -f "$BUILD/install-state" ] && [ ! -L "$BUILD/install-state" ] && [ "$(cat "$BUILD/install-state")" = "$TOKEN" ] || { echo 'install marker changed' >&2; exit 70; }
failed=0
for pair in "binary:$BIN" "shim:$SHIM"; do
  name=${pair%%:*}; path=${pair#*:}
  [ -f "$BUILD/$name.installed" ] || continue
  identity=$(cat "$BUILD/$name.identity" 2>/dev/null) || { failed=1; continue; }
  if [ "$(stat -c '%d:%i' "$path" 2>/dev/null)" = "$identity" ]; then
    rm -f "$path"
    if [ "$(cat "$BUILD/$name.existed")" = 1 ]; then
      cp -a "$BUILD/$name.backup" "$path" || { failed=1; continue; }
    fi
    rm -f "$BUILD/$name.installed"
  elif [ "$(stat -c '%d:%i' "$path.tmp-$TOKEN" 2>/dev/null)" = "$identity" ]; then
    rm -f "$path.tmp-$TOKEN" "$BUILD/$name.installed"
  else
    echo "installed $name changed; refusing its restore" >&2
    failed=1
  fi
done
[ "$(cat "$BUILD/bin-dir.existed")" = 1 ] || rmdir "$HOME/.local/bin" || failed=1
[ "$failed" = 0 ] || exit 71
rm -rf "$BUILD"
'''


class SshE2E(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.target = os.environ.get(TARGET_ENV, "")
        if not cls.target:
            raise RuntimeError(f"{TARGET_ENV} is required")
        if platform.system() != "Darwin":
            raise RuntimeError("SSH E2E must run on macOS")
        cls.binary = (ROOT / "target/release/ego-lite-bridge").resolve()
        if not cls.binary.is_file() or not os.access(cls.binary, os.X_OK):
            raise RuntimeError(f"Mac bridge binary is not executable: {cls.binary}")
        cls.token = f"ssh-e2e-{os.getpid()}-{time.time_ns()}"
        cls.remote_name = cls.token[:64]
        cls.remote_dir = ""
        cls.remote_dir_identity = ""
        cls.remote_install_attempted = False
        cls.remote_installed = False
        cls.remote_add_attempted = False
        cls.remote_endpoint_existed = False
        cls.remote_endpoint_identity = ""
        cls.remote_runtime_identity = ""
        cls.remote_state_dirs_existed: list[bool] = []
        cls.daemon: subprocess.Popen[bytes] | None = None
        cls.added: set[str] = set()
        cls.temp = tempfile.TemporaryDirectory(prefix="elb-", dir="/tmp")
        cls.work = Path(cls.temp.name)
        cls.mac_home = cls.work / "home"
        cls.mac_home.mkdir(mode=0o700)
        user_ssh = Path.home() / ".ssh"
        if user_ssh.is_dir():
            (cls.mac_home / ".ssh").symlink_to(user_ssh, target_is_directory=True)
        cls.mac_env = os.environ.copy()
        cls.mac_env["HOME"] = str(cls.mac_home)
        cls.fake_dir = cls.work / "fake-bin"
        cls.fake_dir.mkdir()
        cls.fake = cls.fake_dir / "ego-browser"
        cls.fake.write_text(FAKE.replace("#!/usr/bin/env python3", f"#!{sys.executable}", 1))
        cls.fake.chmod(0o700)
        cls.app_dir = cls.mac_home / "Library/Application Support/ego-lite-bridge"
        try:
            cls.preflight()
            cls.build_and_install_remote()
        except BaseException:
            cls.cleanup()
            raise

    @classmethod
    def tearDownClass(cls) -> None:
        cls.cleanup()

    @classmethod
    def run_command(
        cls,
        argv: list[str | bytes],
        *,
        input: bytes | None = None,
        timeout: float = DEADLINE,
        check: bool = True,
        env: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess[bytes]:
        result = subprocess.run(
            argv,
            input=input,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
            env=env,
            check=False,
        )
        if check and result.returncode != 0:
            raise AssertionError(
                f"command failed ({result.returncode}): {argv!r}\n"
                f"stdout: {result.stdout.decode(errors='replace')}\n"
                f"stderr: {result.stderr.decode(errors='replace')}"
            )
        return result

    @classmethod
    def bridge(cls, *args: str, **kwargs: object) -> subprocess.CompletedProcess[bytes]:
        kwargs.setdefault("env", cls.mac_env)
        return cls.run_command([str(cls.binary), *args], **kwargs)

    @classmethod
    def start_bridge(cls) -> None:
        log_path = cls.work / "daemon.log"
        with log_path.open("ab") as log:
            cls.daemon = subprocess.Popen(
                [str(cls.binary), "daemon", "--ego-browser", str(cls.fake)],
                env=cls.mac_env,
                stdout=log,
                stderr=log,
            )

        def ready() -> bool:
            assert cls.daemon is not None
            if cls.daemon.poll() is not None:
                detail = log_path.read_text(errors="replace")
                raise AssertionError(
                    f"daemon exited during startup ({cls.daemon.returncode}):\n{detail}"
                )
            return cls.bridge("status", check=False).returncode == 0

        cls.wait_for(ready, "daemon did not start")

    @classmethod
    def stop_bridge(cls, sig: signal.Signals = signal.SIGTERM) -> None:
        daemon = cls.daemon
        if daemon is None:
            return
        if daemon.poll() is None:
            daemon.send_signal(sig)
        try:
            daemon.communicate(timeout=15)
        except subprocess.TimeoutExpired:
            daemon.kill()
            daemon.communicate()
        cls.daemon = None

    @classmethod
    def ssh_argv(cls, target: str | None = None) -> list[str]:
        return [
            "ssh",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            target or cls.target,
        ]

    @classmethod
    def ssh(
        cls, command: str, *args: str, target: str | None = None, **kwargs: object
    ) -> subprocess.CompletedProcess[bytes]:
        quoted = " ".join(shlex.quote(arg) for arg in args)
        remote = command + (" " + quoted if quoted else "")
        return cls.run_command([*cls.ssh_argv(target), remote], **kwargs)

    @classmethod
    def preflight(cls) -> None:
        status = cls.run_command([str(cls.binary), "status"], check=False)
        if status.returncode == 0:
            raise RuntimeError("the real bridge daemon is already running; stop it before testing")
        loaded = cls.run_command(
            ["launchctl", "print", f"gui/{os.getuid()}/com.github.imleon.ego-lite-bridge"],
            check=False,
        )
        if loaded.returncode == 0:
            raise RuntimeError("the bridge LaunchAgent is loaded; stop it before testing")
        probe = cls.ssh(
            "sh",
            "-c",
            "mv --help 2>&1 | grep -q -- '--no-target-directory' || exit 73; "
            "printf '%s\\n' \"$HOME\"; uname -s; command -v cargo; id -u; "
            "d=/tmp/ego-lite-bridge-$(id -u); "
            "[ ! -e \"$d\" ] && [ ! -L \"$d\" ] || exit 74; "
            "for p in \"$HOME/.local\" \"$HOME/.local/state\" "
            "\"$HOME/.local/state/ego-lite-bridge\"; do "
            "if [ -d \"$p\" ] && [ ! -L \"$p\" ]; then echo 1; "
            "elif [ ! -e \"$p\" ] && [ ! -L \"$p\" ]; then echo 0; else exit 75; fi; done; "
            "p=$HOME/.local/state/ego-lite-bridge/endpoint-id; "
            "if [ -e \"$p\" ] || [ -L \"$p\" ]; then "
            "echo \"1 $(stat -c '%d:%i:%u:%a:%s:%Y' \"$p\") $(cksum \"$p\")\"; "
            "else echo 0; fi",
        )
        lines = probe.stdout.decode().splitlines()
        if len(lines) != 8 or not lines[0].startswith("/") or lines[1] != "Linux":
            raise RuntimeError(f"invalid Linux preflight response: {lines!r}")
        cls.remote_home = lines[0]
        cls.remote_uid = int(lines[3])
        cls.remote_state_dirs_existed = [line == "1" for line in lines[4:7]]
        if any(line not in {"0", "1"} for line in lines[4:7]):
            raise RuntimeError(f"invalid remote state snapshot: {lines[4:7]!r}")
        cls.remote_endpoint_existed = lines[7].startswith("1 ")
        cls.remote_endpoint_identity = lines[7][2:] if cls.remote_endpoint_existed else ""
        if not cls.remote_endpoint_existed and lines[7] != "0":
            raise RuntimeError(f"invalid endpoint-id snapshot: {lines[7]!r}")

    @classmethod
    def build_and_install_remote(cls) -> None:
        made = cls.ssh(
            "sh",
            "-c",
            f'd=$(mktemp -d "${{TMPDIR:-/tmp}}/{cls.token}.XXXXXX") && '
            'printf "%s\\n" "$d" && stat -c "%d:%i:%f:%u:%a" "$d"',
        )
        lines = made.stdout.decode().splitlines()
        if len(lines) != 2 or not lines[0].startswith("/"):
            raise RuntimeError(f"invalid remote mktemp response: {lines!r}")
        cls.remote_dir, cls.remote_dir_identity = lines
        extract = subprocess.Popen(
            [*cls.ssh_argv(), "tar -x -C " + shlex.quote(cls.remote_dir)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        assert extract.stdin is not None
        with tarfile.open(fileobj=extract.stdin, mode="w|") as archive:
            for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "src", "tests/fixtures"):
                archive.add(ROOT / name, arcname=f"source/{name}")
        extract.stdin.close()
        extract.stdin = None
        stdout, stderr = extract.communicate(timeout=DEADLINE)
        if extract.returncode != 0:
            raise RuntimeError(
                "remote tar extraction failed: "
                + (stderr or stdout).decode(errors="replace")
            )
        cls.ssh(
            "sh",
            "-c",
            f"cd {shlex.quote(cls.remote_dir)}/source && cargo build --release --locked",
            timeout=600,
        )
        cls.remote_install_attempted = True
        cls.ssh("sh", "-s", "--", cls.token, cls.remote_dir, input=REMOTE_INSTALL.encode())
        cls.remote_installed = True

    @classmethod
    def cleanup(cls) -> None:
        errors: list[str] = []
        if getattr(cls, "remote_add_attempted", False):
            try:
                if cls.bridge(
                    "remote", "status", cls.remote_name, check=False, timeout=15
                ).returncode == 0:
                    cls.added.add(cls.remote_name)
                if not cls.remote_endpoint_existed and not cls.remote_endpoint_identity:
                    endpoint = cls.ssh(
                        "sh",
                        "-c",
                        "p=$HOME/.local/state/ego-lite-bridge/endpoint-id; "
                        "[ -f \"$p\" ] && [ ! -L \"$p\" ] || exit 1; "
                        "printf '%s ' \"$(stat -c '%d:%i:%u:%a:%s:%Y' \"$p\")\"; cksum \"$p\"",
                        check=False,
                        timeout=15,
                    )
                    if endpoint.returncode == 0:
                        cls.remote_endpoint_identity = endpoint.stdout.decode().strip()
            except BaseException as error:
                errors.append(f"inspect attempted add: {error}")
        for name in list(getattr(cls, "added", ())):
            try:
                result = cls.bridge("remote", "remove", name, check=False, timeout=15)
                if result.returncode != 0:
                    status = cls.bridge("remote", "status", name, check=False, timeout=15)
                    if status.returncode == 0:
                        errors.append(f"remove {name}: {result.stderr.decode(errors='replace')}")
                    else:
                        cls.added.discard(name)
                else:
                    cls.added.discard(name)
            except BaseException as error:
                errors.append(f"remove {name}: {error}")
        try:
            cls.stop_bridge()
        except BaseException as error:
            errors.append(f"stop daemon: {error}")
        endpoint_removed = False
        if getattr(cls, "remote_endpoint_identity", "") and not getattr(
            cls, "remote_endpoint_existed", False
        ):
            try:
                result = cls.ssh(
                    "sh",
                    "-c",
                    "p=$HOME/.local/state/ego-lite-bridge/endpoint-id; expected=$1; "
                    "actual=\"$(stat -c '%d:%i:%u:%a:%s:%Y' \"$p\" 2>/dev/null) $(cksum \"$p\" 2>/dev/null)\"; "
                    "[ \"$actual\" = \"$expected\" ] || exit 76; rm \"$p\"",
                    "sh",
                    cls.remote_endpoint_identity,
                    check=False,
                    timeout=15,
                )
                if result.returncode != 0:
                    errors.append("remote endpoint-id changed; refusing removal")
                else:
                    endpoint_removed = True
                    cls.remote_endpoint_identity = ""
            except BaseException as error:
                errors.append(f"remote endpoint-id cleanup: {error}")
        if getattr(cls, "remote_install_attempted", False):
            try:
                runtime = cls.ssh(
                    "sh",
                    "-c",
                    "d=/tmp/ego-lite-bridge-$(id -u); expected=$1; "
                    "if [ -e \"$d\" ] || [ -L \"$d\" ]; then "
                    "[ -n \"$expected\" ] && [ \"$(stat -c '%d:%i:%f:%u:%a' \"$d\" 2>/dev/null)\" = \"$expected\" ] "
                    "|| exit 72; "
                    "for p in \"$d/broker.sock\" \"$d/owner.sock\"; do "
                    "[ ! -e \"$p\" ] && [ ! -S \"$p\" ] || exit 73; done; "
                    "if [ -e \"$d/acquire.lock\" ] || [ -L \"$d/acquire.lock\" ]; then "
                    "[ ! -L \"$d/acquire.lock\" ] && [ -f \"$d/acquire.lock\" ] "
                    "&& [ \"$(stat -c '%u:%a' \"$d/acquire.lock\")\" = \"$(id -u):600\" ] "
                    "|| exit 74; rm \"$d/acquire.lock\" || exit 75; fi; rmdir \"$d\" || exit 76; fi",
                    "sh",
                    cls.remote_runtime_identity,
                    check=False,
                    timeout=15,
                )
                if runtime.returncode != 0:
                    raise AssertionError(
                        "remote runtime is still active or changed; refusing install restore"
                    )
                cls.ssh(
                    "sh",
                    "-s",
                    "--",
                    cls.token,
                    cls.remote_dir,
                    cls.remote_dir_identity,
                    input=REMOTE_RESTORE.encode(),
                    timeout=30,
                )
                cls.remote_installed = False
                cls.remote_install_attempted = False
                cls.remote_dir = ""
            except BaseException as error:
                errors.append(f"remote restore: {error}")
        elif getattr(cls, "remote_dir", ""):
            try:
                cls.ssh(
                    "sh",
                    "-c",
                    "[ \"$(stat -c '%d:%i:%f:%u:%a' \"$1\" 2>/dev/null)\" = \"$2\" ] || exit 69; rm -rf -- \"$1\"",
                    "sh",
                    cls.remote_dir,
                    cls.remote_dir_identity,
                    timeout=15,
                )
                cls.remote_dir = ""
            except BaseException as error:
                errors.append(f"remote temporary directory cleanup: {error}")
        if endpoint_removed:
            try:
                result = cls.ssh(
                    "sh",
                    "-c",
                    "[ \"$1\" = 1 ] || rmdir \"$HOME/.local/state/ego-lite-bridge\" || exit 1; "
                    "[ \"$2\" = 1 ] || rmdir \"$HOME/.local/state\" || exit 1; "
                    "[ \"$3\" = 1 ] || rmdir \"$HOME/.local\" || exit 1",
                    "sh",
                    *("1" if existed else "0" for existed in reversed(cls.remote_state_dirs_existed)),
                    check=False,
                    timeout=15,
                )
                if result.returncode != 0:
                    errors.append("test-created remote state parent is not empty")
            except BaseException as error:
                errors.append(f"remote state directory cleanup: {error}")
        temp = getattr(cls, "temp", None)
        if temp is not None:
            temp.cleanup()
        if errors:
            raise AssertionError("cleanup failed:\n" + "\n".join(errors))

    @classmethod
    def shim_command(cls, *args: str) -> list[str]:
        command = "exec ~/.local/bin/ego-browser " + " ".join(shlex.quote(arg) for arg in args)
        return [*cls.ssh_argv(), command]

    @classmethod
    def shim(cls, *args: str, **kwargs: object) -> subprocess.CompletedProcess[bytes]:
        return cls.run_command(cls.shim_command(*args), **kwargs)

    @classmethod
    def wait_for(cls, predicate, message: str, timeout: float = 15) -> None:
        deadline = time.monotonic() + timeout
        last_error: BaseException | None = None
        while time.monotonic() < deadline:
            try:
                if predicate():
                    return
            except (OSError, subprocess.SubprocessError, AssertionError) as error:
                last_error = error
            time.sleep(0.02)
        detail = f": {last_error}" if last_error else ""
        raise AssertionError(message + detail)

    @classmethod
    def remote_details(cls, name: str | None = None) -> dict[str, str]:
        result = cls.bridge("remote", "status", name or cls.remote_name)
        return dict(line.split(": ", 1) for line in result.stdout.decode().splitlines())

    @classmethod
    def snapshot_remote_runtime(cls) -> None:
        cls.remote_runtime_identity = cls.ssh(
            "sh",
            "-c",
            "d=/tmp/ego-lite-bridge-$(id -u); "
            "stat -c '%d:%i:%f:%u:%a' \"$d\"",
        ).stdout.decode().strip()

    def test_ssh_e2e(self) -> None:
        self.lifecycle_and_crud()
        self.transparent_execution()
        self.capacity_cancel_and_backpressure()
        self.restart_dedupe_and_socket_modes()
        self.owner_conflict()
        self.disconnect_recovery()
        self.bridge("remote", "remove", self.remote_name)
        self.added.remove(self.remote_name)
        self.stop_bridge()

    def lifecycle_and_crud(self) -> None:
        self.start_bridge()
        self.remote_add_attempted = True
        added = self.bridge("remote", "add", self.remote_name, self.target)
        self.added.add(self.remote_name)
        self.remote_add_attempted = False
        self.remote_runtime_identity = self.ssh(
            "sh",
            "-c",
            "d=/tmp/ego-lite-bridge-$(id -u); stat -c '%d:%i:%f:%u:%a' \"$d\"",
        ).stdout.decode().strip()
        endpoint = self.ssh(
            "sh",
            "-c",
            "p=$HOME/.local/state/ego-lite-bridge/endpoint-id; "
            "echo \"$(stat -c '%d:%i:%u:%a:%s:%Y' \"$p\") $(cksum \"$p\")\"",
        ).stdout.decode().strip()
        if self.remote_endpoint_existed:
            self.assertEqual(endpoint, self.remote_endpoint_identity)
        else:
            self.remote_endpoint_identity = endpoint
        fields = added.stdout.decode().rstrip().split("\t")
        self.assertEqual(fields[1:], [self.remote_name, self.target, "desired=active observed=connected"])
        self.assertIn(f"daemon=running remotes=1", self.bridge("status").stdout.decode())
        self.assertEqual(self.remote_details()["active-requests"], "0/8")
        retry = self.bridge("remote", "retry", self.remote_name, check=False)
        self.assertNotEqual(retry.returncode, 0)
        before = (self.app_dir / "config.json").read_bytes()
        doctor = self.bridge("doctor", self.remote_name, check=False)
        self.assertEqual(doctor.returncode, 1)
        self.assertIn(b"FAIL mac.launchd:", doctor.stdout)
        self.assertIn(b"PASS mac.daemon:", doctor.stdout)
        self.assertIn(b"PASS mac.browser:", doctor.stdout)
        self.assertNotIn(b"FAIL remote.", doctor.stdout)
        self.assertEqual((self.app_dir / "config.json").read_bytes(), before)

    def transparent_execution(self) -> None:
        payload = bytes(range(256)) * 257
        result = self.shim("streams", input=payload)
        self.assertEqual(result.stdout, b"OUT\x00\xff" + payload)
        self.assertEqual(result.stderr, b"ERR\x00\xfe" + payload[::-1])
        result = self.shim("echo", "plain", "", input=b"stdin\x00\xff")
        self.assertEqual(result.stdout, b"\x00\x00\x00\x05plain\x00\x00\x00\x00stdin\x00\xff")
        non_utf8 = self.run_command(
            [
                *self.ssh_argv(),
                "exec ~/.local/bin/ego-browser echo \"$(printf '\\377')\"",
            ],
            input=b"",
        )
        self.assertEqual(non_utf8.stdout, b"\x00\x00\x00\x01\xff")
        self.assertEqual(self.shim("exit", "37", check=False).returncode, 37)
        signaled = self.ssh(
            "sh",
            "-c",
            "~/.local/bin/ego-browser signal \"$1\"; status=$?; "
            "[ \"$status\" -eq \"$2\" ] || { echo \"unexpected signal status: $status\" >&2; exit 1; }",
            "sh",
            str(signal.SIGTERM),
            str(128 + signal.SIGTERM),
            check=False,
        )
        self.assertEqual(signaled.returncode, 0, signaled.stderr.decode(errors="replace"))

    def capacity_cancel_and_backpressure(self) -> None:
        release = self.work / "release"
        processes: list[subprocess.Popen[bytes]] = []
        pidfiles: list[Path] = []
        try:
            for index in range(8):
                ready = self.work / f"ready-{index}"
                pidfile = self.work / f"pid-{index}"
                pidfiles.append(pidfile)
                process = subprocess.Popen(
                    self.shim_command("block", str(ready), str(release), str(pidfile)),
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                )
                processes.append(process)
                self.wait_for(ready.exists, f"request {index} did not become ready")
            self.assertEqual(self.remote_details()["active-requests"], "8/8")
            ninth = self.shim("exit", "0", check=False, timeout=10)
            self.assertNotEqual(ninth.returncode, 0)
            self.assertIn(b"capacity", ninth.stderr.lower())
            processes[0].terminate()
            processes[0].wait(timeout=10)
            child_pid = int(pidfiles[0].read_text())
            self.wait_for(
                lambda: subprocess.run(["kill", "-0", str(child_pid)], capture_output=True).returncode != 0,
                "cancelled Mac child survived",
            )
            release.touch()
            for process in processes[1:]:
                self.assertEqual(process.wait(timeout=10), 0)
            processes.clear()
            self.wait_for(
                lambda: self.remote_details()["active-requests"] == "0/8",
                "capacity did not recover after release",
            )
            ready = self.work / "slow-ready"
            slow = subprocess.Popen(
                self.shim_command("slow-read", str(ready)),
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            self.wait_for(ready.exists, "slow reader did not start")
            feed_error: list[BaseException] = []

            def feed_slow_reader() -> None:
                try:
                    slow.communicate(input=b"x" * (2 * 1024 * 1024), timeout=30)
                except BaseException as error:
                    feed_error.append(error)

            feeder = threading.Thread(target=feed_slow_reader)
            feeder.start()
            self.assertEqual(self.shim("exit", "0", timeout=10).returncode, 0)
            feeder.join(30)
            if feeder.is_alive():
                slow.kill()
                feeder.join()
                self.fail("slow reader did not finish before deadline")
            if feed_error:
                if slow.poll() is None:
                    slow.kill()
                slow.communicate()
                raise feed_error[0]
            self.assertEqual(slow.returncode, 0)
            flood = self.shim("flood", str(2 * 1024 * 1024), timeout=30)
            self.assertEqual(flood.stdout, b"o" * (2 * 1024 * 1024))
            self.assertEqual(flood.stderr, b"e" * (2 * 1024 * 1024))
        finally:
            release.touch()
            for process in processes:
                if process.poll() is None:
                    process.terminate()
                try:
                    process.communicate(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.communicate()
        self.wait_for(lambda: self.remote_details()["active-requests"] == "0/8", "capacity did not recover")

    def restart_dedupe_and_socket_modes(self) -> None:
        duplicate = self.bridge("remote", "add", self.remote_name + "-dup", self.target, check=False)
        self.assertNotEqual(duplicate.returncode, 0)
        self.assertIn(b"endpoint", duplicate.stderr.lower())
        alias = os.environ.get(ALIAS_ENV)
        if alias:
            duplicate = self.bridge("remote", "add", self.remote_name + "-alias", alias, check=False)
            self.assertNotEqual(duplicate.returncode, 0)
            self.assertIn(b"endpoint", duplicate.stderr.lower())
        modes = self.ssh(
            "sh",
            "-c",
            "d=/tmp/ego-lite-bridge-$(id -u); stat -c '%a:%u' \"$d\"; stat -c '%a:%u' \"$d/broker.sock\" \"$d/owner.sock\"",
        ).stdout.decode().splitlines()
        self.assertEqual(modes, [f"700:{self.remote_uid}", f"600:{self.remote_uid}", f"600:{self.remote_uid}"])
        self.stop_bridge(signal.SIGKILL)
        self.start_bridge()
        self.wait_for(
            lambda: self.remote_details()["observed"] == "connected",
            "remote did not recover after crash restart",
            30,
        )
        self.snapshot_remote_runtime()
        self.assertEqual(self.shim("exit", "0").returncode, 0)

    def owner_conflict(self) -> None:
        home = self.work / "claimant-home"
        home.mkdir()
        user_ssh = Path.home() / ".ssh"
        if user_ssh.is_dir():
            (home / ".ssh").symlink_to(user_ssh, target_is_directory=True)
        env = os.environ.copy()
        env["HOME"] = str(home)
        claimant = subprocess.Popen(
            [str(self.binary), "daemon", "--ego-browser", str(self.fake)],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        try:
            self.wait_for(
                lambda: self.bridge("status", env=env, check=False).returncode == 0,
                "claimant daemon did not start",
            )
            conflict = self.bridge(
                "remote", "add", "claimant", self.target, env=env, check=False, timeout=35
            )
            self.assertNotEqual(conflict.returncode, 0)
            self.assertIn(b"owner", conflict.stderr.lower())
            self.assertEqual(self.shim("exit", "0").returncode, 0)
        finally:
            if claimant.poll() is None:
                claimant.send_signal(signal.SIGTERM)
            try:
                claimant.communicate(timeout=15)
            except subprocess.TimeoutExpired:
                claimant.kill()
                claimant.communicate()
            app = home / "Library/Application Support/ego-lite-bridge"
            if app.exists():
                allowed = {"config.json", "daemon.lock", "lifecycle.lock"}
                unknown = {path.name for path in app.iterdir()} - allowed
                if unknown:
                    raise AssertionError(f"claimant state has unknown entries: {sorted(unknown)}")
                for path in app.iterdir():
                    path.unlink()
                app.rmdir()

    def disconnect_recovery(self) -> None:
        assert self.daemon is not None
        children = self.run_command(["pgrep", "-P", str(self.daemon.pid)], check=False).stdout.split()
        ssh_pids = []
        for child in children:
            command = self.run_command(["ps", "-p", child, "-o", "comm="], check=False).stdout.strip()
            if command.endswith(b"ssh"):
                ssh_pids.append(int(child))
        self.assertTrue(ssh_pids, "daemon SSH child not found")
        for ssh_pid in ssh_pids:
            os.kill(ssh_pid, signal.SIGKILL)
        self.wait_for(
            lambda: self.remote_details()["observed"]
            in {"reconnecting", "connecting", "connected"},
            "remote did not survive SSH disconnect",
            10,
        )
        self.wait_for(lambda: self.remote_details()["observed"] == "connected", "remote did not reconnect", 30)
        self.assertEqual(self.shim("exit", "0").returncode, 0)
        self.stop_bridge(signal.SIGKILL)
        self.start_bridge()
        self.wait_for(
            lambda: self.remote_details()["observed"] == "connected",
            "remote did not recover after crash",
            30,
        )
        self.snapshot_remote_runtime()


if __name__ == "__main__":
    unittest.main(verbosity=2)
