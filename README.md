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
cargo install --git https://github.com/arm64be/codex-patcher --locked
codex-patcher quickstart
codex-patcher install
```

`install` builds and validates the patched Codex package before it changes a
launcher. With no patch-directory argument, it uses the directory created by
`quickstart`. Keep the `codex-patcher` management binary somewhere outside the
launcher paths you select. If `CODEX_HOME` is unset, `quickstart` uses
`~/.codex/codex-patcher`.

## Patch Directory

Put `codex-patcher.toml` next to your patch files:

```toml
schema = 1
branch = "stable"
target = "native"
failure_mode = "error"
auto_rebuild_patches = true
noninteractive_pending = "auto"
```

`branch` can be `stable`, `alpha`, or `nightly`. `target` is
`native` or a supported same-host target triple. On Linux, `native` uses the
host's normal GNU ABI; choose a `*-musl` triple explicitly only when its Rust
target and native build tools are installed.

`failure_mode` controls what happens when a build fails:

- `error` stops instead of running an older generation.
- `last-good` keeps using the last validated generation when that is safe.

`auto_rebuild_patches` defaults to `true`, including for existing configs that
omit it. When only the local patch stack changes, the next managed Codex launch
rebuilds and activates that same Codex version before starting it. Set it to
`false` to require the normal update prompt or noninteractive policy instead.

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

`quickstart` creates a patch directory with a default config, a `series` file,
an `AGENTS.md`, and one example patch. The example adds a `Codex Patcher` line
to Codex's `/status` screen so you can confirm the patched generation is the one
running.

## How Launches Work

There is no daemon or watcher. A wrapped `codex` launch does three small things:

1. Fingerprints the live config and patch stack.
2. Reuses the recorded upstream result until its HTTP polling floor expires;
   when revalidation is due, resolves it synchronously with a three-second total
   network budget.
3. Runs the active validated generation, automatically rebuilds a same-version
   patch change, or shows an update prompt for a new Codex source.

Patch edits and upstream changes are therefore acted on by the same launch that
detects them. An ordinary warm launch reads the small state, config, and patch
inputs but does not parse the upstream HTTP cache, perform network I/O, or
rewrite state; this path is intended to add only a few milliseconds before
Codex starts. When the polling floor expires, authoritative GitHub revalidation
is synchronous and can take up to the three-second network budget. If GitHub is
temporarily unreachable, the launch uses the last trusted cached response and
waits briefly before retrying; a local patch change can still rebuild against
the active Codex source.

Interactive launches show a Codex-style prompt for source or configuration
changes that are not eligible for automatic patch rebuilding. Service,
protocol, and other noninteractive launches follow `noninteractive_pending`
when an update cannot be applied automatically. A wrapped `codex update` is
routed to `codex-patcher update`.

Arguments, working directory, environment, stdio, signals, and exit status are
preserved. Codex's own startup update prompt is disabled inside managed
generations.

## Commands

Run management commands through the unwrapped `codex-patcher` binary:

```sh
codex-patcher quickstart [--force]
codex-patcher install [PATCH_DIR] [--surface PATH] [--yes]
codex-patcher scan [--verbose | --json]
codex-patcher status
codex-patcher update [--retry] [--accept-retag] [--accept-force-push]
codex-patcher force-rebuild [--accept-retag] [--accept-force-push]
codex-patcher repair-shims
codex-patcher uninstall
codex-patcher gc
```

Retags, deleted releases, downgrades, and non-fast-forward nightly movement need
explicit acceptance flags. Deterministic failures are cached until inputs change
or you pass `--retry`. `force-rebuild` bypasses both the failure cache and the
validated-generation reuse path, then replaces the existing generation only
after the rebuild validates.

## Recovery

`scan` shows a compact, deduplicated launcher summary. `scan --verbose` includes
discovery origins, resolved filesystem identity, and overwrite risk;
`scan --json` exposes the same diagnostic model for automation. Protected or
signed launchers stay visible but are not modified.

If another installer overwrites a managed launcher, `status` reports drift.
`repair-shims --adopt-drift --yes` can adopt that new file as the restore
baseline and then reapply the dispatcher. Shim repair and uninstall use
compare-and-swap checks, so drifted paths are reported instead of overwritten.

`uninstall --yes` restores unchanged baselines and removes patcher state only
when no build or generation lease is active.

## State And Trust

Codex Patcher keeps its mirror, worktrees, generations, logs, backups, and caches
under per-user application-data directories. Set `CODEX_PATCHER_HOME` to place
all patcher-owned state under one explicit root.

Builds use the upstream package builder and run with the current user's
privileges. Patch application is strict: `git apply --check` followed by
`git apply --index`, with no fuzzy or three-way fallback.

Patched Codex generations use upstream's `dev-small` profile, which is intended
for fast local iteration and does not run release ThinLTO after every patch.
The checkout path and `CARGO_TARGET_DIR` are stable for each compatible target
and profile, allowing Cargo to reuse real incremental artifacts instead of
treating every generated worktree as a new workspace. Build failures and
garbage collection do not delete that cache; uninstall waits for active builds
before removing it.
