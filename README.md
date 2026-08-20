# Saffrodex

Saffrodex is a small downstream distribution of
[OpenAI Codex](https://github.com/openai/codex).
It follows released Codex versions and carries a deliberately narrow patch
stack for capabilities maintained in this repository.

Saffrodex currently adds:

- `saffron.await_exec`, which waits efficiently for a running exec session to
  produce output, exit, or reach a caller-selected timeout; and
- a live-process goal supervisor that can revisit an active root goal after
  the parent becomes idle.

The goal supervisor is intentionally process-local.
Pending supervisor snoozes and retries do not survive a Codex process restart.
Saffrodex does not add database migrations or rewrite Codex rollouts.

## Releases

Release versions preserve the upstream Codex version:

```text
<codex-version>+saffrodex.<release>
```

For example, `0.149.0-alpha.7+saffrodex.0` is the first Saffrodex release
based on Codex `0.149.0-alpha.7`.
Release tags use the form `saffrodex-v<version>`.

Download Saffrodex binaries from this repository's
[GitHub releases](https://github.com/abg-OAI/codex/releases).
The executable remains named `codex` and reports the full Saffrodex version
through `codex --version`.

## Building

Follow the upstream
[Codex build instructions](https://github.com/openai/codex/blob/main/docs/install.md).
The Rust workspace lives in `codex-rs/`.

## Upstream documentation

Saffrodex retains Codex's ordinary command-line behavior,
authentication, configuration, and platform support unless this README says
otherwise.
Use the [Codex documentation](https://developers.openai.com/codex)
for those shared capabilities.

Saffrodex is not an OpenAI release.
This repository remains licensed under the [Apache-2.0 License](LICENSE).
