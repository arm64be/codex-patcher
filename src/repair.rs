//! Resumable patch maintenance and repair orchestration.

use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::Utc;
use dialoguer::{Confirm, theme::ColorfulTheme};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use unicode_normalization::UnicodeNormalization;
use walkdir::{DirEntry, WalkDir};

use crate::build::{
    BuildEvent, BuildOptions, build_generation, load_validated_generation, prepare_repair_worktree,
    remove_repair_worktree,
};
use crate::config::Config;
use crate::patchset::PatchSet;
use crate::paths::PatcherPaths;
use crate::state::{StateStore, atomic_write};
use crate::types::{DesiredBuild, FailureRecord, GenerationRef, ProbeKind, ProbeState};

pub const MAINTENANCE_ENV: &str = "CODEX_PATCHER_MAINTENANCE";
const JOURNAL_SCHEMA: u32 = 1;
const JOURNAL_NAME: &str = ".codex-patcher-repair-journal.json";
const REPAIR_SESSION_SCHEMA: u32 = 1;
const BUILD_REPAIR_PATCH_BASENAME: &str = "codex-patcher-build-repair.patch";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairPromptContext {
    pub failure_id: String,
    pub desired_version: String,
    pub upstream_ref: String,
    pub failed_patch: Option<String>,
    pub phase: String,
    pub summary: String,
    pub log_path: PathBuf,
    pub patch_dir: PathBuf,
}

pub fn generate_repair_prompt(context: &RepairPromptContext) -> String {
    let failed_patch = context.failed_patch.as_deref().unwrap_or("not identified");
    format!(
        "Repair the Codex patch stack in this maintenance worktree.\n\n\
         Failure ID: {}\n\
         Desired Codex: {} ({})\n\
         Failed phase: {}\n\
         Failed patch: {}\n\
         Diagnostic: {}\n\
         Full log: {}\n\
         Live patch directory (do not edit directly): {}\n\n\
         Work only in this worktree. Resolve rejected hunks and build errors while preserving \
         the intent and order of the existing patch stack. Remove every .rej file and leave the \
         worktree buildable. Start with focused offline checks using the existing toolchain and \
         cache; do not run broad workspace test suites. The outer repair \
         transaction performs the authoritative package rebuild. Do not commit, amend, rebase, \
         or reset Git history. Do not invoke codex-patcher or edit the live patch directory; the \
         outer repair transaction will re-export, validate, show the changes, and request \
         confirmation before replacing anything.",
        single_line(&context.failure_id),
        single_line(&context.desired_version),
        single_line(&context.upstream_ref),
        single_line(&context.phase),
        single_line(failed_patch),
        single_line(&context.summary),
        context.log_path.display(),
        context.patch_dir.display(),
    )
}

pub fn maintenance_command(
    codex_binary: &Path,
    worktree: &Path,
    prompt: &str,
    yolo_mode: bool,
) -> Command {
    let mut command = Command::new(codex_binary);
    command
        .arg("-c")
        .arg("check_for_update_on_startup=false")
        .arg("-C")
        .arg(worktree);
    if yolo_mode {
        command.arg("--dangerously-bypass-approvals-and-sandbox");
    } else {
        command
            .arg("--sandbox")
            .arg("workspace-write")
            .arg("--ask-for-approval")
            .arg("on-request");
    }
    command
        .arg("--no-alt-screen")
        .arg(prompt)
        .env(MAINTENANCE_ENV, "1")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    command
}

