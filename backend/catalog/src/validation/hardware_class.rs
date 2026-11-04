//! Validates one shared hardware class declaration: its identity, display name and selector.
//!
//! A hardware class names the accelerator vendors it covers and may narrow them to one platform.
//! Driver minimums belong to engine artifacts, so a class selector never sets one, and every
//! problem is reported rather than only the first.

use std::collections::BTreeSet;

use dotcore_domain::{Accelerator, ExecutionPlatform, valid_id};
use dotcore_proto::dotcore::catalog::v1::HardwareClass;

use super::common::valid_selector_memory_architecture;

pub fn validate_hardware_class(hardware_class: &HardwareClass) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    let id = &hardware_class.hardware_class_id;
    if !valid_id(id) || hardware_class.display_name.is_empty() {
        errors.push(anyhow::anyhow!(
            "hardware class {id:?} requires a valid identity and display name"
        ));
    }
    let Some(selector) = hardware_class.selector.as_ref() else {
        errors.push(anyhow::anyhow!("hardware class {id:?} requires a selector"));
        return errors;
    };
    if let Some(platform) = selector.platform.as_ref()
        && ExecutionPlatform::try_from(platform).is_err()
    {
        errors.push(anyhow::anyhow!(
            "hardware class {id:?} selector names an unsupported platform"
        ));
    }
    let mut vendors = BTreeSet::new();
    if selector.accelerators.is_empty()
        || selector.accelerators.iter().any(|name| {
            Accelerator::from_catalog_name(name).is_none() || !vendors.insert(name.as_str())
        })
    {
        errors.push(anyhow::anyhow!(
            "hardware class {id:?} selector requires distinct accelerators, each nvidia or apple"
        ));
    }
    if !valid_selector_memory_architecture(selector) {
        errors.push(anyhow::anyhow!(
            "hardware class {id:?} selector memory architecture must be unified, dedicated or unspecified"
        ));
    }
    if selector.minimum_nvidia_driver_major_version != 0 {
        errors.push(anyhow::anyhow!(
            "hardware class {id:?} selector takes no minimum NVIDIA driver major version"
        ));
    }
    errors
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::{
        catalog::v1::{HardwareClass, OciPlatform, Selector},
        observation::v1::AcceleratorMemoryArchitecture,
    };

    use super::validate_hardware_class;

    #[test]
    fn a_vendor_class_with_an_optional_platform_is_valid() {
        let mut darwin = hardware_class("apple", &["apple"]);
        darwin.selector.as_mut().expect("selector").platform = Some(OciPlatform {
            operating_system: "darwin".to_owned(),
            architecture: "arm64".to_owned(),
            variant: None,
        });

        assert!(errors(&hardware_class("nvidia", &["nvidia"])).is_empty());
        assert!(errors(&darwin).is_empty());
    }

    #[test]
    fn invalid_identity_selector_vendors_and_driver_minimum_are_each_reported() {
        let mut invalid = hardware_class("Not Valid", &["nvidia", "nvidia", "amd"]);
        invalid.display_name.clear();
        invalid
            .selector
            .as_mut()
            .expect("selector")
            .minimum_nvidia_driver_major_version = 580;
        let mut no_selector = hardware_class("bare", &[]);
        no_selector.selector = None;

        assert_eq!(
            errors(&invalid),
            [
                "hardware class \"Not Valid\" requires a valid identity and display name",
                "hardware class \"Not Valid\" selector requires distinct accelerators, each nvidia or apple",
                "hardware class \"Not Valid\" selector takes no minimum NVIDIA driver major version",
            ]
        );
        assert_eq!(
            errors(&hardware_class("any", &[])),
            [
                "hardware class \"any\" selector requires distinct accelerators, each nvidia or apple"
            ]
        );
        assert_eq!(
            errors(&no_selector),
            ["hardware class \"bare\" requires a selector"]
        );
    }

    #[test]
    fn an_unknown_memory_architecture_is_reported() {
        let mut unified = hardware_class("unified", &["nvidia"]);
        unified
            .selector
            .as_mut()
            .expect("selector")
            .set_memory_architecture(AcceleratorMemoryArchitecture::Unified);
        let mut unknown = hardware_class("unknown", &["nvidia"]);
        unknown
            .selector
            .as_mut()
            .expect("selector")
            .memory_architecture = 99;

        assert!(errors(&unified).is_empty());
        assert_eq!(
            errors(&unknown),
            [
                "hardware class \"unknown\" selector memory architecture must be unified, dedicated or unspecified"
            ]
        );
    }

    fn hardware_class(id: &str, accelerators: &[&str]) -> HardwareClass {
        HardwareClass {
            hardware_class_id: id.to_owned(),
            display_name: id.to_uppercase(),
            description: String::new(),
            selector: Some(Selector {
                platform: None,
                accelerators: accelerators
                    .iter()
                    .map(|&vendor| vendor.to_owned())
                    .collect(),
                minimum_nvidia_driver_major_version: 0,
                ..Default::default()
            }),
        }
    }

    fn errors(hardware_class: &HardwareClass) -> Vec<String> {
        validate_hardware_class(hardware_class)
            .iter()
            .map(ToString::to_string)
            .collect()
    }
}
