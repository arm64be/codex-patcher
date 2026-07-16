//! Resolve the source revision for the configured Codex release channel.
//!
//! Git refs alone are not a sufficient publication signal: the release workflow
//! creates a tag before all release work has completed. Stable and alpha
//! therefore select only canonical `rust-v*` tags that also have a published,
//! non-draft GitHub Release.

use crate::config::Branch;
use crate::state::atomic_write;
use crate::types::ResolvedSource;
use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use regex::Regex;
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, ETAG, IF_NONE_MATCH, USER_AGENT};
use semver::Version;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

const CACHE_SCHEMA: u32 = 1;
const FALLBACK_POLL_FLOOR_SECONDS: u64 = 300;
const MAX_TAG_DEPTH: usize = 8;
const DEFAULT_API_BASE: &str = "https://api.github.com/repos/openai/codex";
const GITHUB_ACCEPT: &str = "application/vnd.github+json";
const GITHUB_API_VERSION: &str = "2022-11-28";
const CLIENT_USER_AGENT: &str = "codex-patcher/0.1";

static RELEASE_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^refs/tags/rust-v(?P<version>[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?)$",
    )
    .expect("release tag regex must compile")
});

/// Network and trust-policy controls for [`resolve`].
#[derive(Debug, Clone)]
pub struct ResolveOptions {
    /// Bypass the local 300-second fallback cache. An explicit polling floor
    /// advertised by GitHub is still obeyed.
    pub force: bool,
    /// Permit an existing release tag to resolve to a different Git object.
    pub accept_retag: bool,
    /// Permit `main` to move to a commit that is not a fast-forward from the
    /// last activated nightly generation.
    pub accept_force_push: bool,
    /// Repository-scoped GitHub API base. Overridable for deterministic tests.
    pub api_base: String,
    /// Optional GitHub token. If absent, `GITHUB_TOKEN` and then `GH_TOKEN` are
    /// consulted.
    pub token: Option<String>,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            force: false,
            accept_retag: false,
            accept_force_push: false,
            api_base: DEFAULT_API_BASE.to_owned(),
            token: None,
        }
    }
}

/// Resolve the newest usable source revision for `branch`.
///
/// `cache_file` stores ETags and exact response bodies. `previous_source` is
/// used to reject a moved stable/alpha tag unless `accept_retag` is enabled.
pub fn resolve(
    branch: Branch,
    cache_file: impl AsRef<Path>,
    previous_source: Option<&ResolvedSource>,
    options: ResolveOptions,
) -> Result<ResolvedSource> {
    let mut resolver = Resolver::new(cache_file.as_ref(), options)?;
    let result = (|| {
        if branch != Branch::Nightly && !resolver.options.accept_retag {
            resolver.verify_previous_release(branch, previous_source)?;
        }
        let source = resolver.resolve_branch(branch)?;
        reject_release_regression(
            branch,
            previous_source,
            &source,
            resolver.options.accept_retag,
        )?;
        reject_retag(
            branch,
            previous_source,
            &source,
            resolver.options.accept_retag,
        )?;
        resolver.reject_nightly_force_push(previous_source, &source)?;
        Ok(source)
    })();
    let cache_result = resolver.save_cache();

    let source = match (result, cache_result) {
        (Ok(source), Ok(())) => source,
        (Ok(_), Err(error)) => return Err(error),
        (Err(error), Ok(())) => return Err(error),
        (Err(error), Err(cache_error)) => {
            return Err(error.context(format!("also failed to save HTTP cache: {cache_error:#}")));
        }
    };
    Ok(source)
}

struct Resolver {
    client: Client,
    options: ResolveOptions,
    cache_path: PathBuf,
    cache: HttpCache,
    cache_dirty: bool,
}

impl Resolver {
    fn new(cache_path: &Path, mut options: ResolveOptions) -> Result<Self> {
        if options.api_base.trim().is_empty() {
            bail!("GitHub API base must not be empty");
        }
        options.api_base = options.api_base.trim_end_matches('/').to_owned();
        if options.token.as_deref().is_none_or(str::is_empty) {
            options.token = std::env::var("GITHUB_TOKEN")
                .ok()
                .filter(|token| !token.is_empty())
                .or_else(|| {
                    std::env::var("GH_TOKEN")
                        .ok()
                        .filter(|token| !token.is_empty())
                });
        }

        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .build()
            .context("failed to construct GitHub HTTP client")?;
        let cache = HttpCache::load(cache_path)?;
        Ok(Self {
            client,
            options,
            cache_path: cache_path.to_owned(),
            cache,
            cache_dirty: false,
        })
    }

