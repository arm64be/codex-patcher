use crate::STATE_SCHEMA;
use crate::paths::PatcherPaths;
use crate::shim::{
    SurfaceRecord, UpdaterAdapterRecord, finalize_redirect, finalize_repair_redirect,
    finalize_restore_journal, recover_redirect_journals, recover_repair_journals,
    recover_restore_journals,
};
use crate::types::{FailureRecord, GenerationRef, ProbeState, ResolvedSource};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallState {
    pub schema: u32,
    pub patch_dir: PathBuf,
    pub active: Option<GenerationRef>,
    pub previous: Option<GenerationRef>,
    #[serde(default)]
    pub probe: ProbeState,
    pub failure: Option<FailureRecord>,
    #[serde(default)]
    pub surfaces: Vec<SurfaceRecord>,
    #[serde(default)]
    pub updaters: Vec<UpdaterAdapterRecord>,
    pub installed_at: chrono::DateTime<Utc>,
    pub updated_at: chrono::DateTime<Utc>,
}

impl InstallState {
    pub fn new(patch_dir: PathBuf) -> Self {
        let now = Utc::now();
        Self {
            schema: STATE_SCHEMA,
            patch_dir,
            active: None,
            previous: None,
            probe: ProbeState::default(),
            failure: None,
            surfaces: Vec::new(),
            updaters: Vec::new(),
            installed_at: now,
            updated_at: now,
        }
    }

    pub fn activate(&mut self, generation: GenerationRef) {
        if self.active.as_ref().map(|active| active.id.as_str()) != Some(generation.id.as_str()) {
            self.previous = self.active.take();
        }
        self.active = Some(generation);
        self.failure = None;
        self.updated_at = Utc::now();
    }

    pub fn resolution_baseline(&self, channel: &str) -> Result<Option<&ResolvedSource>> {
        let mut candidates: Vec<_> = self
            .active
            .iter()
            .chain(self.previous.iter())
            .map(|generation| &generation.source)
            .filter(|source| source.channel == channel)
            .collect();
        if let Some(source) = self.probe.desired.as_ref().map(|desired| &desired.source)
            && source.channel == channel
        {
            candidates.push(source);
        }
        if channel == "nightly" {
            return Ok(candidates.pop());
        }

        let mut best: Option<(&ResolvedSource, semver::Version)> = None;
        for source in candidates {
            let version = semver::Version::parse(&source.version).with_context(|| {
                format!(
                    "stored {} source has invalid SemVer {}",
                    source.channel, source.version
                )
            })?;
            if best
                .as_ref()
                .is_none_or(|(_, best_version)| version >= *best_version)
            {
                best = Some((source, version));
            }
        }
        Ok(best.map(|(source, _)| source))
    }
}

#[derive(Debug, Clone)]
pub struct StateStore {
    paths: PatcherPaths,
}

impl StateStore {
    pub fn new(paths: PatcherPaths) -> Self {
        Self { paths }
    }

    pub fn paths(&self) -> &PatcherPaths {
        &self.paths
    }

    pub fn load(&self) -> Result<Option<InstallState>> {
        let path = self.paths.state_file();
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let state: InstallState = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        if state.schema != STATE_SCHEMA {
            bail!(
                "unsupported state schema {} in {}; expected {}",
                state.schema,
                path.display(),
                STATE_SCHEMA
            );
        }
        Ok(Some(state))
    }

    pub fn require(&self) -> Result<InstallState> {
        self.load()?
            .context("codex-patcher is not installed for this user")
    }

    pub fn save(&self, state: &InstallState) -> Result<()> {
        self.paths.ensure()?;
        let mut next = state.clone();
        next.updated_at = Utc::now();
        let bytes = serde_json::to_vec_pretty(&next)?;
        atomic_write(&self.paths.state_file(), &bytes)
    }

