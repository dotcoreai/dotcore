//! Catalog Nix artifact validation shared by engine artifacts and application services.
//!
//! A Nix artifact names a trusted flake reference, the package to build, and what the package
//! provides: a native executable or a Docker image its loader makes available. A locked reference
//! carries one store path per Nix system its declared platforms need, and a `self` reference
//! carries none because the hosting node pins the path it evaluates.

use std::collections::BTreeSet;

use dotcore_domain::nix::{NixFlakeReference, NixFlakeReferenceError};
use dotcore_proto::dotcore::catalog::v1::{NixArtifact, nix_artifact::Provides};

/// What a declaring consumer accepts from a Nix artifact's package.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AcceptedProvides {
    /// A native executable or a Docker image, as engine artifacts accept.
    Any,

    /// A Docker image only, as application services, which always run through Docker, accept.
    DockerImage,
}

/// Returns the Nix systems a Nix artifact needs for its declared platforms: the system building
/// each platform for a native executable, or every system loading each platform for a Docker
/// image. Platforms with no such system contribute none; platform rules reject them separately.
pub(super) fn nix_systems<'a>(
    nix: &NixArtifact,
    platforms: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> BTreeSet<&'static str> {
    let docker_image = dotcore_domain::nix::provides_docker_image(nix);
    platforms
        .into_iter()
        .flat_map(|(operating_system, architecture)| {
            if docker_image {
                dotcore_domain::nix::image_systems_for(operating_system, architecture).to_vec()
            } else {
                dotcore_domain::nix::system_for(operating_system, architecture)
                    .into_iter()
                    .collect()
            }
        })
        .collect()
}

/// Validates one catalog Nix artifact against the Nix systems its declared platforms need and what
/// its consumer accepts. A locked reference precomputes one store path per system; `self`
/// precomputes none.
pub(super) fn validate_nix_artifact(
    nix: &NixArtifact,
    systems: &BTreeSet<&'static str>,
    accepted: AcceptedProvides,
) -> Result<NixFlakeReference, NixArtifactError> {
    let reference = NixFlakeReference::parse(&nix.flake).map_err(NixArtifactError::InvalidFlake)?;
    match nix.package.as_deref() {
        None => return Err(NixArtifactError::MissingPackage),
        Some(package) if !dotcore_domain::nix::valid_package(package) => {
            return Err(NixArtifactError::InvalidPackage);
        }
        Some(_) => {}
    }
    match nix.provides.as_ref() {
        None => return Err(NixArtifactError::MissingProvides),
        Some(Provides::Executable(_)) if accepted == AcceptedProvides::DockerImage => {
            return Err(NixArtifactError::ExecutableNotAccepted);
        }
        Some(Provides::Executable(executable))
            if !dotcore_domain::nix::valid_executable(executable) =>
        {
            return Err(NixArtifactError::InvalidExecutable);
        }
        Some(_) => {}
    }
    if !reference.is_locked() {
        return if nix.store_paths.is_empty() {
            Ok(reference)
        } else {
            Err(NixArtifactError::StorePathsWithoutLockedFlake)
        };
    }
    let mut seen = BTreeSet::new();
    for entry in &nix.store_paths {
        if !systems.contains(entry.system.as_str()) || !seen.insert(entry.system.as_str()) {
            return Err(NixArtifactError::InvalidStorePathSystem);
        }
        if !dotcore_domain::nix::valid_store_path(&entry.store_path) {
            return Err(NixArtifactError::InvalidStorePath);
        }
        // The node launches the image under a reference derived from the loader's store path.
        if dotcore_domain::nix::provides_docker_image(nix)
            && dotcore_domain::nix::docker_launch_reference(&entry.store_path).is_none()
        {
            return Err(NixArtifactError::InvalidDockerImageStorePath);
        }
    }
    // A locked reference is never evaluated, so each needed system requires its store path.
    if let Some(system) = systems
        .iter()
        .copied()
        .find(|system| !seen.contains(system))
    {
        return Err(NixArtifactError::StorePathMissingForSystem { system });
    }
    Ok(reference)
}

