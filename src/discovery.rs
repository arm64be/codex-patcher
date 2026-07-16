use crate::paths::PatcherPaths;
#[cfg(windows)]
use crate::shim::validate_surface_launcher_type;
use crate::shim::{FileIdentity, inspect_file_identity};
use anyhow::{Result, anyhow, bail};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;
use walkdir::WalkDir;

pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
pub const DEFAULT_MAX_PATH_ENTRIES: usize = 256;
pub const DEFAULT_MAX_SURFACES: usize = 512;
pub const DEFAULT_MAX_VERSION_PROBES: usize = 64;
const MAX_PACKAGE_EXECUTABLES: usize = 32;
const MAX_PACKAGE_SCAN_ENTRIES: usize = 4_096;
const MAX_COMMAND_OUTPUT_BYTES: usize = 64 * 1024;
#[cfg(unix)]
const VERSION_PROBE_BUSY_RETRIES: usize = 4;
#[cfg(unix)]
const MAX_SHELL_RESOLUTIONS: usize = 16;
#[cfg(any(windows, test))]
const MAX_POWERSHELL_RESOLUTIONS: usize = 64;
#[cfg(any(windows, test))]
const POWERSHELL_JSON_BEGIN: &str = "__CODEX_PATCHER_GET_COMMAND_BEGIN__";
#[cfg(any(windows, test))]
const POWERSHELL_JSON_END: &str = "__CODEX_PATCHER_GET_COMMAND_END__";

