//! Shared support for the `dotc` integration tests.

#[allow(
    dead_code,
    reason = "not every test crate runs every kind of child command"
)]
pub mod command;
#[allow(
    dead_code,
    reason = "only the coding agent tests run the fake inference API"
)]
pub mod fake_inference;
#[allow(
    dead_code,
    reason = "only the management command tests run the fake manager"
)]
pub mod fake_manager;
#[allow(dead_code, reason = "only the tests driving a real manager start one")]
pub mod manager_server;
#[allow(dead_code, reason = "only the terminal tests drive a pseudo-terminal")]
pub mod pty;
#[allow(
    dead_code,
    reason = "only the real-manager round trip starts a manager"
)]
pub mod simulated_manager;

/// Returns the system temporary directory with symlinks resolved, because the product rejects
/// symlinked ancestors and macOS `TMPDIR` lives under the `/var` symlink. Every fixture directory
/// starts here, so it also relaxes the file sync of in-process writes; `dotc` children keep the
/// full sync.
#[allow(dead_code, reason = "not every test crate creates fixture directories")]
pub fn canonical_temp_dir() -> std::path::PathBuf {
    dotcore::testing::relax_sync_for_tests();
    std::fs::canonicalize(std::env::temp_dir()).expect("canonical temporary directory")
}

/// Creates a temporary directory under the canonical system temporary directory, removed on drop.
#[allow(dead_code, reason = "not every test crate creates fixture directories")]
pub fn canonical_tempdir() -> std::io::Result<tempfile::TempDir> {
    tempfile::tempdir_in(canonical_temp_dir())
}

/// The rows under the `Parameters` heading of a shown deployment, one per parameter with its
/// whitespace collapsed, such as `Memory limit 2048 MB`.
#[allow(
    dead_code,
    reason = "only the deployment contracts read parameter rows"
)]
pub fn parameter_rows(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .skip_while(|line| *line != "Parameters")
        .skip(1)
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect()
}

/// Parses stdout as a stream of YAML documents, each as the JSON value it maps to, so a test
/// compares whole documents field by field.
#[allow(
    dead_code,
    reason = "only the deployment contracts read YAML documents"
)]
pub fn yaml_documents(stdout: &str) -> Vec<serde_json::Value> {
    use serde::Deserialize;

    serde_yaml_ng::Deserializer::from_str(stdout)
        .map(|document| serde_json::Value::deserialize(document).expect("YAML document"))
        .collect()
}