/// Reasons a catalog Nix artifact is rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(super) enum NixArtifactError {
    #[error("Nix artifact has an invalid flake reference: {0}")]
    InvalidFlake(#[source] NixFlakeReferenceError),

    #[error(
        "Nix artifact must name the flake package it builds by attribute path, for example `package: engines.llama-cpp.native`"
    )]
    MissingPackage,

    #[error(
        "Nix artifact package must be a dot-separated path of plain attribute names, for example `engines.llama-cpp.native`"
    )]
    InvalidPackage,

    #[error(
        "Nix artifact must name what its package provides: a native program, for example `executable: llama-server`, or a Docker image, for example `dockerImage: {{}}`"
    )]
    MissingProvides,

    #[error(
        "application service Nix artifact must provide a Docker image (`dockerImage`), not a native executable"
    )]
    ExecutableNotAccepted,

    #[error("Nix artifact executable must be one program name in the package's bin directory")]
    InvalidExecutable,

    #[error(
        "Nix artifact store paths require a locked flake reference; the hosting node pins a `self` artifact"
    )]
    StorePathsWithoutLockedFlake,

    #[error(
        "locked Nix artifact must declare the store path of every Nix system its declared platforms need; {system} has none"
    )]
    StorePathMissingForSystem { system: &'static str },

    #[error(
        "Nix artifact store path must name one Nix system its declared platforms need, exactly once"
    )]
    InvalidStorePathSystem,

    #[error("Nix artifact store path must be a top-level /nix/store path")]
    InvalidStorePath,

    #[error(
        "Docker-delivered Nix artifact store path must name a Docker image derivation package whose name Docker accepts as a repository"
    )]
    InvalidDockerImageStorePath,
}

#[cfg(test)]
mod tests {
    use dotcore_domain::test_support::nix as fixture;
    use dotcore_proto::dotcore::catalog::v1::{NixDockerImage, NixSystemStorePath};

    use super::*;

    #[test]
    fn nix_artifacts_accept_locked_and_development_source_references() {
        let darwin = BTreeSet::from(["aarch64-darwin"]);
        let darwin_and_linux = BTreeSet::from(["aarch64-darwin", "x86_64-linux"]);
        let mut both_systems = locked(&["aarch64-darwin", "x86_64-linux"]);
        both_systems.package = Some("default".to_owned());

        assert!(
            validate_nix_artifact(&locked(&["aarch64-darwin"]), &darwin, AcceptedProvides::Any)
                .is_ok_and(|flake| flake.is_locked())
        );
        assert!(
            validate_nix_artifact(&both_systems, &darwin_and_linux, AcceptedProvides::Any)
                .is_ok_and(|flake| flake.is_locked())
        );
        assert!(
            validate_nix_artifact(&nix("self"), &darwin, AcceptedProvides::Any)
                .is_ok_and(|flake| !flake.is_locked())
        );
    }

    #[test]
    fn docker_image_artifacts_need_every_system_loading_their_platforms() {
        let mut image = locked(&["x86_64-linux", "aarch64-linux", "aarch64-darwin"]);
        image.provides = Some(docker_image());
        for entry in &mut image.store_paths {
            entry.store_path = fixture::DOCKER_IMAGE_STORE_PATH.to_owned();
        }
        let systems = nix_systems(&image, [("linux", "amd64"), ("linux", "arm64")]);
        assert_eq!(
            systems,
            BTreeSet::from(["aarch64-darwin", "aarch64-linux", "x86_64-linux"])
        );
        assert!(validate_nix_artifact(&image, &systems, AcceptedProvides::DockerImage).is_ok());

        let mut native_store_path = image.clone();
        native_store_path.store_paths[0].store_path = fixture::STORE_PATH.to_owned();
        assert_eq!(
            validate_nix_artifact(&native_store_path, &systems, AcceptedProvides::DockerImage),
            Err(NixArtifactError::InvalidDockerImageStorePath)
        );

        let mut without_darwin = image.clone();
        without_darwin
            .store_paths
            .retain(|entry| entry.system != "aarch64-darwin");
        assert_eq!(
            validate_nix_artifact(&without_darwin, &systems, AcceptedProvides::DockerImage),
            Err(NixArtifactError::StorePathMissingForSystem {
                system: "aarch64-darwin"
            })
        );

        let native = nix("self");
        assert_eq!(
            nix_systems(&native, [("linux", "arm64"), ("darwin", "arm64")]),
            BTreeSet::from(["aarch64-darwin", "aarch64-linux"])
        );
    }

