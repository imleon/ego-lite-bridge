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

If `ego-browser skill` reports `Unknown command: skill`, the bridge works but ego lite on the Mac is older than this skill. Ask the user for approval, then run `ego-browser upgrade`; the shim forwards it to the Mac. If `upgrade` is also an unknown command, have the user update ego lite on the Mac from https://lite.ego.app/.

The daemon and remote must be healthy before retrying `ego-browser` on Linux. Bridge failures are explicit; there is no local browser fallback.

For screenshot transfer and Mac-path file behavior during browser tasks, see [`references/bridge.md`](bridge.md).
