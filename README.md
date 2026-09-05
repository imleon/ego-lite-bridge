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

## Quick start

Prerequisites:

- macOS with the real `ego-browser` available on `PATH`.
- A Linux host reachable with non-interactive SSH authentication.
- `ego-lite-bridge` installed at `~/.local/bin/ego-lite-bridge` on Linux.
- `ego-lite-bridge` installed on the Mac.

On Linux, install the binary and create the shim:

```bash
mkdir -p ~/.local/bin
install -m755 target/release/ego-lite-bridge ~/.local/bin/ego-lite-bridge
ln -sf ego-lite-bridge ~/.local/bin/ego-browser
```

On the Mac, start the daemon and add the remote:

```bash
ego-lite-bridge start
ego-lite-bridge remote add dev-linux user@linux-host
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
| `ego-lite-bridge status` | Check daemon health and configured remote count. | `running (<n> remotes)` |
| `ego-lite-bridge remote add <name> <ssh-target>` | Add a remote and wait until its broker is ready. | A tab-separated record ending in `Active/Connected` |
| `ego-lite-bridge remote list` | List all configured remotes. | One tab-separated record per remote; no output when empty |
| `ego-lite-bridge remote status <name-or-config-id>` | Show one remote, including its lifecycle and observed state. | The same record format; an error may follow on an indented line |
| `ego-lite-bridge remote retry <name-or-config-id>` | Retry a remote currently in `Active/Error`. | The updated remote record |
| `ego-lite-bridge remote remove <name-or-config-id>` | Remove a remote and clean up its worker. | `removed <config-id>` |
| `ego-lite-bridge stop` | Stop the daemon and its workers. | `ego-lite-bridge stopped` (or `is stopped` if already stopped) |

Remote records have the form `<config-id>\t<name>\t<ssh-target>\t<lifecycle>/<observed-state>`. Names and config IDs are accepted wherever `<name-or-config-id>` appears. Control commands are macOS-only; Linux exposes the `ego-browser` shim.

## Build and install from source

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

Ensure `~/.local/bin` is on `PATH`. The release installer in `distribution/install.sh` performs the same platform-specific setup when a release is available.

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

- **`ego-browser bridge is not connected`**: run `ego-lite-bridge start` and `ego-lite-bridge remote add <name> user@linux-host` on the Mac.
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
