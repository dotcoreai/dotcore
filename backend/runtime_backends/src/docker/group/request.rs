//! Docker request preparation and group ownership-label construction.

use std::{collections::BTreeMap, net::SocketAddrV4};

use dotcore_runtime::{RuntimeBackendError, WorkloadGroupLaunch, WorkloadUnitName};

use super::{
    super::{
        adapter::{DockerAcceleratorTransport, DockerRuntimeConfig},
        request::{
            DockerGroupContainerRequest, DockerGroupNetworkRequest, DockerGroupPort, DockerMount,
            accelerator_device_requests,
        },
        storage::storage_path,
    },
    types::{GROUP_SCHEMA, LABEL_GROUP, LABEL_NETWORK, LABEL_UNIT},
};

pub(super) struct PreparedGroup {
    pub(super) network_name: String,
    pub(super) network: DockerGroupNetworkRequest,
    pub(super) units: BTreeMap<WorkloadUnitName, DockerGroupContainerRequest>,
}

pub(super) fn prepare_group(
    group: &WorkloadGroupLaunch,
    config: &DockerRuntimeConfig,
) -> Result<PreparedGroup, RuntimeBackendError> {
    let network_name = format!("dotcore_{}_group_{}", config.site_id, group.group_id.0);
    let network = DockerGroupNetworkRequest {
        name: network_name.clone(),
        labels: group_labels(config, group, None),
    };
    // Every unit of a group receives the target's configured accelerator devices, which
    // application targets select only through CDI.
    let device_requests = accelerator_device_requests(
        DockerAcceleratorTransport::Cdi,
        &group.execution_target.accelerator_device_ids,
    );
    let units = group
        .units
        .iter()
        .map(|(name, unit)| {
            let ports = unit
                .ports
                .iter()
                .map(|port| DockerGroupPort {
                    name: port.name.clone(),
                    container_port: port.container_port,
                    publication: (name == &group.publication.unit_name
                        && port.name == group.publication.port_name)
                        .then(|| {
                            SocketAddrV4::new(
                                group.publication.configured_host,
                                group.publication.host_port.get(),
                            )
                        }),
                })
                .collect();
            let mounts = unit
                .storage
                .iter()
                .map(|storage| {
                    Ok(DockerMount {
                        source: storage_path(config, &storage.into())?,
                        target: storage.destination.clone(),
                        read_only: storage.read_only,
                    })
                })
                .collect::<Result<_, RuntimeBackendError>>()?;
            Ok((
                name.clone(),
                DockerGroupContainerRequest {
                    name: format!("{}_unit_{}", network_name, name.0),
                    image: unit.image.clone(),
                    // An empty command or arguments keeps the image's own entrypoint or default
                    // arguments, as in the single-workload Docker path.
                    entrypoint: unit.command.clone(),
                    argv: unit.arguments.clone(),
                    labels: group_labels(config, group, Some(name)),
                    ports,
                    mounts,
                    device_requests: device_requests.clone(),
                    environment: unit
                        .environment
                        .iter()
                        .map(|(key, value)| format!("{key}={value}"))
                        .collect(),
                    network_name: network_name.clone(),
                    aliases: vec![name.0.clone()],
                    extra_hosts: Vec::new(),
                },
            ))
        })
        .collect::<Result<_, RuntimeBackendError>>()?;
    Ok(PreparedGroup {
        network_name,
        network,
        units,
    })
}

pub(super) fn group_labels(
    config: &DockerRuntimeConfig,
    group: &WorkloadGroupLaunch,
    unit: Option<&WorkloadUnitName>,
) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::from([
        ("com.dotcore.managed".to_owned(), "true".to_owned()),
        ("com.dotcore.site".to_owned(), config.site_id.clone()),
        (
            "com.dotcore.execution-target".to_owned(),
            config.execution_target_id.clone(),
        ),
        (
            "com.dotcore.runtime.schema".to_owned(),
            GROUP_SCHEMA.to_owned(),
        ),
        (
            "com.dotcore.workload.correlation".to_owned(),
            group.correlation.owner_id.clone(),
        ),
        (
            "com.dotcore.reconcile.generation".to_owned(),
            group.correlation.reconcile_generation.to_string(),
        ),
        (LABEL_GROUP.to_owned(), group.group_id.0.clone()),
    ]);
    if let Some(unit) = unit {
        labels.insert(LABEL_UNIT.to_owned(), unit.0.clone());
    } else {
        labels.insert(LABEL_NETWORK.to_owned(), "true".to_owned());
    }
    labels
}
