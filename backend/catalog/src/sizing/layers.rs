//! Merges the layered sizing declaration of one model configuration into flat sizing per hardware
//! class and binding.
//!
//! A configuration declares its sizing equations and shared constants once, and each hardware
//! support entry sets the constants that vary by hardware class and by binding. Merging yields one
//! flat sizing for every supported class and binding, and reports every constant set in the wrong
//! layer, declared twice, undeclared or missing, so each constant has exactly one home.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use super::{EstimateSource, SizingSource};

/// The private sizing layers of one model configuration, as read from its catalog source.
#[derive(Clone, Debug)]
pub(crate) struct CatalogSizingSource {
    pub(crate) model_id: String,
    pub(crate) configuration_id: String,

    /// Configuration-level declaration, absent when the configuration declares no sizing.
    pub(crate) declaration: Option<ConfigurationSizingSource>,

    /// Constants set by each hardware support entry, keyed by its hardware class, in entry order.
    pub(crate) support: Vec<(String, SupportSizingSource)>,
}

impl CatalogSizingSource {
    /// Merges the layers into one flat sizing per supported hardware class and binding, skipping
    /// classes `known_class` rejects. Returns every merged sizing and every layering problem.
    pub(crate) fn flatten(
        &self,
        binding_ids: &[&str],
        known_class: impl Fn(&str) -> bool,
    ) -> (Vec<FlatSizing>, Vec<anyhow::Error>) {
        let mut flattened = Vec::new();
        let mut errors = Vec::new();
        let Some(declaration) = &self.declaration else {
            for (hardware_class_id, support) in &self.support {
                if !support.parameters.is_empty() || !support.binding_sizing.is_empty() {
                    errors.push(anyhow::anyhow!(
                        "{} sets sizing constants, but the configuration declares no sizing",
                        self.class_context(hardware_class_id)
                    ));
                }
            }
            return (flattened, errors);
        };

        for name in declaration.names_in_several_layers() {
            errors.push(anyhow::anyhow!(
                "model {:?} configuration {:?} declares sizing constant {name:?} in more than one layer",
                self.model_id,
                self.configuration_id
            ));
        }
        for (hardware_class_id, support) in &self.support {
            if !known_class(hardware_class_id) {
                continue;
            }
            let context = self.class_context(hardware_class_id);
            let class_errors = errors.len();
            declaration.check_class_constants(&context, &support.parameters, &mut errors);
            let binding_sizing = support.binding_sizing_by_id(&context, binding_ids, &mut errors);
            for binding_id in binding_ids {
                let Some(binding) = binding_sizing.get(binding_id) else {
                    errors.push(anyhow::anyhow!(
                        "{context} has no sizing for binding {binding_id:?}"
                    ));
                    continue;
                };
                let binding_errors = errors.len();
                declaration.check_binding_constants(
                    &format!("{context} binding {binding_id:?}"),
                    &binding.parameters,
                    &mut errors,
                );
                if errors.len() == binding_errors && errors.len() == class_errors {
                    flattened.push(FlatSizing {
                        hardware_class_id: hardware_class_id.clone(),
                        binding_id: (*binding_id).to_owned(),
                        source: declaration.merged(&support.parameters, &binding.parameters),
                    });
                }
            }
        }
        (flattened, errors)
    }

    fn class_context(&self, hardware_class_id: &str) -> String {
        format!(
            "model {:?} configuration {:?} hardware class {hardware_class_id:?}",
            self.model_id, self.configuration_id
        )
    }
}

/// One merged sizing for a hardware class and binding of a configuration.
#[derive(Clone, Debug)]
pub(crate) struct FlatSizing {
    pub(crate) hardware_class_id: String,
    pub(crate) binding_id: String,
    pub(crate) source: SizingSource,
}

/// The configuration-level sizing declaration: equations, derived parameters, constants shared by
/// every class and binding, and the names of the constants each support entry sets once for its
/// class and once per binding.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ConfigurationSizingSource {
    #[serde(default)]
    class_parameters: Vec<String>,
    #[serde(default)]
    binding_parameters: Vec<String>,
    #[serde(default)]
    parameters: BTreeMap<String, u64>,
    #[serde(default)]
    derived_parameters: BTreeMap<String, String>,
    estimates: BTreeMap<String, EstimateSource>,
}