    pub fn with_state_lock<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let _lock = self.lock(&self.paths.state_lock, "patcher state")?;
        f()
    }

    pub fn build_lock(&self) -> Result<LockGuard> {
        self.lock(&self.paths.build_lock, "build state")
    }

    pub fn manager_lock(&self) -> Result<LockGuard> {
        self.lock(&self.paths.manager_lock, "manager operation")
    }

    pub fn try_manager_lock(&self) -> Result<Option<LockGuard>> {
        self.try_lock(&self.paths.manager_lock, "manager operation")
    }

    pub fn recover_takeovers(&self) -> Result<usize> {
        let recovered = recover_redirect_journals(&self.paths.backups_dir)?;
        if recovered.is_empty() {
            return Ok(0);
        }
        self.with_state_lock(|| {
            let mut state = self.require().context(
                "launcher takeover journals exist without installation state; refusing to discard them",
            )?;
            for record in &recovered {
                if let Some(existing) = state
                    .surfaces
                    .iter()
                    .find(|existing| existing.id == record.id)
                {
                    if existing != record {
                        bail!(
                            "takeover journal {} disagrees with its durable ownership record",
                            record.id
                        );
                    }
                    continue;
                }
                if state
                    .surfaces
                    .iter()
                    .any(|existing| existing.surface == record.surface)
                {
                    bail!(
                        "multiple ownership records refer to launcher {}",
                        record.surface.display()
                    );
                }
                state.surfaces.push(record.clone());
            }
            self.save(&state)
        })?;
        for record in &recovered {
            finalize_redirect(record, &self.paths.backups_dir)?;
        }
        Ok(recovered.len())
    }

    pub fn recover_restores(&self) -> Result<usize> {
        let restored = recover_restore_journals(&self.paths.backups_dir)?;
        if restored.is_empty() {
            return Ok(0);
        }
        self.with_state_lock(|| {
            let mut state = self.require().context(
                "launcher restore journals exist without installation state; refusing to discard them",
            )?;
            state
                .surfaces
                .retain(|record| !restored.contains(&record.id));
            self.save(&state)
        })?;
        for id in &restored {
            finalize_restore_journal(*id, &self.paths.backups_dir)?;
        }
        Ok(restored.len())
    }

    pub fn recover_repairs(&self) -> Result<usize> {
        let recovered = recover_repair_journals(&self.paths.backups_dir)?;
        if recovered.is_empty() {
            return Ok(0);
        }
        self.with_state_lock(|| {
            let mut state = self.require().context(
                "launcher repair journals exist without installation state; refusing to discard them",
            )?;
            for record in &recovered {
                let existing = state
                    .surfaces
                    .iter_mut()
                    .find(|existing| existing.id == record.id)
                    .with_context(|| {
                        format!(
                            "repair journal {} has no selected launcher ownership record",
                            record.id
                        )
                    })?;
                if existing.surface != record.surface {
                    bail!(
                        "repair journal {} targets {}, but state records {}",
                        record.id,
                        record.surface.display(),
                        existing.surface.display()
                    );
                }
                *existing = record.clone();
            }
            self.save(&state)
        })?;
        for record in &recovered {
            finalize_repair_redirect(record, &self.paths.backups_dir)?;
        }
        Ok(recovered.len())
    }

    pub fn recover_surface_transactions(&self) -> Result<usize> {
        Ok(self.recover_takeovers()? + self.recover_repairs()? + self.recover_restores()?)
    }

    pub fn try_probe_lock(&self) -> Result<Option<LockGuard>> {
        self.try_lock(&self.paths.probe_lock, "probe state")
    }

    fn lock(&self, path: &Path, label: &str) -> Result<LockGuard> {
        self.paths.ensure()?;
        let file = open_lock(path)?;
        file.lock_exclusive()
            .with_context(|| format!("locking {label}"))?;
        Ok(LockGuard { file })
    }

    fn try_lock(&self, path: &Path, label: &str) -> Result<Option<LockGuard>> {
        self.paths.ensure()?;
        let file = open_lock(path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(LockGuard { file })),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error).with_context(|| format!("locking {label}")),
        }
    }
}

pub struct LockGuard {
    file: File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn open_lock(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening lock {}", path.display()))
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("atomic-write path has no parent")?;
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("atomic-write path has no UTF-8 filename")?;
    let temporary = parent.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .with_context(|| format!("creating {}", temporary.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    if let Err(error) = atomic_replace(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error)
            .with_context(|| format!("renaming {} to {}", temporary.display(), path.display()));
    }
    if let Ok(parent_file) = File::open(parent) {
        let _ = parent_file.sync_all();
    }
    Ok(())
}

#[cfg(unix)]
fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
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
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn atomic_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DesiredBuild, ResolvedSource};

    fn source(version: &str, marker: char) -> ResolvedSource {
        ResolvedSource {
            channel: "stable".into(),
            ref_name: format!("refs/tags/rust-v{version}"),
            ref_object_oid: marker.to_string().repeat(40),
            commit_oid: marker.to_ascii_uppercase().to_string().repeat(40),
            version: version.into(),
            release_url: None,
        }
    }

    fn generation(source: ResolvedSource) -> GenerationRef {
        GenerationRef {
            id: source.commit_oid.clone(),
            package_dir: PathBuf::from("/generation/package"),
            binary: PathBuf::from("/generation/package/bin/codex"),
            source_key: source.commit_oid.clone(),
            source,
            patch_fingerprint: "f".repeat(64),
            target: "x86_64-unknown-linux-gnu".into(),
            subcommands: Vec::new(),
            built_at: Utc::now(),
        }
    }

    #[test]
    fn state_round_trip_is_atomic_and_versioned() {
        let temp = tempfile::tempdir().unwrap();
        let paths = PatcherPaths::for_test(temp.path());
        let store = StateStore::new(paths);
        let expected = InstallState::new(temp.path().join("patches"));
        store.save(&expected).unwrap();
        let actual = store.load().unwrap().unwrap();
        assert_eq!(actual.schema, STATE_SCHEMA);
        assert_eq!(actual.patch_dir, expected.patch_dir);
    }

    #[test]
    fn pending_release_is_the_resolution_trust_baseline() {
        let mut state = InstallState::new(PathBuf::from("/patches"));
        state.active = Some(generation(source("1.0.0", 'a')));
        let pending = source("2.0.0", 'b');
        state.probe.desired = Some(DesiredBuild {
            source: pending.clone(),
            patch_fingerprint: "f".repeat(64),
            target: "x86_64-unknown-linux-gnu".into(),
            source_key: "k".repeat(64),
        });

        assert_eq!(state.resolution_baseline("stable").unwrap(), Some(&pending));
    }

    #[test]
    fn manager_try_lock_never_waits_behind_foreground_work() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(PatcherPaths::for_test(temp.path()));
        let held = store.manager_lock().unwrap();
        assert!(store.try_manager_lock().unwrap().is_none());
        drop(held);
        assert!(store.try_manager_lock().unwrap().is_some());
    }
}
