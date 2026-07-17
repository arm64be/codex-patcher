#![allow(dead_code)]

use chrono::{TimeDelta, Utc};
use codex_patcher::patchset::PatchSet;
use codex_patcher::paths::{PATCHER_HOME_ENV, PatcherPaths};
use codex_patcher::state::{InstallState, StateStore};
use codex_patcher::types::{
    DesiredBuild, FileHash, GenerationManifest, GenerationRef, ProbeKind, ProbeState,
    ResolvedSource,
};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;
use tempfile::TempDir;

pub struct DispatcherFixture {
    _temp: TempDir,
    pub root: PathBuf,
    pub paths: PatcherPaths,
    pub patch_dir: PathBuf,
    pub wrapper: PathBuf,
    pub active: GenerationRef,
}

impl DispatcherFixture {
    pub fn new(failure_mode: &str, noninteractive_pending: &str) -> Self {
        let temp = tempfile::tempdir().expect("create dispatcher fixture");
        let root = temp.path().to_path_buf();
        let paths = PatcherPaths::from_home(root.join("patcher-home"));
        paths.ensure().expect("create patcher layout");

        let helper = process_helper();
        copy_executable(&helper, &paths.manager);

        let package_dir = paths.generation("active");
        fs::create_dir_all(&package_dir).expect("create active package");
        let active_binary = package_dir.join(executable_name("active-codex"));
        copy_executable(&helper, &active_binary);

        let wrapper = root.join(executable_name("codex"));
        copy_executable(Path::new(env!("CARGO_BIN_EXE_codex-patcher")), &wrapper);

        let patch_dir = root.join("patches");
        fs::create_dir_all(&patch_dir).expect("create patch directory");
        fs::write(
            patch_dir.join("codex-patcher.toml"),
            format!(
                "schema = 1\nbranch = \"stable\"\ntarget = \"native\"\nfailure_mode = \"{failure_mode}\"\nauto_rebuild_patches = true\nnoninteractive_pending = \"{noninteractive_pending}\"\n"
            ),
        )
        .expect("write test config");

        write_remote_cache(&paths, &[source()]);
        let patches = PatchSet::load(&patch_dir).expect("load empty test patch set");
        let source_key = patches.source_key(&source(), &host_target_for_test());

        let active = generation(
            "active",
            &package_dir,
            &active_binary,
            &source_key,
            &patches.fingerprint,
        );
        let mut state = InstallState::new(patch_dir.clone());
        state.activate(active.clone());
        state.probe = ProbeState {
            kind: ProbeKind::Current,
            checked_at: Some(Utc::now()),
            next_check_at: Some(Utc::now() + TimeDelta::minutes(5)),
            desired: Some(desired(&source_key, &patches.fingerprint)),
            message: None,
        };
        StateStore::new(paths.clone())
            .save(&state)
            .expect("write installed state");

        Self {
            _temp: temp,
            root,
            paths,
            patch_dir,
            wrapper,
            active,
        }
    }

    pub fn state(&self) -> InstallState {
        StateStore::new(self.paths.clone())
            .require()
            .expect("load fixture state")
    }

    pub fn save_state(&self, state: &InstallState) {
        StateStore::new(self.paths.clone())
            .save(state)
            .expect("save fixture state");
    }

    pub fn set_auto_rebuild(&self, enabled: bool) {
        let path = self.patch_dir.join("codex-patcher.toml");
        let config = fs::read_to_string(&path).expect("read test config");
        fs::write(
            path,
            config.replace(
                "auto_rebuild_patches = true",
                &format!("auto_rebuild_patches = {enabled}"),
            ),
        )
        .expect("write auto-rebuild setting");
    }

    pub fn changed_patch_desired(&self) -> DesiredBuild {
        fs::write(self.patch_dir.join("series"), "feature.patch\n")
            .expect("write changed patch series");
        fs::write(
            self.patch_dir.join("feature.patch"),
            "test patch contents\n",
        )
        .expect("write changed patch");
        let patches = PatchSet::load(&self.patch_dir).expect("load changed patch set");
        DesiredBuild {
            source_key: patches.source_key(&self.active.source, &self.active.target),
            patch_fingerprint: patches.fingerprint,
            source: self.active.source.clone(),
            target: self.active.target.clone(),
        }
    }

