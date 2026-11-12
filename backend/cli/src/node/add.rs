//! Adding a node to a site.
//!
//! Waits on the stopped manager for one new node to join. The command holds the site exclusively
//! while it waits, so it refuses to run next to a live manager. The operator allows the specific
//! node that arrives, and the site records it only after the node has installed its own folder.

use std::{
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use clap::Args;
use dotcore::{
    composition::{
        DEFAULT_INFERENCE_PORT, DEFAULT_PRIVATE_PORT, ManagerPrivateListener, PRIVATE_ADDR_ENV,
    },
    model::engine::declaration_validator,
};
use dotcore_domain::{BindAddress, SiteNodeHost, SystemHostResolver, timing::Timing};
use dotcore_proto::dotcore::site::v1::{Site, SiteNode};
use dotcore_site::{
    initial::validation::SiteInputBundleValidator,
    membership::{
        OfflineSite,
        join::{
            JoinApproval, JoinDecision, JoinHost, JoinHostError, JoinHostLimits, JoinHostSettings,
            JoinHostSource, JoinReview, JoinSiteWriter, JoinedNode, LocalPeers, NodeRejoin,
            ReviewedJoin, blocking_review,
        },
    },
};
use futures_util::future::BoxFuture;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{Notices, boxed, parse_timeout, record_value, write_records};
use crate::{
    RootPathArgs,
    hosts::{
        HostEnvironment, InterfaceAddress, SystemHostEnvironment, inference_address_remedy,
        private_address_remedy,
    },
    site::parse_absolute_path,
    terminal::{
        ConfirmPrompt, Message, PromptError, Terminal, TerminalCondition,
        controlling_terminal_condition, open_terminal,
    },
};

/// Arguments of `dotc node add`.
#[derive(Debug, Args)]
pub struct NodeAddArgs {
    /// IPv4 address or host name and port of the one-time join listener; defaults to the
    /// manager's private address. A host name is resolved by this machine.
    #[arg(long, value_name = "ADDR")]
    pub listen: Option<BindAddress>,

    /// Port of the manager's private listener, which joined nodes dial on the manager address
    /// they reach it at; defaults to the DOTCORE_PRIVATE_ADDR port, else 50053. The manager
    /// listens on 50053 by default once nodes have joined, so another port needs
    /// DOTCORE_PRIVATE_ADDR (or `dotcore --private-addr`) when the manager starts.
    #[arg(long, value_name = "PORT", value_parser = clap::value_parser!(u16).range(1..))]
    pub manager_private_port: Option<u16>,

    /// How long the join token stays valid, for example 90s, 15m, or 2h.
    #[arg(long, value_name = "DURATION", default_value = "15m", value_parser = parse_timeout)]
    pub timeout: Duration,

    /// Write the token to this new file (mode 0600, removed when the command ends) instead of
    /// printing it.
    #[arg(long, value_name = "PATH", value_parser = parse_absolute_path)]
    pub token_file: Option<PathBuf>,

    /// Allow the node that presents the token without asking.
    #[arg(long)]
    pub yes: bool,
}

/// Waits for one node to join the stopped site and records it.
pub(crate) async fn node_add(paths: &RootPathArgs, arguments: NodeAddArgs) -> anyhow::Result<()> {
    let site_dir = paths.site_dir()?;
    if !arguments.yes && controlling_terminal_condition() != TerminalCondition::Interactive {
        anyhow::bail!(
            "allowing a node needs a terminal; run in a terminal or pass --yes to allow the node that presents the token"
        );
    }

    let site = OfflineSite::open(
        &site_dir,
        &SiteInputBundleValidator::new(
            declaration_validator(),
            dotcore::model::site_declaration_rules(),
        ),
    )
    .map_err(|error| {
        anyhow::Error::new(error).context(format!(
            "cannot add a node to the site at {}",
            site_dir.display()
        ))
    })?;
    let identity = site
        .host_identity()
        .context("load the manager identity for the join listener")?;
    let current = site.site().context("read the site")?;
    let manager = declared_manager(&current)?.clone();

    let private_addr = std::env::var(PRIVATE_ADDR_ENV).ok();
    let (private_port, listen) = resolve_endpoints(
        &manager.host,
        arguments.manager_private_port,
        arguments.listen,
        private_addr.as_deref(),
    )?;
    let listen_address = listen
        .resolve(&SystemHostResolver)
        .await
        .with_context(|| format!("resolve the join listener address {listen}"))?;
    let listener = TcpListener::bind(listen_address)
        .await
        .with_context(|| format!("bind the join listener at {listen}; is the address free?"))?;
    let bound = listener
        .local_addr()
        .context("read the join listener address")?;
    let advertised = advertised_endpoint(bound.port(), &manager.host)?;
    let token = identity
        .token(advertised.clone())
        .context("create the join token")?;

    let encoded = token.encode();
    let token_file = match &arguments.token_file {
        Some(path) => Some(TokenFile::create(path, &encoded)?),
        None => None,
    };
    let join_command = match &token_file {
        Some(file) => format!("dotc node join --token-file {}", file.path_text()?),
        None => join_command_with_token(&encoded),
    };

    let mut terminal = (!arguments.yes).then(open_terminal);
    let ready = announce_ready(boxed(&mut terminal), &join_command)?;
    if let Some(notice) = private_port_notice(private_port, &manager.host) {
        Notices::new(boxed(&mut terminal)).warn(&notice)?;
    }
    if let Some(notice) = loopback_manager_notice(&manager.host, private_port) {
        Notices::new(boxed(&mut terminal)).warn(&notice)?;
    }
    drop(terminal);
    if let Some(records) = ready {
        write_records(&records)?;
    }

    let host = JoinHost::new(
        JoinHostSettings {
            identity,
            token,
            manager_private_port: private_port,
            allow_observed_loopback: false,
            local_peers: local_join_peers(&SystemHostEnvironment)?,
            limits: JoinHostLimits {
                overall_timeout: arguments.timeout,
                ..JoinHostLimits::default()
            },
        },
        Arc::new(site),
        Arc::new(CliApproval { yes: arguments.yes }),
        Timing::for_process().join,
    );
    let joined = tokio::select! {
        served = host.serve(listener) => served.map_err(|error| host_failure(error, &site_dir))?,
        _ = tokio::signal::ctrl_c() => return Err(anyhow::Error::new(PromptError::Cancelled)),
    };
    drop(token_file);

    let mut terminal = (!arguments.yes).then(open_terminal);
    if let Some(records) = report_added(boxed(&mut terminal), &joined)? {
        write_records(&records)?;
    }
    Ok(())
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

/// Returns the join connection sources that come from this machine: every loopback address and
/// the addresses on its network interfaces.
fn local_join_peers(environment: &dyn HostEnvironment) -> anyhow::Result<LocalPeers> {
    let interfaces = environment
        .interface_addresses()
        .context("read this machine's network addresses")?;
    Ok(LocalPeers::machine(
        interfaces.iter().map(InterfaceAddress::address),
    ))
}

/// Tells the operator to start the manager with the private port given to joining nodes when it
/// is not the default, the only port the manager listens on without a configured private address.
fn private_port_notice(private_port: u16, manager_host: &str) -> Option<String> {
    (private_port != DEFAULT_PRIVATE_PORT).then(|| {
        format!(
            "nodes will reach the manager on port {private_port}; start the manager with {}",
            private_address_remedy(&format!("{manager_host}:{private_port}"))
        )
    })
}

/// Tells the operator how a remote node joins a manager whose declared host is a loopback
/// address, which the token names but only this machine reaches. The manager's inference
/// listener defaults to that loopback host too, so the node's apps need it moved as well.
fn loopback_manager_notice(manager_host: &str, private_port: u16) -> Option<String> {
    let address = SiteNodeHost::parse(manager_host).ok()?.ipv4()?;
    address.is_loopback().then(|| {
        format!(
            "the manager host {manager_host} is a loopback address, which only this machine \
             reaches; a remote node must join with --manager-host <address it reaches this \
             machine at>, and this command and the manager need {}; for the node's apps to \
             reach inference, the manager also needs {}",
            private_address_remedy(&format!("<that address>:{private_port}")),
            inference_address_remedy(&format!("<that address>:{DEFAULT_INFERENCE_PORT}"))
        )
    })
}

/// Reports the recorded node, on the terminal when interactive, else as records. Returns the
/// records to write when there is no terminal.
fn report_added(
    terminal: Option<&mut dyn Terminal>,
    joined: &JoinedNode,
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
            record_value(&joined.display_name)?,
            joined.host,
        )));
    };
    terminal.message(&Message::Success(format!(
        "{verb} {} at {}",
        joined.display_name, joined.host
    )))?;
    Ok(None)
}

