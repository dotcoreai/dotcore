use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use hf_hub::progress::{DownloadEvent, ProgressEvent, ProgressHandler};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

use super::*;
use crate::output::{PreparationEventKind, PreparationStep, test_support::RecordedOutput};

static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

#[test]
fn artifact_path_validation_is_format_neutral_and_path_safe() {
    for valid in [
        "weights.safetensors",
        "weights.gguf",
        "MTP/mtp-weights.gguf",
        "nested/deeper/weights.safetensors",
    ] {
        assert!(valid_repository_path(valid), "{valid}");
    }
    for invalid in [
        "",
        "..",
        "../weights.gguf",
        "./weights.gguf",
        "/weights.gguf",
        "nested//weights.gguf",
        "nested/",
        "nested\\weights.gguf",
        "bad\npath",
    ] {
        assert!(!valid_repository_path(invalid), "{invalid:?}");
    }
}

#[test]
fn snapshot_publication_identity_is_manifest_order_independent_and_coordinate_specific() {
    let root = temp_root("snapshot-identity");
    std::fs::create_dir_all(&root).expect("cache root");
    let cache = ModelArtifactCache::for_test(root.clone(), "http://127.0.0.1:1");
    let first = snapshot_artifact(
        "owner/repository",
        [
            ("config.json", "config"),
            ("tokenizer/config.json", "tokenizer"),
        ],
    );
    let reordered = snapshot_artifact(
        "owner/repository",
        [
            ("tokenizer/config.json", "tokenizer"),
            ("config.json", "config"),
        ],
    );
    let changed_manifest = snapshot_artifact(
        "owner/repository",
        [
            ("config.json", "changed"),
            ("tokenizer/config.json", "tokenizer"),
        ],
    );
    let changed_repository = snapshot_artifact(
        "other/repository",
        [
            ("config.json", "config"),
            ("tokenizer/config.json", "tokenizer"),
        ],
    );

    let first_path = cache.snapshot_path(&first).expect("first path");
    assert_eq!(
        cache.snapshot_path(&reordered).expect("reordered path"),
        first_path
    );
    assert_ne!(
        cache
            .snapshot_path(&changed_manifest)
            .expect("changed manifest path"),
        first_path
    );
    assert_ne!(
        cache
            .snapshot_path(&changed_repository)
            .expect("changed repository path"),
        first_path
    );
    assert_eq!(
        first_path.parent(),
        Some(root.join(".dotcore-snapshots").as_path())
    );
    std::fs::remove_dir_all(root).expect("snapshot identity cleanup");
}

#[tokio::test]
async fn publishes_an_exact_verified_snapshot_and_reuses_it_offline() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"shared");
    let cache = fixture.cache();
    let artifact = snapshot_artifact(
        "owner/repository",
        [
            ("config.json", "shared"),
            ("tokenizer/config.json", "shared"),
        ],
    );

    let published = cache
        .materialize(&artifact)
        .await
        .expect("published snapshot");

    assert_eq!(
        snapshot_regular_files(&published.path),
        BTreeMap::from([
            ("config.json".to_owned(), b"shared".to_vec()),
            ("tokenizer/config.json".to_owned(), b"shared".to_vec()),
        ])
    );
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), 1);
    fixture.set_failure();

    let reused = cache
        .materialize(&artifact)
        .await
        .expect("offline warm reuse");

    assert_eq!(reused.path, published.path);
    assert!(!reused.replaced);
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn publishes_snapshot_files_as_hard_links_to_their_hub_blobs() {
    let fixture = Fixture::new().await;
    let artifact = split_snapshot(&fixture);
    let cache = fixture.cache();

    let (published, records, _) =
        dotcore_domain::test_support::tracing::capture_records_async(|| {
            cache.materialize(&artifact)
        })
        .await;
    let published = published.expect("published snapshot");

    for path in SPLIT_SHARDS.map(|(path, _)| path) {
        assert_eq!(
            inode(&published.path.join(path)),
            inode(&fixture.hub_blob(&artifact, path)),
            "{path}"
        );
    }
    assert_eq!(
        snapshot_regular_files(&published.path),
        SPLIT_SHARDS
            .map(|(path, body)| (path.to_owned(), body.as_bytes().to_vec()))
            .into()
    );
    assert_eq!(publication_counts(&records), [("2", "0")]);
}

#[tokio::test]
async fn copies_snapshot_files_when_hard_links_fail() {
    let fixture = Fixture::new().await;
    let artifact = split_snapshot(&fixture);
    let cache = fixture.cache().with_refused_hard_links();

    let (published, records, _) =
        dotcore_domain::test_support::tracing::capture_records_async(|| {
            cache.materialize(&artifact)
        })
        .await;
    let published = published.expect("copied snapshot");

    for path in SPLIT_SHARDS.map(|(path, _)| path) {
        assert_ne!(
            inode(&published.path.join(path)),
            inode(&fixture.hub_blob(&artifact, path)),
            "{path}"
        );
    }
    assert_eq!(
        snapshot_regular_files(&published.path),
        SPLIT_SHARDS
            .map(|(path, body)| (path.to_owned(), body.as_bytes().to_vec()))
            .into()
    );
    assert_eq!(publication_counts(&records), [("0", "2")]);
}

#[tokio::test]
async fn in_place_corruption_of_a_linked_file_repairs_through_one_forced_download() {
    let fixture = Fixture::new().await;
    let artifact = split_snapshot(&fixture);
    let cache = fixture.cache();
    let initial = cache
        .materialize(&artifact)
        .await
        .expect("initial snapshot");
    let (corrupted, healthy) = (SPLIT_SHARDS[0].0, SPLIT_SHARDS[1].0);
    let corrupted_blob = inode(&fixture.hub_blob(&artifact, corrupted));
    let healthy_inode = inode(&initial.path.join(healthy));
    let requests = fixture.get_requests.load(Ordering::SeqCst);
    // An in-place write reaches the hub blob through the shared inode.
    fs::write(initial.path.join(corrupted), b"corrupt").expect("in-place corruption");
    assert_eq!(
        fs::read(fixture.hub_blob(&artifact, corrupted)).expect("hub blob"),
        b"corrupt"
    );

    let repaired = cache
        .materialize(&artifact)
        .await
        .expect("repaired snapshot");

    assert!(repaired.replaced);
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), requests + 1);
    let repaired_blob = inode(&fixture.hub_blob(&artifact, corrupted));
    assert_ne!(repaired_blob, corrupted_blob);
    assert_eq!(inode(&repaired.path.join(corrupted)), repaired_blob);
    assert_eq!(inode(&repaired.path.join(healthy)), healthy_inode);
    assert_eq!(
        snapshot_regular_files(&repaired.path),
        SPLIT_SHARDS
            .map(|(path, body)| (path.to_owned(), body.as_bytes().to_vec()))
            .into()
    );
}

#[tokio::test]
async fn replaced_linked_file_is_relinked_without_a_download() {
    let fixture = Fixture::new().await;
    let artifact = split_snapshot(&fixture);
    let cache = fixture.cache();
    let initial = cache
        .materialize(&artifact)
        .await
        .expect("initial snapshot");
    let corrupted = SPLIT_SHARDS[0].0;
    let blob = inode(&fixture.hub_blob(&artifact, corrupted));
    let requests = fixture.requests.load(Ordering::SeqCst);
    replace_file(&initial.path.join(corrupted), b"corrupt");

    let repaired = cache
        .materialize(&artifact)
        .await
        .expect("relinked snapshot");

    assert!(repaired.replaced);
    assert_eq!(fixture.requests.load(Ordering::SeqCst), requests);
    assert_eq!(inode(&repaired.path.join(corrupted)), blob);
    assert_eq!(
        fs::read(repaired.path.join(corrupted)).expect("relinked bytes"),
        SPLIT_SHARDS[0].1.as_bytes()
    );
}

#[tokio::test]
async fn linked_snapshot_survives_deleting_its_hub_repository() {
    let fixture = Fixture::new().await;
    let artifact = split_snapshot(&fixture);
    let cache = fixture.counting_cache();
    let published = cache
        .materialize(&artifact)
        .await
        .expect("initial snapshot");
    let hashes = fixture.digest_count.load(Ordering::SeqCst);
    let requests = fixture.requests.load(Ordering::SeqCst);
    fs::remove_dir_all(fixture.root.join("models--owner--repository"))
        .expect("delete hub repository");

    let reused = cache.materialize(&artifact).await.expect("reused snapshot");

    assert!(!reused.replaced);
    assert_eq!(reused.path, published.path);
    assert_eq!(
        fixture.digest_count.load(Ordering::SeqCst),
        hashes + SPLIT_SHARDS.len()
    );
    assert_eq!(
        snapshot_regular_files(&reused.path),
        SPLIT_SHARDS
            .map(|(path, body)| (path.to_owned(), body.as_bytes().to_vec()))
            .into()
    );

    cache.materialize(&artifact).await.expect("warm reuse");

    assert_eq!(
        fixture.digest_count.load(Ordering::SeqCst),
        hashes + SPLIT_SHARDS.len()
    );
    assert_eq!(fixture.requests.load(Ordering::SeqCst), requests);
}

#[tokio::test]
async fn reports_progress_accumulated_over_every_snapshot_file() {
    let fixture = Fixture::new().await;
    let artifact = split_snapshot(&fixture);
    let snapshots = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = snapshots.clone();

    fixture
        .cache()
        .prepare_cancellable(
            &launch_with_artifact(artifact),
            &CancellationToken::new(),
            Some(Arc::new(move |progress| {
                observed.lock().expect("progress snapshots").push(progress);
            })),
        )
        .await
        .expect("downloaded snapshot");

    let snapshots = snapshots.lock().expect("progress snapshots");
    let completed = snapshots
        .iter()
        .map(|progress| progress.completed)
        .collect::<Vec<_>>();
    assert!(
        completed.windows(2).all(|pair| pair[0] <= pair[1]),
        "{completed:?}"
    );
    let size = SPLIT_SHARDS
        .iter()
        .map(|(_, body)| body.len() as u64)
        .sum::<u64>();
    assert_eq!(
        snapshots.last(),
        Some(&crate::progress::ModelArtifactProgress {
            completed: size,
            total: Some(size),
        })
    );
}

