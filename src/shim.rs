use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};
use std::time::Duration;
use uuid::Uuid;
#[cfg(target_os = "linux")]
use wait_timeout::ChildExt;

pub const SURFACE_RECORD_SCHEMA: u32 = 1;
const TAKEOVER_JOURNAL_SCHEMA: u32 = 1;
const TAKEOVER_JOURNAL_PREFIX: &str = ".takeover-";
const TAKEOVER_JOURNAL_SUFFIX: &str = ".json";
const RESTORE_JOURNAL_SCHEMA: u32 = 1;
const RESTORE_JOURNAL_PREFIX: &str = ".restore-";
const RESTORE_JOURNAL_SUFFIX: &str = ".json";
const REPAIR_JOURNAL_SCHEMA: u32 = 1;
const REPAIR_JOURNAL_PREFIX: &str = ".repair-";
const REPAIR_JOURNAL_SUFFIX: &str = ".json";
pub const CODEX_UPDATE_MANAGER_UNIT: &str = "codex-update-manager.service";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BaselineKind {
    Missing,
    File,
    Symlink,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct FileIdentity {
    pub device: Option<u64>,
    pub inode: Option<u64>,
    pub volume_serial: Option<u64>,
    pub file_id: Option<String>,
    pub length: u64,
    pub modified_ns: Option<i128>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtendedAttribute {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

/// Platform-native ownership and access-control state captured independently
/// from ordinary mode/file-attribute bits. Optional storage keeps development
/// records from before this metadata existed readable; every new non-missing
/// baseline records `Some`, including an explicitly empty ACL/xattr set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SecurityMetadata {
    pub unix_uid: Option<u32>,
    pub unix_gid: Option<u32>,
    #[serde(default)]
    pub extended_attributes: Vec<ExtendedAttribute>,
    #[serde(default)]
    pub macos_acl_text: Option<Vec<u8>>,
    #[serde(default)]
    pub windows_security_descriptor: Option<Vec<u8>>,
    #[serde(default)]
    pub windows_dacl_protected: Option<bool>,
}

/// Exact recoverable state of a surface before takeover.
///
/// Regular-file bytes are retained in `backup_path`; symlinks retain their raw
/// (possibly relative) target. Identity fields document what was captured,
/// while compare-and-swap decisions use kind, bytes/hash, target, and mode so a
/// restored copy is still recognizable after its inode changes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BaselineBackup {
    pub kind: BaselineKind,
    pub backup_path: Option<PathBuf>,
    pub symlink_target: Option<PathBuf>,
    pub mode: Option<u32>,
    pub sha256: Option<String>,
    pub identity: Option<FileIdentity>,
    #[serde(default)]
    pub security: Option<SecurityMetadata>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ShimForm {
    PosixSymlink,
    WindowsNative,
    WindowsCmd,
    WindowsPowerShell,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledShim {
    pub form: ShimForm,
    pub manager: PathBuf,
    pub symlink_target: Option<PathBuf>,
    pub mode: Option<u32>,
    pub sha256: Option<String>,
    pub identity: Option<FileIdentity>,
    #[serde(default)]
    pub security: Option<SecurityMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SurfaceRecord {
    pub schema: u32,
    pub id: Uuid,
    pub surface: PathBuf,
    pub baseline: BaselineBackup,
    pub installed: InstalledShim,
    pub installed_at: DateTime<Utc>,
    pub updater: Option<UpdaterAdapterRecord>,
}

/// Durable intent for one launcher takeover.
///
/// The prepared form (`installed == None`) is written and synced before the
/// launcher is touched. The completed form is written after the replacement
/// has been inspected. The journal is deliberately kept until the caller has
/// durably added the returned [`SurfaceRecord`] to manager state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TakeoverJournal {
    schema: u32,
    id: Uuid,
    surface: PathBuf,
    manager: PathBuf,
    baseline: BaselineBackup,
    started_at: DateTime<Utc>,
    installed: Option<InstalledShim>,
    installed_at: Option<DateTime<Utc>>,
}

/// Durable intent for one compare-and-swap baseline restoration.
///
/// A journal without `completed_at` is written before the installed launcher
/// is replaced. Once the baseline is verified, `completed_at` is persisted and
/// the journal remains until manager state no longer contains `record`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RestoreJournal {
    schema: u32,
    id: Uuid,
    record: SurfaceRecord,
    started_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
}

/// Exact observed state used as the compare-and-swap precondition for a
/// launcher repair. Unlike a restore baseline, this has no payload backup: it
/// exists only to prove that the surface has not drifted before retrying the
/// already-authorized mutation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SurfaceSnapshot {
    kind: BaselineKind,
    symlink_target: Option<PathBuf>,
    mode: Option<u32>,
    sha256: Option<String>,
    identity: Option<FileIdentity>,
    #[serde(default)]
    security: Option<SecurityMetadata>,
}

/// Durable intent and result for one repair-shims replacement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RepairJournal {
    schema: u32,
    id: Uuid,
    original: SurfaceRecord,
    manager: PathBuf,
    outcome: RepairOutcome,
    replacement_baseline: BaselineBackup,
    observed: SurfaceSnapshot,
    started_at: DateTime<Utc>,
    updated: Option<SurfaceRecord>,
    completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RepairOutcome {
    Unchanged,
    ReinstalledOwned,
    ReinstalledBaseline,
    AdoptedAndReinstalled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdaterAdapterRecord {
    pub unit: String,
    pub load_state: String,
    pub enabled_state: String,
    pub active_state: String,
    /// Persisted before takeover so a crash between a systemctl action and the
    /// next state write can still restore the exact pre-install state.
    #[serde(default)]
    pub disable_intended: bool,
    #[serde(default)]
    pub stop_intended: bool,
    pub disabled_by_patcher: bool,
    pub stopped_by_patcher: bool,
    pub notes: Vec<String>,
}

/// Capture the exact baseline and atomically redirect a surface to the stable
/// manager.
pub fn install_redirect(
    surface: &Path,
    manager: &Path,
    backups_dir: &Path,
) -> Result<SurfaceRecord> {
    let surface = absolute_path(surface)?;
    let manager = absolute_path(manager)?;
    let backups_dir = absolute_path(backups_dir)?;
    validate_redirect_paths(&surface, &manager)?;
    let baseline = capture_baseline(&surface, &backups_dir)?;
    let mut journal = TakeoverJournal {
        schema: TAKEOVER_JOURNAL_SCHEMA,
        id: Uuid::new_v4(),
        surface: surface.clone(),
        manager: manager.clone(),
        baseline,
        started_at: Utc::now(),
        installed: None,
        installed_at: None,
    };

    // This durable intent must precede the first mutation of the launcher.
    write_takeover_journal(&backups_dir, &journal)?;
    cas_install_shim(
        &surface,
        &manager,
        journal.id,
        journal.baseline.kind,
        |displaced| baseline_is_untouched(displaced, &journal.baseline),
    )?;
    let installed = inspect_installed_shim(&surface, &manager)?;
    let installed_at = Utc::now();
    journal.installed = Some(installed.clone());
    journal.installed_at = Some(installed_at);
    // If this write fails, the prepared journal remains sufficient for
    // recovery to recognize and inspect the manager shim on the next run.
    write_takeover_journal(&backups_dir, &journal)?;

    Ok(SurfaceRecord {
        schema: SURFACE_RECORD_SCHEMA,
        id: journal.id,
        surface,
        baseline: journal.baseline,
        installed,
        installed_at,
        updater: None,
    })
}

/// Remove the takeover journal after `record` has been durably committed to
/// manager state. Calling this for an already-finalized record is harmless.
pub fn finalize_redirect(record: &SurfaceRecord, backups_dir: &Path) -> Result<()> {
    validate_record(record)?;
    let backups_dir = absolute_path(backups_dir)?;
    let path = takeover_journal_path(&backups_dir, record.id);
    match fs::remove_file(&path) {
        Ok(()) => sync_parent(&path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

/// Recover takeover journals left behind by a crash.
///
/// Prepared journals whose baseline is still current represent operations
/// which never mutated the launcher and are finalized immediately. A current
/// manager shim is inspected and returned as a [`SurfaceRecord`]; its journal
/// remains until the caller saves that record and invokes [`finalize_redirect`].
/// Any other current contents are external drift, so recovery fails without
/// modifying the launcher or discarding the journal.
pub fn recover_redirect_journals(backups_dir: &Path) -> Result<Vec<SurfaceRecord>> {
    let backups_dir = absolute_path(backups_dir)?;
    let journal_paths = takeover_journal_paths(&backups_dir)?;
    let mut actions = Vec::with_capacity(journal_paths.len());

    // Classify every journal before deleting or rewriting any of them. This
    // keeps an unexpected drift failure from partially consuming the recovery
    // set and losing records that the caller has not yet persisted.
    for path in journal_paths {
        let journal = read_takeover_journal(&path)?;
        validate_takeover_journal(&journal, &path)?;
        recover_takeover_swap(&journal)?;

        if baseline_is_untouched(&journal.surface, &journal.baseline)? {
            actions.push(RecoveryAction::NoMutation { path, journal });
            continue;
        }
        if matches_shim(&journal.surface, &journal.manager)? {
            let installed = inspect_installed_shim(&journal.surface, &journal.manager)?;
            let installed_at = journal.installed_at.unwrap_or(journal.started_at);
            let record = SurfaceRecord {
                schema: SURFACE_RECORD_SCHEMA,
                id: journal.id,
                surface: journal.surface.clone(),
                baseline: journal.baseline.clone(),
                installed: installed.clone(),
                installed_at,
                updater: None,
            };
            let mut completed = journal;
            completed.installed = Some(installed);
            completed.installed_at = Some(installed_at);
            actions.push(RecoveryAction::Installed {
                completed,
                record: Box::new(record),
            });
            continue;
        }

        bail!(
            "takeover journal {} cannot be recovered because {} matches neither its baseline nor the codex-patcher manager shim; refusing to overwrite drift",
            path.display(),
            journal.surface.display()
        );
    }

    let mut recovered = Vec::new();
    for action in actions {
        match action {
            RecoveryAction::NoMutation { path, journal } => {
                if !baseline_is_untouched(&journal.surface, &journal.baseline)? {
                    bail!(
                        "{} changed while recovering takeover journal {}; leaving the journal intact",
                        journal.surface.display(),
                        path.display()
                    );
                }
                cleanup_unused_baseline(&journal.baseline, &backups_dir)?;
                fs::remove_file(&path).with_context(|| {
                    format!("remove unused takeover journal {}", path.display())
                })?;
                sync_parent(&path)?;
            }
            RecoveryAction::Installed { completed, record } => {
                if !matches_recorded_shim(&record)? {
                    bail!(
                        "{} changed while recovering its takeover journal; leaving the journal intact",
                        record.surface.display()
                    );
                }
                write_takeover_journal(&backups_dir, &completed)?;
                recovered.push(*record);
            }
        }
    }
    Ok(recovered)
}

enum RecoveryAction {
    NoMutation {
        path: PathBuf,
        journal: TakeoverJournal,
    },
    Installed {
        completed: TakeoverJournal,
        record: Box<SurfaceRecord>,
    },
}

/// Compatibility alias for callers that model takeover as a redirect.
pub fn redirect_surface(
    surface: &Path,
    manager: &Path,
    backups_dir: &Path,
) -> Result<SurfaceRecord> {
    install_redirect(surface, manager, backups_dir)
}

fn takeover_journal_path(backups_dir: &Path, id: Uuid) -> PathBuf {
    backups_dir.join(format!(
        "{TAKEOVER_JOURNAL_PREFIX}{id}{TAKEOVER_JOURNAL_SUFFIX}"
    ))
}

fn takeover_journal_paths(backups_dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(backups_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read backup directory {}", backups_dir.display()));
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(TAKEOVER_JOURNAL_PREFIX) || !name.ends_with(TAKEOVER_JOURNAL_SUFFIX) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "takeover journal path is not a regular file: {}",
                entry.path().display()
            );
        }
        paths.push(entry.path());
    }
    paths.sort_by(|left, right| left.as_os_str().cmp(right.as_os_str()));
    Ok(paths)
}

fn write_takeover_journal(backups_dir: &Path, journal: &TakeoverJournal) -> Result<()> {
    fs::create_dir_all(backups_dir)
        .with_context(|| format!("create backup directory {}", backups_dir.display()))?;
    let path = takeover_journal_path(backups_dir, journal.id);
    let bytes = serde_json::to_vec_pretty(journal).context("serialize takeover journal")?;
    atomic_write(&path, &bytes, private_file_mode())
        .with_context(|| format!("persist takeover journal {}", path.display()))
}

fn read_takeover_journal(path: &Path) -> Result<TakeoverJournal> {
    const MAX_JOURNAL_BYTES: u64 = 4 * 1024 * 1024;
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_JOURNAL_BYTES {
        bail!("takeover journal is unreasonably large: {}", path.display());
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse takeover journal {}", path.display()))
}

fn validate_takeover_journal(journal: &TakeoverJournal, path: &Path) -> Result<()> {
    if journal.schema != TAKEOVER_JOURNAL_SCHEMA {
        bail!(
            "unsupported takeover journal schema {} in {} (expected {})",
            journal.schema,
            path.display(),
            TAKEOVER_JOURNAL_SCHEMA
        );
    }
    if path.file_name()
        != Some(OsStr::new(&format!(
            "{TAKEOVER_JOURNAL_PREFIX}{}{TAKEOVER_JOURNAL_SUFFIX}",
            journal.id
        )))
    {
        bail!(
            "takeover journal id does not match its filename: {}",
            path.display()
        );
    }
    if !journal.surface.is_absolute() || !journal.manager.is_absolute() {
        bail!(
            "takeover journal contains non-absolute launcher or manager path: {}",
            path.display()
        );
    }
    match (&journal.installed, journal.installed_at) {
        (Some(installed), Some(_)) if installed.manager == journal.manager => {}
        (None, None) => {}
        (Some(_), Some(_)) => bail!(
            "takeover journal installed manager does not match its intent: {}",
            path.display()
        ),
        _ => bail!(
            "takeover journal has inconsistent installed metadata: {}",
            path.display()
        ),
    }
    Ok(())
}

fn restore_journal_path(backups_dir: &Path, id: Uuid) -> PathBuf {
    backups_dir.join(format!(
        "{RESTORE_JOURNAL_PREFIX}{id}{RESTORE_JOURNAL_SUFFIX}"
    ))
}

fn restore_journal_paths(backups_dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(backups_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read backup directory {}", backups_dir.display()));
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(RESTORE_JOURNAL_PREFIX) || !name.ends_with(RESTORE_JOURNAL_SUFFIX) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "restore journal path is not a regular file: {}",
                entry.path().display()
            );
        }
        paths.push(entry.path());
    }
    paths.sort_by(|left, right| left.as_os_str().cmp(right.as_os_str()));
    Ok(paths)
}

fn write_restore_journal(backups_dir: &Path, journal: &RestoreJournal) -> Result<()> {
    fs::create_dir_all(backups_dir)
        .with_context(|| format!("create backup directory {}", backups_dir.display()))?;
    let path = restore_journal_path(backups_dir, journal.id);
    let bytes = serde_json::to_vec_pretty(journal).context("serialize restore journal")?;
    atomic_write(&path, &bytes, private_file_mode())
        .with_context(|| format!("persist restore journal {}", path.display()))
}

fn read_restore_journal(path: &Path) -> Result<RestoreJournal> {
    const MAX_JOURNAL_BYTES: u64 = 4 * 1024 * 1024;
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_JOURNAL_BYTES {
        bail!("restore journal is unreasonably large: {}", path.display());
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse restore journal {}", path.display()))
}

fn validate_restore_journal(journal: &RestoreJournal, path: &Path) -> Result<()> {
    if journal.schema != RESTORE_JOURNAL_SCHEMA {
        bail!(
            "unsupported restore journal schema {} in {} (expected {})",
            journal.schema,
            path.display(),
            RESTORE_JOURNAL_SCHEMA
        );
    }
    if journal.id != journal.record.id {
        bail!(
            "restore journal id does not match its surface record: {}",
            path.display()
        );
    }
    if path.file_name()
        != Some(OsStr::new(&format!(
            "{RESTORE_JOURNAL_PREFIX}{}{RESTORE_JOURNAL_SUFFIX}",
            journal.id
        )))
    {
        bail!(
            "restore journal id does not match its filename: {}",
            path.display()
        );
    }
    validate_record(&journal.record)?;
    if !journal.record.surface.is_absolute() || !journal.record.installed.manager.is_absolute() {
        bail!(
            "restore journal contains non-absolute launcher or manager path: {}",
            path.display()
        );
    }
    Ok(())
}

fn repair_journal_path(backups_dir: &Path, id: Uuid) -> PathBuf {
    backups_dir.join(format!(
        "{REPAIR_JOURNAL_PREFIX}{id}{REPAIR_JOURNAL_SUFFIX}"
    ))
}

fn repair_journal_paths(backups_dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(backups_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read backup directory {}", backups_dir.display()));
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(REPAIR_JOURNAL_PREFIX) || !name.ends_with(REPAIR_JOURNAL_SUFFIX) {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "repair journal path is not a regular file: {}",
                entry.path().display()
            );
        }
        paths.push(entry.path());
    }
    paths.sort_by(|left, right| left.as_os_str().cmp(right.as_os_str()));
    Ok(paths)
}

fn write_repair_journal(backups_dir: &Path, journal: &RepairJournal) -> Result<()> {
    fs::create_dir_all(backups_dir)
        .with_context(|| format!("create backup directory {}", backups_dir.display()))?;
    let path = repair_journal_path(backups_dir, journal.id);
    let bytes = serde_json::to_vec_pretty(journal).context("serialize repair journal")?;
    atomic_write(&path, &bytes, private_file_mode())
        .with_context(|| format!("persist repair journal {}", path.display()))
}

fn read_repair_journal(path: &Path) -> Result<RepairJournal> {
    const MAX_JOURNAL_BYTES: u64 = 4 * 1024 * 1024;
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_JOURNAL_BYTES {
        bail!("repair journal is unreasonably large: {}", path.display());
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse repair journal {}", path.display()))
}

fn validate_repair_journal(journal: &RepairJournal, path: &Path) -> Result<()> {
    if journal.schema != REPAIR_JOURNAL_SCHEMA {
        bail!(
            "unsupported repair journal schema {} in {} (expected {})",
            journal.schema,
            path.display(),
            REPAIR_JOURNAL_SCHEMA
        );
    }
    if journal.id != journal.original.id {
        bail!(
            "repair journal id does not match its original surface record: {}",
            path.display()
        );
    }
    if path.file_name()
        != Some(OsStr::new(&format!(
            "{REPAIR_JOURNAL_PREFIX}{}{REPAIR_JOURNAL_SUFFIX}",
            journal.id
        )))
    {
        bail!(
            "repair journal id does not match its filename: {}",
            path.display()
        );
    }
    validate_record(&journal.original)?;
    if !journal.original.surface.is_absolute() || !journal.manager.is_absolute() {
        bail!(
            "repair journal contains a non-absolute launcher or manager path: {}",
            path.display()
        );
    }
    if journal.outcome == RepairOutcome::Unchanged {
        bail!(
            "repair journal cannot contain an unchanged operation: {}",
            path.display()
        );
    }
    if journal.updated.is_some() != journal.completed_at.is_some() {
        bail!(
            "repair journal has inconsistent completion metadata: {}",
            path.display()
        );
    }
    if let Some(updated) = journal.updated.as_ref() {
        validate_record(updated)?;
        if updated.id != journal.id
            || updated.surface != journal.original.surface
            || updated.baseline != journal.replacement_baseline
            || updated.installed.manager != journal.manager
        {
            bail!(
                "repair journal has an invalid updated surface record: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn private_file_mode() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(0o600)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

pub fn capture_baseline(surface: &Path, backups_dir: &Path) -> Result<BaselineBackup> {
    let metadata = match fs::symlink_metadata(surface) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return finish_captured_baseline(
                surface,
                backups_dir,
                BaselineBackup {
                    kind: BaselineKind::Missing,
                    backup_path: None,
                    symlink_target: None,
                    mode: None,
                    sha256: None,
                    identity: None,
                    security: None,
                },
            );
        }
        Err(error) => return Err(error).with_context(|| format!("inspect {}", surface.display())),
    };

    if metadata.file_type().is_symlink() {
        let target = fs::read_link(surface)
            .with_context(|| format!("read symlink baseline {}", surface.display()))?;
        return finish_captured_baseline(
            surface,
            backups_dir,
            BaselineBackup {
                kind: BaselineKind::Symlink,
                backup_path: None,
                sha256: Some(hash_os_string(target.as_os_str())),
                symlink_target: Some(target),
                mode: metadata_mode(&metadata),
                identity: Some(file_identity(surface, &metadata)?),
                security: Some(capture_security_metadata(surface, &metadata)?),
            },
        );
    }
    if !metadata.is_file() {
        bail!(
            "refusing to redirect non-file surface {}",
            surface.display()
        );
    }

    fs::create_dir_all(backups_dir)
        .with_context(|| format!("create backup directory {}", backups_dir.display()))?;
    let digest = sha256_file(surface)?;
    let backup_path = backups_dir.join(format!("{}.baseline", Uuid::new_v4()));
    copy_file_atomic(surface, &backup_path, metadata_mode(&metadata), None)?;
    let copied_digest = sha256_file(&backup_path)?;
    if copied_digest != digest {
        let _ = fs::remove_file(&backup_path);
        bail!("baseline changed while backing up {}", surface.display());
    }

    finish_captured_baseline(
        surface,
        backups_dir,
        BaselineBackup {
            kind: BaselineKind::File,
            backup_path: Some(backup_path),
            symlink_target: None,
            mode: metadata_mode(&metadata),
            sha256: Some(digest),
            identity: Some(file_identity(surface, &metadata)?),
            security: Some(capture_security_metadata(surface, &metadata)?),
        },
    )
}

fn finish_captured_baseline(
    surface: &Path,
    backups_dir: &Path,
    baseline: BaselineBackup,
) -> Result<BaselineBackup> {
    if baseline_is_untouched(surface, &baseline)? {
        return Ok(baseline);
    }
    cleanup_unused_baseline(&baseline, backups_dir)?;
    bail!(
        "launcher {} changed while its baseline was being captured",
        surface.display()
    )
}

fn cleanup_unused_baseline(baseline: &BaselineBackup, backups_dir: &Path) -> Result<()> {
    let Some(path) = baseline.backup_path.as_deref() else {
        return Ok(());
    };
    if path.parent() != Some(backups_dir) || path.extension() != Some(OsStr::new("baseline")) {
        bail!(
            "refusing to remove baseline outside patcher backup storage: {}",
            path.display()
        );
    }
    match fs::remove_file(path) {
        Ok(()) => sync_parent(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove baseline {}", path.display())),
    }
}

/// Return whether the current surface resolves to or contains the requested
/// manager shim. This is useful before an ownership record has been loaded.
pub fn matches_shim(surface: &Path, manager: &Path) -> Result<bool> {
    let surface = absolute_path(surface)?;
    let manager = absolute_path(manager)?;
    if fs::symlink_metadata(&surface).is_err() {
        return Ok(false);
    }

    #[cfg(unix)]
    let matches = {
        let metadata = fs::symlink_metadata(&surface)?;
        if !metadata.file_type().is_symlink() {
            false
        } else {
            let target = fs::read_link(&surface)?;
            resolve_link_target(&surface, &target) == manager
        }
    };

    #[cfg(windows)]
    let matches = {
        let expected = windows_shim_bytes(&surface, &manager)?;
        fs::read(&surface).ok().as_deref() == Some(expected.as_slice())
    };

    #[cfg(not(any(unix, windows)))]
    let matches = false;

    Ok(matches)
}

pub fn matches_recorded_shim(record: &SurfaceRecord) -> Result<bool> {
    if record.schema != SURFACE_RECORD_SCHEMA {
        return Ok(false);
    }
    installed_matches_at(&record.surface, &record.installed)
}

fn installed_matches_at(surface: &Path, installed: &InstalledShim) -> Result<bool> {
    let metadata = match fs::symlink_metadata(surface) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if let Some(expected) = installed.identity.as_ref()
        && &file_identity(surface, &metadata)? != expected
    {
        return Ok(false);
    }
    if !metadata_mode_matches(&metadata, installed.mode) {
        return Ok(false);
    }
    if let Some(expected) = installed.security.as_ref()
        && capture_security_metadata(surface, &metadata)? != *expected
    {
        return Ok(false);
    }

    match installed.form {
        ShimForm::PosixSymlink => {
            if !metadata.file_type().is_symlink() {
                return Ok(false);
            }
            Ok(fs::read_link(surface).ok() == installed.symlink_target)
        }
        ShimForm::WindowsNative | ShimForm::WindowsCmd | ShimForm::WindowsPowerShell => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Ok(false);
            }
            let Some(expected) = installed.sha256.as_deref() else {
                return Ok(false);
            };
            Ok(sha256_file(surface)? == expected)
        }
    }
}

pub fn baseline_matches_current(surface: &Path, baseline: &BaselineBackup) -> Result<bool> {
    let metadata = match fs::symlink_metadata(surface) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };

    match baseline.kind {
        BaselineKind::Missing => Ok(metadata.is_none()),
        BaselineKind::Symlink => {
            let Some(metadata) = metadata else {
                return Ok(false);
            };
            if !metadata.file_type().is_symlink() {
                return Ok(false);
            }
            if fs::read_link(surface).ok() != baseline.symlink_target {
                return Ok(false);
            }
            if !metadata_mode_matches(&metadata, baseline.mode) {
                return Ok(false);
            }
            security_metadata_matches(surface, &metadata, baseline.security.as_ref())
        }
        BaselineKind::File => {
            let Some(metadata) = metadata else {
                return Ok(false);
            };
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Ok(false);
            }
            let Some(expected) = baseline.sha256.as_deref() else {
                return Ok(false);
            };
            if sha256_file(surface)? != expected {
                return Ok(false);
            }
            if !metadata_mode_matches(&metadata, baseline.mode) {
                return Ok(false);
            }
            security_metadata_matches(surface, &metadata, baseline.security.as_ref())
        }
    }
}

/// Journal recovery needs to distinguish "the mutation never happened" from
/// an atomic replacement that happened to produce identical bytes, mode, or a
/// symlink to the same target. The captured filesystem identity supplies that
/// distinction; ordinary uninstall CAS intentionally remains content-based so
/// a baseline restored by a prior recovery is still recognizable.
fn baseline_is_untouched(surface: &Path, baseline: &BaselineBackup) -> Result<bool> {
    if !baseline_matches_current(surface, baseline)? {
        return Ok(false);
    }
    if baseline.kind == BaselineKind::Missing {
        return Ok(true);
    }
    let Some(expected) = baseline.identity.as_ref() else {
        return Ok(false);
    };
    let metadata = fs::symlink_metadata(surface)?;
    Ok(file_identity(surface, &metadata)? == *expected)
}

fn capture_surface_snapshot(surface: &Path) -> Result<SurfaceSnapshot> {
    let metadata = match fs::symlink_metadata(surface) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SurfaceSnapshot {
                kind: BaselineKind::Missing,
                symlink_target: None,
                mode: None,
                sha256: None,
                identity: None,
                security: None,
            });
        }
        Err(error) => return Err(error).with_context(|| format!("inspect {}", surface.display())),
    };
    let identity = file_identity(surface, &metadata)?;
    let (kind, symlink_target, sha256) = if metadata.file_type().is_symlink() {
        let target = fs::read_link(surface)
            .with_context(|| format!("read observed symlink {}", surface.display()))?;
        (
            BaselineKind::Symlink,
            Some(target.clone()),
            Some(hash_os_string(target.as_os_str())),
        )
    } else if metadata.is_file() {
        (BaselineKind::File, None, Some(sha256_file(surface)?))
    } else {
        bail!(
            "cannot snapshot non-file repair surface {}",
            surface.display()
        );
    };
    let after = fs::symlink_metadata(surface)
        .with_context(|| format!("reinspect repair surface {}", surface.display()))?;
    if file_identity(surface, &after)? != identity
        || metadata.file_type().is_symlink() != after.file_type().is_symlink()
        || metadata.is_file() != after.is_file()
    {
        bail!(
            "repair surface changed while it was being inspected: {}",
            surface.display()
        );
    }
    Ok(SurfaceSnapshot {
        kind,
        symlink_target,
        mode: metadata_mode(&after),
        sha256,
        identity: Some(identity),
        security: Some(capture_security_metadata(surface, &after)?),
    })
}

