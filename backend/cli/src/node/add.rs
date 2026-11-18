//! Adding a node to the running site.
//!
//! Starts a join through the site manager's management API, shows how to run `dotc node join` on
//! the new machine, and follows the join. The operator allows the specific machine that arrives
//! (or the command does with `--yes`), and the manager records it only after the machine has
//! installed its own folder. Ending the command before the join completes cancels the join.

use std::{
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use anyhow::Context;
use clap::Args;
use dotcore::composition::DEFAULT_PRIVATE_PORT;
use dotcore_proto::dotcore::site::v1::{
    ApproveJoinRequest, CancelJoinRequest, JoinAttemptOutcome, JoinHostSource, JoinProposal,
    JoinTargetRuntime, RejectJoinRequest, StartJoinRequest, WatchJoinRequest, WatchJoinResponse,
    join_attempt_outcome, join_proposal, join_state,
    site_membership_service_client::SiteMembershipServiceClient, watch_join_response,
};
use dotcore_site::{membership::join::JoinToken, node::folder::CertificateFingerprint};
use tonic::{Code, Streaming, transport::Channel};

use super::{Notices, boxed, fingerprint_facts, fingerprint_grid, record_value, write_records};
use crate::{
    RootPathArgs,
    management::{ManagementError, ManagerArgs, single_line},
    site::parse_absolute_path,
    terminal::{
        ConfirmPrompt, Message, PromptError, Terminal, TerminalCondition,
        controlling_terminal_condition, open_terminal,
    },
};

/// Arguments of `dotc node add`.
#[derive(Debug, Args)]
pub struct NodeAddArgs {
    #[command(flatten)]
    manager: ManagerArgs,

    /// Write the token to this new file (mode 0600, removed when the command ends) instead of
    /// printing it.
    #[arg(long, value_name = "PATH", value_parser = parse_absolute_path)]
    pub token_file: Option<PathBuf>,

    /// Allow the node that presents the token without asking.
    #[arg(long)]
    pub yes: bool,
}

/// Starts a join on the running manager and follows it until the machine is recorded, cancelling
/// the join when the command ends any other way.
pub(crate) async fn node_add(_paths: &RootPathArgs, arguments: NodeAddArgs) -> anyhow::Result<()> {
    if !arguments.yes && controlling_terminal_condition() != TerminalCondition::Interactive {
        anyhow::bail!(
            "allowing a node needs a terminal; run in a terminal or pass --yes to allow the node that presents the token"
        );
    }

    let manager = arguments
        .manager
        .connect()
        .await
        .map_err(|error| match error {
            ManagementError::Unreachable { .. } => {
                anyhow::Error::new(error).context("adding a node needs the site manager running")
            }
            other => other.into(),
        })?;
    let mut joins = manager.membership();
    let started = joins
        .start_join(StartJoinRequest {})
        .await
        .map_err(|status| call_failed("start the join", &status))?
        .into_inner();

    let join_id = started.join_id.clone();
    // The interrupt handler is installed on its first poll, so it goes first: announcing the join
    // tells the operator the command is ready, and an interrupt after that must cancel the join.
    let outcome = tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => Err(anyhow::Error::new(PromptError::Cancelled)),
        added = run_join(&mut joins, started, &arguments) => added,
    };
    match outcome {
        Ok(joined) => {
            let mut terminal = (!arguments.yes).then(open_terminal);
            if let Some(records) = report_added(boxed(&mut terminal), &joined)? {
                write_records(&records)?;
            }
            Ok(())
        }
        Err(error) => {
            // The join may already have ended; the manager refuses a cancel then, which changes nothing.
            let _ = joins.cancel_join(CancelJoinRequest { join_id }).await;
            Err(error)
        }
    }
}

/// What the manager recorded for a machine.
struct JoinedMachine {
    node_id: String,
    name: String,
    host: String,
    rejoined: bool,
}

