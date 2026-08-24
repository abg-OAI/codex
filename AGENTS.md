# Saffrodex repository

This repository stores the canonical Saffrodex layer definitions that are
projected onto exact OpenAI Codex releases.
It is not itself a Codex source checkout.

## Repository model

`upstream.json` identifies the exact Codex release used by the current
projection.
`layers/0000-foundation/` contains the non-feature patches that every
projected Saffrodex tree needs.
Every generated commit is defined by a `layers/NNNN-layer-slug/` directory.
The four-digit prefix determines application order through ordinary lexical
sorting; no separate series or order file exists.
Each layer directory contains:

- `COMMIT_MSG`, whose bytes become the generated commit message verbatim;
- optional `overlay/` content for paths introduced by the layer; and
- optional `patches/NNN-*.patch` files applied in lexical order to paths that
  existed before the layer.

A path cannot be both overlay content and a patch target in one layer.
Later layers may patch files introduced by earlier layers.

Repository-owned guidance does not belong in a generated projection.
Changes needed in every generated source tree belong in
`layers/0000-foundation/`.

## Repository changes

Use ordinary descriptive commit subjects without a repository-name prefix.
Treat new guidance and layer definitions as maintained product work:
design cohesive ownership, document non-obvious contracts, and test observable
behavior and lifecycle boundaries.