fn snapshot_matches_current(surface: &Path, snapshot: &SurfaceSnapshot) -> Result<bool> {
    let metadata = match fs::symlink_metadata(surface) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    match snapshot.kind {
        BaselineKind::Missing => Ok(metadata.is_none()),
        BaselineKind::Symlink => {
            let Some(metadata) = metadata else {
                return Ok(false);
            };
            if !metadata.file_type().is_symlink()
                || fs::read_link(surface).ok() != snapshot.symlink_target
                || !metadata_mode_matches(&metadata, snapshot.mode)
            {
                return Ok(false);
            }
            let Some(expected) = snapshot.identity.as_ref() else {
                return Ok(false);
            };
            Ok(file_identity(surface, &metadata)? == *expected
                && security_metadata_matches(surface, &metadata, snapshot.security.as_ref())?)
        }
        BaselineKind::File => {
            let Some(metadata) = metadata else {
                return Ok(false);
            };
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || !metadata_mode_matches(&metadata, snapshot.mode)
            {
                return Ok(false);
            }
            let (Some(expected_hash), Some(expected_identity)) =
                (snapshot.sha256.as_deref(), snapshot.identity.as_ref())
            else {
                return Ok(false);
            };
            Ok(sha256_file(surface)? == expected_hash
                && file_identity(surface, &metadata)? == *expected_identity
                && security_metadata_matches(surface, &metadata, snapshot.security.as_ref())?)
        }
    }
}

