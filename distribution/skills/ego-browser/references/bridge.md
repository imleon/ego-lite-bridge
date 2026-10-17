# Running through ego-lite-bridge

On Linux, `ego-browser` is the ego-lite-bridge shim: the real CLI and browser
run on the configured Mac. Read this before any browser task — the runtime
`ego-browser skill` guide comes from the Mac and may not repeat these
bridge-only rules.

## Screenshots

When the script reaches `ego-browser nodejs` on stdin (heredoc or redirect) or
as `ego-browser nodejs -e '<code>'`, PNG screenshots saved without an explicit
path, or under `process.env.EGO_LITE_BRIDGE_TRANSFER_DIR`, are returned to the
Linux side automatically. Other argument forms, such as extra flags after
`-e '<code>'`, do not get this: their screenshots stay on the Mac. Do not use
arbitrary Mac paths for screenshots that Linux must read.

`ego-browser nodejs -e` waits for stdin to close before it runs. When stdin may
stay open, as in an agent tool shell, add `</dev/null`.

`ego-browser nodejs` scripts from all Linux sessions run one at a time on the
Mac. A script that waits, for example for the user to take over a page, delays
every other session's script until it finishes.

## Other files resolve on the Mac

Paths passed to `page.setInputFiles()` and other file helpers are resolved on
the Mac executor. The bridge does not transfer Linux files, so copy a file to
the Mac first and pass its Mac path.
