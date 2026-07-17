use crate::patchset::PatchSet;
use crate::paths::{PatcherPaths, display_user_path};
use crate::state::atomic_write;
use crate::types::{DesiredBuild, FileHash, GenerationManifest, GenerationRef};
use crate::{STATE_SCHEMA, UPSTREAM_REPOSITORY};
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use uuid::Uuid;
use wait_timeout::ChildExt;
use walkdir::WalkDir;

// Codex's package builder defines this profile specifically for fast, small
// local iteration. Unlike `release`, it does not run ThinLTO after every patch.
const CARGO_PROFILE: &str = "dev-small";
const MAX_FAILURE_LOG_TAIL: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub enum BuildEvent {
    Phase(&'static str),
    Line(String),
}

#[derive(Debug)]
pub struct BuildFailure {
    pub phase: String,
    pub summary: String,
    /// True only for recognized network/transport failures. These must not be
    /// cached as deterministic failures of a source key when last-good exists.
    pub transient: bool,
    pub failed_patch_index: Option<usize>,
    pub failed_patch: Option<String>,
    pub log_path: PathBuf,
}

impl std::fmt::Display for BuildFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}: {} (details: {})",
            self.phase,
            self.summary,
            display_user_path(&self.log_path)
        )
    }
}

impl std::error::Error for BuildFailure {}

impl BuildFailure {
    fn new(phase: impl Into<String>, error: impl std::fmt::Display, log_path: &Path) -> Self {
        let phase = phase.into();
        let summary = sanitize_line(&error.to_string());
        Self {
            transient: transient_network_failure(&phase, &summary, log_path),
            phase,
            summary,
            failed_patch_index: None,
            failed_patch: None,
            log_path: log_path.to_path_buf(),
        }
    }

    fn patch(mut self, index: usize, path: &Path) -> Self {
        self.failed_patch_index = Some(index);
        self.failed_patch = Some(path.to_string_lossy().into_owned());
        self
    }
}

