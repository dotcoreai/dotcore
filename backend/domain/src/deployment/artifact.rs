//! Shared validation for the OCI or Nix launch identity a deployment snapshot retains.
//!
//! Model and application snapshots keep, for each selected artifact, a preparation policy and the
//! immutable identity every launch uses. A digest reference keeps its canonical form, for example
//! `registry.test/engine@sha256:<digest>`, a local-development tag keeps the `sha256:` image ID it
//! resolved to, and a Nix flake package keeps its store path. Any other combination is rejected,
//! so a snapshot never falls back to a tag or a flake, except a local-development tag or a
//! development-source Nix package, whose identity the SiteNode hosting the deployment pins once.

use dotcore_proto::dotcore::{
    catalog::v1::{NixArtifact, OciImageReference},
    deployment::v1::ArtifactPreparationPolicy,
};

use crate::{
    nix::{NixFlakeReference, NixFlakeReferenceError},
    oci::{
        OciReferenceError, OciReferenceKind, canonical_digest_reference, render_reference,
        valid_local_image_id, validate_structured_reference,
    },
};

/// Validates one retained preparation policy and resolved launch identity against the OCI or Nix
/// artifact it was selected from.
pub fn validate_retained_launch_identity(
    artifact: super::launch_pin::RetainedArtifact<'_>,
    policy: i32,
    launch_identity: &str,
) -> Result<ArtifactPreparationPolicy, LaunchIdentityError> {
    match artifact {
        super::launch_pin::RetainedArtifact::Oci(reference) => {
            validate_launch_identity(policy, reference, launch_identity)
        }
        super::launch_pin::RetainedArtifact::Nix(nix) => validate_nix_launch_identity(
            policy,
            nix,
            launch_identity,
            LaunchIdentityResolution::Retained,
        ),
    }
}

/// Validates one retained preparation policy and launch identity against its catalog reference.
pub fn validate_launch_identity(
    policy: i32,
    source: &OciImageReference,
    launch_identity: &str,
) -> Result<ArtifactPreparationPolicy, LaunchIdentityError> {
    let kind =
        validate_structured_reference(source).map_err(LaunchIdentityError::InvalidReference)?;
    match ArtifactPreparationPolicy::try_from(policy) {
        Ok(ArtifactPreparationPolicy::ImmutableRemote) => {
            let expected = immutable_remote_launch_identity(source)
                .map_err(LaunchIdentityError::InvalidReference)?
                .ok_or(LaunchIdentityError::ImmutableRemoteWithoutDigest)?;
            if expected != launch_identity {
                return Err(LaunchIdentityError::ImmutableRemoteIdentityMismatch);
            }
            Ok(ArtifactPreparationPolicy::ImmutableRemote)
        }
        Ok(ArtifactPreparationPolicy::LocalDevelopment) => {
            if kind != OciReferenceKind::TagOnly {
                return Err(LaunchIdentityError::LocalDevelopmentWithDigest);
            }
            if !valid_local_image_id(launch_identity) {
                return Err(LaunchIdentityError::InvalidLocalImageId);
            }
            Ok(ArtifactPreparationPolicy::LocalDevelopment)
        }
        Ok(ArtifactPreparationPolicy::DevelopmentSource) => {
            Err(LaunchIdentityError::DevelopmentSourceForOci)
        }
        Ok(ArtifactPreparationPolicy::Unspecified) | Err(_) => {
            Err(LaunchIdentityError::MissingPolicy)
        }
    }
}

