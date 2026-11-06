//! Downloads and verifies the immutable model artifacts that a resolved
//! model launch requires, materializing them into a shared local cache and
//! reporting each artifact's local file path for model startup.
//!
//! Every cached artifact is verified against its declared digest before first use.
//! Durable verification evidence then confirms the same file identities and exact
//! snapshot tree on warm reuse; any identity or tree change falls back to digest
//! verification and repair before the artifact is served. Cancellation stops all
//! downloads begin.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{BufReader, Read, Write},
    os::unix::ffi::OsStrExt,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::anyhow;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use fs4::fs_std::FileExt;
use futures_util::{StreamExt, stream::FuturesUnordered};
use hf_hub::{
    HFClient, HFError,
    progress::{DownloadEvent, Progress, ProgressEvent, ProgressHandler},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::{
    output::{OutputStream, StepOutput},
    progress::{
        ModelArtifactProgressObserver, ModelArtifactProgressSlot, ModelArtifactProgressTracker,
        ModelArtifactStage, ModelArtifactStageObserver,
    },
};

const HUGGING_FACE_ENDPOINT: &str = "https://huggingface.co";
const VERIFICATION_MARKER_VERSION: u8 = 1;
static MARKER_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// Limited private Hugging Face cache coordinator for known immutable catalog model artifacts.
#[derive(Clone)]
pub struct ModelArtifactCache {
    root: PathBuf,
    client: HFClient,
    progress: Option<Progress>,
    failed_digests: Arc<Mutex<BTreeSet<String>>>,
    digest_count: Option<Arc<AtomicUsize>>,
    lock_attempted: Option<Arc<tokio::sync::Notify>>,
    blocking_work: Option<Arc<BlockingWorkGate>>,
    branch_failure: Option<Arc<BranchFailure>>,
    pointer_rewrite: Option<Arc<PointerRewriteGate>>,
    refuse_hard_links: bool,
}

impl ModelArtifactCache {
    /// Creates the cache over one writable, non-root Hugging Face cache directory.
    pub fn new(root: PathBuf) -> Result<Self, ArtifactCacheError> {
        let client = HFClient::builder()
            .cache_dir(&root)
            .endpoint(HUGGING_FACE_ENDPOINT)
            .build();
        Self::with_client(root, client, None, None)
    }

    /// Returns the cache root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Materializes every immutable artifact selected by one model profile.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn prepare(
        &self,
        artifacts: &BTreeMap<String, ModelArtifactSource>,
    ) -> Result<ModelArtifactPreparation, ArtifactCacheError> {
        self.prepare_artifacts_cancellable(
            artifacts,
            &CancellationToken::new(),
            None,
            None,
            &StepOutput::discard(),
        )
        .await
    }

    /// Materializes one launch's artifacts while cooperatively stopping all Dotcore-owned work.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn prepare_cancellable(
        &self,
        artifacts: &BTreeMap<String, ModelArtifactSource>,
        cancellation: &CancellationToken,
        progress: Option<ModelArtifactProgressObserver>,
    ) -> Result<ModelArtifactPreparation, ArtifactCacheError> {
        self.prepare_artifacts_cancellable(
            artifacts,
            cancellation,
            progress,
            None,
            &StepOutput::discard(),
        )
        .await
    }

    /// Materializes concrete selected artifacts while cooperatively stopping owned work, reporting
    /// each download to `output`.
    pub(crate) async fn prepare_artifacts_cancellable(
        &self,
        artifacts: &BTreeMap<String, ModelArtifactSource>,
        cancellation: &CancellationToken,
        progress: Option<ModelArtifactProgressObserver>,
        stage: Option<ModelArtifactStageObserver>,
        output: &StepOutput,
    ) -> Result<ModelArtifactPreparation, ArtifactCacheError> {
        check_cancellation(cancellation)?;
        let invocation = cancellation.child_token();
        let tracker =
            progress.map(|observer| ModelArtifactProgressTracker::new(artifacts.len(), observer));
        let mut pending = FuturesUnordered::new();
        for (index, (binding_id, artifact)) in artifacts.iter().enumerate() {
            let binding_id = binding_id.clone();
            let invocation = invocation.clone();
            let progress = tracker.as_ref().map(|tracker| tracker.slot(index));
            let stage = stage.as_ref();
            pending.push(async move {
                (
                    binding_id,
                    self.materialize_invocation(artifact, &invocation, progress, stage, output)
                        .await,
                )
            });
        }

        let mut artifacts = BTreeMap::new();
        let mut failures = Vec::new();
        let mut replaced = false;
        while let Some((binding_id, result)) = pending.next().await {
            match result {
                Ok(prepared) => {
                    replaced |= prepared.replaced;
                    artifacts.insert(binding_id, prepared.path);
                }
                Err(error) => failures.push((binding_id, error)),
            }
        }
        check_cancellation(cancellation)?;
        failures.sort_by(|left, right| left.0.cmp(&right.0));
        if let Some((_, error)) = failures
            .into_iter()
            .find(|(_, error)| !error.is_cancelled())
        {
            return Err(error);
        }

        Ok(ModelArtifactPreparation {
            artifacts: PreparedModelArtifacts { artifacts },
            replaced,
        })
    }

    async fn materialize_invocation(
        &self,
        artifact: &ModelArtifactSource,
        invocation: &CancellationToken,
        progress: Option<ModelArtifactProgressSlot>,
        stage: Option<&ModelArtifactStageObserver>,
        output: &StepOutput,
    ) -> Result<MaterializedArtifact, ArtifactCacheError> {
        let result = self
            .materialize_cancellable(
                artifact,
                invocation,
                stage,
                DownloadReports {
                    progress: progress.as_ref(),
                    output,
                },
            )
            .await;
        match &result {
            Ok(_) => {
                if let Some(progress) = &progress {
                    progress.cache_satisfied();
                }
            }
            Err(_) => {
                if let Some(progress) = &progress {
                    progress.failed();
                }
            }
        }
        if result.as_ref().is_err_and(|error| !error.is_cancelled()) {
            invocation.cancel();
        }
        result
    }

    /// Returns stable Hugging Face snapshot paths for one launch's artifacts.
    #[cfg(any(test, feature = "test-support"))]
    pub fn expected_paths(
        &self,
        artifacts: &BTreeMap<String, ModelArtifactSource>,
    ) -> Result<PreparedModelArtifacts, ArtifactCacheError> {
        self.expected_artifact_paths(artifacts)
    }

    /// Returns stable cache paths for concrete selected artifacts.
    pub fn expected_artifact_paths(
        &self,
        selected: &BTreeMap<String, ModelArtifactSource>,
    ) -> Result<PreparedModelArtifacts, ArtifactCacheError> {
        let artifacts = selected
            .iter()
            .map(|(binding_id, artifact)| {
                self.snapshot_path(artifact)
                    .map(|path| (binding_id.clone(), path))
            })
            .collect::<Result<_, _>>()?;

        Ok(PreparedModelArtifacts { artifacts })
    }

    #[cfg(test)]
    async fn materialize(
        &self,
        artifact: &ModelArtifactSource,
    ) -> Result<MaterializedArtifact, ArtifactCacheError> {
        self.materialize_cancellable(
            artifact,
            &CancellationToken::new(),
            None,
            DownloadReports {
                progress: None,
                output: &StepOutput::discard(),
            },
        )
        .await
    }

    async fn materialize_cancellable(
        &self,
        artifact: &ModelArtifactSource,
        cancellation: &CancellationToken,
        stage: Option<&ModelArtifactStageObserver>,
        reports: DownloadReports<'_>,
    ) -> Result<MaterializedArtifact, ArtifactCacheError> {
        if matches!(artifact, ModelArtifactSource::HuggingFaceSnapshot { .. }) {
            return self
                .materialize_snapshot_cancellable(artifact, cancellation, stage, reports)
                .await;
        }
        check_cancellation(cancellation)?;
        let coordinate = ArtifactCacheError::artifact_coordinate(artifact)?;
        if let Some(branch_failure) = &self.branch_failure {
            branch_failure.fail_once(artifact).await?;
        }
        let failed_digest = ArtifactCacheError::failed_digest_key(artifact)?;
        let lock = acquire_lock(
            self.lock_path(&coordinate),
            cancellation,
            self.lock_attempted.as_deref(),
        )
        .await?;
        check_cancellation(cancellation)?;
        if self.is_digest_quarantined(&failed_digest) {
            return Err(ArtifactCacheError::DeclaredDigestMismatch);
        }
        if let Some(path) = self.cached_path(artifact, cancellation).await? {
            let validated = self
                .validate_or_repair_cached_artifact(artifact, path, false, cancellation, reports)
                .await;
            return self.finish_materialization(&failed_digest, validated, cancellation);
        }

        // The Hub request may take unbounded time, so contenders must not retain the coordinate lock.
        drop(lock);

        self.fetch_hugging_face_artifact(
            artifact,
            false,
            cancellation,
            self.download(artifact, false, cancellation, reports),
        )
        .await?;

        let lock = acquire_lock(
            self.lock_path(&coordinate),
            cancellation,
            self.lock_attempted.as_deref(),
        )
        .await?;
        check_cancellation(cancellation)?;
        if self.is_digest_quarantined(&failed_digest) {
            return Err(ArtifactCacheError::DeclaredDigestMismatch);
        }
        let path = self
            .required_cached_path(artifact, "requested", cancellation)
            .await?;
        let validated = self
            .validate_or_repair_cached_artifact(artifact, path, true, cancellation, reports)
            .await;
        let result = self.finish_materialization(&failed_digest, validated, cancellation);
        drop(lock);
        result
    }

    async fn materialize_snapshot_cancellable(
        &self,
        artifact: &ModelArtifactSource,
        cancellation: &CancellationToken,
        stage: Option<&ModelArtifactStageObserver>,
        reports: DownloadReports<'_>,
    ) -> Result<MaterializedArtifact, ArtifactCacheError> {
        check_cancellation(cancellation)?;
        let coordinate = ArtifactCacheError::artifact_coordinate(artifact)?;
        let publication = self.snapshot_path(artifact)?;
        let lock_path = self.lock_path(&coordinate);
        let state_path = lock_path.with_extension("repair.json");
        let lock = acquire_lock(
            lock_path.clone(),
            cancellation,
            self.lock_attempted.as_deref(),
        )
        .await?;
        check_cancellation(cancellation)?;
        let prior_sources = self
            .snapshot_repair_gate(artifact, &publication, &state_path, cancellation)
            .await?;
        if !fs::symlink_metadata(&publication)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            && let Some(stage) = stage
        {
            stage(ModelArtifactStage::Verify);
        }
        if self
            .snapshot_publication_is_valid(artifact, &publication, cancellation)
            .await?
        {
            self.clear_snapshot_repair_failure(&state_path)?;
            if let Some(stage) = stage {
                stage(ModelArtifactStage::Reuse);
            }
            return Ok(MaterializedArtifact {
                path: publication,
                replaced: false,
            });
        }
        let initially_absent = fs::symlink_metadata(&publication)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        let mut sources = match prior_sources {
            Some(sources) => sources,
            None => self.cached_snapshot_sources(artifact, cancellation).await?,
        };
        drop(lock);
        let mut reserved_remote = false;
        if initially_absent {
            let (repository, revision, files) = ArtifactCacheError::snapshot_parts(artifact)?;
            let missing = files
                .keys()
                .filter(|path| !sources.contains_key(*path))
                .cloned()
                .collect::<Vec<_>>();
            for (index, path) in missing.iter().enumerate() {
                let lock = acquire_lock(
                    lock_path.clone(),
                    cancellation,
                    self.lock_attempted.as_deref(),
                )
                .await?;
                check_cancellation(cancellation)?;
                if !fs::symlink_metadata(&publication)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                {
                    sources = self.cached_snapshot_sources(artifact, cancellation).await?;
                    drop(lock);
                    break;
                }
                drop(lock);
                if let Some(progress) = reports.progress {
                    progress.file_started((missing.len() - index - 1) as u64);
                }
                let _file_lock = self
                    .lock_hugging_face_file(repository, revision, path, cancellation)
                    .await?;
                let source = self
                    .fetch_hugging_face_file(
                        repository,
                        revision,
                        path,
                        false,
                        cancellation,
                        self.download_hugging_face_file(
                            repository,
                            revision,
                            path,
                            false,
                            cancellation,
                            reports,
                        ),
                    )
                    .await?
                    .ok_or_else(|| {
                        ArtifactCacheError::message(
                            "Hugging Face cache did not publish the requested snapshot file",
                        )
                    })?;
                sources.insert(path.clone(), source);
            }
        }
        let mut attempted_remote = BTreeSet::new();

        loop {
            let lock = acquire_lock(
                lock_path.clone(),
                cancellation,
                self.lock_attempted.as_deref(),
            )
            .await?;
            check_cancellation(cancellation)?;
            if reserved_remote {
                self.clear_snapshot_repair_failure(&state_path)?;
            }
            if let Some(current_sources) = self
                .snapshot_repair_gate(artifact, &publication, &state_path, cancellation)
                .await?
            {
                sources = current_sources;
            }
            if !fs::symlink_metadata(&publication)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                && let Some(stage) = stage
            {
                stage(ModelArtifactStage::Verify);
            }
            if self
                .snapshot_publication_is_valid(artifact, &publication, cancellation)
                .await?
            {
                self.clear_snapshot_repair_failure(&state_path)?;
                if let Some(stage) = stage {
                    stage(ModelArtifactStage::Reuse);
                }
                return Ok(MaterializedArtifact {
                    path: publication,
                    replaced: false,
                });
            }
            let published = self
                .publish_snapshot(artifact, &publication, sources.clone(), cancellation, stage)
                .await
                .map(|replaced| MaterializedArtifact {
                    path: publication.clone(),
                    replaced,
                });
            match published {
                Err(ArtifactCacheError::SnapshotSourceUnavailable { path })
                    if attempted_remote.insert(path.clone()) =>
                {
                    let (repository, revision, _) = ArtifactCacheError::snapshot_parts(artifact)?;
                    let force = sources.contains_key(&path);
                    let publication_absent = fs::symlink_metadata(&publication)
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
                    if (!publication_absent || force) && !reserved_remote {
                        if let Err(error) = self.reserve_snapshot_remote_repair(&state_path) {
                            self.record_snapshot_repair_failure(
                                &coordinate,
                                &publication,
                                &sources,
                                &state_path,
                                SnapshotRepairCause::IncompleteRemote,
                            )?;
                            return Err(error);
                        }
                        reserved_remote = true;
                    }
                    if reserved_remote {
                        self.record_snapshot_repair_failure(
                            &coordinate,
                            &publication,
                            &sources,
                            &state_path,
                            SnapshotRepairCause::IncompleteRemote,
                        )?;
                    }
                    // Taking the file lock before releasing the coordinate lock makes a concurrent
                    // writer's source probe wait for this fetch, so it never reads the recorded
                    // failure of a repair that is still in progress as a finished one.
                    let _file_lock = self
                        .lock_hugging_face_file(repository, revision, &path, cancellation)
                        .await?;
                    drop(lock);
                    if let Some(progress) = reports.progress {
                        progress.file_started(0);
                    }
                    let source = self
                        .fetch_hugging_face_file(
                            repository,
                            revision,
                            &path,
                            force,
                            cancellation,
                            self.download_hugging_face_file(
                                repository,
                                revision,
                                &path,
                                force,
                                cancellation,
                                reports,
                            ),
                        )
                        .await?
                        .ok_or_else(|| {
                            ArtifactCacheError::message(
                                "Hugging Face cache did not publish the requested snapshot file",
                            )
                        })?;
                    sources.insert(path, source);
                }
                Err(ArtifactCacheError::SnapshotSourceUnavailable { .. }) => {
                    self.record_snapshot_repair_failure(
                        &coordinate,
                        &publication,
                        &sources,
                        &state_path,
                        SnapshotRepairCause::DigestMismatch,
                    )?;
                    return Err(ArtifactCacheError::DeclaredDigestMismatch);
                }
                Ok(materialized) => {
                    self.clear_snapshot_repair_failure(&state_path)?;
                    return Ok(materialized);
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn cached_snapshot_sources(
        &self,
        artifact: &ModelArtifactSource,
        cancellation: &CancellationToken,
    ) -> Result<BTreeMap<String, PathBuf>, ArtifactCacheError> {
        let (repository, revision, files) = ArtifactCacheError::snapshot_parts(artifact)?;
        let mut sources = BTreeMap::new();
        for path in files.keys() {
            if let Some(source) = self
                .cached_hugging_face_file(repository, revision, path, cancellation)
                .await?
            {
                sources.insert(path.clone(), source);
            }
        }
        Ok(sources)
    }

    async fn snapshot_repair_gate(
        &self,
        artifact: &ModelArtifactSource,
        publication: &Path,
        state_path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Option<BTreeMap<String, PathBuf>>, ArtifactCacheError> {
        let state = snapshot_repair_state(state_path)?;
        let Some(retry_after_seconds) = state.retry_after_seconds(snapshot_repair_now()?) else {
            return Ok(None);
        };
        let Some(negative) = state.negative_signature else {
            return Ok(None);
        };
        let sources = self.cached_snapshot_sources(artifact, cancellation).await?;
        let coordinate = ArtifactCacheError::artifact_coordinate(artifact)?;
        let marker = self.verification_marker_path(&coordinate);
        if snapshot_repair_signature(publication, &marker, &sources)? == negative {
            return Err(ArtifactCacheError::SnapshotRepairDeferred {
                retry_after_seconds,
                cause: state
                    .negative_cause
                    .unwrap_or(SnapshotRepairCause::IncompleteRemote)
                    .message(),
            });
        }
        Ok(Some(sources))
    }

    fn clear_snapshot_repair_failure(&self, state_path: &Path) -> Result<(), ArtifactCacheError> {
        let mut state = snapshot_repair_state(state_path)?;
        if state.negative_signature.take().is_some() {
            state.negative_cause = None;
            write_verification_evidence(state_path, &state)?;
        }
        Ok(())
    }

    fn reserve_snapshot_remote_repair(&self, state_path: &Path) -> Result<(), ArtifactCacheError> {
        let mut state = snapshot_repair_state(state_path)?;
        let now = snapshot_repair_now()?;
        if let Some(retry_after_seconds) = state.retry_after_seconds(now) {
            return Err(ArtifactCacheError::SnapshotRepairDeferred {
                retry_after_seconds,
                cause: state
                    .negative_cause
                    .unwrap_or(SnapshotRepairCause::IncompleteRemote)
                    .message(),
            });
        }
        state.spent_at_seconds = Some(now);
        state.negative_signature = None;
        state.negative_cause = None;
        write_verification_evidence(state_path, &state)
    }

    fn record_snapshot_repair_failure(
        &self,
        coordinate: &str,
        publication: &Path,
        sources: &BTreeMap<String, PathBuf>,
        state_path: &Path,
        cause: SnapshotRepairCause,
    ) -> Result<(), ArtifactCacheError> {
        let mut state = snapshot_repair_state(state_path)?;
        let marker = self.verification_marker_path(coordinate);
        state.negative_signature = Some(snapshot_repair_signature(publication, &marker, sources)?);
        state.negative_cause = Some(cause);
        write_verification_evidence(state_path, &state)
    }

    /// Probes the local Hub cache for one file while holding that file's lock, so the probe never
    /// observes a concurrent download's pointer rewrite half done.
    async fn cached_hugging_face_file(
        &self,
        repository: &str,
        revision: &str,
        path: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<PathBuf>, ArtifactCacheError> {
        let _file_lock = acquire_lock(
            self.hub_file_lock_path(repository, revision, path),
            cancellation,
            self.pointer_rewrite
                .as_deref()
                .map(|gate| &gate.probe_contended),
        )
        .await?;
        self.probe_hugging_face_file(repository, revision, path, cancellation)
            .await
    }

    /// Takes the lock of one Hub file, which serializes its downloads and cache probes.
    async fn lock_hugging_face_file(
        &self,
        repository: &str,
        revision: &str,
        path: &str,
        cancellation: &CancellationToken,
    ) -> Result<File, ArtifactCacheError> {
        acquire_lock(
            self.hub_file_lock_path(repository, revision, path),
            cancellation,
            None,
        )
        .await
    }

    /// Downloads one Hub file unless it is already cached, and returns its canonical cached
    /// path. Callers hold the file's lock across the call, so it spans the download and the
    /// probe that follows it: concurrent callers download the file once and never read its
    /// pointer while the Hub client rewrites it.
    async fn fetch_hugging_face_file(
        &self,
        repository: &str,
        revision: &str,
        path: &str,
        force: bool,
        cancellation: &CancellationToken,
        download: impl Future<Output = Result<(), ArtifactCacheError>>,
    ) -> Result<Option<PathBuf>, ArtifactCacheError> {
        if !force
            && let Some(cached) = self
                .probe_hugging_face_file(repository, revision, path, cancellation)
                .await?
        {
            tracing::debug!(
                event_kind = "hugging_face_file_download_skipped",
                repository,
                revision,
                path,
                "Hugging Face file was cached by a concurrent download"
            );
            return Ok(Some(cached));
        }
        download.await?;
        if let Some(gate) = &self.pointer_rewrite {
            gate.replay(&self.hub_pointer_path(repository, revision, path))
                .await?;
        }
        self.probe_hugging_face_file(repository, revision, path, cancellation)
            .await
    }

    /// Locked download for a single-file artifact; the caller re-probes under its coordinate lock.
    async fn fetch_hugging_face_artifact(
        &self,
        artifact: &ModelArtifactSource,
        force: bool,
        cancellation: &CancellationToken,
        download: impl Future<Output = Result<(), ArtifactCacheError>>,
    ) -> Result<(), ArtifactCacheError> {
        let (repository, revision, file, _) = ArtifactCacheError::artifact_parts(artifact)?;
        let _file_lock = self
            .lock_hugging_face_file(repository, revision, file, cancellation)
            .await?;
        self.fetch_hugging_face_file(repository, revision, file, force, cancellation, download)
            .await
            .map(|_| ())
    }

    /// Resolves one file in the local Hub cache without taking its lock; callers hold it.
    async fn probe_hugging_face_file(
        &self,
        repository: &str,
        revision: &str,
        path: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<PathBuf>, ArtifactCacheError> {
        check_cancellation(cancellation)?;
        let (owner, name) = repository
            .split_once('/')
            .expect("validated Hugging Face repository has an owner and name");
        let model = self.client.model(owner, name);
        let request = model
            .download_file()
            .filename(path)
            .revision(revision)
            .local_files_only(true)
            .send();
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ArtifactCacheError::Cancelled),
            result = request => result,
        };
        match result {
            Ok(path) => canonical_regular_path(&self.root, &path, cancellation)
                .await
                .map(Some),
            Err(HFError::LocalEntryNotFound { .. }) => Ok(None),
            Err(error) => Err(anyhow::Error::new(error)
                .context("Hugging Face cache probe failed")
                .into()),
        }
    }

    /// Sends one snapshot file's Hub download; callers hold the file's lock.
    async fn download_hugging_face_file(
        &self,
        repository: &str,
        revision: &str,
        path: &str,
        force: bool,
        cancellation: &CancellationToken,
        reports: DownloadReports<'_>,
    ) -> Result<(), ArtifactCacheError> {
        check_cancellation(cancellation)?;
        let (owner, name) = repository
            .split_once('/')
            .expect("validated Hugging Face repository has an owner and name");
        let model = self.client.model(owner, name);
        let request = model
            .download_file()
            .filename(path)
            .revision(revision)
            .force_download(force)
            .maybe_progress(self.download_progress(reports, path))
            .send();
        reports.output.line(
            OutputStream::Stdout,
            format!("downloading {path} from {repository}"),
        );
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(ArtifactCacheError::Cancelled),
            result = request => result
                .map(|_| ())
                .map_err(|error| {
                    anyhow::Error::new(error)
                        .context(format!(
                            "Hugging Face snapshot file download failed for {repository}@{revision}",
                        ))
                        .into()
                }),
        };
        if result.is_ok() {
            reports
                .output
                .line(OutputStream::Stdout, format!("downloaded {path}"));
        }
        result
    }

    async fn snapshot_publication_is_valid(
        &self,
        artifact: &ModelArtifactSource,
        publication: &Path,
        cancellation: &CancellationToken,
    ) -> Result<bool, ArtifactCacheError> {
        check_cancellation(cancellation)?;
        let started = Instant::now();
        let (repository, revision, files) = ArtifactCacheError::snapshot_parts(artifact)?;
        let coordinate = ArtifactCacheError::artifact_coordinate(artifact)?;
        let marker_path = self.verification_marker_path(&coordinate);
        let file_count = files.len();
        let repository = repository.to_owned();
        let revision = revision.to_owned();
        let publication = publication.to_owned();
        let files = files.clone();
        let digest_count = self.digest_count.clone();
        let task_cancellation = cancellation.clone();
        let validation = tokio::task::spawn_blocking(move || {
            validate_snapshot_tree(
                &publication,
                &repository,
                &revision,
                &files,
                &marker_path,
                digest_count.as_deref(),
                &task_cancellation,
            )
        })
        .await
        .map_err(|error| ArtifactCacheError::context("snapshot validation task failed", error))??;
        check_cancellation(cancellation)?;
        let (valid, validation_mode) = match validation {
            SnapshotTreeValidation::Invalid => (false, "invalid"),
            SnapshotTreeValidation::VerifiedByIdentity => (true, "identity"),
            SnapshotTreeValidation::VerifiedByDigest(marker) => {
                let coordinate = ArtifactCacheError::artifact_coordinate(artifact)?;
                let marker_path = self.verification_marker_path(&coordinate);
                self.persist_verification_marker(&marker_path, &marker, cancellation)
                    .await?;
                (true, "digest")
            }
        };
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::debug!(
            event_kind = "snapshot_publication_validated",
            outcome = if valid { "valid" } else { "invalid" },
            validation_mode,
            file_count,
            duration_ms,
            "validated snapshot publication"
        );
        Ok(valid)
    }

    async fn publish_snapshot(
        &self,
        artifact: &ModelArtifactSource,
        publication: &Path,
        sources: BTreeMap<String, PathBuf>,
        cancellation: &CancellationToken,
        stage: Option<&ModelArtifactStageObserver>,
    ) -> Result<bool, ArtifactCacheError> {
        check_cancellation(cancellation)?;
        let (repository, revision, files) = ArtifactCacheError::snapshot_parts(artifact)?;
        let coordinate = ArtifactCacheError::artifact_coordinate(artifact)?;
        let marker_path = self.verification_marker_path(&coordinate);
        let repository = repository.to_owned();
        let revision = revision.to_owned();
        let publication = publication.to_owned();
        let files = files.clone();
        let digest_count = self.digest_count.clone();
        let task_cancellation = cancellation.clone();
        let blocking_work = self.blocking_work.clone();
        let refuse_hard_links = self.refuse_hard_links;
        if let Some(stage) = stage {
            stage(ModelArtifactStage::Verify);
        }
        let stage = stage.cloned();
        let published = tokio::task::spawn_blocking(move || {
            publish_snapshot_tree(
                SnapshotPublication {
                    path: &publication,
                    repository: &repository,
                    revision: &revision,
                    sources: &sources,
                    files: &files,
                    refuse_hard_links,
                },
                digest_count.as_deref(),
                blocking_work.as_deref(),
                &task_cancellation,
                stage.as_ref(),
            )
        })
        .await
        .map_err(|error| {
            ArtifactCacheError::context("snapshot publication task failed", error)
        })??;
        check_cancellation(cancellation)?;
        self.persist_verification_marker(&marker_path, &published.marker, cancellation)
            .await?;
        tracing::info!(
            event_kind = "snapshot_published",
            repository = %published.marker.repository,
            revision = %published.marker.revision,
            linked_files = published.placements.linked,
            copied_files = published.placements.copied,
            retained_files = published.placements.retained,
            "published snapshot"
        );
        Ok(published.replaced)
    }

    fn finish_materialization(
        &self,
        failed_digest: &str,
        result: Result<MaterializedArtifact, ArtifactCacheError>,
        cancellation: &CancellationToken,
    ) -> Result<MaterializedArtifact, ArtifactCacheError> {
        check_cancellation(cancellation)?;
        match result {
            Ok(materialized) => Ok(materialized),
            Err(ArtifactCacheError::DeclaredDigestMismatch) => {
                // The coordinate lock serializes repair, so a failed digest is quarantined once.
                self.failed_digests
                    .lock()
                    .unwrap_or_else(|value| value.into_inner())
                    .insert(failed_digest.to_owned());
                check_cancellation(cancellation)?;
                Err(ArtifactCacheError::DeclaredDigestMismatch)
            }
            Err(error) => Err(error),
        }
    }

    fn is_digest_quarantined(&self, failed_digest: &str) -> bool {
        self.failed_digests
            .lock()
            .unwrap_or_else(|value| value.into_inner())
            .contains(failed_digest)
    }

    async fn validate_or_repair_cached_artifact(
        &self,
        artifact: &ModelArtifactSource,
        path: PathBuf,
        replaced: bool,
        cancellation: &CancellationToken,
        reports: DownloadReports<'_>,
    ) -> Result<MaterializedArtifact, ArtifactCacheError> {
        check_cancellation(cancellation)?;
        if replaced {
            self.remove_verification_marker(artifact, cancellation)
                .await?;
        }
        if self
            .declared_digest_is_valid(artifact, &path, cancellation)
            .await?
        {
            check_cancellation(cancellation)?;
            return Ok(MaterializedArtifact { path, replaced });
        }

        self.repair_cached_artifact(artifact, cancellation, reports)
            .await
    }

    async fn repair_cached_artifact(
        &self,
        artifact: &ModelArtifactSource,
        cancellation: &CancellationToken,
        reports: DownloadReports<'_>,
    ) -> Result<MaterializedArtifact, ArtifactCacheError> {
        self.remove_verification_marker(artifact, cancellation)
            .await?;
        self.fetch_hugging_face_artifact(
            artifact,
            true,
            cancellation,
            self.download(artifact, true, cancellation, reports),
        )
        .await?;
        let repaired = self
            .required_cached_path(artifact, "repaired", cancellation)
            .await?;
        if !self
            .declared_digest_is_valid(artifact, &repaired, cancellation)
            .await?
        {
            return Err(ArtifactCacheError::DeclaredDigestMismatch);
        }
        Ok(MaterializedArtifact {
            path: repaired,
            replaced: true,
        })
    }

    async fn required_cached_path(
        &self,
        artifact: &ModelArtifactSource,
        state: &str,
        cancellation: &CancellationToken,
    ) -> Result<PathBuf, ArtifactCacheError> {
        self.cached_path(artifact, cancellation)
            .await?
            .ok_or_else(|| {
                ArtifactCacheError::message(format!(
                    "Hugging Face cache did not publish the {state} artifact"
                ))
            })
    }

    async fn cached_path(
        &self,
        artifact: &ModelArtifactSource,
        cancellation: &CancellationToken,
    ) -> Result<Option<PathBuf>, ArtifactCacheError> {
        check_cancellation(cancellation)?;
        let (repository, revision, file, _) = ArtifactCacheError::artifact_parts(artifact)?;
        self.cached_hugging_face_file(repository, revision, file, cancellation)
            .await
    }

    /// Sends one single-file artifact's Hub download; callers hold the file's lock.
    async fn download(
        &self,
        artifact: &ModelArtifactSource,
        force: bool,
        cancellation: &CancellationToken,
        reports: DownloadReports<'_>,
    ) -> Result<(), ArtifactCacheError> {
        check_cancellation(cancellation)?;
        let (repository, revision, file, _) = ArtifactCacheError::artifact_parts(artifact)?;
        let (owner, name) = repository
            .split_once('/')
            .expect("validated Hugging Face repository has an owner and name");
        let model = self.client.model(owner, name);
        let request = model
            .download_file()
            .filename(file)
            .revision(revision)
            .force_download(force)
            .maybe_progress(self.download_progress(reports, file))
            .send();
        reports.output.line(
            OutputStream::Stdout,
            format!("downloading {file} from {repository}"),
        );
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(ArtifactCacheError::Cancelled),
            result = request => result
            .map(|_| ())
            .map_err(|error| {
                anyhow::Error::new(error)
                    .context(format!(
                        "Hugging Face artifact download failed for {repository}@{revision}"
                    ))
                    .into()
            }),
        };
        if result.is_ok() {
            reports
                .output
                .line(OutputStream::Stdout, format!("downloaded {file}"));
        }
        result
    }

    /// Returns the progress handler of one file's download, which feeds the transfer progress
    /// and reports throttled progress lines to the step output.
    fn download_progress(&self, reports: DownloadReports<'_>, path: &str) -> Option<Progress> {
        if reports.progress.is_none() && !reports.output.is_observed() {
            return self.progress.clone();
        }
        Some(Progress::new(HuggingFaceDownloadProgress {
            slot: reports.progress.cloned(),
            output: reports.output.clone(),
            path: path.to_owned(),
            total: AtomicU64::new(0),
            reported_tenths: AtomicU64::new(0),
        }))
    }

    fn snapshot_path(&self, artifact: &ModelArtifactSource) -> Result<PathBuf, ArtifactCacheError> {
        match artifact {
            ModelArtifactSource::HuggingFaceFile {
                repository,
                revision,
                path,
                ..
            } => Ok(self
                .root
                .join(format!("models--{}", repository.replace('/', "--")))
                .join("snapshots")
                .join(revision)
                .join(path)),
            ModelArtifactSource::HuggingFaceSnapshot { .. } => {
                let identity = sha256_text(&ArtifactCacheError::artifact_coordinate(artifact)?);
                Ok(self.root.join(".dotcore-snapshots").join(identity))
            }
        }
    }

    fn lock_path(&self, coordinate: &str) -> PathBuf {
        self.root
            .join(".dotcore-coordinate-locks")
            .join(format!("{}.lock", sha256_text(coordinate)))
    }

    /// Lock file serializing downloads and probes of one Hub file; distinct from coordinate locks.
    fn hub_file_lock_path(&self, repository: &str, revision: &str, path: &str) -> PathBuf {
        self.root.join(".dotcore-coordinate-locks").join(format!(
            "{}.file.lock",
            sha256_text(&format!("hub-file\n{repository}\n{revision}\n{path}"))
        ))
    }

    /// Snapshot pointer the Hub client publishes for one file of a commit revision.
    fn hub_pointer_path(&self, repository: &str, revision: &str, path: &str) -> PathBuf {
        self.root
            .join(format!("models--{}", repository.replace('/', "--")))
            .join("snapshots")
            .join(revision)
            .join(path)
    }

    fn verification_marker_path(&self, coordinate: &str) -> PathBuf {
        self.root
            .join(".dotcore-coordinate-locks")
            .join(format!("{}.verification.json", sha256_text(coordinate)))
    }

    async fn declared_digest_is_valid(
        &self,
        artifact: &ModelArtifactSource,
        path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<bool, ArtifactCacheError> {
        check_cancellation(cancellation)?;
        let (repository, revision, file, declared_digest) =
            ArtifactCacheError::artifact_parts(artifact)?;
        if declared_digest.is_empty() {
            return Ok(true);
        }
        let coordinate = ArtifactCacheError::artifact_coordinate(artifact)?;
        let marker_path = self.verification_marker_path(&coordinate);
        let marker = VerificationMarker::new(
            repository,
            revision,
            file,
            declared_digest,
            path,
            FileIdentity::from_path(path)?,
        );
        if self
            .verification_marker_matches(&marker_path, &marker, cancellation)
            .await?
        {
            return Ok(true);
        }

        let verified_identity = self
            .verify_file_digest(path, declared_digest, cancellation)
            .await?;
        let Some(identity) = verified_identity else {
            return Ok(false);
        };
        let marker =
            VerificationMarker::new(repository, revision, file, declared_digest, path, identity);
        check_cancellation(cancellation)?;
        self.persist_verification_marker(&marker_path, &marker, cancellation)
            .await?;
        check_cancellation(cancellation)?;
        Ok(true)
    }

    async fn verification_marker_matches(
        &self,
        path: &Path,
        marker: &VerificationMarker,
        cancellation: &CancellationToken,
    ) -> Result<bool, ArtifactCacheError> {
        check_cancellation(cancellation)?;
        let path = path.to_owned();
        let marker = marker.clone();
        let result = tokio::task::spawn_blocking(move || marker.matches_path(&path))
            .await
            .map_err(|error| {
                ArtifactCacheError::context("artifact verification marker task failed", error)
            })?;
        check_cancellation(cancellation)?;
        result
    }

    async fn verify_file_digest(
        &self,
        path: &Path,
        declared_digest: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<FileIdentity>, ArtifactCacheError> {
        let path = path.to_owned();
        let declared_digest = declared_digest.to_owned();
        let digest_count = self.digest_count.clone();
        let blocking_work = self.blocking_work.clone();
        let cancellation = cancellation.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(blocking_work) = blocking_work {
                blocking_work.wait();
            }
            FileIdentity::verify_digest(
                &path,
                &declared_digest,
                digest_count.as_deref(),
                &cancellation,
            )
        })
        .await
        .map_err(|error| ArtifactCacheError::context("artifact digest task failed", error))?
    }

    async fn persist_verification_marker<T>(
        &self,
        path: &Path,
        marker: &T,
        cancellation: &CancellationToken,
    ) -> Result<(), ArtifactCacheError>
    where
        T: Clone + Send + Serialize + 'static,
    {
        check_cancellation(cancellation)?;
        let path = path.to_owned();
        let marker = marker.clone();
        let result =
            tokio::task::spawn_blocking(move || write_verification_evidence(&path, &marker))
                .await
                .map_err(|error| {
                    ArtifactCacheError::context("artifact verification marker task failed", error)
                })?;
        check_cancellation(cancellation)?;
        result
    }

    async fn remove_verification_marker(
        &self,
        artifact: &ModelArtifactSource,
        cancellation: &CancellationToken,
    ) -> Result<(), ArtifactCacheError> {
        check_cancellation(cancellation)?;
        let coordinate = ArtifactCacheError::artifact_coordinate(artifact)?;
        let path = self.verification_marker_path(&coordinate);
        let result = tokio::task::spawn_blocking(move || match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ArtifactCacheError::context(
                "artifact verification marker removal failed",
                error,
            )),
        })
        .await
        .map_err(|error| {
            ArtifactCacheError::context("artifact verification marker task failed", error)
        })?;
        check_cancellation(cancellation)?;
        result
    }

    fn with_client(
        root: PathBuf,
        client: Result<HFClient, HFError>,
        progress: Option<Progress>,
        digest_count: Option<Arc<AtomicUsize>>,
    ) -> Result<Self, ArtifactCacheError> {
        if !root.is_absolute() || root.parent().is_none() {
            return Err(ArtifactCacheError::message(
                "Model artifact cache requires an absolute non-root directory",
            ));
        }
        Ok(Self {
            root,
            client: client.map_err(|error| {
                ArtifactCacheError::from(
                    anyhow::Error::new(error).context("Hugging Face client setup failed"),
                )
            })?,
            progress,
            failed_digests: Arc::new(Mutex::new(BTreeSet::new())),
            digest_count,
            lock_attempted: None,
            blocking_work: None,
            branch_failure: None,
            pointer_rewrite: None,
            refuse_hard_links: false,
        })
    }
}

