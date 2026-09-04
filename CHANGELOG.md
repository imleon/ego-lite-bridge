# Changelog

## [0.1.0] - Unreleased

### Added

- Added the macOS `ego-lite-bridge serve <linux-host>` command, which maintains an SSH-backed execution channel to Linux and reconnects after transient failures.
- Added the Linux `ego-browser` shim and private per-user broker socket.
- Added binary-safe forwarding for arguments, stdin, stdout, stderr, exit codes, signals, cancellation, and spawn errors.
- Added queued one-at-a-time request handling and broker takeover when a newer Mac channel connects.
- Added source and release installation support for macOS and Linux.

### Security

- Restricted the Linux broker socket to the owning user (`0600`).
- Kept the Mac execution target fixed to `ego-browser`; no arbitrary executable or local fallback is available.
- Required SSH batch authentication while preserving normal SSH host-key verification.

`ego-lite-bridge` is derived from Herdr and licensed under Apache-2.0. Herdr's release history is intentionally not reproduced here.
