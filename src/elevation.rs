//! Narrow privilege elevation for launcher mutations; discovery and builds
//! always remain in the unelevated manager.

use crate::shim::{self, BaselineKind, RepairOutcome, SURFACE_RECORD_SCHEMA, SurfaceRecord};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

pub const HELPER_COMMAND: &str = "__elevated-shim-helper";
const WIRE_SCHEMA: u32 = 1;
const MAX_WIRE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEnvelope<T> {
    schema: u32,
    payload: T,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "kebab-case", deny_unknown_fields)]
enum ElevationRequest {
    Install {
        surface: PathBuf,
        manager: PathBuf,
        backups_dir: PathBuf,
    },
    Uninstall {
        record: SurfaceRecord,
        backups_dir: PathBuf,
    },
    Repair {
        record: SurfaceRecord,
        manager: PathBuf,
        backups_dir: PathBuf,
        adopt: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case", deny_unknown_fields)]
enum ElevationResponse {
    Installed {
        record: SurfaceRecord,
    },
    Uninstalled,
    Repaired {
        record: SurfaceRecord,
        outcome: RepairOutcome,
    },
    Error {
        message: String,
    },
}

/// Install directly or elevate only an underlying permission failure.
pub fn install_redirect(
    surface: &Path,
    manager: &Path,
    backups_dir: &Path,
) -> Result<SurfaceRecord> {
    match shim::install_redirect(surface, manager, backups_dir) {
        Ok(record) => return Ok(record),
        Err(error) if !is_permission_denied(&error) => return Err(error),
        Err(_) => {}
    }

    // A normal attempt can have persisted its prepared takeover journal just
    // before the protected directory rejected the atomic rename. Recovering
    // here removes that no-mutation intent or returns a completed mutation.
    match shim::recover_redirect_journals(backups_dir) {
        Ok(recovered) => {
            if let Some(record) = recovered.iter().find(|record| {
                same_absolute_path(&record.surface, surface)
                    && same_absolute_path(&record.installed.manager, manager)
            }) {
                return Ok(record.clone());
            }
            if !recovered.is_empty() {
                bail!(
                    "a different completed takeover was recovered; refusing to overwrite it during elevation"
                );
            }
        }
        Err(error) if is_permission_denied(&error) => {}
        Err(error) => return Err(error),
    }

    let request = ElevationRequest::Install {
        surface: absolute_path(surface)?,
        manager: absolute_path(manager)?,
        backups_dir: absolute_path(backups_dir)?,
    };
    match invoke_helper(manager, &request)? {
        ElevationResponse::Installed { record } => Ok(record),
        response => unexpected_response("install", response),
    }
}

/// Restore a redirect with journaled compare-and-swap semantics.
pub fn uninstall_redirect(record: &SurfaceRecord, backups_dir: &Path) -> Result<()> {
    match shim::uninstall_redirect_journaled(record, backups_dir) {
        Ok(()) => return Ok(()),
        Err(error) if !is_permission_denied(&error) => return Err(error),
        Err(_) => {}
    }

    let request = ElevationRequest::Uninstall {
        record: record.clone(),
        backups_dir: absolute_path(backups_dir)?,
    };
    match invoke_helper(&record.installed.manager, &request)? {
        ElevationResponse::Uninstalled => Ok(()),
        response => unexpected_response("uninstall", response),
    }
}

/// Repair a launcher and copy elevated record changes into caller state.
pub fn repair_redirect(
    record: &mut SurfaceRecord,
    manager: &Path,
    backups_dir: &Path,
    adopt: bool,
) -> Result<RepairOutcome> {
    match shim::repair_redirect_journaled(record, manager, backups_dir, adopt) {
        Ok(outcome) => return Ok(outcome),
        Err(error) if !is_permission_denied(&error) => return Err(error),
        Err(_) => {}
    }

    let request = ElevationRequest::Repair {
        record: record.clone(),
        manager: absolute_path(manager)?,
        backups_dir: absolute_path(backups_dir)?,
        adopt,
    };
    match invoke_helper(manager, &request)? {
        ElevationResponse::Repaired {
            record: updated,
            outcome,
        } => {
            *record = updated;
            Ok(outcome)
        }
        response => unexpected_response("repair", response),
    }
}

/// Handle `EXE __elevated-shim-helper REQUEST RESPONSE` before Clap routing.
pub fn helper_entrypoint(raw_args: &[OsString]) -> Result<Option<i32>> {
    if raw_args.get(1).map(OsString::as_os_str) != Some(OsStr::new(HELPER_COMMAND)) {
        return Ok(None);
    }
    if raw_args.len() != 4 {
        bail!("{HELPER_COMMAND} requires exactly a request and response path");
    }
    run_helper(Path::new(&raw_args[2]), Path::new(&raw_args[3]))?;
    Ok(Some(0))
}

fn invoke_helper(executable: &Path, request: &ElevationRequest) -> Result<ElevationResponse> {
    let executable = absolute_path(executable)?;
    if !executable.is_file() {
        bail!(
            "stable codex-patcher manager does not exist: {}",
            executable.display()
        );
    }

    let exchange = PrivateExchange::create(request)?;
    let status = launch_elevated(&executable, &exchange.request_path, &exchange.response_path)?;
    if !status.success() {
        bail!("privileged launcher operation exited with status {status}");
    }
    exchange.read_response()
}

struct PrivateExchange {
    _directory: tempfile::TempDir,
    request_path: PathBuf,
    response_path: PathBuf,
}

impl PrivateExchange {
    fn create(request: &ElevationRequest) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix(".codex-patcher-elevate-")
            .tempdir()
            .context("create private elevation exchange")?;
        set_private_directory(directory.path())?;
        let request_path = directory.path().join("request.json");
        let response_path = directory.path().join("response.json");
        let request_file = create_private_file(&request_path)?;
        let _response_file = create_private_file(&response_path)?;
        write_wire(
            request_file,
            &WireEnvelope {
                schema: WIRE_SCHEMA,
                payload: request,
            },
        )?;
        Ok(Self {
            _directory: directory,
            request_path,
            response_path,
        })
    }

    fn read_response(&self) -> Result<ElevationResponse> {
        let mut file = open_wire_file(&self.response_path)?;
        let envelope: WireEnvelope<ElevationResponse> = read_wire(&mut file)?;
        validate_schema(envelope.schema)?;
        match envelope.payload {
            ElevationResponse::Error { message } => {
                bail!("privileged launcher operation: {message}")
            }
            response => Ok(response),
        }
    }
}

fn run_helper(request_path: &Path, response_path: &Path) -> Result<()> {
    validate_exchange_paths(request_path, response_path)?;
    let helper_identity = helper_identity()?;
    let mut request_file = open_helper_endpoint(request_path, helper_identity, false)?;
    // Open and validate the response before performing any privileged mutation
    // so an invalid or swapped destination cannot strand a completed action.
    let mut response_file = open_helper_endpoint(response_path, helper_identity, true)?;

    let response = (|| -> Result<ElevationResponse> {
        let envelope: WireEnvelope<ElevationRequest> = read_wire(&mut request_file)?;
        validate_schema(envelope.schema)?;
        validate_request(&envelope.payload, &helper_identity)?;
        execute_request(envelope.payload, &helper_identity)
    })()
    .unwrap_or_else(|error| ElevationResponse::Error {
        message: format!("{error:#}"),
    });

    response_file.set_len(0)?;
    response_file.seek(SeekFrom::Start(0))?;
    write_wire(
        response_file,
        &WireEnvelope {
            schema: WIRE_SCHEMA,
            payload: response,
        },
    )
}

fn execute_request(
    request: ElevationRequest,
    identity: &HelperIdentity,
) -> Result<ElevationResponse> {
    match request {
        ElevationRequest::Install {
            surface,
            manager,
            backups_dir,
        } => {
            let result = shim::install_redirect(&surface, &manager, &backups_dir);
            return_artifact_ownership(&backups_dir, identity)?;
            let record = result?;
            Ok(ElevationResponse::Installed { record })
        }
        ElevationRequest::Uninstall {
            record,
            backups_dir,
        } => {
            let result = shim::uninstall_redirect_journaled(&record, &backups_dir);
            // The unelevated first attempt may already have created this
            // journal. The elevated atomic rewrite replaces it with a
            // root-owned inode under the same name, so ownership restoration
            // must inspect every recognized artifact rather than only names
            // which appeared during this process.
            return_artifact_ownership(&backups_dir, identity)?;
            result?;
            Ok(ElevationResponse::Uninstalled)
        }
        ElevationRequest::Repair {
            mut record,
            manager,
            backups_dir,
            adopt,
        } => {
            let result =
                shim::repair_redirect_journaled(&mut record, &manager, &backups_dir, adopt);
            return_artifact_ownership(&backups_dir, identity)?;
            let outcome = result?;
            Ok(ElevationResponse::Repaired { record, outcome })
        }
    }
}

fn validate_request(request: &ElevationRequest, identity: &HelperIdentity) -> Result<()> {
    let (manager, backups_dir) = match request {
        ElevationRequest::Install {
            surface,
            manager,
            backups_dir,
        } => {
            validate_surface(surface)?;
            if same_absolute_path(surface, manager) {
                bail!("launcher surface and stable manager must be different paths");
            }
            (manager, backups_dir)
        }
        ElevationRequest::Uninstall {
            record,
            backups_dir,
        } => {
            validate_surface_record(record, backups_dir, identity)?;
            (&record.installed.manager, backups_dir)
        }
        ElevationRequest::Repair {
            record,
            manager,
            backups_dir,
            ..
        } => {
            validate_surface_record(record, backups_dir, identity)?;
            if record.installed.manager != *manager {
                bail!("repair manager does not match the recorded stable manager");
            }
            (manager, backups_dir)
        }
    };

    validate_absolute_regular_manager(manager)?;
    if !same_file(manager, &std::env::current_exe()?)? {
        bail!("elevation request manager is not the running stable manager");
    }
    validate_backups_directory(backups_dir, identity)
}

fn validate_surface_record(
    record: &SurfaceRecord,
    backups_dir: &Path,
    identity: &HelperIdentity,
) -> Result<()> {
    if record.schema != SURFACE_RECORD_SCHEMA {
        bail!(
            "unsupported surface record schema {} (expected {})",
            record.schema,
            SURFACE_RECORD_SCHEMA
        );
    }
    validate_surface(&record.surface)?;
    if !record.installed.manager.is_absolute() {
        bail!("surface record manager path must be absolute");
    }
    if let Some(backup) = record.baseline.backup_path.as_deref() {
        let expected_parent = normalized_existing_path(backups_dir)?;
        let actual_parent = backup
            .parent()
            .context("baseline backup has no parent directory")?;
        if normalized_existing_path(actual_parent)? != expected_parent {
            bail!("surface record baseline is outside the requested backup directory");
        }
        let metadata = fs::symlink_metadata(backup)
            .with_context(|| format!("inspect baseline backup {}", backup.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "surface record baseline backup is not a regular file: {}",
                backup.display()
            );
        }
        validate_baseline_owner(backup, &metadata, identity)?;
    }
    match record.baseline.kind {
        BaselineKind::Missing
            if record.baseline.backup_path.is_none()
                && record.baseline.symlink_target.is_none()
                && record.baseline.sha256.is_none() => {}
        BaselineKind::File
            if record.baseline.backup_path.is_some()
                && record.baseline.symlink_target.is_none()
                && record.baseline.sha256.is_some() => {}
        BaselineKind::Symlink
            if record.baseline.backup_path.is_none()
                && record.baseline.symlink_target.is_some()
                && record.baseline.sha256.is_some() => {}
        _ => bail!("surface record contains inconsistent baseline metadata"),
    }
    Ok(())
}

fn validate_surface(surface: &Path) -> Result<()> {
    if !surface.is_absolute() {
        bail!("launcher surface path must be absolute");
    }
    if surface.parent().is_none() || surface.file_name().is_none() {
        bail!("launcher surface must name a file");
    }
    Ok(())
}

fn validate_absolute_regular_manager(manager: &Path) -> Result<()> {
    if !manager.is_absolute() {
        bail!("stable manager path must be absolute");
    }
    let metadata = fs::metadata(manager)
        .with_context(|| format!("inspect stable manager {}", manager.display()))?;
    if !metadata.is_file() {
        bail!(
            "stable manager is not a regular file: {}",
            manager.display()
        );
    }
    Ok(())
}

fn validate_backups_directory(path: &Path, identity: &HelperIdentity) -> Result<()> {
    if !path.is_absolute() {
        bail!("backup directory path must be absolute");
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect backup directory {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "backup directory is not a real directory: {}",
            path.display()
        );
    }
    validate_directory_owner(path, &metadata, identity)
}

fn validate_exchange_paths(request: &Path, response: &Path) -> Result<()> {
    if !request.is_absolute() || !response.is_absolute() {
        bail!("elevation exchange paths must be absolute");
    }
    if request == response || request.file_name() != Some(OsStr::new("request.json")) {
        bail!("invalid elevation request path");
    }
    if response.file_name() != Some(OsStr::new("response.json")) {
        bail!("invalid elevation response path");
    }
    if request.parent() != response.parent() {
        bail!("elevation exchange files must share one private directory");
    }
    Ok(())
}

fn validate_schema(schema: u32) -> Result<()> {
    if schema != WIRE_SCHEMA {
        bail!("unsupported elevation request schema {schema} (expected {WIRE_SCHEMA})");
    }
    Ok(())
}

fn read_wire<T: for<'de> Deserialize<'de>>(file: &mut File) -> Result<T> {
    let length = file.metadata()?.len();
    if length == 0 || length > MAX_WIRE_BYTES {
        bail!("elevation exchange has invalid length {length}");
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.seek(SeekFrom::Start(0))?;
    file.take(MAX_WIRE_BYTES + 1).read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes).context("parse elevation exchange")
}

fn write_wire<T: Serialize>(mut file: File, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value).context("serialize elevation exchange")?;
    if bytes.len() as u64 > MAX_WIRE_BYTES {
        bail!("elevation exchange is too large");
    }
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn create_private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .with_context(|| format!("create private exchange file {}", path.display()))?;
    #[cfg(windows)]
    set_windows_private_acl(path, false)?;
    Ok(file)
}