fn transient_network_failure(phase: &str, summary: &str, log_path: &Path) -> bool {
    if !matches!(phase, "resolve" | "build") {
        return false;
    }
    let log = fs::read(log_path).unwrap_or_default();
    let tail = &log[log.len().saturating_sub(MAX_FAILURE_LOG_TAIL)..];
    let text = format!("{summary}\n{}", String::from_utf8_lossy(tail)).to_ascii_lowercase();
    [
        "could not resolve host",
        "temporary failure in name resolution",
        "connection reset",
        "connection refused",
        "connection timed out",
        "operation timed out",
        "network failure",
        "spurious network error",
        "failed to download",
        "failed to get successful http response",
        "unable to access 'http",
        "error sending request",
        "http 429",
        "http 500",
        "http 502",
        "http 503",
        "http 504",
        "tls handshake",
        "ssl connect error",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn package_builder_failure_summary(status: &ExitStatus, target: &str, log_path: &Path) -> String {
    package_builder_log_diagnostic(target, log_path)
        .unwrap_or_else(|| format!("package builder exited with {status}"))
}

fn package_builder_log_diagnostic(target: &str, log_path: &Path) -> Option<String> {
    let log = fs::read(log_path).ok()?;
    let tail = &log[log.len().saturating_sub(MAX_FAILURE_LOG_TAIL)..];
    let text = String::from_utf8_lossy(tail);
    let folded = text.to_ascii_lowercase();
    if folded.contains("can't find crate for `core`")
        && folded.contains("target may not be installed")
    {
        return Some(format!(
            "Rust target {target} is not installed for Codex's pinned toolchain"
        ));
    }
    text.lines()
        .map(str::trim)
        .find(|line| line.starts_with("error:") || line.starts_with("error["))
        .map(|line| format!("package builder failed: {}", sanitize_line(line)))
}

#[derive(Default)]
pub struct BuildOptions {
    pub allow_force_push: bool,
    pub retry: bool,
}

pub fn build_generation(
    paths: &PatcherPaths,
    patches: &PatchSet,
    desired: &DesiredBuild,
    previous: Option<&GenerationRef>,
    options: &BuildOptions,
    progress: &mut dyn FnMut(BuildEvent),
) -> std::result::Result<GenerationRef, BuildFailure> {
    if let Err(error) = paths.ensure() {
        return Err(BuildFailure::new(
            "prepare",
            error,
            &paths.logs_dir().join("prepare.log"),
        ));
    }
    let log_path = paths.logs_dir().join(format!(
        "build-{}-{}.log",
        Utc::now().format("%Y%m%dT%H%M%SZ"),
        &desired.source_key[..12]
    ));
    let mut phase = "prepare";
    let mut context = BuildAttemptContext {
        log_path: &log_path,
        failure_phase: &mut phase,
    };
    let result = build_generation_inner(
        paths,
        patches,
        desired,
        previous,
        options,
        progress,
        &mut context,
    );
    result.map_err(|error| match error.downcast::<BuildFailure>() {
        Ok(failure) => failure,
        Err(error) => BuildFailure::new(phase, error, &log_path),
    })
}

struct BuildAttemptContext<'a> {
    log_path: &'a Path,
    failure_phase: &'a mut &'static str,
}

fn build_generation_inner(
    paths: &PatcherPaths,
    patches: &PatchSet,
    desired: &DesiredBuild,
    previous: Option<&GenerationRef>,
    options: &BuildOptions,
    progress: &mut dyn FnMut(BuildEvent),
    context: &mut BuildAttemptContext<'_>,
) -> Result<GenerationRef> {
    let log_path = context.log_path;
    fs::create_dir_all(paths.logs_dir())?;
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    writeln!(log, "codex-patcher source key: {}", desired.source_key)?;
    writeln!(
        log,
        "upstream: {} @ {}",
        desired.source.ref_name, desired.source.commit_oid
    )?;

    let final_root = paths.generations_dir().join(&desired.source_key);
    let final_manifest = final_root.join("generation.json");
    let replacing_corrupt = if final_root.exists() {
        *context.failure_phase = "validate";
        match load_validated_generation(&final_manifest, desired, &mut log) {
            Ok(manifest) => {
                progress(BuildEvent::Line(
                    "using previously validated immutable generation".into(),
                ));
                return Ok(manifest.generation);
            }
            Err(error) if options.retry => {
                writeln!(
                    log,
                    "retry requested after immutable generation validation failed: {error:#}"
                )?;
                progress(BuildEvent::Line(
                    "quarantining corrupt generation after replacement validates".into(),
                ));
                true
            }
            Err(error) => return Err(error),
        }
    } else {
        false
    };

    *context.failure_phase = "resolve";
    progress(BuildEvent::Phase("resolve"));
    ensure_mirror(paths, &mut log)?;
    let fetched = fetch_source(paths, desired, &mut log)?;
    if fetched.0 != desired.source.ref_object_oid || fetched.1 != desired.source.commit_oid {
        bail!(
            "fetched source identity changed: expected {}/{}, got {}/{}",
            desired.source.ref_object_oid,
            desired.source.commit_oid,
            fetched.0,
            fetched.1
        );
    }

    if desired.source.channel == "nightly"
        && let Some(previous) = previous
        && previous.source.channel == "nightly"
        && previous.source.commit_oid != desired.source.commit_oid
        && !is_ancestor(
            paths,
            &previous.source.commit_oid,
            &desired.source.commit_oid,
        )?
        && !options.allow_force_push
    {
        bail!(
            "upstream main is no longer a descendant of {}; rerun with --accept-force-push",
            previous.source.commit_oid
        );
    }

    *context.failure_phase = "checkout";
    progress(BuildEvent::Phase("checkout"));
    let worktree = build_worktree_dir(paths, desired)?;
    prepare_build_worktree(paths, &worktree, &desired.source.commit_oid, &mut log)?;

    *context.failure_phase = "patch";
    verify_workspace_version(&worktree, desired)?;
    progress(BuildEvent::Phase("patch"));
    for (offset, patch) in patches.patches.iter().enumerate() {
        let index = offset + 1;
        progress(BuildEvent::Line(format!(
            "Applying {index}/{} {}",
            patches.patches.len(),
            patch.path
        )));
        let snapshot =
            paths
                .worktrees_dir()
                .join(format!("patch-{}-{}.patch", Uuid::new_v4(), index));
        atomic_write(&snapshot, &patch.bytes)?;
        let check = git_in(&worktree, ["apply", "--check"], Some(&snapshot), &mut log);
        if let Err(error) = check {
            let _ = fs::remove_file(&snapshot);
            return Err(anyhow!(
                BuildFailure::new("patch", error, log_path).patch(index, Path::new(&patch.path))
            ));
        }
        let apply = git_in(&worktree, ["apply", "--index"], Some(&snapshot), &mut log);
        let _ = fs::remove_file(&snapshot);
        if let Err(error) = apply {
            return Err(anyhow!(
                BuildFailure::new("patch", error, log_path).patch(index, Path::new(&patch.path))
            ));
        }
    }
    verify_workspace_version(&worktree, desired)?;

    let live = PatchSet::load(&patches.root)?;
    if live.fingerprint != patches.fingerprint {
        bail!("patch directory changed while preparing the build");
    }

    *context.failure_phase = "build";
    progress(BuildEvent::Phase("build"));
    let staging_root = paths
        .generations_dir()
        .join(format!(".staging-{}", Uuid::new_v4()));
    if staging_root.exists() {
        fs::remove_dir_all(&staging_root)?;
    }
    let _staging_cleanup = DirectoryCleanup(staging_root.clone());
    let package_dir = staging_root.join("package");
    fs::create_dir_all(&package_dir)?;
    let cargo_target_dir = build_cache_dir(paths, desired);
    let warm_cache = directory_has_entries(&cargo_target_dir);
    fs::create_dir_all(&cargo_target_dir)?;
    writeln!(
        log,
        "incremental compiler cache: {} ({})",
        if warm_cache { "warm; reusing" } else { "cold" },
        cargo_target_dir.display()
    )?;
    writeln!(
        log,
        "cargo profile: {CARGO_PROFILE} (local iteration; no LTO)"
    )?;
    progress(BuildEvent::Line(format!(
        "Profile {CARGO_PROFILE} (incremental, no LTO)"
    )));
    if warm_cache {
        progress(BuildEvent::Line(
            "Cache warm (reusing compiler artifacts)".into(),
        ));
    }
    let (python, prefix) = find_python()?;
    let mut command = Command::new(&python);
    command.args(prefix);
    command
        .arg(worktree.join("scripts/build_codex_package.py"))
        .arg("--target")
        .arg(&desired.target)
        .arg("--variant")
        .arg("codex")
        .arg("--cargo-profile")
        .arg(CARGO_PROFILE)
        .arg("--package-dir")
        .arg(&package_dir)
        .current_dir(&worktree)
        // The Python package builder and every Cargo child it starts inherit
        // this generation-independent cache location.
        .env("CARGO_TARGET_DIR", &cargo_target_dir)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env("CODEX_PATCHER_CARGO_PROFILE", CARGO_PROFILE)
        .env("CODEX_PATCHER_BUILD", "1");
    let status = run_streaming(&mut command, &mut log, progress)?;
    if !status.success() {
        return Err(anyhow!(BuildFailure::new(
            "build",
            package_builder_failure_summary(&status, &desired.target, log_path),
            log_path
        )));
    }

    *context.failure_phase = "validate";
    progress(BuildEvent::Phase("validate"));
    let binary_name = if cfg!(windows) { "codex.exe" } else { "codex" };
    let staged_binary = package_dir.join("bin").join(binary_name);
    let subcommands = validate_package(&package_dir, &staged_binary, desired, &mut log)?;
    let live = PatchSet::load(&patches.root)?;
    if live.fingerprint != patches.fingerprint {
        let _ = fs::remove_dir_all(&staging_root);
        bail!("patch directory changed during the build; discarded staged generation");
    }

    let generation = GenerationRef {
        id: desired.source_key.clone(),
        package_dir: final_root.join("package"),
        binary: final_root.join("package").join("bin").join(binary_name),
        source_key: desired.source_key.clone(),
        source: desired.source.clone(),
        patch_fingerprint: desired.patch_fingerprint.clone(),
        target: desired.target.clone(),
        subcommands,
        built_at: Utc::now(),
    };
    let manifest = GenerationManifest {
        schema: STATE_SCHEMA,
        generation: generation.clone(),
        outputs: hash_tree(&package_dir)?,
        rustc: command_version("rustc", &["-vV"]),
        cargo: command_version("cargo", &["-V"]),
        python: command_version(&python, &["--version"]),
        linker: linker_version(),
        sdk: sdk_version(),
        environment: build_environment(&cargo_target_dir),
    };
    atomic_write(
        &staging_root.join("generation.json"),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;

    *context.failure_phase = "activate";
    if final_root.exists() && !replacing_corrupt {
        bail!(
            "immutable generation destination appeared during build: {}",
            final_root.display()
        );
    }
    let quarantine = paths
        .generations_dir()
        .join(format!(".corrupt-{}", Uuid::new_v4()));
    if replacing_corrupt {
        fs::rename(&final_root, &quarantine).with_context(|| {
            format!(
                "quarantining corrupt immutable generation {}",
                final_root.display()
            )
        })?;
    }
    if let Err(error) = fs::rename(&staging_root, &final_root) {
        if replacing_corrupt {
            let _ = fs::rename(&quarantine, &final_root);
        }
        return Err(error).context("activating validated immutable generation");
    }
    let _ = File::open(paths.generations_dir()).and_then(|file| file.sync_all());
    if replacing_corrupt && let Err(error) = fs::remove_dir_all(&quarantine) {
        writeln!(
            log,
            "warning: retained quarantined corrupt generation {}: {error}",
            quarantine.display()
        )?;
    }
    Ok(generation)
}

pub(crate) fn load_validated_generation(
    manifest_path: &Path,
    desired: &DesiredBuild,
    log: &mut File,
) -> Result<GenerationManifest> {
    if !manifest_path.is_file() {
        bail!(
            "immutable generation directory exists without a manifest: {}",
            manifest_path.display()
        );
    }
    let manifest: GenerationManifest = serde_json::from_slice(&fs::read(manifest_path)?)
        .with_context(|| format!("parsing generation manifest {}", manifest_path.display()))?;
    let root = manifest_path
        .parent()
        .context("generation manifest has no parent")?;
    let binary_name = if desired.target.contains("windows") {
        "codex.exe"
    } else {
        "codex"
    };
    if manifest.schema != STATE_SCHEMA
        || manifest.generation.id != desired.source_key
        || manifest.generation.source_key != desired.source_key
        || manifest.generation.source != desired.source
        || manifest.generation.patch_fingerprint != desired.patch_fingerprint
        || manifest.generation.target != desired.target
        || manifest.generation.package_dir != root.join("package")
        || manifest.generation.binary != root.join("package/bin").join(binary_name)
    {
        bail!(
            "immutable generation manifest does not match desired source key {}",
            desired.source_key
        );
    }
    let actual_outputs = hash_tree(&manifest.generation.package_dir)?;
    if actual_outputs != manifest.outputs {
        bail!(
            "immutable generation {} failed output hash verification",
            desired.source_key
        );
    }
    let subcommands = validate_package(
        &manifest.generation.package_dir,
        &manifest.generation.binary,
        desired,
        log,
    )?;
    if subcommands != manifest.generation.subcommands {
        bail!(
            "immutable generation {} has inconsistent command metadata",
            desired.source_key
        );
    }
    Ok(manifest)
}

fn ensure_mirror(paths: &PatcherPaths, log: &mut File) -> Result<()> {
    fs::create_dir_all(paths.cache_dir())?;
    if !paths.mirror_dir().join("HEAD").exists() {
        let status = Command::new("git")
            .args(["init", "--bare"])
            .arg(paths.mirror_dir())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log.try_clone()?))
            .status()?;
        if !status.success() {
            bail!("git init --bare failed with {status}");
        }
        git_bare(paths, ["remote", "add", "origin", UPSTREAM_REPOSITORY], log)?;
    } else {
        let current = git_bare_output(paths, ["remote", "get-url", "origin"])?;
        if current.trim() != UPSTREAM_REPOSITORY {
            bail!(
                "private mirror origin is {}, expected {UPSTREAM_REPOSITORY}",
                current.trim()
            );
        }
    }
    Ok(())
}