#[tokio::test]
async fn snapshot_preparation_reports_verify_publish_and_warm_reuse() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"shared");
    let cache = fixture.cache();
    let artifacts = BTreeMap::from([(
        "model".to_owned(),
        snapshot_artifact("owner/repository", [("config.json", "shared")]),
    )]);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let stages: ModelArtifactStageObserver = {
        let observed = observed.clone();
        Arc::new(move |stage| observed.lock().expect("stages").push(stage))
    };
    let cancellation = CancellationToken::new();

    cache
        .prepare_artifacts_cancellable(
            &artifacts,
            &cancellation,
            None,
            Some(stages.clone()),
            &discarded(),
        )
        .await
        .expect("first publication");
    assert_eq!(
        *observed.lock().expect("stages"),
        [ModelArtifactStage::Verify, ModelArtifactStage::Publish]
    );

    observed.lock().expect("stages").clear();
    fixture.set_failure();
    cache
        .prepare_artifacts_cancellable(&artifacts, &cancellation, None, Some(stages), &discarded())
        .await
        .expect("warm reuse");
    assert_eq!(
        *observed.lock().expect("stages"),
        [ModelArtifactStage::Verify, ModelArtifactStage::Reuse]
    );
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn snapshot_verification_survives_cache_reconstruction_without_rehashing() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"shared");
    let artifact = snapshot_artifact(
        "owner/repository",
        [
            ("config.json", "shared"),
            ("tokenizer/config.json", "shared"),
        ],
    );

    fixture
        .counting_cache()
        .materialize(&artifact)
        .await
        .expect("initial snapshot verification");
    assert_eq!(fixture.digest_count.load(Ordering::SeqCst), 2);

    for _ in 0..6 {
        fixture
            .counting_cache()
            .materialize(&artifact)
            .await
            .expect("warm snapshot safety scan");
    }

    assert_eq!(fixture.digest_count.load(Ordering::SeqCst), 2);
    assert!(fixture.verification_marker_path(&artifact).is_file());
}

#[tokio::test]
async fn missing_or_malformed_snapshot_evidence_forces_rehash_without_download() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"shared");
    let artifact = snapshot_artifact("owner/repository", [("config.json", "shared")]);
    fixture
        .counting_cache()
        .materialize(&artifact)
        .await
        .expect("initial snapshot verification");
    let marker = fixture.verification_marker_path(&artifact);
    let requests = fixture.get_requests.load(Ordering::SeqCst);
    assert_eq!(fixture.digest_count.load(Ordering::SeqCst), 1);

    std::fs::remove_file(&marker).expect("remove snapshot evidence");
    fixture.set_failure();
    fixture
        .counting_cache()
        .materialize(&artifact)
        .await
        .expect("missing evidence refresh");
    assert_eq!(fixture.digest_count.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), requests);

    std::fs::write(&marker, b"not-json").expect("malformed snapshot evidence");
    fixture
        .counting_cache()
        .materialize(&artifact)
        .await
        .expect("malformed evidence refresh");
    assert_eq!(fixture.digest_count.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), requests);
}

#[tokio::test]
async fn changed_snapshot_identity_forces_rehash_and_repairs_corruption() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"expected");
    let artifact = snapshot_artifact("owner/repository", [("config.json", "expected")]);
    let initial = fixture
        .counting_cache()
        .materialize(&artifact)
        .await
        .expect("initial snapshot verification");
    assert_eq!(fixture.digest_count.load(Ordering::SeqCst), 1);

    replace_file(&initial.path.join("config.json"), b"corrupt");
    let repaired = fixture
        .counting_cache()
        .materialize(&artifact)
        .await
        .expect("snapshot corruption repair");

    assert!(repaired.replaced);
    assert_eq!(fixture.digest_count.load(Ordering::SeqCst), 5);
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        snapshot_regular_files(&repaired.path),
        BTreeMap::from([("config.json".to_owned(), b"expected".to_vec())])
    );
}

#[tokio::test]
async fn snapshot_digest_mismatch_never_publishes_partial_state() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"actual");
    let cache = fixture.cache();
    let artifact = snapshot_artifact(
        "owner/repository",
        [
            ("config.json", "actual"),
            ("tokenizer/config.json", "different"),
        ],
    );
    let publication = cache.snapshot_path(&artifact).expect("publication path");

    let error = cache
        .materialize(&artifact)
        .await
        .expect_err("digest mismatch");

    assert!(matches!(error, ArtifactCacheError::DeclaredDigestMismatch));
    assert!(
        !cache
            .snapshot_publication_is_valid(&artifact, &publication, &CancellationToken::new())
            .await
            .expect("partial publication validation")
    );
}

#[tokio::test]
async fn reacquires_once_when_preexisting_snapshot_cache_is_corrupt() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"fresh");
    let cache = fixture.cache();
    let artifact = snapshot_artifact("owner/repository", [("config.json", "fresh")]);
    let cached_file = ModelArtifactSource::HuggingFaceFile {
        repository: "owner/repository".to_owned(),
        revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        path: "config.json".to_owned(),
        provenance_digest: sha256_text("fresh"),
    };
    fixture.cache_file(&cached_file, b"corrupt");

    let published = cache
        .materialize(&artifact)
        .await
        .expect("reacquired snapshot");

    assert!(published.replaced);
    assert_eq!(
        snapshot_regular_files(&published.path),
        BTreeMap::from([("config.json".to_owned(), b"fresh".to_vec())])
    );
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn forced_snapshot_acquisition_mismatch_is_quarantined_without_retry() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"expected");
    let cache = fixture.cache();
    let artifact = snapshot_artifact("owner/repository", [("config.json", "expected")]);
    let published = cache
        .materialize(&artifact)
        .await
        .expect("initial snapshot");
    std::fs::write(published.path.join("config.json"), b"corrupt").expect("corrupt publication");
    let cached_file = ModelArtifactSource::HuggingFaceFile {
        repository: "owner/repository".to_owned(),
        revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        path: "config.json".to_owned(),
        provenance_digest: sha256_text("expected"),
    };
    fixture.cache_file(&cached_file, b"corrupt");
    fixture.set_response(b"wrong");
    let requests_before = fixture.get_requests.load(Ordering::SeqCst);

    let error = cache
        .materialize(&artifact)
        .await
        .expect_err("forced mismatch");

    assert!(matches!(error, ArtifactCacheError::DeclaredDigestMismatch));
    assert_eq!(
        fixture.get_requests.load(Ordering::SeqCst),
        requests_before + 1
    );
    assert!(matches!(
        cache.materialize(&artifact).await,
        Err(ArtifactCacheError::SnapshotRepairDeferred { .. })
    ));
    assert_eq!(
        fixture.get_requests.load(Ordering::SeqCst),
        requests_before + 1
    );
}

#[tokio::test]
async fn concurrent_snapshot_repair_callers_share_one_remote_batch() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"expected");
    let artifact = snapshot_artifact("owner/repository", [("config.json", "expected")]);
    let initial = fixture
        .cache()
        .materialize(&artifact)
        .await
        .expect("initial snapshot");
    let cached_file = ModelArtifactSource::HuggingFaceFile {
        repository: "owner/repository".to_owned(),
        revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        path: "config.json".to_owned(),
        provenance_digest: sha256_text("expected"),
    };
    fixture.cache_file(&cached_file, b"corrupt");
    fs::write(initial.path.join("config.json"), b"corrupt").expect("invalid publication");
    fixture.set_response(b"wrong");
    let requests = fixture.get_requests.load(Ordering::SeqCst);
    let first = fixture.cache();
    let second = fixture.cache();
    let (first_result, second_result) =
        tokio::join!(first.materialize(&artifact), second.materialize(&artifact));
    let outcomes = [first_result, second_result];
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| matches!(result, Err(ArtifactCacheError::DeclaredDigestMismatch)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| matches!(
                result,
                Err(ArtifactCacheError::SnapshotRepairDeferred { .. })
            ))
            .count(),
        1
    );
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), requests + 1);
}

#[tokio::test]
async fn first_acquisition_rechecks_a_peers_spent_allowance_before_next_remote_file() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"expected");
    let gate = fixture.pause_first_get();
    let artifact = snapshot_artifact(
        "owner/repository",
        [("config.json", "expected"), ("tokenizer.json", "expected")],
    );
    let cache = fixture.cache();
    let publication = cache.snapshot_path(&artifact).expect("publication path");
    let coordinate = ArtifactCacheError::artifact_coordinate(&artifact).expect("coordinate");
    let lock_path = cache.lock_path(&coordinate);
    let first = tokio::spawn(async move { cache.materialize(&artifact).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), gate.started.notified())
        .await
        .expect("first remote request");

    let peer = fixture.cache();
    let cancellation = CancellationToken::new();
    let lock = acquire_lock(lock_path.clone(), &cancellation, None)
        .await
        .expect("coordinate lock");
    fs::create_dir_all(&publication).expect("peer's incomplete publication");
    peer.reserve_snapshot_remote_repair(&lock_path.with_extension("repair.json"))
        .expect("peer's spent allowance");
    drop(lock);
    gate.release.notify_one();

    assert!(matches!(
        first.await.expect("first preparation task"),
        Err(ArtifactCacheError::SnapshotRepairDeferred { .. })
    ));
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), 1);
    assert!(snapshot_regular_files(&publication).is_empty());
}

#[tokio::test]
async fn snapshot_remote_repair_allowance_is_shared_and_local_recovery_still_works() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"expected");
    let artifact = snapshot_artifact("owner/repository", [("config.json", "expected")]);
    let initial = fixture
        .cache()
        .materialize(&artifact)
        .await
        .expect("initial snapshot");
    let cached_file = ModelArtifactSource::HuggingFaceFile {
        repository: "owner/repository".to_owned(),
        revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        path: "config.json".to_owned(),
        provenance_digest: sha256_text("expected"),
    };
    fixture.cache_file(&cached_file, b"corrupt");
    fs::write(initial.path.join("config.json"), b"corrupt").expect("invalid publication");
    fixture.set_response(b"wrong");
    let requests = fixture.get_requests.load(Ordering::SeqCst);
    let first = fixture.counting_cache();
    assert!(matches!(
        first.materialize(&artifact).await,
        Err(ArtifactCacheError::DeclaredDigestMismatch)
    ));
    let hashes = fixture.digest_count.load(Ordering::SeqCst);
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), requests + 1);

    for _ in 0..3 {
        assert!(matches!(
            fixture.counting_cache().materialize(&artifact).await,
            Err(ArtifactCacheError::SnapshotRepairDeferred { .. })
        ));
    }
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), requests + 1);
    assert_eq!(fixture.digest_count.load(Ordering::SeqCst), hashes);

    fixture.cache_file(&cached_file, b"expected");
    let repaired = fixture
        .cache()
        .materialize(&artifact)
        .await
        .expect("local recovery");
    assert!(repaired.replaced);
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), requests + 1);
    fs::write(repaired.path.join("config.json"), b"corrupt").expect("new invalidation");
    fixture.cache_file(&cached_file, b"corrupt");
    assert!(matches!(
        fixture.cache().materialize(&artifact).await,
        Err(ArtifactCacheError::SnapshotRepairDeferred { .. })
    ));
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), requests + 1);
}