impl ConfigurationSizingSource {
    /// Names declared more than once across the class, binding and shared constant layers.
    fn names_in_several_layers(&self) -> BTreeSet<&str> {
        let mut seen = BTreeSet::new();
        self.class_parameters
            .iter()
            .chain(&self.binding_parameters)
            .chain(self.parameters.keys())
            .map(String::as_str)
            .filter(|name| !seen.insert(*name))
            .collect()
    }

    fn check_class_constants(
        &self,
        context: &str,
        values: &BTreeMap<String, u64>,
        errors: &mut Vec<anyhow::Error>,
    ) {
        for name in values.keys() {
            if self.class_parameters.contains(name) {
                continue;
            }
            errors.push(if self.parameters.contains_key(name) {
                anyhow::anyhow!(
                    "{context} sets shared sizing constant {name:?}, which support entries cannot override"
                )
            } else if self.binding_parameters.contains(name) {
                anyhow::anyhow!(
                    "{context} sets binding sizing constant {name:?} for the whole class; set it per binding"
                )
            } else {
                anyhow::anyhow!("{context} sets undeclared sizing constant {name:?}")
            });
        }
        for name in &self.class_parameters {
            if !values.contains_key(name) {
                errors.push(anyhow::anyhow!(
                    "{context} does not set class sizing constant {name:?}"
                ));
            }
        }
    }

    fn check_binding_constants(
        &self,
        context: &str,
        values: &BTreeMap<String, u64>,
        errors: &mut Vec<anyhow::Error>,
    ) {
        for name in values.keys() {
            if self.binding_parameters.contains(name) {
                continue;
            }
            errors.push(if self.parameters.contains_key(name) {
                anyhow::anyhow!(
                    "{context} sets shared sizing constant {name:?}, which support entries cannot override"
                )
            } else if self.class_parameters.contains(name) {
                anyhow::anyhow!(
                    "{context} sets class sizing constant {name:?}; set it once for the class"
                )
            } else {
                anyhow::anyhow!("{context} sets undeclared sizing constant {name:?}")
            });
        }
        for name in &self.binding_parameters {
            if !values.contains_key(name) {
                errors.push(anyhow::anyhow!(
                    "{context} does not set binding sizing constant {name:?}"
                ));
            }
        }
    }

    fn merged(
        &self,
        class_values: &BTreeMap<String, u64>,
        binding_values: &BTreeMap<String, u64>,
    ) -> SizingSource {
        let mut parameters = self.parameters.clone();
        parameters.extend(class_values.clone());
        parameters.extend(binding_values.clone());
        SizingSource {
            parameters,
            binding_parameters: self.binding_parameters.iter().cloned().collect(),
            derived_parameters: self.derived_parameters.clone(),
            estimates: self.estimates.clone(),
        }
    }
}

/// The sizing constants one hardware support entry sets: its class constants and, per binding,
/// its binding constants.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SupportSizingSource {
    #[serde(default)]
    parameters: BTreeMap<String, u64>,
    #[serde(default)]
    binding_sizing: Vec<BindingSizingSource>,
}

impl SupportSizingSource {
    /// Indexes the entry's binding sizing by binding, reporting unknown and repeated bindings.
    fn binding_sizing_by_id<'a>(
        &'a self,
        context: &str,
        binding_ids: &[&str],
        errors: &mut Vec<anyhow::Error>,
    ) -> BTreeMap<&'a str, &'a BindingSizingSource> {
        let mut by_id = BTreeMap::new();
        for binding in &self.binding_sizing {
            let binding_id = binding.binding_id.as_str();
            if !binding_ids.contains(&binding_id) {
                errors.push(anyhow::anyhow!(
                    "{context} sizes unknown binding {binding_id:?}"
                ));
            } else if by_id.insert(binding_id, binding).is_some() {
                errors.push(anyhow::anyhow!(
                    "{context} sizes binding {binding_id:?} more than once"
                ));
            }
        }
        by_id
    }
}

