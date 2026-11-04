//! Encodes and decodes the native owner record: the ownership evidence and symbolic launch intent
//! stored inside each workload's launcher launch specification.
//!
//! The record names the site, SiteNode, execution target, and canonical state root that created
//! the workload, plus the workload spec exactly as its owner issued it and the loopback ports
//! assigned to it, so a later scan can recompute the workload's identity without trusting the
//! launcher's resolved command. Decoding rejects any other record format version.

use std::{collections::BTreeMap, path::PathBuf};

use dotcore_domain::{ExecutionPlatform, SiteNodeId};
use dotcore_proto::dotcore::runtime::store::v1 as store;
use dotcore_runtime::{
    ExecutionTarget, WorkloadArtifactBinding, WorkloadCorrelation, WorkloadExecutable,
    WorkloadExecutableArtifact, WorkloadIpcMode, WorkloadLaunch, WorkloadPort, WorkloadSpec,
    WorkloadSpecError, WorkloadValue,
};
use prost::Message;

/// Version of the stored native owner record format.
pub(super) const SCHEMA_VERSION: u32 = 1;

/// Version of the reconcile generation, launch fingerprint, and command resolution algorithms.
/// A workload recorded under another version is replaced, never adopted.
pub(super) const IDENTITY_VERSION: u32 = 1;

/// Ownership evidence and symbolic launch intent of one native workload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NativeOwnerRecord {
    pub(super) identity_version: u32,
    /// Canonical absolute path of the native state root that created the workload.
    pub(super) state_root: String,
    pub(super) site_id: String,
    pub(super) execution_target_id: String,
    /// Owner launch as issued, with its recorded correlation; the SiteNode is the execution
    /// target's managed node.
    pub(super) launch: WorkloadLaunch,
    /// Loopback port assigned to each declared port, by port name.
    pub(super) ports: BTreeMap<String, u16>,
}

impl NativeOwnerRecord {
    /// Encodes the record; artifact sources must be UTF-8 and the spec must carry a Nix store
    /// path executable and no cache attachment, which launch validation guarantees.
    pub(super) fn encode(&self) -> Result<Vec<u8>, NativeRecordError> {
        let spec = &self.launch.spec;
        let WorkloadExecutableArtifact::NixStorePath(nix_store_path) = &spec.executable.artifact
        else {
            return Err(NativeRecordError::Invalid {
                field: "spec.nix_store_path",
            });
        };
        if !spec.storage.is_empty() {
            return Err(NativeRecordError::Invalid {
                field: "spec.storage",
            });
        }
        let artifacts = spec
            .artifacts
            .iter()
            .map(|binding| {
                Ok(store::ArtifactBinding {
                    name: binding.name.clone(),
                    source: binding
                        .source
                        .to_str()
                        .ok_or(NativeRecordError::Invalid {
                            field: "spec.artifacts.source",
                        })?
                        .to_owned(),
                    destination: binding.destination.clone(),
                    read_only: binding.read_only,
                })
            })
            .collect::<Result<_, NativeRecordError>>()?;
        let ipc_mode = match spec.ipc_mode {
            WorkloadIpcMode::Isolated => store::IpcMode::Isolated,
            WorkloadIpcMode::HostShared => store::IpcMode::HostShared,
        };
        Ok(store::NativeWorkloadRecord {
            schema_version: SCHEMA_VERSION,
            identity_version: self.identity_version,
            state_root: self.state_root.clone(),
            site_id: self.site_id.clone(),
            site_node_id: self
                .launch
                .execution_target
                .managed_node_id
                .as_str()
                .to_owned(),
            execution_target_id: self.execution_target_id.clone(),
            owner_id: self.launch.correlation.owner_id.clone(),
            reconcile_generation: self.launch.correlation.reconcile_generation,
            target_accelerator_device_ids: self
                .launch
                .execution_target
                .accelerator_device_ids
                .clone(),
            spec: Some(store::NativeWorkloadSpec {
                nix_store_path: nix_store_path.clone(),
                executable_path: spec.executable.path.clone(),
                operating_system: spec.required_platform.operating_system().to_owned(),
                architecture: spec.required_platform.architecture().to_owned(),
                arguments: spec.arguments.iter().map(ToString::to_string).collect(),
                environment: spec
                    .environment
                    .iter()
                    .map(|(name, value)| (name.clone(), value.to_string()))
                    .collect(),
                artifacts,
                ports: spec
                    .ports
                    .iter()
                    .map(|port| store::DeclaredPort {
                        name: port.name.clone(),
                        container_port: port.container_port.into(),
                    })
                    .collect(),
                accelerator_device_ids: spec.accelerator_device_ids.clone(),
                required_accelerator_device_count: spec.required_accelerator_device_count,
                ipc_mode: ipc_mode.into(),
            }),
            assigned_ports: self
                .ports
                .iter()
                .map(|(name, port)| store::AssignedPort {
                    name: name.clone(),
                    host_port: (*port).into(),
                })
                .collect(),
        }
        .encode_to_vec())
    }