fn fetch_source(
    paths: &PatcherPaths,
    desired: &DesiredBuild,
    log: &mut File,
) -> Result<(String, String)> {
    let destination = format!("refs/codex-patcher/source/{}", &desired.source_key[..16]);
    let refspec = source_fetch_refspec(desired, &destination);
    git_bare(
        paths,
        ["fetch", "--force", "--no-auto-gc", "origin", &refspec],
        log,
    )?;
    let object = git_bare_output(paths, ["rev-parse", &destination])?
        .trim()
        .to_string();
    let peeled = git_bare_output(paths, ["rev-parse", &format!("{destination}^{{commit}}")])?
        .trim()
        .to_string();
    Ok((object, peeled))
}

fn source_fetch_refspec(desired: &DesiredBuild, destination: &str) -> String {
    // The resolver retains GitHub's canonical fully-qualified ref name
    // (`refs/tags/...` or `refs/heads/main`). Do not prepend another namespace.
    format!("+{}:{destination}", desired.source.ref_name)
}

fn add_worktree(paths: &PatcherPaths, worktree: &Path, commit: &str, log: &mut File) -> Result<()> {
    fs::create_dir_all(paths.worktrees_dir())?;
    if worktree.exists() {
        fs::remove_dir_all(worktree)?;
    }
    let status = Command::new("git")
        .arg("--git-dir")
        .arg(paths.mirror_dir())
        .args(["worktree", "add", "--detach"])
        .arg(worktree)
        .arg(commit)
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?))
        .status()?;
    if !status.success() {
        bail!("git worktree add failed with {status}");
    }
    Ok(())
}

