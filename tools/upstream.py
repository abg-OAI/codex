#!/usr/bin/env python3
"""Read the Codex release identity used by Saffrodex CI and releases."""

from pathlib import Path
import re
import sys
import tomllib


def release_source(manifest: dict) -> tuple[str, str]:
    """Return the foundation's release tag and commit, before remote verification."""
    source = manifest["operation"][0]["git-clone"]
    if source["url"] != "https://github.com/openai/codex.git" or source.get("dir", ""):
        raise ValueError("foundation must import OpenAI Codex at the workspace root")
    ref = source["ref"]
    if not re.fullmatch(
        r"refs/tags/rust-v[0-9]+\.[0-9]+\.[0-9]+"
        r"(-(alpha(\.[0-9]+){0,2}|beta(\.[0-9]+)?))?", ref
    ):
        raise ValueError("foundation must pin a Codex rust-v* release tag")
    commit = source["commit"]
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("foundation must pin a full commit ID")
    return ref.removeprefix("refs/tags/"), commit


def main() -> int:
    path = Path(__file__).resolve().parent.parent / "layers/0000-foundation/layer.toml"
    try:
        with path.open("rb") as stream:
            tag, commit = release_source(tomllib.load(stream))
    except (OSError, ValueError, KeyError, IndexError, TypeError) as error:
        print(f"{path}: {error}", file=sys.stderr)
        return 1
    print(f"{tag}\t{commit}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