/// Repair a surface. Unexpected drift is never silently adopted: callers must
/// pass `adopt = true`, in which case the drifted owner file becomes the new
/// exact uninstall baseline before it is overwritten.
pub fn repair_redirect(
    record: &mut SurfaceRecord,
    manager: &Path,
    backups_dir: &Path,
    adopt: bool,
) -> Result<RepairOutcome> {
    let outcome = repair_redirect_journaled(record, manager, backups_dir, adopt)?;
    if outcome != RepairOutcome::Unchanged {
        finalize_repair_redirect(record, backups_dir)?;
    }
    Ok(outcome)
}

/// Crash-recoverable launcher repair.
///
/// In particular, `adopt = true` captures and durably journals the replacement
/// uninstall baseline before installing the manager shim. The caller receives
/// the updated record through `record`, saves it durably, and then calls
/// [`finalize_repair_redirect`] or [`finalize_repair_journal`].
pub fn repair_redirect_journaled(
    record: &mut SurfaceRecord,
    manager: &Path,
    backups_dir: &Path,
    adopt: bool,
) -> Result<RepairOutcome> {
    validate_record(record)?;
    let manager = absolute_path(manager)?;
    let backups_dir = absolute_path(backups_dir)?;
    let path = repair_journal_path(&backups_dir, record.id);
    let journal = match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!(
                    "repair journal path is not a regular file: {}",
                    path.display()
                );
            }
            let journal = read_repair_journal(&path)?;
            validate_repair_journal(&journal, &path)?;
            if journal.original != *record && journal.updated.as_ref() != Some(record) {
                bail!(
                    "repair journal {} does not match the requested surface record",
                    path.display()
                );
            }
            if journal.manager != manager {
                bail!(
                    "repair journal {} targets a different manager executable",
                    path.display()
                );
            }
            journal
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let Some(journal) = prepare_repair_journal(record, &manager, &backups_dir, adopt)?
            else {
                return Ok(RepairOutcome::Unchanged);
            };
            write_repair_journal(&backups_dir, &journal)?;
            journal
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", path.display()));
        }
    };

    recover_repair_swap(&journal)?;
    let outcome = journal.outcome;
    let disposition = classify_repair_journal(&journal, &path)?;
    *record = complete_repair_journal(&backups_dir, journal, disposition)?;
    Ok(outcome)
}

/// Recover all interrupted repair-shims operations.
///
/// Prepared repairs are retried only while their exact observed precondition
/// remains current. A manager shim installed before a crash is inspected and
/// converted into an updated record without another replacement. Completed
/// records are returned even if later owner drift occurred, but that drift is
/// never overwritten by recovery.
pub fn recover_repair_journals(backups_dir: &Path) -> Result<Vec<SurfaceRecord>> {
    let backups_dir = absolute_path(backups_dir)?;
    let paths = repair_journal_paths(&backups_dir)?;
    let mut actions = Vec::with_capacity(paths.len());
    for path in paths {
        let journal = read_repair_journal(&path)?;
        validate_repair_journal(&journal, &path)?;
        recover_repair_swap(&journal)?;
        let disposition = classify_repair_journal(&journal, &path)?;
        actions.push((journal, disposition));
    }

    let mut recovered = Vec::with_capacity(actions.len());
    for (journal, disposition) in actions {
        recovered.push(complete_repair_journal(&backups_dir, journal, disposition)?);
    }
    Ok(recovered)
}

/// Delete a completed repair journal after manager state durably contains its
/// updated surface record. Missing journals are already finalized; prepared
/// journals are never discarded.
pub fn finalize_repair_journal(id: Uuid, backups_dir: &Path) -> Result<()> {
    let backups_dir = absolute_path(backups_dir)?;
    let path = repair_journal_path(&backups_dir, id);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "repair journal path is not a regular file: {}",
            path.display()
        );
    }
    let journal = read_repair_journal(&path)?;
    validate_repair_journal(&journal, &path)?;
    if journal.id != id {
        bail!("repair journal id mismatch: {}", path.display());
    }
    if journal.updated.is_none() || journal.completed_at.is_none() {
        bail!(
            "refusing to finalize incomplete repair journal {}",
            path.display()
        );
    }
    if journal.original.baseline != journal.replacement_baseline {
        cleanup_unused_baseline(&journal.original.baseline, &backups_dir)?;
    }
    fs::remove_file(&path)
        .with_context(|| format!("remove completed repair journal {}", path.display()))?;
    sync_parent(&path)
}

/// Record-oriented convenience wrapper for [`finalize_repair_journal`].
pub fn finalize_repair_redirect(record: &SurfaceRecord, backups_dir: &Path) -> Result<()> {
    validate_record(record)?;
    let backups_dir = absolute_path(backups_dir)?;
    let path = repair_journal_path(&backups_dir, record.id);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!(
                    "repair journal path is not a regular file: {}",
                    path.display()
                );
            }
            let journal = read_repair_journal(&path)?;
            validate_repair_journal(&journal, &path)?;
            if journal.updated.as_ref() != Some(record) {
                bail!(
                    "repair journal {} does not contain the supplied updated surface record",
                    path.display()
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    }
    finalize_repair_journal(record.id, &backups_dir)
}

fn prepare_repair_journal(
    record: &SurfaceRecord,
    manager: &Path,
    backups_dir: &Path,
    adopt: bool,
) -> Result<Option<RepairJournal>> {
    if matches_recorded_shim(record)? {
        return Ok(None);
    }

    let (outcome, replacement_baseline) = if matches_shim(&record.surface, manager)? {
        (RepairOutcome::ReinstalledOwned, record.baseline.clone())
    } else if baseline_matches_current(&record.surface, &record.baseline)? {
        (RepairOutcome::ReinstalledBaseline, record.baseline.clone())
    } else if adopt {
        (
            RepairOutcome::AdoptedAndReinstalled,
            capture_baseline(&record.surface, backups_dir)?,
        )
    } else {
        bail!(
            "surface {} changed outside codex-patcher; rerun with explicit adoption to overwrite it",
            record.surface.display()
        );
    };
    let observed = capture_surface_snapshot(&record.surface)?;
    let journal = RepairJournal {
        schema: REPAIR_JOURNAL_SCHEMA,
        id: record.id,
        original: record.clone(),
        manager: manager.to_path_buf(),
        outcome,
        replacement_baseline,
        observed,
        started_at: Utc::now(),
        updated: None,
        completed_at: None,
    };
    if !repair_precondition_matches(&journal)? {
        bail!(
            "surface {} changed while preparing its repair",
            record.surface.display()
        );
    }
    Ok(Some(journal))
}

