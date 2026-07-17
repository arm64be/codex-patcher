use std::ffi::{OsStr, OsString};
use std::io::IsTerminal;
#[cfg(windows)]
use std::path::Path;

use crate::build::{BuildEvent, BuildFailure, BuildOptions, build_generation};
use crate::config::{Config, FailureMode, NoninteractivePending};
use crate::patchset::PatchSet;
use crate::paths::PatcherPaths;
use crate::probe;
use crate::state::{InstallState, StateStore};
use crate::tui::{
    FailureChoice, FailureScreen, ProgressDisplay, ProgressScreen, UpdateChoice, UpdateScreen,
};
use crate::types::{DesiredBuild, FailureRecord, GenerationRef, ProbeKind, ProbeState};
use crate::upstream::{ResolveOptions, resolve};
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use fs2::FileExt;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write;

pub const MANAGED_UPDATE_OVERRIDE: &str = "check_for_update_on_startup=false";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpdateOptions {
    pub retry: bool,
    pub accept_retag: bool,
    pub accept_force_push: bool,
    pub interactive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WrappedUpdate {
    Help,
    Run(UpdateOptions),
}

/// Preserve every caller argument and place the patcher's update-management
/// override at the last position still parsed as a global Codex option.
pub fn inject_managed_update_override(arguments: &[OsString]) -> Vec<OsString> {
    let insertion = arguments
        .iter()
        .position(|argument| argument == OsStr::new("--"))
        .unwrap_or(arguments.len());
    let mut output = Vec::with_capacity(arguments.len() + 2);
    output.extend_from_slice(&arguments[..insertion]);
    output.push(OsString::from("-c"));
    output.push(OsString::from(MANAGED_UPDATE_OVERRIDE));
    output.extend_from_slice(&arguments[insertion..]);
    output
}

/// Whether the initiating interactive Codex launch explicitly selected its
/// dangerous approval-and-sandbox bypass mode. Only real CLI options before a
/// `--` delimiter count; prompt text must never be able to enable this mode.
fn invocation_uses_yolo_mode(arguments: &[OsString]) -> bool {
    arguments
        .iter()
        .take_while(|argument| *argument != OsStr::new("--"))
        .any(|argument| {
            matches!(
                argument.to_str(),
                Some("--yolo" | "--dangerously-bypass-approvals-and-sandbox")
            )
        })
}

/// Determine whether a Codex invocation is one of the TUI entry points where
/// it is safe to stop and ask about a detected upstream update.
pub fn is_interactive_invocation(arguments: &[OsString]) -> bool {
    is_interactive_invocation_with_subcommands(arguments, &[])
}

fn is_interactive_invocation_with_subcommands(
    arguments: &[OsString],
    validated_subcommands: &[String],
) -> bool {
    let strings: Vec<_> = arguments
        .iter()
        .map(|argument| argument.to_string_lossy())
        .collect();
    if strings
        .iter()
        .any(|argument| matches!(argument.as_ref(), "-h" | "--help" | "-V" | "--version"))
    {
        return false;
    }

    let known_noninteractive = [
        "exec",
        "e",
        "review",
        "login",
        "logout",
        "mcp",
        "plugin",
        "mcp-server",
        "app-server",
        "remote-control",
        "app",
        "completion",
        "update",
        "doctor",
        "sandbox",
        "debug",
        "execpolicy",
        "apply",
        "a",
        "archive",
        "delete",
        "unarchive",
        "cloud",
        "cloud-tasks",
        "responses-api-proxy",
        "stdio-to-uds",
        "exec-server",
        "features",
        "help",
    ];
    let Some(index) = first_positional_index(&strings) else {
        return true;
    };
    let argument = strings[index].as_ref();
    if matches!(argument, "resume" | "fork") {
        return true;
    }
    if known_noninteractive.contains(&argument)
        || validated_subcommands
            .iter()
            .any(|command| command == argument)
    {
        return false;
    }
    // The default Codex syntax accepts a prompt as its first positional.
    true
}

const GLOBAL_VALUE_OPTIONS: &[&str] = &[
    "-c",
    "--config",
    "--enable",
    "--disable",
    "--remote",
    "--remote-auth-token-env",
    "-m",
    "--model",
    "--local-provider",
    "-C",
    "--cd",
    "--add-dir",
    "-s",
    "--sandbox",
    "-a",
    "--ask-for-approval",
    "-p",
    "--profile",
];

fn first_positional_index(arguments: &[std::borrow::Cow<'_, str>]) -> Option<usize> {
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_ref();
        if argument == "--" {
            return None;
        }
        if matches!(argument, "-i" | "--image") {
            // --image is variadic. Every following non-option is an image, so
            // no later positional can safely be interpreted as a subcommand.
            return None;
        }
        if GLOBAL_VALUE_OPTIONS.contains(&argument) {
            index = index.saturating_add(2);
            continue;
        }
        if option_has_attached_value(argument) || argument.starts_with('-') {
            index += 1;
            continue;
        }
        return Some(index);
    }
    None
}

fn option_has_attached_value(argument: &str) -> bool {
    if let Some((name, _)) = argument.split_once('=') {
        return GLOBAL_VALUE_OPTIONS.contains(&name) || matches!(name, "-i" | "--image");
    }
    ["-c", "-m", "-C", "-s", "-a", "-p", "-i"]
        .iter()
        .any(|prefix| argument.starts_with(prefix) && argument.len() > prefix.len())
}

pub fn run_active(
    paths: &PatcherPaths,
    generation: &GenerationRef,
    arguments: &[OsString],
) -> Result<i32> {
    let binary = &generation.binary;
    if !binary.is_file() {
        bail!("active Codex binary is missing: {}", binary.display());
    }
    let _lease = acquire_generation_lease(paths, &generation.id)?;
    let arguments = inject_managed_update_override(arguments);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = std::process::Command::new(binary).args(&arguments).exec();
        Err(error).with_context(|| format!("executing {}", binary.display()))
    }

    #[cfg(windows)]
    {
        run_active_windows(binary, &arguments)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = arguments;
        bail!("unsupported process platform")
    }
}

