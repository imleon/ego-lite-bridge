# Changelog

## [0.1.0] - Unreleased

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
