use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use codex_patcher::config::Config;
use codex_patcher::discovery::{Redirectability, SurfaceCandidate, SurfaceOwner, discover};
use codex_patcher::dispatch::{
    UpdateOptions, dispatch, foreground_update, foreground_update_locked,
    handle_interactive_failure,
};
use codex_patcher::elevation;
use codex_patcher::patchset::PatchSet;
use codex_patcher::paths::PatcherPaths;
use codex_patcher::probe;
use codex_patcher::repair;
use codex_patcher::shim::{
    RepairOutcome, apply_codex_update_manager_disable, finalize_redirect, finalize_repair_redirect,
    finalize_uninstall_redirect, inspect_codex_update_manager, matches_recorded_shim,
    restore_codex_update_manager, validate_surface_launcher_type,
};
use codex_patcher::state::{InstallState, StateStore};
use dialoguer::{Confirm, MultiSelect, theme::ColorfulTheme};
use directories::BaseDirs;
use fs2::FileExt;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
#[cfg(not(windows))]
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(name = "codex-patcher", version, about)]
struct ManagerCli {
    #[command(subcommand)]
    command: ManagerCommand,
}

#[derive(Debug, Subcommand)]
enum ManagerCommand {
    /// Create a starter patch directory under CODEX_HOME/codex-patcher.
    Quickstart {
        /// Overwrite the starter files if they already exist.
        #[arg(long)]
        force: bool,
    },
    /// Build the first patched generation and take over selected Codex launchers.
    Install {
        patch_dir: PathBuf,
        /// Select exact surfaces without the interactive multi-select prompt.
        #[arg(long = "surface")]
        surfaces: Vec<PathBuf>,
        #[arg(long)]
        yes: bool,
    },
    /// Discover concrete Codex launch surfaces without modifying them.
    Scan {
        #[arg(long)]
        json: bool,
    },
    /// Show active, desired, failure, and shim ownership state.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Resolve, build, validate, and activate the current desired source.
    Update {
        #[arg(long)]
        retry: bool,
        #[arg(long)]
        accept_retag: bool,
        #[arg(long)]
        accept_force_push: bool,
    },
    /// Resume maintenance for a recorded failed generation.
    Repair { failure_id: Option<String> },
    /// Reapply externally overwritten dispatcher surfaces.
    RepairShims {
        #[arg(long)]
        adopt_drift: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Restore recorded launch surfaces using compare-and-swap checks.
    Uninstall {
        #[arg(long)]
        yes: bool,
    },
    /// Delete unused immutable generations and abandoned staging directories.
    Gc,
    #[command(hide = true, name = "__probe")]
    InternalProbe,
}

fn main() {
    if let Err(error) = real_main() {
        eprintln!("codex-patcher: {error:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let raw: Vec<OsString> = std::env::args_os().collect();
    if let Some(code) = elevation::helper_entrypoint(&raw)? {
        return finish(code);
    }
    let paths = PatcherPaths::discover()?;
    if raw.get(1).is_some_and(|argument| argument == "__dispatch") {
        return finish(dispatch(&paths, raw.get(2..).unwrap_or_default())?);
    }
    if raw.get(1).is_some_and(|argument| argument == "__probe") {
        return probe::run_internal(&paths);
    }
    if invoked_as_codex(raw.first()) {
        return finish(dispatch(&paths, raw.get(1..).unwrap_or_default())?);
    }

    match ManagerCli::parse().command {
        ManagerCommand::Quickstart { force } => quickstart(force),
        ManagerCommand::Install {
            patch_dir,
            surfaces,
            yes,
        } => install(&paths, &patch_dir, &surfaces, yes),
        ManagerCommand::Scan { json } => scan(&paths, json),
        ManagerCommand::Status { json } => status(&paths, json),
        ManagerCommand::Update {
            retry,
            accept_retag,
            accept_force_push,
        } => update(
            &paths,
            UpdateOptions {
                retry,
                accept_retag,
                accept_force_push,
                interactive: terminal_interactive(),
            },
        ),
        ManagerCommand::Repair { failure_id } => repair_command(&paths, failure_id.as_deref()),
        ManagerCommand::RepairShims { adopt_drift, yes } => repair_shims(&paths, adopt_drift, yes),
        ManagerCommand::Uninstall { yes } => uninstall(&paths, yes),
        ManagerCommand::Gc => gc(&paths),
        ManagerCommand::InternalProbe => probe::run_internal(&paths),
    }
}

const QUICKSTART_CONFIG: &str = r#"schema = 1
branch = "stable"
target = "official-native"
failure_mode = "error"
noninteractive_pending = "auto"
"#;

const QUICKSTART_SERIES: &str = "status-line.patch\n";

const QUICKSTART_STATUS_PATCH: &str = r#"diff --git a/codex-rs/tui/src/status/card.rs b/codex-rs/tui/src/status/card.rs
--- a/codex-rs/tui/src/status/card.rs
+++ b/codex-rs/tui/src/status/card.rs
@@ -744,6 +744,7 @@ impl StatusCard {
         if self.model_provider.is_some() {
             push_label(&mut labels, &mut seen, "Model provider");
         }
+        push_label(&mut labels, &mut seen, "Codex Patcher");
         if account_value.is_some() {
             push_label(&mut labels, &mut seen, "Account");
         }
@@ -831,6 +832,10 @@ impl StatusCard {
         lines.push(formatter.line("Directory", vec![Span::from(directory_value)]));
         lines.push(formatter.line("Permissions", vec![Span::from(self.permissions.clone())]));
         lines.push(formatter.line("Agents.md", vec![Span::from(agents_summary)]));
+        lines.push(formatter.line(
+            "Codex Patcher",
+            vec![Span::from("quickstart patch is in")],
+        ));
 
         if let Some(account_value) = account_value {
             lines.push(formatter.line("Account", vec![Span::from(account_value)]));
"#;

const QUICKSTART_AGENTS: &str = r#"# codex-patcher Patch Directory

This directory is managed by codex-patcher: https://github.com/arm64be/codex-patcher

Keep `codex-patcher.toml`, `series`, and every `*.patch` file in this directory
small and reviewable. The `series` file is the patch order; list every patch
there, one relative path per line.

Use this workflow:

1. Edit or add patch files here.
2. Run `codex-patcher update --retry` to rebuild the patched Codex generation.
3. Run `/status` in Codex and check the `Codex Patcher` line when using the
   starter patch.

Avoid symlinks, absolute paths, path traversal, duplicate patch names, and
case-only filename differences. codex-patcher rejects those inputs so the same
patch stack behaves consistently across Linux, macOS, and Windows.
"#;

fn quickstart(force: bool) -> Result<()> {
    let patch_dir = quickstart_patch_dir()?;
    write_quickstart(&patch_dir, force)?;
    println!(
        "created quickstart patch directory: {}",
        patch_dir.display()
    );
    println!("next: codex-patcher install {}", patch_dir.display());
    Ok(())
}

fn quickstart_patch_dir() -> Result<PathBuf> {
    let codex_home = match std::env::var_os("CODEX_HOME") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => BaseDirs::new()
            .context("locating home directory for default CODEX_HOME")?
            .home_dir()
            .join(".codex"),
    };
    Ok(codex_home.join("codex-patcher"))
}

fn write_quickstart(patch_dir: &Path, force: bool) -> Result<()> {
    fs::create_dir_all(patch_dir)
        .with_context(|| format!("creating patch directory {}", patch_dir.display()))?;
    write_quickstart_file(
        &patch_dir.join("codex-patcher.toml"),
        QUICKSTART_CONFIG,
        force,
    )?;
    write_quickstart_file(&patch_dir.join("series"), QUICKSTART_SERIES, force)?;
    write_quickstart_file(
        &patch_dir.join("status-line.patch"),
        QUICKSTART_STATUS_PATCH,
        force,
    )?;
    write_quickstart_file(&patch_dir.join("AGENTS.md"), QUICKSTART_AGENTS, force)?;
    Config::load(patch_dir.join("codex-patcher.toml"))?;
    PatchSet::load(patch_dir)?;
    Ok(())
}

fn write_quickstart_file(path: &Path, contents: &str, force: bool) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(force);
    if !force {
        options.create_new(true);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

fn finish(code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        std::process::exit(code)
    }
}

fn update(paths: &PatcherPaths, options: UpdateOptions) -> Result<()> {
    let generation = match foreground_update(paths, options) {
        Ok(generation) => generation,
        Err(error) if options.interactive => {
            eprintln!("update failed: {error:#}");
            return finish(handle_interactive_failure(
                paths,
                &StateStore::new(paths.clone()).require()?,
                None,
            )?);
        }
        Err(error) => return Err(error),
    };
    eprintln!(
        "activated Codex {} ({})",
        generation.source.version,
        &generation.source.commit_oid[..12]
    );
    Ok(())
}

fn invoked_as_codex(argument_zero: Option<&OsString>) -> bool {
    let Some(stem) = argument_zero
        .and_then(|path| Path::new(path).file_stem())
        .and_then(|stem| stem.to_str())
    else {
        return false;
    };
    stem.eq_ignore_ascii_case("codex")
}

fn install(
    paths: &PatcherPaths,
    patch_dir: &Path,
    explicit_surfaces: &[PathBuf],
    yes: bool,
) -> Result<()> {
    paths.ensure()?;
    let store = StateStore::new(paths.clone());
    let _manager_lock = store.manager_lock()?;
    store.recover_surface_transactions()?;
    if store.load()?.is_some() {
        bail!("codex-patcher is already installed; uninstall it before changing patch roots");
    }
    let patch_dir = patch_dir
        .canonicalize()
        .with_context(|| format!("canonicalizing patch directory {}", patch_dir.display()))?;
    Config::load(patch_dir.join("codex-patcher.toml"))?;
    PatchSet::load(&patch_dir)?;

    let candidates = discover(paths)?;
    print_candidates(&candidates);
    let selected = select_surfaces(paths, &candidates, explicit_surfaces, yes)?;
    if selected.is_empty() {
        bail!("no redirectable Codex surfaces were selected");
    }
    for surface in &selected {
        validate_surface_launcher_type(surface)?;
    }
    eprintln!("\nSelected takeover paths:");
    for surface in &selected {
        eprintln!("  {}", surface.display());
    }

    let selected_candidates = selected_candidates(&candidates, &selected)?;
    let owner_managed_selected = selected_candidates
        .iter()
        .any(|candidate| candidate.redirectability == Redirectability::OwnerManaged);
    let system_updater_owner_selected = selected_candidates.iter().any(|candidate| {
        matches!(
            candidate.owner,
            SurfaceOwner::Standalone | SurfaceOwner::Desktop | SurfaceOwner::Daemon
        )
    });
    let mut warned_owner_paths = HashSet::new();
    for candidate in selected_candidates.iter().filter(|candidate| {
        candidate.redirectability == Redirectability::OwnerManaged
            && warned_owner_paths.insert(candidate.raw.clone())
    }) {
        eprintln!(
            "WARNING: {} is owned by {} ({}); that owner can overwrite the dispatcher, and recovery is manual via `codex-patcher repair-shims`",
            candidate.raw.display(),
            format!("{:?}", candidate.owner).to_lowercase(),
            candidate.update_method.label()
        );
    }
    let updater_plan = if system_updater_owner_selected {
        inspect_codex_update_manager(Duration::from_secs(8))?
    } else {
        None
    };
    if let Some(updater) = updater_plan.as_ref() {
        eprintln!(
            "Updater collateral: {} is load={} enabled={} active={}",
            updater.unit, updater.load_state, updater.enabled_state, updater.active_state
        );
        if updater.stop_intended {
            eprintln!("  installation will stop this user service after the build validates");
        }
        if updater.disable_intended {
            eprintln!("  installation will disable this user service after the build validates");
        }
        for note in &updater.notes {
            eprintln!("  warning: {note}");
        }
    } else if owner_managed_selected {
        eprintln!(
            "WARNING: no reversible updater adapter covers every selected owner-managed surface"
        );
    }
    if !yes
        && !Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Build Codex and replace these paths in place?")
            .default(false)
            .interact()?
    {
        bail!("installation cancelled");
    }

    let manager_copied = install_manager_binary(paths)?;
    store.save(&InstallState::new(patch_dir))?;
    let initial = match foreground_update_locked(
        paths,
        UpdateOptions {
            interactive: terminal_interactive(),
            ..UpdateOptions::default()
        },
    ) {
        Ok(initial) => initial,
        Err(error) => {
            rollback_incomplete_install(paths, &store, manager_copied)?;
            return Err(error).context("building the initial patched generation");
        }
    };
    eprintln!(
        "validated initial patched Codex {} ({})",
        initial.source.version,
        &initial.source.commit_oid[..12]
    );

    if let Some(mut updater) = updater_plan {
        // Save reversible intent before the first systemctl mutation. If the
        // process dies between the action and its completion write, uninstall
        // can idempotently restore the recorded original state.
        if let Err(error) = store.with_state_lock(|| {
            let mut state = store.require()?;
            if !state.updaters.contains(&updater) {
                state.updaters.push(updater.clone());
            }
            store.save(&state)
        }) {
            rollback_incomplete_install(paths, &store, manager_copied)?;
            return Err(error).context("recording updater takeover intent");
        }
        if let Err(error) = apply_codex_update_manager_disable(&mut updater, Duration::from_secs(8))
        {
            rollback_incomplete_install(paths, &store, manager_copied)?;
            return Err(error).context("disabling the existing Codex updater");
        }
        if let Err(error) = store.with_state_lock(|| {
            let mut state = store.require()?;
            let recorded = state
                .updaters
                .iter_mut()
                .find(|recorded| recorded.unit == updater.unit)
                .context("updater takeover intent disappeared")?;
            *recorded = updater.clone();
            store.save(&state)
        }) {
            rollback_incomplete_install(paths, &store, manager_copied)?;
            return Err(error).context("recording completed updater takeover");
        }
        for note in &updater.notes {
            eprintln!("warning: {note}");
        }
        if (updater.disable_intended && !updater.disabled_by_patcher)
            || (updater.stop_intended && !updater.stopped_by_patcher)
        {
            eprintln!(
                "WARNING: the updater could not be fully disabled and may overwrite a selected dispatcher; recovery is manual via `codex-patcher repair-shims`"
            );
        }
    }

    for surface in &selected {
        match elevation::install_redirect(surface, &paths.manager, &paths.backups_dir) {
            Ok(record) => {
                if let Err(error) = store.with_state_lock(|| {
                    let mut state = store.require()?;
                    state.surfaces.push(record.clone());
                    store.save(&state)
                }) {
                    // The ownership record was not made durable. Restore the
                    // just-created shim before rolling back earlier entries.
                    if elevation::uninstall_redirect(&record, &paths.backups_dir).is_ok() {
                        let _ = finalize_uninstall_redirect(&record, &paths.backups_dir);
                    }
                    rollback_incomplete_install(paths, &store, manager_copied)?;
                    return Err(error).context("recording launcher takeover");
                }
                if let Err(error) = finalize_redirect(&record, &paths.backups_dir) {
                    rollback_incomplete_install(paths, &store, manager_copied)?;
                    return Err(error).context("finalizing launcher takeover journal");
                }
            }
            Err(error) => {
                rollback_incomplete_install(paths, &store, manager_copied)?;
                return Err(error).with_context(|| format!("redirecting {}", surface.display()));
            }
        }
    }
    eprintln!(
        "codex-patcher installed; management binary: {}",
        paths.manager.display()
    );
    Ok(())
}

fn scan(paths: &PatcherPaths, json: bool) -> Result<()> {
    let candidates = discover(paths)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&candidates)?);
    } else {
        print_candidates(&candidates);
    }
    Ok(())
}

fn print_candidates(candidates: &[SurfaceCandidate]) {
    println!(
        "{:<4} {:<20} {:<12} {:<20} {:<12} {:<10} {:<10} path",
        "#", "precedence", "owner", "update", "redirect", "risk", "version"
    );
    for (index, candidate) in candidates.iter().enumerate() {
        println!(
            "{:<4} {:<20} {:<12} {:<20} {:<12} {:<10} {:<10} {}",
            index + 1,
            candidate.precedence.label(),
            format!("{:?}", candidate.owner).to_lowercase(),
            candidate.update_method.label(),
            format!("{:?}", candidate.redirectability).to_lowercase(),
            format!("{:?}", candidate.risk).to_lowercase(),
            candidate.version.as_deref().unwrap_or("unknown"),
            candidate.raw.display()
        );
        if candidate
            .resolved
            .as_deref()
            .is_some_and(|path| path != candidate.raw)
        {
            println!("     -> {}", candidate.display_path().display());
        }
        if let Some(identity) = candidate.file_identity.identity.as_ref() {
            let platform_identity = match (
                identity.device,
                identity.inode,
                identity.volume_serial,
                identity.file_id.as_deref(),
            ) {
                (Some(device), Some(inode), _, _) => format!("dev={device} ino={inode}"),
                (_, _, Some(volume), Some(file_id)) => {
                    format!("volume={volume} file={file_id}")
                }
                _ => "platform-id=unavailable".to_string(),
            };
            println!(
                "     identity {}: {platform_identity} bytes={} modified-ns={}",
                candidate
                    .file_identity
                    .path
                    .as_deref()
                    .unwrap_or_else(|| candidate.display_path())
                    .display(),
                identity.length,
                identity
                    .modified_ns
                    .map(|value| value.to_string())
                    .as_deref()
                    .unwrap_or("unknown")
            );
        } else if let Some(error) = candidate.file_identity.error.as_deref() {
            println!("     identity unavailable: {error}");
        }
        if candidate.redirectability == Redirectability::NotRedirectable {
            println!("     {}", candidate.risk_reason);
        }
    }
}

fn select_surfaces(
    paths: &PatcherPaths,
    candidates: &[SurfaceCandidate],
    explicit: &[PathBuf],
    yes: bool,
) -> Result<Vec<PathBuf>> {
    if !explicit.is_empty() {
        let mut unique = HashSet::new();
        let mut selected = Vec::new();
        for raw in explicit {
            let absolute = normalize_surface_path(raw)?;
            if absolute == paths.manager {
                bail!("the stable management binary cannot be selected as a Codex surface");
            }
            reject_protected_explicit_surface(&absolute)?;
            let metadata = fs::symlink_metadata(&absolute).with_context(|| {
                format!(
                    "explicit Codex surface does not exist or cannot be inspected: {}",
                    absolute.display()
                )
            })?;
            if !(metadata.is_file() || metadata.file_type().is_symlink()) {
                bail!(
                    "explicit Codex surface is not a file or symlink: {}",
                    absolute.display()
                );
            }
            if let Some(candidate) = candidates
                .iter()
                .find(|candidate| candidate.raw == absolute)
                && candidate.redirectability == Redirectability::NotRedirectable
            {
                bail!(
                    "surface {} is not redirectable: {}",
                    absolute.display(),
                    candidate.risk_reason
                );
            }
            if unique.insert(absolute.clone()) {
                selected.push(absolute);
            }
        }
        return Ok(selected);
    }
    if yes || !terminal_interactive() {
        bail!("noninteractive install requires at least one --surface PATH");
    }
    let selectable: Vec<_> = candidates
        .iter()
        .filter(|candidate| {
            candidate.exists
                && matches!(
                    candidate.redirectability,
                    Redirectability::Direct | Redirectability::OwnerManaged
                )
        })
        .collect();
    let labels: Vec<_> = selectable
        .iter()
        .map(|candidate| {
            format!(
                "{}  owner={:?} version={} risk={:?}",
                candidate.raw.display(),
                candidate.owner,
                candidate.version.as_deref().unwrap_or("unknown"),
                candidate.risk
            )
        })
        .collect();
    let defaults: Vec<_> = selectable
        .iter()
        .map(|candidate| candidate.current)
        .collect();
    let selected = MultiSelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Select Codex command surfaces to redirect")
        .items(&labels)
        .defaults(&defaults)
        .interact()?;
    deduplicate_surface_paths(
        selected
            .into_iter()
            .map(|index| selectable[index].raw.clone()),
    )
}

/// Normalize only the parent of a launcher path. Canonicalizing the complete
/// path would resolve the launcher's final symlink and collapse distinct
/// command surfaces onto their shared package executable. Canonicalizing the
/// parent still removes repeated PATH entries and lexical aliases such as
/// `bin/../bin/codex` without changing the surface that will be replaced.
fn normalize_surface_path(path: &Path) -> Result<PathBuf> {
    let absolute = absolute_path(path)?;
    let name = absolute
        .file_name()
        .context("Codex surface path must name a file")?;
    let parent = absolute
        .parent()
        .context("Codex surface path has no parent directory")?
        .canonicalize()
        .with_context(|| {
            format!(
                "canonicalizing Codex surface parent {}",
                absolute.parent().unwrap().display()
            )
        })?;
    Ok(parent.join(name))
}

fn deduplicate_surface_paths(paths: impl IntoIterator<Item = PathBuf>) -> Result<Vec<PathBuf>> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for path in paths {
        let normalized = normalize_surface_path(&path)?;
        if seen.insert(normalized.clone()) {
            unique.push(normalized);
        }
    }
    Ok(unique)
}

fn selected_candidates<'a>(
    candidates: &'a [SurfaceCandidate],
    selected: &[PathBuf],
) -> Result<Vec<&'a SurfaceCandidate>> {
    let selected: HashSet<_> = selected.iter().cloned().collect();
    let mut matches = Vec::new();
    for candidate in candidates.iter().filter(|candidate| candidate.exists) {
        if selected.contains(&normalize_surface_path(&candidate.raw)?) {
            matches.push(candidate);
        }
    }
    Ok(matches)
}