    fn resolve_branch(&mut self, branch: Branch) -> Result<ResolvedSource> {
        match branch {
            Branch::Stable | Branch::Alpha => self.resolve_release(branch),
            Branch::Nightly => self.resolve_nightly(),
        }
    }

    fn resolve_release(&mut self, branch: Branch) -> Result<ResolvedSource> {
        let refs = self.release_refs()?;
        let candidates = release_candidates(branch, refs)?;
        if candidates.is_empty() {
            bail!("no canonical {} Codex release tags were found", branch);
        }

        for candidate in candidates {
            let Some(release) = self.release_for_tag(&candidate.tag_name)? else {
                continue;
            };
            if !release_is_published(branch, &candidate.tag_name, &release)? {
                continue;
            }

            let ref_object_oid = candidate.object.sha.clone();
            validate_oid(&ref_object_oid).context("release ref has an invalid object ID")?;
            let commit_oid = self.peel_to_commit(&candidate.tag_name, candidate.object)?;
            return Ok(ResolvedSource {
                channel: branch.as_str().to_owned(),
                ref_name: candidate.ref_name,
                ref_object_oid,
                commit_oid,
                version: candidate.version.to_string(),
                release_url: Some(release.html_url),
            });
        }

        bail!(
            "no canonical {} Codex tag has a published, non-draft GitHub Release",
            branch
        )
    }

    /// Revalidate the complete trust anchor even when a newer release will be
    /// selected. Looking only at the winning tag would let an older active or
    /// pending release be silently deleted, unpublished, or retagged during an
    /// otherwise ordinary upgrade.
    fn verify_previous_release(
        &mut self,
        branch: Branch,
        previous: Option<&ResolvedSource>,
    ) -> Result<()> {
        let Some(previous) = previous.filter(|source| source.channel == branch.as_str()) else {
            return Ok(());
        };
        let refs = self.release_refs()?;
        let git_ref = refs
            .into_iter()
            .find(|git_ref| git_ref.ref_name == previous.ref_name)
            .with_context(|| {
                format!(
                    "previously trusted release tag {} was deleted; refusing without --accept-retag",
                    previous.ref_name
                )
            })?;
        let captures = RELEASE_TAG.captures(&git_ref.ref_name).with_context(|| {
            format!(
                "stored release ref {} is no longer canonical",
                git_ref.ref_name
            )
        })?;
        let version = captures
            .name("version")
            .expect("release version capture is required")
            .as_str();
        if version != previous.version {
            bail!(
                "stored release {} claims version {}, but its canonical tag encodes {}; refusing without --accept-retag",
                previous.ref_name,
                previous.version,
                version
            );
        }
        let tag_name = git_ref
            .ref_name
            .strip_prefix("refs/tags/")
            .expect("canonical release refs include a refs/tags prefix");
        let ref_object_oid = git_ref.object.sha.clone();
        let commit_oid = self.peel_to_commit(tag_name, git_ref.object)?;
        if ref_object_oid != previous.ref_object_oid || commit_oid != previous.commit_oid {
            bail!(
                "previously trusted release tag {} moved from {}/{} to {}/{}; refusing without --accept-retag",
                previous.ref_name,
                previous.ref_object_oid,
                previous.commit_oid,
                ref_object_oid,
                commit_oid
            );
        }
        let release = self
            .release_for_tag(tag_name)?
            .with_context(|| {
                format!(
                    "published release for previously trusted tag {} was deleted; refusing without --accept-retag",
                    previous.ref_name
                )
            })?;
        if !release_is_published(branch, tag_name, &release)? {
            bail!(
                "previously trusted release {} is no longer published and eligible; refusing without --accept-retag",
                previous.ref_name
            );
        }
        Ok(())
    }

