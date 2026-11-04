//! Records one approved node in an offline site: its membership entry, its execution targets,
//! and the pin of its certificate. The caller holds the exclusive site lock, and the site either
//! gains the node completely or stays exactly as it was.
//!
//! A declared node that joins again changes only its two addresses in `site.yaml`, in one atomic
//! replacement; its targets, pin, and every other declaration stay as they are.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use dotcore_proto::{
    dotcore::site::v1::{ExecutionTarget, Site, SiteNode},
    yaml,
};

use crate::{
    SITE_FILE,
    atomic::{AtomicError, StdFileSystem, persist_bytes},
    initial::input_store::SiteInputLock,
    manager_trust::{SITE_TRUST_DIRECTORY, WORKER_PIN_DIRECTORY},
    node::folder::CertificateFingerprint,
    repository::SiteRepositoryError,
    validation::{SiteDeclarationRules, load_validated_site, validate_site},
};

/// Appends one node and its targets to the site on disk and pins its leaf certificate, under the
/// exclusive lock that `_lock` proves is held. Returns the site as written.
///
/// The pin is written first, then `site.yaml` is replaced in one atomic rename; a failure before
/// that rename removes the pin and leaves the site untouched. A pin for the node that already
/// exists is left over from an attempt that never reached `site.yaml`, because the node is new
/// to the site, and is replaced. A failure after the rename is reported as
/// [`NodePublicationError::OutcomeUncertain`].
pub(crate) fn publish_node(
    site_root: &Path,
    _lock: &SiteInputLock,
    rules: &SiteDeclarationRules,
    node: &SiteNode,
    targets: &[ExecutionTarget],
    leaf_certificate: &[u8],
) -> Result<Site, NodePublicationError> {
    let site_path = site_root.join(SITE_FILE);
    let current =
        load_validated_site(&site_path, rules).map_err(NodePublicationError::SiteUnreadable)?;
    let updated = site_with_node(&current, rules, node, targets)?;
    let site_yaml = yaml::to_string(&updated).map_err(NodePublicationError::SerializeSite)?;
    let pin = pin_path(site_root, &node.node_id);
    let pin_value = format!(
        "{}\n",
        CertificateFingerprint::for_certificate(leaf_certificate).as_str()
    );

    persist_bytes(&StdFileSystem, &pin, pin_value.as_bytes()).map_err(|error| {
        remove_pin(&pin);
        NodePublicationError::Storage(anyhow::Error::new(error).context("write node pin"))
    })?;
    match persist_bytes(&StdFileSystem, &site_path, site_yaml.as_bytes()) {
        Ok(()) => Ok(updated),
        Err(error @ AtomicError::Storage(_)) => {
            remove_pin(&pin);
            Err(NodePublicationError::Storage(
                anyhow::Error::new(error).context("replace site.yaml"),
            ))
        }
        Err(error @ AtomicError::OutcomeUncertain(_)) => {
            Err(NodePublicationError::OutcomeUncertain(
                anyhow::Error::new(error).context("replace site.yaml"),
            ))
        }
    }
}