fn reject_protected_explicit_surface(path: &Path) -> Result<()> {
    let normalized = path.to_string_lossy().replace('\\', "/");
    let folded = normalized.to_ascii_lowercase();
    if folded.contains(".app/contents/")
        || folded.contains("/windowsapps/")
        || folded.ends_with("/windowsapps")
    {
        bail!(
            "refusing to replace protected or signed application contents directly: {}",
            path.display()
        );
    }
    Ok(())
}

fn install_manager_binary(paths: &PatcherPaths) -> Result<bool> {
    let source = std::env::current_exe().context("locating the running codex-patcher binary")?;
    if source == paths.manager {
        return Ok(false);
    }
    fs::create_dir_all(&paths.manager_dir)?;

    #[cfg(windows)]
    {
        let bytes = fs::read(&source)
            .with_context(|| format!("reading management binary {}", source.display()))?;
        codex_patcher::state::atomic_write(&paths.manager, &bytes)?;
        Ok(true)
    }

    #[cfg(not(windows))]
    {
        let temporary = paths
            .manager_dir
            .join(format!(".codex-patcher-{}.tmp", Uuid::new_v4()));
        fs::copy(&source, &temporary).with_context(|| {
            format!(
                "copying management binary from {} to {}",
                source.display(),
                temporary.display()
            )
        })?;
        let permissions = fs::metadata(&source)?.permissions();
        fs::set_permissions(&temporary, permissions)?;
        OpenOptions::new()
            .write(true)
            .open(&temporary)?
            .sync_all()?;
        fs::rename(&temporary, &paths.manager)?;
        Ok(true)
    }
}

