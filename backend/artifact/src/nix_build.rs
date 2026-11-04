//! Builds the Nix engine artifact a deployment retains on the node that hosts it, for example the
//! store path `/nix/store/<hash>-llama-cpp-b11417` from package `engines.llama-cpp.native` of `self`,
//! and keeps it alive with one garbage-collector root per artifact in the node's root folder.
//!
//! The build must produce exactly the retained store path, so a node whose sources differ fails
//! instead of running another binary. A root that already names the path is reused without running
//! Nix or reaching the network, and a lock shared across processes lets one process publish each
//! root. Without a root folder, as for transient development workloads, a path already in the
//! store is reused and a build roots nothing. The build's log reaches the caller as readable lines
//! while it runs.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    process::Stdio,
};

use dotcore_domain::nix::{DotcoreSources, NixFlakeReference};
use dotcore_proto::dotcore::deployment::v1::ArtifactPreparationPolicy;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::{
    ArtifactCacheError,
    model::{acquire_lock, acquire_step_lock},
    nix::{EXPERIMENTAL_FEATURES, NixCommand},
    output::{OutputStream, StepOutput},
};

/// Directory under the root folder holding one cross-process lock per root.
pub(crate) const LOCK_DIRECTORY: &str = ".locks";

/// Internal-JSON result type of one line a builder printed, carried in its first field.
const BUILD_LOG_LINE_RESULT_TYPE: u64 = 101;

/// Most verbose Nix log level reported as build output: Nix's own default, `info`, which covers
/// builds, copied paths and errors, but not cache queries or downloads of their metadata.
const OUTPUT_LEVEL_LIMIT: u64 = 3;

/// Bounded number of Nix error lines kept with a failure.
const ERROR_LINE_LIMIT: usize = 8;

/// Error text fragments that Nix reports when it cannot fetch a locked flake or its inputs.
const SOURCE_FETCH_MARKERS: [&str; 7] = [
    "while fetching the input",
    "unable to download",
    "Failed to fetch",
    "failed to fetch",
    "Could not resolve host",
    "Couldn't resolve host",
    "fetching Git repository",
];

/// Next step for a failed build of a development-source artifact.
const DEVELOPMENT_SOURCE_BUILD_HINT: &str = "check that this node's development source builds the package with nix build (Nix sees only files tracked by Git), then retry";

/// Next step for a failed build of a locked artifact.
const LOCKED_BUILD_HINT: &str = "check the Nix build log on this node, then retry";

/// Builds and roots the retained store path of Nix engine artifacts on the hosting node.
#[derive(Clone)]
pub struct NixPreparer {
    roots: Option<PathBuf>,
    command: NixCommand,
    store_root: PathBuf,
}

impl NixPreparer {
    /// Roots every prepared store path with one link in `roots`, the node's root folder.
    pub fn with_roots(mut self, roots: PathBuf) -> Self {
        self.roots = Some(roots);
        self
    }

