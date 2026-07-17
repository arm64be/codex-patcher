use crate::paths::PatcherPaths;
use crate::{
    config::Config,
    patchset::PatchSet,
    state::{InstallState, StateStore},
    types::{DesiredBuild, ProbeKind, ProbeState},
    upstream::{ResolveOptions, resolve_with_outcome},
};
use anyhow::Result;
use chrono::{DateTime, TimeDelta, Utc};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

const LAUNCH_NETWORK_BUDGET: Duration = Duration::from_secs(3);
const UNREACHABLE_RETRY_SECONDS: i64 = 30;

/// Resolve all local and remote freshness inputs before a managed Codex launch.
///
/// Local patch changes are never deferred. GitHub work uses the normal HTTP
/// cache and a small total network budget; if GitHub is unreachable, expired
/// cached responses remain usable and an active source can still be rebuilt
/// with a changed local patch stack.
pub fn refresh(paths: &PatcherPaths) -> Result<InstallState> {
    let store = StateStore::new(paths.clone());
    let _manager_lock = store.manager_lock()?;
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

    if cached_remote_freshness_is_valid(&initial, &config, &patches, &target, Utc::now()) {
        return Ok(initial);
    }

    let fallback = initial.active.as_ref().and_then(|active| {
        (active.source.channel == config.branch.as_str() && active.target == target).then(|| {
            DesiredBuild {
                source_key: patches.source_key(&active.source, &target),
                patch_fingerprint: patches.fingerprint.clone(),
                source: active.source.clone(),
                target: target.clone(),
            }
        })
    });
    let outcome = resolve_with_outcome(
        config.branch,
        paths.remote_cache_file(),
        initial.resolution_baseline(config.branch.as_str())?,
        ResolveOptions {
            network_budget: Some(LAUNCH_NETWORK_BUDGET),
            stale_if_unreachable: true,
            ..ResolveOptions::default()
        },
    );
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            let message = format!("upstream freshness check failed: {error:#}");
            if !resolution_is_blocked(&message)
                && let Some(desired) = fallback
            {
                return record_desired(
                    &store,
                    &initial.patch_dir,
                    desired,
                    ProbeKind::Degraded,
                    Some(format!(
                        "{message}; using the active Codex source for local patch freshness"
                    )),
                    Some(retry_check_at()),
                );
            }
            return record_resolution_failure(
                &store,
                &initial.patch_dir,
                message,
                resolution_is_blocked(&format!("{error:#}")),
            );
        }
    };
    let desired = DesiredBuild {
        source_key: patches.source_key(&outcome.source, &target),
        patch_fingerprint: patches.fingerprint,
        source: outcome.source,
        target,
    };
    let message = outcome.used_stale_cache.then(|| {
        "GitHub was unreachable within the launch budget; used the last cached upstream response"
            .to_owned()
    });
    record_desired(
        &store,
        &initial.patch_dir,
        desired,
        if outcome.used_stale_cache {
            ProbeKind::Degraded
        } else {
            ProbeKind::Current
        },
        message,
        outcome.next_check_at,
    )
}

fn cached_remote_freshness_is_valid(
    state: &InstallState,
    config: &Config,
    patches: &PatchSet,
    target: &str,
    now: DateTime<Utc>,
) -> bool {
    if !matches!(state.probe.kind, ProbeKind::Current | ProbeKind::Degraded)
        || state.probe.next_check_at.is_none_or(|next| next <= now)
    {
        return false;
    }
    let (Some(active), Some(desired)) = (state.active.as_ref(), state.probe.desired.as_ref())
    else {
        return false;
    };
    let expected_source_key = patches.source_key(&active.source, target);
    active.source.channel == config.branch.as_str()
        && active.target == target
        && active.patch_fingerprint == patches.fingerprint
        && active.source_key == expected_source_key
        && desired.source == active.source
        && desired.target == target
        && desired.patch_fingerprint == patches.fingerprint
        && desired.source_key == expected_source_key
}

fn retry_check_at() -> DateTime<Utc> {
    Utc::now() + TimeDelta::seconds(UNREACHABLE_RETRY_SECONDS)
}

fn record_desired(
    store: &StateStore,
    expected_patch_dir: &Path,
    desired: DesiredBuild,
    current_kind: ProbeKind,
    message: Option<String>,
    next_check_at: Option<DateTime<Utc>>,
) -> Result<InstallState> {
    store.with_state_lock(|| {
        let mut latest = store.require()?;
        if latest.patch_dir != expected_patch_dir {
            return Ok(latest);
        }
        let current = latest
            .active
            .as_ref()
            .is_some_and(|active| active.source_key == desired.source_key);
        let cached_failure = latest
            .failure
            .as_ref()
            .filter(|failure| failure.desired.source_key == desired.source_key)
            .map(|failure| failure.summary.clone());
        if latest
            .failure
            .as_ref()
            .is_some_and(|failure| failure.desired.source_key != desired.source_key)
        {
            latest.failure = None;
        }
        latest.probe = ProbeState {
            kind: if cached_failure.is_some() {
                ProbeKind::Failed
            } else if current {
                current_kind
            } else {
                ProbeKind::Pending
            },
            checked_at: Some(Utc::now()),
            next_check_at,
            desired: Some(desired),
            message: if let Some(summary) = cached_failure {
                Some(format!(
                    "cached build failure for this patch set: {summary}; use `codex-patcher update --retry`"
                ))
            } else {
                message
            },
        };
        store.save(&latest)?;
        Ok(latest)
    })
}

fn record_resolution_failure(
    store: &StateStore,
    expected_patch_dir: &Path,
    message: String,
    blocked: bool,
) -> Result<InstallState> {
    append_refresh_log(store, &message);
    store.with_state_lock(|| {
        let mut latest = store.require()?;
        if latest.patch_dir != expected_patch_dir {
            return Ok(latest);
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
            next_check_at: Some(retry_check_at()),
            desired: latest.probe.desired.clone(),
            message: Some(message),
        };
        store.save(&latest)?;
        Ok(latest)
    })
}

fn record_local_failure(
    store: &StateStore,
    expected_patch_dir: &Path,
    error: anyhow::Error,
) -> Result<InstallState> {
    append_refresh_log(store, &format!("local patch input is invalid: {error:#}"));
    store.with_state_lock(|| {
        let mut latest = store.require()?;
        if latest.patch_dir != expected_patch_dir {
            return Ok(latest);
        }
        latest.failure = None;
        latest.probe = ProbeState {
            kind: ProbeKind::Failed,
            checked_at: Some(Utc::now()),
            next_check_at: None,
            desired: latest.probe.desired.clone(),
            message: Some(format!("local patch input is invalid: {error:#}")),
        };
        store.save(&latest)?;
        Ok(latest)
    })
}

fn append_refresh_log(store: &StateStore, message: &str) {
    if let Ok(mut log) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(store.paths().logs_dir().join("runtime.log"))
    {
        let _ = writeln!(log, "{} {message}", Utc::now().to_rfc3339());
    }
}

fn resolution_is_blocked(message: &str) -> bool {
    [
        "retag",
        "moved tag",
        "rollback",
        "downgrade",
        "release deletion",
        "non-fast-forward",
        "--accept-force-push",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}