pub fn launch_last_good_codex(
    codex_binary: &Path,
    worktree: &Path,
    prompt: &str,
    yolo_mode: bool,
) -> Result<ExitStatus> {
    ensure!(
        codex_binary.is_file(),
        "last-good Codex does not exist: {}",
        codex_binary.display()
    );
    ensure!(
        worktree.is_dir(),
        "repair worktree does not exist: {}",
        worktree.display()
    );
    maintenance_command(codex_binary, worktree, prompt, yolo_mode)
        .status()
        .with_context(|| {
            format!(
                "launching pinned last-good Codex at {}",
                codex_binary.display()
            )
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepairConfirmation {
    #[default]
    Prompt,
    Accept,
    Decline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunRepairOptions {
    pub confirmation: RepairConfirmation,
    pub yolo_mode: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RepairSession {
    schema: u32,
    failure_id: String,
    desired: DesiredBuild,
    config_sha256: String,
    worktree: PathBuf,
    snapshot_dir: PathBuf,
    candidate_dir: PathBuf,
    last_good: GenerationRef,
    commits: Vec<PatchCommit>,
    stage: RepairStage,
    final_patch_name: Option<PathBuf>,
    diagnostic_phase: String,
    diagnostic_summary: String,
    diagnostic_log: PathBuf,
    #[serde(default)]
    validated_generation: Option<GenerationRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum RepairStage {
    Applying {
        next_index: usize,
        conflicted_index: Option<usize>,
    },
    BuildRepair,
    ReadyToBuild,
}

pub fn run_repair_session(paths: &PatcherPaths, failure: &FailureRecord) -> Result<()> {
    run_repair_session_with_options(paths, failure, RunRepairOptions::default()).map(drop)
}

pub fn run_repair_session_with_options(
    paths: &PatcherPaths,
    failure: &FailureRecord,
    options: RunRepairOptions,
) -> Result<GenerationRef> {
    paths.ensure()?;
    let store = StateStore::new(paths.clone());
    let current = store.require()?;
    let recorded = current
        .failure
        .as_ref()
        .context("there is no recorded failure to repair")?;
    ensure!(
        recorded.id == failure.id && recorded.desired == failure.desired,
        "recorded failure changed while repair was starting"
    );
    let last_good = current
        .active
        .clone()
        .context("patch repair needs a validated last-good generation")?;
    ensure!(
        last_good.binary.is_file(),
        "last-good Codex does not exist: {}",
        last_good.binary.display()
    );
    recover_replacement_journal(&current.patch_dir)?;
    let live_patches = PatchSet::load(&current.patch_dir)?;
    let config_path = current.patch_dir.join("codex-patcher.toml");
    let config_bytes =
        fs::read(&config_path).with_context(|| format!("reading {}", config_path.display()))?;
    validate_failure_config(&config_path, failure)?;
    if live_patches.fingerprint != failure.desired.patch_fingerprint {
        if let Some(session) = load_session(paths, &failure.id)? {
            validate_session_identity(paths, &session, failure, &config_bytes)?;
            ensure!(
                session.last_good == last_good,
                "pinned last-good generation changed during repair"
            );
            if let Some(generation) = session.validated_generation.as_ref() {
                let desired = desired_from_generation(generation);
                if live_patches.fingerprint == desired.patch_fingerprint {
                    let generation =
                        revalidate_persisted_generation(paths, &session, generation, &desired)?;
                    activate_repaired_generation(
                        &store,
                        &session,
                        &generation,
                        &desired,
                        &desired.patch_fingerprint,
                    )?;
                    cleanup_repair_session(paths, &session);
                    return Ok(generation);
                }
            }
        }
        bail!(
            "live patch inputs changed after failure {}; launch Codex again or retry update",
            failure.id
        );
    }
    let mut session = load_or_prepare_session(
        paths,
        &store,
        failure,
        &live_patches,
        &config_bytes,
        last_good.clone(),
    )?;
    validate_session_identity(paths, &session, failure, &config_bytes)?;
    ensure!(
        live_patches.fingerprint == session.desired.patch_fingerprint,
        "live patch stack changed during repair; discard the old repair and retry the new desired state"
    );
    ensure!(
        session.last_good == last_good,
        "pinned last-good generation changed during repair"
    );
    loop {
        let stage = session.stage.clone();
        match &stage {
            RepairStage::Applying {
                conflicted_index: Some(index),
                ..
            } => launch_maintenance(
                &session,
                failure,
                Some(*index),
                &current.patch_dir,
                options.yolo_mode,
            )?,
            RepairStage::BuildRepair => launch_maintenance(
                &session,
                failure,
                None,
                &current.patch_dir,
                options.yolo_mode,
            )?,
            RepairStage::ReadyToBuild => break,
            _ => {}
        }
        let _build_lock = store.build_lock()?;
        ensure_session_still_current(&store, &session)?;
        match stage {
            RepairStage::Applying {
                conflicted_index: Some(index),
                ..
            } => {
                finish_conflicted_patch(&mut session, index)?;
                continue_applying(&mut session)?;
            }
            RepairStage::BuildRepair => finish_build_repair(&mut session)?,
            RepairStage::Applying { .. } => continue_applying(&mut session)?,
            RepairStage::ReadyToBuild => unreachable!(),
        }
        save_session(paths, &session)?;
    }
    let snapshot = PatchSet::load(&session.snapshot_dir)?;
    let exported = export_normalized_patches(&session.worktree, &session.commits)?;
    prepare_candidate_dir(&session, &exported, &config_bytes)?;
    let candidate = PatchSet::load(&session.candidate_dir)?;
    let desired = DesiredBuild {
        source: session.desired.source.clone(),
        patch_fingerprint: candidate.fingerprint.clone(),
        target: session.desired.target.clone(),
        source_key: candidate.source_key(&session.desired.source, &session.desired.target),
    };
    let generation = {
        let _build_lock = store.build_lock()?;
        ensure_session_still_current(&store, &session)?;
        let mut progress = |event: BuildEvent| match event {
            BuildEvent::Phase(phase) => eprintln!("codex-patcher repair: {phase}"),
            BuildEvent::Line(_) => {}
        };
        match build_generation(
            paths,
            &candidate,
            &desired,
            None,
            &BuildOptions {
                allow_force_push: false,
                retry: true,
            },
            &mut progress,
        ) {
            Ok(generation) => generation,
            Err(build_failure) => {
                session.stage = RepairStage::BuildRepair;
                session.validated_generation = None;
                session.diagnostic_phase = build_failure.phase.clone();
                session.diagnostic_summary = build_failure.summary.clone();
                session.diagnostic_log = build_failure.log_path.clone();
                save_session(paths, &session)?;
                return Err(anyhow!(build_failure).context(format!(
                    "repaired candidate still fails validation; rerun `codex-patcher repair {}` to continue in {}",
                    session.failure_id,
                    session.worktree.display()
                )));
            }
        }
    };
    session.validated_generation = Some(generation.clone());
    save_session(paths, &session)?;
    let replacements = complete_patch_stack_replacements(&snapshot, &exported);
    let applied = journaled_replace(
        &current.patch_dir,
        &replacements,
        |previews| confirm_replacements(&current.patch_dir, &replacements, previews, options),
        || ensure_session_still_current(&store, &session),
    )?;
    ensure!(
        applied,
        "repair patch replacement was declined; validated generation remains staged"
    );
    let generation = revalidate_persisted_generation(paths, &session, &generation, &desired)?;
    activate_repaired_generation(
        &store,
        &session,
        &generation,
        &desired,
        &candidate.fingerprint,
    )?;
    cleanup_repair_session(paths, &session);
    Ok(generation)
}

fn validate_failure_config(path: &Path, failure: &FailureRecord) -> Result<()> {
    let config = Config::load(path)?;
    ensure!(
        config.branch.as_str() == failure.desired.source.channel,
        "configured channel changed from {} to {} after the recorded failure",
        failure.desired.source.channel,
        config.branch
    );
    let target = config.resolved_target()?;
    ensure!(
        target == failure.desired.target,
        "configured build target changed from {} to {target} after the recorded failure",
        failure.desired.target
    );
    Ok(())
}

fn validate_session_identity(
    paths: &PatcherPaths,
    session: &RepairSession,
    failure: &FailureRecord,
    config_bytes: &[u8],
) -> Result<()> {
    ensure!(
        session.schema == REPAIR_SESSION_SCHEMA,
        "unsupported repair session schema {}",
        session.schema
    );
    ensure!(
        session.failure_id == failure.id && session.desired == failure.desired,
        "repair session belongs to a different failure"
    );
    ensure!(
        session.worktree == paths.worktrees_dir().join(format!("repair-{}", failure.id))
            && session.snapshot_dir
                == paths
                    .worktrees_dir()
                    .join(format!(".repair-{}-inputs", failure.id))
            && session.candidate_dir
                == paths
                    .worktrees_dir()
                    .join(format!(".repair-{}-candidate", failure.id)),
        "repair session contains paths outside the patcher-owned worktree area"
    );
    ensure!(
        session.config_sha256 == sha256(config_bytes),
        "codex-patcher.toml changed during repair; discard the old repair and retry the new desired state"
    );
    let snapshot = PatchSet::load(&session.snapshot_dir)?;
    ensure!(
        snapshot.fingerprint == session.desired.patch_fingerprint,
        "persisted repair patch snapshot is corrupt"
    );
    let worktree_metadata = fs::symlink_metadata(&session.worktree).with_context(|| {
        format!(
            "persisted repair worktree is missing: {}",
            session.worktree.display()
        )
    })?;
    ensure!(
        worktree_metadata.is_dir() && !worktree_metadata.file_type().is_symlink(),
        "persisted repair worktree is not a regular directory: {}",
        session.worktree.display()
    );
    ensure!(
        session.last_good.binary.is_file(),
        "pinned last-good Codex is missing: {}",
        session.last_good.binary.display()
    );
    if let Some(generation) = session.validated_generation.as_ref() {
        let desired = desired_from_generation(generation);
        ensure!(
            desired.source == session.desired.source && desired.target == session.desired.target,
            "persisted validated repair result is inconsistent"
        );
    }
    Ok(())
}

fn load_session(paths: &PatcherPaths, failure_id: &str) -> Result<Option<RepairSession>> {
    validate_failure_id(failure_id)?;
    let path = repair_session_path(paths, failure_id);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {}", path.display()))
        .map(Some)
}

fn load_or_prepare_session(
    paths: &PatcherPaths,
    store: &StateStore,
    failure: &FailureRecord,
    patches: &PatchSet,
    config_bytes: &[u8],
    last_good: GenerationRef,
) -> Result<RepairSession> {
    validate_failure_id(&failure.id)?;
    if let Some(session) = load_session(paths, &failure.id)? {
        record_repair_worktree(store, failure, &session.worktree)?;
        return Ok(session);
    }

    let worktree = paths.worktrees_dir().join(format!("repair-{}", failure.id));
    let snapshot_dir = paths
        .worktrees_dir()
        .join(format!(".repair-{}-inputs", failure.id));
    let candidate_dir = paths
        .worktrees_dir()
        .join(format!(".repair-{}-candidate", failure.id));
    reset_owned_directory(&snapshot_dir)?;
    write_patch_snapshot(&snapshot_dir, patches, config_bytes)?;
    if candidate_dir.exists() {
        fs::remove_dir_all(&candidate_dir)
            .with_context(|| format!("removing {}", candidate_dir.display()))?;
    }

    let log_path = paths.logs_dir().join(format!("repair-{}.log", failure.id));
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let _build_lock = store.build_lock()?;
    prepare_repair_worktree(paths, &failure.desired, &worktree, &mut log)?;
    configure_synthetic_repository(&worktree)?;
    let mut session = RepairSession {
        schema: REPAIR_SESSION_SCHEMA,
        failure_id: failure.id.clone(),
        desired: failure.desired.clone(),
        config_sha256: sha256(config_bytes),
        worktree,
        snapshot_dir,
        candidate_dir,
        last_good,
        commits: Vec::with_capacity(patches.patches.len() + 1),
        stage: RepairStage::Applying {
            next_index: 0,
            conflicted_index: None,
        },
        final_patch_name: None,
        diagnostic_phase: failure.phase.clone(),
        diagnostic_summary: failure.summary.clone(),
        diagnostic_log: failure.log_path.clone(),
        validated_generation: None,
    };
    continue_applying(&mut session)?;
    save_session(paths, &session)?;
    drop(_build_lock);

    record_repair_worktree(store, failure, &session.worktree)?;
    Ok(session)
}

fn record_repair_worktree(
    store: &StateStore,
    failure: &FailureRecord,
    worktree: &Path,
) -> Result<()> {
    store.with_state_lock(|| {
        let mut state = store.require()?;
        let recorded = state
            .failure
            .as_mut()
            .context("recorded failure disappeared while preparing repair")?;
        ensure!(
            recorded.id == failure.id,
            "recorded failure changed while preparing repair"
        );
        recorded.repair_worktree = Some(worktree.to_path_buf());
        store.save(&state)
    })
}

fn validate_failure_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "unsafe failure id {id:?}"
    );
    Ok(())
}

fn repair_session_path(paths: &PatcherPaths, id: &str) -> PathBuf {
    paths.worktrees_dir().join(format!(".repair-{id}.json"))
}

fn save_session(paths: &PatcherPaths, session: &RepairSession) -> Result<()> {
    atomic_write(
        &repair_session_path(paths, &session.failure_id),
        &serde_json::to_vec_pretty(session)?,
    )
}

fn reset_owned_directory(path: &Path) -> Result<()> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "patcher-owned repair path is not a directory: {}",
            path.display()
        );
        fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))?;
    }
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))
}