    /// Runs `nix` at `executable` instead of discovering it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_executable(mut self, executable: PathBuf) -> Self {
        self.command = NixCommand::Executable(executable);
        self
    }

    /// Resolves store paths under `store_root` instead of `/`, for fake stores in tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_store_root(mut self, store_root: PathBuf) -> Self {
        self.store_root = store_root;
        self
    }

    /// Returns where a store path lives on this host, under the store root.
    pub(crate) fn store_location(&self, store_path: &str) -> PathBuf {
        self.store_root.join(store_path.trim_start_matches('/'))
    }

    /// Returns the `nix` executable this preparer runs, if any.
    pub(crate) fn nix_executable(&self) -> Option<PathBuf> {
        self.command.executable()
    }

    /// Builds the retained store path unless a valid root already names it; `self` references
    /// build from the development checkout in `sources`, and locked references from a local source
    /// holding their revision when there is one. One process at a time publishes each root, and
    /// the reuse check runs under the same lock, so removing an unpinned root never races it. A
    /// build reports its log to `step`, which hears nothing when the path is reused, so it ends as
    /// already prepared.
    pub(crate) async fn prepare(
        &self,
        request: &NixPreparationRequest,
        sources: &DotcoreSources,
        cancellation: CancellationToken,
        step: &StepOutput,
    ) -> Result<(), NixPreparationError> {
        let entry = store_entry(&request.store_path)
            .ok_or_else(|| invalid_request("store path is not a top-level /nix/store path"))?;
        let Some(roots) = &self.roots else {
            return self
                .prepare_unrooted(request, sources, &cancellation, step)
                .await;
        };
        let out_link = roots.join(entry);
        let _lock = lock_root(roots, entry, &cancellation, Some(step)).await?;
        if self.roots(&out_link, &request.store_path).await {
            debug!(event_kind = "nix_engine_artifact_reused", store_path = %request.store_path, "Nix engine artifact already rooted");
            return Ok(());
        }
        step.start();
        let installable = self.installable(request, sources).await?;

        info!(
            event_kind = "nix_engine_build_started",
            installable = %installable,
            store_path = %request.store_path,
            "building Nix engine artifact",
        );
        let built = self
            .build(
                &installable,
                Some(&out_link),
                request.policy,
                &cancellation,
                step,
            )
            .await?;
        // The retained path is the package's default output, which the unsuffixed out-link roots;
        // Nix links any further output as `<out-link>-<output>`, which no workload uses.
        if !built.iter().any(|path| path == &request.store_path) {
            // The link names another path than the retained one, so no workload can use it.
            remove_out_link(&out_link).await?;
            return Err(mismatch(request, installable, &built));
        }
        if !self.roots(&out_link, &request.store_path).await {
            return Err(NixPreparationError::OutLinkMissing {
                out_link: out_link.display().to_string(),
            });
        }
        info!(
            event_kind = "nix_engine_artifact_published",
            store_path = %request.store_path,
            out_link = %out_link.display(),
            "Nix engine artifact built and rooted",
        );
        Ok(())
    }

    /// Reuses the retained store path when it is already in the store, else builds it without
    /// rooting it.
    async fn prepare_unrooted(
        &self,
        request: &NixPreparationRequest,
        sources: &DotcoreSources,
        cancellation: &CancellationToken,
        step: &StepOutput,
    ) -> Result<(), NixPreparationError> {
        if self.present(&request.store_path).await {
            debug!(event_kind = "nix_engine_artifact_reused", store_path = %request.store_path, "Nix engine artifact already in the store");
            return Ok(());
        }
        step.start();
        let installable = self.installable(request, sources).await?;
        info!(
            event_kind = "nix_engine_build_started",
            installable = %installable,
            store_path = %request.store_path,
            "building unrooted Nix engine artifact",
        );
        let built = self
            .build(&installable, None, request.policy, cancellation, step)
            .await?;
        if !built.iter().any(|path| path == &request.store_path) {
            return Err(mismatch(request, installable, &built));
        }
        Ok(())
    }

    /// Returns the installable to build, once the request's policy matches its flake reference. A
    /// locked revision builds from a local Dotcore source holding it, so this node needs no GitHub
    /// access for it; otherwise from GitHub.
    async fn installable(
        &self,
        request: &NixPreparationRequest,
        sources: &DotcoreSources,
    ) -> Result<String, NixPreparationError> {
        let reference = NixFlakeReference::parse(&request.flake).map_err(|_| {
            invalid_request("flake reference is not a locked dotcoreinc/dotcore-oss revision or self")
        })?;
        if request.policy != reference.preparation_policy() {
            return Err(invalid_request(
                "preparation policy does not match the flake reference",
            ));
        }
        if !dotcore_domain::nix::valid_package(&request.package) {
            return Err(invalid_request(
                "package is not an attribute path of plain names",
            ));
        }
        let local = match reference.revision() {
            Some(revision) => self.local_source(revision, sources).await,
            None => None,
        };
        let source = match local {
            Some(source) => source,
            None => reference
                .source_url(sources.development.as_deref())
                .ok_or_else(|| NixPreparationError::DevelopmentSourceUnavailable {
                    flake: request.flake.clone(),
                })?,
        };
        Ok(dotcore_domain::nix::installable(
            &source,
            &request.system,
            &request.package,
        ))
    }

    /// Returns a local flake URL holding a locked revision: the packaged runtime's own source at
    /// that revision, else the development checkout when it holds the commit.
    async fn local_source(&self, revision: &str, sources: &DotcoreSources) -> Option<String> {
        if let Some(packaged) = sources
            .packaged
            .as_ref()
            .filter(|packaged| packaged.revision() == revision)
        {
            return Some(packaged.url().to_owned());
        }
        // A checkout URL with its own query cannot take a revision.
        let checkout = sources
            .development
            .as_deref()
            .filter(|development| {
                development.starts_with("git+file://") && !development.contains('?')
            })
            .map(|development| format!("{development}?rev={revision}"))?;
        self.resolves(&checkout).await.then_some(checkout)
    }

    /// Whether Nix resolves a flake URL, for example a checkout at a commit it may not hold. Any
    /// failure to run Nix counts as no; the build that follows reports it.
    async fn resolves(&self, url: &str) -> bool {
        let Some(nix) = self.command.executable() else {
            return false;
        };
        tokio::process::Command::new(nix)
            .args(EXPERIMENTAL_FEATURES)
            .args(["flake", "metadata", "--no-write-lock-file", url])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await
            .is_ok_and(|status| status.success())
    }

    /// Whether `out_link` names `store_path` and that path exists in the store.
    async fn roots(&self, out_link: &Path, store_path: &str) -> bool {
        let named = tokio::fs::read_link(out_link)
            .await
            .is_ok_and(|target| target == Path::new(store_path));
        named && self.present(store_path).await
    }

    /// Whether `store_path` exists in the store.
    async fn present(&self, store_path: &str) -> bool {
        tokio::fs::symlink_metadata(self.store_location(store_path))
            .await
            .is_ok()
    }

    /// Runs `nix build` for `installable`, rooting it at `out_link` when given, reporting its log
    /// to `step`, and returns the built paths.
    async fn build(
        &self,
        installable: &str,
        out_link: Option<&Path>,
        policy: ArtifactPreparationPolicy,
        cancellation: &CancellationToken,
        step: &StepOutput,
    ) -> Result<Vec<String>, NixPreparationError> {
        let nix = self
            .command
            .executable()
            .ok_or(NixPreparationError::NixMissing)?;
        let mut command = tokio::process::Command::new(nix);
        command.args(EXPERIMENTAL_FEATURES).args([
            "build",
            "--no-write-lock-file",
            "--log-format",
            "internal-json",
            "--print-out-paths",
        ]);
        match out_link {
            Some(out_link) => command.arg("--out-link").arg(out_link),
            None => command.arg("--no-link"),
        };
        let mut child = command
            .arg(installable)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| match source.kind() {
                std::io::ErrorKind::NotFound => NixPreparationError::NixMissing,
                _ => NixPreparationError::Spawn(source),
            })?;
        let mut stdout = child.stdout.take().expect("piped nix stdout");
        let stderr = child.stderr.take().expect("piped nix stderr");
        let log = BuildLog { step };
        let run = async {
            let (output, errors, status) = tokio::join!(
                async {
                    let mut output = String::new();
                    stdout.read_to_string(&mut output).await.map(|_| output)
                },
                log.read(stderr),
                child.wait(),
            );
            let mut output = output.map_err(|source| NixPreparationError::Io {
                context: "could not read nix build output",
                source,
            })?;
            let status = status.map_err(|source| NixPreparationError::Io {
                context: "could not wait for nix build",
                source,
            })?;
            output.truncate(output.trim_end().len());
            Ok::<_, NixPreparationError>((output, errors, status))
        };
        let (output, errors, status) = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(NixPreparationError::Cancelled),
            result = run => result?,
        };
        if !status.success() {
            let detail = if errors.is_empty() {
                format!("nix exited with {status}")
            } else {
                errors.join("; ")
            };
            let installable = installable.to_owned();
            // A development source is local, so its failures are build failures of that checkout
            // even when Nix reports them while fetching it.
            return Err(match policy {
                ArtifactPreparationPolicy::DevelopmentSource => NixPreparationError::Build {
                    installable,
                    hint: DEVELOPMENT_SOURCE_BUILD_HINT,
                    detail,
                },
                _ if SOURCE_FETCH_MARKERS
                    .iter()
                    .any(|marker| detail.contains(marker)) =>
                {
                    NixPreparationError::SourceFetch {
                        installable,
                        detail,
                    }
                }
                _ => NixPreparationError::Build {
                    installable,
                    hint: LOCKED_BUILD_HINT,
                    detail,
                },
            });
        }
        Ok(output.lines().map(str::to_owned).collect())
    }
}

