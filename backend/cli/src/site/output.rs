//! The stable result records init, reset and validation print on standard output.
//!
//! Records are key-value lines after a one-line outcome, so scripts can read them; values with line
//! breaks are refused instead of printed. When no guided summary showed the node host and the
//! operator did not name it, standard error names it and where it came from.

use std::path::Path;

use anyhow::{Context, bail};
use dotcore_proto::dotcore::site::v1::SiteNode;
use dotcore_site::initial::{
    input_store::InitialSiteWriteOutcome,
    inputs::InitialSiteInputs,
    request::{InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteRequest},
};

use crate::{
    hosts::{HostSource, SelectedHost},
    terminal::{Message, notify_stderr},
};

pub(super) fn write_success(
    first_line: &str,
    site_dir: &Path,
    inputs: &InitialSiteInputs,
    request: &InitialSiteRequest,
) -> anyhow::Result<()> {
    let records = success_records(first_line, site_dir, inputs, request)?;
    write_stdout(records.as_bytes(), "write site initialization result")
}

/// Writes the outcome's records, then reports `unreported_host` when given and not named by the
/// operator: the selected host for a written site, the recorded one for a site that already
/// matched.
pub(super) fn write_transaction_success(
    outcome: InitialSiteWriteOutcome,
    site_dir: &Path,
    published: &InitialSiteInputs,
    request: &InitialSiteRequest,
    unreported_host: Option<&SelectedHost>,
) -> anyhow::Result<()> {
    match outcome {
        InitialSiteWriteOutcome::Published => {
            write_success("Site initialized", site_dir, published, request)?;
            if let Some(host) = unreported_host {
                report_host(host);
            }
        }
        InitialSiteWriteOutcome::AlreadyInitialized(existing) => {
            write_success("Site already initialized", site_dir, &existing, request)?;
            if unreported_host.is_some_and(|host| host.source() != &HostSource::Flag) {
                notify_node_host(&format!(
                    "Node host: {} (already recorded)",
                    local_node(&existing)?.host
                ));
            }
        }
    }
    Ok(())
}

/// Names the selected node host and where it came from on stderr, unless the operator named it.
pub(super) fn report_host(host: &SelectedHost) {
    if host.source() != &HostSource::Flag {
        notify_node_host(&format!("Node host: {} ({})", host.host(), host.source()));
    }
}

pub(super) fn write_validate_success(site_dir: &Path) -> anyhow::Result<()> {
    let site_dir = site_dir
        .to_str()
        .filter(|value| !value.contains(['\r', '\n']))
        .context(
            "Site validation output path must be UTF-8 without carriage returns or newlines",
        )?;
    let records = format!("Site valid\nDOTCORE_SITE_DIR={site_dir}\n");
    write_stdout(records.as_bytes(), "write site validation result")
}

/// Writes a command result record to process stdout.
///
/// Unit tests print through libtest's capture instead, since direct stdout writes bypass it.
fn write_stdout(bytes: &[u8], context: &'static str) -> anyhow::Result<()> {
    #[cfg(test)]
    {
        let _ = context;
        print!("{}", String::from_utf8_lossy(bytes));
        Ok(())
    }

    #[cfg(not(test))]
    {
        use std::io::Write as _;

        std::io::stdout().lock().write_all(bytes).context(context)
    }
}

pub(super) fn success_records(
    first_line: &str,
    site_dir: &Path,
    inputs: &InitialSiteInputs,
    request: &InitialSiteRequest,
) -> anyhow::Result<String> {
    let site_dir = site_dir
        .to_str()
        .filter(|value| !value.contains(['\r', '\n']))
        .context("Site output path must be UTF-8 without carriage returns or newlines")?;
    let inference = capability_list(request.inference().iter().map(|inference| match inference {
        InitialInferenceRuntime::Simulated => "simulated",
        InitialInferenceRuntime::Nvidia => "nvidia",
        InitialInferenceRuntime::Apple => "apple",
    }));
    let applications =
        capability_list(
            request
                .applications()
                .iter()
                .map(|applications| match applications {
                    InitialApplicationRuntime::Simulated => "simulated",
                    InitialApplicationRuntime::Docker => "docker",
                }),
        );
    let node = local_node(inputs)?;
    for value in [
        first_line,
        inputs.site().site_id.as_str(),
        node.node_id.as_str(),
        node.host.as_str(),
    ] {
        if value.contains(['\r', '\n']) {
            bail!("Site initialization output value must not contain carriage returns or newlines");
        }
    }
    Ok(format!(
        "{first_line}\nDOTCORE_SITE_DIR={site_dir}\nDOTCORE_SITE_ID={}\nDOTCORE_NODE_ID={}\nDOTCORE_NODE_HOST={}\nDOTCORE_INFERENCE={inference}\nDOTCORE_APPLICATIONS={applications}\n",
        inputs.site().site_id,
        node.node_id,
        node.host,
    ))
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

    use super::success_records;
    use crate::site::{
        ApplicationsChoice, InferenceChoice, prepare_initial_site,
        test_support::{
            FakeApple, FakeDocker, SiteRoot, UnavailableNvidia, flagged_request, init_arguments,
            lan_machine, named_identities,
        },
    };

    #[test]
    fn apple_inference_is_reported_in_the_success_records() {
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

        assert!(
            success_records(
                "Site initialized",
                &site_dir,
                &prepared.inputs,
                &prepared.request
            )
            .expect("records")
            .contains("DOTCORE_INFERENCE=apple\nDOTCORE_APPLICATIONS=simulated\n")
        );
    }

    #[test]
    fn every_capability_or_none_is_reported_in_the_success_records() {
        let site_dir = SiteRoot::new().root.join("site");
        for (inference, applications, expected) in [
            (
                vec![InferenceChoice::Simulated],
                vec![ApplicationsChoice::Docker, ApplicationsChoice::Simulated],
                "DOTCORE_INFERENCE=simulated\nDOTCORE_APPLICATIONS=simulated,docker\n",
            ),
            (
                vec![InferenceChoice::None],
                vec![ApplicationsChoice::None],
                "DOTCORE_INFERENCE=none\nDOTCORE_APPLICATIONS=none\n",
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

            let records = success_records(
                "Site initialized",
                &site_dir,
                &prepared.inputs,
                &prepared.request,
            )
            .expect("records");
            assert!(records.ends_with(expected), "{records}");
        }
    }
}