fn open_wire_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    options
        .open(path)
        .with_context(|| format!("open elevation exchange {}", path.display()))
}

#[derive(Debug, Clone, Copy)]
struct HelperIdentity {
    #[cfg(unix)]
    uid: u32,
    #[cfg(unix)]
    gid: u32,
}

fn helper_identity() -> Result<HelperIdentity> {
    #[cfg(unix)]
    {
        let effective_uid = unsafe { libc::geteuid() };
        if effective_uid != 0 {
            bail!("privileged launcher helper must run as root through sudo");
        }
        let uid = parse_sudo_id("SUDO_UID")?;
        let gid = parse_sudo_id("SUDO_GID")?;
        if uid == 0 {
            bail!("privileged launcher helper refuses a root-origin request");
        }
        Ok(HelperIdentity { uid, gid })
    }
    #[cfg(windows)]
    {
        // UAC preserves the interactive user's identity while adding an
        // elevated token; the private temp directory ACL remains authoritative.
        Ok(HelperIdentity {})
    }
    #[cfg(not(any(unix, windows)))]
    {
        bail!("privilege elevation is unsupported on this platform")
    }
}

#[cfg(unix)]
fn parse_sudo_id(name: &str) -> Result<u32> {
    std::env::var(name)
        .with_context(|| format!("sudo did not provide {name}"))?
        .parse::<u32>()
        .with_context(|| format!("sudo provided an invalid {name}"))
}