fn acquire_generation_lease(paths: &PatcherPaths, generation_id: &str) -> Result<File> {
    if generation_id.is_empty()
        || !generation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("generation id contains unsafe path characters");
    }
    let directory = paths.state_dir.join("leases");
    std::fs::create_dir_all(&directory)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(format!("{generation_id}.lock")))?;
    FileExt::lock_shared(&file).context("acquire generation execution lease")?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let descriptor = file.as_raw_fd();
        // Keep the shared lease across the transparent exec. The kernel drops
        // it automatically when the selected Codex process tree closes the
        // inherited descriptor.
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
        if flags < 0
            || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0
        {
            return Err(std::io::Error::last_os_error())
                .context("make generation lease inheritable");
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        if unsafe {
            SetHandleInformation(
                file.as_raw_handle(),
                HANDLE_FLAG_INHERIT,
                HANDLE_FLAG_INHERIT,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("make generation lease inheritable");
        }
    }
    Ok(file)
}

#[cfg(windows)]
fn run_active_windows(binary: &Path, arguments: &[OsString]) -> Result<i32> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, GetExitCodeProcess, INFINITE, PROCESS_INFORMATION, STARTF_USESTDHANDLES,
        STARTUPINFOW, WaitForSingleObject,
    };

    let application: Vec<u16> = binary.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut command_line = Vec::new();
    append_windows_argument(
        &mut command_line,
        &binary.as_os_str().encode_wide().collect::<Vec<_>>(),
    );
    for argument in arguments {
        command_line.push(b' ' as u16);
        append_windows_argument(
            &mut command_line,
            &argument.encode_wide().collect::<Vec<_>>(),
        );
    }
    command_line.push(0);

    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESTDHANDLES,
        // SAFETY: GetStdHandle returns borrowed process handles which remain
        // valid for the complete CreateProcessW call.
        hStdInput: unsafe { GetStdHandle(STD_INPUT_HANDLE) },
        hStdOutput: unsafe { GetStdHandle(STD_OUTPUT_HANDLE) },
        hStdError: unsafe { GetStdHandle(STD_ERROR_HANDLE) },
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // Inheriting the current environment, directory, console group, and
    // standard handles preserves transparent dispatcher semantics. Keeping
    // the same console group also gives the child normal Ctrl event delivery.
    // SAFETY: every pointer references live storage for the duration of the
    // call; command_line is mutable as required by CreateProcessW.
    if unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            0,
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut process,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("CreateProcessW {}", binary.display()));
    }
    // The primary thread handle is unnecessary after process creation.
    unsafe { CloseHandle(process.hThread) };
    // SAFETY: hProcess is valid until closed below.
    let wait = unsafe { WaitForSingleObject(process.hProcess, INFINITE) };
    if wait != WAIT_OBJECT_0 {
        unsafe { CloseHandle(process.hProcess) };
        bail!("waiting for active Codex failed with Windows wait status {wait}");
    }
    let mut code = 1_u32;
    // SAFETY: hProcess is signalled and remains valid.
    let got_code = unsafe { GetExitCodeProcess(process.hProcess, &mut code) };
    unsafe { CloseHandle(process.hProcess) };
    if got_code == 0 {
        return Err(std::io::Error::last_os_error()).context("read active Codex exit status");
    }
    Ok(code as i32)
}