/// Best-effort transactional rollback for an install that failed after its
/// initial generation was validated. Any launcher that no longer matches our
/// exact ownership record is retained in state and reported instead of being
/// overwritten.
fn rollback_incomplete_install(
    paths: &PatcherPaths,
    store: &StateStore,
    manager_copied: bool,
) -> Result<()> {
    let mut rollback_errors = Vec::new();
    let state = store.require()?;
    for record in state.surfaces.iter().rev() {
        match elevation::uninstall_redirect(record, &paths.backups_dir) {
            Ok(()) => {
                store.with_state_lock(|| {
                    let mut latest = store.require()?;
                    latest.surfaces.retain(|entry| entry.id != record.id);
                    store.save(&latest)
                })?;
                finalize_uninstall_redirect(record, &paths.backups_dir)?;
            }
            Err(error) => rollback_errors.push(format!("{}: {error:#}", record.surface.display())),
        }
    }

    let mut latest = store.require()?;
    if latest.surfaces.is_empty() {
        for updater in latest.updaters.clone() {
            match restore_codex_update_manager(&updater, Duration::from_secs(8)) {
                Ok(()) => {
                    store.with_state_lock(|| {
                        let mut current = store.require()?;
                        current.updaters.retain(|entry| entry != &updater);
                        store.save(&current)
                    })?;
                }
                Err(error) => rollback_errors.push(format!("{}: {error:#}", updater.unit)),
            }
        }
        latest = store.require()?;
    }

    if latest.surfaces.is_empty() && latest.updaters.is_empty() {
        remove_file_if_exists(&paths.state_file())?;
        if manager_copied {
            remove_file_if_exists(&paths.manager)?;
        }
    }

    if rollback_errors.is_empty() {
        Ok(())
    } else {
        bail!(
            "installation failed and rollback was incomplete; retained state for `codex-patcher uninstall --yes`: {}",
            rollback_errors.join("; ")
        )
    }
}

