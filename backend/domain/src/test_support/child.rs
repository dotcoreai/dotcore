//! Helpers for tests that re-run the current test binary as a child process.
//!
//! Children run with their output piped, so a passing run stays silent while a failing child
//! reports its exit status and complete stdout and stderr in the assertion message. Use
//! `current_test_command` to start a child that runs one test of the current binary, and
//! `assert_success` once it has finished.

use std::process::{Command, Output, Stdio};

/// Builds a command that runs exactly one test of the current binary with piped output.
pub fn current_test_command(test_name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("current test executable"));
    command
        .args(["--exact", test_name, "--nocapture"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Asserts that a finished child exited successfully, reporting its captured output otherwise.
pub fn assert_success(output: &Output, description: &str) {
    assert!(
        output.status.success(),
        "{description}: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