/// Replaces the `host` and `manager_host` of the declared node `node` in the site on disk, under
/// the exclusive lock that `_lock` proves is held. Returns the site as written.
///
/// The site is read again and the node is checked once more: still declared, not the manager,
/// pinned to `certificate_der`, and its new host used by no other node. Nothing else changes,
/// and nothing is written when the addresses are already the declared ones. A failure after the
/// replacement is reported as [`NodePublicationError::OutcomeUncertain`].
pub(crate) fn replace_node_hosts(
    site_root: &Path,
    _lock: &SiteInputLock,
    rules: &SiteDeclarationRules,
    node: &SiteNode,
    certificate_der: &[u8],
) -> Result<Site, NodePublicationError> {
    let site_path = site_root.join(SITE_FILE);
    let current =
        load_validated_site(&site_path, rules).map_err(NodePublicationError::SiteUnreadable)?;
    if read_node_pin(site_root, &node.node_id)?
        != Some(CertificateFingerprint::for_certificate(certificate_der))
    {
        return Err(NodePublicationError::PinMismatch {
            node_id: node.node_id.clone(),
        });
    }
    let updated = site_with_node_hosts(&current, rules, node)?;
    if updated == current {
        return Ok(current);
    }

    let site_yaml = yaml::to_string(&updated).map_err(NodePublicationError::SerializeSite)?;
    match persist_bytes(&StdFileSystem, &site_path, site_yaml.as_bytes()) {
        Ok(()) => Ok(updated),
        Err(error @ AtomicError::Storage(_)) => Err(NodePublicationError::Storage(
            anyhow::Error::new(error).context("replace site.yaml"),
        )),
        Err(error @ AtomicError::OutcomeUncertain(_)) => {
            Err(NodePublicationError::OutcomeUncertain(
                anyhow::Error::new(error).context("replace site.yaml"),
            ))
        }
    }
}

/// Reads the pin the site trust holds for `node_id`; `None` when there is no pin.
pub(crate) fn read_node_pin(
    site_root: &Path,
    node_id: &str,
) -> Result<Option<CertificateFingerprint>, NodePublicationError> {
    let pin = pin_path(site_root, node_id);
    let value = match fs::read_to_string(&pin) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(NodePublicationError::PinUnreadable(
                anyhow::Error::new(error).context(format!("read node pin {}", pin.display())),
            ));
        }
    };
    CertificateFingerprint::parse(value.trim())
        .map(Some)
        .map_err(|error| {
            NodePublicationError::PinUnreadable(
                anyhow::Error::new(error).context(format!("parse node pin {}", pin.display())),
            )
        })
}

/// Returns a copy of `site` with the node and its targets added, after checking that the result
/// is a valid site, the node is not a site manager, its host is new, and every target belongs
/// to it. Nothing is written.
pub(super) fn site_with_node(
    site: &Site,
    rules: &SiteDeclarationRules,
    node: &SiteNode,
    targets: &[ExecutionTarget],
) -> Result<Site, NodePublicationError> {
    if node.site_manager.is_some() {
        return Err(NodePublicationError::AdditionalSiteManager);
    }
    if site.nodes.iter().any(|existing| existing.host == node.host) {
        return Err(NodePublicationError::DuplicateHost {
            host: node.host.clone(),
        });
    }
    if let Some(target) = targets
        .iter()
        .find(|target| target.site_node_id != node.node_id)
    {
        return Err(NodePublicationError::ForeignTarget {
            target_id: target.execution_target_id.clone(),
            node_id: node.node_id.clone(),
        });
    }

    let mut updated = site.clone();
    updated.nodes.push(node.clone());
    updated.execution_targets.extend(targets.iter().cloned());
    validate_site(&updated, rules).map_err(NodePublicationError::InvalidSite)?;
    Ok(updated)
}

/// Returns a copy of `site` in which the declared node with `node`'s identity has `node`'s
/// `host` and `manager_host`, after checking that the node is a declared worker, no other node
/// uses its host, and the result is a valid site. Nothing else changes and nothing is written.
pub(super) fn site_with_node_hosts(
    site: &Site,
    rules: &SiteDeclarationRules,
    node: &SiteNode,
) -> Result<Site, NodePublicationError> {
    let mut updated = site.clone();
    let declared = updated
        .nodes
        .iter_mut()
        .find(|declared| declared.node_id == node.node_id)
        .ok_or_else(|| NodePublicationError::UndeclaredNode {
            node_id: node.node_id.clone(),
        })?;
    if declared.site_manager.is_some() {
        return Err(NodePublicationError::AdditionalSiteManager);
    }
    declared.host.clone_from(&node.host);
    declared.manager_host.clone_from(&node.manager_host);
    if site
        .nodes
        .iter()
        .any(|other| other.node_id != node.node_id && other.host == node.host)
    {
        return Err(NodePublicationError::DuplicateHost {
            host: node.host.clone(),
        });
    }
    validate_site(&updated, rules).map_err(NodePublicationError::InvalidSite)?;
    Ok(updated)
}

