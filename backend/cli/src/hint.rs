//! Command and flag hints added to backend failures.
//!
//! The backend states each failure and its remedy in domain words; this module completes the remedy
//! with the command or option that carries it out. Hints match typed backend errors anywhere in a
//! failure's cause chain, so the same failure reads the same whichever command surfaced it.

use std::error::Error;

use dotcore_backend::{composition::CompositionError, telemetry::fit::FitError};
use dotcore_site::{
    initial::input_store::InitialSiteWriteError,
    membership::{
        NodeLeaveError, NodeListError, NodeRemovalError,
        join::{JoinTokenError, NodeFolderError, NodeJoinError},
    },
    repository::SiteRepositoryError,
    store::NodeStoreLayoutError,
    validation::InvalidModelSnapshotError,
};
use tonic::Code;

use crate::management::{DeploymentGroup, ManagementError, ReferenceError};

/// Renders one error's own message followed by its command hint, if it has one.
pub(crate) fn describe(error: &(dyn Error + 'static)) -> String {
    match command_hint(error)
        .map(str::to_owned)
        .or_else(|| deployment_hint(error))
        .or_else(|| fit_hint(error))
    {
        Some(hint) => format!("{error}{hint}"),
        None => error.to_string(),
    }
}

/// Completes a "reset the site" remedy.
const SITE_RESET: &str = " with `dotc site reset`";

/// Completes a "join the site" remedy on a worker node.
const JOIN_SITE: &str = " with `dotc node add` on the manager and `dotc node join` on this machine";

/// Completes a "leave the node folder, then join as a new node" remedy.
const LEAVE_THEN_JOIN: &str =
    " using `dotc node leave` here, then `dotc node add` on the manager and `dotc node join` here";

/// Completes a "remove the node, leave the node folder, then join as a new node" remedy.
const REMOVE_LEAVE_THEN_JOIN: &str = " using `dotc node remove` on the manager, then `dotc node leave` here, `dotc node add` on the manager and `dotc node join` here";

/// Returns the text that completes the remedy of a backend error, starting with its separator.
fn command_hint(error: &(dyn Error + 'static)) -> Option<&'static str> {
    if let Some(error) = error.downcast_ref::<SiteRepositoryError>() {
        return matches!(error, SiteRepositoryError::SchemaMismatch(_)).then_some(SITE_RESET);
    }
    if error.is::<InvalidModelSnapshotError>() {
        return Some(SITE_RESET);
    }
    if let Some(error) = error.downcast_ref::<NodeStoreLayoutError>() {
        return match error {
            NodeStoreLayoutError::UnsupportedSite { .. } => Some(SITE_RESET),
            NodeStoreLayoutError::UnsupportedNodeFolder { .. } => Some(JOIN_SITE),
            NodeStoreLayoutError::Storage { .. } => None,
        };
    }
    if let Some(error) = error.downcast_ref::<CompositionError>() {
        return match error {
            CompositionError::NotSiteManager { .. } => Some(JOIN_SITE),
            CompositionError::UnresolvableNodeHost { .. } => {
                Some(" using `dotc site reset --host`")
            }
            _ => None,
        };
    }
    if let Some(error) = error.downcast_ref::<InitialSiteWriteError>() {
        return matches!(error, InitialSiteWriteError::IntentConflict).then_some(" with --replace");
    }
    if let Some(error) = error.downcast_ref::<JoinTokenError>() {
        return match error {
            JoinTokenError::NotAToken => Some(" that `dotc node add` printed"),
            JoinTokenError::BadPayload(_) => Some(" with `dotc node add`"),
            _ => None,
        };
    }
    if let Some(error) = error.downcast_ref::<NodeFolderError>() {
        return match error {
            NodeFolderError::Exists { .. } => Some(" with --site-dir"),
            NodeFolderError::OtherSite { .. } | NodeFolderError::EarlierTrust { .. } => {
                Some(LEAVE_THEN_JOIN)
            }
            NodeFolderError::CertificateExpired { .. } => Some(REMOVE_LEAVE_THEN_JOIN),
            _ => None,
        };
    }
    if let Some(error) = error.downcast_ref::<NodeJoinError>() {
        return match error {
            NodeJoinError::NodeNotDeclared(_) | NodeJoinError::CertificateNotPinned(_) => {
                Some(LEAVE_THEN_JOIN)
            }
            NodeJoinError::CertificateExpired(_) => Some(REMOVE_LEAVE_THEN_JOIN),
            NodeJoinError::FolderRewrite { .. } | NodeJoinError::FolderRewriteUncertain { .. } => {
                Some(" with a new token from `dotc node add` on the manager")
            }
            _ => None,
        };
    }
    if let Some(error) = error.downcast_ref::<NodeRemovalError>() {
        return matches!(error, NodeRemovalError::PlacedDeployments { .. })
            .then_some(" by passing --forget-deployments");
    }
    if let Some(error) = error.downcast_ref::<NodeLeaveError>() {
        return matches!(error, NodeLeaveError::ManagerFolder { .. })
            .then_some(" with `dotc site delete`");
    }
    if let Some(error) = error.downcast_ref::<NodeListError>() {
        return matches!(error, NodeListError::JoinedNodeFolder { .. })
            .then_some(", run `dotc node list` there");
    }
    None
}

/// Returns the text that completes a manager call or deployment reference error.
fn deployment_hint(error: &(dyn Error + 'static)) -> Option<String> {
    if let Some(error) = error.downcast_ref::<ManagementError>() {
        return match error {
            ManagementError::Unreachable { .. } => {
                Some("; start it with `dotcore` or pass its address with --manager".to_owned())
            }
            ManagementError::Call { group, code, .. } => matches!(
                code,
                Code::NotFound | Code::FailedPrecondition | Code::InvalidArgument
            )
            .then(|| see_list(*group)),
            ManagementError::NotAccepted { group, .. } => Some(see_list(*group)),
        };
    }
    if let Some(error) = error.downcast_ref::<ReferenceError>() {
        return Some(match error {
            ReferenceError::Unknown { group, .. } => see_list(*group),
            ReferenceError::Ambiguous { .. } => "; pass one of the ids instead".to_owned(),
        });
    }
    None
}

/// Names the options that fit the parameter set an unsolvable sizing fit says would solve.
fn fit_hint(error: &(dyn Error + 'static)) -> Option<String> {
    match error.downcast_ref::<FitError>()? {
        FitError::Unsolvable { solvable, .. } if !solvable.is_empty() => Some(format!(
            " with {}",
            solvable
                .iter()
                .map(|parameter| format!("--parameter {parameter}"))
                .collect::<Vec<_>>()
                .join(" ")
        )),
        _ => None,
    }
}

/// Points at the command that lists the group's deployments.
fn see_list(group: DeploymentGroup) -> String {
    format!("; see `{}`", group.list_command())
}

#[cfg(test)]
mod tests {
    use dotcore_backend::telemetry::fit::{FitError, UnsolvableCause};
    use dotcore_site::{
        initial::input_store::InitialSiteWriteError, membership::join::NodeFolderError,
        repository::SiteRepositoryError,
    };

    use crate::format_failure;

    #[test]
    fn a_hint_follows_its_own_error_wherever_it_sits_in_the_chain() {
        let decode = serde_json::from_str::<u32>("\"text\"").expect_err("decode failure");
        let error = anyhow::Error::new(InitialSiteWriteError::DeploymentInspection(
            SiteRepositoryError::SchemaMismatch(decode),
        ))
        .context("site init failed");

        assert_eq!(
            format_failure(&error),
            "site init failed: existing site deployment declarations could not be inspected: \
             site.yaml does not match the site schema of this build (invalid type: string \
             \"text\", expected u32 at line 1 column 6); correct the named value or reset the \
             site: site.yaml does not match the site schema of this build (invalid type: string \
             \"text\", expected u32 at line 1 column 6); correct the named value or reset the \
             site with `dotc site reset`"
        );
    }

    #[test]
    fn an_expired_folder_certificate_names_the_remove_leave_and_join_commands() {
        let error = anyhow::Error::new(NodeFolderError::CertificateExpired {
            root: "/x/node".into(),
        });

        assert_eq!(
            format_failure(&error),
            "the node certificate in the folder at /x/node has expired; remove the node from \
             the site, leave this machine's node folder, then join as a new node using `dotc \
             node remove` on the manager, then `dotc node leave` here, `dotc node add` on the \
             manager and `dotc node join` here"
        );
    }

    #[test]
    fn an_unsolvable_fit_names_the_parameter_options_that_would_solve_it() {
        let error = anyhow::Error::new(FitError::Unsolvable {
            cause: UnsolvableCause::NoData("perTokenQ8".to_owned()),
            solvable: vec!["base".to_owned(), "perToken".to_owned()],
        })
        .context("fit sizing constants");

        assert_eq!(
            format_failure(&error),
            "fit sizing constants: sizing fit is unsolvable: parameter \"perTokenQ8\" has no \
             data: its coefficient is zero on every fit point; fitting only base, perToken would \
             solve it with --parameter base --parameter perToken"
        );
    }

    #[test]
    fn variants_without_a_command_remedy_keep_their_message() {
        let error = anyhow::Error::new(InitialSiteWriteError::RetainedStateResetRequired);

        assert_eq!(
            format_failure(&error),
            "retained site state requires explicit reset confirmation"
        );
    }
}
