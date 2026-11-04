//! Applies and validates the engine launch identity that the SiteNode hosting a local model
//! deployment resolves once, for example the tag `registry.test/engine:dev` pinned to the image ID
//! `sha256:<64 hex>`, a development-source Nix package pinned to its `/nix/store/...` path, or a
//! locked Nix artifact pinned to the catalog store path of the node's own Nix system.
//!
//! A local-development image or any Nix engine keeps an empty launch identity in its immutable
//! snapshot. The hosting node retains the pin it resolved, and the inference
//! controller records it once in the declaration. Every launch applies that pin to the retained
//! snapshot, so the engine is never resolved again and the snapshot itself never changes.

use std::borrow::Cow;

use dotcore_catalog::CatalogError;
use dotcore_domain::deployment::{
    artifact::LaunchIdentityResolution,
    launch_pin::{PinSource, RetainedArtifact, UnpinnedSelection, pin_source, valid_node_pins},
};
use dotcore_proto::dotcore::{
    catalog::v1::{OciPlatform, engine_artifact::Kind as EngineArtifactKind},
    deployment::v1::NodeResolvedLaunchIdentity,
    model::v1::{
        ModelDeploymentDeclaration, ModelDeploymentSnapshot, ResolvedEngineArtifact,
        model_deployment_snapshot::Source,
    },
};

/// Selection key of the engine image among a model deployment's node-resolved launch identities.
pub(crate) const ENGINE_SELECTION_KEY: &str = "engine";

/// Returns the engine selection a local deployment leaves for its hosting SiteNode, with the
/// retained platform the resolved artifact must match, or `None` when the snapshot retains it.
pub(crate) fn unpinned_engine_selection(
    declaration: &ModelDeploymentDeclaration,
) -> Option<(UnpinnedSelection, OciPlatform)> {
    let (_, source) = node_resolved_engine(declaration)?;
    let platform = launch_engine(declaration)?.platform.as_ref()?;
    Some((
        UnpinnedSelection {
            selection_key: ENGINE_SELECTION_KEY.to_owned(),
            source,
        },
        platform.clone(),
    ))
}

/// Returns the snapshot every launch of the deployment uses: the retained snapshot, with the
/// engine launch identity recorded in the declaration applied when the snapshot leaves it to the
/// hosting SiteNode. Returns `None` without a snapshot.
pub(crate) fn launch_snapshot(
    declaration: &ModelDeploymentDeclaration,
) -> Option<Cow<'_, ModelDeploymentSnapshot>> {
    let snapshot = declaration.snapshot.as_ref()?;
    let pin = node_resolved_engine(declaration).and_then(|_| {
        declaration
            .node_resolved_launch_identities
            .iter()
            .find(|pin| pin.selection_key == ENGINE_SELECTION_KEY)
    });
    let Some(pin) = pin else {
        return Some(Cow::Borrowed(snapshot));
    };
    let mut pinned = snapshot.clone();
    if let Some(Source::Local(local)) = pinned.source.as_mut()
        && let Some(engine) = local.engine_artifact.as_mut()
    {
        engine.launch_identity = pin.launch_identity.clone();
    }
    Some(Cow::Owned(pinned))
}

/// Whether a model assignment of a node's desired view carries valid portable intent: its
/// snapshot and pins validate for resolution on the hosting node.
pub(crate) fn valid_node_view_assignment(declaration: &ModelDeploymentDeclaration) -> bool {
    validate_node_resolved_declaration(declaration).is_ok()
}

/// Validates a local declaration whose engine launch identity the hosting SiteNode resolves.
pub(crate) fn validate_node_resolved_declaration(
    declaration: &ModelDeploymentDeclaration,
) -> Result<(), CatalogError> {
    validate_local_declaration(declaration, LaunchIdentityResolution::NodeResolved)
}

/// Validates a local declaration's snapshot and its recorded node pins. With retained resolution
/// the snapshot names its engine launch identity and no pin is recorded; with node resolution a
/// local-development engine may leave it empty, and at most its one engine pin is recorded.
pub(crate) fn validate_local_declaration(
    declaration: &ModelDeploymentDeclaration,
    resolution: LaunchIdentityResolution,
) -> Result<(), CatalogError> {
    let snapshot = declaration.snapshot.as_ref().ok_or_else(|| {
        CatalogError::Invalid(anyhow::anyhow!(
            "local deployment declaration has no immutable model snapshot"
        ))
    })?;
    crate::model::resolved::validate_deployment_snapshot_with(snapshot, resolution)?;
    let pins = &declaration.node_resolved_launch_identities;
    let kind = node_resolved_engine(declaration).map(|(_, source)| source.kind());
    if (resolution == LaunchIdentityResolution::Retained && !pins.is_empty())
        || !valid_node_pins(pins, |key| kind.filter(|_| key == ENGINE_SELECTION_KEY))
    {
        return Err(CatalogError::Invalid(anyhow::anyhow!(
            "local deployment declaration records an invalid node-resolved engine launch identity"
        )));
    }
    Ok(())
}