    #[test]
    fn nix_artifacts_reject_unlocked_path_and_inconsistent_declarations_with_typed_errors() {
        let darwin = BTreeSet::from(["aarch64-darwin"]);
        let darwin_and_linux = BTreeSet::from(["aarch64-darwin", "x86_64-linux"]);
        let mut missing_package = locked(&["aarch64-darwin"]);
        missing_package.package = None;
        let mut bad_package = locked(&["aarch64-darwin"]);
        bad_package.package = Some("packages.default".to_owned());
        let mut missing_provides = nix("self");
        missing_provides.provides = None;
        let mut locked_missing_provides = locked(&["aarch64-darwin"]);
        locked_missing_provides.provides = None;
        let mut bad_executable = locked(&["aarch64-darwin"]);
        bad_executable.provides = Some(Provides::Executable("bin/llama-server".to_owned()));
        let mut self_with_store_path = locked(&["aarch64-darwin"]);
        self_with_store_path.flake = "self".to_owned();
        let mut duplicate_system = locked(&["aarch64-darwin"]);
        duplicate_system
            .store_paths
            .push(store_path("aarch64-darwin"));
        let mut bad_store_path = locked(&["aarch64-darwin"]);
        bad_store_path.store_paths[0].store_path = "/opt/llama-server".to_owned();

        let cases = [
            (
                nix("github:dotcoreinc/dotcore"),
                &darwin,
                NixArtifactError::InvalidFlake(NixFlakeReferenceError::UnlockedReference),
            ),
            (
                nix(&format!("github:example/engines/{}", fixture::REVISION)),
                &darwin,
                NixArtifactError::InvalidFlake(NixFlakeReferenceError::UntrustedRepository),
            ),
            (
                nix("github:dotcoreinc/dotcore/main"),
                &darwin,
                NixArtifactError::InvalidFlake(NixFlakeReferenceError::InvalidRevision),
            ),
            (
                nix("path:/src/dotcore"),
                &darwin,
                NixArtifactError::InvalidFlake(NixFlakeReferenceError::UnsupportedReference),
            ),
            (
                nix("/src/dotcore"),
                &darwin,
                NixArtifactError::InvalidFlake(NixFlakeReferenceError::UnsupportedReference),
            ),
            (
                nix("self?dir=engines/llama-cpp"),
                &darwin,
                NixArtifactError::InvalidFlake(NixFlakeReferenceError::UnsupportedQuery),
            ),
            (missing_package, &darwin, NixArtifactError::MissingPackage),
            (bad_package, &darwin, NixArtifactError::InvalidPackage),
            (missing_provides, &darwin, NixArtifactError::MissingProvides),
            (
                locked_missing_provides,
                &darwin,
                NixArtifactError::MissingProvides,
            ),
            (bad_executable, &darwin, NixArtifactError::InvalidExecutable),
            (
                self_with_store_path,
                &darwin,
                NixArtifactError::StorePathsWithoutLockedFlake,
            ),
            (
                nix(&fixture::locked_flake()),
                &darwin,
                NixArtifactError::StorePathMissingForSystem {
                    system: "aarch64-darwin",
                },
            ),
            (
                locked(&["aarch64-darwin"]),
                &darwin_and_linux,
                NixArtifactError::StorePathMissingForSystem {
                    system: "x86_64-linux",
                },
            ),
            (
                locked(&["x86_64-linux"]),
                &darwin,
                NixArtifactError::InvalidStorePathSystem,
            ),
            (
                duplicate_system,
                &darwin,
                NixArtifactError::InvalidStorePathSystem,
            ),
            (bad_store_path, &darwin, NixArtifactError::InvalidStorePath),
        ];
        for (artifact, systems, expected) in cases {
            assert_eq!(
                validate_nix_artifact(&artifact, systems, AcceptedProvides::Any),
                Err(expected),
                "{artifact:?}"
            );
        }
    }

    #[test]
    fn docker_image_consumers_reject_native_executables() {
        assert_eq!(
            validate_nix_artifact(
                &nix("self"),
                &BTreeSet::new(),
                AcceptedProvides::DockerImage
            ),
            Err(NixArtifactError::ExecutableNotAccepted)
        );
    }

    fn nix(flake: &str) -> NixArtifact {
        NixArtifact {
            flake: flake.to_owned(),
            package: Some("engines.llama-cpp.native".to_owned()),
            provides: Some(Provides::Executable("llama-server".to_owned())),
            ..Default::default()
        }
    }

    fn locked(systems: &[&str]) -> NixArtifact {
        NixArtifact {
            store_paths: systems.iter().map(|system| store_path(system)).collect(),
            ..nix(&fixture::locked_flake())
        }
    }

    fn store_path(system: &str) -> NixSystemStorePath {
        NixSystemStorePath {
            system: system.to_owned(),
            store_path: fixture::STORE_PATH.to_owned(),
        }
    }

    fn docker_image() -> Provides {
        Provides::DockerImage(NixDockerImage {})
    }
}
