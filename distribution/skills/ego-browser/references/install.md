# Connect ego-browser through ego-lite-bridge

On Linux, `ego-browser` is a transparent shim. The real ego-browser CLI and browser run on the configured Mac, using its browser state and login session. Do not install the ego lite app on Linux.

Check that the shim is available:

```bash
command -v ego-browser
```

Switch to the configured Mac before running the following bridge control commands. They are not available on Linux.

Check the Mac daemon and configured remote:

```bash
ego-lite-bridge status
ego-lite-bridge doctor <config-id>
ego-lite-bridge remote status <config-id>
```

If the remote is not connected, start the Mac daemon and add or retry the Linux remote as appropriate:

```bash
ego-lite-bridge start
ego-lite-bridge remote add <linux-host>
ego-lite-bridge remote retry <config-id>
```

The daemon and remote must be healthy before retrying `ego-browser` on Linux. Bridge failures are explicit; there is no local browser fallback.

Paths passed to browser helpers such as `uploadFile()` are resolved on the Mac executor. The bridge does not transfer Linux files, so copy a file to the Mac first and pass its Mac path. PNG screenshots are the exception in the other direction: screenshots saved to the bridge request transfer directory are returned to Linux automatically.