#[cfg(any(windows, test))]
fn append_windows_argument(output: &mut Vec<u16>, argument: &[u16]) {
    let quote = argument.is_empty()
        || argument
            .iter()
            .any(|unit| matches!(*unit, 0x20 | 0x09 | 0x22));
    if !quote {
        output.extend_from_slice(argument);
        return;
    }
    output.push(b'"' as u16);
    let mut backslashes = 0;
    for unit in argument {
        if *unit == b'\\' as u16 {
            backslashes += 1;
        } else if *unit == b'"' as u16 {
            output.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
            output.push(*unit);
            backslashes = 0;
        } else {
            output.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
            output.push(*unit);
            backslashes = 0;
        }
    }
    output.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    output.push(b'"' as u16);
}

pub fn dispatch(paths: &PatcherPaths, arguments: &[OsString]) -> Result<i32> {
    if std::env::var_os("CODEX_PATCHER_MAINTENANCE").is_some() {
        bail!(
            "codex-patcher dispatcher recursion is disabled inside a patch-maintenance session; use the pinned executable named in the repair prompt"
        );
    }
    let store = StateStore::new(paths.clone());

    if let Some(update) = wrapped_update(arguments)? {
        if update == WrappedUpdate::Help {
            eprintln!("Usage: codex update [--retry] [--accept-retag] [--accept-force-push]");
            return Ok(0);
        }
        let WrappedUpdate::Run(mut options) = update else {
            unreachable!()
        };
        options.interactive = terminal_interactive();
        return update_and_run(paths, &store, options, None);
    }

    let snapshot = probe::refresh(paths)?;
    let validated_subcommands = snapshot
        .active
        .as_ref()
        .map(|generation| generation.subcommands.as_slice())
        .unwrap_or_default();
    let is_tui = terminal_interactive()
        && is_interactive_invocation_with_subcommands(arguments, validated_subcommands);

    if snapshot.probe.kind == ProbeKind::Pending
        && patch_only_update(&snapshot)
        && Config::load(snapshot.patch_dir.join("codex-patcher.toml"))?.auto_rebuild_patches
    {
        let desired = snapshot
            .probe
            .desired
            .as_ref()
            .context("pending freshness state has no desired build")?;
        return update_and_run_desired(
            paths,
            &store,
            UpdateOptions {
                interactive: is_tui,
                ..UpdateOptions::default()
            },
            desired,
            arguments,
        );
    }

    match snapshot.probe.kind {
        ProbeKind::Pending if is_tui => {
            let desired = snapshot
                .probe
                .desired
                .as_ref()
                .context("pending freshness state has no desired build")?;
            let active = snapshot
                .active
                .as_ref()
                .context("no active patched generation")?;
            let mut screen = UpdateScreen::new(&active.source.version, &desired.source.version);
            screen.current_patch_fingerprint = Some(active.patch_fingerprint.clone());
            screen.desired_patch_fingerprint = Some(desired.patch_fingerprint.clone());
            screen.release_url = source_link(&desired.source);
            match crate::tui::prompt_update(&screen)? {
                UpdateChoice::Build => update_and_run(
                    paths,
                    &store,
                    UpdateOptions {
                        interactive: true,
                        ..UpdateOptions::default()
                    },
                    Some(arguments),
                ),
                UpdateChoice::Exit => Ok(0),
            }
        }
        ProbeKind::Failed | ProbeKind::Blocked if is_tui => {
            handle_interactive_failure(paths, &snapshot, Some(arguments))
        }
        ProbeKind::Pending | ProbeKind::Failed | ProbeKind::Blocked => {
            apply_noninteractive_policy(paths, &snapshot, arguments)
        }
        ProbeKind::Unknown | ProbeKind::Current | ProbeKind::Degraded => {
            let active = snapshot
                .active
                .as_ref()
                .context("no active patched generation")?;
            run_active(paths, active, arguments)
        }
    }
}