    /// Decodes a stored record, rejecting other format versions and values that no encoded
    /// record can hold.
    pub(super) fn decode(bytes: &[u8]) -> Result<Self, NativeRecordError> {
        let stored =
            store::NativeWorkloadRecord::decode(bytes).map_err(NativeRecordError::Decode)?;
        if stored.schema_version != SCHEMA_VERSION {
            return Err(NativeRecordError::UnsupportedSchema {
                version: stored.schema_version,
            });
        }
        let managed_node_id =
            SiteNodeId::parse(&stored.site_node_id).map_err(|_| NativeRecordError::Invalid {
                field: "site_node_id",
            })?;
        let spec = stored
            .spec
            .ok_or(NativeRecordError::Invalid { field: "spec" })?;
        let required_platform =
            ExecutionPlatform::normalize(&spec.operating_system, &spec.architecture).map_err(
                |_| NativeRecordError::Invalid {
                    field: "spec.platform",
                },
            )?;
        let ipc_mode = match store::IpcMode::try_from(spec.ipc_mode) {
            Ok(store::IpcMode::Isolated) => WorkloadIpcMode::Isolated,
            Ok(store::IpcMode::HostShared) => WorkloadIpcMode::HostShared,
            Ok(store::IpcMode::Unspecified) | Err(_) => {
                return Err(NativeRecordError::Invalid {
                    field: "spec.ipc_mode",
                });
            }
        };
        let ports = spec
            .ports
            .into_iter()
            .map(|port| {
                Ok(WorkloadPort {
                    name: port.name,
                    container_port: port_number(port.container_port, "spec.ports.container_port")?,
                })
            })
            .collect::<Result<_, NativeRecordError>>()?;
        let mut assigned = BTreeMap::new();
        for port in stored.assigned_ports {
            let host_port = port_number(port.host_port, "assigned_ports.host_port")?;
            if assigned.insert(port.name, host_port).is_some() {
                return Err(NativeRecordError::Invalid {
                    field: "assigned_ports.name",
                });
            }
        }
        let spec = WorkloadSpec {
            executable: WorkloadExecutable {
                artifact: WorkloadExecutableArtifact::NixStorePath(spec.nix_store_path),
                path: spec.executable_path,
            },
            required_platform,
            arguments: spec
                .arguments
                .iter()
                .map(|value| template(value, "spec.arguments"))
                .collect::<Result<_, _>>()?,
            environment: spec
                .environment
                .into_iter()
                .map(|(name, value)| Ok((name, template(&value, "spec.environment")?)))
                .collect::<Result<_, NativeRecordError>>()?,
            artifacts: spec
                .artifacts
                .into_iter()
                .map(|binding| WorkloadArtifactBinding {
                    name: binding.name,
                    source: PathBuf::from(binding.source),
                    destination: binding.destination,
                    read_only: binding.read_only,
                })
                .collect(),
            storage: Vec::new(),
            ports,
            accelerator_device_ids: spec.accelerator_device_ids,
            required_accelerator_device_count: spec.required_accelerator_device_count,
            ipc_mode,
            // Real processes measure their memory; the estimate only serves the simulated runtime.
            expected_memory: Default::default(),
        };
        Ok(Self {
            identity_version: stored.identity_version,
            state_root: stored.state_root,
            site_id: stored.site_id,
            execution_target_id: stored.execution_target_id,
            launch: WorkloadLaunch {
                correlation: WorkloadCorrelation {
                    owner_id: stored.owner_id,
                    reconcile_generation: stored.reconcile_generation,
                },
                execution_target: ExecutionTarget {
                    managed_node_id,
                    accelerator_device_ids: stored.target_accelerator_device_ids,
                },
                spec,
            },
            ports: assigned,
        })
    }
}