fn status(paths: &PatcherPaths, json: bool) -> Result<()> {
    let state = StateStore::new(paths.clone()).require()?;
    let surface_status: Vec<_> = state
        .surfaces
        .iter()
        .map(|record| {
            let (ownership, error) = match matches_recorded_shim(record) {
                Ok(true) => ("owned", None),
                Ok(false) => ("overwritten/drifted", None),
                Err(error) => ("inspection-error", Some(format!("{error:#}"))),
            };
            serde_json::json!({
                "path": record.surface,
                "ownership": ownership,
                "error": error,
                "record": record,
            })
        })
        .collect();
    let consumers: Vec<_> = discover(paths)?
        .into_iter()
        .filter(|candidate| {
            matches!(
                candidate.owner,
                SurfaceOwner::Desktop | SurfaceOwner::Daemon
            )
        })
        .map(|candidate| {
            let selected = state
                .surfaces
                .iter()
                .find(|record| record.surface == candidate.raw);
            let patched = selected
                .and_then(|record| matches_recorded_shim(record).ok())
                .unwrap_or(false);
            serde_json::json!({
                "candidate": candidate,
                "selected": selected.is_some(),
                "patched": patched,
            })
        })
        .collect();
    let journals = pending_ownership_journals(paths)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "state": state,
                "surface_status": surface_status,
                "desktop_and_daemon_consumers": consumers,
                "pending_ownership_journals": journals,
            }))?
        );
        return Ok(());
    }
    println!("patch directory: {}", state.patch_dir.display());
    if let Some(active) = state.active.as_ref() {
        println!(
            "active: Codex {} {} ({})",
            active.source.version,
            active.source.ref_name,
            &active.source.commit_oid[..12]
        );
        println!("package: {}", active.package_dir.display());
    } else {
        println!("active: none");
    }
    if let Some(previous) = state.previous.as_ref() {
        println!(
            "previous: Codex {} ({})",
            previous.source.version,
            &previous.source.commit_oid[..12]
        );
    }
    println!("probe: {:?}", state.probe.kind);
    if let Some(desired) = state.probe.desired.as_ref() {
        println!(
            "desired: Codex {} {} ({}) patches {}",
            desired.source.version,
            desired.source.ref_name,
            &desired.source.commit_oid[..12],
            &desired.patch_fingerprint[..8]
        );
    }
    if let Some(message) = state.probe.message.as_deref() {
        println!("probe detail: {message}");
    }
    if let Some(failure) = state.failure.as_ref() {
        println!(
            "failure: {} {}: {}",
            failure.id, failure.phase, failure.summary
        );
        println!("failure log: {}", failure.log_path.display());
    }
    for surface in &surface_status {
        println!(
            "surface: {} [{}]",
            surface["path"].as_str().unwrap_or("<non-UTF-8 path>"),
            surface["ownership"].as_str().unwrap_or("unknown")
        );
        if let Some(error) = surface["error"].as_str() {
            println!("  inspection error: {error}");
        }
    }
    for consumer in &consumers {
        let candidate: SurfaceCandidate = serde_json::from_value(consumer["candidate"].clone())?;
        let label = if consumer["patched"].as_bool() == Some(true) {
            "patched selected surface"
        } else if consumer["selected"].as_bool() == Some(true) {
            "selected but overwritten/drifted"
        } else {
            "not selected; not patched"
        };
        println!("consumer: {} [{label}]", candidate.raw.display());
    }
    for journal in journals {
        println!("pending ownership transaction: {}", journal.display());
    }
    Ok(())
}

