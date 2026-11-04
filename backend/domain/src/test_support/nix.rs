//! Nix engine artifact catalog fixtures for backend unit tests: a locked Dotcore flake revision,
//! the store paths fixture evaluators report, and a single-selector Nix engine artifact.

use dotcore_proto::dotcore::catalog::v1::{
    EngineArtifact, NixArtifact, OciPlatform, Selector,
    engine_artifact::Kind as EngineArtifactKind, nix_artifact::Provides,
};

/// Locked GitHub revision used by Nix fixtures.
pub const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";

/// Store path a fixture evaluator reports for every Nix fixture.
pub const STORE_PATH: &str = "/nix/store/l0j8aykkmbdh1i1pnb0jq3wxj0jv8sh9-llama-cpp-b11417";

/// Store path of a Docker image derivation's loader package in Docker-delivered fixtures.
pub const DOCKER_IMAGE_STORE_PATH: &str =
    "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-n8n";

/// Docker reference derived from [`DOCKER_IMAGE_STORE_PATH`].
pub const DOCKER_IMAGE_LAUNCH_REFERENCE: &str =
    "dotcore-nix/docker-image-n8n:0123456789abcdfghijklmnpqrsvwxyz";

/// Returns a locked GitHub flake reference to the fixture engine directory.
pub fn locked_flake() -> String {
    format!("github:dotcoreinc/dotcore/{REVISION}")
}

/// Builds a Nix engine artifact of package `engines.llama-cpp.native` running `llama-server`, with one
/// selector and no precomputed store paths.
pub fn engine_artifact(
    artifact_id: &str,
    flake: &str,
    operating_system: &str,
    architecture: &str,
    accelerator: &str,
) -> EngineArtifact {
    EngineArtifact {
        artifact_id: artifact_id.to_owned(),
        kind: Some(EngineArtifactKind::Nix(NixArtifact {
            flake: flake.to_owned(),
            package: Some("engines.llama-cpp.native".to_owned()),
            provides: Some(Provides::Executable("llama-server".to_owned())),
            store_paths: Vec::new(),
        })),
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