impl Default for NixPreparer {
    /// Prepares without rooting what it builds.
    fn default() -> Self {
        Self {
            roots: None,
            command: NixCommand::Discover,
            store_root: PathBuf::from("/"),
        }
    }
}

/// One retained Nix engine artifact to prepare on the hosting node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NixPreparationRequest {
    pub flake: String,
    pub package: String,
    pub system: String,
    pub store_path: String,
    pub policy: ArtifactPreparationPolicy,
}

/// Failures while preparing one Nix engine artifact on the hosting node.
#[derive(Debug, thiserror::Error)]
pub enum NixPreparationError {
    #[error(
        "Nix is not installed on this node: no nix executable in {}. Install Nix on the hosting node, then retry",
        dotcore_domain::nix::search_locations()
    )]
    NixMissing,

    #[error(
        "Nix engine artifact {flake:?} builds from the Dotcore development source, but this node has none. Set DOTCORE_DEV_SOURCE_FLAKE for the backend on this node, then retry"
    )]
    DevelopmentSourceUnavailable { flake: String },

    #[error("retained Nix engine artifact is invalid: {reason}")]
    InvalidRequest { reason: &'static str },

    #[error(
        "could not fetch the flake source of {installable:?} on this node; a node fetches a locked revision unless it runs a Dotcore build of that revision or its development checkout holds that commit, so run this node at that revision or check network and repository access, then retry: {detail}"
    )]
    SourceFetch { installable: String, detail: String },

    #[error("nix build of {installable:?} failed on this node; {hint}: {detail}")]
    Build {
        installable: String,
        hint: &'static str,
        detail: String,
    },

    #[error(
        "nix build of {installable:?} produced {built}, but the deployment retains {expected}; {hint}"
    )]
    StorePathMismatch {
        installable: String,
        expected: String,
        built: String,
        hint: &'static str,
    },

    #[error("nix build succeeded but did not create the out-link {out_link}")]
    OutLinkMissing { out_link: String },

    #[error("Nix engine artifact preparation was cancelled")]
    Cancelled,

    #[error("could not run nix build")]
    Spawn(#[source] std::io::Error),

    #[error("Nix engine artifact cache lock failed")]
    Lock(#[source] ArtifactCacheError),

    #[error("Nix engine artifact preparation failed: {context}")]
    Io {
        context: &'static str,
        #[source]
        source: std::io::Error,
    },
}

impl NixPreparationError {
    pub(crate) fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

/// Reads the internal-JSON Nix log, reporting it as readable lines and keeping error messages.
struct BuildLog<'a> {
    step: &'a StepOutput,
}

impl BuildLog<'_> {
    /// Consumes the log and returns its bounded error messages. Lines are decoded lossily so
    /// bytes that are not UTF-8 never stop the log from draining.
    async fn read(&self, stderr: tokio::process::ChildStderr) -> Vec<String> {
        let mut errors = VecDeque::with_capacity(ERROR_LINE_LIMIT);
        let mut reader = BufReader::new(stderr);
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            if !matches!(reader.read_until(b'\n', &mut bytes).await, Ok(read) if read > 0) {
                break;
            }
            let line = String::from_utf8_lossy(&bytes);
            let line = line.trim_end_matches(['\n', '\r']);
            let message = match line
                .strip_prefix("@nix ")
                .map(serde_json::from_str::<serde_json::Value>)
            {
                Some(Ok(entry)) => self.record(&entry),
                Some(Err(_)) | None => {
                    self.report(line);
                    Some(line.to_owned())
                }
            };
            if let Some(message) = message.map(|message| strip_ansi(&message))
                && !message.trim().is_empty()
            {
                if errors.len() == ERROR_LINE_LIMIT {
                    errors.pop_front();
                }
                errors.push_back(message.trim().to_owned());
            }
        }
        errors.into()
    }

    /// Reports one log entry as lines: messages and activities Nix prints by default, such as
    /// a build starting or a path copied from a cache, and each line a builder printed. Returns
    /// the entry's text when it is an error message.
    fn record(&self, entry: &serde_json::Value) -> Option<String> {
        match entry.get("action")?.as_str()? {
            "msg" => {
                let level = entry.get("level")?.as_u64()?;
                let message = entry.get("msg")?.as_str()?;
                if level <= OUTPUT_LEVEL_LIMIT {
                    message.lines().for_each(|line| self.report(line));
                }
                (level == 0).then(|| message.to_owned())
            }
            "start" => {
                let text = entry.get("text")?.as_str()?;
                if !text.is_empty() {
                    debug!(event_kind = "nix_build_activity", activity = %text, "Nix build activity");
                    if entry.get("level")?.as_u64()? <= OUTPUT_LEVEL_LIMIT {
                        self.report(text);
                    }
                }
                None
            }
            "result" if entry.get("type")?.as_u64()? == BUILD_LOG_LINE_RESULT_TYPE => {
                self.report(entry.get("fields")?.as_array()?.first()?.as_str()?);
                None
            }
            _ => None,
        }
    }

    /// Reports one line of the log without its terminal color sequences; Nix writes its whole log
    /// to stderr.
    fn report(&self, line: &str) {
        self.step.line(OutputStream::Stderr, strip_ansi(line));
    }
}