fn open_helper_endpoint(path: &Path, identity: HelperIdentity, writable: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(writable);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .with_context(|| format!("open privileged exchange endpoint {}", path.display()))?;
    validate_endpoint_metadata(path, &file.metadata()?, &identity)?;
    if let Some(parent) = path.parent() {
        let parent_metadata = fs::symlink_metadata(parent)?;
        if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
            bail!("elevation exchange parent is not a real directory");
        }
        validate_private_directory(parent, &parent_metadata, &identity)?;
    }
    Ok(file)
}

fn validate_endpoint_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    identity: &HelperIdentity,
) -> Result<()> {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!(
            "elevation endpoint is not a regular file: {}",
            path.display()
        );
    }
    if metadata.len() > MAX_WIRE_BYTES {
        bail!("elevation endpoint is too large: {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != identity.uid || metadata.nlink() != 1 || metadata.mode() & 0o077 != 0 {
            bail!(
                "elevation endpoint is not a private caller-owned file: {}",
                path.display()
            );
        }
    }
    #[cfg(windows)]
    validate_windows_private_acl(path, false)?;
    let _ = identity;
    Ok(())
}

fn validate_private_directory(
    path: &Path,
    metadata: &fs::Metadata,
    identity: &HelperIdentity,
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != identity.uid || metadata.mode() & 0o077 != 0 {
            bail!(
                "elevation exchange directory is not private and caller-owned: {}",
                path.display()
            );
        }
    }
    #[cfg(windows)]
    validate_windows_private_acl(path, true)?;
    let _ = (path, metadata, identity);
    Ok(())
}

