#![cfg(unix)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::executor::block_on;
use futures::io::{AsyncBufReadExt as _, BufReader};
use instant::Instant;
use warpui::r#async::executor::Background;

use super::{HarnessProcess, HarnessProcessLaunch, HarnessStdio, KILLED_EXIT_CODE};

fn background() -> Arc<Background> {
    Arc::new(Background::new(1, |_| "child-process-test".to_string()))
}

fn spawn_sh(script: &str) -> (HarnessProcess, HarnessStdio) {
    let args = vec!["-c".to_string(), script.to_string()];
    HarnessProcess::spawn(
        HarnessProcessLaunch {
            program: "sh",
            args: &args,
            cwd: Path::new("/"),
            env: &HashMap::new(),
        },
        &background(),
    )
    .expect("sh should spawn")
}

fn process_is_alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

#[test]
fn reports_the_process_exit_code() {
    let (process, _stdio) = spawn_sh("exit 3");
    let exit_code = block_on(process.wait_for_exit(Duration::from_secs(10))).unwrap();
    assert_eq!(exit_code.value(), 3);
}

#[test]
fn closing_stdin_lets_a_well_behaved_process_finish() {
    let (process, stdio) = spawn_sh("cat >/dev/null");
    drop(stdio);
    let exit_code = block_on(process.wait_for_exit(Duration::from_secs(10))).unwrap();
    assert!(exit_code.was_successful(), "{exit_code}");
}

#[test]
fn kills_the_whole_process_group_after_the_grace_period() {
    let (process, stdio) = spawn_sh("sleep 30 & echo $!; wait");
    let mut stdout = BufReader::new(stdio.stdout);
    let mut line = String::new();
    block_on(stdout.read_line(&mut line)).unwrap();
    let sleep_pid: i32 = line.trim().parse().expect("sh should print the sleep pid");
    assert!(process_is_alive(sleep_pid));

    let started = Instant::now();
    let exit_code = block_on(process.wait_for_exit(Duration::from_millis(200))).unwrap();
    assert_eq!(exit_code.value(), KILLED_EXIT_CODE);
    assert!(started.elapsed() < Duration::from_secs(10));

    // SIGKILL delivery to the group is asynchronous; give the kernel a moment to reap.
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_is_alive(sleep_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !process_is_alive(sleep_pid),
        "sleep {sleep_pid} outlived its process group"
    );
}