fn patch_only_update(state: &InstallState) -> bool {
    let (Some(active), Some(desired)) = (state.active.as_ref(), state.probe.desired.as_ref())
    else {
        return false;
    };
    active.source == desired.source
        && active.target == desired.target
        && active.patch_fingerprint != desired.patch_fingerprint
}

fn update_and_run(
    paths: &PatcherPaths,
    store: &StateStore,
    options: UpdateOptions,
    arguments: Option<&[OsString]>,
) -> Result<i32> {
    update_and_run_inner(paths, store, options, None, arguments)
}

fn update_and_run_desired(
    paths: &PatcherPaths,
    store: &StateStore,
    options: UpdateOptions,
    desired: &DesiredBuild,
    arguments: &[OsString],
) -> Result<i32> {
    update_and_run_inner(paths, store, options, Some(desired), Some(arguments))
}

fn update_and_run_inner(
    paths: &PatcherPaths,
    store: &StateStore,
    options: UpdateOptions,
    desired: Option<&DesiredBuild>,
    arguments: Option<&[OsString]>,
) -> Result<i32> {
    let result = match desired {
        Some(desired) => foreground_update_for_launch(paths, options, desired),
        None => foreground_update(paths, options),
    };
    match result {
        Ok(generation) => arguments.map_or(Ok(0), |args| run_active(paths, &generation, args)),
        Err(error) => {
            append_runtime_log(paths, &format!("foreground update failed: {error:#}"));
            if options.interactive {
                handle_interactive_failure(paths, &store.require()?, arguments)
            } else if let Some(arguments) = arguments {
                apply_noninteractive_policy(paths, &store.require()?, arguments)
            } else {
                Err(error)
            }
        }
    }
}

/// Serialize the complete resolve/build/activate transaction against install,
/// repair, shim mutation, garbage collection, and uninstall.
pub fn foreground_update(paths: &PatcherPaths, options: UpdateOptions) -> Result<GenerationRef> {
    let store = StateStore::new(paths.clone());
    let _manager_lock = store.manager_lock()?;
    store.recover_surface_transactions()?;
    foreground_update_locked_with_desired(paths, options, None)
}

fn foreground_update_for_launch(
    paths: &PatcherPaths,
    options: UpdateOptions,
    desired: &DesiredBuild,
) -> Result<GenerationRef> {
    let store = StateStore::new(paths.clone());
    let _manager_lock = store.manager_lock()?;
    store.recover_surface_transactions()?;
    foreground_update_locked_with_desired(paths, options, Some(desired))
}

/// Run an update while the caller already owns the manager-operation lock.
///
/// Installation uses this entry point because it must keep one lock across
/// initial generation creation and launcher takeover. All other callers must
/// use [`foreground_update`].
#[doc(hidden)]
pub fn foreground_update_locked(
    paths: &PatcherPaths,
    options: UpdateOptions,
) -> Result<GenerationRef> {
    foreground_update_locked_with_desired(paths, options, None)
}

