//! Parameter groups and parameter conditions: their shared validation and the rule deciding which
//! parameters are enabled.
//!
//! Application graphs and engine releases may present consecutive parameters together in groups.
//! A parameter may also depend on a boolean parameter, directly or through its switched group, and
//! is enabled only while that boolean is true; a disabled parameter keeps no value. Owners layer
//! their own rules on top, such as engines accepting plain groups only.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::anyhow;
use dotcore_domain::valid_id;
use dotcore_proto::dotcore::catalog::v1::{
    ParameterDefinition, ParameterGroup, ParameterValue, parameter_definition::ParameterType,
    parameter_value::Value,
};

/// Returns the parameter whose boolean value enables `definition`: its own enabling parameter,
/// else its switched group's switch. A switch, and a parameter with neither, has none and is
/// always enabled.
pub fn enabler<'a>(
    definition: &'a ParameterDefinition,
    groups: &'a [ParameterGroup],
) -> Option<&'a str> {
    if let Some(enabler) = definition.enabled_by_parameter_id.as_deref() {
        return Some(enabler);
    }
    let group_id = definition.group_id.as_deref()?;
    groups
        .iter()
        .find(|group| group.group_id == group_id)?
        .switch_parameter_id
        .as_deref()
        .filter(|switch| *switch != definition.parameter_id)
}

/// Returns the ids of the enabled parameters, given values holding at least every enabling
/// parameter's value. Returns none when an enabling parameter has no boolean value among them.
pub fn enabled_parameters<'a>(
    parameters: &'a [ParameterDefinition],
    groups: &[ParameterGroup],
    values: &[ParameterValue],
) -> Option<BTreeSet<&'a str>> {
    let mut enabled = BTreeSet::new();
    for definition in parameters {
        let on = match enabler(definition, groups) {
            None => true,
            Some(enabler) => boolean_value(values, enabler)?,
        };
        if on {
            enabled.insert(definition.parameter_id.as_str());
        }
    }
    Some(enabled)
}

/// Returns the ids of the enabled parameters of retained values, which must hold exactly one value
/// for each enabled parameter and none for any other. Returns none when they don't, so a retained
/// state that lost an enabled value, or kept a disabled one, is caught rather than silently
/// dropping what depends on it.
pub fn retained_enabled_parameters<'a>(
    parameters: &'a [ParameterDefinition],
    groups: &[ParameterGroup],
    values: &[ParameterValue],
) -> Option<BTreeSet<&'a str>> {
    let enabled = enabled_parameters(parameters, groups, values)?;
    let mut retained = BTreeSet::new();
    for value in values {
        if value.value.is_none() || !retained.insert(value.parameter_id.as_str()) {
            return None;
        }
    }
    (retained == enabled).then_some(enabled)
}

/// Validates an owner's groups and parameter conditions: unique well-formed group ids; groups
/// with consecutive members; plain groups with a title and description; switches that are the
/// boolean first member of their group with at least one other member sharing its visibility; and
/// enabling parameters that are booleans of the same group and visibility, with no enabler of
/// their own, directly followed by their dependents. `owner` names the declaring owner in messages.
pub(crate) fn validate_parameter_groups(
    owner: &str,
    parameters: &[ParameterDefinition],
    groups: &[ParameterGroup],
) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    let mut group_ids = BTreeSet::new();
    for group in groups {
        validate_group(owner, group, parameters, &mut group_ids, &mut errors);
    }
    for definition in parameters {
        if let Some(group_id) = definition.group_id.as_deref()
            && !group_ids.contains(group_id)
        {
            errors.push(anyhow!(
                "{owner} parameter {:?} names undeclared group {group_id:?}",
                definition.parameter_id
            ));
        }
    }
    validate_enablers(owner, parameters, groups, &mut errors);
    errors
}

