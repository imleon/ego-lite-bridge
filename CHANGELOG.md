# Changelog

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