/// Pinned Hugging Face source of one model or auxiliary artifact the cache materializes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelArtifactSource {
    /// One immutable regular file.
    HuggingFaceFile {
        repository: String,
        revision: String,
        path: String,
        provenance_digest: String,
    },

    /// One immutable self-contained tree keyed by safe relative path.
    HuggingFaceSnapshot {
        repository: String,
        revision: String,
        files: BTreeMap<String, String>,
    },
}

#[derive(Default)]
pub struct BlockingWorkGate {
    started: tokio::sync::Notify,
    has_started: AtomicBool,
    released: Mutex<bool>,
    release: Condvar,
}

impl BlockingWorkGate {
    fn wait(&self) {
        self.has_started.store(true, Ordering::Release);
        self.started.notify_waiters();
        let mut released = self
            .released
            .lock()
            .unwrap_or_else(|value| value.into_inner());
        while !*released {
            released = self
                .release
                .wait(released)
                .unwrap_or_else(|value| value.into_inner());
        }
    }

    pub async fn wait_started(&self) {
        loop {
            let started = self.started.notified();
            if self.has_started.load(Ordering::Acquire) {
                return;
            }
            started.await;
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn release(&self) {
        *self
            .released
            .lock()
            .unwrap_or_else(|value| value.into_inner()) = true;
        self.release.notify_all();
    }
}

/// Test seam that replays the Hub client's pointer rewrite after a download: the pointer is
/// removed and recreated, and the gap stays open until a cache probe contends for the file's
/// lock or a short deadline passes.
#[derive(Default)]
pub struct PointerRewriteGate {
    opened: tokio::sync::Notify,
    probe_contended: tokio::sync::Notify,
}

impl PointerRewriteGate {
    async fn replay(&self, pointer: &Path) -> Result<(), ArtifactCacheError> {
        let target = fs::read_link(pointer).map_err(|error| {
            ArtifactCacheError::context("test pointer rewrite read failed", error)
        })?;
        fs::remove_file(pointer).map_err(|error| {
            ArtifactCacheError::context("test pointer rewrite removal failed", error)
        })?;
        self.opened.notify_one();
        #[expect(
            clippy::disallowed_methods,
            reason = "bounds a test seam's wait for a real cache probe"
        )]
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.probe_contended.notified(),
        )
        .await;
        std::os::unix::fs::symlink(target, pointer).map_err(|error| {
            ArtifactCacheError::context("test pointer rewrite restore failed", error)
        })
    }
}