fn repair_precondition_matches(journal: &RepairJournal) -> Result<bool> {
    if !snapshot_matches_current(&journal.original.surface, &journal.observed)? {
        return Ok(false);
    }
    match journal.outcome {
        RepairOutcome::Unchanged => Ok(false),
        RepairOutcome::ReinstalledOwned => {
            matches_shim(&journal.original.surface, &journal.manager)
        }
        RepairOutcome::ReinstalledBaseline => {
            baseline_matches_current(&journal.original.surface, &journal.original.baseline)
        }
        RepairOutcome::AdoptedAndReinstalled => {
            baseline_is_untouched(&journal.original.surface, &journal.replacement_baseline)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepairDisposition {
    RetryObserved,
    ManagerPresent,
    CompletedRecordOnly,
}

fn classify_repair_journal(journal: &RepairJournal, path: &Path) -> Result<RepairDisposition> {
    if let Some(updated) = journal.updated.as_ref()
        && matches_recorded_shim(updated)?
    {
        return Ok(RepairDisposition::ManagerPresent);
    }
    if matches_shim(&journal.original.surface, &journal.manager)? {
        return Ok(RepairDisposition::ManagerPresent);
    }
    if repair_precondition_matches(journal)? {
        return Ok(RepairDisposition::RetryObserved);
    }
    if journal.updated.is_some() {
        // The completed record is durable. Preserve later owner drift and let
        // normal status/repair handling report it after state recovery.
        return Ok(RepairDisposition::CompletedRecordOnly);
    }
    bail!(
        "repair journal {} cannot be recovered because {} matches neither its observed precondition nor the manager shim; refusing to overwrite drift",
        path.display(),
        journal.original.surface.display()
    )
}

fn complete_repair_journal(
    backups_dir: &Path,
    mut journal: RepairJournal,
    disposition: RepairDisposition,
) -> Result<SurfaceRecord> {
    let path = repair_journal_path(backups_dir, journal.id);
    match disposition {
        RepairDisposition::RetryObserved => {
            cas_install_shim(
                &journal.original.surface,
                &journal.manager,
                journal.id,
                journal.observed.kind,
                |displaced| snapshot_matches_current(displaced, &journal.observed),
            )?;
        }
        RepairDisposition::ManagerPresent => {
            let exact_completed = journal
                .updated
                .as_ref()
                .map(matches_recorded_shim)
                .transpose()?
                .unwrap_or(false);
            if !exact_completed && !matches_shim(&journal.original.surface, &journal.manager)? {
                bail!(
                    "{} changed while completing repair journal {}; leaving the journal intact",
                    journal.original.surface.display(),
                    path.display()
                );
            }
        }
        RepairDisposition::CompletedRecordOnly => {
            return journal
                .updated
                .context("completed repair journal is missing its updated record");
        }
    }

    let installed = inspect_installed_shim(&journal.original.surface, &journal.manager)?;
    let installed_at = journal
        .updated
        .as_ref()
        .filter(|updated| updated.installed == installed)
        .map(|updated| updated.installed_at)
        .unwrap_or_else(Utc::now);
    let mut updated = journal.original.clone();
    updated.baseline = journal.replacement_baseline.clone();
    updated.installed = installed;
    updated.installed_at = installed_at;
    if !matches_recorded_shim(&updated)? {
        bail!(
            "installed repair shim failed verification for {}",
            updated.surface.display()
        );
    }
    journal.updated = Some(updated.clone());
    if journal.completed_at.is_none() {
        journal.completed_at = Some(Utc::now());
    }
    write_repair_journal(backups_dir, &journal)?;
    Ok(updated)
}

/// Compare-and-swap uninstall: restoration only proceeds while the surface is
/// exactly the shim described by the record.
pub fn uninstall_redirect(record: &SurfaceRecord) -> Result<()> {
    validate_record(record)?;
    if !matches_recorded_shim(record)? {
        bail!(
            "refusing to restore {} because the installed shim no longer matches its ownership record",
            record.surface.display()
        );
    }
    cas_restore_baseline(record, Uuid::new_v4())
}

/// Crash-recoverable compare-and-swap uninstall.
///
/// The restore intent is durably journaled before the launcher is touched.
/// After the exact baseline is restored and verified, the completed journal is
/// retained until the caller durably removes `record` from manager state and
/// calls [`finalize_restore_journal`] or [`finalize_uninstall_redirect`].
pub fn uninstall_redirect_journaled(record: &SurfaceRecord, backups_dir: &Path) -> Result<()> {
    validate_record(record)?;
    let backups_dir = absolute_path(backups_dir)?;
    let path = restore_journal_path(&backups_dir, record.id);
    let journal = match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!(
                    "restore journal path is not a regular file: {}",
                    path.display()
                );
            }
            let journal = read_restore_journal(&path)?;
            validate_restore_journal(&journal, &path)?;
            if journal.record != *record {
                bail!(
                    "restore journal {} does not match the requested surface record",
                    path.display()
                );
            }
            journal
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !matches_recorded_shim(record)? {
                bail!(
                    "refusing to restore {} because the installed shim no longer matches its ownership record",
                    record.surface.display()
                );
            }
            let journal = RestoreJournal {
                schema: RESTORE_JOURNAL_SCHEMA,
                id: record.id,
                record: record.clone(),
                started_at: Utc::now(),
                completed_at: None,
            };
            // This intent is durable before the compare-and-swap restoration.
            write_restore_journal(&backups_dir, &journal)?;
            journal
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", path.display()));
        }
    };

    recover_restore_swap(&journal)?;
    let disposition = classify_restore_journal(&journal, &path)?;
    complete_restore_journal(&backups_dir, journal, disposition)?;
    Ok(())
}

/// Resume all interrupted baseline restorations.
///
/// Exact installed shims are restored, already-restored exact baselines are
/// marked complete, and any other contents are treated as external drift. The
/// returned IDs identify records that callers may now durably remove from
/// manager state before finalizing their journals.
pub fn recover_restore_journals(backups_dir: &Path) -> Result<Vec<Uuid>> {
    let backups_dir = absolute_path(backups_dir)?;
    let paths = restore_journal_paths(&backups_dir)?;
    let mut actions = Vec::with_capacity(paths.len());

    // Preflight every journal before restoring any launcher. This ensures a
    // pre-existing drift failure cannot partially consume the recovery set.
    for path in paths {
        let journal = read_restore_journal(&path)?;
        validate_restore_journal(&journal, &path)?;
        recover_restore_swap(&journal)?;
        let disposition = classify_restore_journal(&journal, &path)?;
        actions.push((journal, disposition));
    }

    let mut completed = Vec::with_capacity(actions.len());
    for (journal, disposition) in actions {
        completed.push(complete_restore_journal(
            &backups_dir,
            journal,
            disposition,
        )?);
    }
    Ok(completed)
}

/// Delete a completed restore journal after its surface record has been
/// durably removed from manager state. Missing journals are treated as already
/// finalized; prepared journals are never discarded by this operation.
pub fn finalize_restore_journal(id: Uuid, backups_dir: &Path) -> Result<()> {
    let backups_dir = absolute_path(backups_dir)?;
    let path = restore_journal_path(&backups_dir, id);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "restore journal path is not a regular file: {}",
            path.display()
        );
    }
    let journal = read_restore_journal(&path)?;
    validate_restore_journal(&journal, &path)?;
    if journal.id != id {
        bail!("restore journal id mismatch: {}", path.display());
    }
    if journal.completed_at.is_none() {
        bail!(
            "refusing to finalize incomplete restore journal {}",
            path.display()
        );
    }
    cleanup_unused_baseline(&journal.record.baseline, &backups_dir)?;
    fs::remove_file(&path)
        .with_context(|| format!("remove completed restore journal {}", path.display()))?;
    sync_parent(&path)
}

/// Record-oriented convenience wrapper for [`finalize_restore_journal`].
pub fn finalize_uninstall_redirect(record: &SurfaceRecord, backups_dir: &Path) -> Result<()> {
    validate_record(record)?;
    let backups_dir = absolute_path(backups_dir)?;
    let path = restore_journal_path(&backups_dir, record.id);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!(
                    "restore journal path is not a regular file: {}",
                    path.display()
                );
            }
            let journal = read_restore_journal(&path)?;
            validate_restore_journal(&journal, &path)?;
            if journal.record != *record {
                bail!(
                    "restore journal {} does not match the requested surface record",
                    path.display()
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    }
    finalize_restore_journal(record.id, &backups_dir)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreDisposition {
    RetryInstalledShim,
    BaselinePresent,
}

fn classify_restore_journal(journal: &RestoreJournal, path: &Path) -> Result<RestoreDisposition> {
    if matches_recorded_shim(&journal.record)? {
        return Ok(RestoreDisposition::RetryInstalledShim);
    }
    if baseline_matches_current(&journal.record.surface, &journal.record.baseline)? {
        return Ok(RestoreDisposition::BaselinePresent);
    }
    bail!(
        "restore journal {} cannot be recovered because {} matches neither its recorded shim nor its baseline; refusing to overwrite drift",
        path.display(),
        journal.record.surface.display()
    )
}

fn complete_restore_journal(
    backups_dir: &Path,
    mut journal: RestoreJournal,
    disposition: RestoreDisposition,
) -> Result<Uuid> {
    let path = restore_journal_path(backups_dir, journal.id);
    match disposition {
        RestoreDisposition::RetryInstalledShim => {
            cas_restore_baseline(&journal.record, journal.id)?;
        }
        RestoreDisposition::BaselinePresent => {
            if !baseline_matches_current(&journal.record.surface, &journal.record.baseline)? {
                bail!(
                    "{} changed while completing restore journal {}; leaving the journal intact",
                    journal.record.surface.display(),
                    path.display()
                );
            }
        }
    }
    if !baseline_matches_current(&journal.record.surface, &journal.record.baseline)? {
        bail!(
            "restored baseline failed verification for {}",
            journal.record.surface.display()
        );
    }
    if journal.completed_at.is_none() {
        journal.completed_at = Some(Utc::now());
    }
    write_restore_journal(backups_dir, &journal)?;
    Ok(journal.id)
}

pub fn restore_surface(record: &SurfaceRecord) -> Result<()> {
    uninstall_redirect(record)
}

fn validate_record(record: &SurfaceRecord) -> Result<()> {
    if record.schema != SURFACE_RECORD_SCHEMA {
        bail!(
            "unsupported surface record schema {} (expected {})",
            record.schema,
            SURFACE_RECORD_SCHEMA
        );
    }
    Ok(())
}

fn validate_redirect_paths(surface: &Path, manager: &Path) -> Result<()> {
    if surface == manager {
        bail!("surface and stable manager are the same path");
    }
    if !manager.is_file() {
        bail!("stable manager does not exist: {}", manager.display());
    }
    let parent = surface
        .parent()
        .context("redirect surface has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create redirect parent {}", parent.display()))?;
    if let Ok(metadata) = fs::symlink_metadata(surface)
        && !metadata.is_file()
        && !metadata.file_type().is_symlink()
    {
        bail!("refusing to replace non-file surface {}", surface.display());
    }
    Ok(())
}

fn cas_paths(surface: &Path, id: Uuid, operation: &str) -> Result<(PathBuf, PathBuf)> {
    let parent = surface
        .parent()
        .context("launcher has no parent directory")?;
    let candidate = parent.join(format!(".codex-patcher-{id}.{operation}-candidate"));
    #[cfg(windows)]
    let displaced = parent.join(format!(".codex-patcher-{id}.{operation}-displaced"));
    #[cfg(not(windows))]
    let displaced = candidate.clone();
    Ok((candidate, displaced))
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn remove_cas_path(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_parent(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove CAS artifact {}", path.display())),
    }
}

fn ensure_cas_paths_clear(candidate: &Path, displaced: &Path) -> Result<()> {
    if path_exists(candidate)? || candidate != displaced && path_exists(displaced)? {
        bail!(
            "launcher CAS artifact already exists: {}",
            if path_exists(candidate)? {
                candidate.display()
            } else {
                displaced.display()
            }
        );
    }
    Ok(())
}

fn create_shim_candidate(_surface: &Path, manager: &Path, candidate: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(manager, candidate).with_context(|| {
            format!(
                "create launcher CAS candidate {} -> {}",
                candidate.display(),
                manager.display()
            )
        })?;
        sync_parent(candidate)
    }
    #[cfg(windows)]
    {
        let bytes = windows_shim_bytes(_surface, manager)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(candidate)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        sync_parent(candidate)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (surface, manager, candidate);
        bail!("launcher CAS is unsupported on this platform")
    }
}