fn pending_ownership_journals(paths: &PatcherPaths) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(&paths.backups_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut journals = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if (name.starts_with(".takeover-")
            || name.starts_with(".restore-")
            || name.starts_with(".repair-"))
            && name.ends_with(".json")
        {
            journals.push(entry.path());
        }
    }
    journals.sort();
    Ok(journals)
}

fn repair_command(paths: &PatcherPaths, failure_id: Option<&str>) -> Result<()> {
    let store = StateStore::new(paths.clone());
    let _manager_lock = store.manager_lock()?;
    store.recover_surface_transactions()?;
    let state = store.require()?;
    let failure = state
        .failure
        .as_ref()
        .context("there is no recorded failure")?;
    if let Some(failure_id) = failure_id
        && failure.id != failure_id
    {
        bail!("recorded failure is {}, not {failure_id}", failure.id);
    }
    repair::run_repair_session(paths, failure)
}

fn repair_shims(paths: &PatcherPaths, adopt_drift: bool, yes: bool) -> Result<()> {
    let store = StateStore::new(paths.clone());
    let _manager_lock = store.manager_lock()?;
    store.recover_surface_transactions()?;
    let state = store.require()?;
    for mut record in state.surfaces {
        if matches_recorded_shim(&record)? {
            println!("unchanged: {}", record.surface.display());
            continue;
        }
        let adopt = adopt_drift
            || (terminal_interactive()
                && !yes
                && Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt(format!(
                        "{} drifted; adopt it as the new restore baseline and overwrite it?",
                        record.surface.display()
                    ))
                    .default(false)
                    .interact()?);
        let outcome =
            elevation::repair_redirect(&mut record, &paths.manager, &paths.backups_dir, adopt)?;
        match outcome {
            RepairOutcome::Unchanged => println!("unchanged: {}", record.surface.display()),
            _ => println!("repaired {:?}: {}", outcome, record.surface.display()),
        }
        store.with_state_lock(|| {
            let mut latest = store.require()?;
            let entry = latest
                .surfaces
                .iter_mut()
                .find(|entry| entry.id == record.id)
                .context("surface ownership record changed during shim repair")?;
            *entry = record.clone();
            store.save(&latest)
        })?;
        finalize_repair_redirect(&record, &paths.backups_dir)?;
    }
    Ok(())
}