fn foreground_update_locked_with_desired(
    paths: &PatcherPaths,
    options: UpdateOptions,
    launch_desired: Option<&DesiredBuild>,
) -> Result<GenerationRef> {
    let store = StateStore::new(paths.clone());
    let _build_lock = store.build_lock()?;
    let state = store.require()?;
    let (config, patches, desired) =
        match resolve_build_inputs(paths, &state, options, launch_desired) {
            Ok(resolved) => resolved,
            Err(error) => {
                record_resolution_problem(&store, &error)?;
                return Err(error);
            }
        };
    if !options.retry
        && let Some(failure) = state
            .failure
            .as_ref()
            .filter(|failure| failure.desired.source_key == desired.source_key)
    {
        bail!(
            "cached failure {} for this source key: {} (log: {}); use --retry or `codex-patcher repair {}`",
            failure.id,
            failure.summary,
            failure.log_path.display(),
            failure.id
        );
    }

    let current_version = state
        .active
        .as_ref()
        .map(|generation| generation.source.version.as_str())
        .unwrap_or("not installed");
    let progress_screen = ProgressScreen::new(current_version, &desired.source.version);
    let (display, handle) = if options.interactive {
        let (display, handle) = ProgressDisplay::start(progress_screen)?;
        (Some(display), handle)
    } else {
        (None, crate::tui::ProgressHandle::detached(progress_screen))
    };
    let mut progress = |event: BuildEvent| match event {
        BuildEvent::Phase(phase) => {
            let _ = handle.set_phase(phase);
            if !options.interactive {
                eprintln!("codex-patcher: {phase}");
            }
        }
        BuildEvent::Line(line) => {
            let _ = handle.set_latest_line(&line);
        }
    };
    let generation_existed = paths
        .generations_dir()
        .join(&desired.source_key)
        .join("generation.json")
        .is_file();
    let result = build_generation(
        paths,
        &patches,
        &desired,
        state.active.as_ref(),
        &BuildOptions {
            allow_force_push: options.accept_force_push,
            retry: options.retry,
        },
        &mut progress,
    );
    drop(display);

    let generation = match result {
        Ok(generation) => generation,
        Err(failure) => {
            if failure.transient && state.active.is_some() {
                record_transient_build_problem(&store, &desired, &failure)?;
            } else {
                record_failure(&store, &desired, &failure)?;
            }
            return Err(anyhow!(failure));
        }
    };
    let discard = || {
        if !generation_existed {
            let _ = std::fs::remove_dir_all(paths.generations_dir().join(&generation.id));
        }
    };
    let latest = store.require()?;
    let (_, live_patches, live_desired) = match resolve_build_inputs(
        paths,
        &latest,
        UpdateOptions {
            retry: false,
            ..options
        },
        launch_desired,
    ) {
        Ok(resolved) => resolved,
        Err(error) => {
            discard();
            record_resolution_problem(&store, &error)?;
            return Err(error.context("rechecking upstream before activation"));
        }
    };
    if live_desired != desired || live_patches.fingerprint != patches.fingerprint {
        discard();
        update_state(&store, |latest| {
            latest.probe = probe_state(
                ProbeKind::Pending,
                &live_desired,
                Some("inputs changed during the build; the staged generation was discarded"),
            );
            Ok(())
        })?;
        bail!("patches, build configuration, or upstream source changed during the build");
    }
    update_state(&store, |latest| {
        let live = PatchSet::load(&latest.patch_dir)?;
        if live.fingerprint != patches.fingerprint {
            bail!("patch directory changed before activation");
        }
        let live_config = Config::load(latest.patch_dir.join("codex-patcher.toml"))?;
        if live_config.branch != config.branch || live_config.resolved_target()? != desired.target {
            bail!("build configuration changed before activation");
        }
        let next_check_at = launch_desired.and_then(|expected| {
            (latest.probe.desired.as_ref() == Some(expected))
                .then_some(latest.probe.next_check_at)
                .flatten()
        });
        latest.activate(generation.clone());
        latest.probe = probe_state(ProbeKind::Current, &desired, None);
        latest.probe.next_check_at = next_check_at;
        Ok(())
    })?;
    Ok(generation)
}

fn resolve_build_inputs(
    paths: &PatcherPaths,
    state: &InstallState,
    options: UpdateOptions,
    launch_desired: Option<&DesiredBuild>,
) -> Result<(Config, PatchSet, DesiredBuild)> {
    let Some(expected) = launch_desired else {
        return resolve_desired(paths, state, options);
    };
    let config = Config::load(state.patch_dir.join("codex-patcher.toml"))?;
    let patches = PatchSet::load(&state.patch_dir)?;
    let target = config.resolved_target()?;
    if config.branch.as_str() != expected.source.channel {
        bail!(
            "release channel changed after the launch freshness check (expected {}, found {})",
            expected.source.channel,
            config.branch
        );
    }
    if target != expected.target {
        bail!(
            "build target changed after the launch freshness check (expected {}, found {})",
            expected.target,
            target
        );
    }
    let desired = DesiredBuild {
        source_key: patches.source_key(&expected.source, &target),
        patch_fingerprint: patches.fingerprint.clone(),
        source: expected.source.clone(),
        target,
    };
    if desired != *expected {
        bail!("patch inputs changed after the launch freshness check");
    }
    Ok((config, patches, desired))
}

