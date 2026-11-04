//! Loads manager trust custody or a joined node folder into private transport inputs.
//! Membership remains owned by site composition; this adapter only supplies exact local credentials
//! and peer policies to transport.

use std::{collections::BTreeMap, fs, net::SocketAddr, os::unix::fs::PermissionsExt, path::Path};

use dotcore_domain::{BindAddress, SiteNodeHost};
use dotcore_proto::dotcore::site::v1::Site;
use dotcore_site::{
    manager_trust::{SITE_TRUST_DIRECTORY, WORKER_PIN_DIRECTORY, validate_for_private_startup},
    node::folder::{CertificateFingerprint, ValidatedNodeFolder},
    private_transport::{PrivateNodeRole, PrivatePeerPolicy, PrivateServerConfiguration},
};

use super::{CompositionError, ManagerPrivateListener, node_binding::node_configuration_for};

/// Chooses the manager's private listener address: the configured one, else the manager SiteNode
/// host with the site default port when the site declares another SiteNode, else none, since only
/// other nodes dial the manager's private listener.
pub(super) fn manager_bind_address(
    listener: &ManagerPrivateListener,
    site: &Site,
) -> Result<Option<BindAddress>, CompositionError> {
    let port = match listener {
        ManagerPrivateListener::Configured(address) => return Ok(Some(address.clone())),
        ManagerPrivateListener::SiteDefault { port } => *port,
    };
    if site.nodes.iter().all(|node| node.site_manager.is_some()) {
        return Ok(None);
    }
    let manager = site
        .nodes
        .iter()
        .find(|node| node.site_manager.is_some())
        .ok_or_else(|| CompositionError::composition("the site declares no site manager node"))?;
    let host = SiteNodeHost::parse(&manager.host)
        .map_err(|source| private_input("read the site manager host", source))?;
    Ok(Some(BindAddress::new(host, port)))
}

pub(super) fn manager_configuration(
    site_root: &Path,
    site: &Site,
    bind_address: SocketAddr,
) -> Result<PrivateServerConfiguration, CompositionError> {
    let node = node_configuration_for(site_root, site)?;
    if node.declaration.site_manager.is_none() {
        return Err(CompositionError::composition(
            "private manager credentials require the site_manager binding",
        ));
    }
    let identity = validate_for_private_startup(site_root, site)
        .map_err(|source| private_input("validate node identity and manager trust", source))?;
    let trust = site_root.join(SITE_TRUST_DIRECTORY);
    let mut peers = BTreeMap::new();
    let pins = trust.join(WORKER_PIN_DIRECTORY);
    require_owner_directory(&pins)?;
    for entry in
        fs::read_dir(&pins).map_err(|source| private_input("read worker pin registry", source))?
    {
        let entry =
            entry.map_err(|source| private_input("read worker pin registry entry", source))?;
        require_owner_file(&entry.path(), "worker pin")?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .and_then(|name| name.strip_suffix(".sha256"))
            .ok_or_else(|| {
                CompositionError::composition("worker pin registry contains an invalid entry")
            })?;
        if !site
            .nodes
            .iter()
            .any(|node| node.node_id == name && node.site_manager.is_none())
        {
            return Err(CompositionError::composition(
                "worker pin does not name a declared worker node",
            ));
        }
        let value = fs::read_to_string(entry.path())
            .map_err(|source| private_input("read worker pin", source))?;
        let fingerprint = CertificateFingerprint::parse(value.trim())
            .map_err(|source| private_input("validate worker pin", source))?;
        peers.insert(
            name.to_owned(),
            PrivatePeerPolicy {
                role: PrivateNodeRole::Worker,
                certificate_sha256: fingerprint,
            },
        );
    }
    Ok(PrivateServerConfiguration {
        bind_address,
        site_id: site.site_id.clone(),
        local_node_id: node.node_id.to_string(),
        local_role: PrivateNodeRole::Manager,
        certificate_der: identity.certificate,
        private_key_pkcs8: identity.private_key,
        trust_anchor_der: identity.trust_anchor,
        peers,
    })
}