fn validate_directory_owner(
    path: &Path,
    metadata: &fs::Metadata,
    identity: &HelperIdentity,
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != identity.uid || metadata.mode() & 0o022 != 0 {
            bail!(
                "backup directory is not safely owned by the sudo caller: {}",
                path.display()
            );
        }
    }
    #[cfg(windows)]
    validate_windows_owner(path)?;
    let _ = (path, metadata, identity);
    Ok(())
}

fn validate_baseline_owner(
    path: &Path,
    metadata: &fs::Metadata,
    identity: &HelperIdentity,
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // A backup created by the failed unprivileged attempt belongs to the
        // caller; one created by a previous elevated attempt may belong to
        // root if that helper crashed before returning ownership.
        if metadata.uid() != identity.uid && metadata.uid() != 0 {
            bail!(
                "baseline backup has an unexpected owner: {}",
                path.display()
            );
        }
        if metadata.nlink() != 1 {
            bail!(
                "baseline backup has multiple hard links: {}",
                path.display()
            );
        }
    }
    #[cfg(windows)]
    validate_windows_owner(path)?;
    let _ = (path, metadata, identity);
    Ok(())
}

fn set_private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    set_windows_private_acl(path, true)?;
    let _ = path;
    Ok(())
}

#[cfg(windows)]
fn current_windows_sid() -> Result<(Vec<u8>, String)> {
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{
        GetLengthSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error()).context("open current Windows token");
    }
    let mut needed = 0;
    unsafe {
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
    }
    let mut words = vec![0_usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            words.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    let _ = unsafe { CloseHandle(token) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("read current Windows token user");
    }
    let sid = unsafe { (*(words.as_ptr().cast::<TOKEN_USER>())).User.Sid };
    let sid_bytes = unsafe {
        std::slice::from_raw_parts(sid.cast::<u8>(), GetLengthSid(sid) as usize).to_vec()
    };
    let mut string_sid = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut string_sid) } == 0 {
        return Err(std::io::Error::last_os_error()).context("format current Windows SID");
    }
    let length = unsafe { (0..).take_while(|&i| *string_sid.add(i) != 0).count() };
    let text = String::from_utf16(unsafe { std::slice::from_raw_parts(string_sid, length) });
    let _ = unsafe { LocalFree(string_sid.cast()) };
    let text = text.context("current Windows SID is not UTF-16")?;
    Ok((sid_bytes, text))
}

