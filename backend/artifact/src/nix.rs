//! Evaluates the store path of a Nix engine artifact on the SiteNode that hosts its model
//! deployment, without building it, for example package `engines.llama-cpp.native` of `self` for
//! `aarch64-darwin` evaluates to `/nix/store/<hash>-llama-cpp-b11417`.
//!
//! Evaluation runs the host `nix` command, found first in the system profile of a NixOS or
//! nix-darwin host, then in the installer's default profile, then on `PATH`, with flakes enabled
//! and a bounded duration. Cancelling an evaluation kills the `nix` process.

use std::{future::Future, path::PathBuf, pin::Pin, process::Stdio, time::Duration};

use dotcore_domain::nix::{discover_nix, search_locations};
use dotcore_domain::real_time;
use tokio_util::sync::CancellationToken;

/// Flags enabling the `nix` subcommands and flake references evaluation uses.
pub(super) const EXPERIMENTAL_FEATURES: [&str; 2] =
    ["--extra-experimental-features", "nix-command flakes"];

/// Nix expression applied to the evaluated package. The store path key avoids `outPath`, which
/// would make Nix serialize the whole attribute set as the path alone.
const EVALUATION_EXPRESSION: &str = "package: { storePath = package.outPath; }";

/// Longest an evaluation may run. A first evaluation of a source may fetch it and its inputs.
const EVALUATION_TIMEOUT: Duration = Duration::from_secs(300);

/// Bounded number of trailing stderr bytes kept with an evaluation failure.
const STDERR_TAIL_BYTES: usize = 2048;

/// Evaluates one installable to its store path.
pub trait NixEvaluator: Send + Sync {
    /// Evaluates `installable` until it finishes or `cancellation` fires.
    fn evaluate<'a>(
        &'a self,
        installable: &'a str,
        cancellation: CancellationToken,
    ) -> NixEvaluationFuture<'a>;
}

/// Boxed result of one Nix evaluation.
pub type NixEvaluationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<NixEvaluation, NixEvaluationError>> + Send + 'a>>;

/// Evaluated facts of one Nix package.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NixEvaluation {
    pub store_path: String,
}

/// Evaluates installables with the host `nix` command, killing an evaluation that runs too long.
#[derive(Clone, Debug)]
pub struct CommandNixEvaluator {
    command: NixCommand,
    timeout: Duration,
}

impl Default for CommandNixEvaluator {
    fn default() -> Self {
        Self {
            command: NixCommand::Discover,
            timeout: EVALUATION_TIMEOUT,
        }
    }
}

impl CommandNixEvaluator {
    /// Runs `nix` at `executable` and stops evaluations after `timeout`, for fake `nix` commands.
    #[cfg(test)]
    fn with_executable(executable: PathBuf, timeout: Duration) -> Self {
        Self {
            command: NixCommand::Executable(executable),
            timeout,
        }
    }
}

impl NixEvaluator for CommandNixEvaluator {
    fn evaluate<'a>(
        &'a self,
        installable: &'a str,
        cancellation: CancellationToken,
    ) -> NixEvaluationFuture<'a> {
        Box::pin(async move {
            let nix = self
                .command
                .executable()
                .ok_or(NixEvaluationError::NixMissing)?;
            let output = tokio::process::Command::new(&nix)
                .args(EXPERIMENTAL_FEATURES)
                .args([
                    "eval",
                    "--json",
                    "--no-write-lock-file",
                    installable,
                    "--apply",
                    EVALUATION_EXPRESSION,
                ])
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output();
            // Dropping the output future kills the child, on timeout and on cancellation alike.
            let output = tokio::select! {
                output = real_time::bound(self.timeout, output) => output,
                () = cancellation.cancelled() => return Err(NixEvaluationError::Cancelled),
            };
            let output = output
                .map_err(|_| NixEvaluationError::TimedOut {
                    installable: installable.to_owned(),
                    timeout: self.timeout,
                })?
                .map_err(|source| match source.kind() {
                    std::io::ErrorKind::NotFound => NixEvaluationError::NixMissing,
                    _ => NixEvaluationError::Spawn(source),
                })?;
            if !output.status.success() {
                return Err(NixEvaluationError::Failed {
                    installable: installable.to_owned(),
                    stderr: stderr_tail(&output.stderr),
                });
            }
            serde_json::from_slice(&output.stdout).map_err(|source| {
                NixEvaluationError::InvalidOutput {
                    installable: installable.to_owned(),
                    source,
                }
            })
        })
    }
}

