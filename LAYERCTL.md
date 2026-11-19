layerctl
========

Saffrodex uses [standalone layerctl](https://github.com/NonLogicalDev/util.layerctl)
to materialize, capture, and verify its layers.
Install the pinned tool with [mise](https://mise.jdx.dev/):

```sh
mise trust
mise install --locked github:NonLogicalDev/util.layerctl
mise exec -- layerctl --version
```

`mise.toml` pins the tool version.
`mise.lock` records release archives and checksums for Linux and macOS,
on x86_64 and ARM64.
CI uses the official `jdx/mise-action` to install this dependency.
Commands below run from the definition repository unless stated otherwise.
Python 3.11 or newer is required for the release-source reader.

Layer contents
--------------

Each `layers/NNNN-layer-slug/layer.toml` defines a generated commit through
`[vcs] commit-msg` and an ordered list of `[[operation]]` entries.
The foundation's first `git-clone` operation pins the official Codex URL,
release tag, and commit and imports it at the projection root.
Subsequent operations apply patches and overlays in manifest order.
A layer can contain multiple patches but produces one generated commit.

Patch paths and overlay directories are relative to their layer directory.
Patches hold changes to inherited files, deletions, type changes, and gitlinks.
Overlays hold complete added files, preserving binary content, executable bits,
and symlinks.
An overlay cannot silently replace an inherited path; reconcile collisions
when the upstream source changes.
Keep manifests intended for `layer refresh` free of comments:
the pinned tool refuses to rewrite commented manifests.

Projections and authoring
------------------------

Projections are independent Git repositories at caller-selected paths.
Their commits and objects are not automatically present in the definition
repository, and they do not use linked-worktree projection refs.
Use `projection list` and `projection path <name>` to locate them.
Do not encode machine-specific workspace paths in repository files.

Read the projected tree's `AGENTS.md` before changing Codex or Saffron source.
Create new feature work from its generated predecessor:

```sh
mise exec -- layerctl projection create edit-new \
  --worktree <path> --through <predecessor>
# Edit and commit in <path>.
mise exec -- layerctl layer add <NNNN-layer-slug> --from edit-new
```

Choose the next available four-digit layer ID.
To revise an existing layer, project through it and commit additional edits:

```sh
mise exec -- layerctl projection create edit-existing \
  --worktree <path> --through <layer>
# Edit and commit in <path>; leave the workspace clean.
mise exec -- layerctl layer refresh <layer> --from edit-existing
```

Refresh preserves existing operations and appends a patch for the committed edits.
It replays the candidate and compares its tree, message, and imported parents.
The hydrated source diff is the review surface; inspect the captured manifest
and patch before committing the definition changes.
Layerctl does not preserve the original commit authorship.

Advance upstream
----------------

Select the highest published Codex semantic version, including prereleases.
Update the foundation's `git-clone` `ref` and `commit` together, using the
published `rust-v*` tag and its peeled commit from OpenAI.
Keep the previous definitions in Git until the new stack is verified.
`python3 tools/upstream.py` prints the configured tag and commit as TSV;
CI and releases fetch the official tag and require it to point to that commit.
Layerctl's own reachability check alone does not enforce this release policy.

```sh
mise exec -- layerctl projection create upstream-advance --worktree <path>
# Resolve the reported conflict and stage the resolved files in <path>.
mise exec -- layerctl projection continue upstream-advance
```

`projection continue` finishes the workspace, not the stored definition repair.
Update the affected patch or overlay explicitly, preserving the resolved source.
When regenerating a patch, diff against the tree before that operation and keep
later operations separate; an appended patch cannot fix an earlier operation
that fails to apply.
Do not change definitions while continuing their interrupted projection:
finish or preserve the resolution first, then update definitions and rebuild.
Review the resolved source diff and verify a fresh projection reproduces it.
A clean upstream advance needs only the source-pin update when all existing
operations still apply.

Cargo can rewrite local-package versions in `codex-rs/Cargo.lock`.
Do not capture this churn: retain upstream versions and use `0.0.0` for new
local packages.

Verification and cleanup
------------------------

Run `mise exec -- layerctl check` before handoff or release.
Verify the complete stack in a fresh definition checkout that obtains its
source only from the configured official upstream release.
It must not depend on previous projection objects or Saffrodex release tags.
If layer operations include scripts, review their effects before replay:
create, refresh, and check can execute them repeatedly.

After preserving accepted work, remove its disposable projection:

```sh
mise exec -- layerctl projection delete <name>
```

Deletion also removes uncaptured work.
After an interrupted operation whose effects are uncertain, inspect and preserve
needed work before deleting and recreating the projection.
Use `projection unlock <name>` only after verifying no layerctl process owns it.
Existing projections created by the embedded tool remain separate;
standalone layerctl does not adopt or delete them.

Release
-------

Layerctl does not publish releases.
Run `tools/release.sh [-b RELEASE_NOTES]` to dispatch from the remote default branch.
The workflow verifies the foundation's Codex release identity, creates a full
projection, and chooses the next available Saffrodex release number.
It stamps versions in the projection, fetches the resulting commit into the
throwaway release checkout, and pushes an immutable tag from that checkout.
All platform builds consume that tag.
Release dispatch accepts neither a projection ref nor a version.
Never change a published Saffrodex tag.