fn create_baseline_candidate(baseline: &BaselineBackup, candidate: &Path) -> Result<()> {
    match baseline.kind {
        BaselineKind::Missing => bail!("a missing baseline has no restoration candidate"),
        BaselineKind::File => {
            let source = baseline
                .backup_path
                .as_deref()
                .context("file baseline is missing its backup path")?;
            let expected = baseline
                .sha256
                .as_deref()
                .context("file baseline is missing its hash")?;
            if sha256_file(source)? != expected {
                bail!(
                    "baseline backup failed hash verification: {}",
                    source.display()
                );
            }
            let mut input = File::open(source)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(candidate)?;
            std::io::copy(&mut input, &mut output)?;
            restore_security_metadata(candidate, baseline.mode, baseline.security.as_ref(), false)?;
            output.sync_all()?;
        }
        BaselineKind::Symlink => {
            let target = baseline
                .symlink_target
                .as_deref()
                .context("symlink baseline is missing its target")?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, candidate)?;
            #[cfg(windows)]
            std::os::windows::fs::symlink_file(target, candidate)?;
            restore_security_metadata(candidate, baseline.mode, baseline.security.as_ref(), true)?;
        }
    }
    sync_parent(candidate)
}

fn candidate_matches_shim(_surface: &Path, candidate: &Path, manager: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        let metadata = match fs::symlink_metadata(candidate) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        Ok(metadata.file_type().is_symlink()
            && fs::read_link(candidate).ok().as_deref() == Some(manager))
    }
    #[cfg(windows)]
    {
        Ok(fs::read(candidate).ok().as_deref()
            == Some(windows_shim_bytes(_surface, manager)?.as_slice()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (_surface, candidate, manager);
        Ok(false)
    }
}

fn cas_install_shim<F>(
    surface: &Path,
    manager: &Path,
    id: Uuid,
    expected_kind: BaselineKind,
    expected: F,
) -> Result<()>
where
    F: FnOnce(&Path) -> Result<bool>,
{
    validate_redirect_paths(surface, manager)?;
    let (candidate, displaced) = cas_paths(surface, id, "shim")?;
    ensure_cas_paths_clear(&candidate, &displaced)?;
    create_shim_candidate(surface, manager, &candidate)?;
    let desired = capture_surface_snapshot(&candidate)?;

    if expected_kind == BaselineKind::Missing {
        if let Err(error) = platform_move_noreplace(&candidate, surface) {
            let _ = remove_cas_path(&candidate);
            return Err(error).with_context(|| {
                format!(
                    "launcher {} appeared during atomic takeover; refusing to overwrite it",
                    surface.display()
                )
            });
        }
        if snapshot_matches_current(surface, &desired)? {
            return sync_parent(surface);
        }
        bail!(
            "launcher {} changed immediately after atomic takeover; preserving the newer drift",
            surface.display()
        );
    }

    if let Err(error) = platform_replace_with_backup(&candidate, surface, &displaced) {
        let _ = remove_cas_path(&candidate);
        return Err(error);
    }
    let desired_intact = snapshot_matches_current(surface, &desired)?;
    match (expected(&displaced), desired_intact) {
        (Ok(true), true) => remove_cas_path(&displaced),
        (_, false) => bail!(
            "launcher {} changed immediately after atomic takeover; preserving both the newer drift and displaced file {}",
            surface.display(),
            displaced.display()
        ),
        validation => {
            if let Err(rollback) = platform_replace_with_backup(&displaced, surface, &candidate) {
                bail!(
                    "launcher {} changed during atomic takeover and rollback failed: {rollback:#}",
                    surface.display()
                );
            }
            remove_cas_path(&candidate)?;
            match validation.0 {
                Ok(false) => bail!(
                    "launcher {} changed during atomic takeover; restored the displaced file without overwriting drift",
                    surface.display()
                ),
                Err(error) => Err(error).with_context(|| {
                    format!(
                        "validate displaced launcher {} during atomic takeover",
                        surface.display()
                    )
                }),
                Ok(true) => unreachable!(),
            }
        }
    }
}

fn cas_restore_baseline(record: &SurfaceRecord, id: Uuid) -> Result<()> {
    let surface = &record.surface;
    let (candidate, displaced) = cas_paths(surface, id, "restore")?;
    ensure_cas_paths_clear(&candidate, &displaced)?;

    if record.baseline.kind == BaselineKind::Missing {
        platform_move_noreplace(surface, &displaced)
            .with_context(|| format!("atomically remove recorded shim {}", surface.display()))?;
        let destination_missing = !path_exists(surface)?;
        match (
            installed_matches_at(&displaced, &record.installed),
            destination_missing,
        ) {
            (Ok(true), true) => return remove_cas_path(&displaced),
            (_, false) => bail!(
                "{} changed immediately after atomic uninstall; preserving both the newer drift and displaced shim {}",
                surface.display(),
                displaced.display()
            ),
            validation => {
                if let Err(rollback) = platform_move_noreplace(&displaced, surface) {
                    bail!(
                        "{} changed during atomic uninstall and rollback failed: {rollback:#}",
                        surface.display()
                    );
                }
                return match validation.0 {
                    Ok(false) => bail!(
                        "{} changed during atomic uninstall; restored the displaced file without clobbering drift",
                        surface.display()
                    ),
                    Err(error) => Err(error).context("validate displaced installed shim"),
                    Ok(true) => unreachable!(),
                };
            }
        }
    }

    create_baseline_candidate(&record.baseline, &candidate)?;
    let desired = capture_surface_snapshot(&candidate)?;
    if let Err(error) = platform_replace_with_backup(&candidate, surface, &displaced) {
        let _ = remove_cas_path(&candidate);
        return Err(error);
    }
    let desired_intact = snapshot_matches_current(surface, &desired)?;
    match (
        installed_matches_at(&displaced, &record.installed),
        desired_intact,
    ) {
        (Ok(true), true) => remove_cas_path(&displaced),
        (_, false) => bail!(
            "{} changed immediately after atomic uninstall; preserving both the newer drift and displaced shim {}",
            surface.display(),
            displaced.display()
        ),
        validation => {
            if let Err(rollback) = platform_replace_with_backup(&displaced, surface, &candidate) {
                bail!(
                    "{} changed during atomic uninstall and rollback failed: {rollback:#}",
                    surface.display()
                );
            }
            remove_cas_path(&candidate)?;
            match validation.0 {
                Ok(false) => bail!(
                    "{} changed during atomic uninstall; restored the displaced file without clobbering drift",
                    surface.display()
                ),
                Err(error) => Err(error).context("validate displaced installed shim"),
                Ok(true) => unreachable!(),
            }
        }
    }
}

fn recover_takeover_swap(journal: &TakeoverJournal) -> Result<()> {
    recover_shim_swap(
        &journal.surface,
        &journal.manager,
        journal.id,
        |displaced| baseline_is_untouched(displaced, &journal.baseline),
    )
}

fn recover_repair_swap(journal: &RepairJournal) -> Result<()> {
    recover_shim_swap(
        &journal.original.surface,
        &journal.manager,
        journal.id,
        |displaced| snapshot_matches_current(displaced, &journal.observed),
    )
}

fn recover_shim_swap<F>(surface: &Path, manager: &Path, id: Uuid, expected: F) -> Result<()>
where
    F: Fn(&Path) -> Result<bool>,
{
    let (candidate, displaced) = cas_paths(surface, id, "shim")?;
    let candidate_exists = path_exists(&candidate)?;
    let displaced_exists = path_exists(&displaced)?;
    if !candidate_exists && !displaced_exists {
        return Ok(());
    }

    if matches_shim(surface, manager)? && displaced_exists {
        if expected(&displaced)? {
            return remove_cas_path(&displaced);
        }
        bail!(
            "launcher {} drifted during its interrupted atomic takeover; preserving displaced file {} because the current manager-shaped path cannot be proven to be our candidate",
            surface.display(),
            displaced.display()
        );
    }

    if candidate_exists && candidate_matches_shim(surface, &candidate, manager)? {
        remove_cas_path(&candidate)?;
        return Ok(());
    }
    bail!(
        "unrecognized launcher CAS artifact for {}; refusing to modify it",
        surface.display()
    )
}

fn recover_restore_swap(journal: &RestoreJournal) -> Result<()> {
    let record = &journal.record;
    let surface = &record.surface;
    let (candidate, displaced) = cas_paths(surface, journal.id, "restore")?;
    let candidate_exists = path_exists(&candidate)?;
    let displaced_exists = path_exists(&displaced)?;
    if !candidate_exists && !displaced_exists {
        return Ok(());
    }

    if record.baseline.kind == BaselineKind::Missing {
        if !path_exists(surface)? && displaced_exists {
            if installed_matches_at(&displaced, &record.installed)? {
                return remove_cas_path(&displaced);
            }
            platform_move_noreplace(&displaced, surface)
                .context("roll back interrupted missing-baseline restoration")?;
            bail!(
                "launcher {} drifted during interrupted uninstall; the drift was restored",
                surface.display()
            );
        }
        bail!(
            "unrecognized missing-baseline CAS artifact for {}; refusing to modify it",
            surface.display()
        );
    }

    if displaced_exists && baseline_matches_current(surface, &record.baseline)? {
        if installed_matches_at(&displaced, &record.installed)? {
            return remove_cas_path(&displaced);
        }
        bail!(
            "launcher {} drifted during interrupted uninstall; preserving displaced file {} because the current baseline-shaped path cannot be proven to be our candidate",
            surface.display(),
            displaced.display()
        );
    }

    if candidate_exists && baseline_matches_current(&candidate, &record.baseline)? {
        remove_cas_path(&candidate)?;
        return Ok(());
    }
    bail!(
        "unrecognized baseline CAS artifact for {}; refusing to modify it",
        surface.display()
    )
}

fn inspect_installed_shim(surface: &Path, manager: &Path) -> Result<InstalledShim> {
    let metadata = fs::symlink_metadata(surface)
        .with_context(|| format!("inspect installed shim {}", surface.display()))?;
    #[cfg(unix)]
    {
        if !metadata.file_type().is_symlink() {
            bail!(
                "installed POSIX shim is not a symlink: {}",
                surface.display()
            );
        }
        let target = fs::read_link(surface)?;
        Ok(InstalledShim {
            form: ShimForm::PosixSymlink,
            manager: manager.to_path_buf(),
            sha256: Some(hash_os_string(target.as_os_str())),
            symlink_target: Some(target),
            mode: metadata_mode(&metadata),
            identity: Some(file_identity(surface, &metadata)?),
            security: Some(capture_security_metadata(surface, &metadata)?),
        })
    }
    #[cfg(windows)]
    {
        Ok(InstalledShim {
            form: windows_shim_form(surface)?,
            manager: manager.to_path_buf(),
            sha256: Some(sha256_file(surface)?),
            symlink_target: None,
            mode: metadata_mode(&metadata),
            identity: Some(file_identity(surface, &metadata)?),
            security: Some(capture_security_metadata(surface, &metadata)?),
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (metadata, manager);
        bail!("shim inspection is unsupported on this platform")
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .context("resolve current directory")?
            .join(path))
    }
}

#[cfg(unix)]
fn resolve_link_target(link: &Path, target: &Path) -> PathBuf {
    if target.is_absolute() {
        target.to_path_buf()
    } else {
        link.parent().unwrap_or_else(|| Path::new(".")).join(target)
    }
}

fn copy_file_atomic(
    source: &Path,
    destination: &Path,
    mode: Option<u32>,
    security: Option<&SecurityMetadata>,
) -> Result<()> {
    let parent = destination
        .parent()
        .context("copy destination has no parent")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".codex-patcher.{}.tmp", Uuid::new_v4()));
    if let Err(error) = fs::copy(source, &temp) {
        let _ = fs::remove_file(&temp);
        return Err(error).with_context(|| {
            format!(
                "copy {} to temporary file {}",
                source.display(),
                temp.display()
            )
        });
    }
    restore_security_metadata(&temp, mode, security, false)?;
    File::open(&temp)?.sync_all()?;
    if let Err(error) = atomic_replace(&temp, destination) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    sync_parent(destination)
}

