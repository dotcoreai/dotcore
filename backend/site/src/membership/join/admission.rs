//! Decides what the site records for a joining node's proposal: its SiteNode identity, the two
//! addresses between it and the manager, its execution targets derived the same way site
//! initialization derives the manager's own, and the observation configuration the node installs.
//!
//! A declared node that joins again from its intact folder is admitted only with proof that it
//! holds the key the site pinned for it, made for this join; it keeps its identity, targets, and
//! deployments, and only its two addresses may change.
//!
//! A proposal whose name or host another node already uses is refused with a distinct reason, and
//! any other conflict with the site is refused with the site's own validation message. Nothing
//! is written here.

use dotcore_domain::SiteNodeHost;
use dotcore_proto::dotcore::{
    node::v1::NodeObservationConfiguration,
    site::v1::{ExecutionTarget, Site, SiteNode},
};

use super::{
    JoinProposal, JoinSecret, JoinSiteError, NodeKeyError,
    node_key::{CertificateTime, certificate_key, certificate_time, rejoin_request_key},
    publication::{site_with_node, site_with_node_hosts},
};
use crate::{
    initial::{inputs::build_execution_targets, request::derive_site_node_id},
    membership::removal::placed_deployment_count,
    node::{
        folder::CertificateFingerprint, observation_configuration::node_observation_configuration,
    },
    private_transport::exact_node_identity,
    validation::SiteDeclarationRules,
};

/// The node, targets, and observation configuration derived for an acceptable proposal.
pub(super) struct Admission {
    pub(super) node: SiteNode,
    pub(super) targets: Vec<ExecutionTarget>,
    pub(super) observation: NodeObservationConfiguration,
}

/// A declared node admitted to join again, with its addresses as the site will record them.
pub(super) struct RejoinAdmission {
    /// The node as the site declares it now.
    pub(super) declared: SiteNode,

    /// The node with its new addresses; equal to `declared` when nothing changes.
    pub(super) node: SiteNode,

    /// The node's declared execution targets, unchanged.
    pub(super) targets: Vec<ExecutionTarget>,

    pub(super) observation: NodeObservationConfiguration,

    /// Model and application deployments placed on the node, which stay on it.
    pub(super) deployments: usize,
}

/// What a rejoining node presents to prove it holds the declared node's identity for one join.
pub(super) struct RejoinProof<'a> {
    /// The node's existing leaf certificate.
    pub(super) certificate_der: &'a [u8],

    /// Signing request made with the node's key and bound to `secret`.
    pub(super) csr_der: &'a [u8],

    /// Join secret of the session the request must be bound to.
    pub(super) secret: &'a JoinSecret,
}

/// Derives the node and target identities for `proposal` and checks them against `site`.
///
/// `host` is where the manager reaches the node and `manager_host` where the node reaches the
/// manager. The node records `manager_host` only when it differs from the declared manager host.
/// Hosts compare in canonical form, so a name differing only by case is the same host.
pub(super) fn admit(
    site: &Site,
    rules: &SiteDeclarationRules,
    proposal: &JoinProposal,
    host: &SiteNodeHost,
    manager_host: &SiteNodeHost,
) -> Result<Admission, AdmissionError> {
    let host = host.to_string();
    if site
        .nodes
        .iter()
        .any(|node| node.display_name == proposal.display_name)
    {
        return Err(AdmissionError::DuplicateName {
            display_name: proposal.display_name.clone(),
        });
    }
    if let Some(existing) = site.nodes.iter().find(|node| node.host == host) {
        return Err(AdmissionError::DuplicateHost {
            host,
            display_name: existing.display_name.clone(),
        });
    }

    let node_id = derive_site_node_id(&site.site_id, &proposal.display_name);
    let node = SiteNode {
        node_id: node_id.clone(),
        display_name: proposal.display_name.clone(),
        site_manager: None,
        host,
        manager_host: recorded_manager_host(site, manager_host),
    };
    let targets = build_execution_targets(&node_id, &proposal.targets);
    let updated = site_with_node(site, rules, &node, &targets)
        .map_err(|error| AdmissionError::Site(JoinSiteError::from(error)))?;
    let observation = node_observation_configuration(&updated, &node_id).map_err(|error| {
        AdmissionError::Site(JoinSiteError::Rejected(anyhow::Error::new(error)))
    })?;
    Ok(Admission {
        node,
        targets,
        observation,
    })
}