    fn resolve_nightly(&mut self) -> Result<ResolvedSource> {
        let url = format!("{}/git/ref/heads/main", self.options.api_base);
        let response = self.get(&url)?;
        expect_status(&url, &response, StatusCode::OK)?;
        let git_ref: GitRef = parse_json(&url, &response.body)?;
        if git_ref.ref_name != "refs/heads/main" {
            bail!(
                "GitHub returned unexpected main ref name {:?}",
                git_ref.ref_name
            );
        }
        if git_ref.object.kind != "commit" {
            bail!(
                "refs/heads/main points to unsupported Git object type {:?}",
                git_ref.object.kind
            );
        }
        validate_oid(&git_ref.object.sha).context("main has an invalid commit ID")?;
        Ok(ResolvedSource {
            channel: branch_name(Branch::Nightly).to_owned(),
            ref_name: git_ref.ref_name,
            ref_object_oid: git_ref.object.sha.clone(),
            commit_oid: git_ref.object.sha,
            version: "0.0.0".to_owned(),
            release_url: None,
        })
    }

    fn reject_nightly_force_push(
        &mut self,
        previous: Option<&ResolvedSource>,
        current: &ResolvedSource,
    ) -> Result<()> {
        if self.options.accept_force_push || current.channel != Branch::Nightly.as_str() {
            return Ok(());
        }
        let Some(previous) = previous.filter(|source| source.channel == current.channel) else {
            return Ok(());
        };
        if previous.commit_oid == current.commit_oid {
            return Ok(());
        }

        let url = format!(
            "{}/compare/{}...{}",
            self.options.api_base, previous.commit_oid, current.commit_oid
        );
        let response = self.get(&url)?;
        expect_status(&url, &response, StatusCode::OK)?;
        let comparison: GitComparison = parse_json(&url, &response.body)?;
        if comparison.status != "ahead" {
            bail!(
                "upstream main moved from {} to {} with comparison status {:?}; refusing a non-fast-forward change without --accept-force-push",
                previous.commit_oid,
                current.commit_oid,
                comparison.status
            );
        }
        Ok(())
    }

    fn release_refs(&mut self) -> Result<Vec<GitRef>> {
        // Unlike the repository-listing endpoints, Git matching-refs returns
        // the complete matching set and does not paginate it.
        let url = format!("{}/git/matching-refs/tags/rust-v", self.options.api_base);
        let response = self.get(&url)?;
        expect_status(&url, &response, StatusCode::OK)?;
        parse_json(&url, &response.body)
    }

    fn release_for_tag(&mut self, tag_name: &str) -> Result<Option<GitRelease>> {
        let url = format!("{}/releases/tags/{tag_name}", self.options.api_base);
        let response = self.get(&url)?;
        match StatusCode::from_u16(response.status).context("cached invalid HTTP status")? {
            StatusCode::OK => parse_json(&url, &response.body).map(Some),
            StatusCode::NOT_FOUND => Ok(None),
            _ => {
                unexpected_status(&url, &response)?;
                unreachable!("unexpected_status always returns an error")
            }
        }
    }

    fn peel_to_commit(&mut self, tag_name: &str, mut object: GitObject) -> Result<String> {
        let mut visited = BTreeSet::new();
        for depth in 0..=MAX_TAG_DEPTH {
            validate_oid(&object.sha).context("tag contains an invalid Git object ID")?;
            match object.kind.as_str() {
                "commit" => return Ok(object.sha),
                "tag" if depth < MAX_TAG_DEPTH => {
                    if !visited.insert(object.sha.clone()) {
                        bail!("annotated tag {tag_name:?} contains an object cycle");
                    }
                    let url = format!("{}/git/tags/{}", self.options.api_base, object.sha);
                    let response = self.get(&url)?;
                    expect_status(&url, &response, StatusCode::OK)?;
                    let tag: GitTag = parse_json(&url, &response.body)?;
                    if depth == 0 && tag.tag != tag_name {
                        bail!(
                            "annotated tag object name {:?} does not match ref {tag_name:?}",
                            tag.tag
                        );
                    }
                    object = tag.object;
                }
                "tag" => bail!(
                    "annotated tag {tag_name:?} exceeds the maximum peel depth of {MAX_TAG_DEPTH}"
                ),
                other => bail!(
                    "release tag {tag_name:?} resolves to unsupported Git object type {other:?}"
                ),
            }
        }
        unreachable!("tag peel loop returns at its depth bound")
    }

