use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

fn main() {
    let arguments: Vec<_> = env::args_os().skip(1).collect();
    if arguments
        .first()
        .is_some_and(|argument| argument == "__probe")
    {
        run_probe();
        return;
    }

    if let Some(path) = env::var_os("CODEX_PATCHER_TEST_ARGUMENTS") {
        let mut output = String::new();
        for argument in &arguments {
            output.push_str(&argument.to_string_lossy());
            output.push('\n');
        }
        fs::write(path, output).expect("write captured arguments");
    }

    if let Some(path) = env::var_os("CODEX_PATCHER_TEST_RUN_COUNT") {
        let mut count = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open run counter");
        writeln!(count, "run").expect("append run counter");
    }

    if let Ok(value) = env::var("CODEX_PATCHER_TEST_STDOUT") {
        print!("{value}");
        io::stdout().flush().expect("flush stdout");
    }
    if let Ok(value) = env::var("CODEX_PATCHER_TEST_STDERR") {
        eprint!("{value}");
        io::stderr().flush().expect("flush stderr");
    }

    let exit_code = env::var("CODEX_PATCHER_TEST_EXIT")
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(0);
    std::process::exit(exit_code);
}

fn run_probe() {
    if let Some(milliseconds) = env::var("CODEX_PATCHER_TEST_PROBE_SLEEP_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        thread::sleep(Duration::from_millis(milliseconds));
    }

    if let (Some(source), Some(destination)) = (
        env::var_os("CODEX_PATCHER_TEST_PROBE_STATE"),
        env::var_os("CODEX_PATCHER_TEST_STATE_PATH"),
    ) {
        replace_file(PathBuf::from(source), PathBuf::from(destination));
    }

    // The manager's output must be redirected to patcher logs, never inherited
    // by an app-server or MCP protocol stream.
    println!("detached probe stdout");
    eprintln!("detached probe stderr");
    io::stdout().flush().expect("flush probe stdout");
    io::stderr().flush().expect("flush probe stderr");

    if let Some(marker) = env::var_os("CODEX_PATCHER_TEST_PROBE_MARKER") {
        fs::write(marker, b"done").expect("write probe completion marker");
    }
}

fn replace_file(source: PathBuf, destination: PathBuf) {
    let bytes = fs::read(source).expect("read replacement state");
    let parent = destination.parent().expect("state file parent");
    let temporary = parent.join(format!(
        ".state.test-{}-{}.tmp",
        std::process::id(),
        thread_id()
    ));
    fs::write(&temporary, bytes).expect("write replacement state");

    #[cfg(windows)]
    if destination.exists() {
        fs::remove_file(&destination).expect("remove previous state on Windows");
    }

    fs::rename(&temporary, destination).expect("activate replacement state");
}

fn thread_id() -> String {
    format!("{:?}", thread::current().id())
        .replace('(', "")
        .replace(')', "")
}