#[cfg(windows)]
fn with_windows_private_descriptor<T>(
    directory: bool,
    operation: impl FnOnce(*mut core::ffi::c_void, &[u8]) -> Result<T>,
) -> Result<T> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };

    let (sid, sid_text) = current_windows_sid()?;
    let inherit = if directory { "OICI" } else { "" };
    let sddl = format!(
        "O:{sid_text}D:P(A;{inherit};FA;;;{sid_text})(A;{inherit};FA;;;SY)(A;{inherit};FA;;;BA)"
    );
    let wide: Vec<u16> = OsStr::new(&sddl).encode_wide().chain(Some(0)).collect();
    let mut descriptor = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("build private Windows DACL");
    }
    let result = operation(descriptor, &sid);
    let _ = unsafe { LocalFree(descriptor) };
    result
}

#[cfg(windows)]
fn set_windows_private_acl(path: &Path, directory: bool) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        SetFileSecurityW,
    };
    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    with_windows_private_descriptor(directory, |descriptor, _| {
        if unsafe {
            SetFileSecurityW(
                path_wide.as_ptr(),
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                descriptor,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("secure elevation exchange {}", path.display()));
        }
        Ok(())
    })
}

#[cfg(windows)]
fn windows_owner_and_dacl(
    path: &Path,
) -> Result<(
    Vec<u8>,
    *mut core::ffi::c_void,
    *mut core::ffi::c_void,
    bool,
)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetFileSecurityW, GetSecurityDescriptorControl,
        GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, OWNER_SECURITY_INFORMATION,
        SE_DACL_PROTECTED,
    };
    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let requested = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut needed = 0;
    unsafe {
        GetFileSecurityW(
            path_wide.as_ptr(),
            requested,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    if needed == 0 {
        return Err(std::io::Error::last_os_error()).context("measure Windows endpoint DACL");
    }
    let mut descriptor = vec![0_u8; needed as usize];
    if unsafe {
        GetFileSecurityW(
            path_wide.as_ptr(),
            requested,
            descriptor.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("read Windows endpoint DACL");
    }
    descriptor.truncate(needed as usize);
    let mut owner = std::ptr::null_mut();
    let mut owner_defaulted = 0;
    let mut dacl = std::ptr::null_mut();
    let mut dacl_present = 0;
    let mut dacl_defaulted = 0;
    let mut control = 0;
    let mut revision = 0;
    let sd = descriptor.as_mut_ptr().cast();
    if unsafe { GetSecurityDescriptorOwner(sd, &mut owner, &mut owner_defaulted) } == 0
        || unsafe {
            GetSecurityDescriptorDacl(sd, &mut dacl_present, &mut dacl, &mut dacl_defaulted)
        } == 0
        || unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) } == 0
        || owner.is_null()
        || dacl_present == 0
        || dacl.is_null()
    {
        return Err(std::io::Error::last_os_error()).context("inspect Windows endpoint DACL");
    }
    Ok((
        descriptor,
        owner,
        dacl.cast(),
        control & SE_DACL_PROTECTED != 0,
    ))
}