#[tokio::test]
async fn snapshot_repair_reuses_shared_local_blob_for_multiple_files() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"expected");
    let artifact = snapshot_artifact(
        "owner/repository",
        [("config.json", "expected"), ("tokenizer.json", "expected")],
    );
    let initial = fixture
        .cache()
        .materialize(&artifact)
        .await
        .expect("initial snapshot");
    for name in ["config.json", "tokenizer.json"] {
        let cached_file = ModelArtifactSource::HuggingFaceFile {
            repository: "owner/repository".to_owned(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            path: name.to_owned(),
            provenance_digest: sha256_text("expected"),
        };
        fixture.cache_file(&cached_file, b"corrupt");
        fs::write(initial.path.join(name), b"corrupt").expect("invalidate file");
    }
    let requests = fixture.get_requests.load(Ordering::SeqCst);
    let repaired = fixture
        .cache()
        .materialize(&artifact)
        .await
        .expect("one repair batch");
    assert!(repaired.replaced);
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), requests + 1);
    assert_eq!(
        snapshot_regular_files(&repaired.path),
        BTreeMap::from([
            ("config.json".to_owned(), b"expected".to_vec()),
            ("tokenizer.json".to_owned(), b"expected".to_vec()),
        ])
    );
}

#[tokio::test]
async fn snapshot_negative_record_yields_to_another_process_publication() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"expected");
    let artifact = snapshot_artifact("owner/repository", [("config.json", "expected")]);
    let initial = fixture
        .cache()
        .materialize(&artifact)
        .await
        .expect("initial snapshot");
    let cached_file = ModelArtifactSource::HuggingFaceFile {
        repository: "owner/repository".to_owned(),
        revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        path: "config.json".to_owned(),
        provenance_digest: sha256_text("expected"),
    };
    fixture.cache_file(&cached_file, b"corrupt");
    fs::write(initial.path.join("config.json"), b"corrupt").expect("invalid publication");
    fixture.set_response(b"wrong");
    assert!(matches!(
        fixture.cache().materialize(&artifact).await,
        Err(ArtifactCacheError::DeclaredDigestMismatch)
    ));
    let requests = fixture.get_requests.load(Ordering::SeqCst);
    fs::write(initial.path.join("config.json"), b"expected").expect("peer repair");
    let recovered = fixture
        .cache()
        .materialize(&artifact)
        .await
        .expect("peer publication");
    assert!(!recovered.replaced);
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), requests);
    fs::write(recovered.path.join("config.json"), b"corrupt").expect("new invalidation");
    assert!(matches!(
        fixture.cache().materialize(&artifact).await,
        Err(ArtifactCacheError::SnapshotRepairDeferred { .. })
    ));
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), requests);
}

#[tokio::test]
async fn expired_snapshot_repair_allowance_admits_one_new_batch() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"expected");
    let artifact = snapshot_artifact("owner/repository", [("config.json", "expected")]);
    let initial = fixture
        .cache()
        .materialize(&artifact)
        .await
        .expect("initial snapshot");
    let cached_file = ModelArtifactSource::HuggingFaceFile {
        repository: "owner/repository".to_owned(),
        revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        path: "config.json".to_owned(),
        provenance_digest: sha256_text("expected"),
    };
    fixture.cache_file(&cached_file, b"corrupt");
    fs::write(initial.path.join("config.json"), b"corrupt").expect("invalid publication");
    fixture.set_response(b"wrong");
    assert!(matches!(
        fixture.cache().materialize(&artifact).await,
        Err(ArtifactCacheError::DeclaredDigestMismatch)
    ));
    let coordinate = ArtifactCacheError::artifact_coordinate(&artifact).expect("coordinate");
    let state_path = fixture
        .cache()
        .lock_path(&coordinate)
        .with_extension("repair.json");
    let mut state = snapshot_repair_state(&state_path).expect("repair record");
    state.spent_at_seconds =
        Some(snapshot_repair_now().expect("clock") - SnapshotRepairState::RETRY_WINDOW_SECONDS);
    write_verification_evidence(&state_path, &state).expect("expire allowance");
    fixture.set_response(b"expected");
    let requests = fixture.get_requests.load(Ordering::SeqCst);
    let repaired = fixture
        .cache()
        .materialize(&artifact)
        .await
        .expect("renewed batch");
    assert!(repaired.replaced);
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), requests + 1);
}

#[tokio::test]
async fn snapshot_network_failure_keeps_its_shared_repair_reservation() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"expected");
    let artifact = snapshot_artifact("owner/repository", [("config.json", "expected")]);
    let initial = fixture
        .cache()
        .materialize(&artifact)
        .await
        .expect("initial snapshot");
    let cached_file = ModelArtifactSource::HuggingFaceFile {
        repository: "owner/repository".to_owned(),
        revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        path: "config.json".to_owned(),
        provenance_digest: sha256_text("expected"),
    };
    fixture.cache_file(&cached_file, b"corrupt");
    fs::write(initial.path.join("config.json"), b"corrupt").expect("invalid publication");
    fixture.set_failure();
    assert!(
        fixture
            .counting_cache()
            .materialize(&artifact)
            .await
            .is_err()
    );
    let requests = fixture.get_requests.load(Ordering::SeqCst);
    let hashes = fixture.digest_count.load(Ordering::SeqCst);
    for _ in 0..3 {
        assert!(matches!(
            fixture.counting_cache().materialize(&artifact).await,
            Err(ArtifactCacheError::SnapshotRepairDeferred { .. })
        ));
    }
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), requests);
    assert_eq!(fixture.digest_count.load(Ordering::SeqCst), hashes);
}

#[tokio::test]
async fn interrupted_first_publication_spends_the_shared_repair_allowance() {
    let fixture = Fixture::new().await;
    fixture.set_failure();
    let artifact = snapshot_artifact("owner/repository", [("config.json", "expected")]);
    let cache = fixture.cache();
    let publication = cache.snapshot_path(&artifact).expect("publication path");
    fs::create_dir_all(&publication).expect("incomplete publication root");
    assert!(cache.materialize(&artifact).await.is_err());
    let requests = fixture.get_requests.load(Ordering::SeqCst);
    fixture.set_response(b"expected");
    assert!(matches!(
        fixture.cache().materialize(&artifact).await,
        Err(ArtifactCacheError::SnapshotRepairDeferred { .. })
    ));
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), requests);
    assert!(snapshot_regular_files(&publication).is_empty());
}

#[tokio::test]
async fn repeated_snapshot_mismatch_is_reacquired_once_before_quarantine() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"wrong");
    let cache = fixture.cache();
    let artifact = snapshot_artifact("owner/repository", [("config.json", "expected")]);
    let publication = cache.snapshot_path(&artifact).expect("publication path");

    let error = cache
        .materialize(&artifact)
        .await
        .expect_err("bounded mismatch");

    assert!(matches!(error, ArtifactCacheError::DeclaredDigestMismatch));
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), 2);
    assert!(
        !cache
            .snapshot_publication_is_valid(&artifact, &publication, &CancellationToken::new())
            .await
            .expect("mismatched publication validation")
    );
    assert!(matches!(
        cache.materialize(&artifact).await,
        Err(ArtifactCacheError::SnapshotRepairDeferred { .. })
    ));
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn distinct_snapshot_manifests_at_one_revision_publish_independently() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"shared");
    let cache = fixture.cache();
    let first = snapshot_artifact("owner/repository", [("config.json", "shared")]);
    let second = snapshot_artifact("owner/repository", [("tokenizer/config.json", "shared")]);

    let first = cache.materialize(&first).await.expect("first manifest");
    let second = cache.materialize(&second).await.expect("second manifest");

    assert_ne!(first.path, second.path);
    assert_eq!(
        snapshot_regular_files(&first.path),
        BTreeMap::from([("config.json".to_owned(), b"shared".to_vec())])
    );
    assert_eq!(
        snapshot_regular_files(&second.path),
        BTreeMap::from([("tokenizer/config.json".to_owned(), b"shared".to_vec(),)])
    );
}

#[tokio::test]
async fn repairs_corrupt_or_undeclared_snapshot_entries_atomically() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"shared");
    let cache = fixture.cache();
    let artifact = snapshot_artifact(
        "owner/repository",
        [
            ("config.json", "shared"),
            ("tokenizer/config.json", "shared"),
        ],
    );
    let initial = cache
        .materialize(&artifact)
        .await
        .expect("initial snapshot");
    std::fs::write(initial.path.join("config.json"), b"corrupt").expect("corrupt file");
    std::fs::write(initial.path.join("undeclared.json"), b"undeclared").expect("undeclared file");

    let repaired = cache
        .materialize(&artifact)
        .await
        .expect("repaired snapshot");

    assert_eq!(repaired.path, initial.path);
    assert!(repaired.replaced);
    assert_eq!(
        snapshot_regular_files(&repaired.path),
        BTreeMap::from([
            ("config.json".to_owned(), b"shared".to_vec()),
            ("tokenizer/config.json".to_owned(), b"shared".to_vec()),
        ])
    );
    assert_eq!(
        std::fs::read_dir(repaired.path.parent().expect("publication parent"))
            .expect("publication parent")
            .count(),
        1
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn snapshot_repair_preserves_consumer_directory_and_healthy_entries() {
    use std::os::fd::AsRawFd;

    let fixture = Fixture::new().await;
    fixture.set_response(b"shared");
    let cache = fixture.counting_cache();
    let artifact = snapshot_artifact(
        "owner/repository",
        [
            ("config.json", "shared"),
            ("tokenizer/config.json", "shared"),
        ],
    );
    let initial = cache
        .materialize(&artifact)
        .await
        .expect("initial snapshot");
    let directory = File::open(&initial.path).expect("consumer directory handle");
    let root_inode = directory.metadata().expect("root metadata").ino();
    let healthy = initial.path.join("tokenizer/config.json");
    let healthy_inode = fs::metadata(&healthy).expect("healthy metadata").ino();
    replace_file(&initial.path.join("config.json"), b"corrupt");

    let repaired = cache
        .materialize(&artifact)
        .await
        .expect("repaired snapshot");

    let retained = format!("/proc/self/fd/{}", directory.as_raw_fd());
    assert_eq!(
        fs::read(format!("{retained}/config.json")).expect("reopen repaired file"),
        b"shared"
    );
    assert_eq!(
        fs::read(format!("{retained}/tokenizer/config.json")).expect("reopen healthy file"),
        b"shared"
    );
    assert_eq!(
        fs::metadata(&repaired.path)
            .expect("published metadata")
            .ino(),
        root_inode
    );
    assert_eq!(
        fs::metadata(&healthy).expect("healthy metadata").ino(),
        healthy_inode
    );
    assert!(repaired.replaced);
    assert_eq!(
        fs::read_dir(repaired.path.parent().expect("coordinate parent"))
            .expect("coordinate entries")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".trash"))
            .count(),
        0
    );
    let hashes_after_repair = fixture.digest_count.load(Ordering::SeqCst);
    let warm = cache
        .materialize(&artifact)
        .await
        .expect("warm snapshot reuse");
    assert!(!warm.replaced);
    assert_eq!(
        fixture.digest_count.load(Ordering::SeqCst),
        hashes_after_repair
    );
}

