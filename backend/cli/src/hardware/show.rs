//! `dotc hardware show`: prints this machine's hardware.
//!
//! A person gets a summary of the machine and the hardware class of each model execution target.
//! Asked for YAML, the output is a document that loads as a declared machine, preceded by comment
//! lines naming the Dotcore version and those classes. Neither carries anything that identifies
//! the machine, so both are safe to share in a bug report.

use std::fmt::Write as _;

use anyhow::{Context, Result};
use dotcore::model::IdleMachine;
use dotcore_catalog::Catalog;
use dotcore_domain::timing::Timing;
use dotcore_node_hardware::MachineDescription;
use dotcore_proto::dotcore::{
    node::v1::gpu_description::Memory, site::v1::execution_target::RuntimeBinding,
};

use super::load_catalog;
use crate::{
    RootPathArgs,
    config::environment_value,
    output::{OutputFormat, write_plain, write_text},
    terminal::Message,
};

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Prints the description of this machine on stdout: a summary for a person, or the description
/// as YAML.
pub(super) fn show(paths: &RootPathArgs, format: OutputFormat) -> Result<()> {
    let machine = MachineDescription::from_local_host()
        .context("cannot describe the hardware of this machine")?;
    let catalog = paths
        .catalog_dir()?
        .map(|dir| load_catalog(&dir))
        .transpose()?;
    let revision = packaged_revision()?;
    match format {
        OutputFormat::Yaml => write_text(&render(&machine, catalog.as_ref(), revision.as_deref())?),
        OutputFormat::Human => {
            let classes = hardware_classes(&machine, catalog.as_ref())?;
            write_plain(&summary(&machine, classes.as_deref(), revision.as_deref()))
        }
    }
}

/// Lays the machine out for a person: its facts, its GPUs, and the hardware class of each model
/// target, or a line saying the classes are unknown.
fn summary(
    machine: &MachineDescription,
    classes: Option<&[(String, String)]>,
    revision: Option<&str>,
) -> Vec<Message> {
    let stored = machine.to_stored();
    let mut dotcore = env!("CARGO_PKG_VERSION").to_owned();
    if let Some(revision) = revision {
        dotcore.push_str(&format!(" (revision {revision})"));
    }
    let mut messages = vec![
        Message::Heading("Hardware".to_owned()),
        Message::KeyValues(vec![
            ("dotcore".to_owned(), dotcore),
            ("platform".to_owned(), machine.platform().to_string()),
            ("cpus".to_owned(), stored.cpu_count.to_string()),
            (
                "host memory".to_owned(),
                format!(
                    "{} ({} in use when idle)",
                    gib(stored.host_memory_total_bytes),
                    gib(stored.host_memory_idle_used_bytes)
                ),
            ),
        ]),
    ];
    if !stored.gpus.is_empty() {
        messages.push(Message::Heading("GPUs".to_owned()));
        messages.push(Message::Table {
            header: ["MODEL", "VENDOR", "DRIVER", "MEMORY"]
                .map(str::to_owned)
                .to_vec(),
            rows: stored
                .gpus
                .iter()
                .map(|gpu| {
                    vec![
                        gpu.model.clone(),
                        format!("{:?}", gpu.vendor()).to_lowercase(),
                        gpu.driver_version.clone(),
                        match &gpu.memory {
                            Some(Memory::Dedicated(memory)) => gib(memory.total_bytes),
                            Some(Memory::Unified(_)) => "unified".to_owned(),
                            None => String::new(),
                        },
                    ]
                })
                .collect(),
        });
    }
    match classes {
        Some([]) => {}
        Some(classes) => {
            messages.push(Message::Heading("Hardware classes".to_owned()));
            messages.push(Message::KeyValues(classes.to_vec()));
        }
        None => messages.push(Message::Info(
            "Hardware classes are unknown: no master catalog found".to_owned(),
        )),
    }
    messages
}

/// Renders the YAML report: comment lines, then the description. Without a catalog the classes
/// cannot be named, which the report says.
fn render(
    machine: &MachineDescription,
    catalog: Option<&Catalog>,
    revision: Option<&str>,
) -> Result<String> {
    let mut report = format!("# Dotcore {}", env!("CARGO_PKG_VERSION"));
    if let Some(revision) = revision {
        write!(report, ", revision {revision}")?;
    }
    report.push('\n');

    match hardware_classes(machine, catalog)? {
        Some(classes) => {
            for (target, class) in classes {
                writeln!(report, "# Hardware class of {target}: {class}")?;
            }
        }
        None => report.push_str("# Hardware classes: unknown, no master catalog found\n"),
    }

    report.push_str(&machine.to_yaml()?);
    Ok(report)
}

