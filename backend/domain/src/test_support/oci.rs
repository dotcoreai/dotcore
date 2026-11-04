//! Structured OCI catalog fixtures for backend unit tests.

use dotcore_proto::dotcore::catalog::v1::{
    ApplicationServiceArtifact, EngineArtifact, OciDigest, OciImageReference, OciPlatform,
    OciRegistryLocation, OciRepository, Selector, application_service_artifact,
    engine_artifact::Kind as EngineArtifactKind,
};

pub fn digest_artifact(
    registry: Option<&str>,
    repository: &str,
    digest: &str,
    platform: Option<OciPlatform>,
) -> ApplicationServiceArtifact {
    ApplicationServiceArtifact {
        kind: Some(application_service_artifact::Kind::Oci(digest_reference(
            registry, repository, digest,
        ))),
        supported_platforms: vec![platform.unwrap_or_else(|| OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            variant: None,
        })],
    }
}

pub fn digest_reference(
    registry: Option<&str>,
    repository: &str,
    digest: &str,
) -> OciImageReference {
    OciImageReference {
        repository: Some(OciRepository {
            registry: registry.map(|host| OciRegistryLocation {
                host: host.to_owned(),
                port: None,
            }),
            name: repository.to_owned(),
        }),
        tag: None,
        digest: Some(OciDigest {
            algorithm: "sha256".to_owned(),
            encoded: digest.to_owned(),
        }),
    }
}

/// Returns the OCI image of a fixture engine artifact for in-place edits.
pub fn reference_mut(artifact: &mut EngineArtifact) -> Option<&mut OciImageReference> {
    match artifact.kind.as_mut()? {
        EngineArtifactKind::Oci(reference) => Some(reference),
        EngineArtifactKind::Nix(_) => None,
    }
}

/// Builds a digest-pinned OCI engine artifact with one selector; NVIDIA selectors require
/// driver major 580.
pub fn engine_artifact(
    artifact_id: &str,
    registry: Option<&str>,
    repository: &str,
    digest: &str,
    operating_system: &str,
    architecture: &str,
    accelerator: &str,
) -> EngineArtifact {
    EngineArtifact {
        artifact_id: artifact_id.to_owned(),
        kind: Some(EngineArtifactKind::Oci(digest_reference(
            registry, repository, digest,
        ))),
        selectors: vec![Selector {
            platform: Some(OciPlatform {
                operating_system: operating_system.to_owned(),
                architecture: architecture.to_owned(),
                variant: None,
            }),
            accelerators: vec![accelerator.to_owned()],
            minimum_nvidia_driver_major_version: if accelerator == "nvidia" { 580 } else { 0 },
            ..Default::default()
        }],
    }
}

/// Test access to the OCI reference of an application service artifact.
pub trait ServiceArtifactOci {
    /// Returns the OCI reference, or `None` for a Nix artifact.
    fn oci(&self) -> Option<&OciImageReference>;

    /// Returns the mutable OCI reference, or `None` for a Nix artifact.
    fn oci_mut(&mut self) -> Option<&mut OciImageReference>;
}

impl ServiceArtifactOci for ApplicationServiceArtifact {
    fn oci(&self) -> Option<&OciImageReference> {
        crate::oci::service_artifact_reference(self)
    }

    fn oci_mut(&mut self) -> Option<&mut OciImageReference> {
        match self.kind.as_mut()? {
            application_service_artifact::Kind::Oci(reference) => Some(reference),
            application_service_artifact::Kind::Nix(_) => None,
        }
    }
}