/// Validates one group's identity, members and, for a switched group, its switch.
fn validate_group<'a>(
    owner: &str,
    group: &'a ParameterGroup,
    parameters: &[ParameterDefinition],
    group_ids: &mut BTreeSet<&'a str>,
    errors: &mut Vec<anyhow::Error>,
) {
    let id = &group.group_id;
    if !valid_id(id) {
        errors.push(anyhow!(
            "{owner} parameter group id {id:?} must match [a-z][a-z0-9-]{{0,62}}"
        ));
    }
    if !group_ids.insert(id.as_str()) {
        errors.push(anyhow!("duplicate {owner} parameter group {id:?}"));
    }

    let members = parameters
        .iter()
        .enumerate()
        .filter(|(_, definition)| definition.group_id.as_deref() == Some(id.as_str()))
        .collect::<Vec<_>>();
    match (members.first(), members.last()) {
        (Some((first, _)), Some((last, _))) if last - first + 1 != members.len() => {
            errors.push(anyhow!(
                "{owner} parameter group {id:?} members must be consecutive parameters"
            ));
        }
        (Some(_), Some(_)) => {}
        _ => errors.push(anyhow!(
            "{owner} parameter group {id:?} requires at least one member"
        )),
    }

    let Some(switch_id) = group.switch_parameter_id.as_deref() else {
        if group.display_name.trim().is_empty() {
            errors.push(anyhow!(
                "{owner} parameter group {id:?} requires a display name"
            ));
        }
        if group.description.trim().is_empty() {
            errors.push(anyhow!(
                "{owner} parameter group {id:?} requires a description"
            ));
        }
        return;
    };
    if !group.display_name.is_empty() || !group.description.is_empty() {
        errors.push(anyhow!(
            "{owner} parameter group {id:?} takes its display name and description from its switch {switch_id:?}"
        ));
    }
    let Some((_, switch)) = members
        .first()
        .filter(|(_, first)| first.parameter_id == switch_id)
    else {
        errors.push(anyhow!(
            "{owner} parameter group {id:?} switch {switch_id:?} must be its first member"
        ));
        return;
    };
    if !is_boolean(switch) {
        errors.push(anyhow!(
            "{owner} parameter group {id:?} switch {switch_id:?} must be a boolean"
        ));
    }
    if switch.enabled_by_parameter_id.is_some() {
        errors.push(anyhow!(
            "{owner} parameter group {id:?} switch {switch_id:?} must not be enabled by another parameter"
        ));
    }
    if members.len() < 2 {
        errors.push(anyhow!(
            "{owner} parameter group {id:?} requires a member besides its switch {switch_id:?}"
        ));
    }
    for (_, member) in &members[1..] {
        let member_id = &member.parameter_id;
        // The group renders in one step, so its members must all show where its switch does.
        if member.visibility != switch.visibility {
            errors.push(anyhow!(
                "{owner} parameter {member_id:?} must share the visibility of its group's switch {switch_id:?}"
            ));
        }
        if member.enabled_by_parameter_id.is_some() {
            errors.push(anyhow!(
                "{owner} parameter {member_id:?} is enabled by its group's switch {switch_id:?}, so it must not name another enabling parameter"
            ));
        }
    }
}

/// Validates each parameter's own enabling parameter and that dependents directly follow it.
fn validate_enablers(
    owner: &str,
    parameters: &[ParameterDefinition],
    groups: &[ParameterGroup],
    errors: &mut Vec<anyhow::Error>,
) {
    let mut positions = BTreeMap::new();
    for (index, definition) in parameters.iter().enumerate() {
        positions
            .entry(definition.parameter_id.as_str())
            .or_insert(index);
    }
    let mut dependents = BTreeMap::<&str, Vec<usize>>::new();
    for (index, definition) in parameters.iter().enumerate() {
        let Some(enabler_id) = definition.enabled_by_parameter_id.as_deref() else {
            continue;
        };
        let id = &definition.parameter_id;
        let Some(enabling) = positions
            .get(enabler_id)
            .map(|position| &parameters[*position])
        else {
            errors.push(anyhow!(
                "{owner} parameter {id:?} is enabled by missing parameter {enabler_id:?}"
            ));
            continue;
        };
        dependents.entry(enabler_id).or_default().push(index);
        if !is_boolean(enabling) {
            errors.push(anyhow!(
                "{owner} parameter {id:?} is enabled by parameter {enabler_id:?}, which is not a boolean"
            ));
        }
        if enabler(enabling, groups).is_some() {
            errors.push(anyhow!(
                "{owner} parameter {id:?} is enabled by parameter {enabler_id:?}, which is itself enabled by another parameter"
            ));
        }
        if enabling.group_id != definition.group_id {
            errors.push(anyhow!(
                "{owner} parameter {id:?} must be in the same group as its enabling parameter {enabler_id:?}"
            ));
        }
        if enabling.visibility != definition.visibility {
            errors.push(anyhow!(
                "{owner} parameter {id:?} must share the visibility of its enabling parameter {enabler_id:?}"
            ));
        }
    }
    for (enabler_id, indexes) in dependents {
        let position = positions[enabler_id];
        if indexes != (position + 1..=position + indexes.len()).collect::<Vec<_>>() {
            errors.push(anyhow!(
                "{owner} parameters enabled by {enabler_id:?} must directly follow it"
            ));
        }
    }
}

fn is_boolean(definition: &ParameterDefinition) -> bool {
    matches!(definition.parameter_type, Some(ParameterType::Boolean(_)))
}