struct BranchFailure {
    file: String,
    blocking_work: Arc<BlockingWorkGate>,
    fired: AtomicBool,
    panic: bool,
}

impl BranchFailure {
    async fn fail_once(&self, artifact: &ModelArtifactSource) -> Result<(), ArtifactCacheError> {
        let (_, _, file, _) = ArtifactCacheError::artifact_parts(artifact)?;
        if file != self.file || self.fired.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.blocking_work.wait_started().await;
        assert!(!self.panic, "injected artifact preparation task panic");
        Err(ArtifactCacheError::message(
            "injected transient artifact branch failure",
        ))
    }
}

/// Local paths keyed by the logical binding IDs in one resolved launch plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedModelArtifacts {
    pub artifacts: BTreeMap<String, PathBuf>,
}

impl PreparedModelArtifacts {
    #[cfg(any(test, feature = "test-support"))]
    pub fn path(&self, binding_id: &str) -> &PathBuf {
        self.artifacts
            .get(binding_id)
            .unwrap_or_else(|| panic!("prepared artifact {binding_id:?}"))
    }
}

/// Prepared artifact paths and whether preparation replaced cached content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelArtifactPreparation {
    pub artifacts: PreparedModelArtifacts,
    pub replaced: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactCacheError {
    #[error("Model artifact preparation was cancelled")]
    Cancelled,

    #[error("Hugging Face cache repair did not satisfy the declared SHA-256 digest")]
    DeclaredDigestMismatch,

    #[error("Snapshot publication has an unsafe structural conflict: {kind}")]
    StructuralConflict { kind: &'static str },

    #[error("Snapshot repair has no verified local source for {path}")]
    SnapshotSourceUnavailable { path: String },

    #[error("Snapshot remote repair deferred ({cause}); retry in {retry_after_seconds} seconds")]
    SnapshotRepairDeferred {
        retry_after_seconds: u64,
        cause: &'static str,
    },

    #[error("Model artifact cache failure: {0}")]
    Other(#[from] anyhow::Error),
}

impl ArtifactCacheError {
    pub(crate) fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    fn message(message: impl Into<String>) -> Self {
        anyhow!(message.into()).into()
    }

    fn context<E>(context: impl Into<String>, error: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::from(anyhow::Error::new(error).context(context.into()))
    }

    fn failed_digest_key(artifact: &ModelArtifactSource) -> Result<String, Self> {
        let coordinate = Self::artifact_coordinate(artifact)?;
        match artifact {
            ModelArtifactSource::HuggingFaceFile {
                provenance_digest, ..
            } => Ok(format!("{coordinate}\n{provenance_digest}")),
            ModelArtifactSource::HuggingFaceSnapshot { .. } => Ok(coordinate),
        }
    }

    fn artifact_coordinate(artifact: &ModelArtifactSource) -> Result<String, Self> {
        match artifact {
            ModelArtifactSource::HuggingFaceFile {
                repository,
                revision,
                path,
                provenance_digest,
            } => {
                validate_hugging_face_coordinates(repository, revision)?;
                if !valid_repository_path(path) {
                    return Err(Self::message(
                        "Model artifact has an unsafe Hugging Face file path",
                    ));
                }
                if !provenance_digest.is_empty() && !valid_digest(provenance_digest) {
                    return Err(Self::message(
                        "Model artifact has an invalid declared SHA-256 digest",
                    ));
                }
                Ok(format!("file\n{repository}\n{revision}\n{path}"))
            }
            ModelArtifactSource::HuggingFaceSnapshot {
                repository,
                revision,
                files,
            } => {
                validate_hugging_face_coordinates(repository, revision)?;
                validate_snapshot_manifest(files)?;
                let mut coordinate = format!("snapshot\n{repository}\n{revision}\n");
                for (path, digest) in files {
                    coordinate.push_str(&path.len().to_string());
                    coordinate.push(':');
                    coordinate.push_str(path);
                    coordinate.push(':');
                    coordinate.push_str(digest);
                    coordinate.push('\n');
                }
                Ok(coordinate)
            }
        }
    }

    fn artifact_parts(artifact: &ModelArtifactSource) -> Result<(&str, &str, &str, &str), Self> {
        let ModelArtifactSource::HuggingFaceFile {
            repository,
            revision,
            path,
            provenance_digest,
        } = artifact
        else {
            return Err(Self::message("Model artifact is not a single file"));
        };
        Self::artifact_coordinate(artifact)?;
        Ok((repository, revision, path, provenance_digest))
    }

    fn snapshot_parts(
        artifact: &ModelArtifactSource,
    ) -> Result<(&str, &str, &BTreeMap<String, String>), Self> {
        let ModelArtifactSource::HuggingFaceSnapshot {
            repository,
            revision,
            files,
        } = artifact
        else {
            return Err(Self::message("Model artifact is not a snapshot"));
        };
        Self::artifact_coordinate(artifact)?;
        Ok((repository, revision, files))
    }
}

#[derive(Debug)]
struct MaterializedArtifact {
    path: PathBuf,
    replaced: bool,
}

struct PublishedSnapshot {
    replaced: bool,
    marker: SnapshotVerificationMarker,
    placements: SnapshotPlacements,
}

/// Counts how one publication placed each declared file.
#[derive(Default)]
struct SnapshotPlacements {
    linked: usize,
    copied: usize,
    retained: usize,
}

struct SnapshotPublication<'a> {
    path: &'a Path,
    repository: &'a str,
    revision: &'a str,
    sources: &'a BTreeMap<String, PathBuf>,
    files: &'a BTreeMap<String, String>,
    refuse_hard_links: bool,
}

/// Durable evidence that one immutable artifact passed digest verification.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct VerificationMarker {
    version: u8,
    repository: String,
    revision: String,
    file: String,
    declared_digest: String,
    canonical_path: String,
    identity: FileIdentity,
}

