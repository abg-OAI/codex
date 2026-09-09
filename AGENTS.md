# Saffrodex repository

This repository stores the canonical Saffrodex layer definitions and the
tooling that projects them onto exact OpenAI Codex releases.
It is not itself a Codex source checkout.

## Repository model

`upstream.json` identifies the exact Codex release used by the current
projection.
`layers/0000-foundation/` contains the non-feature changes that every
projected Saffrodex tree needs.
Every generated commit is defined by one `layers/NNNN-layer-slug/` directory.
The four-digit prefix determines application order through ordinary lexical
sorting; no separate series or order file exists.
Each directory contains a required `COMMIT_EDITMSG` and an optional binary-safe
tree delta in `patch`. Original commit authorship is not part of a layer.
Treat layer directories as generated artifacts:
edit and review source in a hydrated projection,
then use `layerctl layer add` or `layerctl layer refresh` to capture it.

`upstream.json` and the ordered layer definitions form one coherent state.
After advancing `upstream.json`, refresh every layer in order against the new
upstream tree and each refreshed predecessor.
Before handoff, verify that a fresh checkout containing only the configured
upstream tag and commit can apply the complete layer stack.
Layer application must not depend on historical projection objects,
published Saffrodex tags, or objects retained by a maintainer's local clone.

Repository-owned guidance, release tooling, the root `.github/workflows/`
directory, and `layerctl` do not belong in a generated projection.
Changes needed in every generated source tree belong in
`layers/0000-foundation/`.

## Conditional guidance

Before using or changing `layerctl`, working in a generated projection,
advancing the Codex base, or preparing a release, read `LAYERCTL.md`.
It owns those workflows, their invariants, and their completion checks.

## Repository changes

Use ordinary descriptive commit subjects without a repository-name prefix.
Treat new code, guidance, and layer definitions as maintained product work:
design cohesive ownership, document non-obvious contracts, and test observable
behavior and lifecycle boundaries.