    pub fn install_prebuilt_generation(&self, desired: &DesiredBuild) -> GenerationRef {
        let root = self.paths.generation(&desired.source_key);
        let package = root.join("package");
        for directory in [
            package.join("bin"),
            package.join("codex-resources"),
            package.join("codex-path"),
        ] {
            fs::create_dir_all(directory).expect("create prebuilt package directory");
        }

        let suffix = if desired.target.contains("windows") {
            ".exe"
        } else {
            ""
        };
        let helper = process_helper();
        for relative in [
            format!("bin/codex{suffix}"),
            format!("bin/codex-code-mode-host{suffix}"),
            format!("codex-path/rg{suffix}"),
        ] {
            copy_executable(&helper, &package.join(relative));
        }
        if desired.target.contains("windows") {
            for relative in [
                "codex-resources/codex-command-runner.exe",
                "codex-resources/codex-windows-sandbox-setup.exe",
            ] {
                copy_executable(&helper, &package.join(relative));
            }
        } else {
            fs::create_dir_all(package.join("codex-resources/zsh/bin"))
                .expect("create zsh package directory");
            copy_executable(&helper, &package.join("codex-resources/zsh/bin/zsh"));
            if desired.target.contains("linux") {
                copy_executable(&helper, &package.join("codex-resources/bwrap"));
            }
        }
        fs::write(
            package.join("codex-package.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "layoutVersion": 1,
                "version": desired.source.version,
                "target": desired.target,
                "variant": "codex",
                "entrypoint": format!("bin/codex{suffix}"),
                "resourcesDir": "codex-resources",
                "pathDir": "codex-path"
            }))
            .expect("serialize package metadata"),
        )
        .expect("write package metadata");

        let generation = GenerationRef {
            id: desired.source_key.clone(),
            package_dir: package.clone(),
            binary: package.join(format!("bin/codex{suffix}")),
            source_key: desired.source_key.clone(),
            source: desired.source.clone(),
            patch_fingerprint: desired.patch_fingerprint.clone(),
            target: desired.target.clone(),
            subcommands: vec![
                "app-server".to_owned(),
                "exec".to_owned(),
                "fork".to_owned(),
                "resume".to_owned(),
            ],
            built_at: Utc::now(),
        };
        let manifest = GenerationManifest {
            schema: 1,
            generation: generation.clone(),
            outputs: hash_package(&package),
            rustc: None,
            cargo: None,
            python: None,
            linker: None,
            sdk: None,
            environment: Default::default(),
        };
        fs::write(
            root.join("generation.json"),
            serde_json::to_vec_pretty(&manifest).expect("serialize prebuilt manifest"),
        )
        .expect("write prebuilt manifest");
        generation
    }

    pub fn publish_upstream_source(&self, source: ResolvedSource) {
        write_remote_cache(&self.paths, &[self.active.source.clone(), source]);
        let mut state = self.state();
        state.probe.next_check_at = None;
        self.save_state(&state);
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.wrapper);
        command.env(PATCHER_HOME_ENV, &self.paths.home);
        command
    }

    pub fn run<I, S>(&self, arguments: I, configure: impl FnOnce(&mut Command)) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = self.command();
        command.args(arguments);
        configure(&mut command);
        command.output().expect("run dispatcher")
    }
}