/// The hardware class of each model execution target, named after the target, or `None` without
/// a catalog to name them from.
fn hardware_classes(
    machine: &MachineDescription,
    catalog: Option<&Catalog>,
) -> Result<Option<Vec<(String, String)>>> {
    let Some(catalog) = catalog else {
        return Ok(None);
    };
    let idle = IdleMachine::new(machine, Timing::for_process().node_control)
        .context("cannot work out the execution targets")?;
    Ok(Some(
        idle.hardware_classes(catalog)
            .into_iter()
            .filter(|(target, _)| {
                matches!(
                    target.runtime_binding,
                    Some(RuntimeBinding::ModelRuntime(_))
                )
            })
            .map(|(target, class)| {
                (
                    target.display_name.clone(),
                    class.map_or("none".to_owned(), |class| class.hardware_class_id),
                )
            })
            .collect(),
    ))
}

/// Formats a byte count in GiB with one decimal.
pub(crate) fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / GIB)
}

/// Returns the revision of the source a packaged Dotcore was built from, when it says so.
fn packaged_revision() -> Result<Option<String>> {
    Ok(
        environment_value(dotcore::composition::PACKAGED_SOURCE_FLAKE_ENV)?
            .and_then(|value| dotcore_domain::nix::LocalDotcoreSource::parse(value.trim()))
            .map(|source| source.revision().to_owned()),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use dotcore_node_hardware::MachineDescription;

    use super::{hardware_classes, render, summary};
    use crate::{hardware::load_catalog, output::plain_text};

    const MACHINE: &str = "platform: linux/amd64
cpuCount: 8
hostMemory: { totalBytes: 34359738368, idleUsedBytes: 4294967296 }
gpus:
  - id: GPU-fixture
    vendor: nvidia
    model: Fixture GPU X
    driverVersion: \"580.173.02\"
    memory: { architecture: dedicated, totalBytes: 34359738368, reservedBytes: 536870912 }
";

    const YAML: &str = "platform: linux/amd64
cpuCount: 8
hostMemory:
  totalBytes: 34359738368
  idleUsedBytes: 4294967296
gpus:
- id: GPU-fixture
  vendor: nvidia
  model: Fixture GPU X
  driverVersion: 580.173.02
  memory:
    architecture: dedicated
    totalBytes: 34359738368
    reservedBytes: 536870912
";

    #[test]
    fn report_names_the_most_specific_hardware_class_of_each_model_target() {
        let catalog = load_catalog(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/recommendation-catalog"),
        )
        .expect("fixture catalog");
        let machine = MachineDescription::from_yaml("fixture", MACHINE).expect("machine");

        assert_eq!(
            render(&machine, Some(&catalog), Some("0123456789abcdef")).expect("report"),
            format!(
                "# Dotcore {}, revision 0123456789abcdef\n# Hardware class of Fixture GPU X: nvidia-x\n{YAML}",
                env!("CARGO_PKG_VERSION")
            )
        );
    }

    #[test]
    fn summary_lists_the_machine_its_gpus_and_the_hardware_class_of_each_model_target() {
        let catalog = load_catalog(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/recommendation-catalog"),
        )
        .expect("fixture catalog");
        let machine = MachineDescription::from_yaml("fixture", MACHINE).expect("machine");
        let classes = hardware_classes(&machine, Some(&catalog)).expect("classes");

        assert_eq!(
            plain_text(&summary(
                &machine,
                classes.as_deref(),
                Some("0123456789abcdef")
            )),
            format!(
                "Hardware\n  dotcore      {} (revision 0123456789abcdef)\n  platform     linux/amd64\n  cpus         8\n  host memory  32.0 GiB (4.0 GiB in use when idle)\n\nGPUs\n  MODEL          VENDOR  DRIVER      MEMORY\n  Fixture GPU X  nvidia  580.173.02  32.0 GiB\n\nHardware classes\n  Fixture GPU X  nvidia-x\n",
                env!("CARGO_PKG_VERSION")
            )
        );
    }

    #[test]
    fn summary_without_a_catalog_says_the_classes_are_unknown() {
        let machine = MachineDescription::from_yaml("fixture", MACHINE).expect("machine");

        let text = plain_text(&summary(&machine, None, None));

        assert!(
            text.ends_with("32.0 GiB\nHardware classes are unknown: no master catalog found\n"),
            "{text}"
        );
    }

    #[test]
    fn whole_report_loads_back_as_the_declared_machine_it_describes() {
        let catalog = load_catalog(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/recommendation-catalog"),
        )
        .expect("fixture catalog");
        let machine = MachineDescription::from_yaml("fixture", MACHINE).expect("machine");
        let report = render(&machine, Some(&catalog), Some("0123456789abcdef")).expect("report");

        assert_eq!(
            MachineDescription::from_yaml("fixture", &report).expect("report as a machine"),
            machine
        );
    }

    #[test]
    fn report_without_a_catalog_still_prints_the_hardware() {
        let machine = MachineDescription::from_yaml("fixture", MACHINE).expect("machine");

        assert_eq!(
            render(&machine, None, None).expect("report"),
            format!(
                "# Dotcore {}\n# Hardware classes: unknown, no master catalog found\n{YAML}",
                env!("CARGO_PKG_VERSION")
            )
        );
    }
}
