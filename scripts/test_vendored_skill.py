from __future__ import annotations

import re
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SKILL = REPO_ROOT / "distribution" / "skills" / "ego-browser"


def _flat(path: Path) -> str:
    """File text with runs of whitespace collapsed, so assertions ignore wrapping."""
    return " ".join(path.read_text(encoding="utf-8").split())


class VendoredSkillTests(unittest.TestCase):
    def test_skill_metadata_and_references(self) -> None:
        content = (SKILL / "SKILL.md").read_text(encoding="utf-8")
        self.assertRegex(content, r"(?m)^name: ego-browser$")
        self.assertRegex(content, r'(?m)^  version: "2\.1\.0"$')
        self.assertRegex(content, r"(?m)^  bootstrap: true$")
        self.assertIn("references/bridge.md", content)
        self.assertIn("references/install.md", content)
        self.assertTrue((SKILL / "references" / "install.md").is_file())
        self.assertTrue((SKILL / "references" / "bridge.md").is_file())
        self.assertTrue((SKILL / "references" / "clearing-state.md").is_file())

    def test_bridge_install_overlay(self) -> None:
        content = _flat(SKILL / "references" / "install.md")
        for text in (
            "transparent shim",
            "real ego-browser CLI and browser run on the configured Mac",
            "command -v ego-browser",
            "ego-lite-bridge status",
            "ego-lite-bridge doctor",
            "Mac daemon",
            "remote",
            "Do not install the ego lite app on Linux",
            "Unknown command: skill",
            "ego-browser upgrade",
        ):
            with self.subTest(text=text):
                self.assertIn(text, content)

        self.assertFalse((SKILL / "scripts" / "install.sh").exists())
        self.assertIsNone(re.search(r"(?i)\bDMG\b|/Applications|install\.sh", content))

    def test_bridge_reference(self) -> None:
        bridge = _flat(SKILL / "references" / "bridge.md")
        for text in (
            "EGO_LITE_BRIDGE_TRANSFER_DIR",
            "as `ego-browser nodejs -e '<code>'`",
            "page.setInputFiles()",
            "add `</dev/null`",
            "run one at a time on the Mac",
            "resolved on the Mac executor",
            "does not transfer Linux files",
        ):
            with self.subTest(text=text):
                self.assertIn(text, bridge)
        self.assertIsNone(re.search(r"screencast|ffmpeg|uploadFile", bridge))

    def test_skill_md_bridge_insertions(self) -> None:
        content = _flat(SKILL / "SKILL.md")
        self.assertIn("On Linux through ego-lite-bridge, read `references/bridge.md` first", content)
        self.assertIn("[Running through ego-lite-bridge](references/bridge.md)", content)

    def test_dropped_upstream_content(self) -> None:
        self.assertFalse((SKILL / "references" / "video.md").exists())
        self.assertFalse((SKILL / "learnings").exists())


if __name__ == "__main__":
    unittest.main()