fn invalid_request(reason: &'static str) -> NixPreparationError {
    NixPreparationError::InvalidRequest { reason }
}

fn mismatch(
    request: &NixPreparationRequest,
    installable: String,
    built: &[String],
) -> NixPreparationError {
    NixPreparationError::StorePathMismatch {
        installable,
        expected: request.store_path.clone(),
        built: built.join(", "),
        hint: if request.policy == ArtifactPreparationPolicy::DevelopmentSource {
            "this node's development source changed since this node pinned it; create a new deployment"
        } else {
            "the locked flake does not reproduce the retained path on this node; create a new deployment"
        },
    }
}

/// Takes the cross-process lock of one root in the root folder `roots`, creating the folder, and
/// reports `step` waits while another preparation holds it.
pub(crate) async fn lock_root(
    roots: &Path,
    entry: &str,
    cancellation: &CancellationToken,
    step: Option<&StepOutput>,
) -> Result<std::fs::File, NixPreparationError> {
    let path = roots.join(LOCK_DIRECTORY).join(format!("{entry}.lock"));
    match step {
        Some(step) => acquire_step_lock(path, cancellation, step).await,
        None => acquire_lock(path, cancellation, None).await,
    }
    .map_err(|error| match error {
        ArtifactCacheError::Cancelled => NixPreparationError::Cancelled,
        error => NixPreparationError::Lock(error),
    })
}

/// Returns the top-level store entry of a store path, for example `<hash>-llama-cpp-b11417`.
pub(crate) fn store_entry(store_path: &str) -> Option<&str> {
    dotcore_domain::nix::valid_store_path(store_path)
        .then(|| store_path.rsplit('/').next())
        .flatten()
}

async fn remove_out_link(out_link: &Path) -> Result<(), NixPreparationError> {
    match tokio::fs::remove_file(out_link).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(NixPreparationError::Io {
            context: "could not remove the mismatched Nix out-link",
            source,
        }),
    }
}

