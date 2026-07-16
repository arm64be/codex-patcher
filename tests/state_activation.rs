mod common;

use codex_patcher::paths::PatcherPaths;
use codex_patcher::state::{InstallState, StateStore};
use common::generation;
use std::fs;

#[test]
fn activation_keeps_one_previous_generation_and_is_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let paths = PatcherPaths::from_home(temp.path().join("home"));
    paths.ensure().unwrap();
    let package_a = paths.generation("a");
    let package_b = paths.generation("b");
    fs::create_dir_all(&package_a).unwrap();
    fs::create_dir_all(&package_b).unwrap();
    let binary_a = package_a.join(if cfg!(windows) { "codex.exe" } else { "codex" });
    let binary_b = package_b.join(if cfg!(windows) { "codex.exe" } else { "codex" });
    fs::write(&binary_a, b"a").unwrap();
    fs::write(&binary_b, b"b").unwrap();
    let a = generation("a", &package_a, &binary_a, "key-a", "patch-a");
    let b = generation("b", &package_b, &binary_b, "key-b", "patch-b");

    let mut state = InstallState::new(temp.path().join("patches"));
    state.activate(a.clone());
    state.activate(b.clone());
    assert_eq!(state.active.as_ref().map(|item| &item.id), Some(&b.id));
    assert_eq!(state.previous.as_ref().map(|item| &item.id), Some(&a.id));

    state.activate(b.clone());
    assert_eq!(state.active.as_ref().map(|item| &item.id), Some(&b.id));
    assert_eq!(state.previous.as_ref().map(|item| &item.id), Some(&a.id));
}

#[test]
fn atomic_state_save_ignores_orphaned_staging_file() {
    let temp = tempfile::tempdir().unwrap();
    let paths = PatcherPaths::from_home(temp.path().join("home"));
    let store = StateStore::new(paths.clone());
    let expected = InstallState::new(temp.path().join("patches"));
    store.save(&expected).unwrap();

    let state_file = paths.state_file();
    let state_parent = state_file.parent().unwrap();
    fs::write(state_parent.join(".state.json.crash.tmp"), b"{truncated").unwrap();

    let loaded = store.require().unwrap();
    assert_eq!(loaded.patch_dir, expected.patch_dir);
    assert_eq!(loaded.schema, expected.schema);
}

#[test]
fn state_lock_serializes_atomic_read_modify_write() {
    let temp = tempfile::tempdir().unwrap();
    let paths = PatcherPaths::from_home(temp.path().join("home"));
    let store = StateStore::new(paths.clone());
    store
        .save(&InstallState::new(temp.path().join("patches")))
        .unwrap();

    let workers: Vec<_> = (0..8)
        .map(|index| {
            let store = store.clone();
            std::thread::spawn(move || {
                store
                    .with_state_lock(|| {
                        let mut state = store.require()?;
                        state.probe.message = Some(format!("writer-{index}"));
                        store.save(&state)
                    })
                    .unwrap();
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }

    let state = store.require().unwrap();
    assert!(state.probe.message.unwrap().starts_with("writer-"));
    let temporary_files = fs::read_dir(paths.state_file().parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .count();
    assert_eq!(temporary_files, 0);
}
