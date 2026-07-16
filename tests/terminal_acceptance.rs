mod common;

use codex_patcher::paths::PATCHER_HOME_ENV;
use common::DispatcherFixture;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

#[test]
fn interactive_update_restores_a_native_pty_or_conpty() {
    let fixture = DispatcherFixture::new("error", "error");
    fixture.save_state(&fixture.pending_state("patched update pending"));
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 8,
            cols: 40,
            ..PtySize::default()
        })
        .unwrap();
    let mut command = CommandBuilder::new(&fixture.wrapper);
    command.env(PATCHER_HOME_ENV, &fixture.paths.home);
    command.env("TERM", "xterm-256color");
    command.env("CODEX_PATCHER_ASCII", "1");
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let output = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = reader.read_to_end(&mut bytes);
        bytes
    });
    writer.write_all(b"2").unwrap();
    writer.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("interactive update prompt did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(writer);
    drop(pair.master);
    let output = String::from_utf8_lossy(&output.join().unwrap()).into_owned();
    assert!(status.success(), "{status}: {output:?}");
    assert!(
        output.contains("Update available!") && output.contains("patched"),
        "{output:?}"
    );
    assert!(output.contains("\x1b[?1049h") && output.contains("\x1b[?1049l"));
    assert!(output.contains("\x1b[?25l") && output.contains("\x1b[?25h"));
}