/// Announces the join command, then answers each proposal until the join ends.
async fn run_join(
    joins: &mut SiteMembershipServiceClient<Channel>,
    started: dotcore_proto::dotcore::site::v1::StartJoinResponse,
    arguments: &NodeAddArgs,
) -> anyhow::Result<JoinedMachine> {
    let token_file = match &arguments.token_file {
        Some(path) => Some(TokenFile::create(path, &started.token)?),
        None => None,
    };
    let join_command = match &token_file {
        Some(file) => format!("dotc node join --token-file {}", file.path_text()?),
        None => format!("dotc node join --token {}", started.token),
    };

    let mut terminal = (!arguments.yes).then(open_terminal);
    let ready = announce_ready(boxed(&mut terminal), &join_command)?;
    if let Some(notice) = loopback_manager_notice(&started.token) {
        Notices::new(boxed(&mut terminal)).warn(&notice)?;
    }
    drop(terminal);
    if let Some(records) = ready {
        write_records(&records)?;
    }

    let mut watch = joins
        .watch_join(WatchJoinRequest {
            join_id: started.join_id.clone(),
        })
        .await
        .map_err(|status| call_failed("follow the join", &status))?
        .into_inner();
    let joined = follow(joins, &started.join_id, &mut watch, arguments.yes).await;
    drop(token_file);
    joined
}

/// Reads the join's states until it ends, deciding on each proposal that awaits approval.
async fn follow(
    joins: &mut SiteMembershipServiceClient<Channel>,
    join_id: &str,
    watch: &mut Streaming<WatchJoinResponse>,
    yes: bool,
) -> anyhow::Result<JoinedMachine> {
    let mut last_proposal: Option<JoinProposal> = None;
    let mut shown_attempt: Option<JoinAttemptOutcome> = None;
    loop {
        let message = watch
            .message()
            .await
            .map_err(|status| call_failed("follow the join", &status))?
            .context(
                "the manager stopped reporting the join before it ended; run `dotc node add` again",
            )?;
        let Some(watch_join_response::Event::State(state)) = message.event else {
            continue;
        };
        match state
            .state
            .context("the manager reported a join without a state")?
        {
            join_state::State::Waiting(waiting) => {
                if waiting.last_attempt != shown_attempt {
                    if let Some(outcome) = &waiting.last_attempt {
                        show_attempt(outcome, yes)?;
                    }
                    shown_attempt = waiting.last_attempt;
                }
            }
            join_state::State::AwaitingApproval(awaiting) => {
                let proposal = awaiting
                    .proposal
                    .context("the manager sent an approval request without a proposal")?;
                let allowed = decide(&proposal, yes).await?;
                let decision = if allowed {
                    joins
                        .approve_join(ApproveJoinRequest {
                            join_id: join_id.to_owned(),
                            proposal_id: awaiting.proposal_id,
                        })
                        .await
                        .map(drop)
                } else {
                    joins
                        .reject_join(RejectJoinRequest {
                            join_id: join_id.to_owned(),
                            proposal_id: awaiting.proposal_id,
                        })
                        .await
                        .map(drop)
                };
                match decision {
                    Ok(()) if allowed => last_proposal = Some(proposal),
                    Ok(()) => {}
                    // The node's attempt ended while the operator decided; the watch reports why.
                    Err(status)
                        if matches!(status.code(), Code::FailedPrecondition | Code::NotFound) =>
                    {
                        Notices::new(None).warn(
                            "the node's request ended before the decision reached the manager",
                        )?;
                    }
                    Err(status) => return Err(call_failed("send the decision", &status)),
                }
            }
            join_state::State::Committed(committed) => {
                let proposal =
                    last_proposal.context("the join was recorded without an approved proposal")?;
                return Ok(JoinedMachine {
                    node_id: committed.node_id,
                    name: proposal.display_name,
                    host: proposal.host,
                    rejoined: matches!(proposal.kind, Some(join_proposal::Kind::Rejoin(_))),
                });
            }
            join_state::State::Cancelled(_) => anyhow::bail!("the join was cancelled"),
            join_state::State::Expired(_) => {
                anyhow::bail!("the join expired; run `dotc node add` again for a new token")
            }
            join_state::State::Failed(failed) => anyhow::bail!(
                "the join failed: {}; run `dotc node add` again for a new token",
                single_line(&failed.reason)
            ),
        }
    }
}

