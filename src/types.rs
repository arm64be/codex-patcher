use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedSource {
    pub channel: String,
    pub ref_name: String,
    pub ref_object_oid: String,
    pub commit_oid: String,
    pub version: String,
    pub release_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DesiredBuild {
    pub source: ResolvedSource,
    pub patch_fingerprint: String,
    pub target: String,
    pub source_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GenerationRef {
    pub id: String,
    pub package_dir: PathBuf,
    pub binary: PathBuf,
    pub source_key: String,
    pub source: ResolvedSource,
    pub patch_fingerprint: String,
    pub target: String,
    /// Subcommands parsed from the validated binary's help output. The
    /// dispatcher uses this to keep newly introduced service commands out of
    /// its interactive update UI without delaying launches to query help.
    #[serde(default)]
    pub subcommands: Vec<String>,
    pub built_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeKind {
    #[default]
    Unknown,
    Current,
    Pending,
    Degraded,
    Failed,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProbeState {
    pub kind: ProbeKind,
    pub checked_at: Option<DateTime<Utc>>,
    /// Earliest instant at which upstream freshness must be revalidated.
    /// Local config and patch inputs are still inspected on every launch.
    #[serde(default)]
    pub next_check_at: Option<DateTime<Utc>>,
    pub desired: Option<DesiredBuild>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureRecord {
    pub id: String,
    pub desired: DesiredBuild,
    pub phase: String,
    pub summary: String,
    pub failed_patch_index: Option<usize>,
    pub failed_patch: Option<String>,
    pub log_path: PathBuf,
    pub created_at: DateTime<Utc>,
    pub repair_worktree: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileHash {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationManifest {
    pub schema: u32,
    pub generation: GenerationRef,
    pub outputs: Vec<FileHash>,
    pub rustc: Option<String>,
    pub cargo: Option<String>,
    pub python: Option<String>,
    pub linker: Option<String>,
    #[serde(default)]
    pub sdk: Option<String>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
}