impl VerificationMarker {
    fn new(
        repository: &str,
        revision: &str,
        file: &str,
        declared_digest: &str,
        canonical_path: &Path,
        identity: FileIdentity,
    ) -> Self {
        Self {
            version: VERIFICATION_MARKER_VERSION,
            repository: repository.to_owned(),
            revision: revision.to_owned(),
            file: file.to_owned(),
            declared_digest: declared_digest.to_owned(),
            canonical_path: BASE64.encode(canonical_path.as_os_str().as_bytes()),
            identity,
        }
    }

    fn matches_path(&self, path: &Path) -> Result<bool, ArtifactCacheError> {
        verification_evidence_matches(path, self)
    }

    #[cfg(test)]
    fn write_to(&self, path: &Path) -> Result<(), ArtifactCacheError> {
        write_verification_evidence(path, self)
    }
}

/// Durable evidence that one exact snapshot publication passed manifest verification.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct SnapshotVerificationMarker {
    version: u8,
    repository: String,
    revision: String,
    canonical_path: String,
    files: BTreeMap<String, SnapshotFileVerification>,
}

impl SnapshotVerificationMarker {
    fn new(
        repository: &str,
        revision: &str,
        canonical_path: &Path,
        files: &BTreeMap<String, String>,
        identities: &BTreeMap<String, FileIdentity>,
    ) -> Result<Self, ArtifactCacheError> {
        let files: BTreeMap<String, SnapshotFileVerification> = files
            .iter()
            .map(|(path, declared_digest)| {
                let identity = identities.get(path).cloned().ok_or_else(|| {
                    ArtifactCacheError::message(
                        "snapshot verification evidence has an incomplete identity set",
                    )
                })?;
                Ok((
                    path.clone(),
                    SnapshotFileVerification {
                        declared_digest: declared_digest.clone(),
                        identity,
                    },
                ))
            })
            .collect::<Result<_, ArtifactCacheError>>()?;
        if identities.len() != files.len() {
            return Err(ArtifactCacheError::message(
                "snapshot verification evidence has an unexpected identity",
            ));
        }
        Ok(Self {
            version: VERIFICATION_MARKER_VERSION,
            repository: repository.to_owned(),
            revision: revision.to_owned(),
            canonical_path: BASE64.encode(canonical_path.as_os_str().as_bytes()),
            files,
        })
    }