/// Returns the declaration with `pins` recorded as its node-resolved launch identities.
pub(crate) fn with_node_pins(
    declaration: &ModelDeploymentDeclaration,
    pins: Vec<NodeResolvedLaunchIdentity>,
) -> ModelDeploymentDeclaration {
    ModelDeploymentDeclaration {
        node_resolved_launch_identities: pins,
        ..declaration.clone()
    }
}

/// Returns the snapshot engine and what its hosting SiteNode resolves when the snapshot leaves
/// its launch identity to that node: a local-development image tag, a package of the node's
/// development source, or a locked Nix artifact's catalog store paths.
fn node_resolved_engine(
    declaration: &ModelDeploymentDeclaration,
) -> Option<(&ResolvedEngineArtifact, PinSource)> {
    let engine = launch_engine(declaration)?;
    let artifact = match engine.artifact.as_ref()?.kind.as_ref()? {
        EngineArtifactKind::Oci(reference) => RetainedArtifact::Oci(reference),
        EngineArtifactKind::Nix(nix) => RetainedArtifact::Nix(nix),
    };
    let source = pin_source(artifact, engine.preparation_policy, &engine.launch_identity)?;
    Some((engine, source))
}

/// Returns the engine artifact of a local declaration's retained snapshot.
fn launch_engine(declaration: &ModelDeploymentDeclaration) -> Option<&ResolvedEngineArtifact> {
    let Some(Source::Local(local)) = declaration.snapshot.as_ref()?.source.as_ref() else {
        return None;
    };
    local.engine_artifact.as_ref()
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::deployment::v1::ArtifactPreparationPolicy;
    use tokio_util::sync::CancellationToken;

    use super::*;

    const IMAGE_ID: &str =
        "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    const STORE_PATH: &str = dotcore_domain::test_support::nix::STORE_PATH;

    #[test]
    fn a_node_resolved_engine_launches_from_the_recorded_pin() {
        let unpinned = node_resolved_declaration();
        let (selection, platform) = unpinned_engine_selection(&unpinned).expect("unpinned engine");
        assert_eq!(
            selection,
            UnpinnedSelection {
                selection_key: ENGINE_SELECTION_KEY.to_owned(),
                source: PinSource::LocalImage {
                    reference: crate::test_support::TAG_ONLY_ENGINE_REFERENCE.to_owned(),
                },
            }
        );
        assert_eq!(
            (
                platform.operating_system.as_str(),
                platform.architecture.as_str()
            ),
            ("linux", "amd64")
        );
        assert!(crate::model::resolved::ResolvedModelLaunch::from_declaration(&unpinned).is_err());

        let pinned = with_node_pins(&unpinned, vec![engine_pin(IMAGE_ID)]);
        let launch = crate::model::resolved::ResolvedModelLaunch::from_declaration(&pinned)
            .expect("pinned launch");
        assert_eq!(launch.engine.launch_identity, IMAGE_ID);
        assert_eq!(
            launch.engine.preparation_policy,
            ArtifactPreparationPolicy::LocalDevelopment
        );
        assert_eq!(
            unpinned_engine_selection(&pinned).map(|(selection, _)| selection),
            Some(selection),
            "the retained snapshot itself is never changed by a pin"
        );
    }

    #[test]
    fn only_node_resolution_admits_an_empty_engine_identity_and_its_one_pin() {
        let unpinned = node_resolved_declaration();
        let pinned = with_node_pins(&unpinned, vec![engine_pin(IMAGE_ID)]);
        let node = LaunchIdentityResolution::NodeResolved;
        let retained = LaunchIdentityResolution::Retained;

        assert!(validate_local_declaration(&unpinned, node).is_ok());
        assert!(validate_local_declaration(&pinned, node).is_ok());
        assert!(validate_local_declaration(&unpinned, retained).is_err());
        assert!(validate_local_declaration(&pinned, retained).is_err());
        for pins in [
            vec![NodeResolvedLaunchIdentity {
                selection_key: "web".to_owned(),
                launch_identity: IMAGE_ID.to_owned(),
            }],
            vec![engine_pin("registry.dotcore.test/fixture:dev")],
            vec![engine_pin(IMAGE_ID), engine_pin(IMAGE_ID)],
        ] {
            let invalid = with_node_pins(&unpinned, pins);
            assert!(validate_local_declaration(&invalid, node).is_err());
        }

        let owner_resolved = owner_resolved_declaration();
        assert!(validate_local_declaration(&owner_resolved, retained).is_ok());
        assert!(unpinned_engine_selection(&owner_resolved).is_none());
        let pinned_owner_resolved = with_node_pins(&owner_resolved, vec![engine_pin(IMAGE_ID)]);
        assert!(
            validate_local_declaration(&pinned_owner_resolved, node).is_err(),
            "an engine whose snapshot names its identity takes no node pin"
        );
    }

    #[test]
    fn a_development_source_engine_takes_one_store_path_pin_and_launches_from_it() {
        let unpinned = development_source_declaration("");
        let node = LaunchIdentityResolution::NodeResolved;
        let pinned = with_node_pins(&unpinned, vec![engine_pin(STORE_PATH)]);

        let (selection, platform) = unpinned_engine_selection(&unpinned).expect("unpinned engine");
        assert_eq!(
            selection.source,
            PinSource::NixDevelopmentSource {
                package: "engines.llama-cpp.native".to_owned(),
                docker_image: false,
            }
        );
        assert_eq!(
            (
                platform.operating_system.as_str(),
                platform.architecture.as_str()
            ),
            ("linux", "amd64")
        );
        assert!(validate_local_declaration(&unpinned, node).is_ok());
        assert!(validate_local_declaration(&pinned, node).is_ok());
        assert!(
            validate_local_declaration(
                &with_node_pins(&unpinned, vec![engine_pin(IMAGE_ID)]),
                node
            )
            .is_err(),
            "a development-source engine takes no image ID"
        );
        assert!(
            validate_local_declaration(
                &with_node_pins(&node_resolved_declaration(), vec![engine_pin(STORE_PATH)]),
                node
            )
            .is_err(),
            "a local-development image takes no store path"
        );
        assert_eq!(
            crate::model::resolved::ResolvedModelLaunch::from_declaration(&pinned)
                .expect("pinned launch")
                .engine
                .launch_identity,
            STORE_PATH
        );
        assert!(crate::model::resolved::ResolvedModelLaunch::from_declaration(&unpinned).is_err());

        let retained = development_source_declaration(STORE_PATH);
        assert!(unpinned_engine_selection(&retained).is_none());
        assert!(
            validate_local_declaration(&retained, node).is_err(),
            "the snapshot never retains a development-source store path"
        );
    }

    #[test]
    fn a_locked_nix_engine_takes_the_store_path_pin_of_the_node_system() {
        const OTHER_STORE_PATH: &str =
            "/nix/store/11111111111111111111111111111111-llama-cpp-b11417";
        let mut unpinned = owner_resolved_declaration();
        let mut snapshot = crate::test_support::nix::apple_metal_snapshot("fixture-model");
        let Some(Source::Local(local)) = snapshot.source.as_mut() else {
            panic!("local snapshot");
        };
        local
            .engine_artifact
            .as_mut()
            .expect("engine artifact")
            .launch_identity
            .clear();
        unpinned.snapshot = Some(snapshot);
        let node = LaunchIdentityResolution::NodeResolved;

        let (selection, _) = unpinned_engine_selection(&unpinned).expect("unpinned engine");
        assert_eq!(
            selection.source,
            PinSource::NixLocked {
                store_paths: vec![("aarch64-darwin".to_owned(), STORE_PATH.to_owned())],
            }
        );
        assert!(validate_local_declaration(&unpinned, node).is_ok());
        assert!(crate::model::resolved::ResolvedModelLaunch::from_declaration(&unpinned).is_err());

        let pinned = with_node_pins(&unpinned, vec![engine_pin(STORE_PATH)]);
        assert_eq!(
            crate::model::resolved::ResolvedModelLaunch::from_declaration(&pinned)
                .expect("pinned launch")
                .engine
                .launch_identity,
            STORE_PATH
        );
        assert!(
            crate::model::resolved::ResolvedModelLaunch::from_declaration(&with_node_pins(
                &unpinned,
                vec![engine_pin(OTHER_STORE_PATH)]
            ))
            .is_err(),
            "a pin outside the retained store paths never launches"
        );
    }

    #[tokio::test]
    async fn a_docker_delivered_engine_pins_and_launches_the_loader_of_the_node_system() {
        use dotcore_domain::deployment::launch_pin::LaunchPinResolver as _;

        let store_paths = crate::test_support::nix::DOCKER_IMAGE_STORE_PATHS;
        let cache = tempfile::tempdir().expect("cache");
        // A Linux node targets its own platform; a Mac targets linux/arm64 in Docker Desktop.
        for (system, target_architecture) in
            [("x86_64-linux", "amd64"), ("aarch64-darwin", "arm64")]
        {
            let mut declaration = owner_resolved_declaration();
            let snapshot = declaration.snapshot.as_mut().expect("snapshot");
            crate::test_support::nix::retain_docker_delivered_engine(
                snapshot,
                &dotcore_domain::test_support::nix::locked_flake(),
                "",
            );
            let Some(Source::Local(local)) = snapshot.source.as_mut() else {
                panic!("local snapshot");
            };
            local
                .engine_artifact
                .as_mut()
                .expect("engine artifact")
                .platform
                .as_mut()
                .expect("engine platform")
                .architecture = target_architecture.to_owned();
            let (selection, platform) =
                unpinned_engine_selection(&declaration).expect("unpinned engine");
            assert_eq!(
                selection.source,
                PinSource::NixLocked {
                    store_paths: store_paths
                        .map(|(system, path)| (system.to_owned(), path.to_owned()))
                        .to_vec(),
                }
            );
            let manager = dotcore_artifact::ArtifactManager::new(cache.path().to_path_buf(), None)
                .expect("artifact manager")
                .with_nix_system(Some(system));

            let pin = manager
                .resolve_pin(&selection.source, &platform, CancellationToken::new())
                .await
                .expect("pinned store path");

            let expected = store_paths
                .iter()
                .find(|(candidate, _)| *candidate == system)
                .map(|(_, path)| *path)
                .expect("system store path");
            assert_eq!(pin, expected, "{system}");
            let launch = crate::model::resolved::ResolvedModelLaunch::from_declaration(
                &with_node_pins(&declaration, vec![engine_pin(&pin)]),
            )
            .expect("pinned launch");
            assert_eq!(launch.engine.launch_identity, expected);
            assert_eq!(
                launch.engine.container_image(),
                dotcore_domain::nix::docker_launch_reference(expected),
                "{system}"
            );
            assert!(launch.engine.container_image().is_some_and(|reference| {
                reference.starts_with("dotcore-nix/docker-image-llama-cpp-cuda:")
            }));
        }

        let mut development = owner_resolved_declaration();
        crate::test_support::nix::retain_docker_delivered_engine(
            development.snapshot.as_mut().expect("snapshot"),
            "self",
            "",
        );
        let (selection, _) = unpinned_engine_selection(&development).expect("unpinned engine");
        assert_eq!(
            selection.source,
            PinSource::NixDevelopmentSource {
                package: "engines.llama-cpp-cuda.docker".to_owned(),
                docker_image: true,
            }
        );
        let (_, evaluated) = store_paths[0];
        let launch = crate::model::resolved::ResolvedModelLaunch::from_declaration(
            &with_node_pins(&development, vec![engine_pin(evaluated)]),
        )
        .expect("pinned development-source launch");
        assert_eq!(
            launch.engine.container_image(),
            dotcore_domain::nix::docker_launch_reference(evaluated)
        );
        assert!(
            crate::model::resolved::ResolvedModelLaunch::from_declaration(&with_node_pins(
                &development,
                vec![engine_pin(STORE_PATH)],
            ))
            .is_err(),
            "a native store path never launches a Docker-delivered engine"
        );
    }

    fn development_source_declaration(launch_identity: &str) -> ModelDeploymentDeclaration {
        let mut declaration = owner_resolved_declaration();
        crate::test_support::nix::retain_development_source_engine(
            declaration.snapshot.as_mut().expect("snapshot"),
            launch_identity,
        );
        declaration
    }

    fn node_resolved_declaration() -> ModelDeploymentDeclaration {
        let mut declaration = owner_resolved_declaration();
        let Some(Source::Local(local)) = declaration
            .snapshot
            .as_mut()
            .and_then(|snapshot| snapshot.source.as_mut())
        else {
            panic!("local snapshot");
        };
        local
            .engine_artifact
            .as_mut()
            .expect("engine artifact")
            .launch_identity
            .clear();
        declaration
    }

    fn owner_resolved_declaration() -> ModelDeploymentDeclaration {
        ModelDeploymentDeclaration {
            model_deployment_id: "deployment".to_owned(),
            snapshot: Some(crate::test_support::tag_only_engine_snapshot(IMAGE_ID)),
            ..Default::default()
        }
    }

    fn engine_pin(launch_identity: &str) -> NodeResolvedLaunchIdentity {
        NodeResolvedLaunchIdentity {
            selection_key: ENGINE_SELECTION_KEY.to_owned(),
            launch_identity: launch_identity.to_owned(),
        }
    }
}
