pub mod build;
pub mod config;
pub mod discovery;
pub mod dispatch;
pub mod elevation;
pub mod patchset;
pub mod paths;
pub mod probe;
pub mod repair;
pub mod shim;
pub mod state;
pub mod tui;
pub mod types;
pub mod upstream;

pub const STATE_SCHEMA: u32 = 1;
pub const CONFIG_SCHEMA: u32 = 1;
pub const BUILD_RECIPE_VERSION: u32 = 2;
pub const UPSTREAM_REPOSITORY: &str = "https://github.com/openai/codex.git";