/// A node could not be recorded in the site.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NodePublicationError {
    #[error("a joining node cannot be a site manager")]
    AdditionalSiteManager,

    #[error("node host {host:?} already exists in this site")]
    DuplicateHost { host: String },

    #[error("node {node_id:?} is not declared in this site")]
    UndeclaredNode { node_id: String },

    #[error("the certificate of node {node_id:?} is not the one this site pins")]
    PinMismatch { node_id: String },

    #[error("the node pin could not be read: {0}")]
    PinUnreadable(#[source] anyhow::Error),

    #[error("execution target {target_id:?} does not belong to node {node_id:?}")]
    ForeignTarget { target_id: String, node_id: String },

    #[error("the site with the new node would be invalid: {0}")]
    InvalidSite(#[source] SiteRepositoryError),

    #[error("existing site.yaml is invalid: {0}")]
    SiteUnreadable(#[source] SiteRepositoryError),

    #[error("updated site.yaml could not be serialized: {0}")]
    SerializeSite(#[source] dotcore_proto::yaml::RestrictedYamlError),

    #[error("node publication storage failed; the site is unchanged: {0}")]
    Storage(#[source] anyhow::Error),

    #[error("node publication outcome is uncertain: {0}")]
    OutcomeUncertain(#[source] anyhow::Error),
}

/// Path of the pin the site trust holds for `node_id`.
fn pin_path(site_root: &Path, node_id: &str) -> PathBuf {
    site_root
        .join(SITE_TRUST_DIRECTORY)
        .join(WORKER_PIN_DIRECTORY)
        .join(format!("{node_id}.sha256"))
}

/// Removes a pin the site never recorded. A pin that cannot be removed stays as a leftover that
/// the next publication for the same node replaces.
fn remove_pin(pin: &Path) {
    if fs::remove_file(pin).is_ok()
        && let Some(pins) = pin.parent()
    {
        let _ = dotcore_domain::fs_sync::sync_directory(pins);
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::*;
    use crate::{
        initial::input_store::try_acquire_exclusive_site_lock,
        membership::join::test_fixtures::{initialized_site, worker_node, worker_targets},
    };

    fn pin_directory(root: &Path) -> PathBuf {
        root.join(SITE_TRUST_DIRECTORY).join(WORKER_PIN_DIRECTORY)
    }

    fn directory_names(path: &Path) -> Vec<String> {
        let mut names = fs::read_dir(path)
            .expect("directory")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn publication_adds_exactly_one_node_its_targets_and_its_pin() {
        let fixture = initialized_site();
        let lock = try_acquire_exclusive_site_lock(&fixture.root)
            .expect("lock")
            .expect("lock is free");
        let node = worker_node("node-worker", "worker", "10.0.0.7");
        let targets = worker_targets("node-worker");
        let root_entries = directory_names(&fixture.root);
        let pins_before = directory_names(&pin_directory(&fixture.root));

        let published = publish_node(
            &fixture.root,
            &lock,
            &crate::test_support::site_declaration_rules(),
            &node,
            &targets,
            b"leaf-certificate-der",
        )
        .expect("publish");

        let mut expected = fixture.site.clone();
        expected.nodes.push(node.clone());
        expected.execution_targets.extend(targets.iter().cloned());
        assert_eq!(published, expected);
        assert_eq!(
            load_validated_site(
                &fixture.root.join(SITE_FILE),
                &crate::test_support::site_declaration_rules()
            )
            .expect("reload"),
            expected
        );
        assert_eq!(
            fs::read_to_string(pin_directory(&fixture.root).join("node-worker.sha256"))
                .expect("pin"),
            format!(
                "{}\n",
                CertificateFingerprint::for_certificate(b"leaf-certificate-der").as_str()
            )
        );
        assert_eq!(
            directory_names(&pin_directory(&fixture.root)).len(),
            pins_before.len() + 1
        );
        assert_eq!(directory_names(&fixture.root), root_entries);
    }

    #[test]
    fn rejected_additions_leave_site_and_pins_unchanged() {
        let fixture = initialized_site();
        let lock = try_acquire_exclusive_site_lock(&fixture.root)
            .expect("lock")
            .expect("lock is free");
        let manager = fixture.site.nodes[0].clone();
        let site_bytes = fs::read(fixture.root.join(SITE_FILE)).expect("site bytes");
        let root_entries = directory_names(&fixture.root);
        let pins = directory_names(&pin_directory(&fixture.root));

        let mut second_manager = worker_node("node-worker", "worker", "10.0.0.7");
        second_manager.site_manager = manager.site_manager;
        let cases = [
            (
                "duplicate display name",
                worker_node("node-worker", &manager.display_name, "10.0.0.7"),
                worker_targets("node-worker"),
            ),
            (
                "duplicate host",
                worker_node("node-worker", "worker", &manager.host),
                worker_targets("node-worker"),
            ),
            (
                "duplicate node id",
                worker_node(&manager.node_id, "worker", "10.0.0.7"),
                worker_targets(&manager.node_id),
            ),
            (
                "second manager",
                second_manager,
                worker_targets("node-worker"),
            ),
            (
                "foreign target",
                worker_node("node-worker", "worker", "10.0.0.7"),
                worker_targets("node-other"),
            ),
        ];
        for (label, node, targets) in cases {
            let error = publish_node(
                &fixture.root,
                &lock,
                &crate::test_support::site_declaration_rules(),
                &node,
                &targets,
                b"leaf",
            )
            .expect_err(label);
            assert!(
                !matches!(
                    error,
                    NodePublicationError::Storage(_) | NodePublicationError::OutcomeUncertain(_)
                ),
                "{label}: {error:?}"
            );
            assert_eq!(
                fs::read(fixture.root.join(SITE_FILE)).expect("site bytes"),
                site_bytes,
                "{label}"
            );
            assert_eq!(
                directory_names(&pin_directory(&fixture.root)),
                pins,
                "{label}"
            );
            assert_eq!(directory_names(&fixture.root), root_entries, "{label}");
        }
    }

    #[test]
    fn replacing_hosts_changes_only_the_node_addresses_and_nothing_when_equal() {
        let fixture = initialized_site();
        let lock = try_acquire_exclusive_site_lock(&fixture.root)
            .expect("lock")
            .expect("lock is free");
        let node = worker_node("node-worker", "worker", "10.0.0.7");
        let published = publish_node(
            &fixture.root,
            &lock,
            &crate::test_support::site_declaration_rules(),
            &node,
            &worker_targets("node-worker"),
            b"leaf",
        )
        .expect("publish");
        let pins = directory_names(&pin_directory(&fixture.root));
        let pin = fs::read(pin_directory(&fixture.root).join("node-worker.sha256")).expect("pin");

        let mut moved = node.clone();
        moved.host = "10.0.0.8".to_owned();
        moved.manager_host = "localhost".to_owned();
        let written = replace_node_hosts(
            &fixture.root,
            &lock,
            &crate::test_support::site_declaration_rules(),
            &moved,
            b"leaf",
        )
        .expect("replace hosts");

        let mut expected = published;
        expected.nodes[1] = moved.clone();
        assert_eq!(written, expected);
        assert_eq!(
            load_validated_site(
                &fixture.root.join(SITE_FILE),
                &crate::test_support::site_declaration_rules()
            )
            .expect("reload"),
            expected
        );
        assert_eq!(directory_names(&pin_directory(&fixture.root)), pins);
        assert_eq!(
            fs::read(pin_directory(&fixture.root).join("node-worker.sha256")).expect("pin"),
            pin
        );

        let site_bytes = fs::read(fixture.root.join(SITE_FILE)).expect("site bytes");
        replace_node_hosts(
            &fixture.root,
            &lock,
            &crate::test_support::site_declaration_rules(),
            &moved,
            b"leaf",
        )
        .expect("unchanged hosts");
        assert_eq!(
            fs::read(fixture.root.join(SITE_FILE)).expect("site bytes"),
            site_bytes
        );
    }

    #[test]
    fn rejected_host_replacements_leave_site_and_pins_unchanged() {
        let fixture = initialized_site();
        let lock = try_acquire_exclusive_site_lock(&fixture.root)
            .expect("lock")
            .expect("lock is free");
        let node = worker_node("node-worker", "worker", "10.0.0.7");
        publish_node(
            &fixture.root,
            &lock,
            &crate::test_support::site_declaration_rules(),
            &node,
            &worker_targets("node-worker"),
            b"leaf",
        )
        .expect("publish");
        let manager = fixture.site.nodes[0].clone();
        let site_bytes = fs::read(fixture.root.join(SITE_FILE)).expect("site bytes");
        let pins = directory_names(&pin_directory(&fixture.root));

        let mut manager_moved = manager.clone();
        manager_moved.host = "10.0.0.9".to_owned();
        let cases = [
            (
                "undeclared node",
                worker_node("node-other", "other", "10.0.0.8"),
                &b"leaf"[..],
            ),
            ("manager node", manager_moved, b"leaf"),
            ("pin mismatch", node.clone(), b"other leaf"),
            (
                "duplicate host",
                worker_node("node-worker", "worker", &manager.host),
                b"leaf",
            ),
        ];
        for (label, node, certificate) in cases {
            let error = replace_node_hosts(
                &fixture.root,
                &lock,
                &crate::test_support::site_declaration_rules(),
                &node,
                certificate,
            )
            .expect_err(label);
            assert!(
                !matches!(
                    error,
                    NodePublicationError::Storage(_) | NodePublicationError::OutcomeUncertain(_)
                ),
                "{label}: {error:?}"
            );
            assert_eq!(
                fs::read(fixture.root.join(SITE_FILE)).expect("site bytes"),
                site_bytes,
                "{label}"
            );
            assert_eq!(
                directory_names(&pin_directory(&fixture.root)),
                pins,
                "{label}"
            );
        }
    }

    #[test]
    fn a_leftover_pin_of_an_unrecorded_node_is_replaced() {
        let fixture = initialized_site();
        let lock = try_acquire_exclusive_site_lock(&fixture.root)
            .expect("lock")
            .expect("lock is free");
        let leftover = pin_directory(&fixture.root).join("node-worker.sha256");
        fs::write(&leftover, "previous\n").expect("leftover pin");
        let pins_before = directory_names(&pin_directory(&fixture.root));
        let node = worker_node("node-worker", "worker", "10.0.0.7");

        publish_node(
            &fixture.root,
            &lock,
            &crate::test_support::site_declaration_rules(),
            &node,
            &worker_targets("node-worker"),
            b"leaf",
        )
        .expect("publication replaces the leftover pin");

        assert_eq!(
            fs::read_to_string(&leftover).expect("pin"),
            format!(
                "{}\n",
                CertificateFingerprint::for_certificate(b"leaf").as_str()
            )
        );
        assert_eq!(directory_names(&pin_directory(&fixture.root)), pins_before);
        assert!(
            load_validated_site(
                &fixture.root.join(SITE_FILE),
                &crate::test_support::site_declaration_rules()
            )
            .expect("Site")
            .nodes
            .contains(&node)
        );
    }
}
