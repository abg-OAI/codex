# Saffrodex fork model

Saffrodex is a thin patch stack on Frodex,
which is a rolling fork of Codex.
Frodex rebases its commits onto newer Codex revisions;
Saffrodex therefore rebases only its own commits onto published Frodex tags.
Release tags preserve old history even though the rolling branch is rewritten.
Every Saffrodex change will be replayed repeatedly,
so preserve parent structure and keep the Saffrodex delta easy to identify.

## Inviolable rules

- Codex owns the contracts of every model-visible tool namespace it defines.
  Treat those contracts as immutable in Saffrodex,
  including tool names, schemas, exposure, and routing.
  This applies to future Codex namespaces, not only `collaboration.*`.
- Put every Saffrodex-specific tool under `saffrodex.*`.
  Do not extend a namespace owned by Codex, Frodex, an MCP server, or a plugin.
- Keep every change rebase-friendly.
  Put custom behavior in separate files or modules,
  and change inherited code only at the smallest seam that calls into it.
- Do not mix fork behavior with opportunistic refactors, formatting churn,
  or unrelated cleanup.
  A Saffrodex task does not authorize changes to Frodex or Codex.
  If a change belongs in either parent,
  explain that boundary and ask the user for permission to propose it there.
  Do not prepare or send an upstream change on your own.
* Never introduce database migrations.
  These will conflict with Codex's own database migrations.
  If Frodex introduces a database migration on top of Codex, that's a mistake.
  Stop immediately and ask the user to fix it in Frodex.

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

A clean rebase is the new published Frodex release
plus the same intentional Saffrodex patch stack.
Rebase the Saffrodex-only commits from their old Frodex tag
onto the new Frodex tag.
Resolve conflicts by adopting the new parent structure,
then reapply only the Saffrodex behavior that still belongs in this fork.

Before publishing, establish that the new Frodex tag is an ancestor,
every later commit belongs to Saffrodex,
and a range diff against the prior tagged patch stack
contains no unexplained behavior change.
Check the resulting diff and run the tests required by the affected code.

Tag the current release before rewriting the rolling branch.
Never move or delete a published release tag.
Rewrite the rolling branch with force-with-lease semantics.

## Tags and versions

The package version is:

```text
<codex-version>+frodex.<frodex-release>.saffrodex.<saffrodex-release>
```

The release tag is `saffrodex-v<package-version>`.
For example:

```text
saffrodex-v0.148.0-alpha.5+frodex.0.saffrodex.0
```

Keep the current CLI identity in version output:

```text
codex-cli 0.148.0-alpha.5+frodex.0.saffrodex.0
```