fn atomic_write(destination: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    let parent = destination
        .parent()
        .context("write destination has no parent")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".codex-patcher.{}.tmp", Uuid::new_v4()));
    let mut file = File::create(&temp)?;
    file.write_all(bytes)?;
    set_metadata_mode(&temp, mode)?;
    file.sync_all()?;
    drop(file);
    if let Err(error) = atomic_replace(&temp, destination) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    sync_parent(destination)
}

#[cfg(target_os = "linux")]
fn platform_rename(source: &Path, destination: &Path, flags: libc::c_uint) -> Result<()> {
    let source_c = unix_path(source)?;
    let destination_c = unix_path(destination)?;
    // SAFETY: both path strings are live and AT_FDCWD selects their parent
    // directories. renameat2 performs the requested operation atomically.
    if unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source_c.as_ptr(),
            libc::AT_FDCWD,
            destination_c.as_ptr(),
            flags,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).context("renameat2 launcher CAS");
    }
    sync_parent(destination)
}

#[cfg(target_os = "macos")]
fn platform_rename(source: &Path, destination: &Path, flags: libc::c_uint) -> Result<()> {
    let source_c = unix_path(source)?;
    let destination_c = unix_path(destination)?;
    // SAFETY: both C strings remain live for the atomic renamex_np call.
    if unsafe { libc::renamex_np(source_c.as_ptr(), destination_c.as_ptr(), flags) } != 0 {
        return Err(std::io::Error::last_os_error()).context("renamex_np launcher CAS");
    }
    sync_parent(destination)
}

#[cfg(windows)]
fn platform_move_noreplace(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // Without MOVEFILE_REPLACE_EXISTING this is an atomic no-clobber move.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("MoveFileExW no-replace launcher CAS");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn platform_move_noreplace(source: &Path, destination: &Path) -> Result<()> {
    platform_rename(source, destination, libc::RENAME_NOREPLACE)
}

#[cfg(target_os = "macos")]
fn platform_move_noreplace(source: &Path, destination: &Path) -> Result<()> {
    platform_rename(source, destination, libc::RENAME_EXCL)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn platform_move_noreplace(_source: &Path, _destination: &Path) -> Result<()> {
    bail!("atomic launcher compare-and-swap is supported only on Linux, macOS, and Windows")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn platform_replace_with_backup(
    replacement: &Path,
    destination: &Path,
    displaced: &Path,
) -> Result<()> {
    if replacement != displaced {
        bail!("POSIX launcher CAS requires its replacement and displaced paths to match");
    }
    #[cfg(target_os = "linux")]
    platform_rename(replacement, destination, libc::RENAME_EXCHANGE)
        .context("atomically exchange launcher")?;
    #[cfg(target_os = "macos")]
    platform_rename(replacement, destination, libc::RENAME_SWAP)
        .context("atomically exchange launcher")?;
    Ok(())
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn platform_replace_with_backup(
    _replacement: &Path,
    _destination: &Path,
    _displaced: &Path,
) -> Result<()> {
    bail!("atomic launcher compare-and-swap is supported only on Linux, macOS, and Windows")
}

#[cfg(windows)]
fn platform_replace_with_backup(
    replacement: &Path,
    destination: &Path,
    displaced: &Path,
) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};
    let replacement: Vec<u16> = replacement
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let displaced: Vec<u16> = displaced.as_os_str().encode_wide().chain(Some(0)).collect();
    // ReplaceFileW installs the replacement and creates the displaced backup
    // in one filesystem transaction, preserving the old file for validation.
    if unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            replacement.as_ptr(),
            displaced.as_ptr(),
            REPLACEFILE_WRITE_THROUGH,
            std::ptr::null(),
            std::ptr::null(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("ReplaceFileW launcher CAS");
    }
    Ok(())
}

#[cfg(unix)]
fn atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination).with_context(|| {
        format!(
            "atomically replace {} with {}",
            destination.display(),
            source.display()
        )
    })
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error()).context("MoveFileExW atomic replacement");
    }
    Ok(())
}

fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        if let Some(parent) = path.parent() {
            File::open(parent)?.sync_all()?;
        }
    }
    // `MoveFileExW(..., MOVEFILE_WRITE_THROUGH)` in `atomic_replace` provides
    // the Windows durability barrier for the rename. Windows does not expose a
    // portable directory-fsync operation analogous to POSIX.
    let _ = path;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn hash_os_string(value: &OsStr) -> String {
    let mut hasher = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(value.as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for word in value.encode_wide() {
            hasher.update(word.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    hasher.update(value.to_string_lossy().as_bytes());
    hex::encode(hasher.finalize())
}

fn metadata_mode(metadata: &Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(metadata.permissions().mode())
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        Some(metadata.file_attributes())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        None
    }
}

fn metadata_mode_matches(metadata: &Metadata, expected: Option<u32>) -> bool {
    expected.is_none_or(|expected| metadata_mode(metadata) == Some(expected))
}

fn set_metadata_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(windows)]
    if let Some(attributes) = mode {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe { SetFileAttributesW(path.as_ptr(), attributes) } == 0 {
            return Err(std::io::Error::last_os_error()).context("restore Windows file attributes");
        }
    }
    #[cfg(not(any(unix, windows)))]
    let _ = (path, mode);
    Ok(())
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    #[link_name = "lchmod"]
    fn macos_lchmod(path: *const libc::c_char, mode: libc::mode_t) -> libc::c_int;
}

fn capture_security_metadata(path: &Path, _metadata: &Metadata) -> Result<SecurityMetadata> {
    let mut security = SecurityMetadata::default();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        security.unix_uid = Some(_metadata.uid());
        security.unix_gid = Some(_metadata.gid());
    }
    #[cfg(target_os = "linux")]
    {
        security.extended_attributes =
            linux_extended_attributes(path, _metadata.file_type().is_symlink())?;
    }
    #[cfg(target_os = "macos")]
    {
        let no_follow = _metadata.file_type().is_symlink();
        security.extended_attributes = macos_extended_attributes(path, no_follow)?;
        security.macos_acl_text = macos_acl_text(path, no_follow)?;
    }
    #[cfg(windows)]
    {
        let (descriptor, protected) = windows_security_descriptor(path)?;
        security.windows_security_descriptor = Some(descriptor);
        security.windows_dacl_protected = Some(protected);
    }
    #[cfg(not(any(unix, windows)))]
    let _ = (path, _metadata);
    Ok(security)
}

fn security_metadata_matches(
    path: &Path,
    metadata: &Metadata,
    expected: Option<&SecurityMetadata>,
) -> Result<bool> {
    let Some(expected) = expected else {
        // Backwards compatibility for ownership records written before
        // security metadata became part of the baseline schema.
        return Ok(true);
    };
    Ok(capture_security_metadata(path, metadata)? == *expected)
}

fn restore_security_metadata(
    path: &Path,
    mode: Option<u32>,
    security: Option<&SecurityMetadata>,
    _no_follow: bool,
) -> Result<()> {
    if let Some(security) = security {
        #[cfg(unix)]
        restore_unix_owner(path, security)?;
        #[cfg(target_os = "linux")]
        restore_linux_extended_attributes(path, _no_follow, &security.extended_attributes)?;
        #[cfg(target_os = "macos")]
        {
            restore_macos_extended_attributes(path, _no_follow, &security.extended_attributes)?;
            restore_macos_acl(path, _no_follow, security.macos_acl_text.as_deref())?;
        }
        #[cfg(windows)]
        restore_windows_security_descriptor(path, security)?;
    }
    // Ownership changes can clear set-id bits, and ACL installation can alter
    // the POSIX mode mask. Restore the ordinary mode/attributes last without
    // ever following a symlink into its target.
    #[cfg(target_os = "macos")]
    if _no_follow && let Some(mode) = mode {
        let path_c = unix_path(path)?;
        // SAFETY: path_c is live and lchmod operates on the link itself.
        if unsafe { macos_lchmod(path_c.as_ptr(), mode as libc::mode_t) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("restore symlink mode for {}", path.display()));
        }
        return Ok(());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    if _no_follow {
        // Linux symlink permissions are invariant and chmod would follow the
        // target, so uid/gid/xattrs above are the complete restorable state.
        return Ok(());
    }
    set_metadata_mode(path, mode)
}

#[cfg(unix)]
fn unix_path(path: &Path) -> Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("path contains an interior NUL: {}", path.display()))
}

#[cfg(unix)]
fn restore_unix_owner(path: &Path, security: &SecurityMetadata) -> Result<()> {
    let (Some(uid), Some(gid)) = (security.unix_uid, security.unix_gid) else {
        bail!("recorded Unix security metadata is missing uid or gid");
    };
    let current = fs::symlink_metadata(path)
        .with_context(|| format!("inspect ownership for {}", path.display()))?;
    use std::os::unix::fs::MetadataExt;
    if current.uid() == uid && current.gid() == gid {
        return Ok(());
    }
    let path_c = unix_path(path)?;
    // SAFETY: `path_c` is a live NUL-terminated path; lchown intentionally
    // applies to the temporary symlink itself rather than its target.
    if unsafe { libc::lchown(path_c.as_ptr(), uid, gid) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("restore owner for {}", path.display()));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_extended_attributes(path: &Path, no_follow: bool) -> Result<Vec<ExtendedAttribute>> {
    let path_c = unix_path(path)?;
    // SAFETY: null output asks the kernel for the required list size.
    let size = unsafe {
        if no_follow {
            libc::llistxattr(path_c.as_ptr(), std::ptr::null_mut(), 0)
        } else {
            libc::listxattr(path_c.as_ptr(), std::ptr::null_mut(), 0)
        }
    };
    if size < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("list extended attributes for {}", path.display()));
    }
    let mut list = vec![0_u8; size as usize];
    if size > 0 {
        // SAFETY: `list` has the exact capacity reported above.
        let written = unsafe {
            if no_follow {
                libc::llistxattr(path_c.as_ptr(), list.as_mut_ptr().cast(), list.len())
            } else {
                libc::listxattr(path_c.as_ptr(), list.as_mut_ptr().cast(), list.len())
            }
        };
        if written < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("read extended attribute list for {}", path.display()));
        }
        list.truncate(written as usize);
    }
    read_named_extended_attributes(path, &path_c, no_follow, &list, linux_get_xattr)
}

#[cfg(target_os = "linux")]
fn linux_get_xattr(
    path: &std::ffi::CStr,
    name: &std::ffi::CStr,
    no_follow: bool,
    value: *mut libc::c_void,
    size: usize,
) -> isize {
    // SAFETY: callers provide live C strings and either a null buffer or a
    // buffer of `size` bytes.
    unsafe {
        if no_follow {
            libc::lgetxattr(path.as_ptr(), name.as_ptr(), value, size)
        } else {
            libc::getxattr(path.as_ptr(), name.as_ptr(), value, size)
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_named_extended_attributes(
    path: &Path,
    path_c: &std::ffi::CStr,
    no_follow: bool,
    list: &[u8],
    getter: fn(&std::ffi::CStr, &std::ffi::CStr, bool, *mut libc::c_void, usize) -> isize,
) -> Result<Vec<ExtendedAttribute>> {
    let mut attributes = Vec::new();
    for raw_name in list
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name = std::ffi::CString::new(raw_name)
            .context("filesystem returned an extended-attribute name containing NUL")?;
        let size = getter(path_c, &name, no_follow, std::ptr::null_mut(), 0);
        if size < 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!(
                    "measure extended attribute {:?} on {}",
                    raw_name,
                    path.display()
                )
            });
        }
        let mut value = vec![0_u8; size as usize];
        if size > 0 {
            let written = getter(
                path_c,
                &name,
                no_follow,
                value.as_mut_ptr().cast(),
                value.len(),
            );
            if written < 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!(
                        "read extended attribute {:?} on {}",
                        raw_name,
                        path.display()
                    )
                });
            }
            value.truncate(written as usize);
        }
        attributes.push(ExtendedAttribute {
            name: raw_name.to_vec(),
            value,
        });
    }
    attributes.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(attributes)
}

