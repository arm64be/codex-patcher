use anyhow::{Context, Result, bail};
use directories::{BaseDirs, ProjectDirs};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub const PATCHER_HOME_ENV: &str = "CODEX_PATCHER_HOME";

pub fn display_user_path(path: &Path) -> String {
    BaseDirs::new()
        .map(|directories| display_user_path_with_home(path, directories.home_dir()))
        .unwrap_or_else(|| plain_path_display(path))
}

fn display_user_path_with_home(path: &Path, home: &Path) -> String {
    #[cfg(not(windows))]
    if let Ok(relative) = path.strip_prefix(home) {
        return if relative.as_os_str().is_empty() {
            "~".to_owned()
        } else {
            Path::new("~").join(relative).display().to_string()
        };
    }

    let rendered = plain_path_display(path);
    #[cfg(windows)]
    {
        let home = plain_path_display(home);
        let rendered_folded = rendered.to_ascii_lowercase();
        let home_folded = home.to_ascii_lowercase();
        if rendered_folded == home_folded {
            return "~".to_owned();
        }
        let suffix = &rendered[home.len().min(rendered.len())..];
        if rendered_folded.starts_with(&home_folded)
            && (suffix.starts_with('\\') || suffix.starts_with('/'))
        {
            return format!("~{}", &rendered[home.len()..]);
        }
    }
    rendered
}

fn plain_path_display(path: &Path) -> String {
    let rendered = path.display().to_string();
    #[cfg(windows)]
    {
        if let Some(rest) = rendered.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{rest}");
        }
        if let Some(rest) = rendered.strip_prefix(r"\\?\") {
            return rest.to_owned();
        }
    }
    rendered
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatcherPaths {
    pub home: PathBuf,
    pub manager_dir: PathBuf,
    pub manager: PathBuf,
    pub state_dir: PathBuf,
    pub state: PathBuf,
    pub locks_dir: PathBuf,
    pub manager_lock: PathBuf,
    pub state_lock: PathBuf,
    pub probe_lock: PathBuf,
    pub build_lock: PathBuf,
    pub logs_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub cargo_target_dir: PathBuf,
    pub remote_cache: PathBuf,
    pub mirror_dir: PathBuf,
    pub worktrees_dir: PathBuf,
    pub generations_dir: PathBuf,
    pub backups_dir: PathBuf,
}