/// Reuse one stable checkout path per compatible target/profile. Cargo's
/// workspace fingerprints include absolute source paths, and recreating the
/// checkout at a random UUID makes every local Codex crate look new. Resetting
/// the persistent checkout changes mtimes only for source files that actually
/// differ, so patch-only builds can use Cargo's real incremental cache.
fn prepare_build_worktree(
    paths: &PatcherPaths,
    worktree: &Path,
    commit: &str,
    log: &mut File,
) -> Result<()> {
    if fs::symlink_metadata(worktree).is_ok_and(|metadata| metadata.is_dir()) {
        let reset = git_in(worktree, ["reset", "--hard", commit], None, log);
        let clean = reset.and_then(|()| git_in(worktree, ["clean", "-ffdx"], None, log));
        if clean.is_ok() {
            writeln!(log, "reused stable build worktree {}", worktree.display())?;
            return Ok(());
        }
        writeln!(
            log,
            "stable build worktree was invalid; recreating {}",
            worktree.display()
        )?;
    }
    remove_registered_worktree(paths, worktree, log)?;
    add_worktree(paths, worktree, commit, log)
}

fn build_worktree_dir(paths: &PatcherPaths, desired: &DesiredBuild) -> Result<PathBuf> {
    if !desired
        .target
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("build target contains unsafe path characters");
    }
    Ok(paths
        .worktrees_dir()
        .join(format!("build-{}-{CARGO_PROFILE}", desired.target)))
}

/// Materialize the already-resolved source in a persistent repair worktree.
///
/// Repair prefers the source-key ref retained by the original build. If that
/// ref is not present (for example, the failure happened before checkout), it
/// performs the same exact-ref fetch as a normal build and verifies both the
/// tag object and peeled commit before creating the worktree. The caller owns
/// the global build lock and the lifetime of the resulting worktree.
pub(crate) fn prepare_repair_worktree(
    paths: &PatcherPaths,
    desired: &DesiredBuild,
    worktree: &Path,
    log: &mut File,
) -> Result<()> {
    ensure_mirror(paths, log)?;
    let destination = format!("refs/codex-patcher/source/{}", &desired.source_key[..16]);
    let cached = (|| {
        let object = git_bare_output(paths, ["rev-parse", &destination])?
            .trim()
            .to_string();
        let peeled = git_bare_output(paths, ["rev-parse", &format!("{destination}^{{commit}}")])?
            .trim()
            .to_string();
        Ok::<_, anyhow::Error>((object, peeled))
    })();
    let identity = match cached {
        Ok(identity)
            if identity.0 == desired.source.ref_object_oid
                && identity.1 == desired.source.commit_oid =>
        {
            identity
        }
        _ => fetch_source(paths, desired, log)?,
    };
    if identity.0 != desired.source.ref_object_oid || identity.1 != desired.source.commit_oid {
        bail!(
            "repair source identity changed: expected {}/{}, got {}/{}",
            desired.source.ref_object_oid,
            desired.source.commit_oid,
            identity.0,
            identity.1
        );
    }
    remove_repair_worktree(paths, worktree, log)?;
    add_worktree(paths, worktree, &desired.source.commit_oid, log)?;
    verify_workspace_version(worktree, desired)
}

