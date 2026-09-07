#!/usr/bin/env python3
"""Generate a disabled release manifest from the two supported binaries."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import stat
from pathlib import Path

ASSETS = (
    "ego-lite-bridge-linux-x86_64",
    "ego-lite-bridge-macos-aarch64",
)
SEMVER = re.compile(
    r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-((?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
    r"(?:\.(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*))?"
    r"(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$"
)
RELEASE_BASE = "https://github.com/imleon/ego-lite-bridge/releases/download"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as asset:
        for chunk in iter(lambda: asset.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> None:
    parser = argparse.ArgumentParser(
        description="generate a disabled ego-lite-bridge release manifest"
    )
    parser.add_argument("version", help="strict semantic version without a leading v")
    parser.add_argument("assets", nargs=2, type=Path, metavar="ASSET")
    args = parser.parse_args()

    if not SEMVER.fullmatch(args.version):
        parser.error(f"invalid semantic version: {args.version}")

    assets: dict[str, Path] = {}
    for path in args.assets:
        try:
            metadata = path.lstat()
        except OSError as error:
            parser.error(f"cannot inspect asset {path}: {error.strerror}")
        if stat.S_ISLNK(metadata.st_mode):
            parser.error(f"asset must not be a symlink: {path}")
        if not stat.S_ISREG(metadata.st_mode):
            parser.error(f"asset is not a regular file: {path}")
        if metadata.st_size == 0:
            parser.error(f"asset is empty: {path}")
        if path.name not in ASSETS:
            parser.error(f"unknown asset name: {path.name}")
        if path.name in assets:
            parser.error(f"duplicate asset: {path.name}")
        assets[path.name] = path

    missing = set(ASSETS) - assets.keys()
    if missing:
        parser.error(f"missing asset: {', '.join(sorted(missing))}")

    targets = {name.removeprefix("ego-lite-bridge-"): name for name in ASSETS}
    manifest = {
        "product": "ego-lite-bridge",
        "available": False,
        "version": args.version,
        "assets": {
            target: f"{RELEASE_BASE}/v{args.version}/{name}"
            for target, name in targets.items()
        },
        "sha256": {target: sha256(assets[name]) for target, name in targets.items()},
    }
    print(json.dumps(manifest, indent=2))


if __name__ == "__main__":
    main()
