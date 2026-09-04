# ego-lite-bridge

Headless reverse remote exec bridge derived from Herdr.

## Product Direction

This project is Herdr-derived, but it is not simply "Herdr minus TUI". The goal is to reuse Herdr's mature remote/runtime/transport infrastructure while changing the product semantics from a long-lived remote terminal workspace into a one-shot remote exec bridge for `ego-browser`.

Target behavior:

- On macOS, `ego-lite-bridge serve <linux-host>` establishes a persistent SSH-backed channel into Linux without launching a TUI.
- On Linux, running `ego-browser ...` behaves like a local browser CLI, but it is a shim: the actual `ego-browser` process executes on the Mac side through the established bridge.
- Arguments, stdin, stdout, stderr, response payloads, failures, signals when applicable, and exit codes must be forwarded transparently.
- Linux callers should observe the same behavior they would get from a local `ego-browser` binary, while the browser and user login state remain on macOS.
- If the Mac-side bridge is not connected, Linux calls must fail clearly. Do not silently fall back to local execution or another browser.

## Product Design

Roles:

- Mac side: executor and channel owner. It holds the real `ego-browser`, owns the user's browser login state, keeps the channel alive, executes requests, and streams results back.
- Linux side: broker plus CLI shim. The CLI is one-shot; it connects to a local broker socket, sends argv/stdin, waits for stdout/stderr/exit, mirrors them locally, then exits.
- Linux broker: lightweight local daemon/socket endpoint started or reached by the Mac channel. It accepts local CLI requests and routes them over the already-established Mac channel.

Flow:

```text
Linux CLI -> Linux broker -> SSH-backed channel -> Mac executor -> ego-browser
```

The Linux broker exists because the desired product shape is Mac-first: the Mac process establishes the channel before Linux users run one-shot commands. A later Linux CLI invocation should reuse that existing channel instead of opening a fresh SSH connection to Mac.

## Command Naming

- Mac user-facing command: `ego-lite-bridge serve <linux-host>`.
- Linux user-facing command: `ego-browser <args...>`.
- Linux `ego-browser` may be a symlink, wrapper, or argv0-dispatched mode of the bridge binary, but the daily user experience should not expose bridge/remote/Mac implementation details.
- Reserve `ego-lite-bridge` on Linux for management and diagnostics such as status, stop, broker, or doctor commands.
- Avoid `--client`, `--remote`, `ego-browser-remote`, and `ego-browser-mac` as the primary UX names; they make the client/server or remote perspective ambiguous.

## Architecture Boundaries

- Prefer reusing Herdr remote/runtime/transport layers over building new bridge logic from scratch.
- Keep reusable Herdr runtime, remote, session, SSH, process, socket, stream, and error-handling paths intact unless there is a concrete reason to change them.
- Reuse the existing `--remote` SSH stdio bridge path (`remote-client-bridge` / `run_remote_client_bridge`) as the transport foundation where possible.
- Reuse the endpoint protocol framework in `src/protocol/endpoint.rs` for handshake/generation/framing patterns (`endpoint.hello.v1`, `endpoint.welcome.v1`), but do not force one-shot command execution through the TUI shell surface model.
- Treat `shell.snapshot.v1`, `shell.surface.v1`, `shell.input.semantic.v1`, and `shell.blob.v1` as Herdr shell endpoint codecs. Use them only if the bridge truly needs shell-surface semantics; otherwise define the smallest exec-oriented codec for argv/stdin/stdout/stderr/exit forwarding.
- Treat TUI, pane, workspace, and shell surface code as presentation/product semantics. Do not remove shared runtime behavior merely because Herdr originally reached it through those layers.
- Keep shared behavior in runtime/server/protocol layers, not in UI/client presentation layers.
- Do not add compatibility fallbacks, local execution fallbacks, alternate transports, or degraded behavior unless explicitly requested.
- Preserve exact command semantics before adding convenience features.

## Exec Protocol Shape

Prefer a minimal exec-oriented protocol over terminal snapshot semantics:

- `exec.hello.v1` / `exec.welcome.v1` for capability and generation negotiation.
- `exec.open.v1` for one request: argv plus cwd/env allowlist only when explicitly needed.
- `exec.stdin.v1` / `exec.stdin_eof.v1` for stdin streaming.
- `exec.stdout.v1` / `exec.stderr.v1` for binary-safe output streaming.
- `exec.exit.v1` for exit code or signal.
- `exec.error.v1` for spawn, protocol, or channel failures.
- `exec.cancel.v1` for interrupted Linux CLI calls.

Every request should carry a request id so concurrent Linux CLI calls can be supported without redesigning the protocol. Do not execute arbitrary shell commands by default; the fixed Mac-side target is `ego-browser`.

## Implementation Rules

- Smallest working diff wins after tracing the real flow end to end.
- Reuse existing Herdr mechanisms first; add new abstractions only when the existing path cannot serve the headless bridge directly.
- Do not add dependencies unless existing dependencies and the standard library cannot cover the need.
- Platform-specific behavior belongs behind compile gates and, when substantial, in the existing platform modules.
- Multiplicative paths matter: protocol forwarding, process I/O, remote sessions, socket brokers, and client fanout must avoid unnecessary allocation, blocking, and broad locks.
- If a change touches wire protocol, command execution semantics, session identity, remote setup, or persisted state, add or name a characterization test before refactoring.

## MVP Order

1. Mac `ego-lite-bridge serve <linux-host>` starts or connects the Linux broker and keeps one SSH-backed channel alive.
2. Linux CLI connects to the broker socket and forwards argv to Mac.
3. Mac executor spawns `ego-browser` and forwards stdout, stderr, and exit code.
4. Add stdin streaming, spawn-error propagation, Ctrl-C/cancel, channel disconnect handling, and binary-safe output.
5. Add request ids and broker multiplexing for concurrent Linux CLI calls.

## Commands

Use `just` recipes by default:

```bash
just test               # Rust tests + installer tests
just check              # formatting + Clippy + tests
```

## Testing

- Run the narrowest useful test during iteration.
- Run `just check` before committing unless explicitly accepted narrower validation.
- Do not bypass failing checks; fix the failure or report exactly why it is unrelated.
- For bridge behavior, verify stdout, stderr, stdin, argument passing, error propagation, and exit code transparency.
- Unit tests should sit next to Rust code in `#[cfg(test)] mod tests`.

## Code Conventions

- Rust: no `unwrap()` in production code.
- Use `tracing` for logging.
- Use `#[allow]` only with a comment explaining why.
- Keep commit subjects lowercase conventional commits, no emojis, and no AI co-author lines.
- Before committing, propose the commit message and get alignment.