    fn matches_path(&self, path: &Path) -> Result<bool, ArtifactCacheError> {
        verification_evidence_matches(path, self)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct SnapshotFileVerification {
    declared_digest: String,
    identity: FileIdentity,
}

enum SnapshotTreeValidation {
    Invalid,
    VerifiedByIdentity,
    VerifiedByDigest(SnapshotVerificationMarker),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct SnapshotRepairState {
    spent_at_seconds: Option<u64>,
    negative_signature: Option<String>,
    #[serde(default)]
    negative_cause: Option<SnapshotRepairCause>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
enum SnapshotRepairCause {
    IncompleteRemote,
    DigestMismatch,
}

impl SnapshotRepairCause {
    fn message(self) -> &'static str {
        match self {
            Self::IncompleteRemote => "remote repair did not complete",
            Self::DigestMismatch => "reacquired content failed its declared digest",
        }
    }
}

impl SnapshotRepairState {
    const RETRY_WINDOW_SECONDS: u64 = 600;

    fn retry_after_seconds(&self, now: u64) -> Option<u64> {
        self.spent_at_seconds
            .map(|spent| {
                spent
                    .saturating_add(Self::RETRY_WINDOW_SECONDS)
                    .saturating_sub(now)
            })
            .filter(|remaining| *remaining > 0)
    }
}

fn snapshot_repair_now() -> Result<u64, ArtifactCacheError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| ArtifactCacheError::context("snapshot repair clock failed", error))
}

fn snapshot_repair_state(path: &Path) -> Result<SnapshotRepairState, ArtifactCacheError> {
    match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents).map_err(|error| {
            ArtifactCacheError::context("snapshot repair record is invalid", error)
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(SnapshotRepairState::default())
        }
        Err(error) => Err(ArtifactCacheError::context(
            "snapshot repair record read failed",
            error,
        )),
    }
}