/// Validates one retained Nix artifact, preparation policy, and store path. The artifact keeps its
/// flake reference, package, and what the package provides as provenance. A locked reference
/// prepares as an immutable remote artifact and keeps its catalog store paths, one per Nix system;
/// `self` prepares as development source and keeps none. Either way the hosting SiteNode pins the
/// store path of its own Nix system, so the snapshot leaves the identity empty until that pin is
/// applied, and a locked pin must be one of the retained store paths.
pub fn validate_nix_launch_identity(
    policy: i32,
    source: &NixArtifact,
    launch_identity: &str,
    resolution: LaunchIdentityResolution,
) -> Result<ArtifactPreparationPolicy, LaunchIdentityError> {
    let reference =
        NixFlakeReference::parse(&source.flake).map_err(LaunchIdentityError::InvalidNixFlake)?;
    let complete = source
        .package
        .as_deref()
        .is_some_and(crate::nix::valid_package)
        && crate::nix::valid_provides(source);
    if !complete {
        return Err(LaunchIdentityError::IncompleteNixProvenance);
    }
    let policy = match ArtifactPreparationPolicy::try_from(policy) {
        Ok(ArtifactPreparationPolicy::Unspecified) | Err(_) => {
            return Err(LaunchIdentityError::MissingPolicy);
        }
        Ok(policy) if policy == reference.preparation_policy() => policy,
        Ok(_) => return Err(LaunchIdentityError::NixPolicyMismatch),
    };
    let locked = policy == ArtifactPreparationPolicy::ImmutableRemote;
    if !locked && !source.store_paths.is_empty() {
        return Err(LaunchIdentityError::NixStorePathsRetained);
    }
    if locked
        && (source.store_paths.is_empty()
            || source
                .store_paths
                .iter()
                .any(|entry| !crate::nix::valid_store_path(&entry.store_path)))
    {
        return Err(LaunchIdentityError::NixStorePathsMissing);
    }
    if resolution == LaunchIdentityResolution::NodeResolved {
        return if launch_identity.is_empty() {
            Ok(policy)
        } else {
            Err(LaunchIdentityError::NixIdentityRetained)
        };
    }
    if !crate::nix::valid_store_path(launch_identity) {
        return Err(LaunchIdentityError::InvalidNixStorePath);
    }
    // The Docker runtime launches the reference derived from a Docker image derivation's path.
    if crate::nix::provides_docker_image(source)
        && crate::nix::docker_launch_reference(launch_identity).is_none()
    {
        return Err(LaunchIdentityError::InvalidDockerImageStorePath);
    }
    if locked
        && !source
            .store_paths
            .iter()
            .any(|entry| entry.store_path == launch_identity)
    {
        return Err(LaunchIdentityError::NixStorePathNotRetained);
    }
    Ok(policy)
}

/// Where the launch identity of a development selection comes from: a local-development tag or a
/// development-source Nix package.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchIdentityResolution {
    /// The identity is present in the snapshot: retained at creation, or applied from the
    /// recorded node pin.
    Retained,

    /// The SiteNode named by the exact placement resolves it once; the snapshot leaves it empty.
    NodeResolved,
}

/// Whether a selection leaves its launch identity for the hosting SiteNode: a local-development
/// policy on a tag-only reference with no retained identity.
pub fn node_resolved_selection(
    policy: i32,
    source: &OciImageReference,
    launch_identity: &str,
) -> bool {
    launch_identity.is_empty()
        && policy == ArtifactPreparationPolicy::LocalDevelopment as i32
        && validate_structured_reference(source) == Ok(OciReferenceKind::TagOnly)
}

/// Returns the canonical digest reference a digest-bearing artifact launches from, or `None` for
/// a tag-only reference.
pub fn immutable_remote_launch_identity(
    source: &OciImageReference,
) -> Result<Option<String>, OciReferenceError> {
    canonical_digest_reference(&render_reference(source)?)
}

