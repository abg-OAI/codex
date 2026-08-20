# Saffrodex fork model

Saffrodex is a thin patch stack on released OpenAI Codex revisions.
The rolling `saffrodex` branch is rebuilt on exact `rust-v*` release tags,
while immutable Saffrodex release tags preserve published history.
Every Saffrodex change will therefore be replayed repeatedly.
Keep the fork delta explicit, cohesive, and easy to reapply.

Frodex is reference material for selected behavior,
not Saffrodex's parent or an automatic source of changes.
Import a Frodex change only when the requested Saffrodex behavior requires it.
Prefer a clean cherry-pick when the commit is self-contained;
otherwise port the behavior through the current Codex architecture.

## Inviolable rules

- Prefix every Saffrodex-owned commit subject with `saffrodex:`.
  The prefix keeps the fork patch stack immediately distinguishable
  from inherited Codex history.
- Codex owns every model-visible tool namespace it defines.
  Treat those contracts as immutable in Saffrodex,
  including tool names, schemas, exposure, and routing.
- Put every Saffrodex-owned model-visible tool under `saffron.*`.
  Do not extend a namespace owned by Codex, an MCP server, or a plugin.
- Keep every change rebase-friendly.
  Put custom behavior in separate files or modules,
  and change inherited code only at the smallest seam that calls into it.
- Do not mix fork behavior with opportunistic refactors, formatting churn,
  or unrelated cleanup.
  A Saffrodex task does not authorize an upstream Codex change.
  Ask before preparing or sending upstream work.
- Never introduce fork-owned database migrations or implicit runtime schema
  changes.
  They can collide with Codex migrations and make vanilla Codex unable to read
  the same state safely.

## New Saffrodex code

In `codex-core`, put Saffrodex-owned policy, model-facing tools,
and response rendering under `core/src/saffron/`.
Keep inherited modules limited to generic mechanisms and narrow call sites.

When Saffron needs access to an inherited subsystem,
add the smallest generic extension beside that subsystem's existing owner.
Do not widen unrelated types or methods merely for a child module;
Rust child modules can access private items owned by their parent module.

Treat new Saffrodex code as maintained product code,
not disposable fork glue.
Give each module a cohesive responsibility and a narrow interface.
Document its purpose, ownership, invariants, lifecycle,
failure behavior, and non-obvious constraints where its readers need them.
Use names, types, and structure for facts they can express clearly;
use comments for context that would otherwise require reconstruction.
Test observable behavior and material concurrency or lifecycle boundaries.
Rebase isolation does not lower the design, documentation, or testing standard.

## Rebasing

A clean rebase is an exact published Codex release
plus the same intentional Saffrodex patch stack.
Build a candidate branch from the selected `rust-v*` tag,
then port or replay only the Saffrodex behavior that still belongs in the fork.
Do not merge Frodex or copy its complete feature stack.

Before publishing, establish that the selected Codex release is an ancestor,
every later commit belongs to Saffrodex,
and a range diff against the prior tagged patch stack
contains no unexplained behavior change.
Audit the complete delta and run the tests required by the affected code.

Tag the current release before rewriting the rolling branch.
Never move or delete a published release tag.
Preserve a temporary recovery ref during history surgery,
and rewrite the rolling branch with force-with-lease semantics.

## Tags and versions

Keep checked-in `codex-rs/Cargo.lock` entries for workspace-owned packages
at the unreleased `0.0.0` sentinel, independently of
`workspace.package.version` in `codex-rs/Cargo.toml`.
Local Cargo commands may rewrite those entries to the workspace version;
discard that version churn while retaining genuine dependency graph changes.
Only the release workflow may replace a published package's sentinel with
the complete Saffrodex tag version, and only in its ephemeral release checkout.

The package version is:

```text
<codex-version>+saffrodex.<saffrodex-release>
```

The release tag is `saffrodex-v<package-version>`.
For example:

```text
saffrodex-v0.149.0-alpha.7+saffrodex.0
```

Keep the Codex CLI identity in version output:

```text
codex-cli 0.149.0-alpha.7+saffrodex.0
```