fn write_remote_cache(paths: &PatcherPaths, sources: &[ResolvedSource]) {
    let api = "https://api.github.com/repos/openai/codex";
    let entry = |body: serde_json::Value| {
        serde_json::json!({
            "status": 200,
            "etag": "test-etag",
            "checked_at": Utc::now(),
            "poll_floor_seconds": 300,
            "poll_floor_advertised": false,
            "body": serde_json::to_string(&body).expect("serialize cached body")
        })
    };
    let mut entries = serde_json::Map::new();
    entries.insert(
        format!("{api}/git/matching-refs/tags/rust-v"),
        entry(serde_json::Value::Array(
            sources
                .iter()
                .map(|source| {
                    serde_json::json!({
                        "ref": source.ref_name,
                        "object": { "sha": source.ref_object_oid, "type": "tag" }
                    })
                })
                .collect(),
        )),
    );
    for source in sources {
        let tag = source
            .ref_name
            .strip_prefix("refs/tags/")
            .expect("test source is a tag");
        entries.insert(
            format!("{api}/git/tags/{}", source.ref_object_oid),
            entry(serde_json::json!({
                "tag": tag,
                "object": { "sha": source.commit_oid, "type": "commit" }
            })),
        );
        entries.insert(
            format!("{api}/releases/tags/{tag}"),
            entry(serde_json::json!({
                "tag_name": tag,
                "draft": false,
                "prerelease": false,
                "published_at": Utc::now(),
                "html_url": source.release_url
            })),
        );
    }
    let cache = serde_json::json!({
        "schema": 1,
        "entries": entries
    });
    fs::write(
        paths.remote_cache_file(),
        serde_json::to_vec_pretty(&cache).expect("serialize remote cache"),
    )
    .expect("write remote cache");
}

fn hash_package(root: &Path) -> Vec<FileHash> {
    let mut output = Vec::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry.expect("walk prebuilt package");
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .expect("package-relative path")
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = fs::read(entry.path()).expect("read prebuilt package output");
        output.push(FileHash {
            path: relative,
            sha256: hex::encode(Sha256::digest(bytes)),
        });
    }
    output.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    output
}

pub fn desired(source_key: &str, patch_fingerprint: &str) -> DesiredBuild {
    DesiredBuild {
        source: source(),
        patch_fingerprint: patch_fingerprint.to_owned(),
        target: host_target_for_test(),
        source_key: source_key.to_owned(),
    }
}

pub fn generation(
    id: &str,
    package_dir: &Path,
    binary: &Path,
    source_key: &str,
    patch_fingerprint: &str,
) -> GenerationRef {
    GenerationRef {
        id: id.to_owned(),
        package_dir: package_dir.to_path_buf(),
        binary: binary.to_path_buf(),
        source_key: source_key.to_owned(),
        source: source(),
        patch_fingerprint: patch_fingerprint.to_owned(),
        target: host_target_for_test(),
        subcommands: vec![
            "app-server".to_owned(),
            "exec".to_owned(),
            "fork".to_owned(),
            "resume".to_owned(),
        ],
        built_at: Utc::now(),
    }
}

pub fn source() -> ResolvedSource {
    source_version("1.2.3", '1', '2')
}

pub fn source_version(version: &str, ref_byte: char, commit_byte: char) -> ResolvedSource {
    ResolvedSource {
        channel: "stable".to_owned(),
        ref_name: format!("refs/tags/rust-v{version}"),
        ref_object_oid: ref_byte.to_string().repeat(40),
        commit_oid: commit_byte.to_string().repeat(40),
        version: version.to_owned(),
        release_url: Some(format!(
            "https://github.com/openai/codex/releases/tag/rust-v{version}"
        )),
    }
}

pub fn host_target_for_test() -> String {
    codex_patcher::config::host_target()
        .expect("test host is supported")
        .to_owned()
}

pub fn line_count(path: &Path) -> usize {
    fs::read_to_string(path)
        .map(|contents| contents.lines().count())
        .unwrap_or(0)
}

fn process_helper() -> PathBuf {
    static HELPER: OnceLock<PathBuf> = OnceLock::new();
    HELPER
        .get_or_init(|| {
            let output_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("test-support");
            fs::create_dir_all(&output_dir).expect("create helper output directory");
            let output = output_dir.join(executable_name(&format!(
                "process-helper-{}",
                std::process::id()
            )));
            let status = Command::new("rustc")
                .arg("--edition=2024")
                .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/process_helper.rs"))
                .arg("-o")
                .arg(&output)
                .status()
                .expect("launch rustc for process helper");
            assert!(status.success(), "process helper compilation failed");
            output
        })
        .clone()
}

fn executable_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_owned()
    }
}

fn copy_executable(source: &Path, destination: &Path) {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).expect("create executable parent");
    }
    fs::copy(source, destination).unwrap_or_else(|error| {
        panic!(
            "copy executable {} to {}: {error}",
            source.display(),
            destination.display()
        )
    });
}