/// Reasons a retained preparation policy and launch identity are rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LaunchIdentityError {
    #[error("artifact selection retains no preparation policy")]
    MissingPolicy,

    #[error("artifact source reference is invalid")]
    InvalidReference(#[source] OciReferenceError),

    #[error("immutable remote artifact requires a digest reference")]
    ImmutableRemoteWithoutDigest,

    #[error("immutable remote launch identity is not the canonical digest reference")]
    ImmutableRemoteIdentityMismatch,

    #[error("local-development artifact requires a tag-only reference")]
    LocalDevelopmentWithDigest,

    #[error("local-development launch identity is not a SHA-256 image ID")]
    InvalidLocalImageId,

    #[error("development-source preparation applies only to Nix artifacts")]
    DevelopmentSourceForOci,

    #[error("Nix artifact flake reference is invalid")]
    InvalidNixFlake(#[source] NixFlakeReferenceError),

    #[error(
        "Nix artifact selection must retain a valid package and the program it provides: an executable or a Docker image loader"
    )]
    IncompleteNixProvenance,

    #[error("development-source Nix artifact selection must not retain catalog store paths")]
    NixStorePathsRetained,

    #[error("locked Nix artifact selection must retain its valid catalog store paths")]
    NixStorePathsMissing,

    #[error("Nix launch identity is not one of the locked artifact's retained store paths")]
    NixStorePathNotRetained,

    #[error("Nix launch identity is not a top-level /nix/store path")]
    InvalidNixStorePath,

    #[error("Docker-delivered Nix launch identity is not a Docker image derivation store path")]
    InvalidDockerImageStorePath,

    #[error("Nix artifact selection must leave its launch identity for the hosting node")]
    NixIdentityRetained,

    #[error(
        "Nix artifact preparation policy must be immutable remote for a locked flake and development source for self"
    )]
    NixPolicyMismatch,
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::catalog::v1::{
        NixSystemStorePath, OciDigest, OciRegistryLocation, OciRepository, nix_artifact::Provides,
    };

    use super::*;
    use crate::test_support::nix as nix_fixture;

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const IMAGE_ID: &str =
        "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    #[test]
    fn digest_reference_retains_its_canonical_form_without_the_tag() {
        let source = reference(Some("release"), Some(DIGEST));
        let identity = format!("registry.test/dotcore/engine@sha256:{DIGEST}");

        assert_eq!(
            immutable_remote_launch_identity(&source),
            Ok(Some(identity.clone()))
        );
        assert_eq!(
            validate_launch_identity(
                ArtifactPreparationPolicy::ImmutableRemote as i32,
                &source,
                &identity,
            ),
            Ok(ArtifactPreparationPolicy::ImmutableRemote)
        );
    }

    #[test]
    fn tag_only_reference_retains_a_local_image_id() {
        let source = reference(Some("dev"), None);

        assert_eq!(immutable_remote_launch_identity(&source), Ok(None));
        assert_eq!(
            validate_launch_identity(
                ArtifactPreparationPolicy::LocalDevelopment as i32,
                &source,
                IMAGE_ID,
            ),
            Ok(ArtifactPreparationPolicy::LocalDevelopment)
        );
    }

    #[test]
    fn mismatched_policy_reference_and_identity_are_rejected() {
        let digest = reference(None, Some(DIGEST));
        let tag = reference(Some("dev"), None);
        let remote = ArtifactPreparationPolicy::ImmutableRemote as i32;
        let local = ArtifactPreparationPolicy::LocalDevelopment as i32;
        let canonical = format!("registry.test/dotcore/engine@sha256:{DIGEST}");
        let cases = [
            (
                ArtifactPreparationPolicy::Unspecified as i32,
                &digest,
                canonical.as_str(),
                LaunchIdentityError::MissingPolicy,
            ),
            (
                99,
                &digest,
                canonical.as_str(),
                LaunchIdentityError::MissingPolicy,
            ),
            (
                remote,
                &tag,
                "registry.test/dotcore/engine:dev",
                LaunchIdentityError::ImmutableRemoteWithoutDigest,
            ),
            (
                remote,
                &digest,
                "registry.test/dotcore/other@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                LaunchIdentityError::ImmutableRemoteIdentityMismatch,
            ),
            (
                remote,
                &digest,
                "",
                LaunchIdentityError::ImmutableRemoteIdentityMismatch,
            ),
            (
                local,
                &digest,
                IMAGE_ID,
                LaunchIdentityError::LocalDevelopmentWithDigest,
            ),
            (
                local,
                &tag,
                "registry.test/dotcore/engine:dev",
                LaunchIdentityError::InvalidLocalImageId,
            ),
            (
                local,
                &tag,
                "sha256:DDDD",
                LaunchIdentityError::InvalidLocalImageId,
            ),
            (local, &tag, "", LaunchIdentityError::InvalidLocalImageId),
        ];

        for (policy, source, identity, expected) in cases {
            assert_eq!(
                validate_launch_identity(policy, source, identity),
                Err(expected),
                "policy {policy} with identity {identity:?}"
            );
        }
    }

    #[test]
    fn only_an_empty_local_development_tag_is_left_for_the_hosting_node() {
        let tag = reference(Some("dev"), None);
        let digest = reference(None, Some(DIGEST));
        let local = ArtifactPreparationPolicy::LocalDevelopment as i32;
        let remote = ArtifactPreparationPolicy::ImmutableRemote as i32;

        assert!(node_resolved_selection(local, &tag, ""));
        assert!(!node_resolved_selection(local, &tag, IMAGE_ID));
        assert!(!node_resolved_selection(local, &digest, ""));
        assert!(!node_resolved_selection(remote, &tag, ""));
        assert!(!node_resolved_selection(
            local,
            &OciImageReference::default(),
            ""
        ));
    }

    #[test]
    fn development_source_policy_is_rejected_for_oci_artifacts() {
        assert_eq!(
            validate_launch_identity(
                ArtifactPreparationPolicy::DevelopmentSource as i32,
                &reference(Some("dev"), None),
                IMAGE_ID,
            ),
            Err(LaunchIdentityError::DevelopmentSourceForOci)
        );
    }

    #[test]
    fn nix_selection_retains_its_provenance_and_store_path_under_the_matching_policy() {
        const OTHER_STORE_PATH: &str =
            "/nix/store/11111111111111111111111111111111-llama-cpp-b11417";
        let remote = ArtifactPreparationPolicy::ImmutableRemote as i32;
        let development = ArtifactPreparationPolicy::DevelopmentSource as i32;
        let mut locked = nix(&nix_fixture::locked_flake());
        locked.store_paths = vec![NixSystemStorePath {
            system: "aarch64-darwin".to_owned(),
            store_path: nix_fixture::STORE_PATH.to_owned(),
        }];
        let self_source = nix("self");
        let node = LaunchIdentityResolution::NodeResolved;
        let retained = LaunchIdentityResolution::Retained;

        assert_eq!(
            validate_nix_launch_identity(remote, &locked, "", node),
            Ok(ArtifactPreparationPolicy::ImmutableRemote)
        );
        assert_eq!(
            validate_nix_launch_identity(remote, &locked, nix_fixture::STORE_PATH, retained),
            Ok(ArtifactPreparationPolicy::ImmutableRemote)
        );
        assert_eq!(
            validate_nix_launch_identity(development, &self_source, "", node),
            Ok(ArtifactPreparationPolicy::DevelopmentSource)
        );
        assert_eq!(
            validate_nix_launch_identity(
                development,
                &self_source,
                nix_fixture::STORE_PATH,
                retained
            ),
            Ok(ArtifactPreparationPolicy::DevelopmentSource)
        );

        let mut without_executable = locked.clone();
        without_executable.provides = None;
        let mut self_with_store_paths = self_source.clone();
        self_with_store_paths.store_paths = locked.store_paths.clone();
        let mut locked_without_store_paths = locked.clone();
        locked_without_store_paths.store_paths.clear();
        let cases = [
            (
                remote,
                nix("github:dotcoreinc/dotcore-oss"),
                "",
                node,
                LaunchIdentityError::InvalidNixFlake(NixFlakeReferenceError::UnlockedReference),
            ),
            (
                remote,
                without_executable,
                "",
                node,
                LaunchIdentityError::IncompleteNixProvenance,
            ),
            (
                development,
                self_with_store_paths,
                "",
                node,
                LaunchIdentityError::NixStorePathsRetained,
            ),
            (
                remote,
                locked_without_store_paths,
                "",
                node,
                LaunchIdentityError::NixStorePathsMissing,
            ),
            (
                remote,
                locked.clone(),
                nix_fixture::STORE_PATH,
                node,
                LaunchIdentityError::NixIdentityRetained,
            ),
            (
                remote,
                locked.clone(),
                "",
                retained,
                LaunchIdentityError::InvalidNixStorePath,
            ),
            (
                remote,
                locked.clone(),
                OTHER_STORE_PATH,
                retained,
                LaunchIdentityError::NixStorePathNotRetained,
            ),
            (
                development,
                self_source.clone(),
                nix_fixture::STORE_PATH,
                node,
                LaunchIdentityError::NixIdentityRetained,
            ),
            (
                development,
                self_source.clone(),
                "",
                retained,
                LaunchIdentityError::InvalidNixStorePath,
            ),
            (
                development,
                locked.clone(),
                "",
                node,
                LaunchIdentityError::NixPolicyMismatch,
            ),
            (
                remote,
                self_source.clone(),
                nix_fixture::STORE_PATH,
                retained,
                LaunchIdentityError::NixPolicyMismatch,
            ),
            (
                ArtifactPreparationPolicy::LocalDevelopment as i32,
                locked,
                "",
                node,
                LaunchIdentityError::NixPolicyMismatch,
            ),
            (
                ArtifactPreparationPolicy::Unspecified as i32,
                self_source,
                "",
                node,
                LaunchIdentityError::MissingPolicy,
            ),
        ];
        for (policy, source, identity, resolution, expected) in cases {
            assert_eq!(
                validate_nix_launch_identity(policy, &source, identity, resolution),
                Err(expected),
                "{source:?} with policy {policy}, identity {identity:?}, {resolution:?}"
            );
        }
    }

    #[test]
    fn invalid_source_reference_is_rejected_before_the_policy() {
        let source = OciImageReference::default();

        assert_eq!(
            validate_launch_identity(
                ArtifactPreparationPolicy::LocalDevelopment as i32,
                &source,
                IMAGE_ID,
            ),
            Err(LaunchIdentityError::InvalidReference(
                OciReferenceError::MissingRepository
            ))
        );
    }

    fn nix(flake: &str) -> NixArtifact {
        NixArtifact {
            flake: flake.to_owned(),
            package: Some("engines.llama-cpp.native".to_owned()),
            provides: Some(Provides::Executable("llama-server".to_owned())),
            store_paths: Vec::new(),
        }
    }

    fn reference(tag: Option<&str>, digest: Option<&str>) -> OciImageReference {
        OciImageReference {
            repository: Some(OciRepository {
                registry: Some(OciRegistryLocation {
                    host: "registry.test".to_owned(),
                    port: None,
                }),
                name: "dotcore/engine".to_owned(),
            }),
            tag: tag.map(str::to_owned),
            digest: digest.map(|encoded| OciDigest {
                algorithm: "sha256".to_owned(),
                encoded: encoded.to_owned(),
            }),
        }
    }
}