impl PatcherPaths {
    pub fn discover() -> Result<Self> {
        match env::var_os(PATCHER_HOME_ENV) {
            Some(value) if !value.is_empty() => {
                let configured = PathBuf::from(value);
                let home = if configured.is_absolute() {
                    configured
                } else {
                    env::current_dir()
                        .context("resolve current directory for relative CODEX_PATCHER_HOME")?
                        .join(configured)
                };
                Ok(Self::from_home(home))
            }
            _ => {
                let project = ProjectDirs::from("com", "OpenAI", "codex-patcher")
                    .context("platform project directories are unavailable")?;
                let data = project.data_local_dir().to_path_buf();
                let cache = project.cache_dir().to_path_buf();
                let state = project
                    .state_dir()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| data.join("state"));
                Ok(Self::from_roots(data, cache, state))
            }
        }
    }

    pub fn from_home(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        Self::from_roots(home.clone(), home.join("cache"), home.join("state"))
    }

    pub fn from_roots(
        data_root: impl Into<PathBuf>,
        cache_root: impl Into<PathBuf>,
        state_root: impl Into<PathBuf>,
    ) -> Self {
        let home = data_root.into();
        let cache_dir = cache_root.into();
        let state_dir = state_root.into();
        let manager_dir = home.join("bin");
        let locks_dir = state_dir.join("locks");
        let logs_dir = state_dir.join("logs");

        Self {
            manager: manager_dir.join(manager_file_name()),
            state: state_dir.join("state.json"),
            manager_lock: locks_dir.join("manager.lock"),
            state_lock: locks_dir.join("state.lock"),
            probe_lock: locks_dir.join("probe.lock"),
            build_lock: locks_dir.join("build.lock"),
            mirror_dir: cache_dir.join("upstream.git"),
            worktrees_dir: cache_dir.join("worktrees"),
            cargo_target_dir: cache_dir.join("cargo-target"),
            remote_cache: state_dir.join("remote.json"),
            generations_dir: home.join("generations"),
            backups_dir: home.join("backups"),
            home,
            manager_dir,
            state_dir,
            locks_dir,
            logs_dir,
            cache_dir,
        }
    }

    pub fn ensure(&self) -> Result<()> {
        if self.home.as_os_str().is_empty() {
            bail!("patcher home cannot be empty");
        }

        for directory in self.directories() {
            fs::create_dir_all(directory)
                .with_context(|| format!("create patcher directory {}", directory.display()))?;
        }
        Ok(())
    }

    pub fn directories(&self) -> Vec<&Path> {
        vec![
            &self.home,
            &self.manager_dir,
            &self.state_dir,
            &self.locks_dir,
            &self.logs_dir,
            &self.cache_dir,
            &self.cargo_target_dir,
            &self.worktrees_dir,
            &self.generations_dir,
            &self.backups_dir,
        ]
    }

    pub fn generation(&self, id: &str) -> PathBuf {
        self.generations_dir.join(id)
    }

    pub fn worktree(&self, id: &str) -> PathBuf {
        self.worktrees_dir.join(id)
    }

    pub fn log(&self, id: &str) -> PathBuf {
        self.logs_dir.join(format!("{id}.log"))
    }

    pub fn for_test(root: impl Into<PathBuf>) -> Self {
        Self::from_home(root)
    }

    pub fn manager_binary(&self) -> PathBuf {
        self.manager.clone()
    }

    pub fn state_file(&self) -> PathBuf {
        self.state.clone()
    }

    pub fn state_lock(&self) -> PathBuf {
        self.state_lock.clone()
    }

    pub fn probe_lock(&self) -> PathBuf {
        self.probe_lock.clone()
    }

    pub fn build_lock(&self) -> PathBuf {
        self.build_lock.clone()
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.logs_dir.clone()
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.cache_dir.clone()
    }

    pub fn mirror_dir(&self) -> PathBuf {
        self.mirror_dir.clone()
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        self.worktrees_dir.clone()
    }

    pub fn generations_dir(&self) -> PathBuf {
        self.generations_dir.clone()
    }

    pub fn cargo_target_dir(&self) -> PathBuf {
        self.cargo_target_dir.clone()
    }

    pub fn cargo_target_dir_for(&self, target: &str, cargo_profile: &str) -> PathBuf {
        self.cargo_target_dir.join(target).join(cargo_profile)
    }

    pub fn remote_cache_file(&self) -> PathBuf {
        self.remote_cache.clone()
    }
}

pub const fn manager_file_name() -> &'static str {
    if cfg!(windows) {
        "codex-patcher.exe"
    } else {
        "codex-patcher"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_under_home_are_compact_for_display() {
        #[cfg(unix)]
        let (home, child) = (
            Path::new("/home/example"),
            Path::new("/home/example/.codex/codex-patcher"),
        );
        #[cfg(windows)]
        let (home, child) = (
            Path::new(r"C:\Users\example"),
            Path::new(r"\\?\C:\Users\example\.codex\codex-patcher"),
        );

        assert_eq!(display_user_path_with_home(home, home), "~");
        assert_eq!(
            display_user_path_with_home(child, home),
            Path::new("~")
                .join(".codex")
                .join("codex-patcher")
                .display()
                .to_string()
        );
    }

    #[test]
    fn ensure_creates_the_layout_but_not_state_files() {
        let temp = tempfile::tempdir().unwrap();
        let paths = PatcherPaths::from_home(temp.path().join("patcher"));
        paths.ensure().unwrap();

        for directory in paths.directories() {
            assert!(directory.is_dir(), "{}", directory.display());
        }
        assert!(!paths.state.exists());
        assert!(!paths.manager_lock.exists());
        assert!(!paths.build_lock.exists());
    }

    #[test]
    fn platform_roots_keep_cache_and_state_separate() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let cache = temp.path().join("cache");
        let state = temp.path().join("state");
        let paths = PatcherPaths::from_roots(&data, &cache, &state);

        assert!(paths.manager_binary().starts_with(&data));
        assert!(paths.generations_dir().starts_with(&data));
        assert!(paths.backups_dir.starts_with(&data));
        assert!(paths.mirror_dir().starts_with(&cache));
        assert!(paths.worktrees_dir().starts_with(&cache));
        assert!(paths.cargo_target_dir().starts_with(&cache));
        assert!(paths.state_file().starts_with(&state));
        assert!(paths.logs_dir().starts_with(&state));
        assert!(paths.state_lock().starts_with(&state));
        assert_eq!(paths.generation("abc"), data.join("generations/abc"));
        assert_eq!(paths.worktree("abc"), cache.join("worktrees/abc"));
        assert_eq!(paths.log("abc"), state.join("logs/abc.log"));
    }
}
