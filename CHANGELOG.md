# Changelog

## [0.1.3] - 2026-09-24

### Added

- Added macOS `ego-lite-bridge restart`, which validates the current canonical `ego-browser` path before stopping, then starts the daemon with the refreshed path.

## [0.1.2] - 2026-09-15

### Added

- Added `ego-lite-bridge upgrade` for upgrading directly to the latest release, with a per-install nonblocking lock and durable same-directory staged sync, atomic rename, and directory sync. Unknown post-rename durability exits nonzero.
- On macOS, upgrades preserve daemon state and the persisted browser path. Unconfirmed cleanup aborts without commit or restart and leaves the daemon stopped; partial-stop recovery depends on the observed state, and success is printed only after state restoration.
- On Linux, upgrades maintain an `ego-browser` relative shim pointing to the fixed `ego-lite-bridge` binary.
- Added Linux-only `ego-lite-bridge skill install`, which always opens the native skills CLI interface and may overwrite an existing skill; it has no force option. Agent-environment and foreground-TTY checks run before manifest fetch, and downloads use a private system-temporary directory.
- Linux upgrades compare the packaged skill checksum in the current release's `SHA256SUMS` with the latest manifest and enter skill handling only when it changed. Only consent triggers dependency checks and a download, reusing that manifest; the comparison does not inspect Agent installation state.

## [0.1.1] - 2026-09-11

### Fixed

- Returned request-scoped PNG screenshots from the macOS executor to the Linux shim.
- Injected the verified request transfer root into exact `ego-browser nodejs` stdin.
- Restricted screenshot transfer to direct-child `.png` files inside the per-request directory without parsing stdout or stderr as paths or adding a fallback.

## [0.1.0] - 2026-09-08

### Added

- Added the macOS per-user daemon with `start`, `status`, and `stop` lifecycle commands.
- Added daemon-managed Remote CRUD with `remote add/list/status/retry/remove`, persistent configuration, endpoint deduplication, and automatic reconnection.
- Added the Linux `ego-browser` shim and private per-user broker and ownership sockets.
- Added binary-safe forwarding for arguments, stdin, stdout, stderr, exit codes, signals, cancellation, and spawn errors.
- Added up to 8 concurrent requests, immediate capacity rejection, per-request input/output/backpressure isolation, and broker takeover when a newer Mac channel connects.
- Added exact protocol capability negotiation; peers with missing or unknown capabilities are rejected.
- Added source and release installation support for macOS and Linux.

### Security

- Restricted the Linux broker socket to the owning user (`0600`).
- Kept the Mac execution target fixed to `ego-browser`; no arbitrary executable or local fallback is available.
- Required SSH batch authentication while preserving normal SSH host-key verification.

`ego-lite-bridge` is derived from Herdr and licensed under Apache-2.0. Herdr's release history is intentionally not reproduced here.
