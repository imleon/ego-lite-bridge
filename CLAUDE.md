# ego-lite-bridge

Headless reverse remote exec bridge for `ego-browser`, derived from Herdr.

The authoritative product behavior, ownership model, concurrency limits, security boundary, protocol expectations, and acceptance criteria are defined in [`docs/PRD.md`](docs/PRD.md). The delivery sequence is defined in [`docs/ROADMAP.md`](docs/ROADMAP.md). Read both before changing externally observable behavior.

## Non-negotiable product constraints

- On macOS, one per-user daemon manages persistent SSH-backed channels to configured Linux remotes without a TUI; the current `serve <linux-host>` command is transitional and not the 0.1 product interface.
- On Linux, `ego-browser <args...>` is a one-shot shim; the real `ego-browser` runs on the Mac with its browser state and login session.
- Forward arguments, stdin, stdout, stderr, failures, applicable signals, and exit status transparently.
- The fixed Mac-side target is `ego-browser`; do not introduce arbitrary shell execution.
- If the Mac bridge is unavailable, fail clearly. Never add local execution, another browser, alternate transport, compatibility fallback, or degraded behavior without explicit approval.
- Do not expose browser credentials, add environment forwarding, or broaden executable selection without an explicit design and security review.

## Implementation rules

- Make the smallest working change after tracing the end-to-end flow.
- Reuse existing runtime, framing, IPC, SSH, process, stream, and error-handling code before adding abstractions or dependencies.
- Keep platform behavior behind compile gates: daemon, control CLI, remote workers, and executor on macOS; broker and shim on Linux.
- Avoid unnecessary allocation, blocking, and broad locks in protocol, process I/O, socket, and request paths.
- Changes to wire format, execution semantics, SSH setup, socket identity, ownership, or lifecycle require a characterization test and corresponding PRD update when product behavior changes.
- All request-scoped messages carry a request ID. Preserve binary-safe framing and exact command semantics.
- Rust production code must not use `unwrap()`; use `tracing` for structured logging; every `#[allow]` requires an explanatory comment.

## Commands

Use `just` recipes:

```bash
just build            # release build
just test             # Rust tests
just installer-test   # Unix installer tests
just check            # formatting, Clippy, Rust tests, installer tests
```

Run the narrowest relevant test during iteration and `just check` before committing. Do not bypass failures; fix them or report why they are unrelated.

For bridge behavior, verify argument and binary stream forwarding, stdin EOF, stdout/stderr separation, spawn errors, cancellation, concurrency isolation, ownership conflicts, disconnect recovery, signals, and exit status.

## Git

Use lowercase conventional commit subjects without emojis or AI co-author lines. Propose the commit message before committing.

## Attribution and license

Keep explicit Herdr-derived attribution and the Apache-2.0 license in user-facing project documentation.
