//! What init, reset, delete and validation report on standard output.
//!
//! A person gets a one-line outcome followed by the site's facts as plain text on stdout, even
//! after a guided conversation on the terminal. Asked for YAML, the same facts are one document.
//! When no guided summary showed the node host and the operator did not name it, standard error
//! names it and where it came from.

use std::{fmt, path::Path};

use anyhow::Context;
use dotcore_proto::dotcore::site::v1::SiteNode;
use dotcore_site::initial::{
    input_store::InitialSiteWriteOutcome,
    inputs::InitialSiteInputs,
    request::{InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteRequest},
};
use serde::Serialize;

use crate::{
    hosts::{HostSource, SelectedHost},
    output::{OutputFormat, write_plain, write_yaml},
    terminal::{Message, notify_stderr},
};

/// How a site command left the site.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SiteOutcome {
    Initialized,

    AlreadyInitialized,

    Reset,
}

impl SiteOutcome {
    /// The outcome line a person reads.
    fn line(self) -> &'static str {
        match self {
            Self::Initialized => "Site initialized",
            Self::AlreadyInitialized => "Site already initialized",
            Self::Reset => "Site reset",
        }
    }

    /// The outcome word of the YAML document.
    fn key(self) -> &'static str {
        match self {
            Self::Initialized => "initialized",
            Self::AlreadyInitialized => "alreadyInitialized",
            Self::Reset => "reset",
        }
    }
}

/// A site that init or reset wrote or found, ready to report once the command is done.
pub(super) struct SiteReport {
    outcome: SiteOutcome,
    site_dir: std::path::PathBuf,
    inputs: InitialSiteInputs,
    request: InitialSiteRequest,

    /// The selected host when nothing showed it yet, so the report names it.
    unreported_host: Option<SelectedHost>,
}

impl fmt::Debug for SiteReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SiteReport")
            .field("outcome", &self.outcome)
            .field("site_dir", &self.site_dir)
            .finish_non_exhaustive()
    }
}

impl SiteReport {
    /// Reports the outcome of a write: the site it published, or the matching site it kept.
    pub(super) fn written(
        outcome: InitialSiteWriteOutcome,
        site_dir: &Path,
        published: InitialSiteInputs,
        request: InitialSiteRequest,
        unreported_host: Option<SelectedHost>,
    ) -> Self {
        let (outcome, inputs) = match outcome {
            InitialSiteWriteOutcome::Published => (SiteOutcome::Initialized, published),
            InitialSiteWriteOutcome::AlreadyInitialized(existing) => {
                (SiteOutcome::AlreadyInitialized, existing)
            }
        };
        Self {
            outcome,
            site_dir: site_dir.to_owned(),
            inputs,
            request,
            unreported_host,
        }
    }

    /// Reports a site that a reset replaced.
    pub(super) fn reset(
        site_dir: &Path,
        inputs: InitialSiteInputs,
        request: InitialSiteRequest,
        unreported_host: Option<SelectedHost>,
    ) -> Self {
        Self {
            outcome: SiteOutcome::Reset,
            site_dir: site_dir.to_owned(),
            inputs,
            request,
            unreported_host,
        }
    }

    /// Prints the result on stdout, then names the node host on stderr when nothing showed it.
    pub(super) fn print(&self, format: OutputFormat) -> anyhow::Result<()> {
        match format {
            OutputFormat::Yaml => write_yaml(&SiteDocument::of(self)?)?,
            OutputFormat::Human => write_plain(&site_messages(
                self.outcome.line(),
                &self.site_dir,
                &self.inputs,
                &self.request,
            )?)?,
        }
        match (self.outcome, &self.unreported_host) {
            (SiteOutcome::Initialized | SiteOutcome::Reset, Some(host)) => report_host(host),
            (SiteOutcome::AlreadyInitialized, Some(host)) if host.source() != &HostSource::Flag => {
                notify_node_host(&format!(
                    "Node host: {} (already recorded)",
                    local_node(&self.inputs)?.host
                ));
            }
            _ => {}
        }
        Ok(())
    }
}

