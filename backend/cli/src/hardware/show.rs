//! `dotc hardware show`: prints this machine's hardware as a machine description.
//!
//! The output is a YAML document that loads as a declared machine, preceded by comment lines
//! naming the Dotcore version and the hardware class of each model execution target. It carries
//! nothing that identifies the machine, so it is safe to share in a bug report.

use std::fmt::Write as _;

use anyhow::{Context, Result};
use dotcore::model::IdleMachine;
use dotcore_catalog::Catalog;
use dotcore_node_hardware::MachineDescription;
use dotcore_proto::dotcore::site::v1::execution_target::RuntimeBinding;

use super::load_catalog;
use crate::{RootPathArgs, config::environment_value};

/// Prints the description of this machine on stdout.
pub(super) fn show(paths: &RootPathArgs) -> Result<()> {
    let machine = MachineDescription::from_local_host()
        .context("cannot describe the hardware of this machine")?;
    let catalog = paths
        .catalog_dir()?
        .map(|dir| load_catalog(&dir))
        .transpose()?;
    let revision = packaged_revision()?;
    print!(
        "{}",
        render(&machine, catalog.as_ref(), revision.as_deref())?
    );
    Ok(())
}

/// Renders the report: comment lines, then the description. Without a catalog the classes cannot
/// be named, which the report says.
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

    match catalog {
        Some(catalog) => {
            let idle =
                IdleMachine::new(machine).context("cannot work out the execution targets")?;
            for (target, class) in idle.hardware_classes(catalog) {
                if !matches!(
                    target.runtime_binding,
                    Some(RuntimeBinding::ModelRuntime(_))
                ) {
                    continue;
                }
                let class = class.map_or("none".to_owned(), |class| class.hardware_class_id);
                writeln!(
                    report,
                    "# Hardware class of {}: {class}",
                    target.display_name
                )?;
            }
        }
        None => report.push_str("# Hardware classes: unknown, no master catalog found\n"),
    }

    report.push_str(&machine.to_yaml()?);
    Ok(report)
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

    use super::render;
    use crate::hardware::load_catalog;

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
