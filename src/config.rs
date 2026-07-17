use crate::CONFIG_SCHEMA;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Branch {
    #[default]
    Stable,
    Alpha,
    Nightly,
}

impl Branch {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Alpha => "alpha",
            Self::Nightly => "nightly",
        }
    }
}

impl fmt::Display for Branch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TargetSpec {
    #[default]
    OfficialNative,
    Triple(String),
}

impl TargetSpec {
    pub fn resolve(&self) -> Result<String> {
        match self {
            Self::OfficialNative => host_target().map(str::to_owned),
            Self::Triple(target) => {
                validate_host_target(target)?;
                Ok(target.clone())
            }
        }
    }
}

impl Serialize for TargetSpec {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::OfficialNative => serializer.serialize_str("native"),
            Self::Triple(target) => serializer.serialize_str(target),
        }
    }
}

impl<'de> Deserialize<'de> for TargetSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if matches!(value.as_str(), "native" | "official-native") {
            Ok(Self::OfficialNative)
        } else {
            validate_target(&value).map_err(serde::de::Error::custom)?;
            Ok(Self::Triple(value))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FailureMode {
    #[default]
    Error,
    LastGood,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NoninteractivePending {
    #[default]
    Auto,
    WarnRun,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema: u32,
    pub branch: Branch,
    #[serde(default)]
    pub target: TargetSpec,
    #[serde(default)]
    pub failure_mode: FailureMode,
    pub noninteractive_pending: NoninteractivePending,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema: CONFIG_SCHEMA,
            branch: Branch::Stable,
            target: TargetSpec::OfficialNative,
            failure_mode: FailureMode::Error,
            noninteractive_pending: NoninteractivePending::Auto,
        }
    }
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let config: Self = toml::from_str(&contents)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != CONFIG_SCHEMA {
            bail!(
                "unsupported config schema {}; expected {}",
                self.schema,
                CONFIG_SCHEMA
            );
        }
        self.target.resolve()?;
        Ok(())
    }

    pub fn resolved_target(&self) -> Result<String> {
        self.target.resolve()
    }
}

pub const SUPPORTED_TARGETS: &[&str] = &[
    "aarch64-apple-darwin",
    "aarch64-pc-windows-msvc",
    "aarch64-unknown-linux-gnu",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
];

pub fn validate_target(target: &str) -> Result<()> {
    if SUPPORTED_TARGETS.contains(&target) {
        Ok(())
    } else {
        bail!(
            "unsupported target {target:?}; supported targets: {}",
            SUPPORTED_TARGETS.join(", ")
        )
    }
}

pub fn validate_host_target(target: &str) -> Result<()> {
    validate_target_for_host(target, std::env::consts::OS, std::env::consts::ARCH)
}

pub fn validate_target_for_host(target: &str, os: &str, arch: &str) -> Result<()> {
    validate_target(target)?;
    let (_, allowed) = targets_for_host(os, arch)?;
    if allowed.contains(&target) {
        Ok(())
    } else {
        bail!(
            "target {target:?} does not match host platform {os}/{arch}; allowed targets: {}",
            allowed.join(", ")
        )
    }
}

pub fn host_target() -> Result<&'static str> {
    resolve_host_target(std::env::consts::OS, std::env::consts::ARCH)
}

pub fn resolve_host_target(os: &str, arch: &str) -> Result<&'static str> {
    targets_for_host(os, arch).map(|(target, _)| target)
}

fn targets_for_host(os: &str, arch: &str) -> Result<(&'static str, &'static [&'static str])> {
    match (os, arch) {
        ("linux", "x86_64") => Ok((
            "x86_64-unknown-linux-gnu",
            &["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"],
        )),
        ("linux", "aarch64") => Ok((
            "aarch64-unknown-linux-gnu",
            &["aarch64-unknown-linux-gnu", "aarch64-unknown-linux-musl"],
        )),
        ("macos", "x86_64") => Ok(("x86_64-apple-darwin", &["x86_64-apple-darwin"])),
        ("macos", "aarch64") => Ok(("aarch64-apple-darwin", &["aarch64-apple-darwin"])),
        ("windows", "x86_64") => Ok(("x86_64-pc-windows-msvc", &["x86_64-pc-windows-msvc"])),
        ("windows", "aarch64") => Ok(("aarch64-pc-windows-msvc", &["aarch64-pc-windows-msvc"])),
        _ => bail!("unsupported host platform {os}/{arch}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_config_spellings_defaults_and_schema_are_strict() {
        let config: Config = toml::from_str(
            "schema=1\nbranch=\"nightly\"\ntarget=\"x86_64-unknown-linux-gnu\"\n\
             failure_mode=\"last-good\"\nnoninteractive_pending=\"warn-run\"",
        )
        .unwrap();
        assert_eq!(config.branch, Branch::Nightly);
        assert_eq!(
            config.target,
            TargetSpec::Triple("x86_64-unknown-linux-gnu".into())
        );
        assert_eq!(config.failure_mode, FailureMode::LastGood);
        assert_eq!(
            config.noninteractive_pending,
            NoninteractivePending::WarnRun
        );
        let minimal: Config =
            toml::from_str("schema=1\nbranch=\"stable\"\nnoninteractive_pending=\"auto\"").unwrap();
        assert_eq!(minimal.target, TargetSpec::OfficialNative);
        assert_eq!(minimal.failure_mode, FailureMode::Error);
        assert!(toml::to_string(&minimal).unwrap().contains("native"));
        let legacy: Config = toml::from_str(
            "schema=1\nbranch=\"stable\"\ntarget=\"official-native\"\n\
             noninteractive_pending=\"auto\"",
        )
        .unwrap();
        assert_eq!(legacy.target, TargetSpec::OfficialNative);
        assert!(
            toml::from_str::<Config>(
                "schema=1\nbranch=\"stable\"\nnoninteractive_pending=\"auto\"\nmystery=true"
            )
            .unwrap_err()
            .to_string()
            .contains("unknown field")
        );
        assert!(toml::from_str::<Config>("").is_err());
    }

    #[test]
    fn host_mapping_and_explicit_targets_are_native_only() {
        assert_eq!(
            resolve_host_target("linux", "x86_64").unwrap(),
            "x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            resolve_host_target("macos", "aarch64").unwrap(),
            "aarch64-apple-darwin"
        );
        assert!(resolve_host_target("freebsd", "x86_64").is_err());
        assert!(validate_target_for_host("x86_64-unknown-linux-gnu", "linux", "x86_64").is_ok());
        assert!(validate_target_for_host("x86_64-unknown-linux-musl", "linux", "x86_64").is_ok());
        assert!(validate_target_for_host("aarch64-unknown-linux-musl", "linux", "x86_64").is_err());
        assert!(validate_target_for_host("x86_64-pc-windows-msvc", "linux", "x86_64").is_err());
    }
}
