use crate::paths::PatcherPaths;
use crate::{
    config::Config,
    patchset::PatchSet,
    state::StateStore,
    types::{DesiredBuild, ProbeKind, ProbeState},
    upstream::{ResolveOptions, resolve},
};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use std::fs::OpenOptions;
use std::process::{Command, Stdio};

pub fn spawn_detached(paths: &PatcherPaths) -> Result<()> {
    let manager = paths.manager_binary();
    if !manager.is_file() {
        bail!(
            "installed codex-patcher manager is missing: {}",
            manager.display()
        );
    }
    paths.ensure()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.logs_dir().join("probe.log"))?;
    let mut command = Command::new(manager);
    command
        .arg("__probe")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .env("CODEX_PATCHER_INTERNAL", "probe");

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                let child = libc::fork();
                if child == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if child > 0 {
                    libc::_exit(0);
                }
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{
            CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
        };
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    // std::process uses CreateProcessW with handle inheritance enabled when
    // redirecting stdio. Without this guard, a detached probe can also inherit
    // the dispatcher's unrelated stdout/stderr pipes and keep callers waiting
    // for EOF until the probe exits.
    #[cfg(windows)]
    let standard_handles = WindowsStandardHandleInheritanceGuard::new()?;
    let child = command
        .spawn()
        .context("spawning detached freshness probe")?;
    #[cfg(windows)]
    drop(standard_handles);
    #[cfg(unix)]
    {
        let mut child = child;
        child
            .wait()
            .context("reaping detached freshness-probe intermediary")?;
    }
    #[cfg(not(unix))]
    drop(child);
    Ok(())
}

#[cfg(windows)]
struct WindowsStandardHandleInheritanceGuard {
    handles: Vec<windows_sys::Win32::Foundation::HANDLE>,
}

#[cfg(windows)]
impl WindowsStandardHandleInheritanceGuard {
    fn new() -> Result<Self> {
        use windows_sys::Win32::Foundation::{
            GetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
        };
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };

        let mut guard = Self {
            handles: Vec::new(),
        };
        for (name, kind) in [
            ("stdin", STD_INPUT_HANDLE),
            ("stdout", STD_OUTPUT_HANDLE),
            ("stderr", STD_ERROR_HANDLE),
        ] {
            let handle = unsafe { GetStdHandle(kind) };
            if handle.is_null() || handle == INVALID_HANDLE_VALUE || guard.handles.contains(&handle)
            {
                continue;
            }
            let mut flags = 0;
            if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("inspect inherited {name} handle"));
            }
            if flags & HANDLE_FLAG_INHERIT == 0 {
                continue;
            }
            if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("suppress {name} inheritance for detached probe"));
            }
            guard.handles.push(handle);
        }
        Ok(guard)
    }
}

#[cfg(windows)]
impl Drop for WindowsStandardHandleInheritanceGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

        for handle in self.handles.drain(..) {
            unsafe {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
            }
        }
    }
}

pub fn run_internal(paths: &PatcherPaths) -> Result<()> {
    let store = StateStore::new(paths.clone());
    let Some(_probe_lock) = store.try_probe_lock()? else {
        return Ok(());
    };
    let Some(_manager_lock) = store.try_manager_lock()? else {
        return Ok(());
    };
    let initial = store.require()?;

    let inputs = (|| {
        let config = Config::load(initial.patch_dir.join("codex-patcher.toml"))?;
        let patches = PatchSet::load(&initial.patch_dir)?;
        let target = config.resolved_target()?;
        Ok::<_, anyhow::Error>((config, patches, target))
    })();
    let (config, patches, target) = match inputs {
        Ok(inputs) => inputs,
        Err(error) => {
            return record_local_failure(&store, &initial.patch_dir, error);
        }
    };
    let source = match resolve(
        config.branch,
        paths.remote_cache_file(),
        initial.resolution_baseline(config.branch.as_str())?,
        ResolveOptions::default(),
    ) {
        Ok(source) => source,
        Err(error) => {
            let message = format!("upstream probe failed: {error:#}");
            let blocked = message.contains("retag")
                || message.contains("moved tag")
                || message.contains("rollback")
                || message.contains("downgrade")
                || message.contains("release deletion")
                || message.contains("non-fast-forward")
                || message.contains("--accept-force-push");
            return store.with_state_lock(|| {
                let mut latest = store.require()?;
                if latest.patch_dir != initial.patch_dir {
                    return Ok(());
                }
                latest.probe = ProbeState {
                    kind: if blocked {
                        ProbeKind::Blocked
                    } else if latest.active.is_some() {
                        ProbeKind::Degraded
                    } else {
                        ProbeKind::Failed
                    },
                    checked_at: Some(Utc::now()),
                    desired: latest.probe.desired.clone(),
                    message: Some(message),
                };
                store.save(&latest)
            });
        }
    };
    let desired = DesiredBuild {
        source_key: patches.source_key(&source, &target),
        patch_fingerprint: patches.fingerprint,
        source,
        target,
    };

    store.with_state_lock(|| {
        let mut latest = store.require()?;
        if latest.patch_dir != initial.patch_dir {
            return Ok(());
        }
        let current = latest
            .active
            .as_ref()
            .is_some_and(|active| active.source_key == desired.source_key);
        let cached_failure = latest
            .failure
            .as_ref()
            .is_some_and(|failure| failure.desired.source_key == desired.source_key);
        if latest
            .failure
            .as_ref()
            .is_some_and(|failure| failure.desired.source_key != desired.source_key)
        {
            latest.failure = None;
        }
        latest.probe = ProbeState {
            kind: if cached_failure {
                ProbeKind::Failed
            } else if current {
                ProbeKind::Current
            } else {
                ProbeKind::Pending
            },
            checked_at: Some(Utc::now()),
            desired: Some(desired),
            message: cached_failure.then(|| {
                "a deterministic failure is cached for this desired source key; use `codex-patcher update --retry` or repair it"
                    .to_owned()
            }),
        };
        store.save(&latest)
    })
}

fn record_local_failure(
    store: &StateStore,
    expected_patch_dir: &std::path::Path,
    error: anyhow::Error,
) -> Result<()> {
    store.with_state_lock(|| {
        let mut latest = store.require()?;
        if latest.patch_dir != expected_patch_dir {
            return Ok(());
        }
        latest.failure = None;
        latest.probe = ProbeState {
            kind: ProbeKind::Failed,
            checked_at: Some(Utc::now()),
            desired: latest.probe.desired.clone(),
            message: Some(format!("local patch input is invalid: {error:#}")),
        };
        store.save(&latest)
    })
}
