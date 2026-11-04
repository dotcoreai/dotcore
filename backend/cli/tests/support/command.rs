//! Builds the child processes that run the command-line binary under test, for example a model
//! listing against a fake manager with an empty environment and no terminal.
//!
//! A command detached from the test runner's terminal takes the non-interactive output path even
//! when the tests themselves run in a terminal.

use std::{os::unix::process::CommandExt, process::Command};

/// Builds a command running the binary under test with the test runner's environment.
pub fn dotc() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dotc"))
}

/// Builds a command running the binary under test with an empty environment, so no workspace
/// variable reaches it.
pub fn isolated_dotc() -> Command {
    let mut command = dotc();
    command.env_clear();
    command
}

/// Builds `dotc <group> <arguments>` with an empty environment pointing at the management
/// `address`, detached from any terminal of the test runner.
pub fn dotc_at(group: &str, address: &str, arguments: &[&str]) -> Command {
    let mut command = isolated_dotc();
    command
        .arg(group)
        .args(arguments)
        .env("DOTCORE_MANAGEMENT_ADDR", address);
    detach_from_controlling_terminal(&mut command);
    command
}

/// Starts the command in a new session, so it has no controlling terminal.
pub fn detach_from_controlling_terminal(command: &mut Command) {
    // SAFETY: the closure calls only the async-signal-safe setsid operation before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}