fn uninstall(paths: &PatcherPaths, yes: bool) -> Result<()> {
    let store = StateStore::new(paths.clone());
    let _manager_lock = store.manager_lock()?;
    store.recover_surface_transactions()?;
    let _build_lock = store.build_lock()?;
    let state = store.require()?;
    if !yes
        && (!terminal_interactive()
            || !Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt("Restore every still-owned Codex surface and uninstall?")
                .default(false)
                .interact()?)
    {
        bail!("uninstall cancelled; use --yes for a noninteractive uninstall");
    }

    // Migrate v1 development-state records that attached an updater to the
    // first surface into installation-level state before removing anything.
    store.with_state_lock(|| {
        let mut latest = store.require()?;
        let legacy: Vec<_> = latest
            .surfaces
            .iter()
            .filter_map(|record| record.updater.clone())
            .collect();
        for updater in legacy {
            if !latest.updaters.contains(&updater) {
                latest.updaters.push(updater);
            }
        }
        store.save(&latest)
    })?;

    for record in state.surfaces.into_iter().rev() {
        match elevation::uninstall_redirect(&record, &paths.backups_dir) {
            Ok(()) => {
                store.with_state_lock(|| {
                    let mut latest = store.require()?;
                    latest.surfaces.retain(|entry| entry.id != record.id);
                    store.save(&latest)
                })?;
                finalize_uninstall_redirect(&record, &paths.backups_dir)?;
                println!("restored {}", record.surface.display());
            }
            Err(error) => {
                eprintln!(
                    "left drifted surface untouched: {}: {error:#}",
                    record.surface.display()
                );
            }
        }
    }

    if !store.require()?.surfaces.is_empty() {
        bail!("uninstall is incomplete because one or more surfaces drifted");
    }

    for updater in store.require()?.updaters {
        match restore_codex_update_manager(&updater, Duration::from_secs(8)) {
            Ok(()) => {
                store.with_state_lock(|| {
                    let mut latest = store.require()?;
                    latest.updaters.retain(|entry| entry != &updater);
                    store.save(&latest)
                })?;
            }
            Err(error) => {
                eprintln!("could not restore updater {}: {error:#}", updater.unit);
            }
        }
    }
    if !store.require()?.updaters.is_empty() {
        bail!("uninstall is incomplete because an updater could not be restored");
    }

    remove_file_if_exists(&paths.state_file())?;
    remove_file_if_exists(&paths.remote_cache_file())?;
    remove_uninstallable_generations(paths)?;
    if let Err(error) = remove_dir_if_exists(&paths.cache_dir()) {
        eprintln!("retained cache files that could not be removed: {error:#}");
    }
    if let Err(error) = remove_dir_if_exists(&paths.backups_dir) {
        eprintln!("retained backup storage that could not be removed: {error:#}");
    }
    #[cfg(unix)]
    remove_file_if_exists(&paths.manager)?;
    #[cfg(windows)]
    schedule_running_manager_removal(&paths.manager)?;
    println!("codex-patcher uninstalled");
    Ok(())
}