/// The YAML document of a site command: where the site is and, once initialized or reset, what it
/// is.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SiteDocument {
    outcome: &'static str,
    site_dir: String,

    #[serde(flatten)]
    details: Option<SiteDetails>,
}

/// The identity and capabilities of a written site.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SiteDetails {
    site_id: String,
    node_id: String,
    node_host: String,
    inference: Vec<&'static str>,
    applications: Vec<&'static str>,
}

impl SiteDocument {
    fn of(report: &SiteReport) -> anyhow::Result<Self> {
        let node = local_node(&report.inputs)?;
        Ok(Self {
            outcome: report.outcome.key(),
            site_dir: path_text(&report.site_dir)?.to_owned(),
            details: Some(SiteDetails {
                site_id: report.inputs.site().site_id.clone(),
                node_id: node.node_id.clone(),
                node_host: node.host.clone(),
                inference: report
                    .request
                    .inference()
                    .iter()
                    .map(|inference| inference_name(*inference))
                    .collect(),
                applications: report
                    .request
                    .applications()
                    .iter()
                    .map(|applications| applications_name(*applications))
                    .collect(),
            }),
        })
    }

    /// The document of a command that only says what it did to the site folder.
    pub(super) fn at(outcome: &'static str, site_dir: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            outcome,
            site_dir: path_text(site_dir)?.to_owned(),
            details: None,
        })
    }
}

/// Names the selected node host and where it came from on stderr, unless the operator named it.
pub(super) fn report_host(host: &SelectedHost) {
    if host.source() != &HostSource::Flag {
        notify_node_host(&format!("Node host: {} ({})", host.host(), host.source()));
    }
}

pub(super) fn write_validate_success(site_dir: &Path, format: OutputFormat) -> anyhow::Result<()> {
    if format == OutputFormat::Yaml {
        return write_yaml(&SiteDocument::at("valid", site_dir)?);
    }
    write_directory_outcome("Site valid", site_dir)
}

/// Reports a site command that only says what it did to the site folder, such as a deletion.
pub(super) fn write_directory_outcome(outcome_line: &str, site_dir: &Path) -> anyhow::Result<()> {
    write_plain(&[
        Message::Success(outcome_line.to_owned()),
        Message::KeyValues(vec![(
            "directory".to_owned(),
            path_text(site_dir)?.to_owned(),
        )]),
    ])
}

/// The outcome line of a site command and the facts of the site it left.
pub(super) fn site_messages(
    outcome_line: &str,
    site_dir: &Path,
    inputs: &InitialSiteInputs,
    request: &InitialSiteRequest,
) -> anyhow::Result<Vec<Message>> {
    let inference = capability_list(request.inference().iter().map(|&i| inference_name(i)));
    let applications =
        capability_list(request.applications().iter().map(|&a| applications_name(a)));
    let node = local_node(inputs)?;
    Ok(vec![
        Message::Success(outcome_line.to_owned()),
        Message::KeyValues(vec![
            ("directory".to_owned(), path_text(site_dir)?.to_owned()),
            ("site id".to_owned(), inputs.site().site_id.clone()),
            ("node id".to_owned(), node.node_id.clone()),
            ("node host".to_owned(), node.host.clone()),
            ("inference".to_owned(), inference),
            ("applications".to_owned(), applications),
        ]),
    ])
}

/// Returns the declaration of the node these inputs are bound to.
pub(super) fn local_node(inputs: &InitialSiteInputs) -> anyhow::Result<&SiteNode> {
    inputs
        .site()
        .nodes
        .iter()
        .find(|node| node.node_id == inputs.node().node_id)
        .context("validated site inputs lack the local node binding")
}

fn notify_node_host(host_line: &str) {
    notify_stderr(&[Message::Info(host_line.to_owned())], host_line);
}