#[cfg(target_os = "linux")]
fn restore_linux_extended_attributes(
    path: &Path,
    no_follow: bool,
    expected: &[ExtendedAttribute],
) -> Result<()> {
    let current = linux_extended_attributes(path, no_follow)?;
    let path_c = unix_path(path)?;
    for attribute in &current {
        if expected.iter().any(|item| item.name == attribute.name) {
            continue;
        }
        let name = std::ffi::CString::new(attribute.name.as_slice())?;
        // SAFETY: paths and names are valid C strings.
        let result = unsafe {
            if no_follow {
                libc::lremovexattr(path_c.as_ptr(), name.as_ptr())
            } else {
                libc::removexattr(path_c.as_ptr(), name.as_ptr())
            }
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("remove extended attribute on {}", path.display()));
        }
    }
    for attribute in expected {
        let name = std::ffi::CString::new(attribute.name.as_slice())?;
        // SAFETY: the value pointer remains live for its recorded length.
        let result = unsafe {
            if no_follow {
                libc::lsetxattr(
                    path_c.as_ptr(),
                    name.as_ptr(),
                    attribute.value.as_ptr().cast(),
                    attribute.value.len(),
                    0,
                )
            } else {
                libc::setxattr(
                    path_c.as_ptr(),
                    name.as_ptr(),
                    attribute.value.as_ptr().cast(),
                    attribute.value.len(),
                    0,
                )
            }
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("restore extended attribute on {}", path.display()));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_extended_attributes(path: &Path, no_follow: bool) -> Result<Vec<ExtendedAttribute>> {
    let path_c = unix_path(path)?;
    let flags = if no_follow { libc::XATTR_NOFOLLOW } else { 0 };
    // SAFETY: null output asks the kernel for the required list size.
    let size = unsafe { libc::listxattr(path_c.as_ptr(), std::ptr::null_mut(), 0, flags) };
    if size < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("list extended attributes for {}", path.display()));
    }
    let mut list = vec![0_u8; size as usize];
    if size > 0 {
        // SAFETY: `list` has the capacity returned by listxattr.
        let written = unsafe {
            libc::listxattr(path_c.as_ptr(), list.as_mut_ptr().cast(), list.len(), flags)
        };
        if written < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("read extended attribute list for {}", path.display()));
        }
        list.truncate(written as usize);
    }
    read_named_extended_attributes(path, &path_c, no_follow, &list, macos_get_xattr)
}

#[cfg(target_os = "macos")]
fn macos_get_xattr(
    path: &std::ffi::CStr,
    name: &std::ffi::CStr,
    no_follow: bool,
    value: *mut libc::c_void,
    size: usize,
) -> isize {
    let flags = if no_follow { libc::XATTR_NOFOLLOW } else { 0 };
    // SAFETY: callers provide live C strings and an appropriately sized
    // output buffer. Position zero reads the complete non-resource-fork value.
    unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), value, size, 0, flags) }
}

#[cfg(target_os = "macos")]
fn restore_macos_extended_attributes(
    path: &Path,
    no_follow: bool,
    expected: &[ExtendedAttribute],
) -> Result<()> {
    let current = macos_extended_attributes(path, no_follow)?;
    let path_c = unix_path(path)?;
    let flags = if no_follow { libc::XATTR_NOFOLLOW } else { 0 };
    for attribute in &current {
        if expected.iter().any(|item| item.name == attribute.name) {
            continue;
        }
        let name = std::ffi::CString::new(attribute.name.as_slice())?;
        // SAFETY: paths and names are valid C strings.
        if unsafe { libc::removexattr(path_c.as_ptr(), name.as_ptr(), flags) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("remove extended attribute on {}", path.display()));
        }
    }
    for attribute in expected {
        let name = std::ffi::CString::new(attribute.name.as_slice())?;
        // SAFETY: the value pointer remains live for its recorded length.
        if unsafe {
            libc::setxattr(
                path_c.as_ptr(),
                name.as_ptr(),
                attribute.value.as_ptr().cast(),
                attribute.value.len(),
                0,
                flags,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("restore extended attribute on {}", path.display()));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
type MacAcl = *mut libc::c_void;
#[cfg(target_os = "macos")]
const MAC_ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn acl_get_file(path: *const libc::c_char, acl_type: libc::c_int) -> MacAcl;
    fn acl_get_link_np(path: *const libc::c_char, acl_type: libc::c_int) -> MacAcl;
    fn acl_set_file(path: *const libc::c_char, acl_type: libc::c_int, acl: MacAcl) -> libc::c_int;
    fn acl_set_link_np(
        path: *const libc::c_char,
        acl_type: libc::c_int,
        acl: MacAcl,
    ) -> libc::c_int;
    fn acl_delete_file_np(path: *const libc::c_char, acl_type: libc::c_int) -> libc::c_int;
    fn acl_delete_link_np(path: *const libc::c_char, acl_type: libc::c_int) -> libc::c_int;
    fn acl_to_text(acl: MacAcl, length: *mut libc::ssize_t) -> *mut libc::c_char;
    fn acl_from_text(text: *const libc::c_char) -> MacAcl;
    fn acl_free(object: *mut libc::c_void) -> libc::c_int;
}

#[cfg(target_os = "macos")]
fn macos_acl_text(path: &Path, no_follow: bool) -> Result<Option<Vec<u8>>> {
    let path_c = unix_path(path)?;
    // SAFETY: the path is a valid C string and both functions return an owned
    // ACL object which is released below.
    let acl = unsafe {
        if no_follow {
            acl_get_link_np(path_c.as_ptr(), MAC_ACL_TYPE_EXTENDED)
        } else {
            acl_get_file(path_c.as_ptr(), MAC_ACL_TYPE_EXTENDED)
        }
    };
    if acl.is_null() {
        let error = std::io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(code) if code == libc::ENOENT || code == libc::ENOATTR)
        {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("read ACL for {}", path.display()));
    }
    let mut length = 0;
    // SAFETY: `acl` remains live; returned text is released with acl_free.
    let text = unsafe { acl_to_text(acl, &mut length) };
    if text.is_null() {
        // SAFETY: `acl` was allocated by acl_get_*.
        unsafe { acl_free(acl) };
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("serialize ACL for {}", path.display()));
    }
    // SAFETY: acl_to_text reports the exact byte length of its live buffer.
    let bytes = unsafe { std::slice::from_raw_parts(text.cast::<u8>(), length as usize) }.to_vec();
    // SAFETY: both objects came from the ACL API and are no longer used.
    unsafe {
        acl_free(text.cast());
        acl_free(acl);
    }
    Ok(Some(bytes))
}

#[cfg(target_os = "macos")]
fn restore_macos_acl(path: &Path, no_follow: bool, text: Option<&[u8]>) -> Result<()> {
    let path_c = unix_path(path)?;
    let Some(text) = text else {
        // SAFETY: deleting a missing extended ACL is an idempotent restore.
        let result = unsafe {
            if no_follow {
                acl_delete_link_np(path_c.as_ptr(), MAC_ACL_TYPE_EXTENDED)
            } else {
                acl_delete_file_np(path_c.as_ptr(), MAC_ACL_TYPE_EXTENDED)
            }
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(code) if code == libc::ENOENT || code == libc::ENOATTR)
            {
                return Err(error).with_context(|| format!("remove ACL from {}", path.display()));
            }
        }
        return Ok(());
    };
    let text = std::ffi::CString::new(text).context("recorded macOS ACL text contains NUL")?;
    // SAFETY: `text` is live and NUL-terminated; the returned ACL is freed.
    let acl = unsafe { acl_from_text(text.as_ptr()) };
    if acl.is_null() {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("parse recorded ACL for {}", path.display()));
    }
    // SAFETY: all pointers remain live for the call.
    let result = unsafe {
        if no_follow {
            acl_set_link_np(path_c.as_ptr(), MAC_ACL_TYPE_EXTENDED, acl)
        } else {
            acl_set_file(path_c.as_ptr(), MAC_ACL_TYPE_EXTENDED, acl)
        }
    };
    // SAFETY: `acl` came from acl_from_text and is no longer used.
    unsafe { acl_free(acl) };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("restore ACL for {}", path.display()));
    }
    Ok(())
}

#[cfg(windows)]
fn windows_security_descriptor(path: &Path) -> Result<(Vec<u8>, bool)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetFileSecurityW,
        GetSecurityDescriptorControl, OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED,
    };
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let requested =
        OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut needed = 0_u32;
    // SAFETY: null descriptor with zero length is the documented sizing call.
    unsafe {
        GetFileSecurityW(
            path.as_ptr(),
            requested,
            std::ptr::null_mut(),
            0,
            &mut needed,
        )
    };
    if needed == 0 {
        return Err(std::io::Error::last_os_error()).context("measure Windows security descriptor");
    }
    let mut descriptor = vec![0_u8; needed as usize];
    // SAFETY: the output buffer has the measured size.
    if unsafe {
        GetFileSecurityW(
            path.as_ptr(),
            requested,
            descriptor.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("read Windows security descriptor");
    }
    descriptor.truncate(needed as usize);
    let mut control = 0_u16;
    let mut revision = 0_u32;
    // SAFETY: `descriptor` is a valid self-relative descriptor returned by
    // GetFileSecurityW; outputs point to live integers.
    if unsafe {
        GetSecurityDescriptorControl(descriptor.as_mut_ptr().cast(), &mut control, &mut revision)
    } == 0
    {
        return Err(std::io::Error::last_os_error())
            .context("inspect Windows security descriptor control");
    }
    Ok((descriptor, control & SE_DACL_PROTECTED != 0))
}

#[cfg(windows)]
fn restore_windows_security_descriptor(path: &Path, security: &SecurityMetadata) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, SetFileSecurityW,
        UNPROTECTED_DACL_SECURITY_INFORMATION,
    };
    let descriptor = security
        .windows_security_descriptor
        .as_deref()
        .context("recorded Windows security metadata has no descriptor")?;
    if descriptor.is_empty() {
        bail!("recorded Windows security descriptor is empty");
    }
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let protection = if security.windows_dacl_protected.unwrap_or(false) {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };
    let requested = OWNER_SECURITY_INFORMATION
        | GROUP_SECURITY_INFORMATION
        | DACL_SECURITY_INFORMATION
        | protection;
    // SAFETY: path and descriptor remain live for SetFileSecurityW.
    if unsafe {
        SetFileSecurityW(
            path.as_ptr(),
            requested,
            descriptor.as_ptr().cast_mut().cast(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("restore Windows owner/group/DACL");
    }
    Ok(())
}

/// Read the stable filesystem identity used by takeover compare-and-swap
/// records without modifying the file. Discovery uses this to show when
/// distinct launchers resolve to the same executable or package leaf.
pub fn inspect_file_identity(path: &Path) -> Result<FileIdentity> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("inspect file identity for {}", path.display()))?;
    file_identity(path, &metadata)
}

fn file_identity(path: &Path, metadata: &Metadata) -> Result<FileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = path;
        Ok(FileIdentity {
            device: Some(metadata.dev()),
            inode: Some(metadata.ino()),
            volume_serial: None,
            file_id: None,
            length: metadata.len(),
            modified_ns: Some(
                metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128,
            ),
        })
    }
    #[cfg(windows)]
    {
        windows_file_identity(path, metadata)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(FileIdentity {
            length: metadata.len(),
            ..Default::default()
        })
    }
}

#[cfg(windows)]
fn windows_file_identity(path: &Path, metadata: &Metadata) -> Result<FileIdentity> {
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFileInformationByHandle,
    };

    let file = fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .context("open Windows file identity handle")?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let result = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
    if result == 0 {
        return Err(std::io::Error::last_os_error()).context("read Windows file identity");
    }
    let file_index = ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64;
    Ok(FileIdentity {
        device: None,
        inode: None,
        volume_serial: Some(info.dwVolumeSerialNumber as u64),
        file_id: Some(format!("{file_index:016x}")),
        length: metadata.file_size(),
        modified_ns: Some(metadata.last_write_time() as i128 * 100),
    })
}

