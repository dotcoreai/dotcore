//! The live set of peer certificate pins one node's private transport trusts.
//!
//! Inbound request checks and outbound channels read the same shared set on every check, so a pin
//! recorded while the server runs takes effect on the next request or connection, without a
//! restart. A pin binds one SiteNode to its exact certificate and its role. Withdrawing or
//! replacing a pin also revokes it, which closes the connections it authenticated.

use std::{
    collections::BTreeMap,
    sync::{Arc, PoisonError, RwLock},
};

use tokio_util::sync::CancellationToken;

use super::{AuthenticatedMachineIdentity, PrivatePeerPolicy, exact_node_identity};
use crate::node::folder::CertificateFingerprint;

/// Shared, updatable peer pins of one node, keyed by SiteNode id. Clones share the same set.
#[derive(Clone, Debug, Default)]
pub struct PeerPins {
    peers: Arc<RwLock<BTreeMap<String, PinnedPeer>>>,
}

impl PeerPins {
    /// Starts the set with the pins loaded when the node starts.
    pub fn new(peers: BTreeMap<String, PrivatePeerPolicy>) -> Self {
        Self {
            peers: Arc::new(RwLock::new(
                peers
                    .into_iter()
                    .map(|(node_id, policy)| (node_id, PinnedPeer::new(policy)))
                    .collect(),
            )),
        }
    }

    /// Returns the pin of one SiteNode, if it has one.
    pub fn get(&self, node_id: &str) -> Option<PrivatePeerPolicy> {
        self.peers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(node_id)
            .map(|peer| peer.policy.clone())
    }

    /// Records or replaces the pin of one SiteNode; every later check uses it. A replaced pin is
    /// revoked.
    pub fn insert(&self, node_id: impl Into<String>, policy: PrivatePeerPolicy) {
        let replaced = self
            .peers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(node_id.into(), PinnedPeer::new(policy));
        if let Some(replaced) = replaced {
            replaced.revoked.cancel();
        }
    }

    /// Withdraws and revokes the pin of one SiteNode, closing the connections it authenticated.
    /// Returns whether the node had a pin.
    pub fn remove(&self, node_id: &str) -> bool {
        let removed = self
            .peers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(node_id);
        removed.is_some_and(|removed| {
            removed.revoked.cancel();
            true
        })
    }

    /// Authenticates a peer leaf of the site `site_id`: the leaf must claim exactly one SiteNode
    /// of that site, and that node's pin must be this exact certificate.
    pub fn authenticate(
        &self,
        site_id: &str,
        certificate_der: &[u8],
    ) -> Option<AuthenticatedMachineIdentity> {
        self.authenticate_revocable(site_id, certificate_der)
            .map(|(identity, _)| identity)
    }

    /// Authenticates a peer leaf like [`Self::authenticate`], and returns the token cancelled
    /// once the matching pin is withdrawn or replaced. Both come from one read of the set, so a
    /// pin withdrawn later always cancels the returned token.
    pub fn authenticate_revocable(
        &self,
        site_id: &str,
        certificate_der: &[u8],
    ) -> Option<(AuthenticatedMachineIdentity, CancellationToken)> {
        let node_id = exact_node_identity(certificate_der, site_id)?;
        let peers = self.peers.read().unwrap_or_else(PoisonError::into_inner);
        let peer = peers.get(&node_id)?;
        (CertificateFingerprint::for_certificate(certificate_der) == peer.policy.certificate_sha256)
            .then(|| {
                (
                    AuthenticatedMachineIdentity {
                        site_id: site_id.to_owned(),
                        role: peer.policy.role,
                        node_id,
                    },
                    peer.revoked.clone(),
                )
            })
    }
}

/// One pin and the token cancelled when it is withdrawn or replaced.
#[derive(Debug)]
struct PinnedPeer {
    policy: PrivatePeerPolicy,
    revoked: CancellationToken,
}

impl PinnedPeer {
    fn new(policy: PrivatePeerPolicy) -> Self {
        Self {
            policy,
            revoked: CancellationToken::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rcgen::{CertificateParams, KeyPair, SanType};

    use super::*;
    use crate::{node::folder::certificate_identity_uri, private_transport::PrivateNodeRole};

    #[test]
    fn the_same_node_leaf_takes_its_role_from_exact_peer_policy() {
        let mut parameters = CertificateParams::new(vec!["127.0.0.2".to_owned()])
            .expect("node certificate parameters");
        parameters.subject_alt_names.push(SanType::URI(
            certificate_identity_uri("site-one", "node-one")
                .expect("node claim")
                .try_into()
                .expect("node claim IA5"),
        ));
        let leaf = parameters
            .self_signed(&KeyPair::generate().expect("node key"))
            .expect("node leaf")
            .der()
            .to_vec();
        let pin = CertificateFingerprint::for_certificate(&leaf);

        for role in [PrivateNodeRole::Worker, PrivateNodeRole::Manager] {
            let pins = PeerPins::new(BTreeMap::from([(
                "node-one".to_owned(),
                PrivatePeerPolicy {
                    role,
                    certificate_sha256: pin.clone(),
                },
            )]));
            let authenticated = pins
                .authenticate("site-one", &leaf)
                .expect("same pinned node leaf remains authenticated");
            assert_eq!(authenticated.site_id, "site-one");
            assert_eq!(authenticated.node_id, "node-one");
            assert_eq!(authenticated.role, role);
        }
    }
}
