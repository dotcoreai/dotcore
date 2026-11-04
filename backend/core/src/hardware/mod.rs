//! The owner-neutral hardware substrate: target capability and hardware class selection, the
//! capacity component's memory pools and fit evaluation, and the accelerator memory attribution
//! both workload controllers share.
//!
//! Callers read targets and attributed memory figures through this module, so they mean the same
//! thing in every lane. It owns no product state and never reads either controller's records:
//! callers pass the facts they own.

pub(crate) mod attribution;
pub(crate) mod capacity;
pub(crate) mod selection;
pub(crate) mod workloads;

/// Whether a workload's execution target hands it accelerator devices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AcceleratorPresence {
    /// The target assigns no accelerator device, so the workload runs on the host only.
    None,

    /// The target assigns at least one accelerator device to the workload.
    Devices,
}

/// Returns whether the accelerator device ids a target or a retained deployment snapshot assigns
/// give the workload any accelerator.
pub(crate) fn accelerator_presence(accelerator_device_ids: &[String]) -> AcceleratorPresence {
    if accelerator_device_ids.is_empty() {
        AcceleratorPresence::None
    } else {
        AcceleratorPresence::Devices
    }
}

#[cfg(test)]
mod tests {
    use super::{AcceleratorPresence, accelerator_presence};

    #[test]
    fn hardware_sources_have_no_controller_vocabulary() {
        for (name, source) in [
            ("mod.rs", include_str!("mod.rs")),
            ("selection.rs", include_str!("selection.rs")),
            ("attribution.rs", include_str!("attribution.rs")),
            ("capacity/mod.rs", include_str!("capacity/mod.rs")),
            (
                "capacity/evaluation.rs",
                include_str!("capacity/evaluation.rs"),
            ),
            ("capacity/node.rs", include_str!("capacity/node.rs")),
            ("workloads.rs", include_str!("workloads.rs")),
        ] {
            let production = source
                .split("#[cfg(test)]")
                .next()
                .expect("production source");
            for forbidden in [
                ["crate::", "model"].concat(),
                ["crate::", "application"].concat(),
                ["model::", "v1"].concat(),
                ["application::", "v1"].concat(),
                ["Model", "Deployment"].concat(),
                ["Application", "Deployment"].concat(),
            ] {
                assert!(
                    !production.contains(&forbidden),
                    "{name} contains {forbidden}"
                );
            }
        }
    }

    #[test]
    fn accelerator_presence_follows_the_assigned_devices() {
        assert_eq!(accelerator_presence(&[]), AcceleratorPresence::None);
        assert_eq!(
            accelerator_presence(&["nvidia.com/gpu=0".to_owned()]),
            AcceleratorPresence::Devices
        );
    }
}
