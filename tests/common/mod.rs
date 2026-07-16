#![allow(dead_code)]

use chrono::Utc;
use codex_patcher::paths::{PATCHER_HOME_ENV, PatcherPaths};
use codex_patcher::state::{InstallState, StateStore};
use codex_patcher::types::{DesiredBuild, GenerationRef, ProbeKind, ProbeState, ResolvedSource};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};
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
                "schema = 1\nbranch = \"stable\"\ntarget = \"official-native\"\nfailure_mode = \"{failure_mode}\"\nnoninteractive_pending = \"{noninteractive_pending}\"\n"
            ),
        )
        .expect("write test config");

        let active = generation(
            "active",
            &package_dir,
            &active_binary,
            "active-key",
            "old-patch",
        );
        let mut state = InstallState::new(patch_dir.clone());
        state.activate(active.clone());
        state.probe = ProbeState {
            kind: ProbeKind::Current,
            checked_at: Some(Utc::now()),
            desired: Some(desired("active-key", "old-patch")),
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

    pub fn pending_state(&self, message: &str) -> InstallState {
        let mut state = self.state();
        state.probe = ProbeState {
            kind: ProbeKind::Pending,
            checked_at: Some(Utc::now()),
            desired: Some(desired("pending-key", "new-patch")),
            message: Some(message.to_owned()),
        };
        state
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
    ResolvedSource {
        channel: "stable".to_owned(),
        ref_name: "refs/tags/rust-v1.2.3".to_owned(),
        ref_object_oid: "1111111111111111111111111111111111111111".to_owned(),
        commit_oid: "2222222222222222222222222222222222222222".to_owned(),
        version: "1.2.3".to_owned(),
        release_url: Some("https://github.com/openai/codex/releases/tag/rust-v1.2.3".to_owned()),
    }
}

pub fn host_target_for_test() -> String {
    codex_patcher::config::host_target()
        .expect("test host is supported")
        .to_owned()
}

pub fn write_state_snapshot(path: &Path, state: &InstallState) {
    fs::write(
        path,
        serde_json::to_vec_pretty(state).expect("serialize state snapshot"),
    )
    .expect("write state snapshot");
}

pub fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
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
