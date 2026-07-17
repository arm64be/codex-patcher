use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};

fn main() {
    let arguments: Vec<_> = env::args_os().skip(1).collect();
    if arguments.as_slice() == ["--version"] {
        println!("codex-cli 1.2.3");
        return;
    }
    if arguments.as_slice() == ["--help"] {
        println!(
            "Codex test helper\n\nCommands:\n  app-server\n  exec\n  fork\n  resume\n\nOptions:\n  --help"
        );
        return;
    }
    if arguments.as_slice() == ["app-server", "--help"] {
        println!("Usage: codex app-server");
        return;
    }
    if env::var_os("CODEX_PATCHER_VALIDATION").is_some() {
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