/// Tells the operator how to join, on the terminal when interactive, else as the join command
/// record. Returns the records to write when there is no terminal.
fn announce_ready(
    terminal: Option<&mut dyn Terminal>,
    join_command: &str,
) -> anyhow::Result<Option<String>> {
    let Some(terminal) = terminal else {
        return Ok(Some(format!(
            "Node join ready\nDOTCORE_JOIN_COMMAND={}\n",
            record_value(join_command)?
        )));
    };
    terminal.message(&Message::Info("On the new machine, run:".to_owned()))?;
    terminal.message(&Message::Command(join_command.to_owned()))?;
    terminal.message(&Message::Hint("Waiting for the node...".to_owned()))?;
    Ok(None)
}

/// Tells the operator how a remote machine joins a manager whose declared host is a loopback
/// address, which the token names but only this machine reaches.
fn loopback_manager_notice(token: &str) -> Option<String> {
    let token = JoinToken::parse(token).ok()?;
    let host = token.endpoint.host_str()?.to_owned();
    let address: std::net::Ipv4Addr = host.parse().ok()?;
    address.is_loopback().then(|| {
        format!(
            "the manager host {host} is a loopback address, which only this machine reaches; a \
             remote node must join with --manager-host <address it reaches this machine at>, and \
             the manager must listen on that address"
        )
    })
}

/// Shows how the previous attempt of the join ended; the token stays usable for another.
fn show_attempt(outcome: &JoinAttemptOutcome, yes: bool) -> anyhow::Result<()> {
    let text = match &outcome.outcome {
        Some(join_attempt_outcome::Outcome::Rejected(_)) => {
            "the node was not allowed to join; the token still works for another attempt".to_owned()
        }
        Some(join_attempt_outcome::Outcome::Failed(reason)) => format!(
            "the node's attempt failed: {}; the token still works for another attempt",
            single_line(reason)
        ),
        None => return Ok(()),
    };
    if yes {
        Notices::new(None).warn(&text)
    } else {
        let mut terminal = open_terminal();
        Notices::new(Some(terminal.as_mut())).warn(&text)
    }
}

/// Decides on a proposal: allows it unasked with `yes`, else asks on the terminal without
/// blocking the join's progress.
async fn decide(proposal: &JoinProposal, yes: bool) -> anyhow::Result<bool> {
    if yes {
        if proposal.host_differs_from_peer {
            Notices::new(None).warn(&host_mismatch_warning(proposal))?;
        }
        return Ok(true);
    }
    let proposal = proposal.clone();
    tokio::task::spawn_blocking(move || {
        let mut terminal = open_terminal();
        decide_with_terminal(&proposal, terminal.as_mut())
    })
    .await
    .context("ask whether to allow the node")?
}

/// Shows the proposal, the peer address, and the request key fingerprint with its pattern, then
/// asks.
fn decide_with_terminal(
    proposal: &JoinProposal,
    terminal: &mut dyn Terminal,
) -> anyhow::Result<bool> {
    let (heading, question, mut facts) = match &proposal.kind {
        Some(join_proposal::Kind::Rejoin(rejoin)) => (
            "A node asks to join again",
            "Allow this node to join again?",
            vec![
                ("name".to_owned(), single_line(&proposal.display_name)),
                (
                    "host".to_owned(),
                    address_change(&rejoin.previous_host, &proposal.host),
                ),
                (
                    "manager host".to_owned(),
                    address_change(
                        manager_host_text(&rejoin.previous_manager_host),
                        manager_host_text(&rejoin.manager_host),
                    ),
                ),
                (
                    "deployments".to_owned(),
                    deployments_text(rejoin.deployments),
                ),
            ],
        ),
        new_node => {
            let targets = match new_node {
                Some(join_proposal::Kind::NewNode(new_node)) => new_node.targets.as_slice(),
                _ => &[],
            };
            let mut facts = vec![
                ("name".to_owned(), single_line(&proposal.display_name)),
                (
                    "host".to_owned(),
                    format!("{} ({})", proposal.host, host_source_text(proposal)),
                ),
                ("models".to_owned(), targets_summary(targets, true)),
                ("applications".to_owned(), targets_summary(targets, false)),
            ];
            if proposal.private_port != u32::from(DEFAULT_PRIVATE_PORT) {
                facts.insert(
                    2,
                    ("private port".to_owned(), proposal.private_port.to_string()),
                );
            }
            ("A node asks to join", "Allow this node to join?", facts)
        }
    };
    facts.push(("connected from".to_owned(), proposal.peer_address.clone()));
    let fingerprint = CertificateFingerprint::parse(proposal.key_fingerprint.as_str())
        .context("the manager sent a malformed key fingerprint")?;
    facts.extend(fingerprint_facts(&fingerprint));

    terminal.message(&Message::Heading(heading.to_owned()))?;
    terminal.message(&Message::KeyValues(facts))?;
    terminal.message(&fingerprint_grid(&fingerprint))?;
    if proposal.host_differs_from_peer {
        terminal.message(&Message::Warning(host_mismatch_notice(proposal)))?;
    }
    Ok(terminal.confirm(&ConfirmPrompt::new(question, false))?)
}

