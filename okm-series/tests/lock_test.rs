#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! One writer per root, and what happens when the writer is not polite about leaving.
//!
//! The rule lives in two places and they answer different questions: the `LOCK` file names a
//! pid (so another PROCESS is refused, and a pid that is gone is taken over), and an
//! in-process registry keyed by canonical path (so a second `Log` in this process is refused,
//! which the pid cannot see). Both are tested here, along with the case the design exists
//! for: a writer that was killed mid-batch leaves a directory the next process opens.

use std::path::Path;

use okm_series::{Config, Error, Log, Precision};

fn config() -> Config {
    Config {
        precision: Precision::Ms,
        ..Config::default()
    }
}

fn write_lock(root: &Path, contents: &str) {
    std::fs::create_dir_all(root).expect("create root");
    std::fs::write(root.join("LOCK"), contents).expect("write LOCK");
}

/// A pid that no process holds: /proc is the authority here, so the test asks it rather
/// than assuming a number is free.
fn dead_pid() -> u32 {
    (1_000_000..2_000_000)
        .find(|pid| !Path::new(&format!("/proc/{pid}")).exists())
        .expect("no free-looking pid")
}

#[test]
fn a_second_log_for_one_root_is_refused_in_this_process() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first = Log::open(dir.path(), config()).expect("first open");

    let refused = Log::open(dir.path(), config());
    match refused {
        Err(Error::AlreadyOpen(root)) => {
            assert_eq!(
                std::fs::canonicalize(dir.path()).expect("canonical"),
                root,
                "the error names the root it refused"
            );
        }
        other => panic!("a second writer in one process must be refused, got {other:?}"),
    }

    // The claim leaves with the log: closing and reopening is the ordinary path, and a
    // refused open must not have wedged anything.
    drop(first);
    Log::open(dir.path(), config()).expect("reopen after close");
}

#[test]
fn the_same_root_under_another_name_is_the_same_root() {
    let dir = tempfile::tempdir().expect("temp dir");
    let _held = Log::open(dir.path(), config()).expect("first open");

    // `root/.` and `root/../root` are the same directory; a registry keyed by the string a
    // caller passed would hand both of them a writer.
    for alias in [
        dir.path().join("."),
        dir.path().join("..").join(dir.path().file_name().unwrap()),
    ] {
        match Log::open(&alias, config()) {
            Err(Error::AlreadyOpen(_)) => {}
            other => panic!(
                "{} must resolve to the held root, got {other:?}",
                alias.display()
            ),
        }
    }
}

#[test]
fn a_symlink_to_a_held_root_is_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    let _held = Log::open(dir.path(), config()).expect("first open");

    let link = dir.path().join("alias");
    std::os::unix::fs::symlink(dir.path(), &link).expect("symlink");
    match Log::open(&link, config()) {
        Err(Error::AlreadyOpen(_)) => {}
        other => panic!("a symlink is the same root, got {other:?}"),
    }
}

#[test]
fn a_live_writers_pid_is_refused_and_named() {
    let dir = tempfile::tempdir().expect("temp dir");
    // A real, live pid that is not this process: the test must not fake liveness, because
    // liveness is exactly what the check reads (/proc).
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn a holder");
    let pid = child.id();
    write_lock(dir.path(), &format!("{pid}\n"));

    match Log::open(dir.path(), config()) {
        Err(Error::Locked(named)) => assert_eq!(named, pid, "the error names the holder"),
        other => panic!("a live holder must be refused, got {other:?}"),
    }

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_dead_pid_is_taken_over_and_the_lock_rewritten() {
    let dir = tempfile::tempdir().expect("temp dir");
    write_lock(dir.path(), &format!("{}\n", dead_pid()));

    let log = Log::open(dir.path(), config()).expect("take over a stale lock");
    let lock = std::fs::read_to_string(dir.path().join("LOCK")).expect("read LOCK");
    assert_eq!(
        lock.trim().parse::<u32>().expect("a pid"),
        std::process::id(),
        "the LOCK names the new writer"
    );
    drop(log);
}

#[test]
fn a_writer_killed_mid_batch_leaves_a_log_the_next_open_can_read() {
    // The whole point of the file-per-bucket design is that a crash costs the tail of a
    // batch, not the directory. The child writes in plain `append` calls (buffered, no
    // fsync) and is killed without warning; the parent then takes the stale lock and reads
    // whatever is there. Buffered records are expected to be lost — what must NOT happen is
    // an open that fails, or a read that returns torn records.
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("log");

    let child = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args(["--exact", "child_writer", "--nocapture", "--ignored"])
        .env("OKM_SERIES_CRASH_CHILD", &root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the child writer");

    // Wait for the child to actually be writing: LOCK plus a part file, then a moment.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "the child never opened the log"
        );
        if root.join("LOCK").exists() && part_exists(&root) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    std::thread::sleep(std::time::Duration::from_millis(60));

    let mut child = child;
    child.kill().expect("kill -9 the writer");
    let _ = child.wait();

    let log = Log::open(&root, config()).expect("the next open takes the stale lock");
    let rows = log.read(0, 1_000_000).expect("read what survived");
    for (ts, offset, body) in &rows {
        assert!(*ts > 0, "a surviving record keeps its stamped time");
        assert!(!body.is_empty(), "offset {offset} read back empty");
    }
    // Not "some records": whatever count, the offsets are a dense run from 0, which is what
    // tells a reader that nothing in the middle was torn.
    for (index, (_, offset, _)) in rows.iter().enumerate() {
        assert_eq!(*offset, index as u64, "offsets are dense and in order");
    }

    // And the log is usable, not merely readable.
    let mut log = log;
    log.append(&body(1)).expect("append after recovery");
    log.flush().expect("flush after recovery");
    assert_eq!(
        log.read(0, 1_000_000).expect("read again").len(),
        rows.len() + 1
    );
}

fn part_exists(root: &Path) -> bool {
    let Ok(buckets) = std::fs::read_dir(root) else {
        return false;
    };
    for bucket in buckets.flatten() {
        let Ok(entries) = std::fs::read_dir(bucket.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("part-") {
                return true;
            }
        }
    }
    false
}

fn body(index: u64) -> okm_series::Body {
    let mut body = okm_series::Body::new();
    body.push(1, okm_series::Value::U64(index));
    body
}

/// The child half of `a_writer_killed_mid_batch_...`: ignored by default, run only by the
/// test above through its own binary, and it never returns — it is killed.
#[test]
#[ignore]
fn child_writer() {
    let Ok(root) = std::env::var("OKM_SERIES_CRASH_CHILD") else {
        return;
    };
    let mut log = Log::open(Path::new(&root), config()).expect("child open");
    let mut index = 0u64;
    loop {
        log.append(&body(index)).expect("child append");
        if index.is_multiple_of(64) {
            // Buffered, not fsynced: the records between two flushes are the ones a crash
            // is allowed to take.
            log.flush().expect("child flush");
        }
        index += 1;
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