fn record_transient_build_problem(
    store: &StateStore,
    desired: &DesiredBuild,
    failure: &BuildFailure,
) -> Result<()> {
    update_state(store, |state| {
        let deterministic_failure_remains = state
            .failure
            .as_ref()
            .is_some_and(|record| record.desired.source_key == desired.source_key);
        state.probe = probe_state(
            if deterministic_failure_remains {
                ProbeKind::Failed
            } else {
                ProbeKind::Degraded
            },
            desired,
            Some(&format!(
                "temporary network failure during {}: {} (log: {})",
                failure.phase,
                failure.summary,
                failure.log_path.display()
            )),
        );
        Ok(())
    })
}

fn resolve_desired(
    paths: &PatcherPaths,
    state: &InstallState,
    options: UpdateOptions,
) -> Result<(Config, PatchSet, DesiredBuild)> {
    let config = Config::load(state.patch_dir.join("codex-patcher.toml"))?;
    let patches = PatchSet::load(&state.patch_dir)?;
    let target = config.resolved_target()?;
    let source = resolve(
        config.branch,
        paths.remote_cache_file(),
        state.resolution_baseline(config.branch.as_str())?,
        ResolveOptions {
            force: options.retry,
            accept_retag: options.accept_retag,
            accept_force_push: options.accept_force_push,
            ..ResolveOptions::default()
        },
    )?;
    let desired = DesiredBuild {
        patch_fingerprint: patches.fingerprint.clone(),
        source_key: patches.source_key(&source, &target),
        source,
        target,
    };
    Ok((config, patches, desired))
}

