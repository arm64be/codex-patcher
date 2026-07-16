use crate::{BUILD_RECIPE_VERSION, types::ResolvedSource};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

const PATCH_FINGERPRINT_DOMAIN: &[u8] = b"codex-patcher-patchset-v1\0";
const SOURCE_KEY_DOMAIN: &[u8] = b"codex-patcher-source-key-v1\0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub path: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchSet {
    pub root: PathBuf,
    pub patches: Vec<Patch>,
    pub fingerprint: String,
}

impl PatchSet {
    pub fn load(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let metadata = std::fs::symlink_metadata(root)
            .with_context(|| format!("failed to inspect patch directory {}", root.display()))?;
        if metadata.file_type().is_symlink() {
            bail!("patch directory must not be a symlink: {}", root.display());
        }
        if !metadata.is_dir() {
            bail!("patch path is not a directory: {}", root.display());
        }

        let root = root
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", root.display()))?;
        let mut discovered = BTreeMap::<String, PathBuf>::new();
        let mut folded = BTreeMap::<String, String>::new();

        for entry in WalkDir::new(&root).follow_links(false) {
            let entry = entry.with_context(|| format!("failed to walk {}", root.display()))?;
            if entry.path() == root {
                continue;
            }
            if entry.file_type().is_symlink() {
                bail!(
                    "symlinks are not allowed in patch directories: {}",
                    entry.path().display()
                );
            }

            let relative = entry
                .path()
                .strip_prefix(&root)
                .expect("walked path must be below root");
            let relative = portable_relative_path(relative)?;
            let casefold = casefold_path(&relative);
            if casefold == "series" && relative != "series" {
                bail!("path {relative:?} conflicts with reserved series file");
            }
            if !entry.file_type().is_file() {
                continue;
            }
            if !is_patch_name(&relative) {
                continue;
            }
            if let Some(existing) = folded.insert(casefold, relative.clone()) {
                bail!("patch paths collide under case folding: {existing:?} and {relative:?}");
            }
            discovered.insert(relative, entry.path().to_owned());
        }

        let series_path = root.join("series");
        let ordered_paths = if series_path.exists() {
            load_series(&series_path, &discovered)?
        } else {
            discovered.keys().cloned().collect()
        };

        let mut patches = Vec::with_capacity(ordered_paths.len());
        for relative in ordered_paths {
            let path = discovered
                .get(&relative)
                .expect("validated series path must be discovered");
            let bytes = std::fs::read(path)
                .with_context(|| format!("failed to read patch {}", path.display()))?;
            patches.push(Patch {
                path: relative,
                bytes,
            });
        }

        let fingerprint = fingerprint(&patches);
        Ok(Self {
            root,
            patches,
            fingerprint,
        })
    }

    pub fn source_key(&self, source: &ResolvedSource, target: &str) -> String {
        let mut hash = Sha256::new();
        hash.update(SOURCE_KEY_DOMAIN);
        hash_field(&mut hash, &BUILD_RECIPE_VERSION.to_be_bytes());
        hash_field(&mut hash, source.channel.as_bytes());
        hash_field(&mut hash, source.ref_name.as_bytes());
        hash_field(&mut hash, source.ref_object_oid.as_bytes());
        hash_field(&mut hash, source.commit_oid.as_bytes());
        hash_field(&mut hash, source.version.as_bytes());
        hash_field(&mut hash, self.fingerprint.as_bytes());
        hash_field(&mut hash, target.as_bytes());
        hex::encode(hash.finalize())
    }

    pub fn is_empty(&self) -> bool {
        self.patches.is_empty()
    }
}

fn load_series(series_path: &Path, discovered: &BTreeMap<String, PathBuf>) -> Result<Vec<String>> {
    let metadata = std::fs::symlink_metadata(series_path)
        .with_context(|| format!("failed to inspect {}", series_path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("series must be a regular file: {}", series_path.display());
    }
    let bytes = std::fs::read(series_path)
        .with_context(|| format!("failed to read {}", series_path.display()))?;
    let text = std::str::from_utf8(&bytes).context("series is not valid UTF-8")?;

    let mut ordered = Vec::new();
    let mut folded = BTreeSet::new();
    for (index, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line != raw_line {
            bail!(
                "series line {} has leading or trailing whitespace",
                index + 1
            );
        }
        validate_series_entry(line)
            .with_context(|| format!("invalid series entry on line {}", index + 1))?;
        if !folded.insert(casefold_path(line)) {
            bail!("duplicate or case-colliding series entry {line:?}");
        }
        if !discovered.contains_key(line) {
            bail!("series entry does not name a regular patch file: {line:?}");
        }
        ordered.push(line.to_owned());
    }

    let listed: BTreeSet<_> = ordered.iter().cloned().collect();
    let unlisted: Vec<_> = discovered
        .keys()
        .filter(|path| !listed.contains(*path))
        .cloned()
        .collect();
    if !unlisted.is_empty() {
        bail!(
            "series does not list every patch file: {}",
            unlisted.join(", ")
        );
    }
    Ok(ordered)
}

fn validate_series_entry(entry: &str) -> Result<()> {
    let path = Path::new(entry);
    if entry.contains(['\\', '\0'])
        || !is_patch_name(entry)
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("unsafe patch path {entry:?}");
    }
    Ok(())
}