/// Returns the SiteNode identity a rejoining node's certificate names in `site`, so its pin can
/// be looked up before [`admit_rejoin`].
pub(super) fn certified_node_id(
    site: &Site,
    certificate_der: &[u8],
) -> Result<String, AdmissionError> {
    exact_node_identity(certificate_der, &site.site_id).ok_or(AdmissionError::ForeignCertificate)
}

/// Checks that the declared node `node_id`, named by `proof`'s certificate, may join again from
/// `host`, or from its declared host when `None`, reaching the manager at `manager_host`, and
/// returns it with its new addresses.
///
/// The node must be a declared worker, its certificate the one `pin` records and not expired,
/// and the signing request signed by that certificate's key and bound to this join's secret.
/// Its host may be its own current one but no other node's.
pub(super) fn admit_rejoin(
    site: &Site,
    rules: &SiteDeclarationRules,
    node_id: &str,
    proof: &RejoinProof<'_>,
    pin: Option<&CertificateFingerprint>,
    host: Option<&SiteNodeHost>,
    manager_host: &SiteNodeHost,
) -> Result<RejoinAdmission, AdmissionError> {
    let declared = site
        .nodes
        .iter()
        .find(|node| node.node_id == node_id)
        .ok_or_else(|| AdmissionError::NodeNotDeclared {
            node_id: node_id.to_owned(),
        })?;
    if declared.site_manager.is_some() {
        return Err(AdmissionError::ManagerNode);
    }
    if pin
        != Some(&CertificateFingerprint::for_certificate(
            proof.certificate_der,
        ))
    {
        return Err(AdmissionError::CertificateNotPinned {
            display_name: declared.display_name.clone(),
        });
    }
    match certificate_time(proof.certificate_der).ok_or(AdmissionError::ForeignCertificate)? {
        CertificateTime::Valid => {}
        CertificateTime::NotYetValid => {
            return Err(AdmissionError::CertificateNotYetValid {
                display_name: declared.display_name.clone(),
            });
        }
        CertificateTime::Expired => {
            return Err(AdmissionError::CertificateExpired {
                display_name: declared.display_name.clone(),
            });
        }
    }
    let proven_key =
        rejoin_request_key(proof.csr_der, proof.secret).map_err(AdmissionError::InvalidProof)?;
    if certificate_key(proof.certificate_der).as_deref() != Some(proven_key.as_slice()) {
        return Err(AdmissionError::ProofKeyMismatch);
    }
    let host = host.map_or_else(|| declared.host.clone(), ToString::to_string);
    if let Some(existing) = site
        .nodes
        .iter()
        .find(|node| node.node_id != node_id && node.host == host)
    {
        return Err(AdmissionError::DuplicateHost {
            host,
            display_name: existing.display_name.clone(),
        });
    }

    let node = SiteNode {
        host,
        manager_host: recorded_manager_host(site, manager_host),
        ..declared.clone()
    };
    let updated = site_with_node_hosts(site, rules, &node)
        .map_err(|error| AdmissionError::Site(JoinSiteError::from(error)))?;
    let observation = node_observation_configuration(&updated, node_id).map_err(|error| {
        AdmissionError::Site(JoinSiteError::Rejected(anyhow::Error::new(error)))
    })?;
    Ok(RejoinAdmission {
        declared: declared.clone(),
        node,
        targets: site
            .execution_targets
            .iter()
            .filter(|target| target.site_node_id == node_id)
            .cloned()
            .collect(),
        observation,
        deployments: placed_deployment_count(site, node_id),
    })
}

/// Why a proposal cannot be admitted into the site.
#[derive(Debug, thiserror::Error)]
pub(super) enum AdmissionError {
    #[error(
        "a node named {display_name:?} already exists in this site; choose another name, or \
         remove that node first if this machine replaces it"
    )]
    DuplicateName { display_name: String },

    #[error(
        "host {host} is already used by node {display_name:?}; \
         provide the address the manager reaches this node at"
    )]
    DuplicateHost { host: String, display_name: String },

    #[error("the node's certificate does not name a node of this site")]
    ForeignCertificate,

    #[error(
        "node {node_id} is no longer declared in this site; leave this machine's node folder, \
         then join as a new node"
    )]
    NodeNotDeclared { node_id: String },

    #[error("the site manager's own node cannot join again")]
    ManagerNode,

    #[error(
        "the node's certificate is not the one this site pins for node {display_name:?}; leave \
         this machine's node folder, then join as a new node"
    )]
    CertificateNotPinned { display_name: String },

    #[error(
        "the certificate of node {display_name:?} has expired; remove the node, leave this \
         machine's node folder, then join as a new node"
    )]
    CertificateExpired { display_name: String },

    #[error(
        "the certificate of node {display_name:?} is not valid yet; check the clocks of the \
         manager and the joining node"
    )]
    CertificateNotYetValid { display_name: String },

    #[error("the join request does not prove the node holds its key for this join: {0}")]
    InvalidProof(#[source] NodeKeyError),

    #[error("the join request is signed by another key than the node's certificate")]
    ProofKeyMismatch,

    #[error(transparent)]
    Site(JoinSiteError),
}