    fn get(&mut self, url: &str) -> Result<CachedBody> {
        let now = Utc::now();
        if let Some(cached) = self.cache.entries.get(url)
            && cache_entry_is_fresh(cached, now, self.options.force)
        {
            return Ok(cached.body());
        }

        let previous = self.cache.entries.get(url).cloned();
        let mut request = self
            .client
            .get(url)
            .header(USER_AGENT, CLIENT_USER_AGENT)
            .header(ACCEPT, GITHUB_ACCEPT)
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION);
        if let Some(token) = self.options.token.as_deref() {
            request = request.bearer_auth(token);
        }
        if let Some(etag) = previous.as_ref().and_then(|entry| entry.etag.as_deref()) {
            request = request.header(IF_NONE_MATCH, etag);
        }

        let response = request
            .send()
            .with_context(|| format!("failed to request GitHub endpoint {url}"))?;
        let status = response.status();
        let response_etag = header_string(response.headers(), ETAG)?;
        let response_floor = poll_floor(response.headers())?;

        if status == StatusCode::NOT_MODIFIED {
            let mut entry = previous.ok_or_else(|| {
                anyhow!("GitHub returned 304 for {url}, but no cached response exists")
            })?;
            entry.checked_at = now;
            entry.etag = response_etag.or(entry.etag);
            if let Some(response_floor) = response_floor {
                entry.poll_floor_seconds = response_floor;
                entry.poll_floor_advertised = true;
            }
            let body = entry.body();
            self.cache.entries.insert(url.to_owned(), entry);
            self.cache_dirty = true;
            return Ok(body);
        }

        let body = response
            .text()
            .with_context(|| format!("failed to read GitHub response body for {url}"))?;
        let entry = CacheEntry {
            status: status.as_u16(),
            etag: response_etag,
            checked_at: now,
            poll_floor_seconds: response_floor.unwrap_or(FALLBACK_POLL_FLOOR_SECONDS),
            poll_floor_advertised: response_floor.is_some(),
            body,
        };
        let result = entry.body();
        self.cache.entries.insert(url.to_owned(), entry);
        self.cache_dirty = true;
        Ok(result)
    }

    fn save_cache(&mut self) -> Result<()> {
        if !self.cache_dirty {
            return Ok(());
        }
        self.cache.save(&self.cache_path)?;
        self.cache_dirty = false;
        Ok(())
    }
}

fn branch_name(branch: Branch) -> &'static str {
    branch.as_str()
}

#[derive(Debug, Clone, Deserialize)]
struct GitRef {
    #[serde(rename = "ref")]
    ref_name: String,
    object: GitObject,
}

#[derive(Debug, Clone, Deserialize)]
struct GitObject {
    sha: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct GitTag {
    tag: String,
    object: GitObject,
}

#[derive(Debug, Clone, Deserialize)]
struct GitRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    published_at: Option<DateTime<Utc>>,
    html_url: String,
}

#[derive(Debug, Deserialize)]
struct GitComparison {
    status: String,
}

#[derive(Debug, Clone)]
struct TagCandidate {
    ref_name: String,
    tag_name: String,
    version: Version,
    object: GitObject,
}