/// Reports the recorded node, on the terminal when interactive, else as records. Returns the
/// records to write when there is no terminal.
fn report_added(
    terminal: Option<&mut dyn Terminal>,
    joined: &JoinedMachine,
) -> anyhow::Result<Option<String>> {
    let (verb, header) = if joined.rejoined {
        ("Rejoined", "Node rejoined")
    } else {
        ("Added", "Node added")
    };
    let Some(terminal) = terminal else {
        return Ok(Some(format!(
            "{header}\nDOTCORE_NODE_ID={}\nDOTCORE_NODE_NAME={}\nDOTCORE_NODE_HOST={}\n",
            record_value(&joined.node_id)?,
            record_value(&joined.name)?,
            record_value(&joined.host)?,
        )));
    };
    terminal.message(&Message::Success(format!(
        "{verb} {} at {}",
        single_line(&joined.name),
        joined.host
    )))?;
    Ok(None)
}

/// Describes where the proposed host comes from.
fn host_source_text(proposal: &JoinProposal) -> &'static str {
    match proposal.host_source() {
        JoinHostSource::Observed => "observed from the connection",
        JoinHostSource::Declared => "declared for the node",
        JoinHostSource::Proposed | JoinHostSource::Unspecified => "proposed by the node",
    }
}

/// Describes the proposed model or application targets, or `none`.
fn targets_summary(
    targets: &[dotcore_proto::dotcore::site::v1::JoinProposedTarget],
    models: bool,
) -> String {
    use dotcore_proto::dotcore::site::v1::ExecutionTargetWorkloadKind;

    let wanted = if models {
        ExecutionTargetWorkloadKind::Model
    } else {
        ExecutionTargetWorkloadKind::Application
    };
    let summaries = targets
        .iter()
        .filter(|target| target.workload_kind() == wanted)
        .map(|target| {
            let runtime = match target.runtime() {
                JoinTargetRuntime::Simulated => "simulated",
                JoinTargetRuntime::Nvidia => "nvidia",
                JoinTargetRuntime::Apple => "apple",
                JoinTargetRuntime::Docker => "docker",
                JoinTargetRuntime::Unspecified => "unknown",
            };
            let mut details = target.accelerator_device_ids.clone();
            if !target.platform_label.is_empty() {
                details.push(target.platform_label.clone());
            }
            if target.runtime() == JoinTargetRuntime::Apple {
                details.push(
                    match target.native_workload_lifetime() {
                        dotcore_proto::dotcore::site::v1::NativeWorkloadLifetime::Ephemeral => {
                            "ephemeral"
                        }
                        _ => "persistent",
                    }
                    .to_owned(),
                );
            }
            format!("{runtime} ({})", details.join(", "))
        })
        .collect::<Vec<_>>();
    if summaries.is_empty() {
        "none".to_owned()
    } else {
        summaries.join(", ")
    }
}

/// Describes an address as kept or as replaced.
fn address_change(old: &str, new: &str) -> String {
    if old == new {
        format!("{old} (no change)")
    } else {
        format!("{old} -> {new}")
    }
}