/// Remove a persistent repair worktree from both Git's registry and disk.
pub(crate) fn remove_repair_worktree(
    paths: &PatcherPaths,
    worktree: &Path,
    log: &mut File,
) -> Result<()> {
    remove_registered_worktree(paths, worktree, log)
}

fn remove_registered_worktree(paths: &PatcherPaths, worktree: &Path, log: &mut File) -> Result<()> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(paths.mirror_dir())
        .args(["worktree", "remove", "--force"])
        .arg(worktree)
        .output()?;
    log.write_all(&output.stdout)?;
    log.write_all(&output.stderr)?;
    if !output.status.success() {
        // A stale registration can remain after a killed build, and a stale
        // directory can remain without a registration. Both are patcher-owned.
        let prune = Command::new("git")
            .arg("--git-dir")
            .arg(paths.mirror_dir())
            .args(["worktree", "prune"])
            .output()?;
        log.write_all(&prune.stdout)?;
        log.write_all(&prune.stderr)?;
    }
    if worktree.exists() {
        fs::remove_dir_all(worktree)
            .with_context(|| format!("removing worktree {}", worktree.display()))?;
    }
    Ok(())
}

fn verify_workspace_version(worktree: &Path, desired: &DesiredBuild) -> Result<()> {
    let path = worktree.join("codex-rs/Cargo.toml");
    let document: toml::Value = toml::from_str(&fs::read_to_string(&path)?)?;
    let actual = document
        .get("workspace")
        .and_then(|value| value.get("package"))
        .and_then(|value| value.get("version"))
        .and_then(toml::Value::as_str)
        .context("codex-rs/Cargo.toml has no workspace.package.version")?;
    let expected = if desired.source.channel == "nightly" {
        "0.0.0"
    } else {
        desired.source.version.as_str()
    };
    if actual != expected {
        bail!("workspace version {actual} does not match resolved source {expected}");
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct PackageMetadata {
    #[serde(rename = "layoutVersion")]
    layout_version: u32,
    version: String,
    target: String,
    variant: String,
    entrypoint: String,
    #[serde(rename = "resourcesDir")]
    resources_dir: String,
    #[serde(rename = "pathDir")]
    path_dir: String,
}

fn validate_package(
    package: &Path,
    binary: &Path,
    desired: &DesiredBuild,
    log: &mut File,
) -> Result<Vec<String>> {
    let metadata_path = package.join("codex-package.json");
    let metadata: PackageMetadata = serde_json::from_slice(
        &fs::read(&metadata_path)
            .with_context(|| format!("reading canonical metadata {}", metadata_path.display()))?,
    )
    .context("parsing canonical codex-package.json")?;
    let version = run_with_timeout(binary, ["--version"], Duration::from_secs(20), log)?;
    let expected = if desired.source.channel == "nightly" {
        "0.0.0"
    } else {
        desired.source.version.as_str()
    };
    let executable_suffix = if desired.target.contains("windows") {
        ".exe"
    } else {
        ""
    };
    let expected_entrypoint = format!("bin/codex{executable_suffix}");
    if metadata.layout_version != 1
        || metadata.version != expected
        || metadata.target != desired.target
        || metadata.variant != "codex"
        || metadata.entrypoint != expected_entrypoint
        || metadata.resources_dir != "codex-resources"
        || metadata.path_dir != "codex-path"
    {
        bail!(
            "codex-package.json does not describe the requested canonical package: {:?}",
            metadata
        );
    }

    let mut required = vec![
        PathBuf::from(&metadata.entrypoint),
        PathBuf::from(format!("bin/codex-code-mode-host{executable_suffix}")),
        PathBuf::from(format!("codex-path/rg{executable_suffix}")),
    ];
    if desired.target.contains("linux") {
        required.push(PathBuf::from("codex-resources/bwrap"));
    }
    if desired.target.contains("windows") {
        required.extend([
            PathBuf::from("codex-resources/codex-command-runner.exe"),
            PathBuf::from("codex-resources/codex-windows-sandbox-setup.exe"),
        ]);
    } else {
        required.push(PathBuf::from("codex-resources/zsh/bin/zsh"));
    }
    for directory in ["bin", "codex-resources", "codex-path"] {
        if !package.join(directory).is_dir() {
            bail!("canonical package is missing directory {directory}");
        }
    }
    for relative in &required {
        let path = package.join(relative);
        if !path.is_file() {
            bail!("canonical package is missing {}", relative.display());
        }
        #[cfg(unix)]
        if !desired.target.contains("windows") {
            use std::os::unix::fs::PermissionsExt;
            if fs::metadata(&path)?.permissions().mode() & 0o111 == 0 {
                bail!(
                    "canonical package resource is not executable: {}",
                    path.display()
                );
            }
        }
    }
    if !version.contains(expected) {
        bail!("built Codex version {version:?} does not contain expected {expected}");
    }
    let help = run_with_timeout(binary, ["--help"], Duration::from_secs(20), log)?;
    let subcommands = parse_subcommands(&help)?;
    run_with_timeout(
        binary,
        ["app-server", "--help"],
        Duration::from_secs(20),
        log,
    )?;
    run_with_timeout(binary, ["app-server"], Duration::from_secs(20), log)?;
    Ok(subcommands)
}

fn parse_subcommands(help: &str) -> Result<Vec<String>> {
    let mut in_commands = false;
    let mut commands = Vec::new();
    for line in help.lines() {
        if line.trim() == "Commands:" {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        if matches!(line.trim(), "Arguments:" | "Options:") {
            break;
        }
        if !line.starts_with("  ") || line.starts_with("    ") {
            continue;
        }
        if let Some(command) = line.split_whitespace().next()
            && command
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            commands.push(command.to_owned());
        }
    }
    commands.sort();
    commands.dedup();
    for required in ["exec", "resume", "fork", "app-server"] {
        if !commands.iter().any(|command| command == required) {
            bail!("Codex help output is missing expected subcommand {required:?}");
        }
    }
    Ok(commands)
}

fn run_with_timeout<const N: usize>(
    program: &Path,
    args: [&str; N],
    timeout: Duration,
    log: &mut File,
) -> Result<String> {
    const MAX_VALIDATION_OUTPUT: u64 = 4 * 1024 * 1024;
    let validation_home = tempfile::tempdir().context("creating isolated Codex validation home")?;
    let mut stdout = tempfile::tempfile().context("creating validation stdout capture")?;
    let mut stderr = tempfile::tempfile().context("creating validation stderr capture")?;
    let mut child = Command::new(program)
        .args(args)
        .env("CODEX_HOME", validation_home.path())
        .env("CODEX_PATCHER_VALIDATION", "1")
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?))
        .spawn()?;
    match child.wait_timeout(timeout)? {
        Some(status) => {
            let _ = child.wait()?;
            let stdout = read_bounded_capture(&mut stdout, MAX_VALIDATION_OUTPUT)?;
            let stderr = read_bounded_capture(&mut stderr, MAX_VALIDATION_OUTPUT)?;
            log.write_all(&stdout)?;
            log.write_all(&stderr)?;
            if !status.success() {
                bail!("{} exited with {status}", program.display());
            }
            Ok(String::from_utf8_lossy(&stdout).trim().to_string())
        }
        None => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{} timed out after {timeout:?}", program.display())
        }
    }
}