#[cfg(windows)]
fn validate_windows_owner(path: &Path) -> Result<()> {
    use windows_sys::Win32::Security::EqualSid;
    let (_descriptor, owner, _, _) = windows_owner_and_dacl(path)?;
    let (expected, _) = current_windows_sid()?;
    if unsafe { EqualSid(owner, expected.as_ptr().cast_mut().cast()) } == 0 {
        bail!(
            "Windows artifact is not owned by the caller: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn validate_windows_private_acl(path: &Path, directory: bool) -> Result<()> {
    use windows_sys::Win32::Security::{ACL, EqualSid, GetSecurityDescriptorDacl};
    let (_actual_descriptor, owner, actual_acl, protected) = windows_owner_and_dacl(path)?;
    with_windows_private_descriptor(directory, |expected_descriptor, expected_sid| {
        let mut present = 0;
        let mut defaulted = 0;
        let mut expected_acl = std::ptr::null_mut();
        if unsafe {
            GetSecurityDescriptorDacl(
                expected_descriptor,
                &mut present,
                &mut expected_acl,
                &mut defaulted,
            )
        } == 0
            || present == 0
            || expected_acl.is_null()
        {
            bail!("generated private Windows DACL is invalid");
        }
        let actual_size = unsafe { (*(actual_acl.cast::<ACL>())).AclSize as usize };
        let expected_size = unsafe { (*expected_acl).AclSize as usize };
        let actual = unsafe { std::slice::from_raw_parts(actual_acl.cast::<u8>(), actual_size) };
        let expected =
            unsafe { std::slice::from_raw_parts(expected_acl.cast::<u8>(), expected_size) };
        if !protected
            || unsafe { EqualSid(owner, expected_sid.as_ptr().cast_mut().cast()) } == 0
            || actual != expected
        {
            bail!(
                "Windows elevation exchange is not private: {}",
                path.display()
            );
        }
        Ok(())
    })?;
    Ok(())
}

fn return_artifact_ownership(path: &Path, identity: &HelperIdentity) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        use std::os::unix::io::AsRawFd;
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if !is_owned_artifact(&entry.file_name()) {
                continue;
            }
            let entry_path = entry.path();
            let mut options = OpenOptions::new();
            options
                .read(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
            let file = match options.open(&entry_path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != 0 {
                continue;
            }
            if unsafe { libc::fchown(file.as_raw_fd(), identity.uid, identity.gid) } != 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!(
                        "return backup artifact ownership for {}",
                        entry_path.display()
                    )
                });
            }
        }
    }
    let _ = (path, identity);
    Ok(())
}

#[cfg(unix)]
fn is_owned_artifact(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    (name.starts_with(".takeover-") && name.ends_with(".json"))
        || (name.starts_with(".restore-") && name.ends_with(".json"))
        || (name.starts_with(".repair-") && name.ends_with(".json"))
        || name.ends_with(".baseline")
}