/// An empty manager host means the node reaches the manager at the manager's declared host.
fn manager_host_text(manager_host: &str) -> &str {
    if manager_host.is_empty() {
        "default"
    } else {
        manager_host
    }
}

fn deployments_text(count: u32) -> String {
    match count {
        0 => "none placed on this node".to_owned(),
        1 => "1 stays on this node".to_owned(),
        count => format!("{count} stay on this node"),
    }
}

/// The terminal form of the host mismatch warning.
fn host_mismatch_notice(proposal: &JoinProposal) -> String {
    format!(
        "Proposed host {} differs from where the node connected from ({})",
        proposal.host,
        peer_ip(proposal)
    )
}

/// The stderr form of the host mismatch warning, used when no terminal asks.
fn host_mismatch_warning(proposal: &JoinProposal) -> String {
    format!(
        "the node proposes host {} but connected from {}; confirm that other nodes reach it at the proposed host",
        proposal.host,
        peer_ip(proposal)
    )
}

/// The address part of the peer address, which is `ip:port`.
fn peer_ip(proposal: &JoinProposal) -> String {
    proposal
        .peer_address
        .parse::<std::net::SocketAddr>()
        .map_or_else(
            |_| proposal.peer_address.clone(),
            |address| address.ip().to_string(),
        )
}

/// Maps a refused management call to an error naming what was attempted.
fn call_failed(action: &str, status: &tonic::Status) -> anyhow::Error {
    anyhow::anyhow!(
        "cannot {action}: the manager returned {:?}: {}",
        status.code(),
        status.message()
    )
}

/// A token file that exists only while the command runs, readable by its owner alone.
struct TokenFile {
    path: PathBuf,
}

impl TokenFile {
    /// Creates `path` with the token without ever exposing a partial file or replacing one.
    fn create(path: &Path, token: &str) -> anyhow::Result<Self> {
        let staging = path.with_extension(format!("tmp-{}", std::process::id()));
        let write = || -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&staging)?;
            writeln!(file, "{token}")?;
            dotcore_domain::fs_sync::sync(&file)?;
            // A hard link fails when the path exists, so a file created meanwhile is never replaced.
            std::fs::hard_link(&staging, path)
        };
        let linked = write();
        let _ = std::fs::remove_file(&staging);
        linked.map_err(|source| {
            let context = if source.kind() == std::io::ErrorKind::AlreadyExists {
                format!(
                    "token file {} already exists; remove it or choose another path",
                    path.display()
                )
            } else {
                format!("write token file {}", path.display())
            };
            anyhow::Error::new(source).context(context)
        })?;
        Ok(Self {
            path: path.to_owned(),
        })
    }

    fn path_text(&self) -> anyhow::Result<&str> {
        self.path.to_str().context("token file path must be UTF-8")
    }
}