/// How a Nix command finds the `nix` executable.
#[derive(Clone, Debug)]
pub(super) enum NixCommand {
    /// Searches the system and default profiles, then `PATH`.
    Discover,

    /// Runs this executable.
    #[cfg_attr(
        not(any(test, feature = "test-support")),
        expect(dead_code, reason = "tests run a fake nix executable")
    )]
    Executable(PathBuf),
}

impl NixCommand {
    /// Returns the `nix` executable to run, or `None` when discovery finds none.
    pub(super) fn executable(&self) -> Option<PathBuf> {
        match self {
            Self::Discover => discover_nix(),
            Self::Executable(executable) => Some(executable.clone()),
        }
    }
}

/// Failures while evaluating one Nix installable.
#[derive(Debug, thiserror::Error)]
pub enum NixEvaluationError {
    #[error("Nix was not found in {}", search_locations())]
    NixMissing,

    #[error("could not run nix eval")]
    Spawn(#[source] std::io::Error),

    #[error("nix eval of {installable:?} failed: {stderr}")]
    Failed { installable: String, stderr: String },

    #[error(
        "nix eval of {installable:?} did not finish within {timeout:?}; check that the flake source and its inputs are reachable from this backend, then retry"
    )]
    TimedOut {
        installable: String,
        timeout: Duration,
    },

    #[error("nix eval of {installable:?} returned unexpected output")]
    InvalidOutput {
        installable: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("nix eval was cancelled")]
    Cancelled,
}