pub(super) fn worker_configuration_from_node_folder(
    folder: ValidatedNodeFolder,
    bind_address: SocketAddr,
) -> Result<PrivateServerConfiguration, CompositionError> {
    let mut peers = BTreeMap::new();
    peers.insert(
        folder.manager_node_id().to_owned(),
        PrivatePeerPolicy {
            role: PrivateNodeRole::Manager,
            certificate_sha256: folder.manager_certificate_sha256().clone(),
        },
    );
    let site_id = folder.site_id().to_owned();
    let local_node_id = folder.worker_node_id().to_owned();
    let trust_anchor_der = folder.trust_anchor_der().to_vec();
    let (certificate, key) = folder.into_worker_identity();
    Ok(PrivateServerConfiguration {
        bind_address,
        site_id,
        local_node_id,
        local_role: PrivateNodeRole::Worker,
        certificate_der: certificate.as_ref().to_vec(),
        private_key_pkcs8: key.secret_der().to_vec(),
        trust_anchor_der,
        peers,
    })
}

fn require_owner_directory(path: &Path) -> Result<(), CompositionError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| private_input("inspect worker pin registry", source))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(CompositionError::composition(
            "worker pin registry must be a real directory",
        ));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(CompositionError::composition(
            "worker pin registry must deny all group and other access",
        ));
    }
    Ok(())
}

fn require_owner_file(path: &Path, artifact: &'static str) -> Result<(), CompositionError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| private_input("inspect private transport artifact", source))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(CompositionError::composition(format!(
            "{artifact} must be a regular file"
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(CompositionError::composition(format!(
            "{artifact} must deny all group and other access"
        )));
    }
    Ok(())
}

fn private_input(operation: &'static str, source: impl Into<anyhow::Error>) -> CompositionError {
    CompositionError::composition_with_source(operation, source)
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::site::v1::{SiteManagerRole, SiteNode};

    use super::*;

    #[test]
    fn manager_with_joined_nodes_defaults_to_its_host_and_the_default_port() {
        for host in ["10.0.0.5", "manager.example.ts.net"] {
            let site = site_with(host, &["127.0.0.2"]);

            let address = manager_bind_address(&site_default(), &site).expect("address");

            assert_eq!(
                address,
                Some(BindAddress::parse(&format!("{host}:50053")).expect("expected address"))
            );
        }
    }

    #[test]
    fn manager_without_joined_nodes_has_no_default() {
        let site = site_with("10.0.0.5", &[]);

        assert_eq!(
            manager_bind_address(&site_default(), &site).expect("address"),
            None
        );
    }

    #[test]
    fn configured_address_wins_with_and_without_joined_nodes() {
        let configured = BindAddress::parse("127.0.0.1:50153").expect("configured address");
        for workers in [&[][..], &["127.0.0.2"][..]] {
            let site = site_with("10.0.0.5", workers);

            let address = manager_bind_address(
                &ManagerPrivateListener::Configured(configured.clone()),
                &site,
            )
            .expect("address");

            assert_eq!(address, Some(configured.clone()));
        }
    }

    #[test]
    fn site_without_manager_node_is_rejected_when_a_default_is_needed() {
        let mut site = site_with("10.0.0.5", &["127.0.0.2"]);
        site.nodes.retain(|node| node.site_manager.is_none());

        let error = manager_bind_address(&site_default(), &site).expect_err("missing manager");

        let CompositionError::Composition(source) = error else {
            panic!("expected a composition failure, got {error:?}");
        };
        assert_eq!(source.to_string(), "the site declares no site manager node");
    }

    fn site_default() -> ManagerPrivateListener {
        ManagerPrivateListener::SiteDefault { port: 50053 }
    }

    fn site_with(manager_host: &str, worker_hosts: &[&str]) -> Site {
        let manager = SiteNode {
            node_id: "manager".to_owned(),
            site_manager: Some(SiteManagerRole::default()),
            host: manager_host.to_owned(),
            ..SiteNode::default()
        };
        let workers = worker_hosts
            .iter()
            .enumerate()
            .map(|(index, host)| SiteNode {
                node_id: format!("worker-{index}"),
                host: (*host).to_owned(),
                ..SiteNode::default()
            });
        Site {
            nodes: std::iter::once(manager).chain(workers).collect(),
            ..Site::default()
        }
    }
}
