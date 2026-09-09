# ego-lite-bridge

[简体中文](README.zh-CN.md)

`ego-lite-bridge` lets a Linux host use `ego-browser` running on a Mac. The Mac owns the browser process and login state; Linux gets a local-looking `ego-browser` command with arguments, stdin, stdout, stderr, signals, and exit status forwarded across a persistent SSH-backed channel.

This project is derived from [Herdr](https://github.com/herdrdev/herdr) and retains its Apache-2.0 license.

## Architecture

```text
Linux ego-browser shim -> Linux broker -> SSH channel -> Mac executor -> ego-browser
```

- The macOS daemon owns persistent channels configured through `ego-lite-bridge remote ...`.
- A private broker socket on Linux accepts local `ego-browser` invocations.
- The executable name `ego-browser` selects shim mode; the real binary is only started on the Mac.
- If the bridge is unavailable, the Linux command fails instead of falling back to local execution.

## Release status

Version 0.1.0 provides prebuilt binaries for `linux-x86_64` and `macos-aarch64`; users do not need Rust or a source build. The Linux release is a static `x86_64-unknown-linux-musl` binary, and the macOS release is a native `aarch64-apple-darwin` binary.

## Quick start

Prerequisites:

- macOS with the real `ego-browser` available on `PATH`.
- A Linux host reachable with non-interactive SSH authentication.
- `~/.local/bin` on `PATH` on both machines.

Install the latest release on both the Mac and Linux host:

```bash
curl -fsSL https://raw.githubusercontent.com/imleon/ego-lite-bridge/master/distribution/install.sh | sh
```

The installer verifies the binary against the release manifest's SHA-256 checksum. On macOS it installs only `ego-lite-bridge`, without prompting for a skill. On Linux, after committing the binary and `ego-browser` shim, it asks `[Y/n]` through `/dev/tty`: Enter or `y`/`yes` (case-insensitive) starts optional skill installation; `n`/`no` skips it; invalid input prompts again. EOF, a read failure, or no usable terminal skips the skill and prints the manual-install link. Skipping needs no Node.js, npm/npx, or tar and leaves Agent directories untouched.

Only after consent does the optional step require Node.js 22.20.0 or newer, `npm`/`npx`, and `tar`. It downloads the same release's skill using the manifest already fetched for the binary, verifies SHA-256, validates the archive, and extracts it safely. Before any skill download or CLI launch, a guard tied to the fixed `skills@1.5.24` version checks for Agent execution environments; if detected, it asks you to rerun in an ordinary terminal instead of silently clearing environment variables or launching a potentially auto-confirming CLI.

The installer runs `npx --yes skills@1.5.24 add <extracted-skill> --skill ego-browser --global --copy` with stdin, stdout, and stderr connected to `/dev/tty`, including when invoked through `curl | sh`. The outer `npx --yes` permits fetching the CLI; no inner `--yes`, `--agent`, or `--all` is passed. Agent selection and confirmation use the native upstream interface, not a bridge-defined selector: universal targets cannot be deselected, and a single-Agent setup may omit the selection screen. The default `[Y/n]` does not accept all upstream choices for you.

If this optional step fails, the installer exits nonzero and reports that the bridge is installed but the skill step is incomplete; it does not roll back the bridge, retry, or fall back. Upstream cancellation can also exit 0, so a zero exit only reports that the interaction ended—check the CLI output for the result, not a blanket skill-install success message. The CLI may write only some targets; exit 0 does not guarantee every target succeeded, and skills already written are not rolled back.

<a id="optional-agent-skill-installation"></a>
### Optional Agent skill installation

The release retains the vendored `ego-browser-skill.tgz` asset and its URL and SHA-256 in the manifest. As an independent alternative to the installer's native interactive flow, you can install it manually on Linux for an explicit Agent. This requires Node.js 22.20.0 or newer, `npm`/`npx`, and `tar`. Set `VERSION` to the exact bridge release already installed, then set `AGENT_ID` in your shell to one explicit Agent ID supported by the skills CLI; in this manual command, do not omit `--agent` or use `*`.

```bash
(
  set -eu
  VERSION=0.1.0
  : "${AGENT_ID:?set AGENT_ID to one explicit skills CLI Agent ID}"
  WORK_DIR="$(mktemp -d)"
  trap 'rm -rf "$WORK_DIR"' EXIT
  cd "$WORK_DIR"
  RELEASE_URL="https://github.com/imleon/ego-lite-bridge/releases/download/v${VERSION}"
  curl -fLO "${RELEASE_URL}/ego-browser-skill.tgz"
  curl -fLO "${RELEASE_URL}/SHA256SUMS"
  grep ' ego-browser-skill.tgz$' SHA256SUMS > ego-browser-skill.sha256
  sha256sum -c ego-browser-skill.sha256
  mkdir skill
  tar -xzf ego-browser-skill.tgz -C skill
  npx --yes skills@1.5.24 add "$WORK_DIR/skill/ego-browser" \
    --skill ego-browser --global --agent "$AGENT_ID" --yes --copy
)
```

The skills CLI may overwrite an existing `ego-browser` skill, including local changes, for that Agent. Rerunning the bridge installer and consenting to the optional flow can also overwrite existing skills; skipping it leaves them untouched. This is not a runtime updater or automatic skill cleanup. The vendored skill remains a release asset and manifest entry. Its calls still pass through the Linux shim to the real browser running on the Mac.

To download and install the bridge manually instead, run the matching commands on each machine. These steps install the bridge binary and Linux shim only.

macOS arm64:

```bash
curl -fLO https://github.com/imleon/ego-lite-bridge/releases/latest/download/ego-lite-bridge-macos-aarch64
curl -fLO https://github.com/imleon/ego-lite-bridge/releases/latest/download/SHA256SUMS
grep ' ego-lite-bridge-macos-aarch64$' SHA256SUMS | shasum -a 256 -c -
mkdir -p ~/.local/bin
install -m755 ego-lite-bridge-macos-aarch64 ~/.local/bin/ego-lite-bridge
```

Linux x86_64:

```bash
curl -fLO https://github.com/imleon/ego-lite-bridge/releases/latest/download/ego-lite-bridge-linux-x86_64
curl -fLO https://github.com/imleon/ego-lite-bridge/releases/latest/download/SHA256SUMS
grep ' ego-lite-bridge-linux-x86_64$' SHA256SUMS | sha256sum -c -
mkdir -p ~/.local/bin
install -m755 ego-lite-bridge-linux-x86_64 ~/.local/bin/ego-lite-bridge
ln -sf ego-lite-bridge ~/.local/bin/ego-browser
```

On the Mac, start the daemon and add the remote:

```bash
ego-lite-bridge start
ego-lite-bridge remote add user@linux-host
```

Then, on Linux, use the shim as if `ego-browser` were local:

```bash
ego-browser --help
ego-browser <args...>
```

The daemon reconnects automatically after transient SSH or network failures.

## Command reference

Run these control commands on macOS:

| Command | Purpose | Successful output |
| --- | --- | --- |
| `ego-lite-bridge start` | Start the per-user daemon; it is safe to run when already started. | `ego-lite-bridge started` or `ego-lite-bridge is running` |
| `ego-lite-bridge status` | Show daemon health plus each remote's desired and observed state. | `daemon=running remotes=<n>`, followed by `<config-id> desired=<state> observed=<state>` per remote |
| `ego-lite-bridge doctor [config-id]` | Check the local Mac environment and the daemon's current snapshot of all remotes, or one selected remote. | `PASS`, `FAIL`, and `NOT CHECKED` records described below |
| `ego-lite-bridge remote add <ssh-target>` | Add a remote and wait until its broker is ready. | `<config-id>\t<ssh-target>\tdesired=active observed=connected` |
| `ego-lite-bridge remote list` | List all configured remotes. | `<config-id>\t<ssh-target>\tdesired=<state> observed=<state>` per remote; no output when empty |
| `ego-lite-bridge remote status <config-id>` | Show the fields listed at right for one remote. | Labeled lines: `config-id`, `target`, `desired`, `observed`, `state-changed-unix-ms`, `last-error`, `protocol-version`, `capabilities`, `reconnect-attempt`, `reconnect-at-unix-ms`, and `active-requests` |
| `ego-lite-bridge remote retry <config-id>` | Retry a remote currently in `active/error`. | The `remote list` record for the updated remote |
| `ego-lite-bridge remote remove <config-id>` | Remove a remote and clean up its worker. | `removed <config-id>` |
| `ego-lite-bridge stop` | Stop the daemon and its workers. | `ego-lite-bridge stopped` (or `is stopped` if already stopped) |

Desired states are `pending`, `active`, and `removing`; observed states are `connecting`, `connected`, `reconnecting`, `error`, and `removing`. Unknown unavailable detail is printed as `unknown`. `active-requests` is `<active>/<capacity>`. Every `[config-id]` or `<config-id>` selector must be the full 32-character lowercase hexadecimal ID printed by `remote add` or `remote list`; short prefixes, names, selector aliases, migration, and fallback are not supported.

`doctor` is read-only. In M7 it checks whether the LaunchAgent is loaded, the daemon is running, and the configured absolute `ego-browser` path is valid. For each remote, it checks persisted endpoint identity presence and desired/observed state, plus handshake, capacity, and reconnect/error data from the daemon's **current worker snapshot**. It does not verify that the live endpoint identity matches the persisted value. `PASS` means that check is healthy in the inspected local state or snapshot; `FAIL` means an environment, daemon, selector, or snapshot check failed; `NOT CHECKED` explicitly means M7 did not open a new SSH connection or verify live endpoint identity, Linux socket permissions, or end-to-end execution. Those active remote checks are planned for post-0.1 hardening. Exit status is 0 when no check fails, 1 when any check fails, and 2 for invalid `doctor` syntax. `doctor` never repairs, installs, or changes configuration.

Control commands are macOS-only; Linux exposes the `ego-browser` shim.

## Development from source

Rust and `just` are required.

```bash
git clone https://github.com/imleon/ego-lite-bridge.git
cd ego-lite-bridge
just build
```

Install on macOS:

```bash
mkdir -p ~/.local/bin
install -m755 target/release/ego-lite-bridge ~/.local/bin/ego-lite-bridge
```

Install on Linux:

```bash
mkdir -p ~/.local/bin
install -m755 target/release/ego-lite-bridge ~/.local/bin/ego-lite-bridge
ln -sf ego-lite-bridge ~/.local/bin/ego-browser
```

These steps are for contributors building from source; regular users should use the release installer above.

## Current limitations

- Only macOS executors and Linux callers are supported.
- Up to 8 `ego-browser` invocations run concurrently. Additional invocations are rejected immediately at capacity; a blocked or disconnected request does not block the others.
- The Linux broker path is fixed to `~/.local/bin/ego-lite-bridge`.
- The bridge forwards command arguments and standard streams only; it does not mirror the Mac filesystem or environment.

## Trust boundary

- SSH authentication and host-key verification define trust between the Mac and Linux host. Configure and verify them before starting the bridge.
- Linux runtime endpoints are `/tmp/ego-lite-bridge-<uid>/broker.sock` and `/tmp/ego-lite-bridge-<uid>/owner.sock`. The directory is mode `0700` and the sockets are mode `0600`, so only the owning Linux user can connect.
- Any process running as that Linux user can ask the Mac to run the fixed `ego-browser` executable with arbitrary arguments and stdin. Run the bridge only for a Linux account you trust.
- Browser output and exit status come from the connected Mac executor. No local or alternate-browser fallback is used.

## Troubleshooting

- **`ego-browser bridge is not connected`**: run `ego-lite-bridge start` and `ego-lite-bridge remote add user@linux-host` on the Mac.
- **SSH repeatedly reconnects**: verify `ssh user@linux-host true` succeeds without a password or confirmation prompt. The bridge uses SSH batch mode.
- **Remote binary is missing**: install an executable at `~/.local/bin/ego-lite-bridge` on Linux.
- **`ego-browser` is not found on Linux**: create the symlink above and add `~/.local/bin` to `PATH`.
- **Mac spawn failure**: verify the real `ego-browser` is on the `PATH` inherited by `ego-lite-bridge`.
- **Stale Linux runtime endpoints**: stop the Mac bridge, remove `/tmp/ego-lite-bridge-$(id -u)/` only after confirming no broker is running, then start the bridge again.

Both the Mac bridge and Linux broker write lifecycle and request diagnostics to stderr.

## Development

```bash
just test             # Rust tests
just installer-test   # Unix installer tests
just check            # formatting, Clippy, Rust tests, installer tests

# Opt-in: real Mac -> SSH-reachable Linux smoke (not part of just check)
EGO_LITE_BRIDGE_BIN=target/release/ego-lite-bridge \
EGO_LITE_BRIDGE_SSH_TARGET=user@linux-host just e2e-manual
```

Run the narrowest relevant test while iterating and `just check` before committing. The manual smoke requires `EGO_LITE_BRIDGE_BIN` (the current macOS binary) and `EGO_LITE_BRIDGE_SSH_TARGET` (an SSH destination with the Linux bridge installed); `EGO_LITE_BRIDGE_LINUX_SHIM` optionally overrides `~/.local/bin/ego-browser`. It starts and stops the daemon, so do not run it against a daemon serving unrelated work.

## License

Licensed under the [Apache License 2.0](LICENSE). This codebase is derived from Herdr; attribution does not imply endorsement by the Herdr project.
