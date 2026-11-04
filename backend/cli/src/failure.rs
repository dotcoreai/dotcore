//! Operator-facing rendering of failed commands.
//!
//! Turns a failure into a one-line verdict, the facts behind it, and the next steps to take. Typed
//! backend errors anywhere in the cause chain choose the verdict and remedies; any other failure
//! shows its outermost message and root cause, with the whole cause chain on request. Redirected
//! output keeps a single-line chain.

use std::error::Error;

use dotcore_backend::composition::CompositionError;
use dotcore_site::{
    initial::input_store::InitialSiteWriteError,
    membership::{
        NodeLeaveError, NodeRemovalError, PlacedDeployment,
        join::{JoinTokenError, NodeFolderError, NodeJoinError},
    },
    repository::SiteRepositoryError,
    reset::transaction::SiteResetLockError,
    store::NodeStoreLayoutError,
    validation::InvalidModelSnapshotError,
};
use tonic::Code;

use crate::{
    code::{CodeError, CodingAgent},
    hint,
    management::{DeploymentGroup, ManagementError, ReferenceError},
    node::RejoinNameMismatch,
    terminal::{Message, NextStep},
};

/// A command failure arranged for the terminal.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Failure {
    verdict: String,
    facts: Vec<Message>,
    next: Vec<NextStep>,
    causes: Vec<String>,
}

impl Failure {
    /// Reads the failure from the error and its cause chain.
    pub(crate) fn from_error(error: &anyhow::Error) -> Self {
        let causes = cause_messages(error);
        let mapped = error.chain().find_map(typed_failure);
        match mapped {
            Some((verdict, facts, next)) => Self {
                verdict,
                facts,
                next,
                causes,
            },
            None => Self::unmapped(causes),
        }
    }

    /// Lays the failure out as messages; `verbose` adds the cause chain.
    pub(crate) fn messages(&self, verbose: bool) -> Vec<Message> {
        let mut messages = vec![Message::Error(self.verdict.clone())];
        messages.extend(self.facts.iter().cloned());
        if !self.next.is_empty() {
            messages.push(Message::Next(self.next.clone()));
        }
        if verbose && !self.causes.is_empty() {
            messages.push(Message::Heading("Cause".to_owned()));
            messages.extend(
                self.causes
                    .iter()
                    .map(|cause| Message::Info(format!("  {cause}"))),
            );
        }
        messages
    }

    /// Shows the outermost message, the root cause when it differs, and how to see everything.
    fn unmapped(causes: Vec<String>) -> Self {
        let verdict = causes.first().map_or_else(
            || "The command failed".to_owned(),
            |cause| capitalize(cause),
        );
        let mut facts = Vec::new();
        let mut next = Vec::new();
        if causes.len() > 1 {
            if let Some(root) = causes.last() {
                facts.push(Message::Info(format!("  {root}")));
            }
            next.push(NextStep::new("Details: rerun with --verbose"));
        }
        Self {
            verdict,
            facts,
            next,
            causes,
        }
    }
}

/// Renders the single line printed when stderr is not a terminal, or `--verbose` is not wanted:
/// every distinct cause, each followed by the command hint that completes its remedy.
pub(crate) fn chain_line(error: &anyhow::Error) -> String {
    let mut kept: Vec<(String, String)> = Vec::new();
    for cause in error.chain() {
        let raw = cause.to_string();
        let described = hint::describe(cause);
        let hinted = described != raw;
        if !hinted && kept.iter().any(|(earlier, _)| earlier.contains(&raw)) {
            continue;
        }
        kept.push((raw, described));
    }
    kept.into_iter()
        .map(|(_, described)| described)
        .collect::<Vec<_>>()
        .join(": ")
}

/// The distinct cause messages of the chain, outermost first, without hint suffixes.
fn cause_messages(error: &anyhow::Error) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for cause in error.chain() {
        let raw = cause.to_string();
        if !kept.iter().any(|earlier| earlier.contains(&raw)) {
            kept.push(raw);
        }
    }
    kept
}

type Typed = (String, Vec<Message>, Vec<NextStep>);