#[derive(Debug, Clone)]
pub struct DiscoveryOptions {
    pub probe_timeout: Duration,
    pub command_timeout: Duration,
    pub max_path_entries: usize,
    pub max_surfaces: usize,
    pub max_version_probes: usize,
    pub probe_versions: bool,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
            command_timeout: DEFAULT_PROBE_TIMEOUT,
            max_path_entries: DEFAULT_MAX_PATH_ENTRIES,
            max_surfaces: DEFAULT_MAX_SURFACES,
            max_version_probes: DEFAULT_MAX_VERSION_PROBES,
            probe_versions: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SurfaceOwner {
    Patcher,
    Standalone,
    Npm,
    Pnpm,
    Bun,
    Homebrew,
    Desktop,
    Daemon,
    Manual,
    Unknown,
}

impl SurfaceOwner {
    pub fn is_external_manager(self) -> bool {
        matches!(
            self,
            Self::Standalone
                | Self::Npm
                | Self::Pnpm
                | Self::Bun
                | Self::Homebrew
                | Self::Desktop
                | Self::Daemon
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Redirectability {
    AlreadyOwned,
    Direct,
    OwnerManaged,
    NotRedirectable,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum SurfaceRisk {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeStatus {
    NotRun,
    Succeeded,
    TimedOut,
    Failed,
}

/// The bounded command-resolution namespace in which this surface has a
/// precedence. Known absolute locations are still reported, but they are not
/// part of the current shell/process command-resolution order.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PrecedenceScope {
    CliOverride,
    Shell,
    #[serde(rename = "powershell")]
    PowerShell,
    Path,
    KnownLocation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SurfacePrecedence {
    pub scope: PrecedenceScope,
    /// One-based rank within `scope`, when that scope has an ordered resolver.
    pub rank: Option<usize>,
    /// Whether this resolver currently selects the surface. Multiple scopes
    /// can be effective at once (for example PATH and a Desktop CLI override).
    pub effective: bool,
}

impl SurfacePrecedence {
    pub fn label(&self) -> String {
        let scope = match self.scope {
            PrecedenceScope::CliOverride => "cli-override",
            PrecedenceScope::Shell => "shell",
            PrecedenceScope::PowerShell => "powershell",
            PrecedenceScope::Path => "PATH",
            PrecedenceScope::KnownLocation => "known-location",
        };
        let rank = self
            .rank
            .map(|rank| format!("[{rank}]"))
            .unwrap_or_default();
        let effective = if self.effective { "*" } else { "" };
        format!("{scope}{rank}{effective}")
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateMethod {
    CodexPatcher,
    OfficialStandalone,
    NpmGlobal,
    PnpmGlobal,
    BunGlobal,
    HomebrewCask,
    DesktopUpdater,
    Manual,
    Unknown,
}

impl UpdateMethod {
    pub fn label(self) -> &'static str {
        match self {
            Self::CodexPatcher => "codex-patcher",
            Self::OfficialStandalone => "official-standalone",
            Self::NpmGlobal => "npm-global",
            Self::PnpmGlobal => "pnpm-global",
            Self::BunGlobal => "bun-global",
            Self::HomebrewCask => "homebrew-cask",
            Self::DesktopUpdater => "desktop-updater",
            Self::Manual => "manual",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SurfaceFileIdentity {
    /// The resolved executable or package leaf whose identity was inspected.
    pub path: Option<PathBuf>,
    pub identity: Option<FileIdentity>,
    /// Present when the surface has no inspectable file or identity inspection
    /// failed. Discovery remains useful even when one candidate is unreadable.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VersionProbe {
    pub status: ProbeStatus,
    pub version: Option<String>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub elapsed_ms: u128,
}

impl VersionProbe {
    fn not_run() -> Self {
        Self {
            status: ProbeStatus::NotRun,
            version: None,
            output: None,
            error: None,
            elapsed_ms: 0,
        }
    }
}

/// One concrete way a caller can reach Codex. Candidates are deliberately not
/// deduplicated: a PATH shim, a package-manager shim, a Desktop override, and a
/// daemon path remain distinct surfaces even when they resolve to the same
/// executable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SurfaceCandidate {
    pub origin: String,
    pub raw: PathBuf,
    pub resolved: Option<PathBuf>,
    pub version: Option<String>,
    pub owner: SurfaceOwner,
    pub update_method: UpdateMethod,
    pub current: bool,
    pub precedence: SurfacePrecedence,
    pub file_identity: SurfaceFileIdentity,
    pub redirectability: Redirectability,
    pub risk: SurfaceRisk,
    pub risk_reason: String,
    pub exists: bool,
    pub probe: VersionProbe,
}

impl SurfaceCandidate {
    pub fn display_path(&self) -> &Path {
        self.resolved.as_deref().unwrap_or(&self.raw)
    }
}

/// Discover all bounded, known command surfaces.
pub fn discover(paths: &PatcherPaths) -> Result<Vec<SurfaceCandidate>> {
    discover_with_options(paths, &DiscoveryOptions::default())
}

pub fn discover_with_options(
    paths: &PatcherPaths,
    options: &DiscoveryOptions,
) -> Result<Vec<SurfaceCandidate>> {
    let mut seeds = Vec::new();
    let path_dirs: Vec<PathBuf> = env::var_os("PATH")
        .map(|value| {
            env::split_paths(&value)
                .take(options.max_path_entries)
                .collect()
        })
        .unwrap_or_default();

    add_path_surfaces(&mut seeds, &path_dirs);
    add_cli_override(&mut seeds, &path_dirs);
    add_shell_resolution_surfaces(&mut seeds, &path_dirs, options);
    add_standalone_surfaces(&mut seeds);
    add_package_manager_surfaces(&mut seeds, &path_dirs, options);
    add_homebrew_surfaces(&mut seeds, options);
    add_desktop_surfaces(&mut seeds);
    add_patcher_surface(&mut seeds, paths);
    seeds.truncate(options.max_surfaces);

    let first_path_surface = seeds
        .iter()
        .position(|seed| seed.origin.starts_with("PATH[") && is_launchable(&seed.raw));
    let mut probe_cache: HashMap<PathBuf, VersionProbe> = HashMap::new();
    let mut candidates = Vec::with_capacity(seeds.len());

    for (index, seed) in seeds.into_iter().enumerate() {
        let exists = fs::symlink_metadata(&seed.raw).is_ok();
        let resolved = seed
            .probeable
            .then(|| resolve_surface(&seed.raw, &path_dirs))
            .flatten();
        let owner = classify_owner(seed.owner, &seed.raw, resolved.as_deref(), paths);
        let (mut redirectability, mut risk, mut risk_reason) = risk_for(owner, seed.package_leaf);
        if let Some(reason) = seed.read_only_reason {
            redirectability = Redirectability::NotRedirectable;
            risk = SurfaceRisk::Critical;
            risk_reason = reason;
        } else if is_protected_bundle_path(&seed.raw, resolved.as_deref()) {
            redirectability = Redirectability::NotRedirectable;
            risk = SurfaceRisk::Critical;
            risk_reason =
                "signed or protected application-bundle internals cannot be replaced safely"
                    .to_string();
        }
        #[cfg(windows)]
        if let Err(error) = validate_surface_launcher_type(&seed.raw) {
            redirectability = Redirectability::NotRedirectable;
            risk = SurfaceRisk::Critical;
            risk_reason = error.to_string();
        }
        let current = seed.current || first_path_surface == Some(index);
        let probe_path = resolved.as_deref().or(exists.then_some(seed.raw.as_path()));
        let probe = if !seed.probeable {
            VersionProbe::not_run()
        } else if options.probe_versions {
            if let Some(probe_path) = probe_path {
                if let Some(cached) = probe_cache.get(probe_path) {
                    cached.clone()
                } else if probe_cache.len() < options.max_version_probes {
                    let probe = probe_version_detailed(probe_path, options.probe_timeout);
                    probe_cache.insert(probe_path.to_path_buf(), probe.clone());
                    probe
                } else {
                    VersionProbe::not_run()
                }
            } else {
                VersionProbe {
                    status: ProbeStatus::Failed,
                    version: None,
                    output: None,
                    error: Some("surface does not resolve to a file".to_string()),
                    elapsed_ms: 0,
                }
            }
        } else {
            VersionProbe::not_run()
        };
        let precedence = surface_precedence(&seed.origin, current);
        let file_identity =
            surface_file_identity(seed.probeable, &seed.raw, resolved.as_deref(), exists);

        candidates.push(SurfaceCandidate {
            origin: seed.origin,
            raw: seed.raw,
            resolved,
            version: probe.version.clone(),
            owner,
            update_method: update_method_for(owner),
            current,
            precedence,
            file_identity,
            redirectability,
            risk,
            risk_reason,
            exists,
            probe,
        });
    }

    Ok(candidates)
}

fn update_method_for(owner: SurfaceOwner) -> UpdateMethod {
    match owner {
        SurfaceOwner::Patcher => UpdateMethod::CodexPatcher,
        SurfaceOwner::Standalone | SurfaceOwner::Daemon => UpdateMethod::OfficialStandalone,
        SurfaceOwner::Npm => UpdateMethod::NpmGlobal,
        SurfaceOwner::Pnpm => UpdateMethod::PnpmGlobal,
        SurfaceOwner::Bun => UpdateMethod::BunGlobal,
        SurfaceOwner::Homebrew => UpdateMethod::HomebrewCask,
        SurfaceOwner::Desktop => UpdateMethod::DesktopUpdater,
        SurfaceOwner::Manual => UpdateMethod::Manual,
        SurfaceOwner::Unknown => UpdateMethod::Unknown,
    }
}

fn surface_precedence(origin: &str, effective: bool) -> SurfacePrecedence {
    let (scope, rank) = if origin == "CODEX_CLI_PATH" {
        (PrecedenceScope::CliOverride, Some(1))
    } else if origin.starts_with("PATH[") {
        (
            PrecedenceScope::Path,
            bracketed_rank(origin).map(|rank| rank + 1),
        )
    } else if origin.starts_with("shell-") {
        (
            PrecedenceScope::Shell,
            bracketed_rank(origin).map(|rank| rank + 1),
        )
    } else if origin.starts_with("powershell-") {
        (
            PrecedenceScope::PowerShell,
            bracketed_rank(origin).map(|rank| rank + 1),
        )
    } else {
        (PrecedenceScope::KnownLocation, None)
    };
    SurfacePrecedence {
        scope,
        rank,
        effective,
    }
}

fn bracketed_rank(origin: &str) -> Option<usize> {
    let (_, tail) = origin.split_once('[')?;
    let (rank, _) = tail.split_once(']')?;
    rank.parse().ok()
}

fn surface_file_identity(
    probeable: bool,
    raw: &Path,
    resolved: Option<&Path>,
    exists: bool,
) -> SurfaceFileIdentity {
    if !probeable {
        return SurfaceFileIdentity {
            path: None,
            identity: None,
            error: Some("virtual command surface has no executable file".to_string()),
        };
    }

    let Some(path) = resolved.or(exists.then_some(raw)) else {
        return SurfaceFileIdentity {
            path: Some(raw.to_path_buf()),
            identity: None,
            error: Some("surface does not resolve to a file".to_string()),
        };
    };
    match inspect_file_identity(path) {
        Ok(identity) => SurfaceFileIdentity {
            path: Some(path.to_path_buf()),
            identity: Some(identity),
            error: None,
        },
        Err(error) => SurfaceFileIdentity {
            path: Some(path.to_path_buf()),
            identity: None,
            error: Some(error.to_string()),
        },
    }
}

#[derive(Debug)]
struct SurfaceSeed {
    origin: String,
    raw: PathBuf,
    owner: SurfaceOwner,
    current: bool,
    package_leaf: bool,
    probeable: bool,
    read_only_reason: Option<String>,
}

impl SurfaceSeed {
    fn physical(
        origin: impl Into<String>,
        raw: PathBuf,
        owner: SurfaceOwner,
        package_leaf: bool,
    ) -> Self {
        Self {
            origin: origin.into(),
            raw,
            owner,
            current: false,
            package_leaf,
            probeable: true,
            read_only_reason: None,
        }
    }
}

fn push_if_present(
    seeds: &mut Vec<SurfaceSeed>,
    origin: impl Into<String>,
    path: PathBuf,
    owner: SurfaceOwner,
    package_leaf: bool,
) -> bool {
    if fs::symlink_metadata(&path).is_ok() {
        seeds.push(SurfaceSeed::physical(origin, path, owner, package_leaf));
        true
    } else {
        false
    }
}

fn add_path_surfaces(seeds: &mut Vec<SurfaceSeed>, path_dirs: &[PathBuf]) {
    for (index, directory) in path_dirs.iter().enumerate() {
        for name in command_file_names() {
            let path = directory.join(&name);
            push_if_present(
                seeds,
                format!("PATH[{index}]"),
                path,
                SurfaceOwner::Unknown,
                false,
            );
        }
    }
}

fn add_cli_override(seeds: &mut Vec<SurfaceSeed>, path_dirs: &[PathBuf]) {
    let Some(raw) = env::var_os("CODEX_CLI_PATH") else {
        return;
    };
    if raw.is_empty() {
        return;
    }

    let configured = PathBuf::from(raw);
    let candidate = if configured.is_dir() {
        command_file_names()
            .into_iter()
            .map(|name| configured.join(name))
            .find(|path| fs::symlink_metadata(path).is_ok())
            .unwrap_or_else(|| configured.join(primary_command_file_name()))
    } else if is_bare_command(&configured) {
        find_on_path(&configured, path_dirs).unwrap_or(configured)
    } else {
        configured
    };

    // Keep an invalid override visible: it is actionable information and is a
    // command surface even though probing it will fail.
    let mut seed = SurfaceSeed::physical("CODEX_CLI_PATH", candidate, SurfaceOwner::Unknown, false);
    seed.current = true;
    seeds.push(seed);
}

fn add_shell_resolution_surfaces(
    seeds: &mut Vec<SurfaceSeed>,
    path_dirs: &[PathBuf],
    options: &DiscoveryOptions,
) {
    #[cfg(unix)]
    {
        let _ = path_dirs;
        let Some(shell) = env::var_os("SHELL").map(PathBuf::from) else {
            return;
        };
        add_unix_shell_resolution_surfaces_for_shell(seeds, &shell, options.command_timeout);
    }

    #[cfg(windows)]
    add_powershell_resolution_surfaces(seeds, path_dirs, options.command_timeout);
}

fn push_virtual_read_only(
    seeds: &mut Vec<SurfaceSeed>,
    origin: impl Into<String>,
    display: impl Into<String>,
    owner: SurfaceOwner,
    current: bool,
    reason: impl Into<String>,
) {
    seeds.push(SurfaceSeed {
        origin: origin.into(),
        raw: PathBuf::from(display.into()),
        owner,
        current,
        package_leaf: false,
        probeable: false,
        read_only_reason: Some(reason.into()),
    });
}

#[cfg(unix)]
fn add_unix_shell_resolution_surfaces_for_shell(
    seeds: &mut Vec<SurfaceSeed>,
    shell: &Path,
    timeout: Duration,
) {
    use std::os::unix::process::CommandExt;

    if !is_launchable(shell) {
        return;
    }
    let Some(shell_name) = shell.file_name().and_then(OsStr::to_str) else {
        return;
    };
    let shell_name = shell_name.to_ascii_lowercase();
    let resolution_command = match shell_name.as_str() {
        "bash" => "type -a -- codex",
        "zsh" => "whence -va -- codex",
        "fish" => "type -a codex",
        "ksh" | "ksh93" => "whence -va codex",
        _ => return,
    };

    let mut command = Command::new(shell);
    // Alias and function state normally lives in interactive startup files. We
    // request those startup semantics, but the resolver itself is command-only:
    // it has no stdin, no controlling terminal, a hard deadline, and capped
    // output. A startup file therefore cannot turn discovery into a prompt.
    match shell_name.as_str() {
        "fish" => {
            command.args(["--interactive", "--command", resolution_command]);
        }
        _ => {
            command.args(["-i", "-c", resolution_command]);
        }
    }
    command
        .env_remove("BASH_ENV")
        .env_remove("ENV")
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0");
    // `setsid` prevents shell startup code from reopening /dev/tty to ask a
    // question after stdin/stdout/stderr have been redirected.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let Some(output) =
        bounded_command_output_inner(command, timeout, MAX_COMMAND_OUTPUT_BYTES, true)
    else {
        return;
    };
    add_parsed_shell_resolutions(seeds, &shell_name, &output);
}

#[cfg(unix)]
fn add_parsed_shell_resolutions(seeds: &mut Vec<SurfaceSeed>, shell_name: &str, output: &str) {
    let mut emitted = 0;
    for line in output.lines() {
        if emitted >= MAX_SHELL_RESOLUTIONS {
            break;
        }
        let detail = compact_display_text(line, 512);
        if detail.is_empty() {
            continue;
        }
        let lower = detail.to_ascii_lowercase();
        if !lower.starts_with("codex ") && !lower.starts_with("codex:") {
            continue;
        }
        let kind = if lower.contains("alias") {
            "alias"
        } else if lower.contains("function") {
            "function"
        } else {
            // Executable path results are already represented separately by
            // the bounded PATH scan.
            continue;
        };
        push_virtual_read_only(
            seeds,
            format!("shell-{shell_name}[{emitted}]-{kind}"),
            format!("{shell_name} {kind}: {detail}"),
            SurfaceOwner::Manual,
            emitted == 0,
            format!("{shell_name} {kind}s are shell state and cannot be replaced as files"),
        );
        emitted += 1;
    }
}

#[cfg(any(windows, test))]
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
struct PowerShellResolution {
    name: String,
    command_type: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    definition: String,
}

#[cfg(any(windows, test))]
fn parse_powershell_resolutions(output: &str) -> Vec<PowerShellResolution> {
    let payload = output
        .split_once(POWERSHELL_JSON_BEGIN)
        .and_then(|(_, rest)| rest.rsplit_once(POWERSHELL_JSON_END))
        .map(|(payload, _)| payload.trim())
        .unwrap_or_else(|| output.trim());
    serde_json::from_str::<Vec<PowerShellResolution>>(payload)
        .unwrap_or_default()
        .into_iter()
        .take(MAX_POWERSHELL_RESOLUTIONS)
        .collect()
}

#[cfg(windows)]
fn add_powershell_resolution_surfaces(
    seeds: &mut Vec<SurfaceSeed>,
    path_dirs: &[PathBuf],
    timeout: Duration,
) {
    const SCRIPT: &str = r#"$ErrorActionPreference='SilentlyContinue'; $ProgressPreference='SilentlyContinue'; $items=@(Microsoft.PowerShell.Core\Get-Command -All -Name codex -ErrorAction SilentlyContinue | Microsoft.PowerShell.Utility\Select-Object -First 64 | Microsoft.PowerShell.Core\ForEach-Object { [pscustomobject]@{ Name=[string]$_.Name; CommandType=[string]$_.CommandType; Path=[string]$_.Path; Source=[string]$_.Source; Definition=[string]$_.Definition } }); [Console]::Out.WriteLine('__CODEX_PATCHER_GET_COMMAND_BEGIN__'); Microsoft.PowerShell.Utility\ConvertTo-Json -Compress -Depth 2 -InputObject $items; [Console]::Out.WriteLine('__CODEX_PATCHER_GET_COMMAND_END__')"#;

    for program in ["pwsh.exe", "powershell.exe"] {
        let Some(executable) = find_on_path(Path::new(program), path_dirs) else {
            continue;
        };
        let mut command = Command::new(executable);
        command
            .args(["-NoLogo", "-NonInteractive", "-Command", SCRIPT])
            .env("POWERSHELL_TELEMETRY_OPTOUT", "1")
            .env("NO_COLOR", "1");
        let Some(output) = bounded_command_output(command, timeout, MAX_COMMAND_OUTPUT_BYTES)
        else {
            continue;
        };

        for (index, resolution) in parse_powershell_resolutions(&output)
            .into_iter()
            .enumerate()
        {
            let kind = resolution.command_type.to_ascii_lowercase();
            let physical_path = (!resolution.path.trim().is_empty())
                .then(|| PathBuf::from(resolution.path.trim()))
                .or_else(|| {
                    let definition = PathBuf::from(resolution.definition.trim());
                    (definition.is_absolute() && fs::symlink_metadata(&definition).is_ok())
                        .then_some(definition)
                });
            if matches!(kind.as_str(), "application" | "externalscript")
                && let Some(path) = physical_path
            {
                if push_if_present(
                    seeds,
                    format!("powershell-{program}[{index}]-{kind}"),
                    path,
                    SurfaceOwner::Unknown,
                    false,
                ) && index == 0
                    && let Some(seed) = seeds.last_mut()
                {
                    seed.current = true;
                }
                continue;
            }

            let definition = if resolution.definition.trim().is_empty() {
                resolution.source.trim()
            } else {
                resolution.definition.trim()
            };
            let detail = compact_display_text(definition, 512);
            push_virtual_read_only(
                seeds,
                format!("powershell-{program}[{index}]-{kind}"),
                format!(
                    "PowerShell {} {}{}",
                    resolution.command_type,
                    resolution.name,
                    (!detail.is_empty())
                        .then(|| format!(" -> {detail}"))
                        .unwrap_or_default()
                ),
                SurfaceOwner::Manual,
                index == 0,
                format!(
                    "PowerShell {} resolution is session state and cannot be replaced as a file",
                    resolution.command_type
                ),
            );
        }
    }
}

fn compact_display_text(input: &str, max_chars: usize) -> String {
    let mut output = String::new();
    let mut last_was_space = false;
    for character in input.chars().take(max_chars) {
        let character = if character.is_control() || character.is_whitespace() {
            ' '
        } else {
            character
        };
        if character == ' ' {
            if last_was_space {
                continue;
            }
            last_was_space = true;
        } else {
            last_was_space = false;
        }
        output.push(character);
    }
    output.trim().to_string()
}

fn add_standalone_surfaces(seeds: &mut Vec<SurfaceSeed>) {
    let Some(base) = BaseDirs::new() else {
        return;
    };
    let codex_home = env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| base.home_dir().join(".codex"));
    let current = codex_home
        .join("packages")
        .join("standalone")
        .join("current");

    for (origin, path, owner, package_leaf) in [
        (
            "standalone-package",
            current.join("bin").join(primary_native_file_name()),
            SurfaceOwner::Standalone,
            true,
        ),
        (
            "standalone-daemon",
            current.join(primary_native_file_name()),
            SurfaceOwner::Daemon,
            false,
        ),
    ] {
        push_if_present(seeds, origin, path, owner, package_leaf);
    }

    if let Some(install_dir) = env::var_os("CODEX_INSTALL_DIR") {
        for name in command_file_names() {
            push_if_present(
                seeds,
                "CODEX_INSTALL_DIR",
                PathBuf::from(&install_dir).join(name),
                SurfaceOwner::Standalone,
                false,
            );
        }
    } else {
        #[cfg(not(windows))]
        push_if_present(
            seeds,
            "standalone-visible-default",
            base.home_dir().join(".local/bin/codex"),
            SurfaceOwner::Standalone,
            false,
        );

        #[cfg(windows)]
        if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
            push_if_present(
                seeds,
                "standalone-visible-default",
                PathBuf::from(local_app_data).join("Programs/OpenAI/Codex/bin/codex.exe"),
                SurfaceOwner::Standalone,
                false,
            );
        }
    }
}

fn add_package_manager_surfaces(
    seeds: &mut Vec<SurfaceSeed>,
    path_dirs: &[PathBuf],
    options: &DiscoveryOptions,
) {
    for (program, args, owner) in [
        ("npm", &["prefix", "-g"][..], SurfaceOwner::Npm),
        ("pnpm", &["bin", "-g"][..], SurfaceOwner::Pnpm),
        ("bun", &["pm", "bin", "-g"][..], SurfaceOwner::Bun),
    ] {
        add_manager_command_surfaces(
            seeds,
            path_dirs,
            program,
            args,
            owner,
            options.command_timeout,
        );
    }

    if let Some(base) = BaseDirs::new() {
        let bun_bin = base.home_dir().join(".bun/bin");
        for name in command_file_names() {
            push_if_present(
                seeds,
                "bun-default",
                bun_bin.join(name),
                SurfaceOwner::Bun,
                false,
            );
        }
    }

    // Manager roots expose the native leaf that their visible JS shim launches.
    // These are reported for identity/version diagnostics, never as safe
    // redirect targets.
    for (program, args, owner) in [
        ("npm", &["root", "-g"][..], SurfaceOwner::Npm),
        ("pnpm", &["root", "-g"][..], SurfaceOwner::Pnpm),
    ] {
        if let Some(root) = command_output(program, args, options.command_timeout) {
            add_package_leaves(seeds, Path::new(root.trim()), owner, program);
        }
    }
}

fn add_manager_command_surfaces(
    seeds: &mut Vec<SurfaceSeed>,
    path_dirs: &[PathBuf],
    program: &str,
    args: &[&str],
    owner: SurfaceOwner,
    timeout: Duration,
) {
    if find_on_path(Path::new(program), path_dirs).is_none() {
        return;
    }
    let Some(output) = command_output(program, args, timeout) else {
        return;
    };
    let bin_dir = PathBuf::from(output.trim());
    if bin_dir.as_os_str().is_empty() {
        return;
    }

    #[cfg(not(windows))]
    let bin_dir = if program == "npm" {
        bin_dir.join("bin")
    } else {
        bin_dir
    };

    for name in command_file_names() {
        push_if_present(
            seeds,
            format!("{program}-global"),
            bin_dir.join(name),
            owner,
            false,
        );
    }
}

fn add_package_leaves(
    seeds: &mut Vec<SurfaceSeed>,
    root: &Path,
    owner: SurfaceOwner,
    manager: &str,
) {
    if !root.is_dir() {
        return;
    }
    let mut count = 0;
    for entry in WalkDir::new(root)
        .follow_links(false)
        .max_depth(9)
        .into_iter()
        .take(MAX_PACKAGE_SCAN_ENTRIES)
        .filter_map(|entry| entry.ok())
    {
        if count >= MAX_PACKAGE_EXECUTABLES {
            break;
        }
        if !entry.file_type().is_file()
            || entry.file_name() != OsStr::new(primary_native_file_name())
        {
            continue;
        }
        let path_text = entry.path().to_string_lossy().to_ascii_lowercase();
        if path_text.contains("@openai") && path_text.contains("codex") {
            seeds.push(SurfaceSeed::physical(
                format!("{manager}-native-package"),
                entry.into_path(),
                owner,
                true,
            ));
            count += 1;
        }
    }
}

fn add_homebrew_surfaces(seeds: &mut Vec<SurfaceSeed>, options: &DiscoveryOptions) {
    let Some(prefix) = command_output("brew", &["--prefix"], options.command_timeout) else {
        return;
    };
    let prefix = PathBuf::from(prefix.trim());
    push_if_present(
        seeds,
        "homebrew-cask",
        prefix.join("bin/codex"),
        SurfaceOwner::Homebrew,
        false,
    );

    let caskroom = prefix.join("Caskroom/codex");
    if let Ok(versions) = fs::read_dir(caskroom) {
        for version in versions.flatten().take(16) {
            if let Ok(files) = fs::read_dir(version.path()) {
                for file in files.flatten().take(16) {
                    if file.file_name().to_string_lossy().starts_with("codex-") {
                        push_if_present(
                            seeds,
                            "homebrew-cask-package",
                            file.path(),
                            SurfaceOwner::Homebrew,
                            true,
                        );
                    }
                }
            }
        }
    }
}

fn add_desktop_surfaces(seeds: &mut Vec<SurfaceSeed>) {
    let mut candidates: Vec<PathBuf> = Vec::new();

    #[cfg(target_os = "macos")]
    {
        candidates.extend([
            PathBuf::from("/Applications/Codex.app/Contents/Resources/codex"),
            PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex"),
        ]);
        if let Some(base) = BaseDirs::new() {
            candidates.push(
                base.home_dir()
                    .join("Applications/Codex.app/Contents/Resources/codex"),
            );
        }
    }

    #[cfg(target_os = "linux")]
    candidates.extend([
        PathBuf::from("/opt/codex-desktop/resources/codex"),
        PathBuf::from("/opt/Codex/resources/codex"),
        PathBuf::from("/usr/lib/codex-desktop/resources/codex"),
        PathBuf::from("/usr/lib/codex/resources/codex"),
        PathBuf::from("/usr/lib/codex-app/resources/codex"),
    ]);

    #[cfg(windows)]
    {
        for base in [env::var_os("LOCALAPPDATA"), env::var_os("ProgramFiles")]
            .into_iter()
            .flatten()
        {
            let base = PathBuf::from(base);
            candidates.extend([
                base.join("Programs/Codex/resources/codex.exe"),
                base.join("OpenAI/Codex/resources/codex.exe"),
            ]);
        }
    }

    if let Some(appdir) = env::var_os("APPDIR") {
        candidates.extend([
            PathBuf::from(&appdir).join("resources/codex"),
            PathBuf::from(appdir).join("usr/bin/codex"),
        ]);
    }

    for path in candidates {
        push_if_present(seeds, "desktop-bundle", path, SurfaceOwner::Desktop, true);
    }
}

fn add_patcher_surface(seeds: &mut Vec<SurfaceSeed>, paths: &PatcherPaths) {
    push_if_present(
        seeds,
        "codex-patcher-manager",
        paths.manager.clone(),
        SurfaceOwner::Patcher,
        false,
    );
}

fn classify_owner(
    hinted: SurfaceOwner,
    raw: &Path,
    resolved: Option<&Path>,
    paths: &PatcherPaths,
) -> SurfaceOwner {
    if hinted != SurfaceOwner::Unknown {
        return hinted;
    }

    for path in [Some(raw), resolved].into_iter().flatten() {
        if path.starts_with(&paths.home) {
            return SurfaceOwner::Patcher;
        }
        let lower = path
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase();
        if lower.contains("/.codex/packages/standalone/") {
            return SurfaceOwner::Standalone;
        }
        if lower.contains(".bun/") || lower.contains("/bun/install/global/") {
            return SurfaceOwner::Bun;
        }
        if lower.contains("/.pnpm/") || lower.contains("/pnpm/") {
            return SurfaceOwner::Pnpm;
        }
        if lower.contains("node_modules/@openai/codex") || lower.contains("/npm/") {
            return SurfaceOwner::Npm;
        }
        if lower.contains("/caskroom/codex/") || lower.contains("/cellar/codex/") {
            return SurfaceOwner::Homebrew;
        }
        if lower.contains("codex.app/contents/resources/")
            || lower.contains("codex-desktop/resources/")
        {
            return SurfaceOwner::Desktop;
        }
    }
    SurfaceOwner::Manual
}

fn risk_for(owner: SurfaceOwner, package_leaf: bool) -> (Redirectability, SurfaceRisk, String) {
    if package_leaf {
        return (
            Redirectability::OwnerManaged,
            SurfaceRisk::Critical,
            "package-manager-owned native command surface; updates replace it and may affect every launcher for that package"
                .to_string(),
        );
    }
    match owner {
        SurfaceOwner::Patcher => (
            Redirectability::AlreadyOwned,
            SurfaceRisk::Low,
            "patcher-owned surface".to_string(),
        ),
        SurfaceOwner::Manual | SurfaceOwner::Unknown => (
            Redirectability::Direct,
            SurfaceRisk::Medium,
            "ownership is not externally verifiable; retain an exact baseline".to_string(),
        ),
        SurfaceOwner::Desktop => (
            Redirectability::OwnerManaged,
            SurfaceRisk::High,
            "Desktop updater owns this command surface and may overwrite a redirect".to_string(),
        ),
        SurfaceOwner::Daemon => (
            Redirectability::OwnerManaged,
            SurfaceRisk::Critical,
            "daemon path is hard-coded under the upstream standalone tree and auto-updated"
                .to_string(),
        ),
        owner => (
            Redirectability::OwnerManaged,
            SurfaceRisk::High,
            format!("{owner:?} owns this surface and may overwrite a redirect"),
        ),
    }
}

fn is_protected_bundle_path(raw: &Path, resolved: Option<&Path>) -> bool {
    [Some(raw), resolved].into_iter().flatten().any(|path| {
        let lower = path
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase();
        lower.contains(".app/contents/")
            || lower.contains("/windowsapps/")
            || lower.contains("/appx/")
    })
}

fn resolve_surface(raw: &Path, path_dirs: &[PathBuf]) -> Option<PathBuf> {
    let path = if is_bare_command(raw) {
        find_on_path(raw, path_dirs)?
    } else {
        raw.to_path_buf()
    };
    fs::canonicalize(&path).ok().or(Some(path))
}

fn find_on_path(command: &Path, path_dirs: &[PathBuf]) -> Option<PathBuf> {
    if !is_bare_command(command) {
        return fs::symlink_metadata(command)
            .ok()
            .map(|_| command.to_path_buf());
    }
    let name = command.as_os_str().to_string_lossy();
    for directory in path_dirs {
        for candidate_name in command_names_for(&name) {
            let candidate = directory.join(candidate_name);
            if is_launchable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_bare_command(path: &Path) -> bool {
    path.components().count() == 1 && !path.is_absolute()
}

fn is_launchable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn command_file_names() -> Vec<String> {
    #[cfg(not(windows))]
    {
        vec!["codex".to_string()]
    }
    #[cfg(windows)]
    {
        command_names_for("codex")
    }
}

fn command_names_for(stem: &str) -> Vec<String> {
    #[cfg(not(windows))]
    {
        vec![stem.to_string()]
    }
    #[cfg(windows)]
    {
        let mut names = Vec::new();
        if Path::new(stem).extension().is_some() {
            names.push(stem.to_string());
            return names;
        }
        let extensions =
            env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD;.PS1".to_string());
        for extension in extensions.split(';').filter(|value| !value.is_empty()) {
            let name = format!("{stem}{}", extension.to_ascii_lowercase());
            if !names.contains(&name) {
                names.push(name);
            }
        }
        if !names.iter().any(|name| name == stem) {
            names.push(stem.to_string());
        }
        names
    }
}

const fn primary_command_file_name() -> &'static str {
    if cfg!(windows) { "codex.exe" } else { "codex" }
}

const fn primary_native_file_name() -> &'static str {
    primary_command_file_name()
}

pub fn probe_version(path: &Path, timeout: Duration) -> Result<Option<String>> {
    let probe = probe_version_detailed(path, timeout);
    match probe.status {
        ProbeStatus::Succeeded => Ok(probe.version),
        ProbeStatus::TimedOut => bail!("version probe timed out for {}", path.display()),
        ProbeStatus::Failed => Err(anyhow!(
            "version probe failed for {}: {}",
            path.display(),
            probe.error.unwrap_or_else(|| "unknown error".to_string())
        )),
        ProbeStatus::NotRun => Ok(None),
    }
}

pub fn probe_version_detailed(path: &Path, timeout: Duration) -> VersionProbe {
    let started = Instant::now();
    let mut command = version_command(path);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CODEX_PATCHER_DISCOVERY", "1");
    #[cfg(unix)]
    isolate_command_session(&mut command);
    let child = spawn_version_command(&mut command);
    let mut child = match child {
        Ok(child) => child,
        Err(error) => return failed_probe(started, ProbeStatus::Failed, error.to_string()),
    };

    let status = match child.wait_timeout(timeout) {
        Ok(Some(status)) => status,
        Ok(None) => {
            terminate_bounded_child(&mut child, cfg!(unix));
            return failed_probe(
                started,
                ProbeStatus::TimedOut,
                format!("exceeded {} ms", timeout.as_millis()),
            );
        }
        Err(error) => {
            terminate_bounded_child(&mut child, cfg!(unix));
            return failed_probe(started, ProbeStatus::Failed, error.to_string());
        }
    };

    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(error) => return failed_probe(started, ProbeStatus::Failed, error.to_string()),
    };
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let combined = if !stdout.is_empty() { stdout } else { stderr };

    if !status.success() {
        return VersionProbe {
            status: ProbeStatus::Failed,
            version: None,
            output: (!combined.is_empty()).then_some(combined),
            error: Some(format!("exit status {status}")),
            elapsed_ms: started.elapsed().as_millis(),
        };
    }

    let version = parse_codex_version(&combined);
    VersionProbe {
        status: if version.is_some() {
            ProbeStatus::Succeeded
        } else {
            ProbeStatus::Failed
        },
        version,
        output: (!combined.is_empty()).then_some(combined.clone()),
        error: parse_codex_version(&combined)
            .is_none()
            .then_some("unrecognized version output".to_string()),
        elapsed_ms: started.elapsed().as_millis(),
    }
}

#[cfg(unix)]
fn spawn_version_command(command: &mut Command) -> std::io::Result<std::process::Child> {
    for retry in 0..=VERSION_PROBE_BUSY_RETRIES {
        match command.spawn() {
            Err(error)
                if error.raw_os_error() == Some(libc::ETXTBSY)
                    && retry < VERSION_PROBE_BUSY_RETRIES =>
            {
                // A concurrent fork can briefly inherit the writer for a
                // freshly replaced launcher until that child reaches exec.
                // Linux reports ETXTBSY during that window.
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result,
        }
    }
    unreachable!("the bounded retry loop always returns")
}

#[cfg(not(unix))]
fn spawn_version_command(command: &mut Command) -> std::io::Result<std::process::Child> {
    command.spawn()
}

#[cfg(unix)]
fn isolate_command_session(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // A version launcher may delegate to another process. Isolating it lets a
    // timed-out probe terminate that whole process group instead of leaking a
    // child that keeps the launcher or output pipes open.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn failed_probe(started: Instant, status: ProbeStatus, error: String) -> VersionProbe {
    VersionProbe {
        status,
        version: None,
        output: None,
        error: Some(error),
        elapsed_ms: started.elapsed().as_millis(),
    }
}

fn parse_codex_version(output: &str) -> Option<String> {
    let regex =
        regex::Regex::new(r"(?m)(?:^|\s)([0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?)\s*$")
            .expect("version regex is valid");
    regex
        .captures(output.trim())
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_string())
}

fn version_command(path: &Path) -> Command {
    #[cfg(windows)]
    {
        let extension = path
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if extension == "cmd" || extension == "bat" {
            let mut command = Command::new("cmd.exe");
            command.args(["/D", "/C"]).arg(path).arg("--version");
            return command;
        }
        if extension == "ps1" {
            let mut command = Command::new("powershell.exe");
            command
                .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
                .arg(path)
                .arg("--version");
            return command;
        }
    }

    let mut command = Command::new(path);
    command.arg("--version");
    command
}

fn command_output(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    bounded_command_output(
        manager_command(program, args),
        timeout,
        MAX_COMMAND_OUTPUT_BYTES,
    )
}

fn bounded_command_output(command: Command, timeout: Duration, max_bytes: usize) -> Option<String> {
    bounded_command_output_inner(command, timeout, max_bytes, false)
}

fn bounded_command_output_inner(
    mut command: Command,
    timeout: Duration,
    max_bytes: usize,
    kill_session_on_timeout: bool,
) -> Option<String> {
    let mut output_file = tempfile::tempfile().ok()?;
    let child_output = output_file.try_clone().ok()?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(child_output))
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let status = match child.wait_timeout(timeout) {
        Ok(Some(status)) => status,
        Ok(None) | Err(_) => {
            terminate_bounded_child(&mut child, kill_session_on_timeout);
            return None;
        }
    };
    if !status.success() {
        return None;
    }
    output_file.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::with_capacity(max_bytes.min(8 * 1024) + 1);
    output_file
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > max_bytes {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).trim().to_string()).filter(|output| !output.is_empty())
}

fn terminate_bounded_child(child: &mut std::process::Child, kill_session: bool) {
    #[cfg(unix)]
    if kill_session {
        // The shell resolver calls setsid before exec, so its PID is also its
        // process-group ID. Kill startup-file descendants with the shell.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = kill_session;

    let _ = child.kill();
    let _ = child.wait();
}

fn manager_command(program: &str, args: &[&str]) -> Command {
    #[cfg(windows)]
    {
        let path_dirs: Vec<PathBuf> = env::var_os("PATH")
            .map(|value| {
                env::split_paths(&value)
                    .take(DEFAULT_MAX_PATH_ENTRIES)
                    .collect()
            })
            .unwrap_or_default();
        if let Some(path) = find_on_path(Path::new(program), &path_dirs) {
            let extension = path
                .extension()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            if extension == "cmd" || extension == "bat" {
                let mut command = Command::new("cmd.exe");
                command.args(["/D", "/C"]).arg(path).args(args);
                return command;
            }
            if extension == "ps1" {
                let mut command = Command::new("powershell.exe");
                command
                    .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
                    .arg(path)
                    .args(args);
                return command;
            }
            let mut command = Command::new(path);
            command.args(args);
            return command;
        }
    }

    let mut command = Command::new(program);
    command.args(args);
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::io::Write;

    #[cfg(unix)]
    fn executable(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let mut file = fs::File::create(path).unwrap();
        file.write_all(body.as_bytes()).unwrap();
        let mut permissions = file.metadata().unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_parses_codex_and_kills_a_hung_candidate() {
        let temp = tempfile::tempdir().unwrap();
        let codex = temp.path().join("codex");
        executable(&codex, "#!/bin/sh\nprintf 'codex-cli 1.2.3\\n'\n");
        let probe = probe_version_detailed(&codex, Duration::from_secs(1));
        assert_eq!(
            (probe.status, probe.version.as_deref()),
            (ProbeStatus::Succeeded, Some("1.2.3")),
            "{probe:#?}"
        );
        assert_eq!(parse_codex_version("noise"), None);

        executable(&codex, "#!/bin/sh\nsleep 5\n");
        let probe = probe_version_detailed(&codex, Duration::from_millis(30));
        assert_eq!(probe.status, ProbeStatus::TimedOut, "{probe:#?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn version_probe_retries_a_transient_busy_executable() {
        let temp = tempfile::tempdir().unwrap();
        let codex = temp.path().join("codex");
        executable(&codex, "#!/bin/sh\nprintf 'codex-cli 1.2.3\\n'\n");
        let writer = fs::OpenOptions::new().write(true).open(&codex).unwrap();
        let release_writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(15));
            drop(writer);
        });

        let probe = probe_version_detailed(&codex, Duration::from_secs(1));
        release_writer.join().unwrap();
        assert_eq!(
            (probe.status, probe.version.as_deref()),
            (ProbeStatus::Succeeded, Some("1.2.3")),
            "{probe:#?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn path_surfaces_are_not_deduplicated_after_resolution() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let one = temp.path().join("one");
        let two = temp.path().join("two");
        fs::create_dir_all(&one).unwrap();
        fs::create_dir_all(&two).unwrap();
        let real = temp.path().join("real-codex");
        executable(&real, "#!/bin/sh\nprintf 'codex-cli 9.8.7\\n'\n");
        symlink(&real, one.join("codex")).unwrap();
        symlink(&real, two.join("codex")).unwrap();

        let mut seeds = Vec::new();
        add_path_surfaces(&mut seeds, &[one, two]);
        let resolved: Vec<_> = seeds
            .iter()
            .map(|seed| fs::canonicalize(&seed.raw).unwrap())
            .collect();
        assert_eq!(seeds.len(), 2);
        assert_eq!(resolved[0], resolved[1]);
        assert_ne!(seeds[0].raw, seeds[1].raw);
    }

    #[test]
    fn managed_and_protected_surfaces_keep_their_redirectability() {
        assert_eq!(
            update_method_for(SurfaceOwner::Daemon),
            UpdateMethod::OfficialStandalone
        );
        let package = risk_for(SurfaceOwner::Npm, true);
        assert_eq!(
            (package.0, package.1),
            (Redirectability::OwnerManaged, SurfaceRisk::Critical)
        );
        let (redirectability, risk, reason) = risk_for(SurfaceOwner::Daemon, false);
        assert_eq!(
            (redirectability, risk),
            (Redirectability::OwnerManaged, SurfaceRisk::Critical)
        );
        assert!(reason.contains("daemon path"));
        assert!(!is_protected_bundle_path(
            Path::new("/home/test/.codex/packages/standalone/current/codex"),
            None,
        ));
        for path in [
            "/Applications/Codex.app/Contents/Resources/codex",
            r"C:\Program Files\WindowsApps\OpenAI.Codex\codex.exe",
        ] {
            assert!(is_protected_bundle_path(Path::new(path), None));
        }
        assert!(!is_protected_bundle_path(
            Path::new("/usr/local/bin/codex"),
            None
        ));
    }

    #[cfg(unix)]
    #[test]
    fn shell_resolution_process_is_bounded_and_command_only() {
        let temp = tempfile::tempdir().unwrap();
        let shell = temp.path().join("zsh");
        executable(
            &shell,
            "#!/bin/sh\nprintf '%s\\n' \"codex is an alias for '/tmp/codex'\" \"codex is a shell function\"\n",
        );
        let mut seeds = Vec::new();
        add_unix_shell_resolution_surfaces_for_shell(&mut seeds, &shell, Duration::from_secs(1));
        assert_eq!(seeds.len(), 2);
        assert!(
            seeds
                .iter()
                .all(|seed| !seed.probeable && seed.read_only_reason.is_some())
        );
        assert!(seeds[0].origin.contains("alias") && seeds[0].current);
        assert!(seeds[1].origin.contains("function") && !seeds[1].current);
        executable(&shell, "#!/bin/sh\nsleep 5\n");
        seeds.clear();
        let started = Instant::now();
        add_unix_shell_resolution_surfaces_for_shell(&mut seeds, &shell, Duration::from_millis(30));
        assert!(seeds.is_empty());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn parses_bounded_powershell_get_command_results() {
        let parsed = parse_powershell_resolutions(concat!(
            "profile banner\n",
            "__CODEX_PATCHER_GET_COMMAND_BEGIN__\n",
            r#"[{"Name":"codex","CommandType":"Alias","Path":"","Source":"","Definition":"Invoke-Codex"},{"Name":"codex.exe","CommandType":"Application","Path":"C:\\tools\\codex.exe","Source":"C:\\tools\\codex.exe","Definition":"C:\\tools\\codex.exe"}]"#,
            "\n__CODEX_PATCHER_GET_COMMAND_END__\n",
            "profile trailer\n",
        ));
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            (
                parsed[0].command_type.as_str(),
                parsed[0].definition.as_str()
            ),
            ("Alias", "Invoke-Codex")
        );
        assert_eq!(parsed[1].path, r"C:\tools\codex.exe");
    }
}