fn read_bounded_capture(file: &mut File, limit: u64) -> Result<Vec<u8>> {
    let length = file.metadata()?.len();
    if length > limit {
        bail!("validation command emitted more than {limit} bytes");
    }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::with_capacity(length as usize);
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn run_streaming(
    command: &mut Command,
    log: &mut File,
    progress: &mut dyn FnMut(BuildEvent),
) -> Result<ExitStatus> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().context("capturing builder stdout")?;
    let stderr = child.stderr.take().context("capturing builder stderr")?;
    let (sender, receiver) = mpsc::channel::<Option<String>>();
    for stream in [
        Box::new(stdout) as Box<dyn std::io::Read + Send>,
        Box::new(stderr),
    ] {
        let sender = sender.clone();
        thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                let _ = sender.send(Some(line));
            }
            let _ = sender.send(None);
        });
    }
    drop(sender);
    let mut closed = 0;
    while closed < 2 {
        match receiver.recv() {
            Ok(Some(line)) => {
                writeln!(log, "{line}")?;
                progress(BuildEvent::Line(sanitize_line(&line)));
            }
            Ok(None) => closed += 1,
            Err(_) => break,
        }
    }
    Ok(child.wait()?)
}

fn git_in<const N: usize>(
    worktree: &Path,
    args: [&str; N],
    patch: Option<&Path>,
    log: &mut File,
) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("-C").arg(worktree).args(args);
    if let Some(patch) = patch {
        command.arg(patch);
    }
    let output = command.output()?;
    log.write_all(&output.stdout)?;
    log.write_all(&output.stderr)?;
    if !output.status.success() {
        bail!(
            "git failed: {}",
            sanitize_line(&String::from_utf8_lossy(&output.stderr))
        );
    }
    Ok(())
}

fn git_bare<const N: usize>(paths: &PatcherPaths, args: [&str; N], log: &mut File) -> Result<()> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(paths.mirror_dir())
        .args(args)
        .output()?;
    log.write_all(&output.stdout)?;
    log.write_all(&output.stderr)?;
    if !output.status.success() {
        bail!(
            "git failed: {}",
            sanitize_line(&String::from_utf8_lossy(&output.stderr))
        );
    }
    Ok(())
}

