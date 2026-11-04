//! Shared fixtures for the join building-block tests: a fully initialized offline site with its
//! trust material, and a worker node with its two execution targets.

use std::path::PathBuf;

use dotcore_domain::{ExecutionPlatform, test_support::canonical_tempdir};
use dotcore_proto::dotcore::site::v1::{ExecutionTarget, Site, SiteNode, execution_target};

use crate::{
    SITE_FILE,
    initial::{
        input_store::InitialSiteInputWriter,
        inputs::{
            ApplicationTargetChoice, ModelTargetChoice, NodeTargetChoices, build_execution_targets,
        },
        request::{InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteRequest},
        validation::SiteInputBundleValidator,
    },
    test_support::initial_site_inputs_for,
    validation::load_validated_site,
};

/// An initialized site directory removed when dropped.
pub(crate) struct InitializedSite {
    pub(crate) _directory: tempfile::TempDir,
    pub(crate) root: PathBuf,
    pub(crate) site: Site,
}

pub(crate) fn initialized_site() -> InitializedSite {
    initialized_site_with(InitialSiteRequest::new(
        [InitialInferenceRuntime::Simulated],
        [InitialApplicationRuntime::Simulated],
    ))
}

/// An initialized site whose manager is declared at `host`, an address or host name.
pub(crate) fn initialized_site_with_manager_host(host: &str) -> InitializedSite {
    initialized_site_with(
        InitialSiteRequest::new(
            [InitialInferenceRuntime::Simulated],
            [InitialApplicationRuntime::Simulated],
        )
        .with_site_node_host_override(host)
        .expect("manager host"),
    )
}

fn initialized_site_with(request: InitialSiteRequest) -> InitializedSite {
    let directory = canonical_tempdir().expect("Site fixture directory");
    let root = directory.path().join("site");
    InitialSiteInputWriter::new(SiteInputBundleValidator::for_test())
        .initialize(&root, &initial_site_inputs_for(&request), &request, false)
        .expect("initialize site fixture");
    let site = load_validated_site(
        &root.join(SITE_FILE),
        &crate::test_support::site_declaration_rules(),
    )
    .expect("Site fixture loads");
    InitializedSite {
        _directory: directory,
        root,
        site,
    }
}

pub(crate) fn worker_node(node_id: &str, display_name: &str, host: &str) -> SiteNode {
    SiteNode {
        node_id: node_id.to_owned(),
        display_name: display_name.to_owned(),
        site_manager: None,
        host: host.to_owned(),
        manager_host: String::new(),
    }
}

pub(crate) fn worker_targets(node_id: &str) -> Vec<ExecutionTarget> {
    build_execution_targets(
        node_id,
        &NodeTargetChoices {
            model_targets: vec![ModelTargetChoice::Simulated {
                platform: ExecutionPlatform::linux_amd64(),
                device_id: "GPU-00000000-0000-0000-0000-000000005090".to_owned(),
            }],
            application_targets: vec![ApplicationTargetChoice {
                applications: InitialApplicationRuntime::Simulated,
                platform: ExecutionPlatform::linux_amd64(),
                accelerator_device_ids: Vec::new(),
            }],
            accelerator_facts: Vec::new(),
            declared_machine: None,
        },
    )
    // Readable fixture identities, so tests name each target directly.
    .into_iter()
    .map(|mut target| {
        let role = match target.runtime_binding {
            Some(execution_target::RuntimeBinding::ModelRuntime(_)) => "model",
            _ => "application",
        };
        target.execution_target_id = format!("{node_id}-{role}-target");
        target
    })
    .collect()
}