fn snapshot_repair_signature(
    publication: &Path,
    marker: &Path,
    sources: &BTreeMap<String, PathBuf>,
) -> Result<String, ArtifactCacheError> {
    fn include_path(
        digest: &mut Sha256,
        path: &Path,
        relative: &Path,
    ) -> Result<(), ArtifactCacheError> {
        let name = relative.as_os_str().as_bytes();
        digest.update((name.len() as u64).to_le_bytes());
        digest.update(name);
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                digest.update(format!(
                    "{}:{}:{}:{}:{}:{}:{}:{}",
                    metadata.mode(),
                    metadata.dev(),
                    metadata.ino(),
                    metadata.len(),
                    metadata.mtime(),
                    metadata.mtime_nsec(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                ));
                if metadata.is_dir() && !metadata.file_type().is_symlink() {
                    let mut entries = fs::read_dir(path)
                        .map_err(|error| {
                            ArtifactCacheError::context("snapshot repair scan failed", error)
                        })?
                        .map(|entry| entry.map(|entry| entry.file_name()))
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|error| {
                            ArtifactCacheError::context("snapshot repair entry failed", error)
                        })?;
                    entries.sort();
                    for name in entries {
                        include_path(digest, &path.join(&name), &relative.join(name))?;
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => digest.update(b"missing"),
            Err(error) => {
                return Err(ArtifactCacheError::context(
                    "snapshot repair inspection failed",
                    error,
                ));
            }
        }
        Ok(())
    }

    let mut digest = Sha256::new();
    include_path(&mut digest, publication, Path::new("publication"))?;
    include_path(&mut digest, marker, Path::new("marker"))?;
    for (name, path) in sources {
        digest.update((name.len() as u64).to_le_bytes());
        digest.update(name.as_bytes());
        include_path(&mut digest, path, Path::new("source"))?;
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn verification_evidence_matches<T>(path: &Path, expected: &T) -> Result<bool, ArtifactCacheError>
where
    T: DeserializeOwned + PartialEq,
{
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(ArtifactCacheError::context(
                "artifact verification marker read failed",
                error,
            ));
        }
    };
    let actual = match serde_json::from_slice::<T>(&contents) {
        Ok(marker) => marker,
        Err(_) => return Ok(false),
    };
    Ok(actual == *expected)
}

fn write_verification_evidence<T: Serialize>(
    path: &Path,
    evidence: &T,
) -> Result<(), ArtifactCacheError> {
    let parent = path.parent().ok_or_else(|| {
        ArtifactCacheError::message("artifact verification marker has no parent directory")
    })?;
    let encoded = serde_json::to_vec(evidence).map_err(|error| {
        ArtifactCacheError::context("artifact verification marker encoding failed", error)
    })?;
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| {
                ArtifactCacheError::message("artifact verification marker has an invalid filename")
            })?,
        std::process::id(),
        MARKER_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| {
                ArtifactCacheError::context(
                    "artifact verification marker temporary open failed",
                    error,
                )
            })?;
        file.write_all(&encoded).map_err(|error| {
            ArtifactCacheError::context("artifact verification marker write failed", error)
        })?;
        // Sync the temporary file before rename so published evidence has durable contents.
        dotcore_domain::fs_sync::sync(&file).map_err(|error| {
            ArtifactCacheError::context("artifact verification marker sync failed", error)
        })?;
        fs::rename(&temporary, path).map_err(|error| {
            ArtifactCacheError::context(
                "artifact verification marker atomic replacement failed",
                error,
            )
        })?;
        // Sync the directory after rename so the replacement itself is durable.
        File::open(parent)
            .and_then(|directory| dotcore_domain::fs_sync::sync(&directory))
            .map_err(|error| {
                ArtifactCacheError::context(
                    "artifact verification marker directory sync failed",
                    error,
                )
            })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// File facts that change when a verified blob is replaced or modified.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct FileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl FileIdentity {
    fn from_path(path: &Path) -> Result<Self, ArtifactCacheError> {
        let metadata = fs::metadata(path).map_err(|error| {
            ArtifactCacheError::context("artifact file inspection failed", error)
        })?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        })
    }

    /// Compares everything but the change time, which links and renames of the inode also move.
    fn matches_except_change_time(&self, other: &Self) -> bool {
        self.device == other.device
            && self.inode == other.inode
            && self.size == other.size
            && self.modified_seconds == other.modified_seconds
            && self.modified_nanoseconds == other.modified_nanoseconds
    }

    fn verify_digest(
        path: &Path,
        declared_digest: &str,
        digest_count: Option<&AtomicUsize>,
        cancellation: &CancellationToken,
    ) -> Result<Option<Self>, ArtifactCacheError> {
        let file = File::open(path)
            .map_err(|error| ArtifactCacheError::context("artifact file open failed", error))?;
        let before = Self::from_path(path)?;
        if let Some(digest_count) = digest_count {
            digest_count.fetch_add(1, Ordering::SeqCst);
        }
        let actual = digest_file(file, cancellation)?;
        let after = Self::from_path(path)?;
        if before != after {
            return Ok(None);
        }
        if actual == declared_digest {
            Ok(Some(after))
        } else {
            Ok(None)
        }
    }
}

fn validate_hugging_face_coordinates(
    repository: &str,
    revision: &str,
) -> Result<(), ArtifactCacheError> {
    if valid_repository(repository) && valid_revision(revision) {
        Ok(())
    } else {
        Err(ArtifactCacheError::message(
            "Model artifact has unsafe immutable Hugging Face coordinates",
        ))
    }
}

fn validate_snapshot_manifest(files: &BTreeMap<String, String>) -> Result<(), ArtifactCacheError> {
    if files.is_empty()
        || files
            .iter()
            .any(|(path, digest)| !valid_repository_path(path) || !valid_digest(digest))
        || files.keys().any(|path| {
            snapshot_ancestors(path).any(|ancestor| files.contains_key(ancestor.as_str()))
        })
    {
        Err(ArtifactCacheError::message(
            "Model snapshot has an invalid path or SHA-256 manifest",
        ))
    } else {
        Ok(())
    }
}

fn validate_snapshot_tree(
    root: &Path,
    repository: &str,
    revision: &str,
    files: &BTreeMap<String, String>,
    marker_path: &Path,
    digest_count: Option<&AtomicUsize>,
    cancellation: &CancellationToken,
) -> Result<SnapshotTreeValidation, ArtifactCacheError> {
    check_cancellation(cancellation)?;
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SnapshotTreeValidation::Invalid);
        }
        Err(error) => {
            return Err(ArtifactCacheError::context(
                "snapshot publication inspection failed",
                error,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(SnapshotTreeValidation::Invalid);
    }
    let Some((actual_files, actual_directories)) = snapshot_tree_entries(root, cancellation)?
    else {
        return Ok(SnapshotTreeValidation::Invalid);
    };
    let expected_files = files.keys().cloned().collect::<BTreeSet<_>>();
    let expected_directories = files
        .keys()
        .flat_map(|path| snapshot_ancestors(path))
        .collect::<BTreeSet<_>>();
    if actual_files != expected_files || actual_directories != expected_directories {
        return Ok(SnapshotTreeValidation::Invalid);
    }

    let identities = files
        .keys()
        .map(|path| Ok((path.clone(), FileIdentity::from_path(&root.join(path))?)))
        .collect::<Result<BTreeMap<_, _>, ArtifactCacheError>>()?;
    let identity_marker =
        SnapshotVerificationMarker::new(repository, revision, root, files, &identities)?;
    if identity_marker.matches_path(marker_path)? {
        return Ok(SnapshotTreeValidation::VerifiedByIdentity);
    }

    let mut verified_identities = BTreeMap::new();
    for (path, digest) in files {
        check_cancellation(cancellation)?;
        let Some(identity) =
            FileIdentity::verify_digest(&root.join(path), digest, digest_count, cancellation)?
        else {
            return Ok(SnapshotTreeValidation::Invalid);
        };
        verified_identities.insert(path.clone(), identity);
    }
    Ok(SnapshotTreeValidation::VerifiedByDigest(
        SnapshotVerificationMarker::new(repository, revision, root, files, &verified_identities)?,
    ))
}

type SnapshotTreeEntries = (BTreeSet<String>, BTreeSet<String>);

fn snapshot_tree_entries(
    root: &Path,
    cancellation: &CancellationToken,
) -> Result<Option<SnapshotTreeEntries>, ArtifactCacheError> {
    fn collect(
        root: &Path,
        directory: &Path,
        files: &mut BTreeSet<String>,
        directories: &mut BTreeSet<String>,
        cancellation: &CancellationToken,
    ) -> Result<bool, ArtifactCacheError> {
        check_cancellation(cancellation)?;
        for entry in fs::read_dir(directory)
            .map_err(|error| ArtifactCacheError::context("snapshot directory read failed", error))?
        {
            check_cancellation(cancellation)?;
            let entry = entry.map_err(|error| {
                ArtifactCacheError::context("snapshot directory entry read failed", error)
            })?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                ArtifactCacheError::context("snapshot entry inspection failed", error)
            })?;
            let relative = path
                .strip_prefix(root)
                .map_err(|error| {
                    ArtifactCacheError::context("snapshot entry escaped publication root", error)
                })?
                .to_str()
                .ok_or_else(|| {
                    ArtifactCacheError::message("snapshot entry path is not valid UTF-8")
                })?
                .to_owned();
            if metadata.file_type().is_symlink() {
                return Ok(false);
            }
            if metadata.is_dir() {
                directories.insert(relative);
                if !collect(root, &path, files, directories, cancellation)? {
                    return Ok(false);
                }
            } else if metadata.is_file() {
                files.insert(relative);
            } else {
                return Ok(false);
            }
        }
        Ok(true)
    }

    let mut files = BTreeSet::new();
    let mut directories = BTreeSet::new();
    if !collect(root, root, &mut files, &mut directories, cancellation)? {
        return Ok(None);
    }
    Ok(Some((files, directories)))
}