fn write_patch_snapshot(root: &Path, patches: &PatchSet, config_bytes: &[u8]) -> Result<()> {
    atomic_write(&root.join("codex-patcher.toml"), config_bytes)?;
    for patch in &patches.patches {
        let relative = Path::new(&patch.path);
        validate_relative_path(relative)?;
        atomic_write(&root.join(relative), &patch.bytes)?;
    }
    let series = patches
        .patches
        .iter()
        .map(|patch| patch.path.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    atomic_write(&root.join("series"), series.as_bytes())?;
    let snapshot = PatchSet::load(root)?;
    ensure!(
        snapshot.fingerprint == patches.fingerprint,
        "repair snapshot fingerprint changed while writing"
    );
    Ok(())
}

fn configure_synthetic_repository(worktree: &Path) -> Result<()> {
    run_git_checked(worktree, ["config", "user.name", "Codex Patcher Repair"])?;
    run_git_checked(
        worktree,
        ["config", "user.email", "codex-patcher@localhost.invalid"],
    )?;
    run_git_checked(worktree, ["config", "commit.gpgsign", "false"])?;
    Ok(())
}

fn expected_head(session: &RepairSession) -> &str {
    session
        .commits
        .last()
        .map(|commit| commit.commit.as_str())
        .unwrap_or(session.desired.source.commit_oid.as_str())
}

fn ensure_expected_head(session: &RepairSession) -> Result<()> {
    let head = git_output(&session.worktree, ["rev-parse", "HEAD"])?;
    ensure!(
        head.trim() == expected_head(session),
        "repair worktree HEAD drifted: expected {}, found {}",
        expected_head(session),
        head.trim()
    );
    Ok(())
}

fn continue_applying(session: &mut RepairSession) -> Result<()> {
    session.validated_generation = None;
    let patches = PatchSet::load(&session.snapshot_dir)?;
    let next_index = match session.stage {
        RepairStage::Applying {
            next_index,
            conflicted_index: None,
        } => next_index,
        RepairStage::Applying {
            conflicted_index: Some(_),
            ..
        } => return Ok(()),
        _ => return Ok(()),
    };
    ensure!(
        session.commits.len() == next_index,
        "repair session commit/index mismatch"
    );
    ensure_expected_head(session)?;
    ensure_worktree_clean(&session.worktree)?;

    for index in next_index..patches.patches.len() {
        let patch = &patches.patches[index];
        let patch_path = session.snapshot_dir.join(&patch.path);
        let check = git_status(&session.worktree, ["apply", "--check"], Some(&patch_path))?;
        if check.success() {
            let apply = git_status(&session.worktree, ["apply", "--index"], Some(&patch_path))?;
            ensure!(
                apply.success(),
                "git apply --index failed after a successful check for {}",
                patch.path
            );
            let commit = commit_staged_patch(
                &session.worktree,
                Path::new(&patch.path),
                &format!(
                    "codex-patcher synthetic patch {}/{}: {}",
                    index + 1,
                    patches.patches.len(),
                    patch.path
                ),
            )?;
            session.commits.push(commit);
            session.stage = RepairStage::Applying {
                next_index: index + 1,
                conflicted_index: None,
            };
            continue;
        }

        let rejects_before = find_reject_files(&session.worktree)?;
        ensure!(
            rejects_before.is_empty(),
            "repair worktree already contains rejects"
        );
        let _ = git_status(&session.worktree, ["apply", "--reject"], Some(&patch_path))?;
        if find_reject_files(&session.worktree)?.is_empty() {
            let reject_dir = session.worktree.join(".codex-patcher-rejects");
            fs::create_dir_all(&reject_dir)?;
            atomic_write(
                &reject_dir.join(format!(
                    "{:04}-{}.rej",
                    index + 1,
                    &sha256(&patch.bytes)[..12]
                )),
                &patch.bytes,
            )?;
        }
        session.stage = RepairStage::Applying {
            next_index: index,
            conflicted_index: Some(index),
        };
        return Ok(());
    }

    session.stage = if session.diagnostic_phase == "patch" {
        RepairStage::ReadyToBuild
    } else {
        RepairStage::BuildRepair
    };
    Ok(())
}

fn launch_maintenance(
    session: &RepairSession,
    original_failure: &FailureRecord,
    conflict_index: Option<usize>,
    patch_dir: &Path,
    yolo_mode: bool,
) -> Result<()> {
    let snapshot = PatchSet::load(&session.snapshot_dir)?;
    let failed_patch = conflict_index
        .and_then(|index| snapshot.patches.get(index))
        .map(|patch| patch.path.clone())
        .or_else(|| original_failure.failed_patch.clone());
    let prompt = generate_repair_prompt(&RepairPromptContext {
        failure_id: session.failure_id.clone(),
        desired_version: session.desired.source.version.clone(),
        upstream_ref: session.desired.source.ref_name.clone(),
        failed_patch,
        phase: session.diagnostic_phase.clone(),
        summary: session.diagnostic_summary.clone(),
        log_path: session.diagnostic_log.clone(),
        patch_dir: patch_dir.to_path_buf(),
    });
    let _ = launch_last_good_codex(
        &session.last_good.binary,
        &session.worktree,
        &prompt,
        yolo_mode,
    )?;
    Ok(())
}

fn finish_conflicted_patch(session: &mut RepairSession, index: usize) -> Result<()> {
    session.validated_generation = None;
    ensure_expected_head(session)?;
    ensure_no_rejects_or_unmerged(&session.worktree)?;
    let patches = PatchSet::load(&session.snapshot_dir)?;
    let patch = patches
        .patches
        .get(index)
        .context("repair conflict index is outside the patch stack")?;
    run_git_checked(&session.worktree, ["add", "-A"])?;
    let commit = commit_staged_patch(
        &session.worktree,
        Path::new(&patch.path),
        &format!(
            "codex-patcher repaired synthetic patch {}/{}: {}",
            index + 1,
            patches.patches.len(),
            patch.path
        ),
    )?;
    session.commits.push(commit);
    session.stage = RepairStage::Applying {
        next_index: index + 1,
        conflicted_index: None,
    };
    Ok(())
}

fn finish_build_repair(session: &mut RepairSession) -> Result<()> {
    session.validated_generation = None;
    ensure_expected_head(session)?;
    ensure_no_rejects_or_unmerged(&session.worktree)?;
    ensure!(
        !worktree_status(&session.worktree)?.is_empty(),
        "maintenance Codex left no tracked or untracked repair changes in {}; the repair session is preserved",
        session.worktree.display()
    );
    run_git_checked(&session.worktree, ["add", "-A"])?;

    if let Some(name) = session.final_patch_name.clone() {
        ensure!(
            session
                .commits
                .last()
                .is_some_and(|commit| commit.name == name),
            "repair session final-patch metadata is inconsistent"
        );
        ensure_staged_changes(&session.worktree)?;
        run_synthetic_commit(
            &session.worktree,
            &[
                "commit",
                "--amend",
                "--no-edit",
                "--no-gpg-sign",
                "--no-verify",
            ],
        )?;
        let commit = git_output(&session.worktree, ["rev-parse", "HEAD"])?;
        session.commits.last_mut().expect("checked above").commit = commit.trim().to_owned();
    } else {
        let name = choose_build_repair_name(session)?;
        let commit = commit_staged_patch(
            &session.worktree,
            &name,
            "codex-patcher synthetic build repair",
        )?;
        session.final_patch_name = Some(name);
        session.commits.push(commit);
    }
    session.stage = RepairStage::ReadyToBuild;
    Ok(())
}

fn choose_build_repair_name(session: &RepairSession) -> Result<PathBuf> {
    let snapshot = PatchSet::load(&session.snapshot_dir)?;
    let names: HashSet<_> = snapshot
        .patches
        .iter()
        .map(|patch| casefold_path(Path::new(&patch.path)))
        .collect();
    for suffix in 0..10_000usize {
        let candidate = if suffix == 0 {
            BUILD_REPAIR_PATCH_BASENAME.to_owned()
        } else {
            format!("codex-patcher-build-repair-{suffix}.patch")
        };
        if !names.contains(&casefold_path(Path::new(&candidate))) {
            return Ok(PathBuf::from(candidate));
        }
    }
    bail!("could not choose a unique build-repair patch filename")
}

fn commit_staged_patch(worktree: &Path, name: &Path, message: &str) -> Result<PatchCommit> {
    validate_relative_path(name)?;
    ensure_staged_changes(worktree)?;
    let parent = git_output(worktree, ["rev-parse", "HEAD"])?;
    run_synthetic_commit(
        worktree,
        &["commit", "--no-gpg-sign", "--no-verify", "-m", message],
    )?;
    let commit = git_output(worktree, ["rev-parse", "HEAD"])?;
    Ok(PatchCommit {
        parent: parent.trim().to_owned(),
        commit: commit.trim().to_owned(),
        name: name.to_path_buf(),
    })
}

fn ensure_staged_changes(worktree: &Path) -> Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["diff", "--cached", "--quiet", "--exit-code"])
        .status()?;
    match status.code() {
        Some(1) => Ok(()),
        Some(0) => {
            bail!("repaired patch produced no changes; preserve its intent before continuing")
        }
        _ => bail!("git diff --cached failed with {status}"),
    }
}

