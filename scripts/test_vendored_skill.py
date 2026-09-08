from __future__ import annotations

import re
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SKILL = REPO_ROOT / "distribution" / "skills" / "ego-browser"


class VendoredSkillTests(unittest.TestCase):
    def test_skill_metadata_and_install_reference(self) -> None:
        content = (SKILL / "SKILL.md").read_text(encoding="utf-8")
        self.assertRegex(content, r"(?m)^name: ego-browser$")
        self.assertRegex(content, r'(?m)^  version: "1\.2\.6"$')
        self.assertIn("`references/install.md`", content)
        self.assertTrue((SKILL / "references" / "install.md").is_file())

    def test_bridge_install_overlay(self) -> None:
        content = (SKILL / "references" / "install.md").read_text(encoding="utf-8")
        for text in (
            "transparent shim",
            "real ego-browser CLI and browser run on the configured Mac",
            "command -v ego-browser",
            "ego-lite-bridge status",
            "ego-lite-bridge doctor",
            "Mac daemon",
            "remote",
            "Do not install the ego lite app on Linux",
            "uploadFile()",
            "resolved on the Mac executor",
            "does not transfer Linux files",
        ):
            with self.subTest(text=text):
                self.assertIn(text, content)

        self.assertFalse((SKILL / "scripts" / "install.sh").exists())
        self.assertIsNone(re.search(r"(?i)\bDMG\b|/Applications|install\.sh", content))

        video = (SKILL / "references" / "video.md").read_text(encoding="utf-8")
        self.assertIn("Mac executor's working directory", video)
        self.assertIn("does not transfer the resulting file to Linux", video)
        self.assertIn("Install `ffmpeg` on the Mac executor's `PATH`", video)


if __name__ == "__main__":
    unittest.main()