#[tokio::test]
async fn snapshot_repair_reuses_local_files_across_cache_instances() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"shared");
    let artifact = snapshot_artifact(
        "owner/repository",
        [
            ("config.json", "shared"),
            ("tokenizer/config.json", "shared"),
        ],
    );
    let initial = fixture
        .cache()
        .materialize(&artifact)
        .await
        .expect("initial snapshot");
    let requests = fixture.get_requests.load(Ordering::SeqCst);
    let healthy = initial.path.join("tokenizer/config.json");
    let healthy_inode = fs::metadata(&healthy).expect("healthy metadata").ino();
    replace_file(&initial.path.join("config.json"), b"corrupt");

    let repaired = fixture
        .cache()
        .materialize(&artifact)
        .await
        .expect("local repair");
    let reused = fixture
        .cache()
        .materialize(&artifact)
        .await
        .expect("independent reuse");

    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), requests);
    assert_eq!(
        fs::metadata(&healthy).expect("healthy metadata").ino(),
        healthy_inode
    );
    assert_eq!(repaired.path, reused.path);
    assert!(repaired.replaced);
    assert!(!reused.replaced);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn concurrent_snapshot_repairs_preserve_a_retained_consumer_directory() {
    use std::os::fd::AsRawFd;

    let fixture = Fixture::new().await;
    fixture.set_response(b"shared");
    let artifact = snapshot_artifact(
        "owner/repository",
        [
            ("config.json", "shared"),
            ("tokenizer/config.json", "shared"),
        ],
    );
    let initial = fixture
        .cache()
        .materialize(&artifact)
        .await
        .expect("initial snapshot");
    let directory = File::open(&initial.path).expect("consumer directory handle");
    fs::write(initial.path.join("config.json"), b"corrupt").expect("invalid entry");
    let first = fixture.cache();
    let second = fixture.cache();

    let (first, second) = tokio::join!(first.materialize(&artifact), second.materialize(&artifact));

    assert_eq!(first.expect("first writer").path, initial.path);
    assert_eq!(second.expect("second writer").path, initial.path);
    let retained = format!("/proc/self/fd/{}", directory.as_raw_fd());
    assert_eq!(
        fs::read(format!("{retained}/config.json")).expect("reopen repaired entry"),
        b"shared"
    );
    assert_eq!(
        fs::read(format!("{retained}/tokenizer/config.json")).expect("reopen healthy entry"),
        b"shared"
    );
}

#[tokio::test]
async fn abandoned_snapshot_writer_lock_allows_repair() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"shared");
    let cache = fixture.cache();
    let artifact = snapshot_artifact("owner/repository", [("config.json", "shared")]);
    let initial = cache
        .materialize(&artifact)
        .await
        .expect("initial snapshot");
    fs::write(initial.path.join("config.json"), b"corrupt").expect("invalid file");
    let coordinate = ArtifactCacheError::artifact_coordinate(&artifact).expect("coordinate");
    let lock_path = cache.lock_path(&coordinate);
    let independent = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("independent lock descriptor");
    let (acquired, received) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let holder = tokio::spawn(async move {
        let _lock = acquire_lock(lock_path, &CancellationToken::new(), None)
            .await
            .expect("writer lock");
        acquired.send(()).expect("signal acquired lock");
        released.await.expect("release retained writer");
        panic!("simulate writer termination");
    });
    received.await.expect("writer acquired lock");
    assert!(matches!(
        independent.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));

    release.send(()).expect("release retained writer");
    assert!(holder.await.expect_err("writer terminated").is_panic());
    independent
        .try_lock()
        .expect("probe abandoned writer lock release");
    drop(independent);

    let repaired = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        cache.materialize(&artifact),
    )
    .await
    .expect("snapshot repair deadlock watchdog")
    .expect("repair after writer termination");
    assert_eq!(
        fs::read(repaired.path.join("config.json")).expect("repaired file"),
        b"shared"
    );
}

#[tokio::test]
async fn complete_snapshot_before_marker_recovers_without_replacing_directory() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"shared");
    let cache = fixture.counting_cache();
    let artifact = snapshot_artifact("owner/repository", [("config.json", "shared")]);
    let initial = cache
        .materialize(&artifact)
        .await
        .expect("initial snapshot");
    let root_inode = fs::metadata(&initial.path).expect("root metadata").ino();
    let staged = fixture.root.join("verified-replacement");
    fs::write(&staged, b"shared").expect("verified replacement bytes");
    fs::rename(&staged, initial.path.join("config.json"))
        .expect("simulate interruption before marker persistence");
    let hashes_before = fixture.digest_count.load(Ordering::SeqCst);

    let accepted = cache
        .materialize(&artifact)
        .await
        .expect("rehash complete tree");
    assert!(!accepted.replaced);
    assert_eq!(
        fixture.digest_count.load(Ordering::SeqCst),
        hashes_before + 1
    );
    assert_eq!(
        fs::metadata(&accepted.path).expect("root metadata").ino(),
        root_inode
    );
    assert!(
        !cache
            .materialize(&artifact)
            .await
            .expect("warm reuse")
            .replaced
    );
    assert_eq!(
        fixture.digest_count.load(Ordering::SeqCst),
        hashes_before + 1
    );
}

#[tokio::test]
async fn interrupted_snapshot_cleanup_remains_unaccepted_until_entries_are_exact() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"shared");
    let cache = fixture.cache();
    let artifact = snapshot_artifact("owner/repository", [("config.json", "shared")]);
    let initial = cache
        .materialize(&artifact)
        .await
        .expect("initial snapshot");
    let root_inode = fs::metadata(&initial.path).expect("root metadata").ino();
    fs::write(initial.path.join("first.extra"), b"first").expect("first extra");
    fs::write(initial.path.join("second.extra"), b"second").expect("second extra");
    fs::remove_file(initial.path.join("first.extra"))
        .expect("simulate interruption during undeclared cleanup");

    assert!(
        !cache
            .snapshot_publication_is_valid(&artifact, &initial.path, &CancellationToken::new())
            .await
            .expect("incomplete cleanup validation")
    );
    let repaired = cache.materialize(&artifact).await.expect("finish cleanup");
    assert_eq!(
        fs::metadata(&repaired.path).expect("root metadata").ino(),
        root_inode
    );
    assert!(repaired.replaced);
    assert_eq!(
        snapshot_regular_files(&repaired.path),
        BTreeMap::from([("config.json".to_owned(), b"shared".to_vec())])
    );
}

#[tokio::test]
async fn interrupted_per_file_repair_cannot_accept_an_incomplete_tree() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"shared");
    let cache = fixture.cache();
    let artifact = snapshot_artifact(
        "owner/repository",
        [
            ("config.json", "shared"),
            ("tokenizer/config.json", "shared"),
            ("tokenizer/extra.json", "shared"),
        ],
    );
    let initial = cache
        .materialize(&artifact)
        .await
        .expect("initial snapshot");
    let root_inode = fs::metadata(&initial.path).expect("root metadata").ino();
    replace_file(&initial.path.join("config.json"), b"corrupt");
    replace_file(&initial.path.join("tokenizer/extra.json"), b"corrupt");
    let staged = fixture.root.join("staged-replacement");
    fs::write(&staged, b"shared").expect("verified replacement bytes");
    fs::rename(&staged, initial.path.join("config.json"))
        .expect("simulate interruption after first atomic replacement");

    assert!(
        !cache
            .snapshot_publication_is_valid(&artifact, &initial.path, &CancellationToken::new())
            .await
            .expect("incomplete repair validation")
    );
    assert_eq!(
        fs::read(initial.path.join("tokenizer/config.json")).expect("healthy entry"),
        b"shared"
    );
    let repaired = cache.materialize(&artifact).await.expect("finish repair");
    assert_eq!(
        fs::metadata(&repaired.path).expect("root metadata").ino(),
        root_inode
    );
    assert_eq!(
        snapshot_regular_files(&repaired.path),
        BTreeMap::from([
            ("config.json".to_owned(), b"shared".to_vec()),
            ("tokenizer/config.json".to_owned(), b"shared".to_vec()),
            ("tokenizer/extra.json".to_owned(), b"shared".to_vec()),
        ])
    );
}

#[tokio::test]
async fn snapshot_repair_fails_closed_on_symlinked_ancestor_and_root() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"shared");
    let cache = fixture.cache();
    let artifact = snapshot_artifact("owner/repository", [("tokenizer/config.json", "shared")]);
    let publication = cache.snapshot_path(&artifact).expect("publication path");
    let outside = fixture.root.join("outside");
    fs::create_dir_all(&outside).expect("outside directory");
    fs::write(outside.join("config.json"), b"outside").expect("outside content");
    fs::create_dir_all(&publication).expect("publication root");
    std::os::unix::fs::symlink(&outside, publication.join("tokenizer"))
        .expect("symlinked ancestor");

    assert!(matches!(
        cache.materialize(&artifact).await,
        Err(ArtifactCacheError::StructuralConflict {
            kind: "declared directory is not a directory"
        })
    ));
    assert_eq!(
        fs::read(outside.join("config.json")).expect("outside remains"),
        b"outside"
    );
    fs::remove_file(publication.join("tokenizer")).expect("remove test symlink");
    fs::remove_dir(&publication).expect("remove empty test root");
    std::os::unix::fs::symlink(&outside, &publication).expect("symlinked root");

    assert!(matches!(
        cache.materialize(&artifact).await,
        Err(ArtifactCacheError::StructuralConflict {
            kind: "publication root is not a directory"
        })
    ));
    assert_eq!(
        fs::read(outside.join("config.json")).expect("outside remains"),
        b"outside"
    );
    fs::remove_file(&publication).expect("remove test root symlink");
    fs::create_dir(&publication).expect("restore publication root");
    fs::write(publication.join("tokenizer"), b"wrong type").expect("wrong ancestor type");
    assert!(matches!(
        cache.materialize(&artifact).await,
        Err(ArtifactCacheError::StructuralConflict {
            kind: "declared directory is not a directory"
        })
    ));
    assert_eq!(
        fs::read(publication.join("tokenizer")).expect("wrong type preserved"),
        b"wrong type"
    );
}