/// The manager host the site records for a node reaching the manager at `manager_host`: empty
/// when it is the declared manager host.
fn recorded_manager_host(site: &Site, manager_host: &SiteNodeHost) -> String {
    let manager_host = manager_host.to_string();
    let declared = site
        .nodes
        .iter()
        .find(|node| node.site_manager.is_some())
        .map(|node| node.host.as_str());
    if declared == Some(manager_host.as_str()) {
        String::new()
    } else {
        manager_host
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use dotcore_domain::ExecutionPlatform;
    use dotcore_proto::dotcore::{
        observation::v1::{AcceleratorFacts, AcceleratorVendor},
        site::v1::{
            NativeRuntimeAdapter, NativeWorkloadLifetime, execution_target, model_runtime_binding,
        },
    };

    use super::*;
    use crate::{
        initial::{
            inputs::{ApplicationTargetChoice, ModelTargetChoice, NodeTargetChoices},
            request::{
                InitialApplicationRuntime, InitialInferenceRuntime,
                derive_application_execution_target_id, derive_model_execution_target_id,
            },
        },
        membership::join::test_fixtures::{initialized_site, worker_node, worker_targets},
    };

    #[test]
    fn a_host_differing_from_an_existing_one_only_by_case_is_a_duplicate() {
        let fixture = initialized_site();
        let mut site = fixture.site.clone();
        site.nodes
            .push(worker_node("node-gpu", "gpu-box", "gpu-box.example.ts.net"));
        site.execution_targets.extend(worker_targets("node-gpu"));
        let host = SiteNodeHost::normalize("GPU-Box.Example.TS.net.").expect("input host");

        let error = admit(
            &site,
            &crate::test_support::site_declaration_rules(),
            &proposal("other-box"),
            &host,
            &SiteNodeHost::LOOPBACK,
        )
        .err()
        .expect("duplicate host");

        assert!(matches!(
            &error,
            AdmissionError::DuplicateHost { host, display_name }
                if host == "gpu-box.example.ts.net" && display_name == "gpu-box"
        ));
    }

    #[test]
    fn a_named_node_records_a_manager_host_only_when_it_differs_from_the_declared_one() {
        let fixture = initialized_site();
        let host = SiteNodeHost::parse("gpu-box.example.ts.net").expect("host");

        let declared = admit(
            &fixture.site,
            &crate::test_support::site_declaration_rules(),
            &proposal("gpu-box"),
            &host,
            &SiteNodeHost::LOOPBACK,
        )
        .unwrap_or_else(|error| panic!("admitted: {error}"));
        assert_eq!(declared.node.host, "gpu-box.example.ts.net");
        assert_eq!(declared.node.manager_host, "");

        let manager_host = SiteNodeHost::parse("localhost").expect("manager host");
        let other = admit(
            &fixture.site,
            &crate::test_support::site_declaration_rules(),
            &proposal("gpu-box"),
            &host,
            &manager_host,
        )
        .unwrap_or_else(|error| panic!("admitted: {error}"));
        assert_eq!(other.node.manager_host, "localhost");
    }

    #[test]
    fn apple_proposal_admits_a_native_darwin_target_with_its_device_and_lifetime() {
        let site = initialized_site().site;
        let host = SiteNodeHost::from_ipv4(Ipv4Addr::new(192, 0, 2, 9)).expect("host");
        let proposal = JoinProposal {
            display_name: "mac-studio".to_owned(),
            host: Some(host.clone()),
            private_port: 7443,
            targets: NodeTargetChoices {
                model_targets: vec![ModelTargetChoice::Apple {
                    device_id: "apple-gpu-0".to_owned(),
                    lifetime: NativeWorkloadLifetime::Ephemeral,
                }],
                application_targets: vec![application(InitialApplicationRuntime::Simulated)],
                accelerator_facts: vec![AcceleratorFacts {
                    vendor: AcceleratorVendor::Apple as i32,
                    model: "Apple M3 Max".to_owned(),
                    device_id: "apple-gpu-0".to_owned(),
                    vendor_index: Some(0),
                    ..AcceleratorFacts::default()
                }],
                declared_machine: None,
            },
        };
        let manager_host = SiteNodeHost::from_ipv4(Ipv4Addr::new(192, 0, 2, 1)).expect("host");

        let admission = admit(
            &site,
            &crate::test_support::site_declaration_rules(),
            &proposal,
            &host,
            &manager_host,
        )
        .expect("admit Apple node");

        let model = &admission.targets[0];
        assert_eq!(model.display_name, "Apple M3 Max");
        assert_eq!(admission.targets[1].display_name, "Simulated applications");
        assert_eq!(
            model.platform,
            Some(ExecutionPlatform::darwin_arm64().to_oci_platform())
        );
        assert_eq!(model.accelerator_device_ids, ["apple-gpu-0"]);
        let Some(execution_target::RuntimeBinding::ModelRuntime(binding)) = &model.runtime_binding
        else {
            panic!("model binding")
        };
        assert_eq!(
            binding.adapter,
            Some(model_runtime_binding::Adapter::NativeRuntime(
                NativeRuntimeAdapter {
                    workload_lifetime: NativeWorkloadLifetime::Ephemeral as i32,
                }
            ))
        );
    }

    #[test]
    fn nvidia_proposal_admits_one_named_model_target_per_selected_gpu() {
        let site = initialized_site().site;
        let host = SiteNodeHost::from_ipv4(Ipv4Addr::new(192, 0, 2, 9)).expect("host");
        let facts = (0..2)
            .map(|index| AcceleratorFacts {
                vendor: AcceleratorVendor::Nvidia as i32,
                model: "NVIDIA GeForce RTX 5090".to_owned(),
                device_id: format!("GPU-{index}"),
                vendor_index: Some(index),
                ..AcceleratorFacts::default()
            })
            .collect::<Vec<_>>();

        for (selectors, expected) in [
            (
                vec!["nvidia.com/gpu=gpu0", "nvidia.com/gpu=gpu1"],
                vec![
                    "NVIDIA GeForce RTX 5090 · GPU 0",
                    "NVIDIA GeForce RTX 5090 · GPU 1",
                    "Docker applications",
                ],
            ),
            (
                vec!["nvidia.com/gpu=gpu1"],
                vec!["NVIDIA GeForce RTX 5090 · GPU 1", "Docker applications"],
            ),
        ] {
            let proposal = JoinProposal {
                targets: NodeTargetChoices {
                    model_targets: selectors.iter().map(|selector| nvidia(selector)).collect(),
                    application_targets: vec![application(InitialApplicationRuntime::Docker)],
                    accelerator_facts: facts.clone(),
                    declared_machine: None,
                },
                ..proposal("gpu-box")
            };

            let admission = admit(
                &site,
                &crate::test_support::site_declaration_rules(),
                &proposal,
                &host,
                &SiteNodeHost::LOOPBACK,
            )
            .expect("admit NVIDIA node");

            let names = admission
                .targets
                .iter()
                .map(|target| target.display_name.as_str())
                .collect::<Vec<_>>();
            assert_eq!(names, expected);
            let node_id = &admission.node.node_id;
            let mut ids = selectors
                .iter()
                .map(|selector| {
                    derive_model_execution_target_id(
                        node_id,
                        InitialInferenceRuntime::Nvidia,
                        Some(selector),
                    )
                })
                .collect::<Vec<_>>();
            ids.push(derive_application_execution_target_id(
                node_id,
                InitialApplicationRuntime::Docker,
            ));
            let actual = admission
                .targets
                .iter()
                .map(|target| target.execution_target_id.clone())
                .collect::<Vec<_>>();
            assert_eq!(actual, ids);
            let devices = admission
                .targets
                .iter()
                .map(|target| target.accelerator_device_ids.clone())
                .collect::<Vec<_>>();
            let mut expected_devices = selectors
                .iter()
                .map(|selector| vec![(*selector).to_owned()])
                .collect::<Vec<_>>();
            expected_devices.push(Vec::new());
            assert_eq!(devices, expected_devices);
            assert_eq!(
                admission.observation.execution_targets.len(),
                selectors.len() + 1
            );
        }
    }

    #[test]
    fn mixed_proposal_admits_every_target_with_distinct_identities() {
        let site = initialized_site().site;
        let host = SiteNodeHost::from_ipv4(Ipv4Addr::new(192, 0, 2, 9)).expect("host");
        let proposal = JoinProposal {
            targets: NodeTargetChoices {
                model_targets: vec![
                    ModelTargetChoice::Simulated {
                        platform: ExecutionPlatform::linux_amd64(),
                        device_id: "GPU-00000000-0000-0000-0000-000000005090".to_owned(),
                    },
                    nvidia("nvidia.com/gpu=gpu0"),
                ],
                application_targets: vec![
                    application(InitialApplicationRuntime::Docker),
                    application(InitialApplicationRuntime::Simulated),
                ],
                accelerator_facts: vec![AcceleratorFacts {
                    vendor: AcceleratorVendor::Nvidia as i32,
                    model: "NVIDIA GeForce RTX 5090".to_owned(),
                    device_id: "GPU-0".to_owned(),
                    vendor_index: Some(0),
                    ..AcceleratorFacts::default()
                }],
                declared_machine: None,
            },
            ..proposal("gpu-box")
        };

        let admission = admit(
            &site,
            &crate::test_support::site_declaration_rules(),
            &proposal,
            &host,
            &SiteNodeHost::LOOPBACK,
        )
        .expect("admit mixed node");

        let node_id = &admission.node.node_id;
        let targets = admission
            .targets
            .iter()
            .map(|target| {
                (
                    target.execution_target_id.clone(),
                    target.display_name.as_str(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            targets,
            [
                (
                    derive_model_execution_target_id(
                        node_id,
                        InitialInferenceRuntime::Simulated,
                        Some("GPU-00000000-0000-0000-0000-000000005090")
                    ),
                    "Simulated GPU"
                ),
                (
                    derive_model_execution_target_id(
                        node_id,
                        InitialInferenceRuntime::Nvidia,
                        Some("nvidia.com/gpu=gpu0")
                    ),
                    "NVIDIA GeForce RTX 5090"
                ),
                (
                    derive_application_execution_target_id(
                        node_id,
                        InitialApplicationRuntime::Docker
                    ),
                    "Docker applications"
                ),
                (
                    derive_application_execution_target_id(
                        node_id,
                        InitialApplicationRuntime::Simulated
                    ),
                    "Simulated applications"
                ),
            ]
        );
        assert_eq!(admission.observation.execution_targets.len(), 4);
    }

    #[test]
    fn proposal_without_targets_admits_a_node_with_none() {
        let site = initialized_site().site;
        let host = SiteNodeHost::from_ipv4(Ipv4Addr::new(192, 0, 2, 9)).expect("host");
        let proposal = JoinProposal {
            targets: NodeTargetChoices::default(),
            ..proposal("empty-box")
        };

        let admission = admit(
            &site,
            &crate::test_support::site_declaration_rules(),
            &proposal,
            &host,
            &SiteNodeHost::LOOPBACK,
        )
        .expect("admit node without targets");

        assert_eq!(admission.node.display_name, "empty-box");
        assert!(admission.targets.is_empty());
        assert!(admission.observation.execution_targets.is_empty());
    }

    fn proposal(display_name: &str) -> JoinProposal {
        JoinProposal {
            display_name: display_name.to_owned(),
            host: None,
            private_port: 7443,
            targets: NodeTargetChoices {
                model_targets: vec![ModelTargetChoice::Simulated {
                    platform: ExecutionPlatform::linux_amd64(),
                    device_id: "GPU-00000000-0000-0000-0000-000000005090".to_owned(),
                }],
                application_targets: vec![application(InitialApplicationRuntime::Simulated)],
                accelerator_facts: Vec::new(),
                declared_machine: None,
            },
        }
    }

    fn nvidia(selector: &str) -> ModelTargetChoice {
        ModelTargetChoice::Nvidia {
            selector: selector.to_owned(),
            platform: ExecutionPlatform::linux_amd64(),
        }
    }

    fn application(applications: InitialApplicationRuntime) -> ApplicationTargetChoice {
        ApplicationTargetChoice {
            applications,
            platform: ExecutionPlatform::linux_amd64(),
            accelerator_device_ids: Vec::new(),
        }
    }
}