fn gc(paths: &PatcherPaths) -> Result<()> {
    let store = StateStore::new(paths.clone());
    let _manager_lock = store.manager_lock()?;
    store.recover_surface_transactions()?;
    let _build_lock = store.build_lock()?;
    let state = store.require()?;
    let mut retained = HashSet::new();
    if let Some(active) = state.active.as_ref() {
        retained.insert(active.id.clone());
    }
    if let Some(previous) = state.previous.as_ref() {
        retained.insert(previous.id.clone());
    }
    let repair_in_progress = fs::read_dir(paths.worktrees_dir())?
        .filter_map(Result::ok)
        .any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(".repair-") && name.ends_with(".json")
        });
    let root = paths.generations_dir();
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(".staging-") {
            match fs::remove_dir_all(entry.path()) {
                Ok(()) => println!("removed abandoned staging directory {name}"),
                Err(error) => eprintln!("could not remove staging directory {name}: {error}"),
            }
            continue;
        }
        if retained.contains(&name) {
            continue;
        }
        if repair_in_progress {
            println!("retained generation {name} for an unfinished repair session");
            continue;
        }
        if generation_in_use(paths, &entry.path()) {
            println!("retained running generation {name}");
            continue;
        }
        match fs::remove_dir_all(entry.path()) {
            Ok(()) => println!("removed generation {name}"),
            Err(error) => eprintln!("could not remove generation {name}: {error}"),
        }
    }
    Ok(())
}