fn publish_snapshot_tree(
    snapshot: SnapshotPublication<'_>,
    digest_count: Option<&AtomicUsize>,
    blocking_work: Option<&BlockingWorkGate>,
    cancellation: &CancellationToken,
    stage: Option<&ModelArtifactStageObserver>,
) -> Result<PublishedSnapshot, ArtifactCacheError> {
    let SnapshotPublication {
        path: publication,
        repository,
        revision,
        sources,
        files,
        refuse_hard_links,
    } = snapshot;
    check_cancellation(cancellation)?;
    let parent = publication.parent().ok_or_else(|| {
        ArtifactCacheError::message("snapshot publication has no parent directory")
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        ArtifactCacheError::context("snapshot publication directory creation failed", error)
    })?;
    let suffix = format!(
        "{}.{}",
        std::process::id(),
        MARKER_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let name = publication
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| ArtifactCacheError::message("snapshot publication has an invalid name"))?;
    let temporary = parent.join(format!(".{name}.{suffix}.tmp"));
    let result = (|| {
        fs::create_dir(&temporary).map_err(|error| {
            ArtifactCacheError::context("snapshot temporary directory creation failed", error)
        })?;
        if let Some(blocking_work) = blocking_work {
            blocking_work.wait();
            check_cancellation(cancellation)?;
        }

        let directories = files
            .keys()
            .flat_map(|path| snapshot_ancestors(path))
            .collect::<BTreeSet<_>>();
        match fs::symlink_metadata(publication) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(ArtifactCacheError::StructuralConflict {
                    kind: "publication root is not a directory",
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(publication).map_err(|error| {
                    ArtifactCacheError::context("snapshot publication root creation failed", error)
                })?;
            }
            Err(error) => {
                return Err(ArtifactCacheError::context(
                    "snapshot publication root inspection failed",
                    error,
                ));
            }
        }
        for directory in &directories {
            check_cancellation(cancellation)?;
            let path = publication.join(directory);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => {
                    return Err(ArtifactCacheError::StructuralConflict {
                        kind: "declared directory is not a directory",
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&path).map_err(|error| {
                        ArtifactCacheError::context(
                            "snapshot declared directory creation failed",
                            error,
                        )
                    })?;
                }
                Err(error) => {
                    return Err(ArtifactCacheError::context(
                        "snapshot declared directory inspection failed",
                        error,
                    ));
                }
            }
        }

        let mut retained = BTreeMap::new();
        let mut replacements = Vec::new();
        let mut placements = SnapshotPlacements::default();
        let mut linked_inodes = BTreeSet::new();
        for (path, digest) in files {
            check_cancellation(cancellation)?;
            let destination = publication.join(path);
            match fs::symlink_metadata(&destination) {
                Ok(metadata) if metadata.is_dir() => {
                    return Err(ArtifactCacheError::StructuralConflict {
                        kind: "declared file is a directory",
                    });
                }
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                    if let Some(identity) = FileIdentity::verify_digest(
                        &destination,
                        digest,
                        digest_count,
                        cancellation,
                    )? {
                        retained.insert(path.clone(), identity);
                        placements.retained += 1;
                        continue;
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(ArtifactCacheError::context(
                        "snapshot declared file inspection failed",
                        error,
                    ));
                }
            }
            let source =
                sources
                    .get(path)
                    .ok_or_else(|| ArtifactCacheError::SnapshotSourceUnavailable {
                        path: path.clone(),
                    })?;
            let staged = temporary.join(path);
            fs::create_dir_all(staged.parent().expect("snapshot file parent exists")).map_err(
                |error| {
                    ArtifactCacheError::context("snapshot file directory creation failed", error)
                },
            )?;
            let placement = stage_snapshot_file(source, &staged, refuse_hard_links)?;
            let Some(staged_identity) =
                FileIdentity::verify_digest(&staged, digest, digest_count, cancellation)?
            else {
                return Err(ArtifactCacheError::SnapshotSourceUnavailable { path: path.clone() });
            };
            match placement {
                SnapshotFilePlacement::Linked => {
                    placements.linked += 1;
                    linked_inodes.insert((staged_identity.device, staged_identity.inode));
                }
                SnapshotFilePlacement::Copied => placements.copied += 1,
            }
            File::open(&staged)
                .and_then(|file| dotcore_domain::fs_sync::sync(&file))
                .map_err(|error| ArtifactCacheError::context("snapshot file sync failed", error))?;
            replacements.push((path.clone(), staged));
        }
        sync_snapshot_directories(&temporary)?;
        check_cancellation(cancellation)?;
        if let Some(stage) = stage {
            stage(ModelArtifactStage::Publish);
        }
        for (path, staged) in &replacements {
            check_cancellation(cancellation)?;
            let destination = publication.join(path);
            fs::rename(staged, &destination).map_err(|error| {
                ArtifactCacheError::context("snapshot file publication failed", error)
            })?;
            File::open(destination.parent().expect("snapshot file parent exists"))
                .and_then(|directory| dotcore_domain::fs_sync::sync(&directory))
                .map_err(|error| {
                    ArtifactCacheError::context("snapshot file parent sync failed", error)
                })?;
        }
        let removed = remove_undeclared_snapshot_entries(
            publication,
            publication,
            files,
            &directories,
            cancellation,
        )?;
        sync_snapshot_directories(publication)?;
        File::open(parent)
            .and_then(|directory| dotcore_domain::fs_sync::sync(&directory))
            .map_err(|error| {
                ArtifactCacheError::context("snapshot publication parent sync failed", error)
            })?;

        let mut identities = BTreeMap::new();
        for path in files.keys() {
            check_cancellation(cancellation)?;
            let identity = FileIdentity::from_path(&publication.join(path))?;
            // Identical repository files share one hub blob, so linking or renaming a replacement
            // also moves the change time of a retained file on the same inode.
            if retained.get(path).is_some_and(|previous| {
                previous != &identity
                    && !(linked_inodes.contains(&(previous.device, previous.inode))
                        && previous.matches_except_change_time(&identity))
            }) {
                return Err(ArtifactCacheError::message(
                    "snapshot file changed during publication",
                ));
            }
            identities.insert(path.clone(), identity);
        }
        let marker =
            SnapshotVerificationMarker::new(repository, revision, publication, files, &identities)?;
        Ok(PublishedSnapshot {
            replaced: !replacements.is_empty() || removed,
            marker,
            placements,
        })
    })();
    let cleanup = remove_owned_snapshot_path(&temporary);
    match (result, cleanup) {
        (Ok(published), Ok(())) => Ok(published),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

/// Places one verified hub blob at its staged snapshot path by hard link, so the publication
/// shares the blob's bytes instead of copying them. The hub client replaces blobs by rename and
/// never writes through an existing inode, so a linked publication keeps its bytes when the hub
/// cache changes. Any link failure (another filesystem, no link support, link limit) falls back to
/// a copy, which still clones on filesystems that support it.
fn stage_snapshot_file(
    source: &Path,
    staged: &Path,
    refuse_hard_links: bool,
) -> Result<SnapshotFilePlacement, ArtifactCacheError> {
    // Hard links do not follow symlinks on Linux, so only a regular blob is linked.
    let regular = fs::symlink_metadata(source).is_ok_and(|metadata| metadata.is_file());
    let linked = if !regular {
        Err(std::io::Error::other(
            "snapshot source is not a regular file",
        ))
    } else if refuse_hard_links {
        Err(std::io::Error::other("hard links refused for test"))
    } else {
        fs::hard_link(source, staged)
    };
    match linked {
        Ok(()) => Ok(SnapshotFilePlacement::Linked),
        Err(error) => {
            tracing::debug!(
                event_kind = "snapshot_file_link_fallback",
                source = %source.display(),
                error = %error,
                "copying snapshot file after hard link failed"
            );
            fs::copy(source, staged)
                .map_err(|error| ArtifactCacheError::context("snapshot file copy failed", error))?;
            Ok(SnapshotFilePlacement::Copied)
        }
    }
}

/// How one snapshot file reached its staged path.
enum SnapshotFilePlacement {
    Linked,
    Copied,
}

fn remove_undeclared_snapshot_entries(
    root: &Path,
    directory: &Path,
    files: &BTreeMap<String, String>,
    directories: &BTreeSet<String>,
    cancellation: &CancellationToken,
) -> Result<bool, ArtifactCacheError> {
    let mut removed = false;
    for entry in fs::read_dir(directory).map_err(|error| {
        ArtifactCacheError::context("snapshot cleanup directory read failed", error)
    })? {
        check_cancellation(cancellation)?;
        let path = entry
            .map_err(|error| {
                ArtifactCacheError::context("snapshot cleanup entry read failed", error)
            })?
            .path();
        let relative = path.strip_prefix(root).map_err(|error| {
            ArtifactCacheError::context("snapshot cleanup path escaped publication", error)
        })?;
        let relative = relative.to_str().ok_or_else(|| {
            ArtifactCacheError::message("snapshot cleanup path is not valid UTF-8")
        })?;
        if directories.contains(relative) {
            removed |=
                remove_undeclared_snapshot_entries(root, &path, files, directories, cancellation)?;
        } else if !files.contains_key(relative) {
            remove_owned_snapshot_path(&path)?;
            removed = true;
        }
    }
    Ok(removed)
}

fn sync_snapshot_directories(root: &Path) -> Result<(), ArtifactCacheError> {
    let mut directories = vec![root.to_owned()];
    let mut index = 0;
    while index < directories.len() {
        for entry in fs::read_dir(&directories[index]).map_err(|error| {
            ArtifactCacheError::context("snapshot directory sync scan failed", error)
        })? {
            let path = entry
                .map_err(|error| {
                    ArtifactCacheError::context("snapshot directory sync entry failed", error)
                })?
                .path();
            if fs::symlink_metadata(&path)
                .map_err(|error| {
                    ArtifactCacheError::context("snapshot directory sync inspection failed", error)
                })?
                .is_dir()
            {
                directories.push(path);
            }
        }
        index += 1;
    }
    for directory in directories.into_iter().rev() {
        File::open(directory)
            .and_then(|directory| dotcore_domain::fs_sync::sync(&directory))
            .map_err(|error| {
                ArtifactCacheError::context("snapshot directory sync failed", error)
            })?;
    }
    Ok(())
}

fn remove_owned_snapshot_path(path: &Path) -> Result<(), ArtifactCacheError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path).map_err(|error| {
                ArtifactCacheError::context("snapshot owned directory removal failed", error)
            })
        }
        Ok(_) => fs::remove_file(path).map_err(|error| {
            ArtifactCacheError::context("snapshot owned path removal failed", error)
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ArtifactCacheError::context(
            "snapshot owned path inspection failed",
            error,
        )),
    }
}

fn snapshot_ancestors(path: &str) -> impl Iterator<Item = String> + '_ {
    let segments = path.split('/').collect::<Vec<_>>();
    let ancestor_count = segments.len().saturating_sub(1);
    let mut ancestor = String::new();
    segments
        .into_iter()
        .take(ancestor_count)
        .map(move |segment| {
            if !ancestor.is_empty() {
                ancestor.push('/');
            }
            ancestor.push_str(segment);
            ancestor.clone()
        })
}

