//! Process entry point of `dotcore-launcher`, started once per workload as
//! `dotcore-launcher <workload directory>` by the supervisor.
//!
//! It exits successfully once the detached launcher has recorded its identity, and with a
//! failure status and a message on stderr when the launch could not get that far.

fn main() -> std::process::ExitCode {
    dotcore_launcher::launcher::main()
}