fn git_bare_output<const N: usize>(paths: &PatcherPaths, args: [&str; N]) -> Result<String> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(paths.mirror_dir())
        .args(args)
        .output()?;
    if !output.status.success() {
        bail!(
            "git failed: {}",
            sanitize_line(&String::from_utf8_lossy(&output.stderr))
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn is_ancestor(paths: &PatcherPaths, old: &str, new: &str) -> Result<bool> {
    let status = Command::new("git")
        .arg("--git-dir")
        .arg(paths.mirror_dir())
        .args(["merge-base", "--is-ancestor", old, new])
        .status()?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("git merge-base failed with {status}"),
    }
}

fn find_python() -> Result<(String, Vec<&'static str>)> {
    for (program, prefix) in [("python3", vec![]), ("python", vec![])] {
        if Command::new(program)
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
        {
            return Ok((program.to_string(), prefix));
        }
    }
    #[cfg(windows)]
    if Command::new("py")
        .args(["-3", "--version"])
        .output()
        .is_ok_and(|out| out.status.success())
    {
        return Ok(("py".into(), vec!["-3"]));
    }
    bail!("Python 3 is required to run the Codex package builder")
}

fn command_version(program: impl AsRef<OsStr>, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = if output.stdout.is_empty() {
        output.stderr
    } else {
        output.stdout
    };
    Some(String::from_utf8_lossy(&text).trim().to_string())
}

fn linker_version() -> Option<String> {
    for linker in ["cc", "clang", "gcc", "link"] {
        if let Some(version) = command_version(linker, &["--version"]) {
            return Some(version.lines().next().unwrap_or_default().to_string());
        }
    }
    None
}

fn sdk_version() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        return command_version("xcrun", &["--show-sdk-version"]);
    }
    #[cfg(windows)]
    {
        return command_version("cmd", &["/C", "ver"]);
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        None
    }
}

fn build_environment(cargo_target_dir: &Path) -> BTreeMap<String, String> {
    const RECORDED: &[&str] = &[
        "PATH",
        "RUSTUP_TOOLCHAIN",
        "CC",
        "CXX",
        "AR",
        "CFLAGS",
        "CXXFLAGS",
        "LDFLAGS",
        "SDKROOT",
        "MACOSX_DEPLOYMENT_TARGET",
        "CARGO_BUILD_TARGET",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "PKG_CONFIG_PATH",
        "V8_FROM_SOURCE",
    ];
    let mut environment = BTreeMap::new();
    for name in RECORDED {
        if let Some(value) = std::env::var_os(name) {
            environment.insert((*name).to_owned(), value.to_string_lossy().into_owned());
        }
    }
    environment.insert(
        "CARGO_TARGET_DIR".to_owned(),
        cargo_target_dir.display().to_string(),
    );
    environment.insert("CODEX_PATCHER_BUILD".to_owned(), "1".to_owned());
    environment.insert(
        "CODEX_PATCHER_CARGO_PROFILE".to_owned(),
        CARGO_PROFILE.to_owned(),
    );
    environment.insert("RUSTFLAGS".to_owned(), "<removed>".to_owned());
    environment.insert("CARGO_ENCODED_RUSTFLAGS".to_owned(), "<removed>".to_owned());
    environment
}

fn directory_has_entries(path: &Path) -> bool {
    fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_some())
}

fn build_cache_dir(paths: &PatcherPaths, desired: &DesiredBuild) -> PathBuf {
    paths.cargo_target_dir_for(&desired.target, CARGO_PROFILE)
}

fn hash_tree(root: &Path) -> Result<Vec<FileHash>> {
    let mut output = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_dir() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        let mut hasher = Sha256::new();
        if entry.file_type().is_symlink() {
            hasher.update(fs::read_link(entry.path())?.to_string_lossy().as_bytes());
        } else {
            hasher.update(fs::read(entry.path())?);
        }
        output.push(FileHash {
            path: relative,
            sha256: hex::encode(hasher.finalize()),
        });
    }
    output.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    Ok(output)
}

pub fn sanitize_line(line: &str) -> String {
    let mut clean = String::with_capacity(line.len());
    for character in line.chars() {
        if character == '\t' || (!character.is_control() && character != '\u{1b}') {
            clean.push(character);
        }
    }
    let trimmed = clean.trim();
    let mut output: String = trimmed.chars().take(300).collect();
    if trimmed.chars().count() > 300 {
        output.push('…');
    }
    output
}

struct DirectoryCleanup(PathBuf);

impl Drop for DirectoryCleanup {
    fn drop(&mut self) {
        if self.0.exists() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ResolvedSource;

    #[test]
    fn log_lines_drop_terminal_controls_and_are_bounded() {
        let source = format!("\u{1b}[31m{}\n", "x".repeat(400));
        let clean = sanitize_line(&source);
        assert!(!clean.contains('\u{1b}'));
        assert_eq!(clean.chars().count(), 301);
    }

    #[test]
    fn fetch_refspec_does_not_duplicate_the_resolved_tag_namespace() {
        let desired = DesiredBuild {
            source: ResolvedSource {
                channel: "stable".into(),
                ref_name: "refs/tags/rust-v1.2.3".into(),
                ref_object_oid: "a".repeat(40),
                commit_oid: "b".repeat(40),
                version: "1.2.3".into(),
                release_url: None,
            },
            patch_fingerprint: "c".repeat(64),
            target: "x86_64-unknown-linux-musl".into(),
            source_key: "d".repeat(64),
        };
        assert_eq!(
            source_fetch_refspec(&desired, "refs/codex-patcher/source/test"),
            "+refs/tags/rust-v1.2.3:refs/codex-patcher/source/test"
        );
    }

    #[test]
    fn help_subcommands_are_captured_for_dispatch_classification() {
        let help = "Codex CLI\n\nCommands:\n  exec        Run\n  app-server  Serve\n  resume      Resume\n  fork        Fork\n  future-io   New protocol\n  help        Help\n\nArguments:\n  [PROMPT]\n";
        assert_eq!(
            parse_subcommands(help).unwrap(),
            ["app-server", "exec", "fork", "future-io", "help", "resume"]
        );
    }

    #[test]
    fn only_network_build_failures_are_marked_transient() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            temp.path(),
            b"spurious network error: failed to download crate\n",
        )
        .unwrap();
        assert!(BuildFailure::new("build", "builder failed", temp.path()).transient);
        assert!(!BuildFailure::new("patch", "builder failed", temp.path()).transient);