fn stderr_tail(stderr: &[u8]) -> String {
    let start = stderr.len().saturating_sub(STDERR_TAIL_BYTES);
    String::from_utf8_lossy(&stderr[start..]).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use std::{os::unix::fs::PermissionsExt as _, path::Path};

    use super::*;

    #[tokio::test]
    async fn command_evaluation_reads_the_output_without_writing_a_lock_file() {
        let root = tempfile::tempdir().expect("temporary directory");
        let calls = root.path().join("calls");
        let nix = fake_nix(
            root.path(),
            &format!(
                "printf '%s\\n' \"$*\" > '{}'\necho '{{\"storePath\":\"/nix/store/a-b\"}}'",
                calls.display()
            ),
        );

        let evaluation = CommandNixEvaluator::with_executable(nix, Duration::from_secs(30))
            .evaluate(
                "git+file:///src#legacyPackages.aarch64-darwin.engines.llama-cpp.native",
                CancellationToken::new(),
            )
            .await
            .expect("evaluation");

        assert_eq!(
            evaluation,
            NixEvaluation {
                store_path: "/nix/store/a-b".to_owned(),
            }
        );
        assert_eq!(
            std::fs::read_to_string(calls).expect("calls"),
            format!(
                "--extra-experimental-features nix-command flakes eval --json --no-write-lock-file git+file:///src#legacyPackages.aarch64-darwin.engines.llama-cpp.native --apply {EVALUATION_EXPRESSION}\n"
            )
        );
    }

    #[tokio::test]
    async fn command_evaluation_failures_keep_the_stderr_tail_and_time_out() {
        let root = tempfile::tempdir().expect("temporary directory");
        let installable = "git+file:///src#legacyPackages.aarch64-darwin.engines.llama-cpp.native";
        let failing = fake_nix(
            root.path(),
            "echo 'evaluating flake' >&2\necho \"error: attribute 'engines.llama-cpp.native' missing\" >&2\nexit 1",
        );
        let hanging = fake_nix(root.path(), "exec sleep 30");

        let failed = CommandNixEvaluator::with_executable(failing, Duration::from_secs(30))
            .evaluate(installable, CancellationToken::new())
            .await
            .expect_err("failed evaluation");
        let started = std::time::Instant::now();
        let timed_out = CommandNixEvaluator::with_executable(hanging, Duration::from_millis(200))
            .evaluate(installable, CancellationToken::new())
            .await
            .expect_err("timed out evaluation");

        assert!(
            matches!(&failed, NixEvaluationError::Failed { stderr, .. }
                if stderr == "evaluating flake\nerror: attribute 'engines.llama-cpp.native' missing"),
            "{failed:?}"
        );
        assert!(
            matches!(&timed_out, NixEvaluationError::TimedOut { installable: timed, timeout }
                if timed == installable && *timeout == Duration::from_millis(200)),
            "{timed_out:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(
            timed_out.to_string(),
            format!(
                "nix eval of {installable:?} did not finish within 200ms; check that the flake source and its inputs are reachable from this backend, then retry"
            )
        );
        assert_eq!(
            NixEvaluationError::NixMissing.to_string(),
            "Nix was not found in /run/current-system/sw/bin, /nix/var/nix/profiles/default/bin, or PATH"
        );
    }

    #[tokio::test]
    async fn cancelled_command_evaluation_returns_promptly_and_kills_nix() {
        let root = tempfile::tempdir().expect("temporary directory");
        let pid_file = root.path().join("pid");
        let hanging = fake_nix(
            root.path(),
            &format!("echo $$ > '{}'\nexec sleep 30", pid_file.display()),
        );
        let cancellation = CancellationToken::new();
        let evaluator = CommandNixEvaluator::with_executable(hanging, Duration::from_secs(30));
        let evaluation =
            evaluator.evaluate("git+file:///src#legacyPackages.x.y", cancellation.clone());
        tokio::pin!(evaluation);
        let pid = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                tokio::select! {
                    _ = &mut evaluation => panic!("evaluation finished before cancellation"),
                    () = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
                if let Ok(pid) = std::fs::read_to_string(&pid_file)
                    && let Ok(pid) = pid.trim().parse::<i32>()
                {
                    return pid;
                }
            }
        })
        .await
        .expect("fake nix started");

        let started = std::time::Instant::now();
        cancellation.cancel();
        let error = evaluation.await.expect_err("cancelled evaluation");

        assert!(matches!(error, NixEvaluationError::Cancelled), "{error:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
        tokio::time::timeout(Duration::from_secs(10), async {
            while process_running(pid) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the nix process is killed");
    }

    /// Evaluates the repository flake's llama.cpp engine package from this checkout as a
    /// development source, the way a hosting node does, without building it. Skipped without
    /// Nix or a Git checkout (for example inside a Nix build sandbox).
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn host_nix_evaluates_the_llama_cpp_engine_package_from_the_development_source() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("repository root");
        if discover_nix().is_none() || !root.join(".git").exists() {
            eprintln!("skipping: requires Nix and a Git checkout");
            return;
        }
        let development_source = format!("git+file://{}", root.display());
        let installable = dotcore_domain::nix::installable(
            &dotcore_domain::nix::NixFlakeReference::parse("self")
                .expect("reference")
                .source_url(Some(&development_source))
                .expect("source"),
            "aarch64-darwin",
            "engines.llama-cpp.native",
        );

        let evaluation = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(
                CommandNixEvaluator::default().evaluate(&installable, CancellationToken::new()),
            )
            .expect("host Nix evaluation");

        assert!(
            dotcore_domain::nix::valid_store_path(&evaluation.store_path),
            "{evaluation:?}"
        );
    }

    /// Whether `pid` names a live process; a killed child awaiting reaping counts as gone.
    fn process_running(pid: i32) -> bool {
        std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .is_ok_and(|output| {
                let state = String::from_utf8_lossy(&output.stdout);
                !state.trim().is_empty() && !state.trim().starts_with('Z')
            })
    }

    /// Writes an executable fake `nix` running `body` into `directory`.
    fn fake_nix(directory: &Path, body: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = directory.join(format!(
            "nix-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("fake nix");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("fake nix permissions");
        path
    }
}