#[tokio::test]
async fn repairs_symlinks_without_exposing_their_targets() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"shared");
    let cache = fixture.cache();
    let artifact = snapshot_artifact("owner/repository", [("config.json", "shared")]);
    let initial = cache
        .materialize(&artifact)
        .await
        .expect("initial snapshot");
    let outside = fixture.root.join("outside");
    std::fs::write(&outside, b"outside").expect("outside file");
    std::fs::remove_file(initial.path.join("config.json")).expect("remove published file");
    std::os::unix::fs::symlink(&outside, initial.path.join("config.json"))
        .expect("published symlink");

    let repaired = cache
        .materialize(&artifact)
        .await
        .expect("repaired snapshot");

    assert!(repaired.replaced);
    assert_eq!(
        snapshot_regular_files(&repaired.path),
        BTreeMap::from([("config.json".to_owned(), b"shared".to_vec())])
    );
    assert_eq!(std::fs::read(&outside).expect("outside target"), b"outside");
}

#[tokio::test]
async fn concurrent_snapshot_callers_observe_one_complete_publication() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"shared");
    let cache = Arc::new(fixture.cache());
    let artifact = snapshot_artifact(
        "owner/repository",
        [
            ("config.json", "shared"),
            ("tokenizer/config.json", "shared"),
        ],
    );

    let (first, second) = tokio::join!(cache.materialize(&artifact), cache.materialize(&artifact));
    let first = first.expect("first snapshot");
    let second = second.expect("second snapshot");

    assert_eq!(first.path, second.path);
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        snapshot_regular_files(&first.path),
        BTreeMap::from([
            ("config.json".to_owned(), b"shared".to_vec()),
            ("tokenizer/config.json".to_owned(), b"shared".to_vec()),
        ])
    );
    assert_eq!(
        std::fs::read_dir(first.path.parent().expect("publication parent"))
            .expect("publication parent")
            .count(),
        1
    );
}

#[tokio::test]
async fn cache_probe_waits_for_an_in_flight_pointer_rewrite() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"shared");
    let gate = Arc::new(PointerRewriteGate::default());
    let cache = fixture.cache().with_pointer_rewrite(gate.clone());
    let repository = "owner/repository";
    let revision = "0123456789abcdef0123456789abcdef01234567";
    let cancellation = CancellationToken::new();

    let download = tokio::spawn({
        let cache = cache.clone();
        let cancellation = cancellation.clone();
        async move {
            let _file_lock = cache
                .lock_hugging_face_file(repository, revision, "config.json", &cancellation)
                .await?;
            cache
                .fetch_hugging_face_file(
                    repository,
                    revision,
                    "config.json",
                    false,
                    &cancellation,
                    cache.download_hugging_face_file(
                        repository,
                        revision,
                        "config.json",
                        false,
                        &cancellation,
                        DownloadReports {
                            progress: None,
                            output: &discarded(),
                        },
                    ),
                )
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), gate.opened.notified())
        .await
        .expect("pointer rewrite gap opened");
    let probed = cache
        .cached_hugging_face_file(repository, revision, "config.json", &cancellation)
        .await
        .expect("probe during the pointer rewrite");
    let downloaded = download.await.expect("download task").expect("download");

    assert!(probed.is_some(), "probe observed the rewrite gap");
    assert_eq!(probed, downloaded);
    assert_eq!(
        std::fs::read(probed.expect("probed path")).expect("cached file"),
        b"shared"
    );
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelled_snapshot_preparation_does_not_publish_state() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"shared");
    let cache = fixture.cache();
    let artifact = snapshot_artifact("owner/repository", [("config.json", "shared")]);
    let publication = cache.snapshot_path(&artifact).expect("publication path");
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let error = cache
        .materialize_cancellable(
            &artifact,
            &cancellation,
            None,
            DownloadReports {
                progress: None,
                output: &discarded(),
            },
        )
        .await
        .expect_err("cancelled snapshot");

    assert!(matches!(error, ArtifactCacheError::Cancelled));
    assert!(!publication.exists());
}

#[tokio::test]
async fn cancellation_drains_snapshot_publication_without_exposing_temporary_state() {
    let fixture = Fixture::new().await;
    let gate = Arc::new(BlockingWorkGate::default());
    let mut cache = fixture.cache();
    cache.blocking_work = Some(gate.clone());
    let artifact = snapshot_artifact("owner/repository", [("config.json", "shared")]);
    let cached_file = ModelArtifactSource::HuggingFaceFile {
        repository: "owner/repository".to_owned(),
        revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        path: "config.json".to_owned(),
        provenance_digest: sha256_text("shared"),
    };
    fixture.cache_file(&cached_file, b"shared");
    let publication = cache.snapshot_path(&artifact).expect("publication path");
    let cancellation = CancellationToken::new();
    let preparation = tokio::spawn({
        let cache = cache.clone();
        let artifact = artifact.clone();
        let cancellation = cancellation.clone();
        async move {
            cache
                .materialize_cancellable(
                    &artifact,
                    &cancellation,
                    None,
                    DownloadReports {
                        progress: None,
                        output: &discarded(),
                    },
                )
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), gate.wait_started())
        .await
        .expect("snapshot publication started");

    cancellation.cancel();
    tokio::task::yield_now().await;
    assert!(!preparation.is_finished());
    gate.release();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), preparation)
        .await
        .expect("snapshot publication drained")
        .expect("snapshot publication task");

    assert!(matches!(result, Err(ArtifactCacheError::Cancelled)));
    assert!(!publication.exists());
    assert_eq!(
        std::fs::read_dir(publication.parent().expect("publication parent"))
            .expect("publication parent")
            .count(),
        0
    );
}
#[tokio::test]
async fn missing_snapshot_file_never_publishes_a_tree() {
    let fixture = Fixture::new().await;
    let cache = fixture.cache();
    let artifact = snapshot_artifact(
        "owner/repository",
        [
            ("config.json", "shared"),
            ("tokenizer/config.json", "shared"),
        ],
    );
    let cached_file = ModelArtifactSource::HuggingFaceFile {
        repository: "owner/repository".to_owned(),
        revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        path: "config.json".to_owned(),
        provenance_digest: sha256_text("shared"),
    };
    fixture.cache_file(&cached_file, b"shared");
    fixture.set_failure();
    let publication = cache.snapshot_path(&artifact).expect("publication path");

    let error = cache
        .materialize(&artifact)
        .await
        .expect_err("missing snapshot file");

    assert_eq!(
        error.to_string(),
        "Model artifact cache failure: Hugging Face snapshot file download failed for owner/repository@0123456789abcdef0123456789abcdef01234567"
    );
    assert!(!publication.exists());
}
type HubResponse = (u16, Vec<u8>);

#[tokio::test]
async fn prepares_main_projector_and_mtp_assets_from_the_hugging_face_cache() {
    let fixture = Fixture::new().await;
    let cache = fixture.cache();
    let launch = launch("model", "projector", "draft");
    fixture.cache_file(&artifact("model"), b"model");
    fixture.cache_file(&artifact("projector"), b"projector");
    fixture.cache_file(&artifact("draft"), b"draft");

    let prepared = cache.prepare(&launch).await.expect("prepared assets");

    assert_eq!(
        std::fs::read(prepared.artifacts.path("model")).expect("model"),
        b"model"
    );
    assert_eq!(
        std::fs::read(prepared.artifacts.path("vision-projector")).expect("projector"),
        b"projector"
    );
    assert_eq!(
        std::fs::read(prepared.artifacts.path("mtp-draft")).expect("draft"),
        b"draft"
    );
    assert!(!prepared.replaced);
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn prepares_a_nested_repository_file_as_its_canonical_blob() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"draft");
    let draft = ModelArtifactSource::HuggingFaceFile {
        repository: "owner/repository".to_owned(),
        revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        path: "MTP/mtp-draft.gguf".to_owned(),
        provenance_digest: sha256_text("draft"),
    };
    let launch = BTreeMap::from([("mtp-draft".to_owned(), draft.clone())]);

    let prepared = fixture
        .cache()
        .prepare(&launch)
        .await
        .expect("downloaded draft");

    let blob = prepared.artifacts.path("mtp-draft");
    assert_eq!(std::fs::read(blob).expect("draft bytes"), b"draft");
    assert_eq!(
        blob.parent().and_then(Path::file_name),
        Some(std::ffi::OsStr::new("blobs"))
    );
    let pointer = fixture.cache_file_path(&draft);
    assert!(pointer.ends_with("MTP/mtp-draft.gguf"), "{pointer:?}");
    assert!(
        std::fs::symlink_metadata(&pointer)
            .expect("nested pointer")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::canonicalize(&pointer).expect("pointer target"),
        *blob
    );
    assert!(
        fixture
            .request_path()
            .await
            .ends_with("/MTP/mtp-draft.gguf")
    );
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), 1);

    let requests = fixture.requests.load(Ordering::SeqCst);
    let warm = fixture.cache().prepare(&launch).await.expect("warm draft");

    assert_eq!(warm.artifacts.path("mtp-draft"), blob);
    assert_eq!(fixture.requests.load(Ordering::SeqCst), requests);
}