fn run_synthetic_commit(worktree: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Codex Patcher Repair")
        .env("GIT_AUTHOR_EMAIL", "codex-patcher@localhost.invalid")
        .env("GIT_COMMITTER_NAME", "Codex Patcher Repair")
        .env("GIT_COMMITTER_EMAIL", "codex-patcher@localhost.invalid")
        .output()?;
    ensure!(
        output.status.success(),
        "synthetic Git commit failed: {}",
        single_line(&String::from_utf8_lossy(&output.stderr))
    );
    Ok(())
}

fn ensure_no_rejects_or_unmerged(worktree: &Path) -> Result<()> {
    let rejects = find_reject_files(worktree)?;
    ensure!(
        rejects.is_empty(),
        "repair remains incomplete; remove or resolve reject files: {}",
        rejects
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let unmerged = git_output(worktree, ["diff", "--name-only", "--diff-filter=U"])?;
    ensure!(
        unmerged.trim().is_empty(),
        "repair remains incomplete; unresolved paths: {}",
        single_line(unmerged.trim())
    );
    Ok(())
}

fn ensure_worktree_clean(worktree: &Path) -> Result<()> {
    let status = worktree_status(worktree)?;
    ensure!(
        status.is_empty(),
        "repair worktree has unexpected changes before applying the next patch: {}",
        single_line(&status)
    );
    Ok(())
}

fn worktree_status(worktree: &Path) -> Result<String> {
    git_output(
        worktree,
        ["status", "--porcelain=v1", "--untracked-files=all"],
    )
    .map(|status| status.trim().to_owned())
}

fn git_status<const N: usize>(
    worktree: &Path,
    args: [&str; N],
    final_path: Option<&Path>,
) -> Result<ExitStatus> {
    let mut command = Command::new("git");
    command.arg("-C").arg(worktree).args(args);
    if let Some(path) = final_path {
        command.arg(path);
    }
    command
        .output()
        .map(|output| output.status)
        .context("running Git maintenance command")
}

fn run_git_checked<const N: usize>(worktree: &Path, args: [&str; N]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "git command failed: {}",
        single_line(&String::from_utf8_lossy(&output.stderr))
    );
    Ok(())
}