/// Maps a typed backend error to its verdict, facts and next steps.
fn typed_failure(cause: &(dyn Error + 'static)) -> Option<Typed> {
    if let Some(NodeRemovalError::PlacedDeployments {
        node_name,
        deployments,
    }) = cause.downcast_ref::<NodeRemovalError>()
    {
        return Some(placed_deployments(node_name, deployments));
    }
    if matches!(
        cause.downcast_ref::<SiteRepositoryError>(),
        Some(SiteRepositoryError::SchemaMismatch(_))
    ) || cause.is::<InvalidModelSnapshotError>()
        || matches!(
            cause.downcast_ref::<NodeStoreLayoutError>(),
            Some(NodeStoreLayoutError::UnsupportedSite { .. })
        )
    {
        return Some((
            "This site's saved files don't match this version of dotc".to_owned(),
            Vec::new(),
            vec![
                NextStep::new("Reset the site (this deletes its state):")
                    .with_command("dotc site reset"),
            ],
        ));
    }
    let running_node = |verdict_prefix: &str, node_dir: &std::path::Path| {
        (
            format!(
                "{verdict_prefix} {}: the node is running",
                node_dir.display()
            ),
            Vec::new(),
            vec![NextStep::new(
                "Stop the node server, then run the command again",
            )],
        )
    };
    if let Some(SiteResetLockError::NodeRunning { site_root }) =
        cause.downcast_ref::<SiteResetLockError>()
    {
        return Some(running_node("Can't delete", site_root));
    }
    if let Some(NodeLeaveError::NodeRunning { node_root }) = cause.downcast_ref::<NodeLeaveError>()
    {
        return Some(running_node("Can't leave the site from", node_root));
    }
    if let Some(error) = cause.downcast_ref::<JoinTokenError>() {
        return match error {
            JoinTokenError::NotAToken => Some((
                "That isn't a join token".to_owned(),
                Vec::new(),
                vec![
                    NextStep::new("Copy the whole token printed on the manager by:")
                        .with_command("dotc node add"),
                ],
            )),
            JoinTokenError::BadPayload(_) => Some((
                "The join token is not valid".to_owned(),
                Vec::new(),
                vec![NextStep::new("Get a new token:").with_command("dotc node add")],
            )),
            _ => None,
        };
    }
    if let Some(error) = cause.downcast_ref::<NodeFolderError>() {
        match error {
            NodeFolderError::Exists { root } => {
                return Some((
                    format!("{} already exists", root.display()),
                    Vec::new(),
                    vec![
                        NextStep::new("Join into a folder that doesn't exist yet:")
                            .with_command("dotc node join --site-dir <new folder>"),
                    ],
                ));
            }
            NodeFolderError::OtherSite { root } => {
                return Some(leave_then_join(
                    format!("{} belongs to another site", root.display()),
                    root,
                ));
            }
            NodeFolderError::EarlierTrust { root } => {
                return Some(leave_then_join(
                    format!(
                        "{} belongs to an earlier trust of this site",
                        root.display()
                    ),
                    root,
                ));
            }
            NodeFolderError::NodeRunning { root } => {
                return Some(running_node("Can't join again from", root));
            }
            NodeFolderError::CertificateExpired { .. } => {
                return Some(rejected_node("This node's certificate has expired", true));
            }
            NodeFolderError::CertificateNotYetValid { .. } => {
                return Some(early_certificate());
            }
            _ => {}
        }
    }
    if let Some(error) = cause.downcast_ref::<NodeJoinError>() {
        match error {
            NodeJoinError::NodeNotDeclared(_) => {
                return Some(rejected_node("The site no longer has this node", false));
            }
            NodeJoinError::CertificateNotPinned(_) => {
                return Some(rejected_node(
                    "The site doesn't recognize this node's certificate",
                    false,
                ));
            }
            NodeJoinError::CertificateExpired(_) => {
                return Some(rejected_node("This node's certificate has expired", true));
            }
            NodeJoinError::CertificateNotYetValid(_) => {
                return Some(early_certificate());
            }
            NodeJoinError::FolderRewrite { root, .. } => {
                return Some(rewritten_folder(
                    "The site recorded the node's new addresses, but its folder wasn't updated",
                    root,
                ));
            }
            NodeJoinError::FolderRewriteUncertain { root, .. } => {
                return Some(rewritten_folder(
                    "The site recorded the node's new addresses, but the folder update may not \
                     be durable",
                    root,
                ));
            }
            _ => {}
        }
    }
    if let Some(RejoinNameMismatch { name, root }) = cause.downcast_ref::<RejoinNameMismatch>() {
        return Some((
            format!("{name} is not the node in {}", root.display()),
            Vec::new(),
            vec![NextStep::new(
                "Join again without --name, or with the name the node was added under",
            )],
        ));
    }
    if matches!(
        cause.downcast_ref::<InitialSiteWriteError>(),
        Some(InitialSiteWriteError::IntentConflict)
    ) {
        return Some((
            "This site already has a different configuration".to_owned(),
            Vec::new(),
            vec![NextStep::new(
                "Replace it by adding --replace to the same command",
            )],
        ));
    }
    if matches!(
        cause.downcast_ref::<CompositionError>(),
        Some(CompositionError::NotSiteManager { .. })
    ) {
        return Some((
            "This machine is a joined node, not the site manager".to_owned(),
            Vec::new(),
            vec![
                NextStep::new("Start it from its own node folder after joining it:")
                    .with_command("dotc node join"),
            ],
        ));
    }
    if let Some(CompositionError::UnresolvableNodeHost { host, source }) =
        cause.downcast_ref::<CompositionError>()
    {
        return Some(unresolvable_node_host(host, source));
    }
    if let Some(error) = cause.downcast_ref::<ManagementError>() {
        return Some(management_failure(error));
    }
    if let Some(error) = cause.downcast_ref::<ReferenceError>() {
        return Some(reference_failure(error));
    }
    if let Some(error) = cause.downcast_ref::<CodeError>() {
        return Some(code_failure(error));
    }
    None
}

/// The steps that retire a node folder this machine cannot join again from and join as new.
fn leave_then_join(verdict: String, root: &std::path::Path) -> Typed {
    let leave = format!(
        "dotc node leave --site-dir {}",
        shell_word(&root.display().to_string())
    );
    (
        verdict,
        Vec::new(),
        vec![
            NextStep::new("Leave that site on this machine:").with_command(leave),
            NextStep::new("Then, on the manager, get a new token to join as a new node:")
                .with_command("dotc node add"),
        ],
    )
}

/// A manager refusal of a node that joins again from a folder the site no longer accepts.
fn rejected_node(verdict: &str, remove_first: bool) -> Typed {
    let mut next = Vec::new();
    if remove_first {
        next.push(
            NextStep::new("Remove the node on the manager:")
                .with_command("dotc node remove <node name>"),
        );
    }
    next.push(NextStep::new("Leave the site on this machine:").with_command("dotc node leave"));
    next.push(
        NextStep::new("Then, on the manager, get a new token to join as a new node:")
            .with_command("dotc node add"),
    );
    (verdict.to_owned(), Vec::new(), next)
}

/// A node certificate that is not valid yet, which usually means a clock is behind.
fn early_certificate() -> Typed {
    (
        "This node's certificate is not valid yet".to_owned(),
        Vec::new(),
        vec![NextStep::new(
            "Check the clocks of this machine and the manager, then join again",
        )],
    )
}

/// A rejoin the site recorded whose node folder update failed or may not be durable.
fn rewritten_folder(verdict: &str, root: &std::path::Path) -> Typed {
    (
        verdict.to_owned(),
        vec![Message::Info(format!("  {}", root.display()))],
        vec![
            NextStep::new("On the manager, get a new token to join again:")
                .with_command("dotc node add"),
        ],
    )
}

/// A node host name the server cannot resolve: start the network that provides it, or reset the
/// site with another host. The resolver's own failure stays as the fact.
fn unresolvable_node_host(
    host: &dotcore_domain::SiteNodeHost,
    source: &dotcore_domain::HostResolutionError,
) -> Typed {
    let mut root: &(dyn Error + 'static) = source;
    while let Some(next) = root.source() {
        root = next;
    }
    (
        format!("Can't resolve the node host {host}"),
        vec![Message::Info(format!("  {root}"))],
        vec![
            NextStep::new("Start the network that provides it, then start the server again"),
            NextStep::new("Or reset the site with another host (this deletes its state):")
                .with_command("dotc site reset --host <host>"),
        ],
    )
}

fn management_failure(error: &ManagementError) -> Typed {
    match error {
        ManagementError::Unreachable { address, .. } => (
            format!("Can't reach the manager at {address}"),
            Vec::new(),
            vec![
                NextStep::new("Start the site manager:").with_command("dotcore"),
                NextStep::new("Or pass its address with --manager or DOTCORE_MANAGEMENT_ADDR"),
            ],
        ),
        ManagementError::Call {
            group,
            action,
            code,
            message,
        } => {
            let verdict = if refusal(*code) {
                format!("The manager refused to {action}")
            } else {
                format!("Can't {action}")
            };
            let next = match code {
                Code::NotFound => vec![list_step(*group)],
                Code::FailedPrecondition | Code::InvalidArgument => {
                    vec![check_state_step(*group)]
                }
                _ => Vec::new(),
            };
            (verdict, manager_message(message), next)
        }
        ManagementError::NotAccepted {
            group,
            action,
            desired,
        } => (
            format!("The manager did not accept the request to {action}"),
            vec![Message::Info(format!("  Its desired state is {desired}"))],
            vec![check_state_step(*group)],
        ),
    }
}

/// The manager's own explanation of a failed call, as a fact when it gave one.
fn manager_message(message: &str) -> Vec<Message> {
    if message.is_empty() {
        Vec::new()
    } else {
        vec![Message::Info(format!("  {message}"))]
    }
}

fn reference_failure(error: &ReferenceError) -> Typed {
    match error {
        ReferenceError::Unknown { group, reference } => (
            format!("No {} matches {reference}", group.noun()),
            Vec::new(),
            vec![list_step(*group)],
        ),
        ReferenceError::Ambiguous {
            group,
            reference,
            ids,
        } => (
            format!("{} {}s are named {reference}", ids.len(), group.noun()),
            ids.iter()
                .map(|id| Message::Info(format!("  {id}")))
                .collect(),
            vec![NextStep::new("Pass one of these ids instead")],
        ),
    }
}

fn code_failure(error: &CodeError) -> Typed {
    let route_facts = |ready: &[String]| {
        ready
            .iter()
            .map(|name| Message::Info(format!("  {name}")))
            .collect()
    };
    match error {
        CodeError::InferenceUnreachable { address, .. } => (
            format!("Can't reach the inference API at {address}"),
            Vec::new(),
            inference_address_steps(),
        ),
        CodeError::InferenceRefused { address, status } => (
            format!("The inference API at {address} answered with status {status}"),
            Vec::new(),
            inference_address_steps(),
        ),
        CodeError::InvalidRouteList { address, .. } => (
            format!("The answer from {address} is not an inference route list"),
            Vec::new(),
            inference_address_steps(),
        ),
        CodeError::NoRoute => (
            "The site has no ready inference route".to_owned(),
            Vec::new(),
            vec![NextStep::new(
                "Create a route in the console, or wait for its model to be ready",
            )],
        ),
        CodeError::UnknownRoute { agent, name, ready } => (
            format!("No ready inference route is named {name}"),
            route_facts(ready),
            vec![
                pass_route_step(*agent),
                NextStep::new("Or wait for its model to be ready"),
            ],
        ),
        CodeError::RouteRequired { agent, ready } => (
            "Choosing a route needs a terminal".to_owned(),
            route_facts(ready),
            vec![pass_route_step(*agent)],
        ),
        CodeError::AgentNotInstalled { agent } => (
            format!("{} is not installed", agent.display_name()),
            vec![Message::Info(format!(
                "  No `{}` executable on PATH",
                agent.name()
            ))],
            vec![
                NextStep::new(format!(
                    "Install {}, then run the command again:",
                    agent.display_name()
                ))
                .with_command(agent.install_hint()),
            ],
        ),
    }
}

/// Where to find the inference API address the command should use.
fn inference_address_steps() -> Vec<NextStep> {
    vec![
        NextStep::new("Pass its address with --inference-addr or DOTCORE_INFERENCE_ADDR"),
        NextStep::new("A route's Connect screen in the console shows the address"),
    ]
}

/// The copyable command that starts the agent on a named route.
fn pass_route_step(agent: CodingAgent) -> NextStep {
    NextStep::new("Pass one of these routes:")
        .with_command(format!("dotc code {} --model <route>", agent.name()))
}

/// Whether a status code means the manager judged the request itself, rather than failing to
/// carry it out.
fn refusal(code: Code) -> bool {
    matches!(
        code,
        Code::InvalidArgument
            | Code::NotFound
            | Code::AlreadyExists
            | Code::PermissionDenied
            | Code::FailedPrecondition
            | Code::OutOfRange
            | Code::Unauthenticated
    )
}

fn list_step(group: DeploymentGroup) -> NextStep {
    NextStep::new(format!("List the {}s:", group.noun())).with_command(group.list_command())
}

fn check_state_step(group: DeploymentGroup) -> NextStep {
    NextStep::new("Check its current state:").with_command(group.list_command())
}

fn placed_deployments(node_name: &str, deployments: &[PlacedDeployment]) -> Typed {
    let count = deployments.len();
    let verdict = format!(
        "Can't remove {node_name}: it still runs {count} deployment{}",
        if count == 1 { "" } else { "s" }
    );
    let facts = vec![
        Message::Heading("Placed on this node".to_owned()),
        Message::Table {
            header: ["KIND", "NAME", "STATE"].map(str::to_owned).to_vec(),
            rows: deployments
                .iter()
                .map(|deployment| {
                    vec![
                        deployment.kind.to_string(),
                        deployment.name.clone(),
                        deployment.state.to_owned(),
                    ]
                })
                .collect(),
        },
    ];
    let next = vec![
        NextStep::new("Remove them in the console first (the site must be running), or"),
        NextStep::new("Drop them without cleaning up the node:").with_command(format!(
            "dotc node remove {} --forget-deployments",
            shell_word(node_name)
        )),
    ];
    (verdict, facts, next)
}

/// Quotes a word for a copyable command only when it needs it.
pub(crate) fn shell_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
    {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use dotcore_site::{
        initial::input_store::InitialSiteWriteError,
        membership::{
            DeploymentKind, NodeLeaveError, NodeRemovalError, PlacedDeployment,
            join::{JoinTokenError, NodeFolderError, NodeJoinError},
        },
        repository::SiteRepositoryError,
        reset::transaction::SiteResetLockError,
    };
    use tonic::Status;

    use super::{Failure, chain_line};
    use crate::{
        management::{
            DeploymentGroup, ManagementError, Manager, ManagerAddress, ReferenceError, call_failed,
        },
        node::RejoinNameMismatch,
        terminal::{Message, NextStep},
    };

    fn placed() -> anyhow::Error {
        anyhow::Error::new(NodeRemovalError::PlacedDeployments {
            node_name: "gb10-host-2".to_owned(),
            deployments: vec![PlacedDeployment {
                kind: DeploymentKind::Model,
                id: "model-3eb5".to_owned(),
                name: "Gemma 4 E4B QAT".to_owned(),
                state: "present",
            }],
        })
        .context("cannot remove node \"gb10-host-2\"")
    }

    fn lines(failure: &Failure, verbose: bool) -> Vec<String> {
        failure
            .messages(verbose)
            .iter()
            .flat_map(Message::plain_lines)
            .collect()
    }

    #[test]
    fn placed_deployments_show_the_verdict_the_deployments_and_both_ways_forward() {
        let failure = Failure::from_error(&placed());

        assert_eq!(
            lines(&failure, false),
            [
                "✘ Can't remove gb10-host-2: it still runs 1 deployment",
                "Placed on this node",
                "  KIND   NAME             STATE",
                "  model  Gemma 4 E4B QAT  present",
                "Next",
                "  › Remove them in the console first (the site must be running), or",
                "  › Drop them without cleaning up the node:",
                "      dotc node remove gb10-host-2 --forget-deployments",
            ]
        );
    }

    #[test]
    fn verbose_adds_the_deduplicated_cause_chain_after_the_next_steps() {
        let lines = lines(&Failure::from_error(&placed()), true);

        assert_eq!(&lines[8], "Cause");
        assert_eq!(lines[9], "  cannot remove node \"gb10-host-2\"");
        assert!(lines[10].starts_with("  node \"gb10-host-2\" still has 1 deployment(s)"));
        assert_eq!(lines.len(), 11);
    }

    #[test]
    fn a_schema_mismatch_anywhere_in_the_chain_asks_for_a_reset() {
        let decode = serde_json::from_str::<u32>("\"text\"").expect_err("decode failure");
        let error = anyhow::Error::new(InitialSiteWriteError::DeploymentInspection(
            SiteRepositoryError::SchemaMismatch(decode),
        ))
        .context("site init failed");

        assert_eq!(
            Failure::from_error(&error).messages(false),
            [
                Message::Error(
                    "This site's saved files don't match this version of dotc".to_owned()
                ),
                Message::Next(vec![
                    NextStep::new("Reset the site (this deletes its state):")
                        .with_command("dotc site reset")
                ]),
            ]
        );
    }

    #[test]
    fn join_token_mistakes_name_the_command_that_prints_a_token() {
        let not_a_token = anyhow::Error::new(JoinTokenError::NotAToken).context("--token");
        let damaged = anyhow::Error::new(JoinTokenError::BadPayload("bad".to_owned()));

        assert_eq!(
            lines(&Failure::from_error(&not_a_token), false),
            [
                "✘ That isn't a join token",
                "Next",
                "  › Copy the whole token printed on the manager by:",
                "      dotc node add",
            ]
        );
        assert_eq!(
            lines(&Failure::from_error(&damaged), false)[0],
            "✘ The join token is not valid"
        );
    }

    #[test]
    fn an_intent_conflict_asks_for_replace() {
        let error = anyhow::Error::new(InitialSiteWriteError::IntentConflict);

        assert_eq!(
            lines(&Failure::from_error(&error), false),
            [
                "✘ This site already has a different configuration",
                "Next",
                "  › Replace it by adding --replace to the same command",
            ]
        );
    }

    #[test]
    fn a_running_node_stops_delete_and_leave_with_their_own_verdict() {
        let delete = anyhow::Error::new(SiteResetLockError::NodeRunning {
            site_root: "/x/node".into(),
        })
        .context("cannot delete the node folder at /x/node; stop the node server and retry");
        let leave = anyhow::Error::new(NodeLeaveError::NodeRunning {
            node_root: "/x/node".into(),
        })
        .context("cannot leave the site from /x/node");

        assert_eq!(
            lines(&Failure::from_error(&delete), false),
            [
                "✘ Can't delete /x/node: the node is running",
                "Next",
                "  › Stop the node server, then run the command again",
            ]
        );
        assert_eq!(
            lines(&Failure::from_error(&leave), false),
            [
                "✘ Can't leave the site from /x/node: the node is running",
                "Next",
                "  › Stop the node server, then run the command again",
            ]
        );
    }

    #[test]
    fn a_folder_of_another_site_or_trust_is_left_before_joining_as_a_new_node() {
        for (error, verdict) in [
            (
                NodeFolderError::OtherSite {
                    root: "/x/my node".into(),
                },
                "✘ /x/my node belongs to another site",
            ),
            (
                NodeFolderError::EarlierTrust {
                    root: "/x/my node".into(),
                },
                "✘ /x/my node belongs to an earlier trust of this site",
            ),
        ] {
            let error = anyhow::Error::new(error).context("cannot join again from /x/my node");

            assert_eq!(
                lines(&Failure::from_error(&error), false),
                [
                    verdict,
                    "Next",
                    "  › Leave that site on this machine:",
                    "      dotc node leave --site-dir '/x/my node'",
                    "  › Then, on the manager, get a new token to join as a new node:",
                    "      dotc node add",
                ]
            );
        }
    }

    #[test]
    fn a_running_node_stops_a_join_again() {
        let error = anyhow::Error::new(NodeFolderError::NodeRunning {
            root: "/x/node".into(),
        })
        .context("cannot join again from /x/node");

        assert_eq!(
            lines(&Failure::from_error(&error), false),
            [
                "✘ Can't join again from /x/node: the node is running",
                "Next",
                "  › Stop the node server, then run the command again",
            ]
        );
    }

    #[test]
    fn a_manager_refusing_the_folder_asks_to_leave_and_join_as_a_new_node() {
        let refused = |error: fn(tonic::Status) -> NodeJoinError| {
            anyhow::Error::new(error(tonic::Status::failed_precondition("refused")))
                .context("node join failed")
        };

        assert_eq!(
            lines(
                &Failure::from_error(&refused(NodeJoinError::NodeNotDeclared)),
                false
            ),
            [
                "✘ The site no longer has this node",
                "Next",
                "  › Leave the site on this machine:",
                "      dotc node leave",
                "  › Then, on the manager, get a new token to join as a new node:",
                "      dotc node add",
            ]
        );
        assert_eq!(
            lines(
                &Failure::from_error(&refused(NodeJoinError::CertificateNotPinned)),
                false
            )[0],
            "✘ The site doesn't recognize this node's certificate"
        );
        assert_eq!(
            lines(
                &Failure::from_error(&refused(NodeJoinError::CertificateExpired)),
                false
            ),
            [
                "✘ This node's certificate has expired",
                "Next",
                "  › Remove the node on the manager:",
                "      dotc node remove <node name>",
                "  › Leave the site on this machine:",
                "      dotc node leave",
                "  › Then, on the manager, get a new token to join as a new node:",
                "      dotc node add",
            ]
        );
    }

    #[test]
    fn a_failed_folder_rewrite_asks_for_another_join_with_a_new_token() {
        let error = anyhow::Error::new(NodeJoinError::FolderRewrite {
            root: "/x/node".into(),
            source: NodeFolderError::Storage(anyhow::anyhow!("disk full")),
        })
        .context("node join failed");

        assert_eq!(
            lines(&Failure::from_error(&error), false),
            [
                "✘ The site recorded the node's new addresses, but its folder wasn't updated",
                "  /x/node",
                "Next",
                "  › On the manager, get a new token to join again:",
                "      dotc node add",
            ]
        );
    }

    #[test]
    fn a_folder_rewrite_that_may_not_be_durable_asks_for_another_join() {
        let error = anyhow::Error::new(NodeJoinError::FolderRewriteUncertain {
            root: "/x/node".into(),
            source: NodeFolderError::RewriteUncertain(anyhow::anyhow!("sync failed")),
        })
        .context("node join failed");

        assert_eq!(
            lines(&Failure::from_error(&error), false),
            [
                "✘ The site recorded the node's new addresses, but the folder update may not be durable",
                "  /x/node",
                "Next",
                "  › On the manager, get a new token to join again:",
                "      dotc node add",
            ]
        );
    }

    #[test]
    fn a_folder_certificate_outside_its_validity_gets_its_own_next_steps() {
        let expired = anyhow::Error::new(NodeFolderError::CertificateExpired {
            root: "/x/node".into(),
        })
        .context("cannot join again from /x/node");
        let early = anyhow::Error::new(NodeFolderError::CertificateNotYetValid {
            root: "/x/node".into(),
        })
        .context("cannot join again from /x/node");
        let refused_early = anyhow::Error::new(NodeJoinError::CertificateNotYetValid(
            tonic::Status::failed_precondition("refused"),
        ))
        .context("node join failed");

        assert_eq!(
            lines(&Failure::from_error(&expired), false),
            [
                "✘ This node's certificate has expired",
                "Next",
                "  › Remove the node on the manager:",
                "      dotc node remove <node name>",
                "  › Leave the site on this machine:",
                "      dotc node leave",
                "  › Then, on the manager, get a new token to join as a new node:",
                "      dotc node add",
            ]
        );
        for error in [early, refused_early] {
            assert_eq!(
                lines(&Failure::from_error(&error), false),
                [
                    "✘ This node's certificate is not valid yet",
                    "Next",
                    "  › Check the clocks of this machine and the manager, then join again",
                ]
            );
        }
    }

    #[test]
    fn a_name_that_is_not_the_folders_node_asks_to_drop_the_flag() {
        let error = anyhow::Error::new(RejoinNameMismatch {
            name: "other".to_owned(),
            root: "/x/node".into(),
        });

        assert_eq!(
            lines(&Failure::from_error(&error), false),
            [
                "✘ other is not the node in /x/node",
                "Next",
                "  › Join again without --name, or with the name the node was added under",
            ]
        );
    }

    #[test]
    fn an_unmapped_failure_shows_its_outer_message_its_root_cause_and_the_verbose_hint() {
        let error = anyhow::anyhow!("connection refused").context("server start failed");

        assert_eq!(
            lines(&Failure::from_error(&error), false),
            [
                "✘ Server start failed",
                "  connection refused",
                "Next",
                "  › Details: rerun with --verbose",
            ]
        );
        let single = anyhow::anyhow!("no such directory");
        assert_eq!(
            lines(&Failure::from_error(&single), false),
            ["✘ No such directory"]
        );
    }

    #[test]
    fn the_plain_line_drops_repeated_causes_but_keeps_hinted_ones() {
        let error = anyhow::anyhow!("inner text").context("inner text: more");

        assert_eq!(chain_line(&error), "inner text: more");
    }

    #[test]
    fn an_unreachable_manager_names_the_address_and_how_to_start_or_point_at_it() {
        let (address, error) = unreachable_manager();
        let failure = Failure::from_error(&error.context("cannot list model deployments"));
        let verdict = format!("✘ Can't reach the manager at http://{address}");
        let layout = [
            verdict.as_str(),
            "Next",
            "  › Start the site manager:",
            "      dotcore",
            "  › Or pass its address with --manager or DOTCORE_MANAGEMENT_ADDR",
        ];

        assert_eq!(lines(&failure, false), layout);
        let verbose = lines(&failure, true);
        assert_eq!(verbose[..5], layout);
        assert_eq!(
            verbose[5..],
            [
                "Cause".to_owned(),
                "  cannot list model deployments".to_owned(),
                format!("  cannot reach the manager at http://{address}"),
                "  transport error".to_owned(),
                "  tcp connect error".to_owned(),
                format!(
                    "  {}",
                    std::io::Error::from_raw_os_error(libc::ECONNREFUSED)
                ),
            ]
        );
    }

    #[test]
    fn an_unreachable_manager_plain_line_carries_the_hint() {
        let (address, error) = unreachable_manager();

        assert_eq!(
            chain_line(&error),
            format!(
                "cannot reach the manager at http://{address}; start it with `dotcore` \
                 or pass its address with --manager: transport error: tcp connect error: \
                 {}",
                std::io::Error::from_raw_os_error(libc::ECONNREFUSED)
            )
        );
    }

    #[test]
    fn a_failed_precondition_shows_the_manager_message_and_where_to_check_the_state() {
        let error = anyhow::Error::new(call_failed(DeploymentGroup::Model, "pause Gemma")(
            Status::failed_precondition(
                "model lifecycle operation is not valid for the current state",
            ),
        ));
        let failure = Failure::from_error(&error);
        let layout = [
            "✘ The manager refused to pause Gemma",
            "  model lifecycle operation is not valid for the current state",
            "Next",
            "  › Check its current state:",
            "      dotc model list",
        ];

        assert_eq!(lines(&failure, false), layout);
        assert_eq!(
            lines(&failure, true),
            [
                &layout[..],
                &[
                    "Cause",
                    "  cannot pause Gemma: the manager returned FailedPrecondition: model \
                     lifecycle operation is not valid for the current state",
                ],
            ]
            .concat()
        );
        assert_eq!(
            chain_line(&error),
            "cannot pause Gemma: the manager returned FailedPrecondition: model lifecycle \
             operation is not valid for the current state; see `dotc model list`"
        );
    }

    #[test]
    fn a_not_found_status_lists_the_deployments() {
        let error = anyhow::Error::new(call_failed(DeploymentGroup::Application, "resume Notes")(
            Status::not_found("application deployment was not found"),
        ))
        .context("cannot resume Notes");
        let failure = Failure::from_error(&error);
        let layout = [
            "✘ The manager refused to resume Notes",
            "  application deployment was not found",
            "Next",
            "  › List the application deployments:",
            "      dotc application list",
        ];

        assert_eq!(lines(&failure, false), layout);
        assert_eq!(
            lines(&failure, true),
            [
                &layout[..],
                &[
                    "Cause",
                    "  cannot resume Notes",
                    "  cannot resume Notes: the manager returned NotFound: application \
                     deployment was not found",
                ],
            ]
            .concat()
        );
    }

    #[test]
    fn a_status_the_manager_could_not_carry_out_reads_as_cannot() {
        let error = anyhow::Error::new(call_failed(DeploymentGroup::Model, "pause Gemma")(
            Status::unavailable("Model lifecycle outcome is uncertain."),
        ));

        assert_eq!(
            lines(&Failure::from_error(&error), false),
            [
                "✘ Can't pause Gemma",
                "  Model lifecycle outcome is uncertain."
            ]
        );
    }

    #[test]
    fn an_unaccepted_lifecycle_request_shows_the_returned_desired_state() {
        let error = anyhow::Error::new(ManagementError::NotAccepted {
            group: DeploymentGroup::Model,
            action: "pause Gemma".to_owned(),
            desired: "Present".to_owned(),
        });

        assert_eq!(
            lines(&Failure::from_error(&error), false),
            [
                "✘ The manager did not accept the request to pause Gemma",
                "  Its desired state is Present",
                "Next",
                "  › Check its current state:",
                "      dotc model list",
            ]
        );
    }

    #[test]
    fn unknown_and_ambiguous_references_point_at_the_list_or_the_ids() {
        let unknown = anyhow::Error::new(ReferenceError::Unknown {
            group: DeploymentGroup::Model,
            reference: "gemma".to_owned(),
        });
        let ambiguous = anyhow::Error::new(ReferenceError::Ambiguous {
            group: DeploymentGroup::Model,
            reference: "Qwen".to_owned(),
            ids: vec!["model-2".to_owned(), "model-3".to_owned()],
        });

        assert_eq!(
            lines(&Failure::from_error(&unknown), false),
            [
                "✘ No model deployment matches gemma",
                "Next",
                "  › List the model deployments:",
                "      dotc model list",
            ]
        );
        assert_eq!(
            chain_line(&unknown),
            "no model deployment is named or has id \"gemma\"; see `dotc model list`"
        );
        assert_eq!(
            lines(&Failure::from_error(&ambiguous), false),
            [
                "✘ 2 model deployments are named Qwen",
                "  model-2",
                "  model-3",
                "Next",
                "  › Pass one of these ids instead",
            ]
        );
        assert_eq!(
            chain_line(&ambiguous),
            "2 model deployments are named \"Qwen\": model-2, model-3; pass one of the ids \
             instead"
        );
    }

    #[test]
    fn an_unknown_route_lists_the_ready_routes_to_pass_instead() {
        let error = anyhow::Error::new(crate::code::CodeError::UnknownRoute {
            agent: crate::code::CodingAgent::Pi,
            name: "draft".to_owned(),
            ready: vec!["chat".to_owned(), "code".to_owned()],
        });

        assert_eq!(
            lines(&Failure::from_error(&error), false),
            [
                "✘ No ready inference route is named draft",
                "  chat",
                "  code",
                "Next",
                "  › Pass one of these routes:",
                "      dotc code pi --model <route>",
                "  › Or wait for its model to be ready",
            ]
        );
    }

    #[test]
    fn a_refused_route_listing_says_where_to_find_the_address() {
        let error = anyhow::Error::new(crate::code::CodeError::InferenceRefused {
            address: "10.0.0.5:50052".parse().expect("address"),
            status: 503,
        });

        assert_eq!(
            lines(&Failure::from_error(&error), false),
            [
                "✘ The inference API at http://10.0.0.5:50052 answered with status 503",
                "Next",
                "  › Pass its address with --inference-addr or DOTCORE_INFERENCE_ADDR",
                "  › A route's Connect screen in the console shows the address",
            ]
        );
    }

    /// Connects to a loopback port nothing listens on and returns its address and the failure.
    fn unreachable_manager() -> (std::net::SocketAddr, anyhow::Error) {
        let address = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("free loopback port");
        let manager = address
            .to_string()
            .parse::<ManagerAddress>()
            .expect("loopback address");
        let error = crate::build_runtime()
            .expect("test runtime")
            .block_on(Manager::connect(&manager))
            .expect_err("nothing listens on the port");
        (address, anyhow::Error::new(error))
    }

    #[test]
    fn node_names_with_spaces_are_quoted_in_copyable_commands() {
        let error = anyhow::Error::new(NodeRemovalError::PlacedDeployments {
            node_name: "Worker two".to_owned(),
            deployments: Vec::new(),
        });

        assert!(
            lines(&Failure::from_error(&error), false)
                .contains(&"      dotc node remove 'Worker two' --forget-deployments".to_owned())
        );
    }

    fn unresolvable_node_host(host: &str) -> anyhow::Error {
        anyhow::Error::new(
            dotcore_backend::composition::CompositionError::UnresolvableNodeHost {
                host: dotcore_domain::SiteNodeHost::parse(host).expect("host"),
                source: dotcore_domain::HostResolutionError::Lookup {
                    host: host.to_owned(),
                    source: std::io::Error::other("failed to lookup address information"),
                },
            },
        )
        .context("server start failed")
    }

    #[test]
    fn an_unresolvable_node_host_asks_to_start_its_network_or_reset_the_host() {
        let error = unresolvable_node_host("gpu1.example.ts.net");

        assert_eq!(
            lines(&Failure::from_error(&error), false),
            [
                "✘ Can't resolve the node host gpu1.example.ts.net",
                "  failed to lookup address information",
                "Next",
                "  › Start the network that provides it, then start the server again",
                "  › Or reset the site with another host (this deletes its state):",
                "      dotc site reset --host <host>",
            ]
        );
        assert_eq!(
            chain_line(&error),
            "server start failed: cannot resolve the node host gpu1.example.ts.net; start \
             the network that provides it and start the server again, or reset the site with \
             another node host using `dotc site reset --host`: could not resolve host name \
             gpu1.example.ts.net; check that the network providing it is up: failed to \
             lookup address information"
        );
    }
}