fn portable_relative_path(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        let Component::Normal(part) = component else {
            bail!("unsafe relative path {}", path.display());
        };
        let part = part
            .to_str()
            .with_context(|| format!("patch path is not valid UTF-8: {}", path.display()))?;
        if part.contains('\\') || part.contains('\0') {
            bail!("unsafe patch path {}", path.display());
        }
        parts.push(part);
    }
    Ok(parts.join("/"))
}

fn is_patch_name(path: &str) -> bool {
    Path::new(path).extension().and_then(|value| value.to_str()) == Some("patch")
}

fn casefold_path(path: &str) -> String {
    path.nfkc().flat_map(char::to_lowercase).nfkc().collect()
}

fn fingerprint(patches: &[Patch]) -> String {
    let mut hash = Sha256::new();
    hash.update(PATCH_FINGERPRINT_DOMAIN);
    for patch in patches {
        hash_field(&mut hash, patch.path.as_bytes());
        hash_field(&mut hash, &patch.bytes);
    }
    hex::encode(hash.finalize())
}

fn hash_field(hash: &mut Sha256, field: &[u8]) {
    hash.update((field.len() as u64).to_be_bytes());
    hash.update(field);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn fallback_order_fingerprints_and_source_keys_are_deterministic() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("z.patch"), b"z").unwrap();
        fs::create_dir(root.path().join("a")).unwrap();
        fs::write(root.path().join("a/2.patch"), b"two").unwrap();
        fs::write(root.path().join("a/10.patch"), b"ten").unwrap();

        let set = PatchSet::load(root.path()).unwrap();
        let paths: Vec<_> = set
            .patches
            .iter()
            .map(|patch| patch.path.as_str())
            .collect();
        assert_eq!(paths, ["a/10.patch", "a/2.patch", "z.patch"]);
        fs::write(root.path().join("z.patch"), b"z\n").unwrap();
        assert_ne!(
            set.fingerprint,
            PatchSet::load(root.path()).unwrap().fingerprint
        );
        let source = ResolvedSource {
            channel: "stable".into(),
            ref_name: "refs/tags/rust-v1.2.3".into(),
            ref_object_oid: "a".repeat(40),
            commit_oid: "b".repeat(40),
            version: "1.2.3".into(),
            release_url: None,
        };
        assert_ne!(
            set.source_key(&source, "x86_64-unknown-linux-musl"),
            set.source_key(&source, "aarch64-unknown-linux-musl")
        );
    }

    #[test]
    fn series_controls_order_and_enforces_portable_complete_paths() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("a.patch"), b"a").unwrap();
        fs::write(root.path().join("b.patch"), b"b").unwrap();
        fs::write(root.path().join("series"), b"b.patch\na.patch\n").unwrap();
        let set = PatchSet::load(root.path()).unwrap();
        assert_eq!(set.patches[0].path, "b.patch");

        fs::write(root.path().join("series"), b"a.patch\n").unwrap();
        assert!(PatchSet::load(root.path()).is_err());
        fs::write(root.path().join("series"), b"../a.patch\n").unwrap();
        assert!(PatchSet::load(root.path()).is_err());
        fs::write(root.path().join("series"), b"a.patch\nA.patch\nb.patch\n").unwrap();
        assert!(PatchSet::load(root.path()).is_err());

        fs::remove_file(root.path().join("series")).unwrap();
        if supports_case_distinct_paths(root.path()) {
            fs::write(root.path().join("A.patch"), b"A").unwrap();
            assert!(PatchSet::load(root.path()).is_err());
            fs::remove_file(root.path().join("A.patch")).unwrap();
        }

        fs::create_dir(root.path().join("Series")).unwrap();
        assert!(PatchSet::load(root.path()).is_err());
        assert_eq!(casefold_path("É.patch"), casefold_path("e\u{301}.patch"));
        assert_eq!(casefold_path("Ａ.patch"), casefold_path("a.patch"));
    }

    fn supports_case_distinct_paths(root: &Path) -> bool {
        let lower = root.join("case-probe.tmp");
        let upper = root.join("CASE-PROBE.tmp");
        fs::write(&lower, b"lower").unwrap();
        fs::write(&upper, b"upper").unwrap();
        let distinct = fs::read(&lower).ok().as_deref() == Some(b"lower".as_slice())
            && fs::read(&upper).ok().as_deref() == Some(b"upper".as_slice());
        let _ = fs::remove_file(&lower);
        let _ = fs::remove_file(&upper);
        distinct
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_rejected() {
        use std::os::unix::fs::symlink;
        let root = tempdir().unwrap();
        let outside = root.path().join("outside");
        fs::write(&outside, b"patch").unwrap();
        symlink(&outside, root.path().join("linked.patch")).unwrap();
        assert!(PatchSet::load(root.path()).is_err());
    }
}