fn git_output<const N: usize>(worktree: &Path, args: [&str; N]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(args)
        .output()?;
    ensure!(
        output.status.success(),
        "git command failed: {}",
        single_line(&String::from_utf8_lossy(&output.stderr))
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn prepare_candidate_dir(
    session: &RepairSession,
    exported: &[ExportedPatch],
    config_bytes: &[u8],
) -> Result<()> {
    reset_owned_directory(&session.candidate_dir)?;
    atomic_write(
        &session.candidate_dir.join("codex-patcher.toml"),
        config_bytes,
    )?;
    for replacement in patch_stack_replacements(exported) {
        let contents = replacement
            .contents
            .context("candidate patch stack unexpectedly contains a deletion")?;
        atomic_write(
            &session.candidate_dir.join(&replacement.relative_path),
            &contents,
        )?;
    }
    Ok(())
}

fn complete_patch_stack_replacements(
    original: &PatchSet,
    exported: &[ExportedPatch],
) -> Vec<FileReplacement> {
    let exported_names: HashSet<_> = exported.iter().map(|patch| patch.name.clone()).collect();
    let mut replacements = patch_stack_replacements(exported);
    replacements.extend(
        original
            .patches
            .iter()
            .map(|patch| PathBuf::from(&patch.path))
            .filter(|path| !exported_names.contains(path))
            .map(FileReplacement::delete),
    );
    replacements
}

fn activate_repaired_generation(
    store: &StateStore,
    session: &RepairSession,
    generation: &GenerationRef,
    desired: &DesiredBuild,
    installed_fingerprint: &str,
) -> Result<()> {
    store.with_state_lock(|| {
        let mut latest = store.require()?;
        let latest_failure = latest
            .failure
            .as_ref()
            .context("recorded failure disappeared before repair activation")?;
        ensure!(
            latest_failure.id == session.failure_id && latest_failure.desired == session.desired,
            "recorded failure changed before repair activation"
        );
        ensure!(
            latest.active.as_ref().map(|active| active.id.as_str())
                == Some(session.last_good.id.as_str()),
            "active generation changed before repair activation"
        );
        let live = PatchSet::load(&latest.patch_dir)?;
        ensure!(
            live.fingerprint == installed_fingerprint
                && generation.patch_fingerprint == installed_fingerprint
                && desired.patch_fingerprint == installed_fingerprint,
            "installed repaired patch stack does not match validated candidate"
        );
        latest.activate(generation.clone());
        latest.probe = ProbeState {
            kind: ProbeKind::Current,
            checked_at: Some(Utc::now()),
            next_check_at: None,
            desired: Some(desired.clone()),
            message: None,
        };
        store.save(&latest)
    })
}

fn revalidate_persisted_generation(
    paths: &PatcherPaths,
    session: &RepairSession,
    generation: &GenerationRef,
    desired: &DesiredBuild,
) -> Result<GenerationRef> {
    let reconstructed = desired_from_generation(generation);
    ensure!(
        &reconstructed == desired,
        "persisted repair generation does not match its desired build"
    );
    let root = generation
        .package_dir
        .parent()
        .context("persisted repair package has no generation directory")?;
    let mut log = OpenOptions::new().create(true).append(true).open(
        paths
            .logs_dir()
            .join(format!("repair-{}.log", session.failure_id)),
    )?;
    let manifest = load_validated_generation(&root.join("generation.json"), desired, &mut log)
        .context("revalidating repaired generation after transaction restart")?;
    ensure!(
        manifest.generation == *generation,
        "persisted repair session and immutable generation manifest disagree"
    );
    Ok(manifest.generation)
}

fn desired_from_generation(generation: &GenerationRef) -> DesiredBuild {
    DesiredBuild {
        source: generation.source.clone(),
        patch_fingerprint: generation.patch_fingerprint.clone(),
        target: generation.target.clone(),
        source_key: generation.source_key.clone(),
    }
}

fn ensure_session_still_current(store: &StateStore, session: &RepairSession) -> Result<()> {
    store.with_state_lock(|| {
        let state = store.require()?;
        let failure = state
            .failure
            .as_ref()
            .context("recorded failure disappeared during repair")?;
        ensure!(
            failure.id == session.failure_id && failure.desired == session.desired,
            "recorded failure changed during repair"
        );
        ensure!(
            state.active.as_ref() == Some(&session.last_good),
            "active generation changed during repair"
        );
        let live = PatchSet::load(&state.patch_dir)?;
        ensure!(
            live.fingerprint == session.desired.patch_fingerprint,
            "live patch stack changed during repair"
        );
        let config = fs::read(state.patch_dir.join("codex-patcher.toml"))?;
        ensure!(
            sha256(&config) == session.config_sha256,
            "codex-patcher.toml changed during repair"
        );
        Ok(())
    })
}

fn confirm_replacements(
    patch_dir: &Path,
    replacements: &[FileReplacement],
    previews: &[ReplacementPreview],
    options: RunRepairOptions,
) -> Result<bool> {
    show_patch_file_changes(patch_dir, replacements, previews)?;
    match options.confirmation {
        RepairConfirmation::Accept => Ok(true),
        RepairConfirmation::Decline => Ok(false),
        RepairConfirmation::Prompt => {
            ensure!(
                std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
                "repair validation completed, but explicit confirmation requires a TTY"
            );
            Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt("Replace the live patch stack with these validated files?")
                .default(false)
                .interact()
                .context("reading repair confirmation")
        }
    }
}

fn show_patch_file_changes(
    patch_dir: &Path,
    replacements: &[FileReplacement],
    previews: &[ReplacementPreview],
) -> Result<()> {
    eprintln!("\nValidated repair patch-file changes:");
    for preview in previews {
        eprintln!(
            "  {:?} {}  {} -> {}",
            preview.action,
            preview.relative_path.display(),
            preview.old_sha256.as_deref().unwrap_or("missing"),
            preview.new_sha256.as_deref().unwrap_or("deleted")
        );
    }
    eprintln!();
    for replacement in replacements {
        let target = patch_dir.join(&replacement.relative_path);
        let old = match fs::read(&target) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", target.display()));
            }
        };
        if old == replacement.contents {
            continue;
        }
        eprintln!("--- old/{}", replacement.relative_path.display());
        eprintln!("+++ new/{}", replacement.relative_path.display());
        match (&old, &replacement.contents) {
            (Some(old), Some(new)) => print_sanitized_diff(old, new),
            (None, Some(new)) => print_prefixed_bytes('+', new),
            (Some(old), None) => print_prefixed_bytes('-', old),
            (None, None) => {}
        }
    }
    Ok(())
}