fn release_candidates(branch: Branch, refs: Vec<GitRef>) -> Result<Vec<TagCandidate>> {
    if branch == Branch::Nightly {
        bail!("nightly does not use release tag candidates");
    }

    let mut candidates = Vec::new();
    let mut seen = BTreeMap::<String, GitObject>::new();
    for git_ref in refs {
        let Some(captures) = RELEASE_TAG.captures(&git_ref.ref_name) else {
            continue;
        };
        let version_text = captures
            .name("version")
            .expect("version capture is required")
            .as_str();
        let Ok(version) = Version::parse(version_text) else {
            continue;
        };
        if version.to_string() != version_text {
            continue;
        }
        let belongs = match branch {
            Branch::Stable => version.pre.is_empty(),
            Branch::Alpha => !version.pre.is_empty(),
            Branch::Nightly => unreachable!(),
        };
        if !belongs {
            continue;
        }

        if let Some(existing) = seen.get(&git_ref.ref_name) {
            if existing.sha != git_ref.object.sha || existing.kind != git_ref.object.kind {
                bail!(
                    "GitHub returned conflicting objects for ref {:?}",
                    git_ref.ref_name
                );
            }
            continue;
        }
        seen.insert(git_ref.ref_name.clone(), git_ref.object.clone());
        candidates.push(TagCandidate {
            tag_name: git_ref
                .ref_name
                .strip_prefix("refs/tags/")
                .expect("release regex includes refs/tags prefix")
                .to_owned(),
            ref_name: git_ref.ref_name,
            version,
            object: git_ref.object,
        });
    }

    candidates.sort_by(|left, right| {
        right
            .version
            .cmp(&left.version)
            .then_with(|| left.ref_name.cmp(&right.ref_name))
    });
    Ok(candidates)
}

fn release_is_published(branch: Branch, tag_name: &str, release: &GitRelease) -> Result<bool> {
    if release.tag_name != tag_name {
        bail!(
            "GitHub Release tag {:?} does not match requested tag {tag_name:?}",
            release.tag_name
        );
    }
    if release.draft || release.published_at.is_none() {
        return Ok(false);
    }
    let expected_prerelease = branch == Branch::Alpha;
    if release.prerelease != expected_prerelease {
        bail!(
            "published release {tag_name:?} has prerelease={}, expected {} for {} channel",
            release.prerelease,
            expected_prerelease,
            branch
        );
    }
    if release.html_url.is_empty() {
        bail!("published release {tag_name:?} has no HTML URL");
    }
    Ok(true)
}

fn reject_retag(
    branch: Branch,
    previous: Option<&ResolvedSource>,
    current: &ResolvedSource,
    accept_retag: bool,
) -> Result<()> {
    if branch == Branch::Nightly || accept_retag {
        return Ok(());
    }
    let Some(previous) = previous else {
        return Ok(());
    };
    if previous.channel == current.channel
        && previous.ref_name == current.ref_name
        && (previous.ref_object_oid != current.ref_object_oid
            || previous.commit_oid != current.commit_oid)
    {
        bail!(
            "release tag {} moved from {}/{} to {}/{}; refusing retagged source without explicit acceptance",
            current.ref_name,
            previous.ref_object_oid,
            previous.commit_oid,
            current.ref_object_oid,
            current.commit_oid
        );
    }
    Ok(())
}

fn reject_release_regression(
    branch: Branch,
    previous: Option<&ResolvedSource>,
    current: &ResolvedSource,
    accept_retag: bool,
) -> Result<()> {
    if branch == Branch::Nightly || accept_retag {
        return Ok(());
    }
    let Some(previous) = previous.filter(|source| source.channel == current.channel) else {
        return Ok(());
    };
    let previous_version = Version::parse(&previous.version)
        .with_context(|| format!("stored source has invalid SemVer {}", previous.version))?;
    let current_version = Version::parse(&current.version)
        .with_context(|| format!("resolved source has invalid SemVer {}", current.version))?;
    if current_version < previous_version {
        bail!(
            "resolved {} release {} is older than the active release {}; refusing release deletion or downgrade",
            current.channel,
            current.version,
            previous.version
        );
    }
    Ok(())
}

fn validate_oid(oid: &str) -> Result<()> {
    if matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        bail!("invalid Git object ID {oid:?}")
    }
}