fn valid_repository_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && !value.contains('\\')
        && value.split('/').all(|segment| {
            !segment.is_empty()
                && segment.len() <= 255
                && !matches!(segment, "." | "..")
                && !segment.chars().any(char::is_control)
        })
}

pub(super) async fn acquire_lock(
    path: PathBuf,
    cancellation: &CancellationToken,
    attempted: Option<&tokio::sync::Notify>,
) -> Result<File, ArtifactCacheError> {
    acquire_lock_as(path, cancellation, attempted, LockMode::Exclusive, None).await
}

/// Takes a lock that other shared holders may hold at the same time, but no exclusive one.
pub(super) async fn acquire_shared_lock(
    path: PathBuf,
    cancellation: &CancellationToken,
) -> Result<File, ArtifactCacheError> {
    acquire_lock_as(path, cancellation, None, LockMode::Shared, None).await
}

/// Acquires the cross-process lock at `path` for one preparation step, reporting the step waits
/// when another preparation holds it.
pub(super) async fn acquire_step_lock(
    path: PathBuf,
    cancellation: &CancellationToken,
    step: &StepOutput,
) -> Result<File, ArtifactCacheError> {
    acquire_lock_as(path, cancellation, None, LockMode::Exclusive, Some(step)).await
}

#[derive(Clone, Copy)]
enum LockMode {
    Exclusive,
    Shared,
}

#[expect(
    clippy::disallowed_methods,
    reason = "polls a file lock another process may hold"
)]
async fn acquire_lock_as(
    path: PathBuf,
    cancellation: &CancellationToken,
    attempted: Option<&tokio::sync::Notify>,
    mode: LockMode,
    mut waiting: Option<&StepOutput>,
) -> Result<File, ArtifactCacheError> {
    check_cancellation(cancellation)?;
    let file: File = tokio::task::spawn_blocking(move || -> Result<File, ArtifactCacheError> {
        let parent = path.parent().ok_or_else(|| {
            ArtifactCacheError::message("artifact coordinate lock has no parent directory")
        })?;
        std::fs::create_dir_all(parent).map_err(|error| {
            ArtifactCacheError::context("artifact coordinate lock directory creation failed", error)
        })?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|error| {
                ArtifactCacheError::context("artifact coordinate lock open failed", error)
            })?;
        Ok(file)
    })
    .await
    .map_err(|error| {
        ArtifactCacheError::context("artifact coordinate lock task failed", error)
    })??;
    loop {
        check_cancellation(cancellation)?;
        if let Some(attempted) = attempted {
            attempted.notify_one();
        }
        let locked = match mode {
            LockMode::Exclusive => file.try_lock_exclusive(),
            LockMode::Shared => FileExt::try_lock_shared(&file),
        };
        match locked {
            Ok(true) => return Ok(file),
            Ok(false) => {
                if let Some(step) = waiting.take() {
                    step.waiting();
                }
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(ArtifactCacheError::Cancelled),
                    () = tokio::time::sleep(std::time::Duration::from_millis(10)) => {},
                }
            }
            Err(error) => {
                return Err(ArtifactCacheError::context(
                    "artifact coordinate lock acquisition failed",
                    error,
                ));
            }
        }
    }
}

async fn canonical_regular_path(
    root: &Path,
    path: &Path,
    cancellation: &CancellationToken,
) -> Result<PathBuf, ArtifactCacheError> {
    check_cancellation(cancellation)?;
    let root = root.to_owned();
    let path = path.to_owned();
    let result = tokio::task::spawn_blocking(move || {
        let root = root.canonicalize().map_err(|error| {
            ArtifactCacheError::context("Hugging Face cache root canonicalization failed", error)
        })?;
        let path = path.canonicalize().map_err(|error| {
            ArtifactCacheError::context("Hugging Face returned path canonicalization failed", error)
        })?;
        if !path.starts_with(&root) {
            return Err(ArtifactCacheError::message(
                "Hugging Face returned a path outside the approved cache root",
            ));
        }
        let metadata = std::fs::metadata(&path).map_err(|error| {
            ArtifactCacheError::context("Hugging Face returned path inspection failed", error)
        })?;
        if !metadata.is_file() {
            return Err(ArtifactCacheError::message(
                "Hugging Face returned a non-regular artifact path",
            ));
        }
        Ok(path)
    })
    .await
    .map_err(|error| {
        ArtifactCacheError::context("Hugging Face returned path validation task failed", error)
    })?;
    check_cancellation(cancellation)?;
    result
}

fn digest_file(file: File, cancellation: &CancellationToken) -> Result<String, ArtifactCacheError> {
    let mut reader = BufReader::new(file);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        check_cancellation(cancellation)?;
        let bytes = reader
            .read(&mut buffer)
            .map_err(|error| ArtifactCacheError::context("artifact file read failed", error))?;
        if bytes == 0 {
            return Ok(hex_digest(digest.finalize()));
        }
        digest.update(&buffer[..bytes]);
    }
}

fn check_cancellation(cancellation: &CancellationToken) -> Result<(), ArtifactCacheError> {
    if cancellation.is_cancelled() {
        Err(ArtifactCacheError::Cancelled)
    } else {
        Ok(())
    }
}

fn sha256_text(value: &str) -> String {
    hex_digest(Sha256::digest(value.as_bytes()))
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn valid_repository(value: &str) -> bool {
    let segments = value.split('/').collect::<Vec<_>>();
    segments.len() == 2 && segments.iter().all(|segment| valid_path_segment(segment))
}

fn valid_revision(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_path_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && !matches!(value, "." | "..")
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Where one artifact's downloads report: the invocation's transfer progress, when observed, and
/// the output of the model download step.
#[derive(Clone, Copy)]
struct DownloadReports<'a> {
    progress: Option<&'a ModelArtifactProgressSlot>,
    output: &'a StepOutput,
}

/// Progress of one file's Hub download, fed to the invocation's transfer progress and reported
/// as a line each time another tenth of the file arrived.
struct HuggingFaceDownloadProgress {
    slot: Option<ModelArtifactProgressSlot>,
    output: StepOutput,
    path: String,
    total: AtomicU64,
    reported_tenths: AtomicU64,
}

impl HuggingFaceDownloadProgress {
    fn report(&self, completed: u64, total: u64) {
        let total = if total > 0 {
            self.total.fetch_max(total, Ordering::SeqCst).max(total)
        } else {
            self.total.load(Ordering::SeqCst)
        };
        // A completed file is reported by its own line.
        if total == 0 || completed >= total {
            return;
        }
        let tenths = (u128::from(completed) * 10 / u128::from(total)) as u64;
        if self.reported_tenths.fetch_max(tenths, Ordering::SeqCst) < tenths {
            self.output.line(
                OutputStream::Stdout,
                format!("{}: {}% of {}", self.path, tenths * 10, human_bytes(total)),
            );
        }
    }
}

impl ProgressHandler for HuggingFaceDownloadProgress {
    fn on_progress(&self, event: &ProgressEvent) {
        let ProgressEvent::Download(event) = event else {
            return;
        };
        match event {
            DownloadEvent::Start { total_bytes, .. } => {
                self.total.store(*total_bytes, Ordering::SeqCst);
                if let Some(slot) = &self.slot {
                    slot.transfer_started(*total_bytes);
                }
            }
            DownloadEvent::Progress { files } => {
                for file in files {
                    if let Some(slot) = &self.slot {
                        slot.ordinary_progress(file.bytes_completed, file.total_bytes);
                    }
                    self.report(file.bytes_completed, file.total_bytes);
                }
            }
            DownloadEvent::AggregateProgress {
                bytes_completed,
                total_bytes,
                ..
            } => {
                if let Some(slot) = &self.slot {
                    slot.aggregate_progress(*bytes_completed, *total_bytes);
                }
                self.report(*bytes_completed, *total_bytes);
            }
            DownloadEvent::Complete => {
                if let Some(slot) = &self.slot {
                    slot.transfer_completed();
                }
            }
        }
    }
}

/// Renders a byte count with a binary unit and one decimal, for example `1.5 GiB`.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(any(test, feature = "test-support"))]
impl ModelArtifactCache {
    pub fn for_test(root: PathBuf, endpoint: &str) -> Self {
        Self::with_client(
            root.clone(),
            HFClient::builder()
                .cache_dir(root)
                .endpoint(endpoint)
                .build(),
            None,
            None,
        )
        .expect("test cache")
    }

    pub fn for_test_with_progress(root: PathBuf, progress: Progress) -> Self {
        Self::with_client(
            root.clone(),
            HFClient::builder()
                .cache_dir(root)
                .endpoint(HUGGING_FACE_ENDPOINT)
                .build(),
            Some(progress),
            None,
        )
        .expect("test cache")
    }

    pub fn for_test_with_digest_count(
        root: PathBuf,
        endpoint: &str,
        digest_count: Arc<AtomicUsize>,
    ) -> Self {
        Self::with_client(
            root.clone(),
            HFClient::builder()
                .cache_dir(root)
                .endpoint(endpoint)
                .build(),
            None,
            Some(digest_count),
        )
        .expect("test cache")
    }

    #[cfg(test)]
    fn with_pointer_rewrite(mut self, gate: Arc<PointerRewriteGate>) -> Self {
        self.pointer_rewrite = Some(gate);
        self
    }

    /// Makes every snapshot hard link fail, so publication takes its copy fallback.
    #[cfg(test)]
    fn with_refused_hard_links(mut self) -> Self {
        self.refuse_hard_links = true;
        self
    }

    #[cfg(test)]
    fn with_lock_attempted(mut self, attempted: Arc<tokio::sync::Notify>) -> Self {
        self.lock_attempted = Some(attempted);
        self
    }

    #[cfg(test)]
    fn with_blocking_work_failure(
        mut self,
        file: &str,
        blocking_work: Arc<BlockingWorkGate>,
    ) -> Self {
        self.blocking_work = Some(blocking_work.clone());
        self.branch_failure = Some(Arc::new(BranchFailure {
            file: file.to_owned(),
            blocking_work,
            fired: AtomicBool::new(false),
            panic: false,
        }));
        self
    }

    pub fn with_cross_deployment_panic(
        mut self,
        file: &str,
        blocking_work: Arc<BlockingWorkGate>,
    ) -> Self {
        self.blocking_work = Some(blocking_work.clone());
        self.branch_failure = Some(Arc::new(BranchFailure {
            file: file.to_owned(),
            blocking_work,
            fired: AtomicBool::new(false),
            panic: true,
        }));
        self
    }
}

#[cfg(test)]
mod tests;