pub fn validate_surface_launcher_type(surface: &Path) -> Result<()> {
    #[cfg(windows)]
    windows_shim_form(surface).map(|_| ())?;
    #[cfg(not(windows))]
    let _ = surface;
    Ok(())
}

#[cfg(windows)]
fn windows_shim_form(surface: &Path) -> Result<ShimForm> {
    let extension = surface
        .extension()
        .map(|extension| {
            extension
                .to_str()
                .context("Windows launcher extension is not valid Unicode")
                .map(str::to_ascii_lowercase)
        })
        .transpose()?;
    match extension.as_deref() {
        Some("cmd" | "bat") => Ok(ShimForm::WindowsCmd),
        Some("ps1") => Ok(ShimForm::WindowsPowerShell),
        None | Some("exe") => Ok(ShimForm::WindowsNative),
        Some(extension) => bail!(
            "unsupported Windows Codex launcher extension .{extension}; select a native .exe, .cmd/.bat, or .ps1 surface"
        ),
    }
}

#[cfg(windows)]
fn windows_shim_bytes(surface: &Path, manager: &Path) -> Result<Vec<u8>> {
    match windows_shim_form(surface)? {
        ShimForm::WindowsNative => {
            fs::read(manager).with_context(|| format!("read stable manager {}", manager.display()))
        }
        ShimForm::WindowsCmd => {
            let manager = manager.to_string_lossy().replace('%', "%%");
            Ok(
                format!("@echo off\r\n\"{manager}\" __dispatch %*\r\nexit /b %ERRORLEVEL%\r\n")
                    .into_bytes(),
            )
        }
        ShimForm::WindowsPowerShell => {
            let manager = manager.to_string_lossy().replace('\'', "''");
            Ok(format!("& '{manager}' '__dispatch' @args\r\nexit $LASTEXITCODE\r\n").into_bytes())
        }
        ShimForm::PosixSymlink => unreachable!(),
    }
}

/// Inspect the existing Linux user updater without changing it. The returned
/// intent is safe to display before confirmation and must be saved durably
/// before [`apply_codex_update_manager_disable`] is called.
pub fn inspect_codex_update_manager(timeout: Duration) -> Result<Option<UpdaterAdapterRecord>> {
    #[cfg(target_os = "linux")]
    {
        let Some((_, load_state)) = systemctl_query(
            &[
                "show",
                "--property=LoadState",
                "--value",
                CODEX_UPDATE_MANAGER_UNIT,
            ],
            timeout,
        ) else {
            return Ok(None);
        };
        let load_state = load_state.trim().to_string();
        if load_state.is_empty() || load_state == "not-found" {
            return Ok(None);
        }

        let enabled_state = systemctl_query(&["is-enabled", CODEX_UPDATE_MANAGER_UNIT], timeout)
            .map(|(_, output)| output.trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let active_state = systemctl_query(&["is-active", CODEX_UPDATE_MANAGER_UNIT], timeout)
            .map(|(_, output)| output.trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let mut record = UpdaterAdapterRecord {
            unit: CODEX_UPDATE_MANAGER_UNIT.to_string(),
            load_state,
            enabled_state: enabled_state.clone(),
            active_state: active_state.clone(),
            disable_intended: matches!(enabled_state.as_str(), "enabled" | "enabled-runtime"),
            stop_intended: matches!(active_state.as_str(), "active" | "activating"),
            disabled_by_patcher: false,
            stopped_by_patcher: false,
            notes: Vec::new(),
        };
        if matches!(enabled_state.as_str(), "linked" | "linked-runtime") {
            record.notes.push(
                "linked unit was not disabled because its external link target cannot be restored exactly"
                    .to_string(),
            );
        }
        Ok(Some(record))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = timeout;
        Ok(None)
    }
}

/// Apply a previously inspected and durably recorded updater intent. This
/// never creates, installs, or masks a unit. Individual action failures are
/// retained as warnings so takeover can proceed with manual shim repair.
pub fn apply_codex_update_manager_disable(
    record: &mut UpdaterAdapterRecord,
    timeout: Duration,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if record.unit != CODEX_UPDATE_MANAGER_UNIT {
            bail!("refusing to manage unknown updater unit {}", record.unit);
        }
        if record.stop_intended && !record.stopped_by_patcher {
            match systemctl_action(&["stop", &record.unit], timeout) {
                Ok(()) => record.stopped_by_patcher = true,
                Err(error) => push_unique_note(
                    &mut record.notes,
                    format!("could not stop updater: {error}"),
                ),
            }
        }
        if record.disable_intended && !record.disabled_by_patcher {
            match systemctl_action(&["disable", &record.unit], timeout) {
                Ok(()) => record.disabled_by_patcher = true,
                Err(error) => push_unique_note(
                    &mut record.notes,
                    format!("could not disable updater: {error}"),
                ),
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (record, timeout);
        Ok(())
    }
}

/// Compatibility convenience for callers which do not need a preflight. New
/// takeover code should inspect, persist, and then apply in separate steps.
pub fn disable_codex_update_manager(timeout: Duration) -> Result<Option<UpdaterAdapterRecord>> {
    let Some(mut record) = inspect_codex_update_manager(timeout)? else {
        return Ok(None);
    };
    apply_codex_update_manager_disable(&mut record, timeout)?;
    Ok(Some(record))
}

#[cfg(target_os = "linux")]
fn push_unique_note(notes: &mut Vec<String>, note: String) {
    if !notes.contains(&note) {
        notes.push(note);
    }
}

/// Restore the preflight state after either a completed action or a crash with
/// only persisted intent. Enabling an originally enabled unit and starting an
/// originally active unit are deliberately idempotent recovery operations.
pub fn restore_codex_update_manager(
    record: &UpdaterAdapterRecord,
    timeout: Duration,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if record.unit != CODEX_UPDATE_MANAGER_UNIT {
            bail!("refusing to manage unknown updater unit {}", record.unit);
        }
        let exists = systemctl_query(
            &["show", "--property=LoadState", "--value", &record.unit],
            timeout,
        )
        .map(|(_, value)| value.trim() != "not-found")
        .unwrap_or(false);
        if !exists {
            bail!("updater unit no longer exists: {}", record.unit);
        }

        if record.disabled_by_patcher || record.disable_intended {
            match record.enabled_state.as_str() {
                "enabled" => systemctl_action(&["enable", &record.unit], timeout)?,
                "enabled-runtime" => {
                    systemctl_action(&["enable", "--runtime", &record.unit], timeout)?
                }
                state => bail!("cannot exactly restore unsupported enablement state {state}"),
            }
        }
        if (record.stopped_by_patcher || record.stop_intended)
            && matches!(record.active_state.as_str(), "active" | "activating")
        {
            systemctl_action(&["start", &record.unit], timeout)?;
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (record, timeout);
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn systemctl_query(args: &[&str], timeout: Duration) -> Option<(bool, String)> {
    let mut child = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let status = match child.wait_timeout(timeout).ok()? {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    let output = child.wait_with_output().ok()?;
    Some((
        status.success(),
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

#[cfg(target_os = "linux")]
fn systemctl_action(args: &[&str], timeout: Duration) -> Result<()> {
    let Some((success, output)) = systemctl_query(args, timeout) else {
        bail!(
            "systemctl --user {} timed out or could not start",
            args.join(" ")
        );
    };
    if !success {
        bail!("systemctl --user {} failed: {}", args.join(" "), output);
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Fixture {
        _temp: tempfile::TempDir,
        surface: PathBuf,
        manager: PathBuf,
        backups: PathBuf,
    }

    impl Fixture {
        fn new(bytes: &[u8]) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let fixture = Self {
                surface: temp.path().join("codex"),
                manager: temp.path().join("codex-patcher"),
                backups: temp.path().join("backups"),
                _temp: temp,
            };
            fixture.write(&fixture.surface, bytes, 0o751);
            fixture.write(&fixture.manager, b"manager", 0o755);
            fixture
        }

        fn write(&self, path: &Path, bytes: &[u8], mode: u32) {
            fs::write(path, bytes).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        }

        fn replace(&self, bytes: &[u8]) {
            fs::remove_file(&self.surface).unwrap();
            self.write(&self.surface, bytes, 0o751);
        }

        fn install(&self) -> SurfaceRecord {
            install_redirect(&self.surface, &self.manager, &self.backups).unwrap()
        }
    }

    #[test]
    fn file_baseline_round_trips_exact_bytes_mode_and_security_metadata() {
        let fixture = Fixture::new(b"original");
        #[cfg(target_os = "linux")]
        restore_linux_extended_attributes(
            &fixture.surface,
            false,
            &[ExtendedAttribute {
                name: b"user.codex-patcher-test".to_vec(),
                value: b"preserve me".to_vec(),
            }],
        )
        .unwrap();
        let record = fixture.install();
        uninstall_redirect(&record).unwrap();
        assert_eq!(fs::read(&fixture.surface).unwrap(), b"original");
        let metadata = fs::symlink_metadata(&fixture.surface).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o751);
        let security = capture_security_metadata(&fixture.surface, &metadata).unwrap();
        assert_eq!(security, record.baseline.security.unwrap());
    }

    #[test]
    fn relative_symlink_baseline_is_restored_verbatim() {
        let fixture = Fixture::new(b"discard");
        let original = fixture._temp.path().join("original");
        fixture.write(&original, b"original", 0o755);
        fs::remove_file(&fixture.surface).unwrap();
        symlink("original", &fixture.surface).unwrap();
        let record = fixture.install();
        uninstall_redirect(&record).unwrap();
        let target = fs::read_link(&fixture.surface).unwrap();
        assert_eq!(target, Path::new("original"));
    }

    #[test]
    fn atomic_cas_races_and_external_drift_never_clobber_owner() {
        let fixture = Fixture::new(b"original");
        let baseline = capture_baseline(&fixture.surface, &fixture.backups).unwrap();
        fixture.replace(b"racing takeover");
        let takeover_id = Uuid::new_v4();
        assert!(
            cas_install_shim(
                &fixture.surface,
                &fixture.manager,
                takeover_id,
                baseline.kind,
                |displaced| baseline_is_untouched(displaced, &baseline)
            )
            .is_err()
        );
        assert_eq!(fs::read(&fixture.surface).unwrap(), b"racing takeover");

        fixture.replace(b"original");
        let record = fixture.install();
        fixture.replace(b"racing uninstall");
        assert!(uninstall_redirect(&record).is_err());
        assert_eq!(fs::read(&fixture.surface).unwrap(), b"racing uninstall");
    }

    #[test]
    fn prepared_takeover_recovers_after_atomic_mutation() {
        let fixture = Fixture::new(b"original");
        let record = fixture.install();
        let path = takeover_journal_path(&fixture.backups, record.id);
        let mut journal = read_takeover_journal(&path).unwrap();
        journal.installed = None;
        journal.installed_at = None;
        write_takeover_journal(&fixture.backups, &journal).unwrap();
        let recovered = recover_redirect_journals(&fixture.backups).unwrap();
        assert_eq!(recovered.len(), 1);
        assert!(matches_recorded_shim(&recovered[0]).unwrap());
        assert!(read_takeover_journal(&path).unwrap().installed.is_some());
        finalize_redirect(&recovered[0], &fixture.backups).unwrap();
        uninstall_redirect(&recovered[0]).unwrap();
        assert_eq!(fs::read(&fixture.surface).unwrap(), b"original");
    }

    #[test]
    fn journaled_adopt_repair_is_recoverable_and_restores_new_baseline() {
        let fixture = Fixture::new(b"original");
        let mut record = fixture.install();
        finalize_redirect(&record, &fixture.backups).unwrap();
        fixture.replace(b"owner update");
        assert!(
            repair_redirect_journaled(&mut record, &fixture.manager, &fixture.backups, false)
                .is_err()
        );
        assert_eq!(
            repair_redirect_journaled(&mut record, &fixture.manager, &fixture.backups, true)
                .unwrap(),
            RepairOutcome::AdoptedAndReinstalled
        );
        assert_eq!(
            recover_repair_journals(&fixture.backups).unwrap(),
            vec![record.clone()]
        );
        finalize_repair_redirect(&record, &fixture.backups).unwrap();
        uninstall_redirect(&record).unwrap();
        assert_eq!(fs::read(&fixture.surface).unwrap(), b"owner update");
    }
}