#[derive(Debug, Clone)]
struct CachedBody {
    status: u16,
    body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheEntry {
    status: u16,
    etag: Option<String>,
    checked_at: DateTime<Utc>,
    poll_floor_seconds: u64,
    #[serde(default)]
    poll_floor_advertised: bool,
    body: String,
}

impl CacheEntry {
    fn body(&self) -> CachedBody {
        CachedBody {
            status: self.status,
            body: self.body.clone(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpCache {
    schema: u32,
    entries: BTreeMap<String, CacheEntry>,
}

impl Default for HttpCache {
    fn default() -> Self {
        Self {
            schema: CACHE_SCHEMA,
            entries: BTreeMap::new(),
        }
    }
}

impl HttpCache {
    fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let cache: Self = serde_json::from_slice(&bytes)
                    .with_context(|| format!("failed to parse HTTP cache {}", path.display()))?;
                if cache.schema != CACHE_SCHEMA {
                    bail!(
                        "unsupported HTTP cache schema {} in {}; expected {}",
                        cache.schema,
                        path.display(),
                        CACHE_SCHEMA
                    );
                }
                Ok(cache)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => {
                Err(error).with_context(|| format!("failed to read HTTP cache {}", path.display()))
            }
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        let mut bytes =
            serde_json::to_vec_pretty(self).context("failed to serialize HTTP cache")?;
        bytes.push(b'\n');
        atomic_write(path, &bytes)
            .with_context(|| format!("failed to replace HTTP cache {}", path.display()))
    }
}

fn cache_entry_is_fresh(entry: &CacheEntry, now: DateTime<Utc>, force: bool) -> bool {
    if force && !entry.poll_floor_advertised {
        return false;
    }
    let floor = entry.poll_floor_seconds.max(1);
    match now.signed_duration_since(entry.checked_at).to_std() {
        Ok(age) => age < Duration::from_secs(floor),
        Err(_) => true,
    }
}

fn header_string(
    headers: &reqwest::header::HeaderMap,
    name: reqwest::header::HeaderName,
) -> Result<Option<String>> {
    headers
        .get(name)
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .context("GitHub returned a non-text response header")
        })
        .transpose()
}

fn poll_floor(headers: &reqwest::header::HeaderMap) -> Result<Option<u64>> {
    let Some(value) = headers.get("x-poll-interval") else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .context("GitHub returned an invalid X-Poll-Interval header")?;
    let seconds = value
        .parse::<u64>()
        .context("GitHub returned a non-numeric X-Poll-Interval header")?;
    if seconds == 0 {
        bail!("GitHub returned a zero X-Poll-Interval header");
    }
    Ok(Some(seconds))
}

fn parse_json<T: DeserializeOwned>(url: &str, body: &str) -> Result<T> {
    serde_json::from_str(body).with_context(|| format!("GitHub returned invalid JSON for {url}"))
}

fn expect_status(url: &str, response: &CachedBody, expected: StatusCode) -> Result<()> {
    let status = StatusCode::from_u16(response.status).context("cached invalid HTTP status")?;
    if status == expected {
        Ok(())
    } else {
        unexpected_status(url, response)
    }
}

fn unexpected_status(url: &str, response: &CachedBody) -> Result<()> {
    let status = StatusCode::from_u16(response.status).context("cached invalid HTTP status")?;
    let mut snippet: String = response.body.chars().take(500).collect();
    if response.body.chars().count() > 500 {
        snippet.push_str("...");
    }
    bail!("GitHub endpoint {url} returned HTTP {status}: {snippet}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    fn object(byte: char) -> GitObject {
        GitObject {
            sha: byte.to_string().repeat(40),
            kind: "commit".to_owned(),
        }
    }

    fn git_ref(name: &str, byte: char) -> GitRef {
        GitRef {
            ref_name: name.to_owned(),
            object: object(byte),
        }
    }

    fn release(tag: &str, prerelease: bool) -> GitRelease {
        GitRelease {
            tag_name: tag.to_owned(),
            draft: false,
            prerelease,
            published_at: Some(Utc::now()),
            html_url: format!("https://example.invalid/{tag}"),
        }
    }

    #[test]
    fn canonical_release_tags_are_filtered_and_semver_sorted() {
        let refs = vec![
            git_ref("refs/tags/rust-v1.9.0", 'a'),
            git_ref("refs/tags/rust-v2.0.0-alpha.9", 'b'),
            git_ref("refs/tags/rust-v1.10.0", 'c'),
            git_ref("refs/tags/rust-v2.0.0-beta.1", 'd'),
            git_ref("refs/tags/rust-v01.11.0", 'e'),
            git_ref("refs/tags/rust-v2.0.0-rc.1", 'f'),
            git_ref("refs/tags/latest-alpha-cli", '0'),
        ];

        let stable = release_candidates(Branch::Stable, refs.clone()).unwrap();
        assert_eq!(
            stable
                .iter()
                .map(|candidate| candidate.version.to_string())
                .collect::<Vec<_>>(),
            ["1.10.0", "1.9.0"]
        );
        let alpha = release_candidates(Branch::Alpha, refs).unwrap();
        assert_eq!(
            alpha
                .iter()
                .map(|candidate| candidate.version.to_string())
                .collect::<Vec<_>>(),
            ["2.0.0-rc.1", "2.0.0-beta.1", "2.0.0-alpha.9"]
        );
    }

    #[test]
    fn publication_gate_rejects_drafts_and_wrong_release_kinds() {
        let tag = "rust-v1.2.3";
        assert!(release_is_published(Branch::Stable, tag, &release(tag, false)).unwrap());

        let mut draft = release(tag, false);
        draft.draft = true;
        assert!(!release_is_published(Branch::Stable, tag, &draft).unwrap());

        let wrong_kind = release(tag, true);
        assert!(release_is_published(Branch::Stable, tag, &wrong_kind).is_err());
    }

    #[test]
    fn stable_retags_are_rejected_but_nightly_moves_are_normal() {
        let previous = ResolvedSource {
            channel: "stable".to_owned(),
            ref_name: "refs/tags/rust-v1.2.3".to_owned(),
            ref_object_oid: "a".repeat(40),
            commit_oid: "b".repeat(40),
            version: "1.2.3".to_owned(),
            release_url: None,
        };
        let mut current = previous.clone();
        current.commit_oid = "c".repeat(40);
        assert!(reject_retag(Branch::Stable, Some(&previous), &current, false).is_err());
        assert!(reject_retag(Branch::Stable, Some(&previous), &current, true).is_ok());
        assert!(reject_retag(Branch::Nightly, Some(&previous), &current, false).is_ok());
    }

    #[test]
    fn release_deletion_or_downgrade_is_rejected() {
        let previous = ResolvedSource {
            channel: "stable".to_owned(),
            ref_name: "refs/tags/rust-v2.0.0".to_owned(),
            ref_object_oid: "a".repeat(40),
            commit_oid: "b".repeat(40),
            version: "2.0.0".to_owned(),
            release_url: None,
        };
        let mut current = previous.clone();
        current.ref_name = "refs/tags/rust-v1.99.0".to_owned();
        current.version = "1.99.0".to_owned();
        assert!(
            reject_release_regression(Branch::Stable, Some(&previous), &current, false).is_err()
        );
        assert!(reject_release_regression(Branch::Stable, Some(&previous), &current, true).is_ok());

        current.channel = "alpha".to_owned();
        current.version = "3.0.0-alpha.1".to_owned();
        assert!(reject_release_regression(Branch::Alpha, Some(&previous), &current, false).is_ok());
    }

    #[test]
    fn response_cache_obeys_the_poll_floor() {
        let now = Utc::now();
        let entry = CacheEntry {
            status: 200,
            etag: Some("etag".to_owned()),
            checked_at: now - TimeDelta::seconds(299),
            poll_floor_seconds: 300,
            poll_floor_advertised: false,
            body: "{}".to_owned(),
        };
        assert!(cache_entry_is_fresh(&entry, now, false));
        assert!(!cache_entry_is_fresh(&entry, now, true));

        let expired = CacheEntry {
            checked_at: now - TimeDelta::seconds(300),
            ..entry
        };
        assert!(!cache_entry_is_fresh(&expired, now, false));

        let advertised = CacheEntry {
            checked_at: now - TimeDelta::seconds(9),
            poll_floor_seconds: 10,
            poll_floor_advertised: true,
            ..expired
        };
        assert!(cache_entry_is_fresh(&advertised, now, true));
        assert!(!cache_entry_is_fresh(
            &CacheEntry {
                checked_at: now - TimeDelta::seconds(10),
                ..advertised
            },
            now,
            true
        ));
    }

    #[test]
    fn object_ids_must_be_full_hex_values() {
        assert!(validate_oid(&"a".repeat(40)).is_ok());
        assert!(validate_oid(&"f".repeat(64)).is_ok());
        assert!(validate_oid("abc").is_err());
        assert!(validate_oid(&"z".repeat(40)).is_err());
    }
}