/// Removes terminal color sequences, which Nix includes in error messages.
fn strip_ansi(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            for next in characters.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::fs::PermissionsExt as _,
        time::{Duration, Instant},
    };

    use dotcore_domain::test_support::nix::{STORE_PATH, locked_flake};

    use super::*;
    use crate::{
        ArtifactManager, EngineArtifactPreparation, EngineArtifactSource, NixSources, OciPlatform,
        output::{PreparationEventKind, PreparationStep, test_support::RecordedOutput},
    };

    const OTHER_STORE_PATH: &str = "/nix/store/00000000000000000000000000000002-other";
    const DEVELOPMENT_FLAKE: &str = "self";
    const DEVELOPMENT_SOURCE: &str = "git+file:///src/dotcore";

    #[tokio::test]
    async fn locked_revisions_build_from_a_local_source_holding_them() {
        let revision = dotcore_domain::test_support::nix::REVISION;
        let packaged = |revision: &str| {
            dotcore_domain::nix::LocalDotcoreSource::parse(&format!(
                "path:/nix/store/0123456789abcdfghijklmnpqrsvwxyz-source?rev={revision}"
            ))
        };
        let checkout = format!("{DEVELOPMENT_SOURCE}?rev={revision}");
        let installable = |source: &str| {
            format!("{source}#legacyPackages.aarch64-darwin.engines.llama-cpp.native")
        };
        // Each Nix call is identified by its last argument: the checkout a metadata call resolves,
        // or the installable a build builds.
        let metadata = checkout.clone();
        let other = "f".repeat(40);
        let cases = [
            (
                "packaged at the revision",
                DotcoreSources {
                    development: Some(DEVELOPMENT_SOURCE.to_owned()),
                    packaged: packaged(revision),
                },
                true,
                vec![installable(packaged(revision).expect("packaged").url())],
            ),
            (
                "checkout holding the revision",
                DotcoreSources {
                    development: Some(DEVELOPMENT_SOURCE.to_owned()),
                    packaged: packaged(&other),
                },
                true,
                vec![metadata.clone(), installable(&checkout)],
            ),
            (
                "checkout without the revision",
                development(DEVELOPMENT_SOURCE),
                false,
                vec![metadata.clone(), installable(&locked_flake())],
            ),
            (
                "no local source",
                DotcoreSources::default(),
                false,
                vec![installable(&locked_flake())],
            ),
        ];

        for (case, sources, checkout_holds, expected) in cases {
            let fixture = Fixture::new();
            let nix = fixture.nix(&format!(
                "if [ \"$4\" = metadata ]; then exit {}; fi\nln -sfn '{STORE_PATH}' \"$link\"\necho '{STORE_PATH}'",
                if checkout_holds { 0 } else { 1 }
            ));

            fixture
                .preparer(nix)
                .prepare(
                    &request(&locked_flake()),
                    &sources,
                    CancellationToken::new(),
                    &discarded(),
                )
                .await
                .unwrap_or_else(|error| panic!("{case}: {error}"));

            let calls: Vec<String> = fixture
                .calls()
                .iter()
                .map(|call| call.rsplit(' ').next().unwrap_or_default().to_owned())
                .collect();
            assert_eq!(calls, expected, "{case}");
        }
    }

    #[tokio::test]
    async fn locked_artifact_is_built_rooted_in_the_root_folder_and_reported_already_prepared_once_rooted()
     {
        let fixture = Fixture::new();
        let nix = fixture.nix(&format!(
            "{}\nln -sfn '{STORE_PATH}' \"$link\"\necho '{STORE_PATH}'",
            fixture.log(BUILD_LOG)
        ));
        let preparer = fixture.preparer(nix);
        let recorded = RecordedOutput::new();
        let request = request(&locked_flake());
        let sources = DotcoreSources::default();
        // Each call completes its step as the artifact manager does.
        let prepare = || {
            let step = recorded
                .output()
                .step(PreparationStep::EngineBuild, STORE_PATH);
            let (preparer, request, sources) = (&preparer, &request, &sources);
            async move {
                let result = preparer
                    .prepare(request, sources, CancellationToken::new(), &step)
                    .await;
                step.complete(&result, |_| false);
                result
            }
        };

        prepare().await.expect("locked preparation");
        let built = recorded.kinds();
        prepare().await.expect("rooted preparation");

        let out_link = fixture.out_link();
        assert_eq!(
            std::fs::read_link(&out_link).expect("out-link"),
            Path::new(STORE_PATH)
        );
        assert_eq!(
            fixture.calls(),
            [format!(
                "--extra-experimental-features nix-command flakes build --no-write-lock-file --log-format internal-json --print-out-paths --out-link {} {}#legacyPackages.aarch64-darwin.engines.llama-cpp.native",
                out_link.display(),
                locked_flake()
            )]
        );
        assert_eq!(
            built,
            [
                (
                    PreparationStep::EngineBuild,
                    PreparationEventKind::Started {
                        artifact: STORE_PATH.to_owned()
                    }
                ),
                build_line("this derivation will be built:"),
                build_line("  /nix/store/00000000000000000000000000000003-fixture.drv"),
                build_line(
                    "copying path '/nix/store/00000000000000000000000000000004-dependency' from 'https://cache.nixos.org'"
                ),
                build_line("building '/nix/store/00000000000000000000000000000003-fixture.drv'"),
                build_line("compiling fixture"),
                build_line("plain stderr line"),
                (
                    PreparationStep::EngineBuild,
                    PreparationEventKind::Finished {
                        duration: Duration::ZERO
                    }
                ),
            ]
        );
        assert_eq!(
            recorded.kinds()[built.len()..],
            [(
                PreparationStep::EngineBuild,
                PreparationEventKind::AlreadyPrepared {
                    artifact: STORE_PATH.to_owned()
                }
            )],
            "a rooted artifact reports only that it is already prepared"
        );
    }

    #[tokio::test]
    async fn failed_build_reports_its_log_and_the_error_it_returns() {
        let fixture = Fixture::new();
        let nix = fixture.nix(&format!("{}\nexit 1", fixture.log(FAILED_BUILD_LOG)));
        let recorded = RecordedOutput::new();

        let error = fixture
            .manager(nix)
            .prepare_engine(&nix_engine(), CancellationToken::new(), &recorded.output())
            .await
            .expect_err("failed build");

        assert!(
            matches!(
                &error,
                crate::ArtifactPreparationError::Nix(NixPreparationError::Build { .. })
            ),
            "{error:?}"
        );
        assert_eq!(
            recorded.kinds(),
            [
                (
                    PreparationStep::EngineBuild,
                    PreparationEventKind::Started {
                        artifact: STORE_PATH.to_owned()
                    }
                ),
                build_line("building '/nix/store/00000000000000000000000000000003-fixture.drv'"),
                build_line("make: *** [all] Error 2"),
                build_line(
                    "error: Cannot build '/nix/store/00000000000000000000000000000003-fixture.drv'."
                ),
                build_line("       Reason: builder failed with exit code 2."),
                (
                    PreparationStep::EngineBuild,
                    PreparationEventKind::Failed {
                        error: error.to_string()
                    }
                ),
            ]
        );
    }

    #[tokio::test]
    async fn failures_map_to_actionable_typed_errors() {
        let fixture = Fixture::new();
        let failing = |message: &str| {
            fixture.nix(&format!(
                "echo '@nix {{\"action\":\"msg\",\"level\":0,\"msg\":\"{message}\"}}' >&2\nexit 1"
            ))
        };
        let prepare = |preparer: NixPreparer, flake: String| async move {
            preparer
                .prepare(
                    &request(&flake),
                    &DotcoreSources::default(),
                    CancellationToken::new(),
                    &discarded(),
                )
                .await
                .expect_err("preparation failure")
        };
        let prepare_development = |preparer: NixPreparer| async move {
            preparer
                .prepare(
                    &request(DEVELOPMENT_FLAKE),
                    &development(DEVELOPMENT_SOURCE),
                    CancellationToken::new(),
                    &discarded(),
                )
                .await
                .expect_err("development-source preparation failure")
        };

        let missing = prepare(
            fixture.preparer(fixture.root_path().join("absent-nix")),
            locked_flake(),
        )
        .await;
        assert!(matches!(missing, NixPreparationError::NixMissing));
        assert!(
            missing
                .to_string()
                .contains("Install Nix on the hosting node")
        );

        let fetch = prepare(
            fixture.preparer(failing(
                "error: \\u001b[31mwhile fetching the input github:o/r\\u001b[0m: unable to download",
            )),
            locked_flake(),
        )
        .await;
        assert!(
            matches!(&fetch, NixPreparationError::SourceFetch { detail, .. }
                if detail == "error: while fetching the input github:o/r: unable to download"),
            "{fetch:?}"
        );
        assert!(
            fetch
                .to_string()
                .contains("check network and repository access")
        );

        let build = prepare(
            fixture.preparer(failing(
                "error: builder for /nix/store/x.drv failed with exit code 2",
            )),
            locked_flake(),
        )
        .await;
        assert!(
            matches!(&build, NixPreparationError::Build { detail, hint, .. }
                if detail == "error: builder for /nix/store/x.drv failed with exit code 2"
                    && *hint == LOCKED_BUILD_HINT),
            "{build:?}"
        );

        let development_build = prepare_development(fixture.preparer(failing(
            "error: while fetching the input git+file:///src/dotcore: path is not tracked",
        )))
        .await;
        assert!(
            matches!(&development_build, NixPreparationError::Build { detail, hint, .. }
                if detail == "error: while fetching the input git+file:///src/dotcore: path is not tracked"
                    && *hint == DEVELOPMENT_SOURCE_BUILD_HINT),
            "{development_build:?}"
        );
        assert!(
            development_build
                .to_string()
                .contains("Nix sees only files tracked by Git")
        );

        let mismatch = prepare_development(fixture.preparer(fixture.nix(&format!(
            "ln -sfn '{OTHER_STORE_PATH}' \"$link\"\necho '{OTHER_STORE_PATH}'"
        ))))
        .await;
        assert!(
            matches!(&mismatch, NixPreparationError::StorePathMismatch { expected, built, .. }
                if expected == STORE_PATH && built == OTHER_STORE_PATH),
            "{mismatch:?}"
        );
        assert!(
            mismatch
                .to_string()
                .contains("changed since this node pinned it")
        );
        assert!(
            std::fs::symlink_metadata(fixture.out_link()).is_err(),
            "mismatched out-link is removed"
        );

        let calls = fixture.calls().len();
        let unavailable = prepare(
            fixture.preparer(failing("unreachable")),
            DEVELOPMENT_FLAKE.to_owned(),
        )
        .await;
        assert!(matches!(
            &unavailable,
            NixPreparationError::DevelopmentSourceUnavailable { flake } if flake == DEVELOPMENT_FLAKE
        ));
        assert!(unavailable.to_string().contains("DOTCORE_DEV_SOURCE_FLAKE"));
        assert_eq!(
            fixture.calls().len(),
            calls,
            "nix is not run without a source"
        );
    }

    #[tokio::test]
    async fn build_log_with_invalid_utf8_keeps_draining_to_the_error() {
        let fixture = Fixture::new();
        let nix = fixture.nix(
            "printf 'garbage \\377\\376\\n' >&2\necho '@nix {\"action\":\"msg\",\"level\":0,\"msg\":\"error: after bad bytes\"}' >&2\nexit 1",
        );

        let error = fixture
            .preparer(nix)
            .prepare(
                &request(&locked_flake()),
                &DotcoreSources::default(),
                CancellationToken::new(),
                &discarded(),
            )
            .await
            .expect_err("build failure");

        assert!(
            matches!(&error, NixPreparationError::Build { detail, .. }
                if detail == "garbage \u{fffd}\u{fffd}; error: after bad bytes"),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn locked_reference_to_another_repository_is_rejected_before_nix_runs() {
        let fixture = Fixture::new();
        let request = NixPreparationRequest {
            flake: format!(
                "github:example/engines/{}",
                "0123456789abcdef".repeat(2) + "01234567"
            ),
            policy: ArtifactPreparationPolicy::ImmutableRemote,
            ..request(&locked_flake())
        };

        let error = fixture
            .preparer(fixture.nix("exit 97"))
            .prepare(
                &request,
                &DotcoreSources::default(),
                CancellationToken::new(),
                &discarded(),
            )
            .await
            .expect_err("untrusted repository");

        assert!(
            matches!(&error, NixPreparationError::InvalidRequest { reason }
                if reason.contains("dotcoreinc/dotcore-oss")),
            "{error:?}"
        );
        assert!(fixture.calls().is_empty(), "nix must not run");
    }

    #[tokio::test]
    async fn existing_valid_out_link_is_reused_without_running_nix() {
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.out_link().parent().expect("out-link directory"))
            .expect("out-link directory");
        std::os::unix::fs::symlink(STORE_PATH, fixture.out_link()).expect("existing out-link");
        let preparer = fixture.preparer(fixture.nix("exit 97"));
        let recorded = RecordedOutput::new();

        preparer
            .prepare(
                &request(&locked_flake()),
                &DotcoreSources::default(),
                CancellationToken::new(),
                &discarded(),
            )
            .await
            .expect("reused out-link");
        fixture
            .manager(fixture.nix("exit 97"))
            .prepare_engine(&nix_engine(), CancellationToken::new(), &recorded.output())
            .await
            .expect("reused engine");

        assert!(fixture.calls().is_empty(), "nix must not run");
        assert_eq!(
            recorded.kinds(),
            [(
                PreparationStep::EngineBuild,
                PreparationEventKind::AlreadyPrepared {
                    artifact: STORE_PATH.to_owned()
                }
            )]
        );
    }

    #[tokio::test]
    async fn without_a_root_folder_a_present_path_is_reused_and_a_build_roots_nothing() {
        let fixture = Fixture::new();
        let recorded = RecordedOutput::new();
        // Each call completes its step as the artifact manager does.
        let prepare = |nix: PathBuf, request: NixPreparationRequest| {
            let step = recorded
                .output()
                .step(PreparationStep::EngineBuild, request.store_path.clone());
            let preparer = fixture.unrooted_preparer(nix);
            async move {
                let result = preparer
                    .prepare(
                        &request,
                        &DotcoreSources::default(),
                        CancellationToken::new(),
                        &step,
                    )
                    .await;
                step.complete(&result, |_| false);
                result
            }
        };

        prepare(fixture.nix("exit 97"), request(&locked_flake()))
            .await
            .expect("present path reused");
        assert!(fixture.calls().is_empty(), "nix must not run");
        let reused = recorded.kinds();
        assert_eq!(
            reused,
            [(
                PreparationStep::EngineBuild,
                PreparationEventKind::AlreadyPrepared {
                    artifact: STORE_PATH.to_owned()
                }
            )]
        );

        let absent = NixPreparationRequest {
            store_path: OTHER_STORE_PATH.to_owned(),
            ..request(&locked_flake())
        };
        prepare(fixture.nix(&format!("echo '{OTHER_STORE_PATH}'")), absent)
            .await
            .expect("unrooted build");

        assert_eq!(
            fixture.calls(),
            [format!(
                "--extra-experimental-features nix-command flakes build --no-write-lock-file --log-format internal-json --print-out-paths --no-link {}#legacyPackages.aarch64-darwin.engines.llama-cpp.native",
                locked_flake()
            )]
        );
        assert_eq!(
            recorded.kinds()[reused.len()..],
            [
                (
                    PreparationStep::EngineBuild,
                    PreparationEventKind::Started {
                        artifact: OTHER_STORE_PATH.to_owned()
                    }
                ),
                (
                    PreparationStep::EngineBuild,
                    PreparationEventKind::Finished {
                        duration: Duration::ZERO
                    }
                ),
            ]
        );
        assert!(
            !fixture.root_path().join("roots").exists(),
            "no root folder is created"
        );
    }

    #[test]
    fn two_processes_preparing_one_artifact_publish_it_once() {
        const ROLE: &str = "DOTCORE_NIX_PREPARATION_CHILD";
        const ROOT: &str = "DOTCORE_NIX_PREPARATION_ROOT";
        const TEST_NAME: &str =
            "nix_build::tests::two_processes_preparing_one_artifact_publish_it_once";

        if let Some(role) = std::env::var_os(ROLE) {
            let fixture = Fixture::at(PathBuf::from(std::env::var_os(ROOT).expect("root")));
            let preparer = fixture.preparer(fixture.root_path().join("nix-bin"));
            tokio::runtime::Runtime::new()
                .expect("runtime")
                .block_on(preparer.prepare(
                    &request(&locked_flake()),
                    &DotcoreSources::default(),
                    CancellationToken::new(),
                    &discarded(),
                ))
                .expect("child preparation");
            std::fs::write(
                fixture
                    .root_path()
                    .join(format!("{}-done", role.to_string_lossy())),
                b"done",
            )
            .expect("publish child result");
            return;
        }

        let fixture = Fixture::new();
        let nix = fixture.nix(&format!(
            "sleep 1\nln -sfn '{STORE_PATH}' \"$link\"\necho '{STORE_PATH}'"
        ));
        std::fs::rename(nix, fixture.root_path().join("nix-bin")).expect("shared fake nix");
        let children = ["first", "second"].map(|role| {
            std::process::Command::new(std::env::current_exe().expect("test executable"))
                .args(["--exact", TEST_NAME, "--nocapture"])
                .env(ROLE, role)
                .env(ROOT, fixture.root_path())
                .spawn()
                .expect("child process")
        });

        let deadline = Instant::now() + Duration::from_secs(60);
        for mut child in children {
            loop {
                if let Some(status) = child.try_wait().expect("child status") {
                    assert!(status.success(), "child preparation failed");
                    break;
                }
                assert!(Instant::now() < deadline, "child preparation deadline");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        assert_eq!(fixture.calls().len(), 1, "one process builds and publishes");
        for role in ["first", "second"] {
            assert!(fixture.root_path().join(format!("{role}-done")).is_file());
        }
        assert_eq!(
            std::fs::read_link(fixture.out_link()).expect("out-link"),
            Path::new(STORE_PATH)
        );
    }

    /// Builds the repository flake's llama.cpp engine package from this checkout as a development
    /// source, then re-prepares it from the existing out-link with a `nix` that cannot run. Skipped
    /// without Nix or a Git checkout.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn host_nix_builds_and_roots_the_llama_cpp_engine_package_from_the_development_source() {
        use crate::nix::{CommandNixEvaluator, NixEvaluator as _};

        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("repository root");
        if dotcore_domain::nix::discover_nix().is_none() || !repository.join(".git").exists() {
            eprintln!("skipping: requires Nix and a Git checkout");
            return;
        }
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let development_source = format!("git+file://{}", repository.display());
        let evaluation = runtime
            .block_on(CommandNixEvaluator::default().evaluate(
                &dotcore_domain::nix::installable(
                    &development_source,
                    "aarch64-darwin",
                    "engines.llama-cpp.native",
                ),
                tokio_util::sync::CancellationToken::new(),
            ))
            .expect("host evaluation");
        let cache = tempfile::tempdir().expect("cache");
        let request = NixPreparationRequest {
            package: "engines.llama-cpp.native".to_owned(),
            store_path: evaluation.store_path.clone(),
            ..request(DEVELOPMENT_FLAKE)
        };

        runtime
            .block_on(
                NixPreparer::default()
                    .with_roots(cache.path().to_owned())
                    .prepare(
                        &request,
                        &development(&development_source),
                        CancellationToken::new(),
                        &discarded(),
                    ),
            )
            .expect("host nix build");
        let out_link = cache
            .path()
            .join(store_entry(&evaluation.store_path).expect("store entry"));
        assert_eq!(
            std::fs::read_link(&out_link).expect("out-link"),
            Path::new(&evaluation.store_path)
        );

        runtime
            .block_on(
                NixPreparer::default()
                    .with_roots(cache.path().to_owned())
                    .with_executable(cache.path().join("absent-nix"))
                    .prepare(
                        &request,
                        &DotcoreSources::default(),
                        CancellationToken::new(),
                        &discarded(),
                    ),
            )
            .expect("re-preparation from the out-link");
    }

    /// Log of a build that copies a dependency from a cache and builds one derivation, in the
    /// shape Nix 2.34 writes it: entries Nix shows by default, entries it hides, and one line
    /// outside the structured format.
    const BUILD_LOG: &str = r#"@nix {"action":"msg","level":3,"msg":"this derivation will be built:"}
@nix {"action":"msg","level":3,"msg":"  /nix/store/00000000000000000000000000000003-fixture.drv"}
@nix {"action":"start","id":1,"level":6,"parent":0,"text":"querying info about missing paths","type":0}
@nix {"action":"start","fields":["https://cache.nixos.org/00000000000000000000000000000004.narinfo"],"id":2,"level":4,"parent":0,"text":"downloading 'https://cache.nixos.org/00000000000000000000000000000004.narinfo'","type":101}
@nix {"action":"start","id":3,"level":0,"parent":0,"text":"","type":102}
@nix {"action":"start","fields":["/nix/store/00000000000000000000000000000004-dependency","https://cache.nixos.org","local"],"id":4,"level":3,"parent":0,"text":"copying path '/nix/store/00000000000000000000000000000004-dependency' from 'https://cache.nixos.org'","type":100}
@nix {"action":"result","fields":[1,2,0,0],"id":3,"type":105}
@nix {"action":"result","fields":[101,1],"id":3,"type":106}
@nix {"action":"start","fields":["/nix/store/00000000000000000000000000000003-fixture.drv","",1,1],"id":5,"level":3,"parent":0,"text":"building '/nix/store/00000000000000000000000000000003-fixture.drv'","type":105}
@nix {"action":"result","fields":["\u001b[32mcompiling\u001b[0m fixture"],"id":5,"type":101}
@nix {"action":"msg","level":5,"msg":"verbose detail"}
@nix {"action":"stop","id":5}
plain stderr line
"#;

    /// Log of a build whose builder fails, ending with Nix's error message.
    const FAILED_BUILD_LOG: &str = r#"@nix {"action":"start","fields":["/nix/store/00000000000000000000000000000003-fixture.drv","",1,1],"id":5,"level":3,"parent":0,"text":"building '/nix/store/00000000000000000000000000000003-fixture.drv'","type":105}
@nix {"action":"result","fields":["make: *** [all] Error 2"],"id":5,"type":101}
@nix {"action":"msg","column":null,"file":null,"level":0,"line":null,"msg":"\u001b[31;1merror:\u001b[0m Cannot build '\u001b[35;1m/nix/store/00000000000000000000000000000003-fixture.drv\u001b[0m'.\n       Reason: \u001b[31;1mbuilder failed with exit code 2\u001b[0m."}
"#;

    fn build_line(text: &str) -> (PreparationStep, PreparationEventKind) {
        (
            PreparationStep::EngineBuild,
            PreparationEventKind::Line {
                stream: OutputStream::Stderr,
                text: text.to_owned(),
            },
        )
    }

    /// Native Nix engine of the fixture's locked flake, retained at the fixture store path.
    fn nix_engine() -> EngineArtifactPreparation {
        EngineArtifactPreparation {
            source: EngineArtifactSource::Nix {
                flake: locked_flake(),
                package: "engines.llama-cpp.native".to_owned(),
            },
            platform: OciPlatform {
                operating_system: "darwin".to_owned(),
                architecture: "arm64".to_owned(),
                variant: None,
            },
            preparation_policy: ArtifactPreparationPolicy::ImmutableRemote,
            launch_identity: STORE_PATH.to_owned(),
        }
    }

    fn discarded() -> StepOutput {
        StepOutput::discard()
    }

    fn development(source: &str) -> DotcoreSources {
        DotcoreSources {
            development: Some(source.to_owned()),
            packaged: None,
        }
    }

    fn request(flake: &str) -> NixPreparationRequest {
        let locked = NixFlakeReference::parse(flake)
            .expect("fixture flake")
            .is_locked();
        NixPreparationRequest {
            flake: flake.to_owned(),
            package: "engines.llama-cpp.native".to_owned(),
            system: "aarch64-darwin".to_owned(),
            store_path: STORE_PATH.to_owned(),
            policy: if locked {
                ArtifactPreparationPolicy::ImmutableRemote
            } else {
                ArtifactPreparationPolicy::DevelopmentSource
            },
        }
    }

    /// Temporary cache, fake store holding the fixture store path, and fake `nix` call log.
    struct Fixture {
        root: FixtureRoot,
    }

    enum FixtureRoot {
        Owned(tempfile::TempDir),
        Shared(PathBuf),
    }

    impl FixtureRoot {
        fn path(&self) -> &Path {
            match self {
                Self::Owned(directory) => directory.path(),
                Self::Shared(path) => path,
            }
        }
    }

    impl Fixture {
        fn new() -> Self {
            let fixture = Self {
                root: FixtureRoot::Owned(
                    tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
                        .expect("fixture root"),
                ),
            };
            std::fs::create_dir_all(
                fixture
                    .root_path()
                    .join("store")
                    .join(STORE_PATH.trim_start_matches('/')),
            )
            .expect("fake store path");
            fixture
        }

        fn at(root: PathBuf) -> Self {
            Self {
                root: FixtureRoot::Shared(root),
            }
        }

        fn root_path(&self) -> &Path {
            self.root.path()
        }

        fn preparer(&self, nix: PathBuf) -> NixPreparer {
            self.unrooted_preparer(nix)
                .with_roots(self.root_path().join("roots"))
        }

        fn unrooted_preparer(&self, nix: PathBuf) -> NixPreparer {
            NixPreparer::default()
                .with_executable(nix)
                .with_store_root(self.root_path().join("store"))
        }

        /// Returns an artifact manager building with `nix` into this fixture's cache and store,
        /// rooting what it builds in the fixture's root folder.
        fn manager(&self, nix: PathBuf) -> ArtifactManager {
            ArtifactManager::new(self.root_path().join("cache"), None)
                .expect("manager")
                .with_nix_sources(NixSources::default().with_roots(self.root_path().join("roots")))
                .with_nix_store(self.root_path().join("store"), nix)
                .with_nix_system(Some("aarch64-darwin"))
        }

        fn out_link(&self) -> PathBuf {
            self.root_path()
                .join("roots")
                .join(STORE_PATH.trim_start_matches("/nix/store/"))
        }

        /// Writes a fake `nix` that logs its arguments, finds `--out-link`, then runs `body`.
        fn nix(&self, body: &str) -> PathBuf {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = self.root_path().join(format!(
                "nix-{}",
                NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            ));
            let calls = self.root_path().join("calls");
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nlink=''\nprevious=''\nfor argument in \"$@\"; do\n  if [ \"$previous\" = '--out-link' ]; then link=\"$argument\"; fi\n  previous=\"$argument\"\ndone\n{body}\n",
                    calls.display()
                ),
            )
            .expect("fake nix");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("fake nix permissions");
            path
        }

        /// Writes `log` to a file and returns the shell command printing it to stderr.
        fn log(&self, log: &str) -> String {
            let path = self.root_path().join("nix-log");
            std::fs::write(&path, log).expect("fake nix log");
            format!("cat '{}' >&2", path.display())
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.root_path().join("calls"))
                .map(|calls| calls.lines().map(str::to_owned).collect())
                .unwrap_or_default()
        }
    }
}