/// Asks the operator on the controlling terminal whether the node that arrived may join.
struct CliApproval {
    yes: bool,
}

impl JoinApproval for CliApproval {
    fn review(
        &self,
        review: JoinReview,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, JoinDecision> {
        let yes = self.yes;
        // A terminal read cannot be interrupted; the host stops waiting for it when the join
        // ends, and the process exit releases the blocked thread.
        blocking_review(review, cancel, move |review, _cancel| {
            let decision = if yes {
                allow_unattended(review)
            } else {
                let mut terminal = open_terminal();
                decide_with_terminal(review, terminal.as_mut())
            };
            decision.unwrap_or_else(|error| {
                eprintln!("cannot ask whether to allow the node: {error:#}");
                JoinDecision::Reject
            })
        })
    }
}

/// Adds the command the operator should run next to a host failure.
fn host_failure(error: JoinHostError, site_dir: &Path) -> anyhow::Error {
    let hint = match &error {
        JoinHostError::Expired | JoinHostError::TooManyFailedAttempts => {
            "run `dotc node add` again for a new token".to_owned()
        }
        JoinHostError::PublicationUncertain(_) => format!(
            "inspect {}/site.yaml and its trust/worker-pins before retrying",
            site_dir.display()
        ),
        JoinHostError::InvalidSettings(_) | JoinHostError::Transport(_) => {
            "check the listener settings and run `dotc node add` again".to_owned()
        }
    };
    anyhow::Error::new(error).context(format!("node join did not complete; {hint}"))
}

fn allow_unattended(review: &JoinReview) -> anyhow::Result<JoinDecision> {
    if review.host_differs_from_peer {
        Notices::new(None).warn(&host_mismatch_warning(review))?;
    }
    Ok(JoinDecision::Allow)
}

/// Shows the proposal, the peer address, and the request key fingerprint, then asks.
fn decide_with_terminal(
    review: &JoinReview,
    terminal: &mut dyn Terminal,
) -> anyhow::Result<JoinDecision> {
    let proposal = match &review.request {
        ReviewedJoin::New(proposal) => proposal,
        ReviewedJoin::Rejoin(rejoin) => {
            return decide_rejoin_with_terminal(review, rejoin, terminal);
        }
    };
    let mut facts = vec![
        ("name".to_owned(), proposal.display_name.clone()),
        (
            "host".to_owned(),
            format!(
                "{} ({})",
                review.host,
                match review.host_source {
                    JoinHostSource::Proposed => "proposed by the node",
                    JoinHostSource::Observed => "observed from the connection",
                    JoinHostSource::Declared => "declared for the node",
                }
            ),
        ),
        ("models".to_owned(), proposal.inference_summary()),
        ("applications".to_owned(), proposal.applications_summary()),
        ("connected from".to_owned(), review.peer_address.to_string()),
        (
            "key fingerprint".to_owned(),
            review.csr_key_fingerprint.as_str().to_owned(),
        ),
    ];
    if proposal.private_port != DEFAULT_PRIVATE_PORT {
        facts.insert(
            2,
            ("private port".to_owned(), proposal.private_port.to_string()),
        );
    }
    terminal.message(&Message::Heading("A node asks to join".to_owned()))?;
    terminal.message(&Message::KeyValues(facts))?;
    if review.host_differs_from_peer {
        terminal.message(&Message::Warning(host_mismatch_notice(review)))?;
    }
    let allowed = terminal.confirm(&ConfirmPrompt::new("Allow this node to join?", false))?;
    Ok(if allowed {
        JoinDecision::Allow
    } else {
        JoinDecision::Reject
    })
}

/// Shows a declared node's rejoin with what changes in its addresses and what stays, then asks.
fn decide_rejoin_with_terminal(
    review: &JoinReview,
    rejoin: &NodeRejoin,
    terminal: &mut dyn Terminal,
) -> anyhow::Result<JoinDecision> {
    let (declared, rejoined) = (&rejoin.declared, &rejoin.rejoined);
    let mut facts = vec![
        ("name".to_owned(), declared.display_name.clone()),
        (
            "host".to_owned(),
            address_change(&declared.host, &rejoined.host),
        ),
        (
            "manager host".to_owned(),
            address_change(
                manager_host_text(&declared.manager_host),
                manager_host_text(&rejoined.manager_host),
            ),
        ),
        (
            "deployments".to_owned(),
            deployments_text(rejoin.deployments),
        ),
        ("connected from".to_owned(), review.peer_address.to_string()),
        (
            "key fingerprint".to_owned(),
            review.csr_key_fingerprint.as_str().to_owned(),
        ),
    ];
    if let Some(change) = observation_change(rejoin) {
        facts.insert(3, ("observation".to_owned(), change));
    }
    terminal.message(&Message::Heading("A node asks to join again".to_owned()))?;
    terminal.message(&Message::KeyValues(facts))?;
    if review.host_differs_from_peer {
        terminal.message(&Message::Warning(host_mismatch_notice(review)))?;
    }
    let allowed = terminal.confirm(&ConfirmPrompt::new("Allow this node to join again?", false))?;
    Ok(if allowed {
        JoinDecision::Allow
    } else {
        JoinDecision::Reject
    })
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

/// Describes how the node's observation configuration changes, or `None` when it stays.
fn observation_change(rejoin: &NodeRejoin) -> Option<String> {
    let (installed, offered) = (&rejoin.installed_observation, &rejoin.observation);
    if installed == offered {
        None
    } else if installed.interval_seconds != offered.interval_seconds {
        Some(format!(
            "refresh {}s -> {}s",
            installed.interval_seconds, offered.interval_seconds
        ))
    } else {
        Some("updated".to_owned())
    }
}

fn deployments_text(count: usize) -> String {
    match count {
        0 => "none placed on this node".to_owned(),
        1 => "1 stays on this node".to_owned(),
        count => format!("{count} stay on this node"),
    }
}

/// The terminal form of the host mismatch warning.
fn host_mismatch_notice(review: &JoinReview) -> String {
    format!(
        "Proposed host {} differs from where the node connected from ({})",
        review.host,
        review.peer_address.ip()
    )
}

/// The stderr form of the host mismatch warning, used when no terminal asks.
fn host_mismatch_warning(review: &JoinReview) -> String {
    format!(
        "the node proposes host {} but connected from {}; confirm that other nodes reach it at the proposed host",
        review.host,
        review.peer_address.ip()
    )
}

/// Returns the site's manager node, which a validated site always has.
fn declared_manager(site: &Site) -> anyhow::Result<&SiteNode> {
    site.nodes
        .iter()
        .find(|node| node.site_manager.is_some())
        .context("site.yaml declares no site manager")
}

/// Chooses the manager's private port, which joined nodes dial on the manager host they reach
/// it at, and the address the join listener binds. The listener may bind any local address, such
/// as the private listener's own, and defaults to the declared manager host.
fn resolve_endpoints(
    manager_host: &str,
    private_port: Option<u16>,
    listen: Option<BindAddress>,
    private_addr: Option<&str>,
) -> anyhow::Result<(u16, BindAddress)> {
    // Parsed as the server parses it, so `node add` accepts exactly the values the manager starts with.
    let private_listener = ManagerPrivateListener::from_value(private_addr)?;
    let private_port = private_port.unwrap_or_else(|| private_listener.port());
    let listen = match (listen, private_listener) {
        (Some(listen), _) => listen,
        (None, ManagerPrivateListener::Configured(address)) => address,
        (None, ManagerPrivateListener::SiteDefault { .. }) => {
            let host = SiteNodeHost::parse(manager_host).with_context(|| {
                format!("the declared manager host {manager_host} is not valid; pass --listen")
            })?;
            BindAddress::new(host, private_port)
        }
    };
    Ok((private_port, listen))
}

/// Builds the endpoint the token names: the declared manager host, which the joining node can
/// reach and the manager certificate covers, with the join listener's actual port.
fn advertised_endpoint(port: u16, manager_host: &str) -> anyhow::Result<Url> {
    Url::parse(&format!("https://{manager_host}:{port}")).context("build the join endpoint")
}

/// Returns the command that joins a machine with the token as a flag.
fn join_command_with_token(token: &str) -> String {
    format!("dotc node join --token {token}")
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
    use std::{net::SocketAddr, os::unix::fs::PermissionsExt};

    use dotcore_domain::ExecutionPlatform;
    use dotcore_proto::dotcore::node::v1::NodeObservationConfiguration;
    use dotcore_site::{
        initial::{
            inputs::{ApplicationTargetChoice, ModelTargetChoice, NodeTargetChoices},
            request::InitialApplicationRuntime,
        },
        membership::join::{JoinHostSource, JoinProposal},
        node::folder::CertificateFingerprint,
    };

    use super::*;
    use crate::{
        hosts::test_support::FakeHostEnvironment,
        terminal::{ScriptedAnswer, ScriptedPrompt, ScriptedTerminal},
    };

    #[test]
    fn join_peers_from_this_machine_are_loopback_and_its_interface_addresses() {
        let environment = FakeHostEnvironment::on_tailnet();

        let peers = local_join_peers(&environment).expect("local peers");

        for local in ["127.0.0.1", "127.0.0.2", "100.101.102.103", "192.168.1.20"] {
            assert!(peers.contains(local.parse().expect("address")), "{local}");
        }
        for remote in ["192.0.2.7", "100.101.102.104"] {
            assert!(
                !peers.contains(remote.parse().expect("address")),
                "{remote}"
            );
        }
    }

    #[test]
    fn private_address_values_are_accepted_exactly_as_the_server_accepts_them() {
        for value in [
            "0.0.0.0:6000",
            "10.0.0.77:6000",
            "localhost:6000",
            "10.0.0.77",
            "::1:6000",
        ] {
            assert_eq!(
                resolve_endpoints("10.0.0.76", None, None, Some(value)).is_ok(),
                ManagerPrivateListener::from_value(Some(value)).is_ok(),
                "{value}"
            );
        }
    }

    #[test]
    fn the_private_port_comes_from_the_flag_then_the_environment_then_the_default() {
        // The private listener binds a local address the far side cannot use as the manager host.
        for bind in ["10.0.0.77:6000", "0.0.0.0:6000"] {
            let (port, listen) =
                resolve_endpoints("10.0.0.76", None, None, Some(bind)).expect("environment");
            assert_eq!(port, 6000);
            assert_eq!(listen, bind.parse().unwrap());
        }

        let (port, listen) = resolve_endpoints("10.0.0.76", None, None, None).expect("default");
        assert_eq!(port, 50053);
        assert_eq!(listen, "10.0.0.76:50053".parse().unwrap());

        let (port, listen) =
            resolve_endpoints("manager.example.ts.net", None, None, None).expect("named default");
        assert_eq!(port, 50053);
        assert_eq!(listen.to_string(), "manager.example.ts.net:50053");

        let (_, listen) = resolve_endpoints("10.0.0.76", None, None, Some("localhost:6000"))
            .expect("named environment");
        assert_eq!(listen.to_string(), "localhost:6000");

        let (port, listen) = resolve_endpoints(
            "10.0.0.76",
            Some(7000),
            Some("0.0.0.0:7100".parse().unwrap()),
            Some("10.0.0.77:1"),
        )
        .expect("flags win");
        assert_eq!(port, 7000);
        assert_eq!(listen, "0.0.0.0:7100".parse().unwrap());

        assert!(resolve_endpoints("10.0.0.76", None, None, Some("not-an-address")).is_err());
    }

    #[test]
    fn an_interactive_announcement_shows_the_command_once_and_writes_no_records() {
        let mut terminal = ScriptedTerminal::interactive([]);

        let records = announce_ready(
            Some(&mut terminal),
            "dotc node join --token dcjoin1.example",
        )
        .expect("announce");

        assert_eq!(records, None);
        assert_eq!(
            terminal.lines(),
            [
                "On the new machine, run:",
                "  dotc node join --token dcjoin1.example",
                "Waiting for the node...",
            ]
        );
    }

    #[test]
    fn an_unattended_announcement_is_the_command_record_only() {
        let records =
            announce_ready(None, "dotc node join --token dcjoin1.example").expect("announce");

        assert_eq!(
            records.as_deref(),
            Some("Node join ready\nDOTCORE_JOIN_COMMAND=dotc node join --token dcjoin1.example\n")
        );
    }

    #[test]
    fn the_added_node_is_one_line_interactively_and_records_otherwise() {
        let host = SiteNodeHost::parse("10.0.0.7").unwrap();
        let mut terminal = ScriptedTerminal::interactive([]);

        let joined = JoinedNode {
            node_id: "node-7".to_owned(),
            display_name: "gpu-box".to_owned(),
            host,
            manager_host: None,
            certificate_sha256: CertificateFingerprint::from_bytes([3; 32]),
            rejoined: false,
        };

        assert_eq!(report_added(Some(&mut terminal), &joined).unwrap(), None);
        assert_eq!(terminal.lines(), ["✔ Added gpu-box at 10.0.0.7"]);
        assert_eq!(
            report_added(None, &joined).unwrap().as_deref(),
            Some(
                "Node added\nDOTCORE_NODE_ID=node-7\nDOTCORE_NODE_NAME=gpu-box\nDOTCORE_NODE_HOST=10.0.0.7\n"
            )
        );
    }

    #[test]
    fn the_join_command_passes_the_token_as_a_flag() {
        assert_eq!(
            join_command_with_token("dcjoin1.example"),
            "dotc node join --token dcjoin1.example"
        );
    }

    #[test]
    fn private_port_notice_appears_only_for_a_non_default_port() {
        assert_eq!(private_port_notice(50053, "10.0.0.76"), None);
        assert_eq!(
            private_port_notice(61053, "10.0.0.76").as_deref(),
            Some(
                "nodes will reach the manager on port 61053; start the manager with \
                 DOTCORE_PRIVATE_ADDR=10.0.0.76:61053 (or dotcore --private-addr 10.0.0.76:61053)"
            )
        );
    }

    #[test]
    fn loopback_manager_notice_appears_only_for_a_loopback_manager_host() {
        assert_eq!(
            loopback_manager_notice("127.0.0.1", 50053).as_deref(),
            Some(
                "the manager host 127.0.0.1 is a loopback address, which only this machine \
                 reaches; a remote node must join with --manager-host <address it reaches this \
                 machine at>, and this command and the manager need \
                 DOTCORE_PRIVATE_ADDR=<that address>:50053 (or dotcore --private-addr <that \
                 address>:50053); for the node's apps to reach inference, the manager also \
                 needs DOTCORE_INFERENCE_ADDR=<that address>:50052"
            )
        );
        assert_eq!(loopback_manager_notice("gpu1.example.ts.net", 50053), None);
        assert_eq!(loopback_manager_notice("100.101.102.103", 50053), None);
    }

    #[test]
    fn the_token_advertises_the_declared_host_with_the_actual_port() {
        let endpoint = advertised_endpoint(7000, "10.0.0.76").expect("endpoint");
        assert_eq!(endpoint.as_str(), "https://10.0.0.76:7000/");
    }

    #[test]
    fn the_token_file_is_private_is_never_replaced_and_is_removed_on_drop() {
        let directory = crate::test_support::canonical_tempdir().expect("directory");
        let path = directory.path().join("join.token");

        let file = TokenFile::create(&path, "dcjoin1.example").expect("token file");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "dcjoin1.example\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(TokenFile::create(&path, "other").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "dcjoin1.example\n");

        drop(file);
        assert!(!path.exists());
    }

    #[test]
    fn the_operator_sees_the_proposal_and_decides() {
        for (answer, expected) in [(true, JoinDecision::Allow), (false, JoinDecision::Reject)] {
            let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(answer)]);

            let decision =
                decide_with_terminal(&review("10.0.0.7"), &mut terminal).expect("decide");

            assert_eq!(decision, expected);
            assert_eq!(
                terminal.lines(),
                [
                    "A node asks to join".to_owned(),
                    "  name             gpu-box".to_owned(),
                    "  host             10.0.0.7 (proposed by the node)".to_owned(),
                    "  models           simulated (GPU-00000000-0000-0000-0000-00000000gb10, \
                     linux/arm64), nvidia (nvidia.com/gpu=gpu0, linux/arm64)"
                        .to_owned(),
                    "  applications     docker (linux/arm64)".to_owned(),
                    "  connected from   10.0.0.7:41000".to_owned(),
                    format!("  key fingerprint  {}", "ab".repeat(32)),
                ]
            );
            assert!(matches!(
                terminal.prompts(),
                [ScriptedPrompt::Confirm(prompt)]
                    if prompt.title() == "Allow this node to join?" && !prompt.default()
            ));
        }
    }