fn generation_in_use(paths: &PatcherPaths, generation: &Path) -> bool {
    let Some(id) = generation.file_name() else {
        return true;
    };
    let leases = paths.state_dir.join("leases");
    if fs::create_dir_all(&leases).is_err() {
        return true;
    }
    let lease = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(leases.join(Path::new(id).with_extension("lock")))
    {
        Ok(lease) => lease,
        Err(_) => return true,
    };
    if lease.try_lock_exclusive().is_err() {
        return true;
    }
    let _ = FileExt::unlock(&lease);

    #[cfg(target_os = "linux")]
    return linux_generation_in_use(generation);
    #[cfg(not(target_os = "linux"))]
    false
}

#[cfg(target_os = "linux")]
fn linux_generation_in_use(generation: &Path) -> bool {
    let Ok(processes) = fs::read_dir("/proc") else {
        return true;
    };
    processes.filter_map(Result::ok).any(|process| {
        if !process
            .file_name()
            .to_string_lossy()
            .chars()
            .all(|character| character.is_ascii_digit())
        {
            return false;
        }
        if ["exe", "cwd"].iter().any(|leaf| {
            fs::read_link(process.path().join(leaf)).is_ok_and(|path| path.starts_with(generation))
        }) {
            return true;
        }
        if fs::read_dir(process.path().join("fd")).is_ok_and(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                fs::read_link(entry.path()).is_ok_and(|path| path.starts_with(generation))
            })
        }) {
            return true;
        }
        fs::read_to_string(process.path().join("maps")).is_ok_and(|maps| {
            maps.lines()
                .filter_map(|line| line.split_whitespace().last())
                .any(|path| Path::new(path).starts_with(generation))
        })
    })
}

fn remove_uninstallable_generations(paths: &PatcherPaths) -> Result<()> {
    let root = paths.generations_dir();
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(".staging-") && generation_in_use(paths, &entry.path()) {
            eprintln!("retained running generation {name}");
            continue;
        }
        match fs::remove_dir_all(entry.path()) {
            Ok(()) => {}
            Err(error) => eprintln!("retained generation {name}: {error}"),
        }
    }
    if fs::read_dir(&root)?.next().is_none() {
        fs::remove_dir(&root)?;
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(windows)]
fn schedule_running_manager_removal(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW};

    if remove_file_if_exists(path).is_ok() || !path.exists() {
        return Ok(());
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is a live, nul-terminated UTF-16 path and the null second
    // path requests deletion at the next reboot.
    let scheduled =
        unsafe { MoveFileExW(wide.as_ptr(), std::ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
    if scheduled == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("scheduling removal of {}", path.display()));
    }
    eprintln!(
        "scheduled the running management binary for removal at reboot: {}",
        path.display()
    );
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn terminal_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_surface_paths_are_lexically_normalized_and_deduplicated() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("codex"), b"owner launcher").unwrap();
        let aliases = vec![
            bin.join("codex"),
            bin.join(".").join("codex"),
            bin.join("nested").join("..").join("codex"),
        ];
        fs::create_dir(bin.join("nested")).unwrap();

        let selected = deduplicate_surface_paths(aliases).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(
            selected[0].parent(),
            Some(bin.canonicalize().unwrap().as_path())
        );
        assert_eq!(selected[0].file_name(), Some(std::ffi::OsStr::new("codex")));
    }

    #[cfg(unix)]
    #[test]
    fn normalization_preserves_the_final_launcher_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let package = temp.path().join("package");
        let bin = temp.path().join("bin");
        fs::create_dir(&package).unwrap();
        fs::create_dir(&bin).unwrap();
        fs::write(package.join("codex"), b"package executable").unwrap();
        symlink(package.join("codex"), bin.join("codex")).unwrap();

        assert_eq!(
            normalize_surface_path(&bin.join("codex")).unwrap(),
            bin.join("codex")
        );
    }

    #[test]
    fn generation_lease_prevents_collection_on_every_platform() {
        let temp = tempfile::tempdir().unwrap();
        let paths = PatcherPaths::from_home(temp.path().join("patcher"));
        paths.ensure().unwrap();
        let id = "a".repeat(64);
        let generation = paths.generations_dir().join(&id);
        fs::create_dir(&generation).unwrap();
        let leases = paths.state_dir.join("leases");
        fs::create_dir_all(&leases).unwrap();
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(leases.join(format!("{id}.lock")))
            .unwrap();
        lease.lock_shared().unwrap();
        assert!(generation_in_use(&paths, &generation));
        FileExt::unlock(&lease).unwrap();
        assert!(!generation_in_use(&paths, &generation));
    }

    #[test]
    fn quickstart_writes_a_valid_patch_directory_without_overwriting() {
        let temp = tempfile::tempdir().unwrap();
        let patch_dir = temp.path().join("codex-home/codex-patcher");

        write_quickstart(&patch_dir, false).unwrap();
        Config::load(patch_dir.join("codex-patcher.toml")).unwrap();
        let set = PatchSet::load(&patch_dir).unwrap();
        assert_eq!(set.patches.len(), 1);
        assert_eq!(set.patches[0].path, "status-line.patch");
        assert!(
            fs::read_to_string(patch_dir.join("AGENTS.md"))
                .unwrap()
                .contains("https://github.com/arm64be/codex-patcher")
        );

        assert!(write_quickstart(&patch_dir, false).is_err());
        write_quickstart(&patch_dir, true).unwrap();
    }
}