        fs::write(temp.path(), b"error[E0308]: mismatched types\n").unwrap();
        assert!(!BuildFailure::new("build", "builder failed", temp.path()).transient);
    }

    #[test]
    fn package_builder_failures_surface_the_first_useful_diagnostic() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            temp.path(),
            b"error[E0463]: can't find crate for `core`\n\
              = note: the `x86_64-unknown-linux-musl` target may not be installed\n\
              error: could not compile `cfg-if`\n",
        )
        .unwrap();
        assert_eq!(
            package_builder_log_diagnostic("x86_64-unknown-linux-musl", temp.path()).as_deref(),
            Some(
                "Rust target x86_64-unknown-linux-musl is not installed for Codex's pinned toolchain"
            )
        );

        fs::write(
            temp.path(),
            b"warning: noisy\nerror: linking with `cc` failed\nTraceback (most recent call last)\n",
        )
        .unwrap();
        assert_eq!(
            package_builder_log_diagnostic("x86_64-unknown-linux-gnu", temp.path()).as_deref(),
            Some("package builder failed: error: linking with `cc` failed")
        );
    }

    #[test]
    fn compatible_source_keys_reuse_a_warm_incremental_cache() {
        let temp = tempfile::tempdir().unwrap();
        let paths = PatcherPaths::from_home(temp.path());
        let first_desired = DesiredBuild {
            source: ResolvedSource {
                channel: "stable".into(),
                ref_name: "refs/tags/rust-v1.2.3".into(),
                ref_object_oid: "a".repeat(40),
                commit_oid: "b".repeat(40),
                version: "1.2.3".into(),
                release_url: None,
            },
            patch_fingerprint: "c".repeat(64),
            target: "x86_64-unknown-linux-gnu".into(),
            source_key: "d".repeat(64),
        };
        let second_desired = DesiredBuild {
            source_key: "e".repeat(64),
            patch_fingerprint: "f".repeat(64),
            ..first_desired.clone()
        };
        let first = build_cache_dir(&paths, &first_desired);
        let second = build_cache_dir(&paths, &second_desired);
        let first_worktree = build_worktree_dir(&paths, &first_desired).unwrap();
        let second_worktree = build_worktree_dir(&paths, &second_desired).unwrap();

        assert_ne!(first_desired.source_key, second_desired.source_key);
        assert_eq!(first, second);
        assert_eq!(first_worktree, second_worktree);
        assert_eq!(
            first,
            temp.path()
                .join("cache/cargo-target/x86_64-unknown-linux-gnu/dev-small")
        );
        assert!(!directory_has_entries(&first));
        fs::create_dir_all(&first).unwrap();
        fs::write(first.join("compiler-artifact"), b"cached").unwrap();
        assert!(directory_has_entries(&second));
    }

    #[test]
    fn stable_build_worktree_is_reset_and_reused() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let git = |directory: &Path, arguments: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(directory)
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output
        };
        git(&source, &["init"]);
        git(&source, &["config", "user.name", "Test"]);
        git(&source, &["config", "user.email", "test@example.invalid"]);
        fs::write(source.join("tracked.txt"), "base\n").unwrap();
        git(&source, &["add", "tracked.txt"]);
        git(&source, &["commit", "-m", "base"]);
        let commit = String::from_utf8(git(&source, &["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_owned();

        let paths = PatcherPaths::from_home(temp.path().join("patcher"));
        fs::create_dir_all(paths.cache_dir()).unwrap();
        let clone = Command::new("git")
            .args(["clone", "--bare"])
            .arg(&source)
            .arg(paths.mirror_dir())
            .output()
            .unwrap();
        assert!(clone.status.success());
        let worktree = paths.worktrees_dir().join("build-test-dev-small");
        let mut log = File::create(temp.path().join("worktree.log")).unwrap();

        prepare_build_worktree(&paths, &worktree, &commit, &mut log).unwrap();
        let baseline = fs::read(worktree.join("tracked.txt")).unwrap();
        fs::write(worktree.join("tracked.txt"), "patched\n").unwrap();
        fs::write(worktree.join("untracked.txt"), "temporary\n").unwrap();
        prepare_build_worktree(&paths, &worktree, &commit, &mut log).unwrap();

        assert_eq!(fs::read(worktree.join("tracked.txt")).unwrap(), baseline);
        assert!(!worktree.join("untracked.txt").exists());
        assert!(git(&worktree, &["status", "--porcelain"]).stdout.is_empty());
        remove_registered_worktree(&paths, &worktree, &mut log).unwrap();
    }
}
