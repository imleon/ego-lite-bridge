# Vendored skill source

- Upstream: https://github.com/citrolabs/ego-lite/tree/e6b9f9ee153911cada0a4f27b7a2d37ec195d3d6/skills/ego-browser
- Skill version: 2.1.0
- Sibling commit: `e6b9f9ee153911cada0a4f27b7a2d37ec195d3d6`
- License: MIT; see `ego-browser/LICENSE`

Upstream 2.1.0 is a bootstrap stub: `ego-browser/SKILL.md` tells the agent to run `ego-browser skill` for the version-matched guide. That guide comes from the Mac and may omit bridge-only behavior, so carry it ourselves:

- Sync `ego-browser/SKILL.md` from upstream, then re-add two bridge insertions: the "On Linux through ego-lite-bridge, read `references/bridge.md` first" paragraph after the `ego-browser skill` instructions, and the `references/bridge.md` entry at the top of the References section.
- Keep `ego-browser/references/install.md` as the bridge connect/install overlay, including the `Unknown command: skill` upgrade path.
- Carry `ego-browser/references/bridge.md` (bridge-only: screenshot transfer, Mac-path files) — not present upstream.
- Sync `ego-browser/references/clearing-state.md` unchanged from upstream.
- Exclude the macOS `ego-browser/scripts/install.sh`.
- Drop upstream `ego-browser/learnings/` and `ego-browser/references/video.md` (2.1.0 removed `page.screencast`).
