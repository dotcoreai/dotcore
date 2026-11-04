//! Entry point for the `dotc` product CLI process.
//!
//! It prepares the process, parses the command line, and hands the parsed
//! command to the library for dispatch, converting any failure into the
//! process exit code.

use std::{process::ExitCode, time::Duration};

fn main() -> ExitCode {
    dotcore_cli::initialize_tracing();

    let command = match dotcore_cli::Cli::parse_process() {
        Ok(command) => command,
        Err(exit_code) => return exit_code,
    };
    let verbose = command.verbose;
    let runtime = match dotcore_cli::build_runtime() {
        Ok(runtime) => runtime,
        Err(error) => return dotcore_cli::report_failure(error, verbose),
    };

    let outcome = runtime.block_on(dotcore_cli::dispatch(command));
    // A terminal prompt left blocked on its read cannot be cancelled; do not wait for it.
    runtime.shutdown_timeout(Duration::from_millis(250));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => dotcore_cli::report_failure(error, verbose),
    }
}