fn inference_name(inference: InitialInferenceRuntime) -> &'static str {
    match inference {
        InitialInferenceRuntime::Simulated => "simulated",
        InitialInferenceRuntime::Nvidia => "nvidia",
        InitialInferenceRuntime::Apple => "apple",
    }
}

fn applications_name(applications: InitialApplicationRuntime) -> &'static str {
    match applications {
        InitialApplicationRuntime::Simulated => "simulated",
        InitialApplicationRuntime::Docker => "docker",
    }
}

/// The path as text, which a result can print only when it is UTF-8 without line breaks.
fn path_text(path: &Path) -> anyhow::Result<&str> {
    path.to_str()
        .filter(|value| !value.contains(['\r', '\n']))
        .context("Site output path must be UTF-8 without carriage returns or newlines")
}

/// Joins capability names with commas, such as `simulated,nvidia`, or `none` without any.
fn capability_list<'a>(capabilities: impl Iterator<Item = &'a str>) -> String {
    let capabilities = capabilities.collect::<Vec<_>>();
    if capabilities.is_empty() {
        "none".to_owned()
    } else {
        capabilities.join(",")
    }
}

#[cfg(test)]
mod tests {
    use dotcore_site::initial::probe::InitialSiteProber;

    use super::site_messages;
    use crate::output::plain_text;
    use crate::site::{
        ApplicationsChoice, InferenceChoice, prepare_initial_site,
        test_support::{
            FakeApple, FakeDocker, SiteRoot, UnavailableNvidia, flagged_request, init_arguments,
            lan_machine, named_identities,
        },
    };

    #[test]
    fn apple_inference_is_reported_in_the_site_messages() {
        let fixture = SiteRoot::new();
        let site_dir = fixture.root.join("site");
        let mut arguments = init_arguments(ApplicationsChoice::Simulated);
        arguments.configuration.inference = vec![InferenceChoice::Apple.into()];
        let prober = InitialSiteProber::new(Box::new(FakeDocker), Box::new(UnavailableNvidia))
            .with_apple_accelerator(Box::new(FakeApple));
        let request = flagged_request(&arguments.configuration);

        let prepared = crate::build_runtime()
            .expect("runtime")
            .block_on(prepare_initial_site(
                request,
                &prober,
                lan_machine(),
                &mut named_identities(None),
            ))
            .expect("prepare Apple site");

        let text = plain_text(
            &site_messages(
                "Site initialized",
                &site_dir,
                &prepared.inputs,
                &prepared.request,
            )
            .expect("messages"),
        );
        assert!(
            text.ends_with("  inference     apple\n  applications  simulated\n"),
            "{text}"
        );
    }

    #[test]
    fn every_capability_or_none_is_reported_in_the_site_messages() {
        let site_dir = SiteRoot::new().root.join("site");
        for (inference, applications, expected) in [
            (
                vec![InferenceChoice::Simulated],
                vec![ApplicationsChoice::Docker, ApplicationsChoice::Simulated],
                "  inference     simulated\n  applications  simulated,docker\n",
            ),
            (
                vec![InferenceChoice::None],
                vec![ApplicationsChoice::None],
                "  inference     none\n  applications  none\n",
            ),
        ] {
            let mut arguments = init_arguments(ApplicationsChoice::Simulated);
            arguments.configuration.inference = inference.into_iter().map(Into::into).collect();
            arguments.configuration.applications = applications;
            let prober = InitialSiteProber::new(Box::new(FakeDocker), Box::new(UnavailableNvidia))
                .with_apple_accelerator(Box::new(FakeApple));

            let prepared = crate::build_runtime()
                .expect("runtime")
                .block_on(prepare_initial_site(
                    flagged_request(&arguments.configuration),
                    &prober,
                    lan_machine(),
                    &mut named_identities(None),
                ))
                .expect("prepare site");

            let text = plain_text(
                &site_messages(
                    "Site initialized",
                    &site_dir,
                    &prepared.inputs,
                    &prepared.request,
                )
                .expect("messages"),
            );
            assert!(text.ends_with(expected), "{text}");
        }
    }
}