impl Drop for TokenFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::site::v1::{
        ExecutionTargetWorkloadKind, JoinProposedTarget, NativeWorkloadLifetime,
    };

    use super::*;
    use crate::terminal::{GridColor, ScriptedAnswer, ScriptedTerminal};

    fn target(
        kind: ExecutionTargetWorkloadKind,
        runtime: JoinTargetRuntime,
        devices: &[&str],
        platform: &str,
    ) -> JoinProposedTarget {
        JoinProposedTarget {
            workload_kind: kind as i32,
            runtime: runtime as i32,
            platform_label: platform.to_owned(),
            accelerator_device_ids: devices.iter().map(|device| (*device).to_owned()).collect(),
            native_workload_lifetime: NativeWorkloadLifetime::Unspecified as i32,
        }
    }

    #[test]
    fn targets_are_summarized_by_workload_kind() {
        let targets = [
            target(
                ExecutionTargetWorkloadKind::Model,
                JoinTargetRuntime::Simulated,
                &["GPU-1f0d"],
                "linux/amd64",
            ),
            target(
                ExecutionTargetWorkloadKind::Application,
                JoinTargetRuntime::Docker,
                &[],
                "linux/amd64",
            ),
        ];

        assert_eq!(
            targets_summary(&targets, true),
            "simulated (GPU-1f0d, linux/amd64)"
        );
        assert_eq!(targets_summary(&targets, false), "docker (linux/amd64)");
        assert_eq!(targets_summary(&[], true), "none");
    }

    #[test]
    fn the_review_shows_the_grouped_key_fingerprint_and_its_pattern() {
        let proposal = JoinProposal {
            node_id: "node-7".to_owned(),
            display_name: "gpu-box".to_owned(),
            host: "192.0.2.10".to_owned(),
            host_source: JoinHostSource::Proposed as i32,
            peer_address: "192.0.2.10:41000".to_owned(),
            private_port: u32::from(DEFAULT_PRIVATE_PORT),
            key_fingerprint: "87524a4a3c1f9e0b5d26e8f1a7c4093b6e2d51f8c0a97b34e1d6f2085ab9c7e3"
                .to_owned(),
            kind: Some(join_proposal::Kind::NewNode(Default::default())),
            ..JoinProposal::default()
        };
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(false)]);

        let allowed = decide_with_terminal(&proposal, &mut terminal).expect("decision");

        assert!(!allowed);
        assert_eq!(
            terminal.lines(),
            [
                "A node asks to join",
                "  name             gpu-box",
                "  host             192.0.2.10 (proposed by the node)",
                "  models           none",
                "  applications     none",
                "  connected from   192.0.2.10:41000",
                "  key fingerprint  8752 4a4a 3c1f 9e0b 5d26 e8f1 a7c4 093b",
                "                   6e2d 51f8 c0a9 7b34 e1d6 f208 5ab9 c7e3",
                "  ████    ▓▓▓▓    ████",
                "  ████    ▓▓▓▓    ████",
                "  ▓▓▓▓    ░░░░    ▓▓▓▓",
                "  ▓▓▓▓    ░░░░    ▓▓▓▓",
                "  ░░░░            ░░░░",
                "  ░░░░            ░░░░",
                "  ████░░░░░░░░░░░░████",
                "  ████░░░░░░░░░░░░████",
                "  ▓▓▓▓████░░░░████▓▓▓▓",
                "  ▓▓▓▓████░░░░████▓▓▓▓",
            ]
        );
        assert_eq!(
            terminal.messages()[2],
            Message::ColorGrid {
                colors: vec![GridColor::Blue, GridColor::Yellow, GridColor::Magenta],
                rows: vec![
                    vec![Some(0), None, Some(1), None, Some(0)],
                    vec![Some(1), None, Some(2), None, Some(1)],
                    vec![Some(2), None, None, None, Some(2)],
                    vec![Some(0), Some(2), Some(2), Some(2), Some(0)],
                    vec![Some(1), Some(0), Some(2), Some(0), Some(1)],
                ],
            }
        );
    }

    #[test]
    fn a_rejoin_shows_how_many_deployments_stay_and_what_changes() {
        assert_eq!(deployments_text(0), "none placed on this node");
        assert_eq!(deployments_text(1), "1 stays on this node");
        assert_eq!(deployments_text(3), "3 stay on this node");
        assert_eq!(
            address_change("10.0.0.3", "10.0.0.3"),
            "10.0.0.3 (no change)"
        );
        assert_eq!(
            address_change("10.0.0.3", "10.0.0.4"),
            "10.0.0.3 -> 10.0.0.4"
        );
        assert_eq!(manager_host_text(""), "default");
    }

    #[test]
    fn only_a_loopback_manager_host_gets_the_remote_node_notice() {
        let token = |host: &str| {
            JoinToken::generate(
                url::Url::parse(&format!("https://{host}:50053")).expect("endpoint"),
                "site",
                "manager",
                dotcore_site::node::folder::CertificateFingerprint::from_bytes([1; 32]),
                dotcore_site::node::folder::CertificateFingerprint::from_bytes([2; 32]),
            )
            .expect("token")
            .encode()
        };

        assert!(loopback_manager_notice(&token("127.0.0.2")).is_some());
        assert_eq!(loopback_manager_notice(&token("192.0.2.10")), None);
        assert_eq!(loopback_manager_notice("not a token"), None);
    }
}