#[tokio::test]
async fn prepares_only_the_profile_selected_artifact_at_distinct_cache_coordinates() {
    let fixture = Fixture::new().await;
    let cache = fixture.cache();
    let fast_start = launch_with_artifact(profile_artifact("gemma-4-E2B-it-Q4_K_M", "Q4_K_M"));
    let higher_precision = launch_with_artifact(profile_artifact("gemma-4-E2B-it-Q8_0", "Q8_0"));
    fixture.cache_file(&fast_start["model"], b"Q4_K_M");

    let prepared = cache
        .prepare(&fast_start)
        .await
        .expect("fast-start artifact");
    let higher_precision_paths = cache
        .expected_paths(&higher_precision)
        .expect("higher-precision paths");

    assert_eq!(
        prepared.artifacts.path("model"),
        &fixture.cache_file_path(&fast_start["model"])
    );
    assert_eq!(
        std::fs::read(prepared.artifacts.path("model")).expect("Q4_K_M bytes"),
        b"Q4_K_M"
    );
    assert_ne!(
        prepared.artifacts.path("model"),
        higher_precision_paths.path("model")
    );
    assert!(!higher_precision_paths.path("model").exists());
    assert!(
        fixture
            .verification_marker_path(&fast_start["model"])
            .is_file()
    );
    assert!(
        !fixture
            .verification_marker_path(&higher_precision["model"])
            .exists()
    );
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 0);

    fixture.cache_file(&higher_precision["model"], b"Q8_0");
    let higher_precision_prepared = cache
        .prepare(&higher_precision)
        .await
        .expect("higher-precision artifact");

    assert_eq!(
        higher_precision_prepared.artifacts.path("model"),
        higher_precision_paths.path("model")
    );
    assert_eq!(
        std::fs::read(higher_precision_prepared.artifacts.path("model")).expect("Q8_0 bytes"),
        b"Q8_0"
    );
    assert!(
        fixture
            .verification_marker_path(&higher_precision["model"])
            .is_file()
    );
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn sibling_failure_cancels_only_child_and_drains_blocking_work_before_retry() {
    let fixture = Fixture::new().await;
    let gate = Arc::new(BlockingWorkGate::default());
    let cache = fixture
        .cache()
        .with_blocking_work_failure("projector.gguf", gate.clone());
    let valid = launch("model", "projector", "draft");
    fixture.cache_file(&artifact("model"), b"model");
    fixture.cache_file(&artifact("projector"), b"projector");
    fixture.cache_file(&artifact("draft"), b"draft");
    let parent = CancellationToken::new();
    let preparation = tokio::spawn({
        let cache = cache.clone();
        let parent = parent.clone();
        let valid = valid.clone();
        async move { cache.prepare_cancellable(&valid, &parent, None).await }
    });
    // Bound deadlocks without imposing a latency target on parallel test execution.
    let watchdog = std::time::Duration::from_secs(10);
    tokio::time::timeout(watchdog, gate.wait_started())
        .await
        .expect("blocking digest started");
    tokio::task::yield_now().await;

    assert!(
        !preparation.is_finished(),
        "preparation must drain owned blocking work after sibling failure"
    );
    assert!(!parent.is_cancelled());

    gate.release();
    let error = tokio::time::timeout(watchdog, preparation)
        .await
        .expect("drained preparation")
        .expect("preparation task")
        .expect_err("transient projector failure");
    assert!(!error.is_cancelled());
    assert!(!parent.is_cancelled());

    let mut retry = valid.clone();
    for artifact in retry.values_mut() {
        if let ModelArtifactSource::HuggingFaceFile {
            provenance_digest, ..
        } = artifact
        {
            provenance_digest.clear();
        }
    }
    // Keep the deadline focused on fresh-child and coordinate-lock progress; durable
    // verification-marker writes are covered separately and have no one-second contract.
    let prepared = tokio::time::timeout(watchdog, cache.prepare_cancellable(&retry, &parent, None))
        .await
        .expect("fresh child retry completion")
        .expect("fresh child retry");
    assert_eq!(
        std::fs::read(prepared.artifacts.path("model")).expect("model"),
        b"model"
    );
    assert_eq!(
        std::fs::read(prepared.artifacts.path("vision-projector")).expect("projector"),
        b"projector"
    );
    assert_eq!(
        std::fs::read(prepared.artifacts.path("mtp-draft")).expect("draft"),
        b"draft"
    );
}

#[tokio::test]
async fn accepts_an_omitted_digest_from_a_cache_hit() {
    let fixture = Fixture::new().await;
    let cache = fixture.cache();
    let mut artifact = artifact("model");
    let ModelArtifactSource::HuggingFaceFile {
        provenance_digest, ..
    } = &mut artifact
    else {
        panic!("file artifact");
    };
    provenance_digest.clear();
    fixture.cache_file(&artifact, b"cache value");

    assert_eq!(
        cache.materialize(&artifact).await.expect("cache hit").path,
        fixture.cache_file_path(&artifact)
    );
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn hashes_the_initial_download_before_preparation_succeeds() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"model");

    fixture
        .counting_cache()
        .materialize(&artifact("model"))
        .await
        .expect("verified download");

    assert_eq!(fixture.digest_count.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reports_cumulative_progress_for_an_invocation_download() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"model");
    let snapshots = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = snapshots.clone();

    fixture
        .cache()
        .prepare_cancellable(
            &launch_with_artifact(artifact("model")),
            &CancellationToken::new(),
            Some(Arc::new(move |progress| {
                observed.lock().expect("progress snapshots").push(progress);
            })),
        )
        .await
        .expect("downloaded launch");

    let snapshots = snapshots.lock().expect("progress snapshots");
    let final_snapshot = snapshots.last().expect("final progress snapshot");
    assert_eq!(final_snapshot.completed, 5);
    assert_eq!(final_snapshot.total, Some(5));
}

#[tokio::test]
async fn reports_each_download_as_step_output_and_warm_reuse_as_already_prepared() {
    let fixture = Fixture::new().await;
    fixture.set_response(b"model");
    let manager = crate::ArtifactManager::from_parts(fixture.cache(), None);
    let artifacts = launch_with_artifact(artifact("model"));
    let prepare = async |recorded: &RecordedOutput| {
        manager
            .prepare_model(
                &artifacts,
                &CancellationToken::new(),
                None,
                None,
                &recorded.output(),
            )
            .await
    };

    let cold = RecordedOutput::new();
    prepare(&cold).await.expect("downloaded artifact");
    let warm = RecordedOutput::new();
    prepare(&warm).await.expect("reused artifact");

    let description =
        "owner/repository/model.gguf@0123456789abcdef0123456789abcdef01234567".to_owned();
    assert_eq!(
        cold.kinds(),
        [
            (
                PreparationStep::ModelDownload,
                PreparationEventKind::Started {
                    artifact: description.clone()
                }
            ),
            download_line("downloading model.gguf from owner/repository"),
            download_line("downloaded model.gguf"),
            (
                PreparationStep::ModelDownload,
                PreparationEventKind::Finished {
                    duration: std::time::Duration::ZERO
                }
            ),
        ]
    );
    assert_eq!(
        warm.kinds(),
        [(
            PreparationStep::ModelDownload,
            PreparationEventKind::AlreadyPrepared {
                artifact: description
            }
        )]
    );
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), 1);
}

#[test]
fn download_progress_lines_report_each_new_tenth_of_a_file_once() {
    let recorded = RecordedOutput::new();
    let handler = HuggingFaceDownloadProgress {
        slot: None,
        output: recorded
            .output()
            .step(PreparationStep::ModelDownload, "model.gguf"),
        path: "model.gguf".to_owned(),
        total: AtomicU64::new(0),
        reported_tenths: AtomicU64::new(0),
    };
    let progress = |completed: u64| {
        ProgressEvent::Download(DownloadEvent::Progress {
            files: vec![hf_hub::progress::FileProgress {
                filename: "model.gguf".to_owned(),
                bytes_completed: completed,
                total_bytes: 3 * 1024 * 1024 * 1024,
                status: hf_hub::progress::FileStatus::InProgress,
            }],
        })
    };
    let gib = 1024 * 1024 * 1024;

    handler.on_progress(&ProgressEvent::Download(DownloadEvent::Start {
        total_files: 1,
        total_bytes: 3 * gib,
    }));
    for completed in [gib / 100, gib / 3, gib / 2, gib, 3 * gib] {
        handler.on_progress(&progress(completed));
    }
    handler.on_progress(&ProgressEvent::Download(DownloadEvent::AggregateProgress {
        bytes_completed: gib / 2,
        total_bytes: 3 * gib,
        bytes_per_sec: None,
    }));

    assert_eq!(
        recorded.kinds(),
        [
            (
                PreparationStep::ModelDownload,
                PreparationEventKind::Started {
                    artifact: "model.gguf".to_owned()
                }
            ),
            download_line("model.gguf: 10% of 3.0 GiB"),
            download_line("model.gguf: 30% of 3.0 GiB"),
        ]
    );
}

#[tokio::test]
async fn persists_successful_verification_across_cache_construction_and_safety_scans() {
    let fixture = Fixture::new().await;
    let artifact = artifact("model");
    fixture.cache_file(&artifact, b"model");

    fixture
        .counting_cache()
        .materialize(&artifact)
        .await
        .expect("initial verification");
    assert_eq!(fixture.digest_count.load(Ordering::SeqCst), 1);

    for _ in 0..6 {
        fixture
            .counting_cache()
            .materialize(&artifact)
            .await
            .expect("warm safety scan");
    }

    assert_eq!(fixture.digest_count.load(Ordering::SeqCst), 1);
    assert!(fixture.verification_marker_path(&artifact).is_file());
}