/// Native owner record that cannot be stored or trusted.
#[derive(Debug, thiserror::Error)]
pub enum NativeRecordError {
    #[error("decode native owner record")]
    Decode(#[source] prost::DecodeError),

    #[error("unsupported native owner record schema version {version}")]
    UnsupportedSchema { version: u32 },

    #[error("native owner record field {field} is missing or invalid")]
    Invalid { field: &'static str },

    #[error("native owner record field {field} holds an invalid template")]
    Template {
        field: &'static str,
        #[source]
        source: WorkloadSpecError,
    },
}

fn template(value: &str, field: &'static str) -> Result<WorkloadValue, NativeRecordError> {
    WorkloadValue::parse(value).map_err(|source| NativeRecordError::Template { field, source })
}

fn port_number(value: u32, field: &'static str) -> Result<u16, NativeRecordError> {
    u16::try_from(value).map_err(|_| NativeRecordError::Invalid { field })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::native::test_support::{HASH, launch, ports};

    #[test]
    fn records_round_trip_the_symbolic_spec_and_assigned_ports() {
        let record = record();

        let decoded = NativeOwnerRecord::decode(&record.encode().expect("encode")).expect("decode");

        assert_eq!(decoded, record);
        assert_eq!(
            decoded.launch.spec.arguments[4].to_string(),
            "--port={service:http.port}"
        );
        assert_eq!(decoded.ports, BTreeMap::from([("http".to_owned(), 41_234)]));
    }

    #[test]
    fn other_schema_versions_and_impossible_values_are_rejected() {
        let encoded = record().encode().expect("encode");
        let stored = store::NativeWorkloadRecord::decode(encoded.as_slice()).expect("stored");
        let mutate = |change: fn(&mut store::NativeWorkloadRecord)| {
            let mut stored = stored.clone();
            change(&mut stored);
            NativeOwnerRecord::decode(&stored.encode_to_vec())
        };

        assert!(matches!(
            mutate(|stored| stored.schema_version = 2),
            Err(NativeRecordError::UnsupportedSchema { version: 2 })
        ));
        for (change, field) in [
            (
                (|stored: &mut store::NativeWorkloadRecord| stored.spec = None)
                    as fn(&mut store::NativeWorkloadRecord),
                "spec",
            ),
            (
                |stored| stored.site_node_id = "Not A Node".to_owned(),
                "site_node_id",
            ),
            (
                |stored| stored.spec.as_mut().unwrap().ipc_mode = 0,
                "spec.ipc_mode",
            ),
            (
                |stored| stored.spec.as_mut().unwrap().operating_system = "plan9".to_owned(),
                "spec.platform",
            ),
            (
                |stored| stored.assigned_ports[0].host_port = 70_000,
                "assigned_ports.host_port",
            ),
            (
                |stored| {
                    let duplicate = stored.assigned_ports[0].clone();
                    stored.assigned_ports.push(duplicate);
                },
                "assigned_ports.name",
            ),
        ] {
            assert!(
                matches!(mutate(change), Err(NativeRecordError::Invalid { field: found }) if found == field),
                "{field}"
            );
        }
        assert!(matches!(
            mutate(|stored| stored.spec.as_mut().unwrap().arguments[0] = "{nope}".to_owned()),
            Err(NativeRecordError::Template {
                field: "spec.arguments",
                ..
            })
        ));
        assert!(matches!(
            NativeOwnerRecord::decode(b"\xff"),
            Err(NativeRecordError::Decode(_))
        ));
    }

    fn record() -> NativeOwnerRecord {
        NativeOwnerRecord {
            identity_version: IDENTITY_VERSION,
            state_root: "/sites/home/runtime/native".to_owned(),
            site_id: "site-one".to_owned(),
            execution_target_id: "mac-gpu".to_owned(),
            launch: launch(
                &format!("/store/{HASH}-fake"),
                Path::new("/artifacts/model.bin"),
            ),
            ports: ports(),
        }
    }
}
