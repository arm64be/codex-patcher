# Codex Patcher

Codex Patcher keeps a local Codex CLI on your own patch stack. It builds Codex
from [OpenAI's source](https://github.com/openai/codex), applies your patches in
a repeatable order, validates the result, and then redirects only the launchers
you choose.

It is designed for people who want a patched `codex` command without babysitting
source checkouts, rebuilds, or stale shims.

## Quick Start

Install Git, Python 3, Rustup/Cargo, and the native build tools required by the
upstream Codex package builder. Builds are native on Linux, macOS, and Windows,
including x86-64 and ARM64 runners.

```sh
cargo build --release --locked
target/release/codex-patcher scan
target/release/codex-patcher install /path/to/patch-directory
```

`install` builds and validates the patched Codex package before it changes a
launcher. Keep the `codex-patcher` management binary somewhere outside the
launcher paths you select.

## Patch Directory

Put `codex-patcher.toml` next to your patch files:

```toml
schema = 1
branch = "stable"
target = "official-native"
failure_mode = "error"
noninteractive_pending = "auto"
```

`branch` can be `stable`, `alpha`, or `nightly`. `target` is
`official-native` or a supported same-host target triple.

`failure_mode` controls what happens when a build fails:

- `error` stops instead of running an older generation.
- `last-good` keeps using the last validated generation when that is safe.

`noninteractive_pending` controls service and protocol launches when an update is
waiting:

- `auto` follows the failure mode.
- `warn-run` prints a warning and runs the current generation.
- `error` exits instead of starting Codex.

If a `series` file exists, it defines the patch order with one relative patch
path per line. Blank lines and `#` comments are allowed. Without `series`, all
regular `*.patch` files are applied recursively in bytewise path order.

Codex Patcher rejects absolute paths, traversal, symlinks, duplicates,
case-folding collisions, missing files, and unlisted patches.

## How Launches Work

There is no daemon or watcher. A wrapped `codex` launch does three small things:

1. Reads the last saved update check.
2. Starts one detached background probe if another probe is not already running.
3. Immediately runs the active validated generation, or shows an update prompt
   if a previous probe found new source.

The probe never builds anything for the launch that started it. If launch A finds
an update in the background, launch B is the first one that can offer to build
it.

Interactive launches show a Codex-style prompt. Service, protocol, and other
noninteractive launches follow `noninteractive_pending`. A wrapped
`codex update` is routed to `codex-patcher update`.

Arguments, working directory, environment, stdio, signals, and exit status are
preserved. Codex's own startup update prompt is disabled inside managed
generations.

## Commands

Run management commands through the unwrapped `codex-patcher` binary:

```sh
codex-patcher scan
codex-patcher status
codex-patcher update [--retry] [--accept-retag] [--accept-force-push]
codex-patcher repair [FAILURE_ID]
codex-patcher repair-shims
codex-patcher uninstall
codex-patcher gc
```

Retags, deleted releases, downgrades, and non-fast-forward nightly movement need
explicit acceptance flags. Deterministic failures are cached until inputs change
or you pass `--retry`.

## Recovery

`scan` reports each launcher separately, including owner, precedence, resolved
identity, and overwrite risk. Protected or signed launchers stay visible but are
not modified.

If another installer overwrites a managed launcher, `status` reports drift.
`repair-shims --adopt-drift --yes` can adopt that new file as the restore
baseline and then reapply the dispatcher. Repair and uninstall use
compare-and-swap checks, so drifted paths are reported instead of overwritten.

`repair [FAILURE_ID]` recreates the failed source and patch stack in a temporary
worktree, launches the pinned last-good Codex for a repair pass, rebuilds the
result, and shows every patch-file change before confirmation.

`uninstall --yes` restores unchanged baselines and removes patcher state only
when no build or generation lease is active.

## State And Trust

Codex Patcher keeps its mirror, worktrees, generations, logs, backups, and caches
under per-user application-data directories. Set `CODEX_PATCHER_HOME` to place
all patcher-owned state under one explicit root.

Builds use the upstream package builder and run with the current user's
privileges. Patch application is strict: `git apply --check` followed by
`git apply --index`, with no fuzzy or three-way fallback.

Compiler output is kept in a persistent `CARGO_TARGET_DIR` partitioned by
compatible target, Cargo profile, and toolchain identity. Build failures and
garbage collection do not delete that cache; uninstall waits for active builds
before removing it.
