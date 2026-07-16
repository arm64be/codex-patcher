# Codex Patcher

Keep a local patch stack on the Codex CLI without babysitting rebuilds. Codex Patcher builds complete, validated packages from [OpenAI's Codex source](https://github.com/openai/codex), redirects the `codex` launchers you choose, and atomically switches between immutable patched generations.

## Quick start

You need Git, Python 3, Rustup/Cargo, and the native tools required by Codex's upstream package builder. Builds are native-only on Linux, macOS, and Windows (x86-64 and ARM64); Linux defaults to the official MUSL target and may use the matching GNU target.

```sh
cargo build --release --locked
target/release/codex-patcher scan
target/release/codex-patcher install /path/to/patch-directory
```

`install` builds and validates Codex before touching anything, then lets you select one or more discovered command surfaces. Keep the resulting `codex-patcher` management command outside those selected paths.

## How launches work

There is no watcher, timer, or repair service. Every wrapped launch:

1. Reads the freshness result saved by an earlier launch.
2. Starts one detached, coalesced probe and does not wait for it.
3. Immediately runs the active generation when the saved result is current, or shows a Codex-style update screen when that result is pending.

The new probe only resolves and records the desired source; it never builds or activates anything. Its result cannot affect the launch that started it: launch A may discover an update in the background, and launch B offers **Build patched update now** or **Exit**. Service and other noninteractive launches never open a TUI and follow the configured pending policy. A wrapped `codex update` runs `codex-patcher update`.

The dispatcher preserves arguments, working directory, environment, stdio, signals, and exit status. It keeps protocol stdout clean and disables Codex's own startup update prompt.

## Patch directory

Place `codex-patcher.toml` beside your patches:

```toml
schema = 1
branch = "stable"
target = "official-native"
failure_mode = "error"
noninteractive_pending = "auto"
```

`branch` is `stable`, `alpha` (published prereleases), or `nightly` (the current `main` commit). `target` is `official-native` or a supported same-host triple. `failure_mode` is `error` or `last-good`; `noninteractive_pending` is `auto`, `warn-run`, or `error`, with `auto` deriving from the failure mode. Unknown keys and unsupported values fail validation.

If `series` exists, it is authoritative: use one UTF-8 relative patch path per line; blank lines and `#` comments are allowed. Otherwise all regular `*.patch` files are applied recursively in bytewise relative-path order. Absolute paths, traversal, symlinks, duplicates, case-folding collisions, missing files, and unlisted patches are rejected. Fingerprints cover ordered names and exact bytes, not mtimes.

## Manage

The management commands are `scan`, `status`, `update [--retry] [--accept-retag] [--accept-force-push]`, `repair [FAILURE_ID]`, `repair-shims`, `uninstall`, and `gc`; run them as `codex-patcher <command>`.

Retags, release deletion or downgrade, and non-fast-forward `main` changes require the corresponding explicit acceptance flag. Deterministic failures stay cached until inputs change or `--retry` is supplied; a warm-generation network failure is reported as degraded, not as a confirmed update.

## Recovery and takeover

Discovery lists each launcher separately, including owner, precedence, resolved identity, and overwrite risk. Signed bundles and protected command surfaces remain visible but read-only. Recognized updaters are disabled only through reversible adapters; privileged takeover or restore elevates only that narrow operation.

An installation owner may later overwrite a dispatcher. `status` detects drift; `repair-shims --adopt-drift --yes` can explicitly adopt the new artifact as the restore baseline and reapply the dispatcher. Repair and uninstall use compare-and-swap identity checks, so a drifted path is reported and left untouched rather than clobbered. `uninstall --yes` restores unchanged baselines and removes patcher state only when no build or generation lease is in use.

For patch failures, `repair [FAILURE_ID]` reconstructs the exact failed source and patch commits in a disposable worktree, launches the pinned last-good Codex with workspace-write access, regenerates the complete normalized patch stack, rebuilds it, and shows every patch-file change before asking for confirmation.

## Build and security model

Codex Patcher keeps a private Git mirror, isolated worktrees, exact source identities, and strict `git apply --check` then `git apply --index` semantics—never fuzzy or three-way fallback. It uses Codex's [canonical package builder](https://github.com/openai/codex/tree/main/scripts/codex_package), validates package metadata, resources, CLI help/version, and app-server behavior, then rechecks live inputs before atomic activation.

Compiler output lives in a persistent `CARGO_TARGET_DIR`, partitioned only by compatible target, Cargo profile, and toolchain identity—not by commit, patch fingerprint, source key, generation, or staging path. Updates therefore reuse incremental artifacts. Build failure and `gc` never delete this cache; uninstall waits for any build before removing it.

State, logs, backups, generations, mirror, and caches use platform per-user application-data locations. `CODEX_PATCHER_HOME` places all patcher-owned data under one explicit root. Patched generations stay outside Codex's official standalone tree. Builds run trusted upstream and patch code with the current user's privileges.