    #[test]
    fn a_proposed_host_that_differs_from_the_peer_is_flagged() {
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(false)]);

        decide_with_terminal(&review("10.0.0.99"), &mut terminal).expect("decide");

        assert_eq!(
            terminal.lines().last().map(String::as_str),
            Some("! Proposed host 10.0.0.99 differs from where the node connected from (10.0.0.7)")
        );
    }

    #[test]
    fn a_rejoin_review_shows_each_address_change_and_the_deployments_that_stay() {
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(true)]);

        let decision = decide_with_terminal(
            &rejoin_review("10.0.0.7", "", "10.0.0.9", "10.0.0.1", 2),
            &mut terminal,
        )
        .expect("decide");

        assert_eq!(decision, JoinDecision::Allow);
        assert_eq!(
            terminal.lines(),
            [
                "A node asks to join again".to_owned(),
                "  name             gpu-box".to_owned(),
                "  host             10.0.0.7 -> 10.0.0.9".to_owned(),
                "  manager host     default -> 10.0.0.1".to_owned(),
                "  deployments      2 stay on this node".to_owned(),
                "  connected from   10.0.0.7:41000".to_owned(),
                format!("  key fingerprint  {}", "ab".repeat(32)),
                "! Proposed host 10.0.0.9 differs from where the node connected from (10.0.0.7)"
                    .to_owned(),
            ]
        );
        assert!(matches!(
            terminal.prompts(),
            [ScriptedPrompt::Confirm(prompt)]
                if prompt.title() == "Allow this node to join again?" && !prompt.default()
        ));
    }

    #[test]
    fn a_rejoin_review_without_changes_says_so_and_can_be_declined() {
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(false)]);

        let decision = decide_with_terminal(
            &rejoin_review("10.0.0.7", "10.0.0.1", "10.0.0.7", "10.0.0.1", 0),
            &mut terminal,
        )
        .expect("decide");

        assert_eq!(decision, JoinDecision::Reject);
        assert_eq!(
            terminal.lines(),
            [
                "A node asks to join again".to_owned(),
                "  name             gpu-box".to_owned(),
                "  host             10.0.0.7 (no change)".to_owned(),
                "  manager host     10.0.0.1 (no change)".to_owned(),
                "  deployments      none placed on this node".to_owned(),
                "  connected from   10.0.0.7:41000".to_owned(),
                format!("  key fingerprint  {}", "ab".repeat(32)),
            ]
        );
    }

    #[test]
    fn a_rejoin_review_shows_an_observation_refresh_that_changes() {
        let mut review = rejoin_review("10.0.0.7", "10.0.0.1", "10.0.0.7", "10.0.0.1", 0);
        let ReviewedJoin::Rejoin(rejoin) = &mut review.request else {
            unreachable!("a rejoin review")
        };
        rejoin.observation = observation(60);
        assert!(!rejoin.changes_nothing());
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(true)]);

        decide_with_terminal(&review, &mut terminal).expect("decide");

        assert_eq!(
            terminal.lines(),
            [
                "A node asks to join again".to_owned(),
                "  name             gpu-box".to_owned(),
                "  host             10.0.0.7 (no change)".to_owned(),
                "  manager host     10.0.0.1 (no change)".to_owned(),
                "  observation      refresh 30s -> 60s".to_owned(),
                "  deployments      none placed on this node".to_owned(),
                "  connected from   10.0.0.7:41000".to_owned(),
                format!("  key fingerprint  {}", "ab".repeat(32)),
            ]
        );

        let ReviewedJoin::Rejoin(rejoin) = &mut review.request else {
            unreachable!("a rejoin review")
        };
        rejoin.observation = NodeObservationConfiguration {
            schema_version: 2,
            ..observation(30)
        };
        assert_eq!(observation_change(rejoin).as_deref(), Some("updated"));
    }

    #[test]
    fn a_rejoined_node_is_reported_as_rejoined() {
        let joined = JoinedNode {
            node_id: "node-7".to_owned(),
            display_name: "gpu-box".to_owned(),
            host: SiteNodeHost::parse("10.0.0.9").unwrap(),
            manager_host: None,
            certificate_sha256: CertificateFingerprint::from_bytes([3; 32]),
            rejoined: true,
        };
        let mut terminal = ScriptedTerminal::interactive([]);

        assert_eq!(report_added(Some(&mut terminal), &joined).unwrap(), None);
        assert_eq!(terminal.lines(), ["✔ Rejoined gpu-box at 10.0.0.9"]);
        assert_eq!(
            report_added(None, &joined).unwrap().as_deref(),
            Some(
                "Node rejoined\nDOTCORE_NODE_ID=node-7\nDOTCORE_NODE_NAME=gpu-box\nDOTCORE_NODE_HOST=10.0.0.9\n"
            )
        );
    }

    fn rejoin_review(
        old_host: &str,
        old_manager_host: &str,
        new_host: &str,
        new_manager_host: &str,
        deployments: usize,
    ) -> JoinReview {
        let node = |host: &str, manager_host: &str| SiteNode {
            display_name: "gpu-box".to_owned(),
            host: host.to_owned(),
            manager_host: manager_host.to_owned(),
            ..SiteNode::default()
        };
        JoinReview {
            request: ReviewedJoin::Rejoin(NodeRejoin {
                declared: node(old_host, old_manager_host),
                rejoined: node(new_host, new_manager_host),
                installed_observation: observation(30),
                observation: observation(30),
                private_port: 50053,
                deployments,
            }),
            node_id: "node-7".to_owned(),
            host: new_host.parse().unwrap(),
            host_source: JoinHostSource::Proposed,
            host_differs_from_peer: new_host != "10.0.0.7",
            manager_host: SiteNodeHost::parse(new_manager_host).unwrap(),
            peer_address: "10.0.0.7:41000".parse::<SocketAddr>().unwrap(),
            csr_key_fingerprint: CertificateFingerprint::from_bytes([0xab; 32]),
        }
    }

    fn observation(interval_seconds: u32) -> NodeObservationConfiguration {
        NodeObservationConfiguration {
            schema_version: 1,
            interval_seconds,
            execution_targets: Vec::new(),
        }
    }

    fn review(host: &str) -> JoinReview {
        JoinReview {
            request: ReviewedJoin::New(JoinProposal {
                display_name: "gpu-box".to_owned(),
                host: Some(host.parse().unwrap()),
                private_port: 50053,
                targets: NodeTargetChoices {
                    model_targets: vec![
                        ModelTargetChoice::Simulated {
                            platform: ExecutionPlatform::linux_arm64(),
                            device_id: "GPU-00000000-0000-0000-0000-00000000gb10".to_owned(),
                        },
                        ModelTargetChoice::Nvidia {
                            selector: "nvidia.com/gpu=gpu0".to_owned(),
                            platform: ExecutionPlatform::linux_arm64(),
                        },
                    ],
                    application_targets: vec![ApplicationTargetChoice {
                        applications: InitialApplicationRuntime::Docker,
                        platform: ExecutionPlatform::linux_arm64(),
                        accelerator_device_ids: Vec::new(),
                    }],
                    accelerator_facts: Vec::new(),
                    declared_machine: None,
                },
            }),
            node_id: "node-0000000001".to_owned(),
            host: host.parse().unwrap(),
            host_source: JoinHostSource::Proposed,
            host_differs_from_peer: host != "10.0.0.7",
            manager_host: SiteNodeHost::parse("10.0.0.1").unwrap(),
            peer_address: "10.0.0.7:41000".parse::<SocketAddr>().unwrap(),
            csr_key_fingerprint: CertificateFingerprint::from_bytes([0xab; 32]),
        }
    }
}