fn boolean_value(values: &[ParameterValue], parameter_id: &str) -> Option<bool> {
    match values
        .iter()
        .find(|value| value.parameter_id == parameter_id)?
        .value
    {
        Some(Value::Boolean(value)) => Some(value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::catalog::v1::{
        BooleanParameterSchema, ParameterVisibility, TextParameterSchema,
    };

    use super::*;

    #[test]
    fn enabler_is_the_own_enabling_parameter_else_the_switch_of_a_switched_group() {
        let groups = [switched("login", "require-login"), plain("memory")];

        assert_eq!(enabler(&switch_member("require-login"), &groups), None);
        assert_eq!(
            enabler(&switch_member("admin-email"), &groups),
            Some("require-login")
        );
        assert_eq!(enabler(&member("cache", "memory"), &groups), None);
        assert_eq!(
            enabler(&dependent("mode", "signup"), &groups),
            Some("signup")
        );
        assert_eq!(enabler(&text("name"), &groups), None);
    }

    #[test]
    fn enabled_parameters_follow_their_enablers_value() {
        let parameters = login_parameters();
        let groups = [switched("login", "require-login")];

        assert_eq!(
            enabled_parameters(
                &parameters,
                &groups,
                &[
                    boolean_value("require-login", false),
                    boolean_value("signup", true)
                ]
            ),
            Some(BTreeSet::from(["require-login", "signup", "mode"]))
        );
        assert_eq!(
            enabled_parameters(
                &parameters,
                &groups,
                &[
                    boolean_value("require-login", true),
                    boolean_value("signup", false)
                ]
            ),
            Some(BTreeSet::from([
                "require-login",
                "admin-email",
                "admin-password",
                "signup"
            ]))
        );
        assert_eq!(enabled_parameters(&parameters, &groups, &[]), None);
        assert_eq!(
            enabled_parameters(&parameters, &groups, &[text_value("require-login", "yes")]),
            None
        );
    }

    #[test]
    fn retained_values_must_match_the_enabled_parameters_exactly() {
        let parameters = login_parameters();
        let groups = [switched("login", "require-login")];
        let off = [
            boolean_value("require-login", false),
            boolean_value("signup", false),
        ];
        let on = [
            boolean_value("require-login", true),
            text_value("admin-email", "admin@example.com"),
            text_value("admin-password", "password"),
            boolean_value("signup", false),
        ];

        assert_eq!(
            retained_enabled_parameters(&parameters, &groups, &off),
            Some(BTreeSet::from(["require-login", "signup"]))
        );
        assert_eq!(
            retained_enabled_parameters(&parameters, &groups, &on),
            Some(BTreeSet::from([
                "require-login",
                "admin-email",
                "admin-password",
                "signup"
            ]))
        );
        let disabled_with_value = [off.as_slice(), &[text_value("admin-email", "a@b")]].concat();
        assert_eq!(
            retained_enabled_parameters(&parameters, &groups, &disabled_with_value),
            None
        );
        assert_eq!(
            retained_enabled_parameters(&parameters, &groups, &on[..3]),
            None
        );
        let duplicate = [on.as_slice(), &[boolean_value("signup", true)]].concat();
        assert_eq!(
            retained_enabled_parameters(&parameters, &groups, &duplicate),
            None
        );
        let empty = [
            off.as_slice(),
            &[ParameterValue {
                parameter_id: "admin-email".to_owned(),
                value: None,
            }],
        ]
        .concat();
        assert_eq!(
            retained_enabled_parameters(&parameters, &groups, &empty),
            None
        );
    }

    #[test]
    fn valid_groups_and_conditions_pass() {
        let mut parameters = login_parameters();
        parameters.extend([
            member("cache", "memory"),
            ParameterDefinition {
                visibility: ParameterVisibility::Advanced as i32,
                ..member("offload", "memory")
            },
        ]);
        let groups = [switched("login", "require-login"), plain("memory")];

        assert_eq!(errors(&parameters, &groups), Vec::<String>::new());
    }

    #[test]
    fn group_rules_reject_each_malformed_group() {
        let cases: [GroupCase; 9] = [
            (
                "unknown group",
                vec![member("cache", "memory")],
                vec![],
                &["application parameter \"cache\" names undeclared group \"memory\""],
            ),
            (
                "duplicate group",
                vec![member("cache", "memory")],
                vec![plain("memory"), plain("memory")],
                &["duplicate application parameter group \"memory\""],
            ),
            (
                "malformed group id",
                vec![member("cache", "Memory")],
                vec![plain("Memory")],
                &["application parameter group id \"Memory\" must match [a-z][a-z0-9-]{0,62}"],
            ),
            (
                "group without members",
                vec![text("cache")],
                vec![plain("memory")],
                &["application parameter group \"memory\" requires at least one member"],
            ),
            (
                "non-contiguous members",
                vec![
                    member("cache", "memory"),
                    text("name"),
                    member("offload", "memory"),
                ],
                vec![plain("memory")],
                &["application parameter group \"memory\" members must be consecutive parameters"],
            ),
            (
                "plain group without title or description",
                vec![member("cache", "memory")],
                vec![ParameterGroup {
                    display_name: " ".to_owned(),
                    description: String::new(),
                    ..plain("memory")
                }],
                &[
                    "application parameter group \"memory\" requires a display name",
                    "application parameter group \"memory\" requires a description",
                ],
            ),
            (
                "switched group with its own title",
                login_parameters(),
                vec![ParameterGroup {
                    display_name: "Login".to_owned(),
                    ..switched("login", "require-login")
                }],
                &[
                    "application parameter group \"login\" takes its display name and description from its switch \"require-login\"",
                ],
            ),
            (
                "switch not first",
                vec![switch_member("admin-email"), switch_member("require-login")],
                vec![switched("login", "require-login")],
                &[
                    "application parameter group \"login\" switch \"require-login\" must be its first member",
                ],
            ),
            (
                "switch not a boolean",
                vec![
                    switch_member("admin-email"),
                    switch_member("admin-password"),
                ],
                vec![switched("login", "admin-email")],
                &["application parameter group \"login\" switch \"admin-email\" must be a boolean"],
            ),
        ];

        for (label, parameters, groups, expected) in cases {
            assert_eq!(errors(&parameters, &groups), expected, "{label}");
        }
    }

    #[test]
    fn switch_rules_reject_each_malformed_switched_group() {
        let groups = [switched("login", "require-login")];
        let cases: [(&str, Vec<ParameterDefinition>, &[&str]); 4] = [
            (
                "switch with its own enabler",
                vec![
                    boolean("signup"),
                    ParameterDefinition {
                        enabled_by_parameter_id: Some("signup".to_owned()),
                        ..switch_member("require-login")
                    },
                    switch_member("admin-email"),
                ],
                &[
                    "application parameter group \"login\" switch \"require-login\" must not be enabled by another parameter",
                    "application parameter \"require-login\" must be in the same group as its enabling parameter \"signup\"",
                ],
            ),
            (
                "switched group with only its switch",
                vec![switch_member("require-login")],
                &[
                    "application parameter group \"login\" requires a member besides its switch \"require-login\"",
                ],
            ),
            (
                "member with its own enabler",
                vec![
                    switch_member("require-login"),
                    ParameterDefinition {
                        enabled_by_parameter_id: Some("require-login".to_owned()),
                        ..switch_member("admin-email")
                    },
                ],
                &[
                    "application parameter \"admin-email\" is enabled by its group's switch \"require-login\", so it must not name another enabling parameter",
                ],
            ),
            (
                "member with another visibility",
                vec![
                    switch_member("require-login"),
                    ParameterDefinition {
                        visibility: ParameterVisibility::Advanced as i32,
                        ..switch_member("admin-email")
                    },
                ],
                &[
                    "application parameter \"admin-email\" must share the visibility of its group's switch \"require-login\"",
                ],
            ),
        ];

        for (label, parameters, expected) in cases {
            assert_eq!(errors(&parameters, &groups), expected, "{label}");
        }
    }

    #[test]
    fn enabler_rules_reject_each_malformed_condition() {
        let cases: [(&str, Vec<ParameterDefinition>, &[&str]); 7] = [
            (
                "missing enabler",
                vec![dependent("mode", "signup")],
                &["application parameter \"mode\" is enabled by missing parameter \"signup\""],
            ),
            (
                "enabler not a boolean",
                vec![text("signup"), dependent("mode", "signup")],
                &[
                    "application parameter \"mode\" is enabled by parameter \"signup\", which is not a boolean",
                ],
            ),
            (
                "enabler chain",
                vec![
                    boolean("signup"),
                    ParameterDefinition {
                        enabled_by_parameter_id: Some("signup".to_owned()),
                        ..boolean("invite")
                    },
                    dependent("mode", "invite"),
                ],
                &[
                    "application parameter \"mode\" is enabled by parameter \"invite\", which is itself enabled by another parameter",
                ],
            ),
            (
                "enabler in another group",
                vec![
                    boolean("signup"),
                    ParameterDefinition {
                        group_id: Some("memory".to_owned()),
                        ..dependent("mode", "signup")
                    },
                ],
                &[
                    "application parameter \"mode\" must be in the same group as its enabling parameter \"signup\"",
                ],
            ),
            (
                "enabler with another visibility",
                vec![
                    boolean("signup"),
                    ParameterDefinition {
                        visibility: ParameterVisibility::Advanced as i32,
                        ..dependent("mode", "signup")
                    },
                ],
                &[
                    "application parameter \"mode\" must share the visibility of its enabling parameter \"signup\"",
                ],
            ),
            (
                "dependent apart from its enabler",
                vec![boolean("signup"), text("name"), dependent("mode", "signup")],
                &["application parameters enabled by \"signup\" must directly follow it"],
            ),
            (
                "dependent before its enabler",
                vec![dependent("mode", "signup"), boolean("signup")],
                &["application parameters enabled by \"signup\" must directly follow it"],
            ),
        ];
        let groups = [plain("memory")];

        for (label, mut parameters, expected) in cases {
            // Keeps the plain group declared and used, so only the condition under test fails.
            parameters.push(member("cache", "memory"));
            assert_eq!(errors(&parameters, &groups), expected, "{label}");
        }
    }

    /// A labelled set of parameters and groups with the exact validation errors it yields.
    type GroupCase = (
        &'static str,
        Vec<ParameterDefinition>,
        Vec<ParameterGroup>,
        &'static [&'static str],
    );

    fn errors(parameters: &[ParameterDefinition], groups: &[ParameterGroup]) -> Vec<String> {
        validate_parameter_groups("application", parameters, groups)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// Open WebUI's shape: a login switch with two members, then an ungrouped switch with one
    /// dependent.
    fn login_parameters() -> Vec<ParameterDefinition> {
        vec![
            ParameterDefinition {
                group_id: Some("login".to_owned()),
                ..boolean("require-login")
            },
            switch_member("admin-email"),
            switch_member("admin-password"),
            boolean("signup"),
            dependent("mode", "signup"),
        ]
    }

    fn plain(group_id: &str) -> ParameterGroup {
        ParameterGroup {
            group_id: group_id.to_owned(),
            display_name: "Memory".to_owned(),
            description: "Where the cache lives.".to_owned(),
            switch_parameter_id: None,
        }
    }

    fn switched(group_id: &str, switch: &str) -> ParameterGroup {
        ParameterGroup {
            group_id: group_id.to_owned(),
            display_name: String::new(),
            description: String::new(),
            switch_parameter_id: Some(switch.to_owned()),
        }
    }

    fn switch_member(parameter_id: &str) -> ParameterDefinition {
        let definition = if parameter_id == "require-login" {
            boolean(parameter_id)
        } else {
            text(parameter_id)
        };
        member_of(definition, "login")
    }

    fn member(parameter_id: &str, group_id: &str) -> ParameterDefinition {
        member_of(text(parameter_id), group_id)
    }

    fn member_of(definition: ParameterDefinition, group_id: &str) -> ParameterDefinition {
        ParameterDefinition {
            group_id: Some(group_id.to_owned()),
            ..definition
        }
    }

    fn dependent(parameter_id: &str, enabler: &str) -> ParameterDefinition {
        ParameterDefinition {
            enabled_by_parameter_id: Some(enabler.to_owned()),
            ..text(parameter_id)
        }
    }

    fn boolean(parameter_id: &str) -> ParameterDefinition {
        definition(
            parameter_id,
            ParameterType::Boolean(BooleanParameterSchema {
                default_value: Some(false),
            }),
        )
    }

    fn text(parameter_id: &str) -> ParameterDefinition {
        definition(
            parameter_id,
            ParameterType::Text(TextParameterSchema {
                minimum_length: 0,
                maximum_length: None,
                default_value: Some(String::new()),
                format: None,
            }),
        )
    }

    fn definition(parameter_id: &str, parameter_type: ParameterType) -> ParameterDefinition {
        ParameterDefinition {
            parameter_id: parameter_id.to_owned(),
            display_name: parameter_id.to_owned(),
            description: String::new(),
            visibility: ParameterVisibility::Basic as i32,
            group_id: None,
            enabled_by_parameter_id: None,
            parameter_type: Some(parameter_type),
        }
    }

    fn boolean_value(parameter_id: &str, value: bool) -> ParameterValue {
        ParameterValue {
            parameter_id: parameter_id.to_owned(),
            value: Some(Value::Boolean(value)),
        }
    }

    fn text_value(parameter_id: &str, value: &str) -> ParameterValue {
        ParameterValue {
            parameter_id: parameter_id.to_owned(),
            value: Some(Value::String(value.to_owned())),
        }
    }
}