#[cfg(unix)]
fn launch_elevated(executable: &Path, request: &Path, response: &Path) -> Result<ExitStatus> {
    Command::new("sudo")
        .arg("--")
        .arg(executable)
        .arg(HELPER_COMMAND)
        .arg(request)
        .arg(response)
        .status()
        .context("launch narrow launcher operation through sudo")
}

#[cfg(windows)]
fn launch_elevated(executable: &Path, request: &Path, response: &Path) -> Result<ExitStatus> {
    let arguments = format!(
        "{} {} {}",
        quote_windows_arg(OsStr::new(HELPER_COMMAND)),
        quote_windows_arg(request.as_os_str()),
        quote_windows_arg(response.as_os_str())
    );
    Command::new("powershell.exe")
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
        .arg(
            "$p = Start-Process -FilePath $args[0] -ArgumentList $args[1] -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
        )
        .arg(executable)
        .arg(arguments)
        .status()
        .context("launch narrow launcher operation through Windows UAC")
}

#[cfg(not(any(unix, windows)))]
fn launch_elevated(_executable: &Path, _request: &Path, _response: &Path) -> Result<ExitStatus> {
    bail!("privilege elevation is unsupported on this platform")
}

#[cfg(windows)]
fn quote_windows_arg(value: &OsStr) -> String {
    use std::os::windows::ffi::OsStrExt;
    let value = String::from_utf16_lossy(&value.encode_wide().collect::<Vec<_>>());
    let mut quoted = String::from("\"");
    let mut slashes = 0;
    for character in value.chars() {
        if character == '\\' {
            slashes += 1;
        } else if character == '"' {
            quoted.push_str(&"\\".repeat(slashes * 2 + 1));
            quoted.push('"');
            slashes = 0;
        } else {
            quoted.push_str(&"\\".repeat(slashes));
            slashes = 0;
            quoted.push(character);
        }
    }
    quoted.push_str(&"\\".repeat(slashes * 2));
    quoted.push('"');
    quoted
}

fn is_permission_denied(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
    })
}

fn same_absolute_path(left: &Path, right: &Path) -> bool {
    absolute_path(left).ok() == absolute_path(right).ok()
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn normalized_existing_path(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).with_context(|| format!("canonicalize {}", path.display()))
}

fn same_file(left: &Path, right: &Path) -> Result<bool> {
    Ok(normalized_existing_path(left)? == normalized_existing_path(right)?)
}

fn unexpected_response<T>(operation: &str, response: ElevationResponse) -> Result<T> {
    bail!("privileged {operation} returned an unexpected response: {response:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn request_validation_rejects_relative_paths_before_mutation() {
        let temporary = tempfile::tempdir().unwrap();
        let request = ElevationRequest::Install {
            surface: PathBuf::from("relative/codex"),
            manager: temporary.path().join("manager"),
            backups_dir: temporary.path().join("backups"),
        };
        #[cfg(unix)]
        let identity = HelperIdentity {
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
        };
        #[cfg(windows)]
        let identity = HelperIdentity {};
        assert!(
            validate_request(&request, &identity)
                .unwrap_err()
                .to_string()
                .contains("absolute")
        );
    }

    #[test]
    fn exchange_validation_rejects_different_directories() {
        let temporary = tempfile::tempdir().unwrap();
        let left = temporary.path().join("left").join("request.json");
        let right = temporary.path().join("right").join("response.json");
        assert!(validate_exchange_paths(&left, &right).is_err());
    }

    #[test]
    fn wire_schema_and_unknown_fields_fail_closed() {
        let bad_schema = br#"{"schema":2,"payload":{"operation":"install","surface":"/tmp/a","manager":"/tmp/m","backups_dir":"/tmp/b"}}"#;
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(bad_schema).unwrap();
        let envelope: WireEnvelope<ElevationRequest> = read_wire(&mut file).unwrap();
        assert!(validate_schema(envelope.schema).is_err());

        let unknown = br#"{"schema":1,"extra":true,"payload":{"operation":"install","surface":"/tmp/a","manager":"/tmp/m","backups_dir":"/tmp/b"}}"#;
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(unknown).unwrap();
        assert!(read_wire::<WireEnvelope<ElevationRequest>>(&mut file).is_err());
    }
}