/// The binding constants one hardware support entry sets for one binding.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BindingSizingSource {
    binding_id: String,
    #[serde(default)]
    parameters: BTreeMap<String, u64>,
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::{CatalogSizingSource, ConfigurationSizingSource, SupportSizingSource};

    #[test]
    fn layers_merge_into_one_flat_sizing_per_class_and_binding() {
        let source = sizing_source(
            declaration(&["offset"], &["base"], &[("shared", 7)]),
            vec![
                (
                    "small",
                    support(&[("offset", 1)], &[("a", &[("base", 10)])]),
                ),
                (
                    "large",
                    support(&[("offset", 2)], &[("a", &[("base", 20)])]),
                ),
            ],
        );

        let (flattened, errors) = source.flatten(&["a"], |_| true);

        assert!(errors.is_empty(), "{errors:?}");
        let merged = flattened
            .iter()
            .map(|flat| {
                (
                    flat.hardware_class_id.as_str(),
                    flat.binding_id.as_str(),
                    flat.source.parameters.clone(),
                    flat.source.binding_parameters.clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            merged,
            vec![
                (
                    "small",
                    "a",
                    parameters(&[("base", 10), ("offset", 1), ("shared", 7)]),
                    BTreeSet::from(["base".to_owned()]),
                ),
                (
                    "large",
                    "a",
                    parameters(&[("base", 20), ("offset", 2), ("shared", 7)]),
                    BTreeSet::from(["base".to_owned()]),
                ),
            ]
        );
    }

    #[test]
    fn each_constant_has_exactly_one_home() {
        let cases: [(&str, ConfigurationSizingSource, SupportSizingSource, &str); 7] = [
            (
                "class entry overrides a shared constant",
                declaration(&[], &["base"], &[("shared", 7)]),
                support(&[("shared", 1)], &[("a", &[("base", 1)])]),
                "sets shared sizing constant \"shared\", which support entries cannot override",
            ),
            (
                "binding entry overrides a shared constant",
                declaration(&[], &["base"], &[("shared", 7)]),
                support(&[], &[("a", &[("base", 1), ("shared", 1)])]),
                "binding \"a\" sets shared sizing constant \"shared\", which support entries cannot override",
            ),
            (
                "binding constant set for the whole class",
                declaration(&[], &["base"], &[]),
                support(&[("base", 1)], &[("a", &[("base", 1)])]),
                "sets binding sizing constant \"base\" for the whole class; set it per binding",
            ),
            (
                "class constant set per binding",
                declaration(&["offset"], &[], &[]),
                support(&[("offset", 1)], &[("a", &[("offset", 1)])]),
                "binding \"a\" sets class sizing constant \"offset\"; set it once for the class",
            ),
            (
                "undeclared constant",
                declaration(&[], &["base"], &[]),
                support(&[], &[("a", &[("base", 1), ("extra", 1)])]),
                "binding \"a\" sets undeclared sizing constant \"extra\"",
            ),
            (
                "missing class constant",
                declaration(&["offset"], &[], &[]),
                support(&[], &[("a", &[])]),
                "does not set class sizing constant \"offset\"",
            ),
            (
                "missing binding constant",
                declaration(&[], &["base"], &[]),
                support(&[], &[("a", &[])]),
                "binding \"a\" does not set binding sizing constant \"base\"",
            ),
        ];
        for (label, declaration, support, expected) in cases {
            let source = sizing_source(declaration, vec![("nvidia", support)]);

            let (flattened, errors) = source.flatten(&["a"], |_| true);

            assert!(flattened.is_empty(), "{label}");
            assert_eq!(
                messages(&errors),
                vec![format!(
                    "model \"fixture-model\" configuration \"local\" hardware class \"nvidia\" {expected}"
                )],
                "{label}"
            );
        }
    }

    #[test]
    fn a_name_declared_in_two_layers_is_rejected() {
        let source = sizing_source(
            declaration(&["base"], &["base", "margin"], &[("margin", 1)]),
            vec![(
                "nvidia",
                support(&[("base", 1)], &[("a", &[("margin", 1)])]),
            )],
        );

        let (_, errors) = source.flatten(&["a"], |_| true);

        assert_eq!(
            messages(&errors)[..2],
            [
                "model \"fixture-model\" configuration \"local\" declares sizing constant \"base\" in more than one layer",
                "model \"fixture-model\" configuration \"local\" declares sizing constant \"margin\" in more than one layer",
            ]
        );
    }

    #[test]
    fn every_binding_needs_sizing_in_every_support_entry() {
        let source = sizing_source(
            declaration(&[], &[], &[]),
            vec![(
                "nvidia",
                support(&[], &[("a", &[]), ("a", &[]), ("unknown", &[])]),
            )],
        );

        let (flattened, errors) = source.flatten(&["a", "b"], |_| true);

        assert_eq!(
            messages(&errors),
            [
                "model \"fixture-model\" configuration \"local\" hardware class \"nvidia\" sizes binding \"a\" more than once",
                "model \"fixture-model\" configuration \"local\" hardware class \"nvidia\" sizes unknown binding \"unknown\"",
                "model \"fixture-model\" configuration \"local\" hardware class \"nvidia\" has no sizing for binding \"b\"",
            ]
        );
        assert!(flattened.is_empty());
    }

    #[test]
    fn unknown_classes_are_skipped_and_unsized_configurations_take_no_constants() {
        let unknown = sizing_source(
            declaration(&[], &["base"], &[]),
            vec![("missing", support(&[], &[]))],
        );
        let (flattened, errors) = unknown.flatten(&["a"], |id| id != "missing");
        assert!(flattened.is_empty());
        assert!(errors.is_empty(), "{errors:?}");

        let unsized_configuration = CatalogSizingSource {
            declaration: None,
            ..sizing_source(
                declaration(&[], &[], &[]),
                vec![("nvidia", support(&[], &[("a", &[])]))],
            )
        };
        let (_, errors) = unsized_configuration.flatten(&["a"], |_| true);
        assert_eq!(
            messages(&errors),
            [
                "model \"fixture-model\" configuration \"local\" hardware class \"nvidia\" sets sizing constants, but the configuration declares no sizing"
            ]
        );
    }

    fn sizing_source(
        declaration: ConfigurationSizingSource,
        support: Vec<(&str, SupportSizingSource)>,
    ) -> CatalogSizingSource {
        CatalogSizingSource {
            model_id: "fixture-model".to_owned(),
            configuration_id: "local".to_owned(),
            declaration: Some(declaration),
            support: support
                .into_iter()
                .map(|(class, support)| (class.to_owned(), support))
                .collect(),
        }
    }

    fn declaration(
        class_parameters: &[&str],
        binding_parameters: &[&str],
        shared: &[(&str, u64)],
    ) -> ConfigurationSizingSource {
        serde_json::from_value(serde_json::json!({
            "classParameters": class_parameters,
            "bindingParameters": binding_parameters,
            "parameters": parameters(shared),
            "estimates": {
                "acceleratorMemory": { "unit": "bytes", "equation": "${total-context}" }
            },
        }))
        .expect("sizing declaration")
    }

    fn support(
        class_values: &[(&str, u64)],
        bindings: &[(&str, &[(&str, u64)])],
    ) -> SupportSizingSource {
        serde_json::from_value(serde_json::json!({
            "parameters": parameters(class_values),
            "bindingSizing": bindings
                .iter()
                .map(|(binding_id, values)| serde_json::json!({
                    "bindingId": binding_id,
                    "parameters": parameters(values),
                }))
                .collect::<Vec<_>>(),
        }))
        .expect("support sizing")
    }

    fn parameters(values: &[(&str, u64)]) -> BTreeMap<String, u64> {
        values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), *value))
            .collect()
    }

    fn messages(errors: &[anyhow::Error]) -> Vec<String> {
        errors.iter().map(ToString::to_string).collect()
    }
}
