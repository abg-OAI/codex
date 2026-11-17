# Saffrodex repository

This repository stores the canonical Saffrodex layer definitions and the
release automation that projects them onto pinned OpenAI Codex releases.
It is not itself a Codex source checkout.

## Repository model

The first `git-clone` operation in `layers/0000-foundation/layer.toml`
identifies the official Codex release tag and commit used by the projection.
`layers/0000-foundation/` contains the non-feature changes that every
projected Saffrodex tree needs.
Every generated commit is defined by one `layers/NNNN-layer-slug/` directory.
The four-digit prefix determines application order through ordinary lexical
sorting; no separate series or order file exists.
Each directory contains `layer.toml`, with a commit message and ordered
operations referencing binary-safe patches and complete added files in overlays.
`LAYERCTL.md` owns capture, collision, and upstream repair workflows.
Original commit authorship is not part of a layer.
Treat layer directories as generated artifacts:
edit and review source in a hydrated projection,
then use `layerctl layer add` or `layerctl layer refresh` to capture it.
Upstream conflicts require explicit repair of the affected stored operations,
followed by fresh replay; `projection continue` alone does not capture them.

The foundation source pin and ordered layer definitions form one coherent state.
After advancing the pin, reconcile conflicting operations against the new
upstream tree and each repaired predecessor.
Cargo commands can rewrite local-package entries in `codex-rs/Cargo.lock` to
the workspace release version. Do not capture that generated version churn in
a layer. Local package versions already present upstream must remain unchanged,
and a new local package must use the `0.0.0` development version.
Before handoff, verify that a fresh checkout containing only the configured
upstream tag and commit can apply the complete layer stack.
Layer application must not depend on historical projection objects,
published Saffrodex tags, or objects retained by a maintainer's local clone.

Repository-owned guidance, release tooling, the root `.github/workflows/`
directory, and `layerctl` do not belong in a generated projection.
Changes needed in every generated source tree belong in
`layers/0000-foundation/`.
Do not patch an upstream `AGENTS.md` through a layer.
When working in a hydrated projection, follow the upstream guidance available
in that projected checkout together with the Saffrodex integration rules
below.

## Projected source integration

Keep the downstream delta cohesive and easy to reapply when upstream source
changes.

### Inviolable boundaries

- Codex owns every model-visible tool namespace it defines.
  Treat those contracts as immutable in Saffrodex, including tool names,
  schemas, exposure, and routing.
- Put every Saffrodex-owned model-visible tool under `saffron.*`.
  Do not extend a namespace owned by Codex, an MCP server, or a plugin.
- Never change a Codex-owned database schema or migration ledger.
  Saffrodex may use isolated `saffron_*.sqlite` databases with Saffron-owned
  schemas and migration ledgers that cannot collide with upstream.
  Vanilla Codex must remain able to read its own state safely.
- Keep inherited source changes narrow.
  Put custom behavior in separate files or modules and change inherited code
  only at the smallest seam that calls into it.
- Do not mix Saffrodex behavior with opportunistic refactors, formatting
  churn, or unrelated cleanup.

### Saffron code

In `codex-core`, put Saffrodex-owned policy, model-facing tools, and response
rendering under `core/src/saffron/`.
Keep inherited modules limited to generic mechanisms and narrow call sites.

When Saffron needs access to an inherited subsystem, add the smallest generic
extension beside that subsystem's existing owner.
Do not widen unrelated types or methods merely for a child module; Rust child
modules can access private items owned by their parent module.

Treat Saffrodex code as maintained product code rather than disposable fork
glue.
Give each module a cohesive responsibility and a narrow interface.
Document its purpose, ownership, invariants, lifecycle, failure behavior, and
non-obvious constraints where readers need them.

## Conditional guidance

Before using or changing `layerctl`, working in a generated projection,
advancing the Codex base, or preparing a release, read `LAYERCTL.md`.
It owns those workflows, their invariants, and their completion checks.

## Repository changes

Use ordinary descriptive commit subjects without a repository-name prefix.
Treat new code, guidance, and layer definitions as maintained product work:
design cohesive ownership, document non-obvious contracts, and test observable
behavior and lifecycle boundaries.