fn print_sanitized_diff(old: &[u8], new: &[u8]) {
    let old_lines: Vec<_> = String::from_utf8_lossy(old)
        .lines()
        .map(str::to_owned)
        .collect();
    let new_lines: Vec<_> = String::from_utf8_lossy(new)
        .lines()
        .map(str::to_owned)
        .collect();
    let prefix = old_lines
        .iter()
        .zip(&new_lines)
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = old_lines[prefix..]
        .iter()
        .rev()
        .zip(new_lines[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    for line in &old_lines[prefix..old_lines.len().saturating_sub(suffix)] {
        eprintln!("-{}", single_line(line));
    }
    for line in &new_lines[prefix..new_lines.len().saturating_sub(suffix)] {
        eprintln!("+{}", single_line(line));
    }
}

fn print_prefixed_bytes(prefix: char, bytes: &[u8]) {
    for line in String::from_utf8_lossy(bytes).lines() {
        eprintln!("{prefix}{}", single_line(line));
    }
}

fn cleanup_repair_session(paths: &PatcherPaths, session: &RepairSession) {
    let store = StateStore::new(paths.clone());
    if let Ok(_lock) = store.build_lock() {
        remove_repair_worktree_best_effort(paths, session);
    }
    let _ = fs::remove_dir_all(&session.snapshot_dir);
    let _ = fs::remove_dir_all(&session.candidate_dir);
    let _ = fs::remove_file(repair_session_path(paths, &session.failure_id));
}

fn remove_repair_worktree_best_effort(paths: &PatcherPaths, session: &RepairSession) {
    let Ok(mut log) = OpenOptions::new().create(true).append(true).open(
        paths
            .logs_dir()
            .join(format!("repair-{}.log", session.failure_id)),
    ) else {
        return;
    };
    let _ = remove_repair_worktree(paths, &session.worktree, &mut log);
}

pub fn find_reject_files(worktree: &Path) -> Result<Vec<PathBuf>> {
    ensure!(
        worktree.is_dir(),
        "repair worktree does not exist: {}",
        worktree.display()
    );
    let mut rejects = Vec::new();
    for entry in WalkDir::new(worktree)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !is_dot_git(entry))
    {
        let entry = entry.with_context(|| format!("walking {}", worktree.display()))?;
        if !entry.file_type().is_dir() && entry.path().extension() == Some(OsStr::new("rej")) {
            rejects.push(entry.path().to_path_buf());
        }
    }
    rejects.sort_by(|left, right| left.as_os_str().cmp(right.as_os_str()));
    Ok(rejects)
}

fn is_dot_git(entry: &DirEntry) -> bool {
    entry.depth() > 0 && entry.file_name() == OsStr::new(".git")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PatchCommit {
    pub parent: String,
    pub commit: String,
    pub name: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedPatch {
    pub name: PathBuf,
    pub bytes: Vec<u8>,
}

pub fn export_normalized_patches(
    repository: &Path,
    patches: &[PatchCommit],
) -> Result<Vec<ExportedPatch>> {
    ensure!(
        repository.is_dir(),
        "repair repository does not exist: {}",
        repository.display()
    );
    let mut names = HashSet::new();
    let mut exported = Vec::with_capacity(patches.len());
    for patch in patches {
        validate_relative_path(&patch.name)?;
        ensure!(
            patch.name.extension() == Some(OsStr::new("patch")),
            "exported patch name must end in .patch: {}",
            patch.name.display()
        );
        ensure!(
            names.insert(casefold_path(&patch.name)),
            "duplicate or case-colliding patch name: {}",
            patch.name.display()
        );
        validate_revision(&patch.parent)?;
        validate_revision(&patch.commit)?;
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args([
                "diff",
                "--binary",
                "--full-index",
                "--no-color",
                "--no-ext-diff",
                "--no-renames",
                "--src-prefix=a/",
                "--dst-prefix=b/",
            ])
            .arg(&patch.parent)
            .arg(&patch.commit)
            .arg("--")
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .output()
            .with_context(|| format!("exporting patch {}", patch.name.display()))?;
        ensure!(
            output.status.success(),
            "git diff failed for {}: {}",
            patch.name.display(),
            single_line(&String::from_utf8_lossy(&output.stderr))
        );
        ensure!(
            !output.stdout.is_empty(),
            "patch {} produced an empty diff",
            patch.name.display()
        );
        exported.push(ExportedPatch {
            name: patch.name.clone(),
            bytes: output.stdout,
        });
    }
    Ok(exported)
}

pub fn patch_stack_replacements(exported: &[ExportedPatch]) -> Vec<FileReplacement> {
    let mut replacements: Vec<_> = exported
        .iter()
        .map(|patch| FileReplacement::write(patch.name.clone(), patch.bytes.clone()))
        .collect();
    let series = exported
        .iter()
        .map(|patch| patch.name.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    replacements.push(FileReplacement::write("series", series.into_bytes()));
    replacements
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReplacement {
    pub relative_path: PathBuf,
    pub contents: Option<Vec<u8>>,
}

impl FileReplacement {
    pub fn write(path: impl Into<PathBuf>, contents: impl Into<Vec<u8>>) -> Self {
        Self {
            relative_path: path.into(),
            contents: Some(contents.into()),
        }
    }

    pub fn delete(path: impl Into<PathBuf>) -> Self {
        Self {
            relative_path: path.into(),
            contents: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplacementAction {
    Create,
    Update,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplacementPreview {
    pub relative_path: PathBuf,
    pub action: ReplacementAction,
    pub old_sha256: Option<String>,
    pub new_sha256: Option<String>,
}

pub fn journaled_replace<C, P>(
    patch_dir: &Path,
    replacements: &[FileReplacement],
    confirm: C,
    source_fingerprint_precheck: P,
) -> Result<bool>
where
    C: FnOnce(&[ReplacementPreview]) -> Result<bool>,
    P: FnOnce() -> Result<()>,
{
    ensure!(
        patch_dir.is_dir(),
        "patch directory does not exist: {}",
        patch_dir.display()
    );
    recover_replacement_journal(patch_dir)?;
    let prepared = prepare_replacements(patch_dir, replacements)?;
    if !confirm(&prepared.previews)? {
        return Ok(false);
    }
    source_fingerprint_precheck()?;
    apply_prepared(patch_dir, prepared)?;
    Ok(true)
}

#[derive(Debug)]
struct PreparedReplacement {
    relative_path: PathBuf,
    contents: Option<Vec<u8>>,
    original: Option<Vec<u8>>,
    original_mode: Option<u32>,
}

#[derive(Debug)]
struct PreparedSet {
    previews: Vec<ReplacementPreview>,
    entries: Vec<PreparedReplacement>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum JournalPhase {
    Applying,
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum EntryState {
    Prepared,
    Applying,
    Applied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalEntry {
    relative_path: PathBuf,
    original_exists: bool,
    original_mode: Option<u32>,
    staged: Option<PathBuf>,
    backup: Option<PathBuf>,
    state: EntryState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReplacementJournal {
    schema: u32,
    transaction_dir: PathBuf,
    phase: JournalPhase,
    entries: Vec<JournalEntry>,
}

fn prepare_replacements(patch_dir: &Path, replacements: &[FileReplacement]) -> Result<PreparedSet> {
    ensure!(!replacements.is_empty(), "replacement set is empty");
    let mut seen = HashSet::new();
    let mut previews = Vec::with_capacity(replacements.len());
    let mut entries = Vec::with_capacity(replacements.len());
    for replacement in replacements {
        validate_relative_path(&replacement.relative_path)?;
        ensure!(
            seen.insert(casefold_path(&replacement.relative_path)),
            "duplicate or case-colliding replacement: {}",
            replacement.relative_path.display()
        );
        let target = checked_target(patch_dir, &replacement.relative_path)?;
        let (original, original_mode) = match fs::symlink_metadata(&target) {
            Ok(metadata) => {
                ensure!(
                    metadata.file_type().is_file(),
                    "replacement target is not a regular file: {}",
                    target.display()
                );
                (Some(fs::read(&target)?), permission_mode(&metadata))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, None),
            Err(error) => {
                return Err(error).with_context(|| format!("inspecting {}", target.display()));
            }
        };
        if original.is_none() && replacement.contents.is_none() {
            bail!("cannot delete missing file {}", target.display());
        }
        let action = match (&original, &replacement.contents) {
            (None, Some(_)) => ReplacementAction::Create,
            (Some(_), Some(_)) => ReplacementAction::Update,
            (Some(_), None) => ReplacementAction::Delete,
            (None, None) => unreachable!(),
        };
        previews.push(ReplacementPreview {
            relative_path: replacement.relative_path.clone(),
            action,
            old_sha256: original.as_deref().map(sha256),
            new_sha256: replacement.contents.as_deref().map(sha256),
        });
        entries.push(PreparedReplacement {
            relative_path: replacement.relative_path.clone(),
            contents: replacement.contents.clone(),
            original,
            original_mode,
        });
    }
    Ok(PreparedSet { previews, entries })
}

fn apply_prepared(patch_dir: &Path, prepared: PreparedSet) -> Result<()> {
    let transaction_name = format!(".codex-patcher-repair-{}", uuid::Uuid::new_v4());
    let transaction_dir = patch_dir.join(&transaction_name);
    fs::create_dir(&transaction_dir).with_context(|| {
        format!(
            "creating transaction directory {}",
            transaction_dir.display()
        )
    })?;
    let mut journal = ReplacementJournal {
        schema: JOURNAL_SCHEMA,
        transaction_dir: PathBuf::from(&transaction_name),
        phase: JournalPhase::Applying,
        entries: Vec::with_capacity(prepared.entries.len()),
    };

    for (index, entry) in prepared.entries.iter().enumerate() {
        let staged = entry
            .contents
            .as_ref()
            .map(|bytes| {
                let relative = PathBuf::from(format!("staged-{index}"));
                write_new_sync(&transaction_dir.join(&relative), bytes, entry.original_mode)?;
                Ok::<_, anyhow::Error>(relative)
            })
            .transpose()?;
        let backup = entry
            .original
            .as_ref()
            .map(|bytes| {
                let relative = PathBuf::from(format!("backup-{index}"));
                write_new_sync(&transaction_dir.join(&relative), bytes, entry.original_mode)?;
                Ok::<_, anyhow::Error>(relative)
            })
            .transpose()?;
        journal.entries.push(JournalEntry {
            relative_path: entry.relative_path.clone(),
            original_exists: entry.original.is_some(),
            original_mode: entry.original_mode,
            staged,
            backup,
            state: EntryState::Prepared,
        });
    }
    save_journal(patch_dir, &journal)?;

    let result = (|| {
        for index in 0..journal.entries.len() {
            journal.entries[index].state = EntryState::Applying;
            save_journal(patch_dir, &journal)?;
            let entry = &journal.entries[index];
            let target = checked_target(patch_dir, &entry.relative_path)?;
            if let Some(staged) = &entry.staged {
                install_from_bytes(
                    &target,
                    &fs::read(transaction_dir.join(staged))?,
                    entry.original_mode,
                )?;
            } else if target.exists() {
                fs::remove_file(&target)
                    .with_context(|| format!("deleting {}", target.display()))?;
                sync_parent(&target);
            }
            journal.entries[index].state = EntryState::Applied;
            save_journal(patch_dir, &journal)?;
        }
        journal.phase = JournalPhase::Complete;
        save_journal(patch_dir, &journal)
    })();

    if let Err(error) = result {
        let rollback = rollback_journal(patch_dir, &journal);
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(error.context(format!(
                "rollback also failed: {rollback_error:#}; run recovery before continuing"
            ))),
        };
    }
    cleanup_journal(patch_dir, &journal)
}

pub fn recover_replacement_journal(patch_dir: &Path) -> Result<()> {
    let path = patch_dir.join(JOURNAL_NAME);
    if !path.exists() {
        return Ok(());
    }
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let journal: ReplacementJournal =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    ensure!(
        journal.schema == JOURNAL_SCHEMA,
        "unsupported repair journal schema {}",
        journal.schema
    );
    validate_relative_path(&journal.transaction_dir)?;
    if journal.phase == JournalPhase::Complete {
        cleanup_journal(patch_dir, &journal)
    } else {
        rollback_journal(patch_dir, &journal)?;
        cleanup_journal(patch_dir, &journal)
    }
}

fn rollback_journal(patch_dir: &Path, journal: &ReplacementJournal) -> Result<()> {
    let transaction_dir = checked_target(patch_dir, &journal.transaction_dir)?;
    for entry in journal.entries.iter().rev() {
        if entry.state == EntryState::Prepared {
            continue;
        }
        let target = checked_target(patch_dir, &entry.relative_path)?;
        if entry.original_exists {
            let backup = entry
                .backup
                .as_ref()
                .context("journal is missing an original-file backup")?;
            validate_relative_path(backup)?;
            install_from_bytes(
                &target,
                &fs::read(transaction_dir.join(backup))?,
                entry.original_mode,
            )?;
        } else if target.exists() {
            fs::remove_file(&target)
                .with_context(|| format!("rolling back {}", target.display()))?;
            sync_parent(&target);
        }
    }
    Ok(())
}

fn cleanup_journal(patch_dir: &Path, journal: &ReplacementJournal) -> Result<()> {
    let transaction_dir = checked_target(patch_dir, &journal.transaction_dir)?;
    if transaction_dir.exists() {
        fs::remove_dir_all(&transaction_dir)
            .with_context(|| format!("removing {}", transaction_dir.display()))?;
    }
    let journal_path = patch_dir.join(JOURNAL_NAME);
    match fs::remove_file(&journal_path) {
        Ok(()) => sync_parent(&journal_path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("removing {}", journal_path.display()));
        }
    }
    Ok(())
}

fn save_journal(patch_dir: &Path, journal: &ReplacementJournal) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(journal)?;
    install_from_bytes(&patch_dir.join(JOURNAL_NAME), &bytes, None)
}

fn write_new_sync(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    set_permission_mode(path, mode)?;
    Ok(())
}

fn install_from_bytes(target: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    let parent = target.parent().context("target has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        target
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("replacement"),
        uuid::Uuid::new_v4()
    ));
    write_new_sync(&temporary, bytes, mode)?;
    if let Err(error) = atomic_install(&temporary, target) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    sync_parent(target);
    Ok(())
}

#[cfg(not(windows))]
fn atomic_install(temporary: &Path, target: &Path) -> Result<()> {
    fs::rename(temporary, target)
        .with_context(|| format!("atomically replacing {}", target.display()))
}

#[cfg(windows)]
fn atomic_install(temporary: &Path, target: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let from: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let result = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
            .with_context(|| format!("atomically replacing {}", target.display()))
    } else {
        Ok(())
    }
}

fn checked_target(root: &Path, relative: &Path) -> Result<PathBuf> {
    validate_relative_path(relative)?;
    let mut current = root.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(part) = component else {
            unreachable!()
        };
        current.push(part);
        if index + 1 < components.len() {
            match fs::symlink_metadata(&current) {
                Ok(metadata) => ensure!(
                    metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
                    "replacement path traverses a non-directory or symlink: {}",
                    current.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("inspecting {}", current.display()));
                }
            }
        }
    }
    Ok(current)
}

fn validate_relative_path(path: &Path) -> Result<()> {
    ensure!(!path.as_os_str().is_empty(), "path is empty");
    ensure!(
        !path.is_absolute(),
        "path must be relative: {}",
        path.display()
    );
    for component in path.components() {
        ensure!(
            matches!(component, Component::Normal(_)),
            "unsafe relative path: {}",
            path.display()
        );
        if let Component::Normal(part) = component {
            let part = part
                .to_str()
                .with_context(|| format!("path is not valid UTF-8: {}", path.display()))?;
            ensure!(
                !part.contains('\\') && !part.contains('\0'),
                "unsafe relative path: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn validate_revision(revision: &str) -> Result<()> {
    ensure!(!revision.is_empty(), "Git revision is empty");
    ensure!(
        !revision.starts_with('-'),
        "unsafe Git revision: {revision}"
    );
    ensure!(
        !revision
            .chars()
            .any(|character| character.is_control() || character.is_whitespace()),
        "unsafe Git revision: {revision}"
    );
    Ok(())
}

fn casefold_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .nfkc()
        .flat_map(char::to_lowercase)
        .nfkc()
        .collect()
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn single_line(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn sync_parent(path: &Path) {
    let Some(parent) = path.parent() else {
        return;
    };
    let Ok(directory) = File::open(parent) else {
        return;
    };
    let _ = directory.sync_all();
}

#[cfg(unix)]
fn permission_mode(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode())
}

#[cfg(unix)]
fn set_permission_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(windows)]
fn permission_mode(metadata: &fs::Metadata) -> Option<u32> {
    Some(u32::from(metadata.permissions().readonly()))
}

#[cfg(windows)]
fn set_permission_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    if let Some(mode) = mode {
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_readonly(mode != 0);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn permission_mode(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

#[cfg(not(any(unix, windows)))]
fn set_permission_mode(_path: &Path, _mode: Option<u32>) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ResolvedSource;
    use std::cell::Cell;

    #[test]
    fn maintenance_is_direct_constrained_and_recursion_safe() {
        let command = maintenance_command(
            Path::new("/generation/bin/codex"),
            Path::new("/repair/worktree"),
            "repair this",
            false,
        );
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            args,
            "-c check_for_update_on_startup=false -C /repair/worktree --sandbox workspace-write \
             --ask-for-approval on-request --no-alt-screen repair this"
        );
        assert!(
            command
                .get_envs()
                .any(|(key, value)| { key == MAINTENANCE_ENV && value == Some(OsStr::new("1")) })
        );
        let yolo_args = maintenance_command(
            Path::new("/generation/bin/codex"),
            Path::new("/repair/worktree"),
            "repair this",
            true,
        )
        .get_args()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
        assert_eq!(
            yolo_args,
            "-c check_for_update_on_startup=false -C /repair/worktree \
             --dangerously-bypass-approvals-and-sandbox --no-alt-screen repair this"
        );
        let prompt = generate_repair_prompt(&RepairPromptContext {
            failure_id: "failure-1".into(),
            desired_version: "0.145.0".into(),
            upstream_ref: "rust-v0.145.0".into(),
            failed_patch: Some("0002-change.patch".into()),
            phase: "apply".into(),
            summary: "hunk failed".into(),
            log_path: "/state/failure.log".into(),
            patch_dir: "/patches".into(),
        });
        for required in [
            "0002-change.patch",
            "do not edit directly",
            "Do not invoke codex-patcher",
            "Do not commit",
            "Remove every .rej file",
            "focused offline checks",
            "authoritative package rebuild",
        ] {
            assert!(prompt.contains(required));
        }
    }

    #[test]
    fn replacement_confirms_then_prechecks_and_refuses_drift() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("old.patch"), b"old").unwrap();
        let confirmed = Cell::new(false);
        let applied = journaled_replace(
            temp.path(),
            &[
                FileReplacement::write("old.patch", b"new".to_vec()),
                FileReplacement::write("series", b"old.patch\n".to_vec()),
            ],
            |preview| {
                assert_eq!(preview[0].action, ReplacementAction::Update);
                assert_eq!(preview[1].action, ReplacementAction::Create);
                confirmed.set(true);
                Ok(true)
            },
            || {
                assert!(confirmed.get());
                Ok(())
            },
        )
        .unwrap();
        assert!(applied);
        assert_eq!(fs::read(temp.path().join("old.patch")).unwrap(), b"new");
        assert_eq!(
            fs::read(temp.path().join("series")).unwrap(),
            b"old.patch\n"
        );
        assert!(!temp.path().join(JOURNAL_NAME).exists());
        let error = journaled_replace(
            temp.path(),
            &[FileReplacement::write("old.patch", b"drifted".to_vec())],
            |_| Ok(true),
            || bail!("live fingerprint drifted"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("fingerprint drifted"));
        assert_eq!(fs::read(temp.path().join("old.patch")).unwrap(), b"new");
    }

    #[test]
    fn conflict_resume_and_build_only_repairs_preserve_patch_order() {
        let repository = tempfile::tempdir().unwrap();
        initialize_repository(repository.path());
        fs::write(repository.path().join("file.txt"), b"base\n").unwrap();
        fs::write(repository.path().join("blob.bin"), [0; 256]).unwrap();
        git(repository.path(), &["add", "file.txt", "blob.bin"]);
        git(repository.path(), &["commit", "-qm", "base"]);
        let base = oid(repository.path());
        fs::write(repository.path().join("file.txt"), b"first\n").unwrap();
        fs::write(repository.path().join("blob.bin"), [1; 256]).unwrap();
        let first = git(repository.path(), &["diff", "--binary", "HEAD", "--"]);
        git(repository.path(), &["restore", "."]);
        fs::write(repository.path().join("file.txt"), b"second\n").unwrap();
        let second = git(repository.path(), &["diff", "--binary", "HEAD", "--"]);
        git(repository.path(), &["restore", "."]);
        let inputs = tempfile::tempdir().unwrap();
        fs::write(inputs.path().join("one.patch"), first).unwrap();
        fs::write(inputs.path().join("two.patch"), second).unwrap();
        fs::write(inputs.path().join("series"), b"one.patch\ntwo.patch\n").unwrap();
        let patches = PatchSet::load(inputs.path()).unwrap();
        let mut session = test_session(repository.path(), inputs.path(), &base, &patches);
        configure_synthetic_repository(repository.path()).unwrap();
        continue_applying(&mut session).unwrap();
        assert_eq!(session.commits.len(), 1);
        assert_eq!(
            session.stage,
            RepairStage::Applying {
                next_index: 1,
                conflicted_index: Some(1)
            }
        );
        fs::write(repository.path().join(".git/internal.rej"), b"ignored").unwrap();
        assert_eq!(find_reject_files(repository.path()).unwrap().len(), 1);
        for reject in find_reject_files(repository.path()).unwrap() {
            fs::remove_file(reject).unwrap();
        }
        fs::write(repository.path().join("file.txt"), b"second\n").unwrap();
        finish_conflicted_patch(&mut session, 1).unwrap();
        continue_applying(&mut session).unwrap();
        assert_eq!(session.stage, RepairStage::ReadyToBuild);
        assert_eq!(session.commits.len(), 2);
        let exported = export_normalized_patches(repository.path(), &session.commits).unwrap();
        assert_eq!(exported[0].name, Path::new("one.patch"));
        assert_eq!(exported[1].name, Path::new("two.patch"));
        let first = String::from_utf8_lossy(&exported[0].bytes);
        let indexes = first
            .lines()
            .find(|line| line.starts_with("index "))
            .unwrap();
        let (old, new) = indexes
            .split_whitespace()
            .nth(1)
            .unwrap()
            .split_once("..")
            .unwrap();
        assert!(first.contains("GIT binary patch"));
        assert_eq!((old.len(), new.len()), (40, 40));
        session.stage = RepairStage::BuildRepair;
        fs::write(repository.path().join("repair.txt"), b"build fix\n").unwrap();
        finish_build_repair(&mut session).unwrap();
        assert_eq!(session.commits.len(), 3);
        let exported = export_normalized_patches(repository.path(), &session.commits).unwrap();
        assert_eq!(
            exported.last().unwrap().name,
            Path::new(BUILD_REPAIR_PATCH_BASENAME)
        );
        assert!(String::from_utf8_lossy(&exported.last().unwrap().bytes).contains("+build fix"));
    }

    fn initialize_repository(repository: &Path) {
        git(repository, &["init", "-q"]);
        git(
            repository,
            &["config", "user.email", "test@example.invalid"],
        );
        git(repository, &["config", "user.name", "Test"]);
    }

    fn test_session(
        repository: &Path,
        inputs: &Path,
        base: &str,
        patches: &PatchSet,
    ) -> RepairSession {
        let source = ResolvedSource {
            channel: "stable".into(),
            ref_name: "refs/tags/rust-v1.0.0".into(),
            ref_object_oid: base.into(),
            commit_oid: base.into(),
            version: "1.0.0".into(),
            release_url: None,
        };
        let desired = DesiredBuild {
            source: source.clone(),
            patch_fingerprint: patches.fingerprint.clone(),
            target: "test-target".into(),
            source_key: "a".repeat(64),
        };
        let generation = GenerationRef {
            id: "last-good".into(),
            package_dir: repository.into(),
            binary: repository.join("codex"),
            source_key: "b".repeat(64),
            source,
            patch_fingerprint: "c".repeat(64),
            target: "test-target".into(),
            subcommands: Vec::new(),
            built_at: Utc::now(),
        };
        RepairSession {
            schema: REPAIR_SESSION_SCHEMA,
            failure_id: "test-failure".into(),
            desired,
            config_sha256: String::new(),
            worktree: repository.into(),
            snapshot_dir: inputs.into(),
            candidate_dir: inputs.join("candidate"),
            last_good: generation,
            commits: Vec::new(),
            stage: RepairStage::Applying {
                next_index: 0,
                conflicted_index: None,
            },
            final_patch_name: None,
            diagnostic_phase: "patch".into(),
            diagnostic_summary: "test".into(),
            diagnostic_log: inputs.join("failure.log"),
            validated_generation: None,
        }
    }

    fn git(repository: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?} failed");
        output.stdout
    }

    fn oid(repository: &Path) -> String {
        String::from_utf8(git(repository, &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .into()
    }
}