#[tokio::test]
async fn repairs_modified_cache_content_then_refreshes_the_verification_marker() {
    let fixture = Fixture::new().await;
    let artifact = artifact("model");
    fixture.cache_file(&artifact, b"model");
    fixture
        .counting_cache()
        .materialize(&artifact)
        .await
        .expect("initial verification");
    std::fs::write(fixture.cache_file_path(&artifact), b"corrupt").expect("in-place corruption");
    fixture.set_response(b"model");

    let repaired = fixture
        .counting_cache()
        .materialize(&artifact)
        .await
        .expect("repaired artifact");

    assert!(repaired.replaced);
    assert_eq!(fixture.digest_count.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), 1);
    fixture
        .counting_cache()
        .materialize(&artifact)
        .await
        .expect("warm repaired artifact");
    assert_eq!(fixture.digest_count.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn rehashes_identical_content_when_the_cache_file_is_replaced() {
    let root = temp_root("replacement");
    let artifact = artifact("model");
    let path = root
        .join("models--owner--repository")
        .join("snapshots")
        .join("0123456789abcdef0123456789abcdef01234567")
        .join("model.gguf");
    std::fs::create_dir_all(path.parent().expect("snapshot parent")).expect("snapshot parent");
    std::fs::write(&path, b"model").expect("cached artifact");
    let count = Arc::new(AtomicUsize::new(0));
    let cache = ModelArtifactCache::for_test_with_digest_count(
        root.clone(),
        "http://127.0.0.1:1",
        count.clone(),
    );

    cache
        .materialize(&artifact)
        .await
        .expect("initial verification");
    let replacement = path.with_extension("replacement");
    std::fs::write(&replacement, b"model").expect("replacement artifact");
    std::fs::rename(replacement, &path).expect("atomic artifact replacement");
    ModelArtifactCache::for_test_with_digest_count(
        root.clone(),
        "http://127.0.0.1:1",
        count.clone(),
    )
    .materialize(&artifact)
    .await
    .expect("replacement verification");

    assert_eq!(count.load(Ordering::SeqCst), 2);
    std::fs::remove_dir_all(root).expect("replacement cleanup");
}

#[test]
fn atomically_replaces_verification_markers() {
    let root = temp_root("marker");
    let directory = root.join(".dotcore-coordinate-locks");
    std::fs::create_dir_all(&directory).expect("marker directory");
    let path = directory.join("verification.json");
    let first = VerificationMarker::new(
        "owner/repository",
        "0123456789abcdef0123456789abcdef01234567",
        "model.gguf",
        &sha256_text("first"),
        &root.join("first.gguf"),
        FileIdentity {
            device: 1,
            inode: 1,
            size: 1,
            modified_seconds: 1,
            modified_nanoseconds: 1,
            changed_seconds: 1,
            changed_nanoseconds: 1,
        },
    );
    let second = VerificationMarker {
        declared_digest: sha256_text("second"),
        ..first.clone()
    };

    first.write_to(&path).expect("first marker");
    second.write_to(&path).expect("replacement marker");

    assert_eq!(
        serde_json::from_slice::<VerificationMarker>(
            &std::fs::read(&path).expect("complete marker")
        )
        .expect("marker JSON"),
        second
    );
    assert_eq!(
        std::fs::read_dir(&directory)
            .expect("marker directory")
            .count(),
        1
    );
    std::fs::remove_dir_all(root).expect("marker cleanup");
}

#[tokio::test]
async fn validates_pinned_coordinates_and_repairs_a_declared_digest_once() {
    let fixture = Fixture::new().await;
    let cache = fixture.cache();
    let artifact = artifact("model");
    fixture.cache_file(&artifact, b"corrupt");
    fixture.set_response(b"model");

    let prepared = cache
        .materialize(&artifact)
        .await
        .expect("repaired artifact");

    assert!(prepared.replaced);
    assert_eq!(
        std::fs::read(prepared.path).expect("repaired bytes"),
        b"model"
    );
    assert_eq!(
        fixture.request_path().await,
        "/owner/repository/resolve/0123456789abcdef0123456789abcdef01234567/model.gguf"
    );
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_digest_repair_uses_one_forced_download() {
    let fixture = Fixture::new().await;
    let cache = Arc::new(fixture.cache());
    let artifact = artifact("model");
    fixture.cache_file(&artifact, b"corrupt");
    fixture.set_response(b"model");

    let (first, second) = tokio::join!(cache.materialize(&artifact), cache.materialize(&artifact));

    assert_eq!(
        std::fs::read(first.expect("first repair").path).expect("bytes"),
        b"model"
    );
    assert_eq!(
        std::fs::read(second.expect("second cache hit").path).expect("bytes"),
        b"model"
    );
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn coordinate_lock_contention_is_cancelled_while_external_lock_remains_held() {
    let fixture = Fixture::new().await;
    let artifact = artifact("model");
    let coordinate = ArtifactCacheError::artifact_coordinate(&artifact).expect("coordinate");
    let cache = fixture.cache();
    let lock_path = cache.lock_path(&coordinate);
    std::fs::create_dir_all(lock_path.parent().expect("lock parent")).expect("lock parent");
    let external_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .expect("external lock file");
    external_lock.lock().expect("external coordinate lock");
    let attempted = Arc::new(tokio::sync::Notify::new());
    let cache = cache.with_lock_attempted(attempted.clone());
    let cancellation = CancellationToken::new();
    let preparation = tokio::spawn({
        let cancellation = cancellation.clone();
        async move {
            cache
                .prepare_cancellable(&launch_with_artifact(artifact), &cancellation, None)
                .await
        }
    });
    attempted.notified().await;

    cancellation.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), preparation)
        .await
        .expect("lock waiter cancellation")
        .expect("preparation task");

    assert!(matches!(result, Err(ArtifactCacheError::Cancelled)));
    drop(external_lock);
}

#[tokio::test]
async fn quarantines_a_failed_declared_digest_repair_until_the_catalog_digest_changes() {
    let fixture = Fixture::new().await;
    let cache = fixture.cache();
    let artifact = artifact("model");
    fixture.cache_file(&artifact, b"corrupt");
    fixture.set_response(b"still corrupt");

    assert!(matches!(
        cache.materialize(&artifact).await,
        Err(ArtifactCacheError::DeclaredDigestMismatch)
    ));
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), 1);
    assert!(matches!(
        cache.materialize(&artifact).await,
        Err(ArtifactCacheError::DeclaredDigestMismatch)
    ));
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), 1);

    let mut changed = artifact.clone();
    let ModelArtifactSource::HuggingFaceFile {
        provenance_digest, ..
    } = &mut changed
    else {
        panic!("file artifact");
    };
    *provenance_digest = sha256_text("replacement");
    fixture.set_response(b"replacement");

    assert_eq!(
        std::fs::read(
            cache
                .materialize(&changed)
                .await
                .expect("changed catalog digest")
                .path
        )
        .expect("replacement bytes"),
        b"replacement"
    );
    assert_eq!(fixture.get_requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn rejects_paths_that_escape_the_hugging_face_cache_or_are_not_regular_files() {
    let fixture = Fixture::new().await;
    let cache = fixture.cache();
    let artifact = artifact("model");
    let outside = fixture.root.parent().expect("temp parent").join(format!(
        "outside-{}.gguf",
        TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&outside, b"outside").expect("outside file");
    let escaped = fixture.cache_file_path(&artifact);
    std::fs::create_dir_all(escaped.parent().expect("snapshot parent")).expect("snapshot parent");
    std::os::unix::fs::symlink(&outside, &escaped).expect("escape symlink");

    assert_eq!(
        cache
            .materialize(&artifact)
            .await
            .expect_err("path escape")
            .to_string(),
        "Model artifact cache failure: Hugging Face returned a path outside the approved cache root"
    );

    std::fs::remove_file(&escaped).expect("escape removal");
    std::fs::create_dir(&escaped).expect("directory snapshot");
    assert_eq!(
        cache
            .materialize(&artifact)
            .await
            .expect_err("non-regular path")
            .to_string(),
        "Model artifact cache failure: Hugging Face returned a non-regular artifact path"
    );
}

#[tokio::test]
async fn reports_hub_failures_without_exposing_cache_paths() {
    let fixture = Fixture::new().await;
    let cache = fixture.cache();
    fixture.set_failure();

    let error = cache
        .materialize(&artifact("model"))
        .await
        .expect_err("Hub failure");

    assert!(error.to_string().starts_with(
        "Model artifact cache failure: Hugging Face artifact download failed for owner/repository@"
    ));
    assert!(
        !error
            .to_string()
            .contains(fixture.root.to_str().expect("cache root"))
    );
}

#[tokio::test]
async fn marker_removal_preserves_io_source_and_bounded_display() {
    let fixture = Fixture::new().await;
    let artifact = artifact("model");
    let marker = fixture.verification_marker_path(&artifact);
    std::fs::create_dir_all(&marker).expect("marker directory");

    let error = fixture
        .cache()
        .remove_verification_marker(&artifact, &CancellationToken::new())
        .await
        .expect_err("directory marker removal");

    assert_eq!(
        error.to_string(),
        "Model artifact cache failure: artifact verification marker removal failed"
    );
    let source = std::error::Error::source(&error).expect("contextual anyhow source");
    assert_eq!(
        source.to_string(),
        "artifact verification marker removal failed"
    );
    assert!(
        source
            .source()
            .is_some_and(|source| source.is::<std::io::Error>())
    );
}

#[tokio::test]
#[ignore = "requires a network connection to a public Xet-backed Hugging Face model"]
async fn net_qualifies_a_small_public_xet_download() {
    let root = temp_root("xet");
    std::fs::create_dir_all(&root).expect("cache root");
    let transfers = Arc::new(TransferCounter::default());
    let cache = ModelArtifactCache::for_test_with_progress(root.clone(), transfers.clone().into());
    let artifact = ModelArtifactSource::HuggingFaceFile {
        repository: "itlwas/SmolLM2-135M-Q4_K_M-GGUF".to_owned(),
        revision: "6ac5471109ed689c36f98adefd48f6d82a3c1484".to_owned(),
        path: "smollm2-135m-q4_k_m.gguf".to_owned(),
        provenance_digest: "3dac8a6535e754df3da638794da83508b748bb31c4100499a392173d17cb695a"
            .to_owned(),
    };
    let launch = launch_with_artifact(artifact);

    let first = cache.prepare(&launch).await.expect("Xet preparation");
    let first_transfer_count = transfers.count();
    let second = cache.prepare(&launch).await.expect("Xet cache hit");

    assert!(first.artifacts.path("model").is_file());
    assert_eq!(first_transfer_count, 1);
    assert_eq!(transfers.count(), first_transfer_count);
    assert_eq!(
        second.artifacts.path("model"),
        first.artifacts.path("model")
    );
    assert!(!second.replaced);
    std::fs::remove_dir_all(root).expect("cache cleanup");
}

#[derive(Default)]
struct TransferCounter {
    count: AtomicUsize,
}

impl TransferCounter {
    fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

impl ProgressHandler for TransferCounter {
    fn on_progress(&self, event: &ProgressEvent) {
        if matches!(event, ProgressEvent::Download(DownloadEvent::Start { .. })) {
            self.count.fetch_add(1, Ordering::SeqCst);
        }
    }
}

fn discarded() -> StepOutput {
    StepOutput::discard()
}

fn download_line(text: &str) -> (PreparationStep, PreparationEventKind) {
    (
        PreparationStep::ModelDownload,
        PreparationEventKind::Line {
            stream: OutputStream::Stdout,
            text: text.to_owned(),
        },
    )
}

fn launch(model: &str, projector: &str, draft: &str) -> BTreeMap<String, ModelArtifactSource> {
    BTreeMap::from([
        ("model".to_owned(), artifact(model)),
        ("vision-projector".to_owned(), artifact(projector)),
        ("mtp-draft".to_owned(), artifact(draft)),
    ])
}

fn launch_with_artifact(artifact: ModelArtifactSource) -> BTreeMap<String, ModelArtifactSource> {
    BTreeMap::from([("model".to_owned(), artifact)])
}

fn profile_artifact(file: &str, body: &str) -> ModelArtifactSource {
    let mut artifact = artifact(file);
    let ModelArtifactSource::HuggingFaceFile {
        provenance_digest, ..
    } = &mut artifact
    else {
        panic!("file artifact");
    };
    *provenance_digest = sha256_text(body);
    artifact
}

fn artifact(body: &str) -> ModelArtifactSource {
    ModelArtifactSource::HuggingFaceFile {
        repository: "owner/repository".to_owned(),
        revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        path: format!("{body}.gguf"),
        provenance_digest: sha256_text(body),
    }
}

fn snapshot_artifact<const N: usize>(
    repository: &str,
    files: [(&str, &str); N],
) -> ModelArtifactSource {
    ModelArtifactSource::HuggingFaceSnapshot {
        repository: repository.to_owned(),
        revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        files: files
            .into_iter()
            .map(|(path, body)| (path.to_owned(), sha256_text(body)))
            .collect(),
    }
}

/// Shards of a split GGUF set with distinct bodies, so each one is its own hub blob.
const SPLIT_SHARDS: [(&str, &str); 2] = [
    ("Q4/model-00001-of-00002.gguf", "first"),
    ("Q4/model-00002-of-00002.gguf", "second"),
];

/// Serves the split shards from the fixture hub and returns their snapshot artifact.
fn split_snapshot(fixture: &Fixture) -> ModelArtifactSource {
    for (path, body) in SPLIT_SHARDS {
        let file = path.rsplit('/').next().expect("shard file name");
        fixture.set_file_response(file, body.as_bytes());
    }
    snapshot_artifact("owner/repository", SPLIT_SHARDS)
}

/// Returns the device and inode a path resolves to.
fn inode(path: &Path) -> (u64, u64) {
    let metadata = fs::metadata(path).expect("file metadata");
    (metadata.dev(), metadata.ino())
}

/// Returns the linked and copied counts of every snapshot publication event.
fn publication_counts(
    records: &[dotcore_domain::test_support::tracing::Record],
) -> Vec<(&str, &str)> {
    records
        .iter()
        .filter_map(|record| match record {
            dotcore_domain::test_support::tracing::Record::Event { fields, .. }
                if fields.get("event_kind").map(String::as_str)
                    == Some("\"snapshot_published\"") =>
            {
                Some((
                    fields["linked_files"].as_str(),
                    fields["copied_files"].as_str(),
                ))
            }
            _ => None,
        })
        .collect()
}

fn snapshot_regular_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn collect(root: &Path, directory: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(directory).expect("snapshot directory") {
            let entry = entry.expect("snapshot entry");
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).expect("snapshot metadata");
            assert!(
                !metadata.file_type().is_symlink(),
                "published symlink: {path:?}"
            );
            if metadata.is_dir() {
                collect(root, &path, files);
            } else {
                assert!(metadata.is_file(), "non-regular snapshot entry: {path:?}");
                let relative = path
                    .strip_prefix(root)
                    .expect("snapshot-relative path")
                    .to_string_lossy()
                    .into_owned();
                files.insert(relative, std::fs::read(path).expect("snapshot file"));
            }
        }
    }

    let mut files = BTreeMap::new();
    collect(root, root, &mut files);
    files
}

/// Corrupts a published snapshot file by replacing it, leaving the hub blob it was linked from
/// intact, the way an unlink-and-recreate edit would.
fn replace_file(path: &Path, body: &[u8]) {
    let replacement = path.with_extension("replacement");
    fs::write(&replacement, body).expect("replacement file");
    fs::rename(&replacement, path).expect("replace published file");
}

fn temp_root(name: &str) -> PathBuf {
    dotcore_domain::test_support::canonical_temp_dir().join(format!(
        "dotcore-artifact-cache-{name}-{}-{}",
        std::process::id(),
        TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

struct FirstGetGate {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    waiting: AtomicBool,
}

impl FirstGetGate {
    fn new() -> Self {
        Self {
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            waiting: AtomicBool::new(false),
        }
    }
}

struct Fixture {
    root: PathBuf,
    endpoint: String,
    response: Arc<std::sync::Mutex<Option<HubResponse>>>,
    file_responses: Arc<std::sync::Mutex<BTreeMap<String, Vec<u8>>>>,
    first_get_gate: Arc<Mutex<Option<Arc<FirstGetGate>>>>,
    requests: Arc<AtomicUsize>,
    get_requests: Arc<AtomicUsize>,
    digest_count: Arc<AtomicUsize>,
    paths: Arc<tokio::sync::Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("fixture listener");
        let endpoint = format!("http://{}", listener.local_addr().expect("fixture address"));
        let response = Arc::new(std::sync::Mutex::new(None));
        let file_responses = Arc::new(std::sync::Mutex::new(BTreeMap::<String, Vec<u8>>::new()));
        let first_get_gate = Arc::new(Mutex::new(None::<Arc<FirstGetGate>>));
        let requests = Arc::new(AtomicUsize::new(0));
        let get_requests = Arc::new(AtomicUsize::new(0));
        let digest_count = Arc::new(AtomicUsize::new(0));
        let paths = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let task = tokio::spawn({
            let response = response.clone();
            let file_responses = file_responses.clone();
            let requests = requests.clone();
            let first_get_gate = first_get_gate.clone();
            let get_requests = get_requests.clone();
            let paths = paths.clone();
            async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let response = response.clone();
                    let file_responses = file_responses.clone();
                    let requests = requests.clone();
                    let first_get_gate = first_get_gate.clone();
                    let get_requests = get_requests.clone();
                    let paths = paths.clone();
                    tokio::spawn(async move {
                        let mut request = vec![0_u8; 4096];
                        let bytes = socket.read(&mut request).await.expect("request read");
                        let request = String::from_utf8_lossy(&request[..bytes]);
                        let mut parts = request
                            .lines()
                            .next()
                            .expect("request line")
                            .split_whitespace();
                        let method = parts.next().expect("method");
                        let path = parts.next().expect("path").to_owned();
                        requests.fetch_add(1, Ordering::SeqCst);
                        let file_response = file_responses
                            .lock()
                            .expect("file responses")
                            .iter()
                            .find(|(file, _)| path.ends_with(&format!("/{file}")))
                            .map(|(_, body)| body.clone());
                        paths.lock().await.push(path);
                        if method == "GET" {
                            get_requests.fetch_add(1, Ordering::SeqCst);
                            let gate = first_get_gate.lock().expect("first GET gate").clone();
                            if let Some(gate) = gate
                                && !gate.waiting.swap(true, Ordering::SeqCst)
                            {
                                gate.started.notify_one();
                                gate.release.notified().await;
                            }
                        }
                        // A per-file body gets its own blob; the shared body reuses one blob.
                        let (status, body, etag) = match file_response {
                            Some(body) => {
                                let etag = sha256_text(&String::from_utf8_lossy(&body));
                                (200, body, etag)
                            }
                            None => {
                                let (status, body) = response
                                    .lock()
                                    .expect("response")
                                    .clone()
                                    .unwrap_or((500, b"not configured".to_vec()));
                                (status, body, "fixture-blob".to_owned())
                            }
                        };
                        let headers = format!(
                            "HTTP/1.1 {status} Test\r\nETag: \"{etag}\"\r\nX-Repo-Commit: 0123456789abcdef0123456789abcdef01234567\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        socket
                            .write_all(headers.as_bytes())
                            .await
                            .expect("response headers");
                        if method != "HEAD" {
                            socket.write_all(&body).await.expect("response body");
                        }
                    });
                }
            }
        });
        let root = temp_root("fixture");
        std::fs::create_dir_all(&root).expect("cache root");
        Self {
            root,
            endpoint,
            response,
            file_responses,
            first_get_gate,
            requests,
            get_requests,
            digest_count,
            paths,
            task,
        }
    }

    fn cache(&self) -> ModelArtifactCache {
        ModelArtifactCache::for_test(self.root.clone(), &self.endpoint)
    }
    fn counting_cache(&self) -> ModelArtifactCache {
        ModelArtifactCache::for_test_with_digest_count(
            self.root.clone(),
            &self.endpoint,
            self.digest_count.clone(),
        )
    }
    fn set_response(&self, body: &[u8]) {
        *self.response.lock().expect("response") = Some((200, body.to_vec()));
    }

    /// Serves `body` for every repository path ending in `/{file}`, as its own blob.
    fn set_file_response(&self, file: &str, body: &[u8]) {
        self.file_responses
            .lock()
            .expect("file responses")
            .insert(file.to_owned(), body.to_vec());
    }

    fn set_failure(&self) {
        *self.response.lock().expect("response") = Some((500, b"failure".to_vec()));
    }

    fn pause_first_get(&self) -> Arc<FirstGetGate> {
        let gate = Arc::new(FirstGetGate::new());
        *self.first_get_gate.lock().expect("first GET gate") = Some(gate.clone());
        gate
    }

    fn cache_file_path(&self, artifact: &ModelArtifactSource) -> PathBuf {
        self.cache().snapshot_path(artifact).expect("cache path")
    }
    fn cache_file(&self, artifact: &ModelArtifactSource, body: &[u8]) {
        let path = self.cache_file_path(artifact);
        std::fs::create_dir_all(path.parent().expect("snapshot parent")).expect("snapshot parent");
        std::fs::write(path, body).expect("cached file");
    }
    /// Returns the hub blob a snapshot file's hub pointer resolves to.
    fn hub_blob(&self, artifact: &ModelArtifactSource, path: &str) -> PathBuf {
        let (repository, revision, _) =
            ArtifactCacheError::snapshot_parts(artifact).expect("snapshot parts");
        fs::canonicalize(self.cache().hub_pointer_path(repository, revision, path))
            .expect("hub blob")
    }

    fn verification_marker_path(&self, artifact: &ModelArtifactSource) -> PathBuf {
        let coordinate =
            ArtifactCacheError::artifact_coordinate(artifact).expect("artifact coordinate");
        self.cache().verification_marker_path(&coordinate)
    }
    async fn request_path(&self) -> String {
        self.paths
            .lock()
            .await
            .first()
            .expect("request path")
            .clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