fn record_failure(
    store: &StateStore,
    desired: &DesiredBuild,
    failure: &BuildFailure,
) -> Result<()> {
    update_state(store, |state| {
        let existing = state
            .failure
            .as_ref()
            .filter(|record| record.desired.source_key == desired.source_key);
        let record = FailureRecord {
            id: existing
                .map(|record| record.id.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            desired: desired.clone(),
            phase: failure.phase.clone(),
            summary: failure.summary.clone(),
            failed_patch_index: failure.failed_patch_index,
            failed_patch: failure.failed_patch.clone(),
            log_path: failure.log_path.clone(),
            created_at: existing
                .map(|record| record.created_at)
                .unwrap_or_else(Utc::now),
            repair_worktree: existing.and_then(|record| record.repair_worktree.clone()),
        };
        state.failure = Some(record);
        state.probe = probe_state(ProbeKind::Failed, desired, Some(&failure.to_string()));
        Ok(())
    })
}

fn record_resolution_problem(store: &StateStore, error: &anyhow::Error) -> Result<()> {
    let message = format!("source resolution failed: {error:#}");
    let blocked = [
        "retag",
        "moved tag",
        "downgrade",
        "release deletion",
        "non-fast-forward",
        "--accept-force-push",
    ]
    .iter()
    .any(|needle| message.contains(needle));
    update_state(store, |state| {
        state.probe.kind = if blocked {
            ProbeKind::Blocked
        } else if state.active.is_some() {
            ProbeKind::Degraded
        } else {
            ProbeKind::Failed
        };
        state.probe.checked_at = Some(Utc::now());
        state.probe.next_check_at = None;
        state.probe.message = Some(message);
        Ok(())
    })
}

fn update_state<T>(
    store: &StateStore,
    mutate: impl FnOnce(&mut InstallState) -> Result<T>,
) -> Result<T> {
    store.with_state_lock(|| {
        let mut state = store.require()?;
        let value = mutate(&mut state)?;
        store.save(&state)?;
        Ok(value)
    })
}

fn probe_state(kind: ProbeKind, desired: &DesiredBuild, message: Option<&str>) -> ProbeState {
    ProbeState {
        kind,
        checked_at: Some(Utc::now()),
        next_check_at: None,
        desired: Some(desired.clone()),
        message: message.map(str::to_owned),
    }
}

#[doc(hidden)]
pub fn handle_interactive_failure(
    paths: &PatcherPaths,
    state: &InstallState,
    original_arguments: Option<&[OsString]>,
) -> Result<i32> {
    let yolo_mode = original_arguments.is_some_and(invocation_uses_yolo_mode);
    let failure = state.failure.as_ref();
    let active = state.active.as_ref();
    let desired_version = failure
        .map(|record| record.desired.source.version.clone())
        .or_else(|| {
            state
                .probe
                .desired
                .as_ref()
                .map(|desired| desired.source.version.clone())
        })
        .unwrap_or_else(|| {
            active
                .map(|generation| generation.source.version.clone())
                .unwrap_or_else(|| "unknown".to_owned())
        });
    let screen = FailureScreen {
        current_version: active
            .map(|generation| generation.source.version.clone())
            .unwrap_or_else(|| "not installed".into()),
        desired_version,
        phase: failure
            .map(|record| record.phase.clone())
            .unwrap_or_else(|| "resolve".to_owned()),
        summary: failure
            .map(|record| record.summary.clone())
            .or_else(|| state.probe.message.clone())
            .unwrap_or_else(|| "the desired patched generation is unavailable".to_owned()),
        failed_patch_index: failure.and_then(|record| record.failed_patch_index),
        failed_patch: failure.and_then(|record| record.failed_patch.clone()),
        log_path: failure
            .map(|record| record.log_path.clone())
            .unwrap_or_else(|| paths.logs_dir().join("runtime.log")),
        last_good_version: failure
            .and(active)
            .map(|generation| generation.source.version.clone()),
    };
    match crate::tui::prompt_failure(&screen)? {
        FailureChoice::Repair if active.is_some() && failure.is_some() => {
            let store = StateStore::new(paths.clone());
            let _manager_lock = store.manager_lock()?;
            store.recover_surface_transactions()?;
            let generation = crate::repair::run_repair_session_with_options(
                paths,
                failure.expect("checked above"),
                crate::repair::RunRepairOptions {
                    yolo_mode,
                    ..crate::repair::RunRepairOptions::default()
                },
            )?;
            if let Some(arguments) = original_arguments {
                run_active(paths, &generation, arguments)
            } else {
                Ok(0)
            }
        }
        FailureChoice::Repair | FailureChoice::Exit => Ok(75),
    }
}

fn apply_noninteractive_policy(
    paths: &PatcherPaths,
    state: &InstallState,
    arguments: &[OsString],
) -> Result<i32> {
    let config = match Config::load(state.patch_dir.join("codex-patcher.toml")) {
        Ok(config) => config,
        Err(error) => {
            eprintln!(
                "codex-patcher: invalid live configuration after the freshness check: {error:#}"
            );
            if let Some(detail) = state.probe.message.as_deref() {
                eprintln!("detail: {detail}");
            }
            return Ok(75);
        }
    };
    let warn_run = match config.noninteractive_pending {
        NoninteractivePending::WarnRun => true,
        NoninteractivePending::Error => false,
        NoninteractivePending::Auto => config.failure_mode == FailureMode::LastGood,
    };
    if warn_run {
        let active = state
            .active
            .as_ref()
            .context("no last-good patched generation")?;
        eprintln!(
            "codex-patcher: update pending or failed; running last-good patched Codex {}",
            active.source.version
        );
        run_active(paths, active, arguments)
    } else {
        let detail = state
            .probe
            .message
            .as_deref()
            .unwrap_or("a patched update is pending");
        eprintln!("codex-patcher: {detail}");
        if let Some(failure) = state.failure.as_ref() {
            eprintln!("log: {}", failure.log_path.display());
            eprintln!("repair: codex-patcher repair {}", failure.id);
        } else {
            eprintln!("update: codex-patcher update");
        }
        Ok(75)
    }
}

fn terminal_interactive() -> bool {
    std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal()
        && std::env::var_os("TERM").is_none_or(|term| term != "dumb")
}

fn wrapped_update(arguments: &[OsString]) -> Result<Option<WrappedUpdate>> {
    let strings: Vec<_> = arguments
        .iter()
        .map(|argument| argument.to_string_lossy())
        .collect();
    let Some(position) = first_positional_index(&strings) else {
        return Ok(None);
    };
    if strings[position] != "update" {
        return Ok(None);
    }
    if strings[..position]
        .iter()
        .any(|argument| matches!(argument.as_ref(), "-h" | "--help" | "-V" | "--version"))
    {
        return Ok(None);
    }

    let mut options = UpdateOptions::default();
    for argument in &strings[position + 1..] {
        match argument.as_ref() {
            "-h" | "--help" => return Ok(Some(WrappedUpdate::Help)),
            "--retry" => options.retry = true,
            "--accept-retag" => options.accept_retag = true,
            "--accept-force-push" => options.accept_force_push = true,
            unknown => bail!("unsupported `codex update` argument {unknown:?}"),
        }
    }
    Ok(Some(WrappedUpdate::Run(options)))
}

fn source_link(source: &crate::types::ResolvedSource) -> Option<String> {
    source.release_url.clone().or_else(|| {
        (source.channel == "nightly").then(|| {
            format!(
                "https://github.com/openai/codex/commit/{}",
                source.commit_oid
            )
        })
    })
}

fn append_runtime_log(paths: &PatcherPaths, message: &str) {
    if let Ok(mut log) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.logs_dir().join("runtime.log"))
    {
        let _ = writeln!(log, "{} {message}", Utc::now().to_rfc3339());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn update_override_is_last_before_delimiter() {
        assert_eq!(
            inject_managed_update_override(&args(&["exec", "--", "-not-a-flag"])),
            args(&[
                "exec",
                "-c",
                "check_for_update_on_startup=false",
                "--",
                "-not-a-flag"
            ])
        );
    }

    #[test]
    fn update_override_wins_over_caller_value() {
        assert_eq!(
            inject_managed_update_override(&args(&[
                "-c",
                "check_for_update_on_startup=true",
                "hello"
            ])),
            args(&[
                "-c",
                "check_for_update_on_startup=true",
                "hello",
                "-c",
                "check_for_update_on_startup=false"
            ])
        );
    }

    #[test]
    fn interactive_classification_covers_tui_and_protocol_modes() {
        assert!(is_interactive_invocation(&args(&[])));
        assert!(is_interactive_invocation(&args(&["resume", "--last"])));
        assert!(is_interactive_invocation(&args(&["-m", "gpt-5", "fix it"])));
        assert!(!is_interactive_invocation(&args(&["exec", "fix it"])));
        assert!(!is_interactive_invocation(&args(&["app-server"])));
        assert!(!is_interactive_invocation(&args(&["help", "resume"])));
        assert!(!is_interactive_invocation(&args(&[
            "--enable", "foo", "exec", "fix it"
        ])));
        assert!(!is_interactive_invocation(&args(&[
            "--remote=ws://localhost:1",
            "app-server"
        ])));
        assert!(!is_interactive_invocation(&args(&["--version"])));
    }

    #[test]
    fn repair_yolo_inheritance_only_recognizes_real_cli_flags() {
        assert!(invocation_uses_yolo_mode(&args(&["--yolo", "fix it"])));
        assert!(invocation_uses_yolo_mode(&args(&[
            "fix it",
            "--dangerously-bypass-approvals-and-sandbox"
        ])));
        assert!(!invocation_uses_yolo_mode(&args(&["fix --yolo"])));
        assert!(!invocation_uses_yolo_mode(&args(&["--", "--yolo"])));
    }

    #[test]
    fn wrapped_update_accepts_management_flags_after_global_options() {
        assert_eq!(
            wrapped_update(&args(&[
                "-c",
                "foo=true",
                "update",
                "--retry",
                "--accept-force-push"
            ]))
            .unwrap(),
            Some(WrappedUpdate::Run(UpdateOptions {
                retry: true,
                accept_retag: false,
                accept_force_push: true,
                interactive: false,
            }))
        );
        assert_eq!(
            wrapped_update(&args(&["update", "--help"])).unwrap(),
            Some(WrappedUpdate::Help)
        );
        assert_eq!(wrapped_update(&args(&["--", "update"])).unwrap(), None);
        assert!(wrapped_update(&args(&["update", "--wat"])).is_err());
    }

    #[test]
    fn nightly_update_uses_an_immutable_commit_link() {
        let source = crate::types::ResolvedSource {
            channel: "nightly".to_owned(),
            ref_name: "refs/heads/main".to_owned(),
            ref_object_oid: "a".repeat(40),
            commit_oid: "a".repeat(40),
            version: "0.0.0".to_owned(),
            release_url: None,
        };
        assert_eq!(
            source_link(&source).as_deref(),
            Some(format!("https://github.com/openai/codex/commit/{}", "a".repeat(40)).as_str())
        );
    }

    #[test]
    fn windows_command_line_quotes_spaces_quotes_and_trailing_backslashes() {
        fn quote(value: &str) -> String {
            let mut output = Vec::new();
            append_windows_argument(&mut output, &value.encode_utf16().collect::<Vec<_>>());
            String::from_utf16(&output).unwrap()
        }
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("two words"), "\"two words\"");
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote("space\\"), "space\\");
        assert_eq!(quote("two words\\"), "\"two words\\\\\"");
    }
}
