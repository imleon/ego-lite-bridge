# ego-lite-bridge

Headless reverse remote exec bridge for `ego-browser`, derived from Herdr.

## Product rules

- On macOS, `ego-lite-bridge serve <linux-host>` owns a persistent SSH-backed channel to Linux without a TUI.
- On Linux, `ego-browser <args...>` is a one-shot shim; the real `ego-browser` runs on the Mac with its browser state and login session.
- Forward arguments, stdin, stdout, stderr, failures, applicable signals, and exit status transparently.
- If the Mac bridge is unavailable, fail clearly. Never add local execution, another browser, alternate transport, compatibility fallback, or degraded behavior without explicit approval.

## Architecture

```text
Linux ego-browser shim -> Linux broker -> SSH channel -> Mac executor -> ego-browser
```

- The Mac is the executor and channel owner.
- The Linux broker exposes a private per-user socket and routes local shim requests over the existing channel.
- The fixed Mac-side target is `ego-browser`; do not introduce arbitrary shell execution.
- The bridge runs up to 8 requests concurrently and rejects additional requests at capacity. Keep request input, output, cancellation, errors, and backpressure isolated by request ID.

## Protocol

Keep the exec protocol minimal:

- handshake: hello and welcome
- request lifecycle: open, stdin, stdin EOF, cancel
- result lifecycle: stdout, stderr, exit, error

All request-scoped messages carry a request ID. Preserve binary-safe framing and exact command semantics before adding convenience features.

## Trust boundary

- SSH authentication and host-key verification establish trust between Mac and Linux.
- The Linux broker socket must remain private to the owning user.
- Any process under that Linux user can invoke the fixed Mac-side `ego-browser` with arbitrary arguments and stdin.
- Do not expose browser credentials, add environment forwarding, or broaden executable selection without an explicit design and security review.

## Implementation rules

- Make the smallest working change after tracing the end-to-end flow.
- Reuse existing runtime, framing, IPC, SSH, process, stream, and error-handling code before adding abstractions or dependencies.
- Keep platform behavior behind compile gates: serve on macOS, broker and shim on Linux.
- Avoid unnecessary allocation, blocking, and broad locks in protocol, process I/O, socket, and request paths.
- Changes to wire format, execution semantics, SSH setup, socket identity, or lifecycle require a characterization test.
- Rust production code must not use `unwrap()`; use `tracing` for logging when structured logging is needed; every `#[allow]` requires an explanatory comment.

## Commands

Use `just` recipes:

```bash
just build            # release build
just test             # Rust tests
just installer-test   # Unix installer tests
just check            # formatting, Clippy, Rust tests, installer tests
```

Run the narrowest relevant test during iteration and `just check` before committing. Do not bypass failures; fix them or report why they are unrelated.

For bridge behavior, verify argument and binary stream forwarding, stdin EOF, stdout/stderr separation, spawn errors, cancellation, disconnect recovery, signals, and exit status.

## Git

Use lowercase conventional commit subjects without emojis or AI co-author lines. Propose the commit message before committing.

## Attribution and license

Keep explicit Herdr-derived attribution and the Apache-2.0 license in user-facing project documentation.
