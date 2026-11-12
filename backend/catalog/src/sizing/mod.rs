//! Compiles and evaluates private sizing declarations, one per hardware class and binding.
//!
//! Sizing equations are checked against configuration-local engine and option
//! schemas. Evaluation uses the resolved immutable values, omits estimates whose
//! evidence predicate is false, and keeps arithmetic failures explicit.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_proto::dotcore::catalog::v1::{
    EngineParameterSchema, ModelEngineConfiguration, ModelOptionParameterValue, ModelOptionValues,
    ParameterValues, model_option_parameter_definition::ParameterType as OptionParameterType,
    model_option_parameter_value::Value as OptionParameterValue,
    parameter_definition::ParameterType as EngineParameterType,
    parameter_value::Value as EngineParameterValueKind,
};
use serde::Deserialize;

mod expression;
mod layers;

pub(super) use layers::{CatalogSizingSource, ConfigurationSizingSource, SupportSizingSource};

/// Canonical estimate naming a binding's total accelerator memory footprint.
pub const ACCELERATOR_MEMORY_RESOURCE: &str = "acceleratorMemory";

/// Canonical estimate naming a binding's total host memory footprint.
pub const HOST_MEMORY_RESOURCE: &str = "hostMemory";

const MAX_PARAMETERS: usize = 32;
const MAX_DERIVED_PARAMETERS: usize = 32;
const MAX_ESTIMATES: usize = 32;
const MAX_NAME_BYTES: usize = 63;
const OPTION_SYMBOL_PREFIX: &str = "options";

/// Flat sizing of one hardware class and binding: every constant merged from its layers, the
/// names of the binding constants, derived parameters and estimates.
#[derive(Clone, Debug)]
pub(super) struct SizingSource {
    parameters: BTreeMap<String, u64>,
    binding_parameters: BTreeSet<String>,
    derived_parameters: BTreeMap<String, String>,
    estimates: BTreeMap<String, EstimateSource>,
}

/// One declared estimate: the measured equation, an optional allowance for budgets the engine may
/// consume later, such as a prompt cache or a safety margin, and an optional availability
/// predicate. The estimate's value is the equation plus the allowance.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EstimateSource {
    unit: String,
    equation: String,
    #[serde(default)]
    allowance: Option<String>,
    #[serde(default)]
    available_when: Option<String>,
}

/// Identity of one compiled sizing: a model configuration's binding on one hardware class.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct SizingKey {
    pub(super) model_id: String,
    pub(super) configuration_id: String,
    pub(super) hardware_class_id: String,
    pub(super) binding_id: String,
}

#[derive(Clone, Debug)]
pub(super) struct CompiledSizing {
    parameters: BTreeMap<String, u64>,
    binding_parameters: BTreeSet<String>,
    derived_parameters: Vec<CompiledDerivedParameter>,
    estimates: Vec<CompiledEstimate>,
}

impl CompiledSizing {
    /// Evaluates derivations for the engine parameters absent from the supplied values, in
    /// parameter id order. Supplied values always take precedence over a derivation.
    pub(super) fn derive(
        &self,
        engine_values: &ParameterValues,
        option_values: &[ModelOptionValues],
    ) -> Result<Vec<(String, u64)>, SizingError> {
        let values = self.symbol_values(engine_values, option_values)?;
        let mut derived = Vec::new();
        for parameter in &self.derived_parameters {
            if values.contains_key(&parameter.parameter_id) {
                continue;
            }
            let value = parameter
                .expression
                .evaluate_unsigned(&values)
                .map_err(SizingError::Arithmetic)?;
            derived.push((parameter.parameter_id.clone(), value));
        }
        Ok(derived)
    }

    /// Whether this binding derives the engine parameter when a caller omits it.
    pub(super) fn derives(&self, parameter_id: &str) -> bool {
        self.derived_parameters
            .iter()
            .any(|parameter| parameter.parameter_id == parameter_id)
    }

    /// Whether this binding declares the named estimate.
    pub(super) fn declares_estimate(&self, resource: &str) -> bool {
        self.estimates
            .iter()
            .any(|estimate| estimate.resource == resource)
    }

    /// Evaluates available results in deterministic resource-name order.
    pub(super) fn evaluate(
        &self,
        engine_values: &ParameterValues,
        option_values: &[ModelOptionValues],
    ) -> Result<Vec<ScalarResourceEstimate>, SizingError> {
        let values = self.symbol_values(engine_values, option_values)?;

        let mut estimates = Vec::new();
        for estimate in &self.estimates {
            if !estimate
                .available_when
                .evaluate_boolean(&values)
                .map_err(SizingError::Arithmetic)?
            {
                continue;
            }
            estimates.push(ScalarResourceEstimate {
                resource: estimate.resource.clone(),
                value: estimate.evaluate(&values)?,
                unit: estimate.unit.clone(),
            });
        }
        Ok(estimates)
    }

    /// Builds a fit model for one estimate's measured equation whose named parameters, or the
    /// binding constants the equation uses when none are named, are unknown. Class and shared
    /// constants stay known by default, since they hold for every binding and are not measured per
    /// binding. The allowance is carried for comparison but never fitted.
    pub(super) fn fit_model(
        &self,
        resource: &str,
        unknowns: &[String],
    ) -> Result<SizingFitModel, SizingError> {
        let estimate = self
            .estimates
            .iter()
            .find(|estimate| estimate.resource == resource)
            .ok_or_else(|| SizingError::declaration(format!("has no estimate {resource:?}")))?;
        let unknowns = if unknowns.is_empty() {
            self.binding_parameters_in(&estimate.expression)
        } else {
            unknowns.to_vec()
        };
        if unknowns.is_empty() {
            return Err(SizingError::declaration(
                "fit requires at least one parameter",
            ));
        }
        for unknown in &unknowns {
            if !self.parameters.contains_key(unknown) {
                return Err(SizingError::declaration(format!(
                    "has no parameter {unknown:?} to fit"
                )));
            }
        }
        self.estimate_fit_model(estimate, &unknowns)
    }

    /// Builds a model of one estimate whose constants all keep their declared values, to check
    /// stored constants against telemetry.
    pub(super) fn check_model(&self, resource: &str) -> Result<SizingFitModel, SizingError> {
        let estimate = self
            .estimates
            .iter()
            .find(|estimate| estimate.resource == resource)
            .ok_or_else(|| SizingError::declaration(format!("has no estimate {resource:?}")))?;
        self.estimate_fit_model(estimate, &[])
    }

    /// The binding's declared sizing parameters.
    pub(super) fn parameters(&self) -> &BTreeMap<String, u64> {
        &self.parameters
    }

    fn estimate_fit_model(
        &self,
        estimate: &CompiledEstimate,
        unknowns: &[String],
    ) -> Result<SizingFitModel, SizingError> {
        let known = self
            .parameters
            .iter()
            .filter(|(name, _)| !unknowns.contains(name))
            .map(|(name, value)| (name.clone(), *value))
            .collect();
        let allowance = estimate.allowance.as_ref().map(|allowance| FitAllowance {
            parameters: self.binding_parameters_in(allowance),
            expression: allowance.clone(),
        });
        let binding_constants = self
            .parameters
            .iter()
            .filter(|(name, _)| self.binding_parameters.contains(*name))
            .map(|(name, value)| (name.clone(), *value))
            .collect();
        SizingFitModel::new(
            estimate.expression.clone(),
            allowance,
            binding_constants,
            Some(estimate.available_when.clone()),
            known,
            unknowns,
            Vec::new(),
        )
    }

    /// The binding constants an expression references, in name order.
    fn binding_parameters_in(&self, expression: &expression::CompiledExpression) -> Vec<String> {
        self.binding_parameters
            .iter()
            .filter(|name| expression.references_any(&BTreeSet::from([(*name).clone()])))
            .cloned()
            .collect()
    }

    fn symbol_values(
        &self,
        engine_values: &ParameterValues,
        option_values: &[ModelOptionValues],
    ) -> Result<BTreeMap<String, expression::Value>, SizingError> {
        let mut values = self
            .parameters
            .iter()
            .map(|(name, value)| (name.clone(), expression::Value::Unsigned(*value)))
            .collect::<BTreeMap<_, _>>();
        insert_engine_values(&mut values, engine_values)?;
        insert_option_values(&mut values, option_values)?;
        Ok(values)
    }
}

#[derive(Clone, Debug)]
struct CompiledDerivedParameter {
    parameter_id: String,
    expression: expression::CompiledExpression,
}

#[derive(Clone, Debug)]
struct CompiledEstimate {
    resource: String,
    unit: String,
    available_when: expression::CompiledExpression,
    expression: expression::CompiledExpression,
    allowance: Option<expression::CompiledExpression>,
}

impl CompiledEstimate {
    /// Evaluates the measured equation plus the allowance.
    fn evaluate(&self, values: &BTreeMap<String, expression::Value>) -> Result<u64, SizingError> {
        let equation = self
            .expression
            .evaluate_unsigned(values)
            .map_err(SizingError::Arithmetic)?;
        let allowance = evaluate_allowance(self.allowance.as_ref(), values)?;
        equation.checked_add(allowance).ok_or_else(|| {
            SizingError::evaluation(format!(
                "estimate {:?} overflows adding its allowance",
                self.resource
            ))
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScalarResourceEstimate {
    pub resource: String,
    pub value: u64,
    pub unit: String,
}

/// Compiles one sizing block against the configuration's engine parameter schema and its
/// configuration-local options.
pub(super) fn compile(
    source: &SizingSource,
    schema: &EngineParameterSchema,
    configuration: &ModelEngineConfiguration,
) -> Result<CompiledSizing, SizingError> {
    if source.parameters.len() > MAX_PARAMETERS {
        return Err(SizingError::declaration("exceeds 32 parameters"));
    }
    if source.derived_parameters.len() > MAX_DERIVED_PARAMETERS {
        return Err(SizingError::declaration("exceeds 32 derived parameters"));
    }
    if source.estimates.is_empty() {
        return Err(SizingError::declaration("requires at least one estimate"));
    }
    if source.estimates.len() > MAX_ESTIMATES {
        return Err(SizingError::declaration("exceeds 32 estimates"));
    }

    let mut symbols = configuration_symbols(schema, configuration)?;
    for name in source.parameters.keys() {
        if symbols
            .insert(name.clone(), expression::ValueType::Unsigned)
            .is_some()
        {
            return Err(SizingError::declaration(format!(
                "symbol {name:?} collides with a configuration parameter"
            )));
        }
    }
    let derived_parameters = compile_derived_parameters(source, schema, &symbols)?;
    let estimates = source
        .estimates
        .iter()
        .map(|(resource, estimate)| {
            if !valid_name(resource) {
                return Err(SizingError::declaration(format!(
                    "invalid resource name {resource:?}"
                )));
            }
            if !valid_name(&estimate.unit) {
                return Err(SizingError::declaration(format!(
                    "estimate {resource:?} has an invalid unit"
                )));
            }
            let available_when = expression::compile(
                estimate.available_when.as_deref().unwrap_or("true"),
                &symbols,
                expression::ValueType::Boolean,
            )
            .map_err(SizingError::InvalidExpression)?;
            let expression = expression::compile(
                &estimate.equation,
                &symbols,
                expression::ValueType::Unsigned,
            )
            .map_err(SizingError::InvalidExpression)?;
            let allowance = estimate
                .allowance
                .as_deref()
                .map(|allowance| {
                    expression::compile(allowance, &symbols, expression::ValueType::Unsigned)
                        .map_err(SizingError::InvalidExpression)
                })
                .transpose()?;
            // A constant in both parts would make the fitted equation depend on the allowance,
            // which fits treat as absent.
            if let Some(allowance) = &allowance
                && let Some(shared) = source.parameters.keys().find(|name| {
                    let name = BTreeSet::from([(*name).clone()]);
                    expression.references_any(&name) && allowance.references_any(&name)
                })
            {
                return Err(SizingError::declaration(format!(
                    "estimate {resource:?} uses parameter {shared:?} in both its equation and its allowance"
                )));
            }
            Ok(CompiledEstimate {
                resource: resource.clone(),
                unit: estimate.unit.clone(),
                available_when,
                expression,
                allowance,
            })
        })
        .collect::<Result<Vec<_>, SizingError>>()?;

    Ok(CompiledSizing {
        parameters: source.parameters.clone(),
        binding_parameters: source.binding_parameters.clone(),
        derived_parameters,
        estimates,
    })
}

/// Validates every configuration-local compatibility predicate against its option schema.
pub(super) fn validate_compatibility(
    configuration: &ModelEngineConfiguration,
) -> Result<(), SizingError> {
    let symbols = option_symbols(configuration)?;
    for option in &configuration.options {
        if !option.compatible_when.is_empty() {
            expression::compile(
                &option.compatible_when,
                &symbols,
                expression::ValueType::Boolean,
            )
            .map_err(SizingError::InvalidExpression)?;
        }
    }
    Ok(())
}

/// Evaluates every configuration-local compatibility predicate against complete values.
pub(super) fn compatible(
    configuration: &ModelEngineConfiguration,
    option_values: &[ModelOptionValues],
) -> Result<bool, SizingError> {
    let symbols = option_symbols(configuration)?;
    let mut values = BTreeMap::new();
    insert_option_values(&mut values, option_values)?;
    for option in &configuration.options {
        if !option.compatible_when.is_empty()
            && !expression::compile(
                &option.compatible_when,
                &symbols,
                expression::ValueType::Boolean,
            )
            .map_err(SizingError::InvalidExpression)?
            .evaluate_boolean(&values)
            .map_err(SizingError::Arithmetic)?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// A sizing equation that is linear in the parameters being fitted.
///
/// Each observation point yields a constant and one coefficient per unknown, so
/// the equation value is `constant + sum(coefficient * unknown)`. A model with no unknown
/// evaluates the stored constants as they are.
#[derive(Clone, Debug)]
pub struct SizingFitModel {
    expression: expression::CompiledExpression,
    allowance: Option<FitAllowance>,
    binding_constants: BTreeMap<String, u64>,
    available_when: Option<expression::CompiledExpression>,
    group_symbols: BTreeSet<String>,
    terms: Vec<expression::LinearTerm>,
    known: BTreeMap<String, u64>,
    unknowns: Vec<String>,
    extra_symbols: Vec<String>,
}

impl SizingFitModel {
    fn new(
        expression: expression::CompiledExpression,
        allowance: Option<FitAllowance>,
        binding_constants: BTreeMap<String, u64>,
        available_when: Option<expression::CompiledExpression>,
        known: BTreeMap<String, u64>,
        unknowns: &[String],
        extra_symbols: Vec<String>,
    ) -> Result<Self, SizingError> {
        let unknown_set = unknowns.iter().cloned().collect::<BTreeSet<_>>();
        if unknown_set.len() != unknowns.len() {
            return Err(SizingError::declaration("fit parameters must be distinct"));
        }
        if let Some(unused) = unknowns
            .iter()
            .find(|name| !expression.references_any(&BTreeSet::from([(*name).clone()])))
        {
            return Err(SizingError::declaration(format!(
                "fit parameter {unused:?} does not appear in the equation"
            )));
        }
        if available_when
            .as_ref()
            .is_some_and(|predicate| predicate.references_any(&unknown_set))
        {
            return Err(SizingError::declaration(
                "availableWhen must not depend on a fitted parameter",
            ));
        }
        let terms = expression
            .linear_terms(&unknown_set)
            .map_err(SizingError::InvalidExpression)?;
        Ok(Self {
            group_symbols: expression.categorical_condition_symbols(),
            expression,
            allowance,
            binding_constants,
            available_when,
            terms,
            known,
            unknowns: unknowns.to_vec(),
            extra_symbols,
        })
    }

    /// Fitted parameter names, in the order of every point's coefficients.
    pub fn unknowns(&self) -> &[String] {
        &self.unknowns
    }

    /// Binding constants the estimate's allowance uses, such as its margin, in name order.
    pub fn allowance_parameters(&self) -> &[String] {
        self.allowance
            .as_ref()
            .map_or(&[], |allowance| allowance.parameters.as_slice())
    }

    /// Every constant the binding stores, at its stored value; empty for an ad hoc equation.
    pub fn binding_constants(&self) -> &BTreeMap<String, u64> {
        &self.binding_constants
    }

    /// The option choices that select which terms of the equation apply to a point, keyed by
    /// option and parameter id (`kv-cache.precision`). Points sharing every choice form one
    /// group; numeric options the equation reads as quantities form none.
    pub fn group(
        &self,
        option_values: &[ModelOptionValues],
    ) -> Result<BTreeMap<String, String>, SizingError> {
        let mut values = BTreeMap::new();
        insert_option_values(&mut values, option_values)?;
        self.group_symbols
            .iter()
            .map(|symbol| {
                let label = symbol
                    .strip_prefix(&format!("{OPTION_SYMBOL_PREFIX}."))
                    .unwrap_or(symbol)
                    .to_owned();
                match values.get(symbol) {
                    Some(expression::Value::Boolean(value)) => Ok((label, value.to_string())),
                    Some(expression::Value::String(value)) => Ok((label, value.clone())),
                    Some(expression::Value::Unsigned(value)) => Ok((label, value.to_string())),
                    None => Err(SizingError::evaluation(format!(
                        "point has no value for {symbol:?}"
                    ))),
                }
            })
            .collect()
    }

    /// Evaluates one point's constant and coefficients, or none when the estimate is unavailable.
    pub fn point(
        &self,
        engine_values: &ParameterValues,
        option_values: &[ModelOptionValues],
        extra_values: &BTreeMap<String, u64>,
    ) -> Result<Option<SizingFitPoint>, SizingError> {
        let values = self.values(engine_values, option_values, extra_values, None)?;
        if let Some(predicate) = &self.available_when
            && !predicate
                .evaluate_boolean(&values)
                .map_err(SizingError::Arithmetic)?
        {
            return Ok(None);
        }
        let mut point = SizingFitPoint {
            constant: 0,
            coefficients: vec![0; self.unknowns.len()],
        };
        for term in &self.terms {
            let coefficient = term.coefficient(&values).map_err(SizingError::Arithmetic)?;
            let slot = match term.unknown() {
                Some(unknown) => {
                    let index = self
                        .unknowns
                        .iter()
                        .position(|name| name == unknown)
                        .expect("linear terms only name fitted parameters");
                    &mut point.coefficients[index]
                }
                None => &mut point.constant,
            };
            *slot = slot
                .checked_add(coefficient)
                .ok_or_else(|| SizingError::evaluation("fit coefficient overflow"))?;
        }
        Ok(Some(point))
    }

    /// Evaluates the equation exactly with fitted parameter values.
    pub fn predict(
        &self,
        parameters: &BTreeMap<String, u64>,
        engine_values: &ParameterValues,
        option_values: &[ModelOptionValues],
        extra_values: &BTreeMap<String, u64>,
    ) -> Result<u64, SizingError> {
        let values = self.values(engine_values, option_values, extra_values, Some(parameters))?;
        self.expression
            .evaluate_unsigned(&values)
            .map_err(SizingError::Arithmetic)
    }

    /// Evaluates the estimate's allowance, zero when it declares none. Fitted parameters never
    /// appear in an allowance, so it needs only the point's values.
    pub fn allowance(
        &self,
        engine_values: &ParameterValues,
        option_values: &[ModelOptionValues],
        extra_values: &BTreeMap<String, u64>,
    ) -> Result<u64, SizingError> {
        let values = self.values(engine_values, option_values, extra_values, None)?;
        evaluate_allowance(
            self.allowance
                .as_ref()
                .map(|allowance| &allowance.expression),
            &values,
        )
    }

    fn values(
        &self,
        engine_values: &ParameterValues,
        option_values: &[ModelOptionValues],
        extra_values: &BTreeMap<String, u64>,
        parameters: Option<&BTreeMap<String, u64>>,
    ) -> Result<BTreeMap<String, expression::Value>, SizingError> {
        let mut values = self
            .known
            .iter()
            .map(|(name, value)| (name.clone(), expression::Value::Unsigned(*value)))
            .collect::<BTreeMap<_, _>>();
        insert_engine_values(&mut values, engine_values)?;
        insert_option_values(&mut values, option_values)?;
        for (name, value) in extra_values {
            if self.extra_symbols.contains(name) {
                insert_value(
                    &mut values,
                    name.clone(),
                    expression::Value::Unsigned(*value),
                )?;
            }
        }
        if let Some(parameters) = parameters {
            for unknown in &self.unknowns {
                let value = parameters.get(unknown).ok_or_else(|| {
                    SizingError::evaluation(format!("fitted parameter {unknown:?} has no value"))
                })?;
                insert_value(
                    &mut values,
                    unknown.clone(),
                    expression::Value::Unsigned(*value),
                )?;
            }
        }
        Ok(values)
    }
}

/// An estimate's allowance as a fit model carries it: the expression and the binding constants it
/// uses.
#[derive(Clone, Debug)]
struct FitAllowance {
    expression: expression::CompiledExpression,
    parameters: Vec<String>,
}

/// One observation's equation constant and per-parameter coefficients.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SizingFitPoint {
    pub constant: u128,
    pub coefficients: Vec<u128>,
}

/// Builds a fit model for an ad hoc equation over the configuration's symbols, the binding's
/// declared parameters, the fitted parameters, and extra per-point symbols.
pub(super) fn inline_fit_model(
    equation: &str,
    schema: &EngineParameterSchema,
    configuration: &ModelEngineConfiguration,
    known: BTreeMap<String, u64>,
    unknowns: &[String],
    extra_symbols: &[String],
) -> Result<SizingFitModel, SizingError> {
    let mut symbols = configuration_symbols(schema, configuration)?;
    for name in known
        .keys()
        .chain(unknowns.iter().filter(|name| !known.contains_key(*name)))
        .chain(extra_symbols)
    {
        if symbols
            .insert(name.clone(), expression::ValueType::Unsigned)
            .is_some()
        {
            return Err(SizingError::declaration(format!(
                "symbol {name:?} collides with a configuration symbol"
            )));
        }
    }
    let known = known
        .into_iter()
        .filter(|(name, _)| !unknowns.contains(name))
        .collect();
    let expression = expression::compile(equation, &symbols, expression::ValueType::Unsigned)
        .map_err(SizingError::InvalidExpression)?;
    if unknowns.is_empty() {
        return Err(SizingError::declaration(
            "fit requires at least one parameter",
        ));
    }
    SizingFitModel::new(
        expression,
        None,
        BTreeMap::new(),
        None,
        known,
        unknowns,
        extra_symbols.to_vec(),
    )
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SizingError {
    #[error("Catalog sizing declaration: {0}")]
    Declaration(String),

    #[error("Catalog sizing evaluation: {0}")]
    Evaluation(String),

    #[error("Catalog sizing expression is invalid: {0}")]
    InvalidExpression(#[source] expression::ExpressionError),

    #[error("Catalog sizing arithmetic failed: {0}")]
    Arithmetic(#[source] expression::ExpressionError),
}

impl SizingError {
    fn declaration(message: impl Into<String>) -> Self {
        Self::Declaration(message.into())
    }

    pub(super) fn evaluation(message: impl Into<String>) -> Self {
        Self::Evaluation(message.into())
    }
}

/// Compiles derivations whose targets are unsigned engine parameters. A derivation may not
/// reference any derived parameter, so evaluation order never matters.
fn compile_derived_parameters(
    source: &SizingSource,
    schema: &EngineParameterSchema,
    symbols: &BTreeMap<String, expression::ValueType>,
) -> Result<Vec<CompiledDerivedParameter>, SizingError> {
    let engine_symbols = unsigned_engine_symbols(schema);
    let derived_ids = source
        .derived_parameters
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    source
        .derived_parameters
        .iter()
        .map(|(parameter_id, equation)| {
            if !engine_symbols.contains_key(parameter_id) {
                return Err(SizingError::declaration(format!(
                    "derived parameter {parameter_id:?} is not an unsigned engine parameter"
                )));
            }
            let expression =
                expression::compile(equation, symbols, expression::ValueType::Unsigned)
                    .map_err(SizingError::InvalidExpression)?;
            if expression.references_any(&derived_ids) {
                return Err(SizingError::declaration(format!(
                    "derived parameter {parameter_id:?} must not reference a derived parameter"
                )));
            }
            Ok(CompiledDerivedParameter {
                parameter_id: parameter_id.clone(),
                expression,
            })
        })
        .collect()
}

fn evaluate_allowance(
    allowance: Option<&expression::CompiledExpression>,
    values: &BTreeMap<String, expression::Value>,
) -> Result<u64, SizingError> {
    allowance.map_or(Ok(0), |allowance| {
        allowance
            .evaluate_unsigned(values)
            .map_err(SizingError::Arithmetic)
    })
}

fn configuration_symbols(
    schema: &EngineParameterSchema,
    configuration: &ModelEngineConfiguration,
) -> Result<BTreeMap<String, expression::ValueType>, SizingError> {
    let mut symbols = unsigned_engine_symbols(schema);
    for (name, value_type) in option_symbols(configuration)? {
        if symbols.insert(name.clone(), value_type).is_some() {
            return Err(SizingError::declaration(format!(
                "duplicate configuration symbol {name:?}"
            )));
        }
    }
    Ok(symbols)
}

fn unsigned_engine_symbols(
    schema: &EngineParameterSchema,
) -> BTreeMap<String, expression::ValueType> {
    schema
        .parameters
        .iter()
        .filter(|parameter| {
            matches!(
                parameter.parameter_type.as_ref(),
                Some(EngineParameterType::UnsignedInteger(_))
            )
        })
        .map(|parameter| {
            (
                parameter.parameter_id.clone(),
                expression::ValueType::Unsigned,
            )
        })
        .collect()
}

fn option_symbols(
    configuration: &ModelEngineConfiguration,
) -> Result<BTreeMap<String, expression::ValueType>, SizingError> {
    let mut symbols = BTreeMap::new();
    for option in &configuration.options {
        for parameter in &option.parameters {
            let value_type = match parameter.parameter_type.as_ref() {
                Some(OptionParameterType::Boolean(_)) => expression::ValueType::Boolean,
                Some(OptionParameterType::UnsignedInteger(_)) => expression::ValueType::Unsigned,
                Some(OptionParameterType::StringEnum(_)) => expression::ValueType::String,
                None => {
                    return Err(SizingError::declaration(format!(
                        "option parameter {:?}.{:?} has no type",
                        option.option_id, parameter.parameter_id
                    )));
                }
            };
            let name = option_symbol(&option.option_id, &parameter.parameter_id);
            if symbols.insert(name.clone(), value_type).is_some() {
                return Err(SizingError::declaration(format!(
                    "duplicate option symbol {name:?}"
                )));
            }
        }
    }
    Ok(symbols)
}

fn insert_engine_values(
    symbols: &mut BTreeMap<String, expression::Value>,
    values: &ParameterValues,
) -> Result<(), SizingError> {
    for parameter in &values.values {
        match parameter.value.as_ref() {
            Some(EngineParameterValueKind::UnsignedInteger(value)) => insert_value(
                symbols,
                parameter.parameter_id.clone(),
                expression::Value::Unsigned(*value),
            )?,
            // Sizing references integers only.
            Some(EngineParameterValueKind::Boolean(_) | EngineParameterValueKind::String(_)) => {}
            None => {
                return Err(SizingError::evaluation(format!(
                    "Engine parameter {:?} has no value",
                    parameter.parameter_id
                )));
            }
        }
    }
    Ok(())
}

fn insert_option_values(
    symbols: &mut BTreeMap<String, expression::Value>,
    values: &[ModelOptionValues],
) -> Result<(), SizingError> {
    for option in values {
        for parameter in &option.values {
            let name = option_symbol(&option.option_id, &parameter.parameter_id);
            insert_value(symbols, name, option_value(parameter)?)?;
        }
    }
    Ok(())
}

fn option_value(parameter: &ModelOptionParameterValue) -> Result<expression::Value, SizingError> {
    match parameter.value.as_ref() {
        Some(OptionParameterValue::Boolean(value)) => Ok(expression::Value::Boolean(*value)),
        Some(OptionParameterValue::UnsignedInteger(value)) => {
            Ok(expression::Value::Unsigned(*value))
        }
        Some(OptionParameterValue::String(value)) => Ok(expression::Value::String(value.clone())),
        None => Err(SizingError::evaluation(format!(
            "option parameter {:?} has no value",
            parameter.parameter_id
        ))),
    }
}

fn insert_value(
    symbols: &mut BTreeMap<String, expression::Value>,
    name: String,
    value: expression::Value,
) -> Result<(), SizingError> {
    if symbols.insert(name.clone(), value).is_some() {
        return Err(SizingError::evaluation(format!(
            "duplicate or colliding value for symbol {name:?}"
        )));
    }
    Ok(())
}

fn option_symbol(option_id: &str, parameter_id: &str) -> String {
    format!("{OPTION_SYMBOL_PREFIX}.{option_id}.{parameter_id}")
}

fn valid_name(value: &str) -> bool {
    (1..=MAX_NAME_BYTES).contains(&value.len())
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use dotcore_proto::dotcore::catalog::v1::{
        BooleanModelOptionParameterSchema, BooleanParameterSchema, EngineParameterSchema,
        ModelConfigurationOption, ModelEngineConfiguration, ModelOptionParameterDefinition,
        ModelOptionParameterValue, ModelOptionValues, ParameterDefinition, ParameterValue,
        ParameterValues, ParameterVisibility, StringEnumModelOptionParameterSchema,
        UnsignedIntegerModelOptionParameterSchema, UnsignedIntegerParameterSchema,
        model_option_parameter_definition::ParameterType as OptionParameterType,
        model_option_parameter_value::Value as OptionParameterValueKind,
        parameter_definition::ParameterType as EngineParameterType,
        parameter_value::Value as EngineParameterValueKind,
    };

    use super::{EstimateSource, ScalarResourceEstimate, SizingSource, compile};

    fn schema() -> EngineParameterSchema {
        EngineParameterSchema {
            schema_id: "test-schema".to_owned(),
            parameters: vec![ParameterDefinition {
                parameter_id: "total-context".to_owned(),
                display_name: "Total context".to_owned(),
                parameter_type: Some(EngineParameterType::UnsignedInteger(
                    UnsignedIntegerParameterSchema {
                        minimum: 1,
                        maximum: 131_072,
                        unit: "tokens".to_owned(),
                        default_value: None,
                    },
                )),
                visibility: ParameterVisibility::Basic as i32,
                description: "Maximum context.".to_owned(),
            }],
        }
    }

    fn configuration() -> ModelEngineConfiguration {
        ModelEngineConfiguration {
            options: vec![ModelConfigurationOption {
                option_id: "vision".to_owned(),
                parameters: vec![
                    ModelOptionParameterDefinition {
                        parameter_id: "enabled".to_owned(),
                        display_name: "Enabled".to_owned(),
                        parameter_type: Some(OptionParameterType::Boolean(
                            BooleanModelOptionParameterSchema {},
                        )),
                    },
                    ModelOptionParameterDefinition {
                        parameter_id: "token-budget".to_owned(),
                        display_name: "Token budget".to_owned(),
                        parameter_type: Some(OptionParameterType::UnsignedInteger(
                            UnsignedIntegerModelOptionParameterSchema {
                                minimum: 0,
                                maximum: 4_096,
                                unit: "tokens".to_owned(),
                            },
                        )),
                    },
                    ModelOptionParameterDefinition {
                        parameter_id: "projector".to_owned(),
                        display_name: "Projector".to_owned(),
                        parameter_type: Some(OptionParameterType::StringEnum(
                            StringEnumModelOptionParameterSchema {
                                values: vec!["bf16".to_owned()],
                            },
                        )),
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn source(equation: &str, available_when: Option<&str>) -> SizingSource {
        SizingSource {
            parameters: BTreeMap::from([
                ("baseVramBytes".to_owned(), 100),
                ("vramBytesPerContextToken".to_owned(), 3),
                ("marginBytes".to_owned(), 5),
            ]),
            binding_parameters: BTreeSet::from([
                "vramBytesPerContextToken".to_owned(),
                "marginBytes".to_owned(),
            ]),
            derived_parameters: BTreeMap::new(),
            estimates: BTreeMap::from([(
                "acceleratorMemory".to_owned(),
                EstimateSource {
                    unit: "bytes".to_owned(),
                    equation: equation.to_owned(),
                    allowance: None,
                    available_when: available_when.map(str::to_owned),
                },
            )]),
        }
    }

    fn engine_values(total_context: u64) -> ParameterValues {
        ParameterValues {
            values: vec![ParameterValue {
                parameter_id: "total-context".to_owned(),
                value: Some(EngineParameterValueKind::UnsignedInteger(total_context)),
            }],
        }
    }

    fn engine_values_with_boolean(total_context: u64, enabled: bool) -> ParameterValues {
        let mut values = engine_values(total_context);
        values.values.push(ParameterValue {
            parameter_id: "feature-enabled".to_owned(),
            value: Some(EngineParameterValueKind::Boolean(enabled)),
        });
        values
    }

    fn option_values(enabled: bool, token_budget: u64) -> Vec<ModelOptionValues> {
        vec![ModelOptionValues {
            option_id: "vision".to_owned(),
            values: vec![
                ModelOptionParameterValue {
                    parameter_id: "enabled".to_owned(),
                    value: Some(OptionParameterValueKind::Boolean(enabled)),
                },
                ModelOptionParameterValue {
                    parameter_id: "token-budget".to_owned(),
                    value: Some(OptionParameterValueKind::UnsignedInteger(token_budget)),
                },
                ModelOptionParameterValue {
                    parameter_id: "projector".to_owned(),
                    value: Some(OptionParameterValueKind::String("bf16".to_owned())),
                },
            ],
        }]
    }

    #[test]
    fn compiles_and_evaluates_sorted_scalar_results() {
        let mut declaration = source(
            "${baseVramBytes} + ${vramBytesPerContextToken} * ${total-context}",
            None,
        );
        declaration.estimates.insert(
            "hostMemory".to_owned(),
            EstimateSource {
                unit: "bytes".to_owned(),
                equation: "${baseVramBytes}".to_owned(),
                allowance: None,
                available_when: None,
            },
        );
        let sizing = compile(&declaration, &schema(), &configuration()).expect("sizing compiles");

        assert_eq!(
            sizing
                .evaluate(&engine_values(4), &option_values(false, 0))
                .expect("minimum evaluates"),
            vec![
                ScalarResourceEstimate {
                    resource: "acceleratorMemory".to_owned(),
                    value: 112,
                    unit: "bytes".to_owned(),
                },
                ScalarResourceEstimate {
                    resource: "hostMemory".to_owned(),
                    value: 100,
                    unit: "bytes".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn estimates_add_their_allowance_to_the_equation() {
        let sizing = compile(
            &allowance_source(
                "${baseVramBytes} + ${vramBytesPerContextToken} * ${total-context}",
                "${marginBytes} + 2 * ${total-context}",
                Some("${options.vision.enabled}"),
            ),
            &schema(),
            &configuration(),
        )
        .expect("sizing compiles");

        assert_eq!(
            sizing
                .evaluate(&engine_values(4), &option_values(true, 0))
                .expect("estimate evaluates"),
            vec![ScalarResourceEstimate {
                resource: "acceleratorMemory".to_owned(),
                value: 125,
                unit: "bytes".to_owned(),
            }]
        );
        assert_eq!(
            sizing
                .evaluate(&engine_values(4), &option_values(false, 0))
                .expect("availability gates the whole estimate"),
            Vec::new()
        );
    }

    #[test]
    fn fits_solve_the_equation_and_carry_the_allowance() {
        let sizing = compile(
            &allowance_source(
                "${baseVramBytes} + ${vramBytesPerContextToken} * ${total-context}",
                "${marginBytes} + 2 * ${total-context}",
                None,
            ),
            &schema(),
            &configuration(),
        )
        .expect("sizing compiles");

        let fit = sizing
            .fit_model("acceleratorMemory", &[])
            .expect("default fit model");

        assert_eq!(fit.unknowns(), ["vramBytesPerContextToken"]);
        assert_eq!(
            fit.point(&engine_values(10), &[], &BTreeMap::new())
                .expect("fit point"),
            Some(super::SizingFitPoint {
                constant: 100,
                coefficients: vec![10],
            })
        );
        assert_eq!(
            fit.allowance(&engine_values(10), &[], &BTreeMap::new())
                .expect("allowance evaluates"),
            25
        );
        assert_eq!(
            sizing
                .fit_model("acceleratorMemory", &["marginBytes".to_owned()])
                .expect_err("allowance parameters are never fitted")
                .to_string(),
            "Catalog sizing declaration: fit parameter \"marginBytes\" does not appear in the equation"
        );
    }

    #[test]
    fn check_models_evaluate_stored_constants_without_unknowns() {
        let sizing = compile(
            &allowance_source(
                "${baseVramBytes} + ${vramBytesPerContextToken} * ${total-context}",
                "${marginBytes}",
                None,
            ),
            &schema(),
            &configuration(),
        )
        .expect("sizing compiles");

        let check = sizing
            .check_model("acceleratorMemory")
            .expect("check model");

        assert_eq!(check.unknowns(), Vec::<String>::new());
        assert_eq!(check.allowance_parameters(), ["marginBytes"]);
        assert_eq!(
            check.binding_constants(),
            &BTreeMap::from([
                ("marginBytes".to_owned(), 5),
                ("vramBytesPerContextToken".to_owned(), 3),
            ])
        );
        assert_eq!(
            check
                .predict(&BTreeMap::new(), &engine_values(10), &[], &BTreeMap::new())
                .expect("stored prediction"),
            130
        );
        assert_eq!(
            check
                .allowance(&engine_values(10), &[], &BTreeMap::new())
                .expect("stored allowance"),
            5
        );
        assert_eq!(
            sizing
                .fit_model("acceleratorMemory", &[])
                .and_then(|_| compile(
                    &allowance_source("${baseVramBytes}", "${marginBytes}", None),
                    &schema(),
                    &configuration(),
                )?
                .fit_model("acceleratorMemory", &[]))
                .expect_err("an equation without binding constants has nothing to fit")
                .to_string(),
            "Catalog sizing declaration: fit requires at least one parameter"
        );
    }

    #[test]
    fn fit_groups_key_points_by_the_choices_the_equation_branches_on() {
        let sizing = compile(
            &source(
                r#"if(${options.vision.enabled} && ${options.vision.projector} == "bf16", 1, 0) * ${vramBytesPerContextToken} * ${total-context} + if(${options.vision.token-budget} > 0, 1, 0) * ${options.vision.token-budget}"#,
                None,
            ),
            &schema(),
            &configuration(),
        )
        .expect("sizing compiles");
        let fit = sizing
            .fit_model("acceleratorMemory", &[])
            .expect("fit model");

        assert_eq!(
            fit.group(&option_values(true, 1_024)).expect("group"),
            BTreeMap::from([
                ("vision.enabled".to_owned(), "true".to_owned()),
                ("vision.projector".to_owned(), "bf16".to_owned()),
            ])
        );
        assert_eq!(
            fit.group(&[])
                .expect_err("missing option value")
                .to_string(),
            "Catalog sizing evaluation: point has no value for \"options.vision.enabled\""
        );
    }

    #[test]
    fn rejects_invalid_allowances() {
        for (allowance, expected) in [
            (
                "${baseVramBytes} + ${marginBytes}",
                "Catalog sizing declaration: estimate \"acceleratorMemory\" uses parameter \"baseVramBytes\" in both its equation and its allowance",
            ),
            (
                "${unknown}",
                "Catalog sizing expression is invalid: sizing expression at byte 0: unknown symbol \"unknown\"",
            ),
            (
                "${options.vision.enabled}",
                "Catalog sizing expression is invalid: sizing expression at byte 0: expected unsigned integer result, found boolean",
            ),
        ] {
            assert_eq!(
                compile(
                    &allowance_source("${baseVramBytes}", allowance, None),
                    &schema(),
                    &configuration()
                )
                .expect_err("invalid allowance must reject")
                .to_string(),
                expected,
                "{allowance}"
            );
        }
    }

    #[test]
    fn an_allowance_overflowing_the_estimate_fails() {
        let sizing = compile(
            &allowance_source("18446744073709551615", "${marginBytes}", None),
            &schema(),
            &configuration(),
        )
        .expect("sizing compiles");

        assert_eq!(
            sizing
                .evaluate(&engine_values(4), &option_values(false, 0))
                .expect_err("overflow must fail")
                .to_string(),
            "Catalog sizing evaluation: estimate \"acceleratorMemory\" overflows adding its allowance"
        );
    }

    #[test]
    fn default_fit_unknowns_are_the_binding_constants_the_estimate_uses() {
        let sizing = compile(
            &source(
                "${baseVramBytes} + ${vramBytesPerContextToken} * ${total-context}",
                None,
            ),
            &schema(),
            &configuration(),
        )
        .expect("sizing compiles");

        let default_fit = sizing
            .fit_model("acceleratorMemory", &[])
            .expect("default fit model");
        let explicit_fit = sizing
            .fit_model("acceleratorMemory", &["baseVramBytes".to_owned()])
            .expect("explicit fit model");

        assert_eq!(default_fit.unknowns(), ["vramBytesPerContextToken"]);
        assert_eq!(
            default_fit
                .point(&engine_values(10), &[], &BTreeMap::new())
                .expect("fit point"),
            Some(super::SizingFitPoint {
                constant: 100,
                coefficients: vec![10],
            })
        );
        assert_eq!(explicit_fit.unknowns(), ["baseVramBytes"]);
    }

    #[test]
    fn boolean_engine_values_do_not_break_numeric_sizing() {
        let sizing = compile(
            &source("${baseVramBytes} + ${total-context}", None),
            &schema(),
            &configuration(),
        )
        .expect("sizing compiles");

        assert_eq!(
            sizing
                .evaluate(
                    &engine_values_with_boolean(4, false),
                    &option_values(false, 0)
                )
                .expect("boolean value is carried outside numeric symbols"),
            vec![ScalarResourceEstimate {
                resource: "acceleratorMemory".to_owned(),
                value: 104,
                unit: "bytes".to_owned(),
            }]
        );
    }

    #[test]
    fn option_conditions_select_lazy_equations_and_unavailable_estimates() {
        let declaration = source(
            "if(${options.vision.enabled}, ${baseVramBytes} + ${options.vision.token-budget}, ${baseVramBytes})",
            Some(
                "${options.vision.projector} == \"bf16\" && ${options.vision.token-budget} <= 4096",
            ),
        );
        let sizing = compile(&declaration, &schema(), &configuration()).expect("sizing compiles");

        assert_eq!(
            sizing
                .evaluate(&engine_values(4), &option_values(true, 1_024))
                .expect("available estimate"),
            vec![ScalarResourceEstimate {
                resource: "acceleratorMemory".to_owned(),
                value: 1_124,
                unit: "bytes".to_owned(),
            }]
        );
        assert_eq!(
            sizing
                .evaluate(&engine_values(4), &option_values(false, 0))
                .expect("explicit false and zero remain available"),
            vec![ScalarResourceEstimate {
                resource: "acceleratorMemory".to_owned(),
                value: 100,
                unit: "bytes".to_owned(),
            }]
        );

        let unavailable = source(
            "18446744073709551615 * 18446744073709551615",
            Some("${options.vision.enabled}"),
        );
        assert_eq!(
            compile(&unavailable, &schema(), &configuration())
                .expect("sizing compiles")
                .evaluate(&engine_values(4), &option_values(false, 0))
                .expect("unavailable is not an error"),
            Vec::new()
        );

        let arithmetic_failure = source("18446744073709551615 * 18446744073709551615", None);
        assert!(
            compile(&arithmetic_failure, &schema(), &configuration())
                .expect("sizing compiles")
                .evaluate(&engine_values(4), &option_values(false, 0))
                .expect_err("available overflowing equation must fail")
                .to_string()
                .starts_with("Catalog sizing arithmetic failed:")
        );
    }

    #[test]
    fn rejects_unknown_and_wrongly_typed_option_symbols() {
        for (equation, expected) in [
            (
                "if(true, 1, ${options.vision.unknown})",
                "unknown symbol \"options.vision.unknown\"",
            ),
            (
                "${options.vision.enabled} + 1",
                "requires unsigned integer operands",
            ),
        ] {
            assert!(
                compile(&source(equation, None), &schema(), &configuration())
                    .expect_err("invalid expression")
                    .to_string()
                    .contains(expected)
            );
        }
    }

    #[test]
    fn rejects_parameter_collisions() {
        let mut declaration = source("${total-context}", None);
        declaration.parameters.insert("total-context".to_owned(), 1);

        assert_eq!(
            compile(&declaration, &schema(), &configuration())
                .expect_err("sizing must reject")
                .to_string(),
            "Catalog sizing declaration: symbol \"total-context\" collides with a configuration parameter"
        );
    }

    #[test]
    fn rejects_empty_estimates_and_invalid_flexible_names() {
        let mut no_estimates = source("${total-context}", None);
        no_estimates.estimates.clear();
        assert_eq!(
            compile(&no_estimates, &schema(), &configuration())
                .expect_err("empty estimates must reject")
                .to_string(),
            "Catalog sizing declaration: requires at least one estimate"
        );

        let mut invalid_unit = source("${total-context}", None);
        invalid_unit
            .estimates
            .get_mut("acceleratorMemory")
            .expect("estimate")
            .unit = " bytes ".to_owned();
        assert_eq!(
            compile(&invalid_unit, &schema(), &configuration())
                .expect_err("invalid unit must reject")
                .to_string(),
            "Catalog sizing declaration: estimate \"acceleratorMemory\" has an invalid unit"
        );
    }

    #[test]
    fn derived_parameters_fill_omitted_values_and_feed_estimates() {
        let sizing = compile(
            &derived_source("${cache-bytes} + 1"),
            &derived_schema(),
            &configuration(),
        )
        .expect("sizing compiles");

        let derived = sizing
            .derive(&engine_values(4), &option_values(false, 0))
            .expect("derivation evaluates");
        assert_eq!(derived, vec![("cache-bytes".to_owned(), 112)]);

        let mut complete = engine_values(4);
        complete.values.push(cache_bytes_value(112));
        assert_eq!(
            sizing
                .evaluate(&complete, &option_values(false, 0))
                .expect("estimate evaluates"),
            vec![ScalarResourceEstimate {
                resource: "acceleratorMemory".to_owned(),
                value: 113,
                unit: "bytes".to_owned(),
            }]
        );
    }

    #[test]
    fn supplied_values_take_precedence_over_derivations() {
        let sizing = compile(
            &derived_source("${cache-bytes} + 1"),
            &derived_schema(),
            &configuration(),
        )
        .expect("sizing compiles");
        let mut supplied = engine_values(4);
        supplied.values.push(cache_bytes_value(500));

        assert_eq!(
            sizing
                .derive(&supplied, &option_values(false, 0))
                .expect("derivation evaluates"),
            Vec::new()
        );
        assert_eq!(
            sizing
                .evaluate(&supplied, &option_values(false, 0))
                .expect("estimate evaluates")[0]
                .value,
            501
        );
    }

    #[test]
    fn rejects_invalid_derived_parameters() {
        for (parameter_id, equation, expected) in [
            (
                "cache-bytes",
                "${cache-bytes} + 1",
                "Catalog sizing declaration: derived parameter \"cache-bytes\" must not reference a derived parameter",
            ),
            (
                "feature-enabled",
                "1",
                "Catalog sizing declaration: derived parameter \"feature-enabled\" is not an unsigned engine parameter",
            ),
            (
                "unknown",
                "1",
                "Catalog sizing declaration: derived parameter \"unknown\" is not an unsigned engine parameter",
            ),
        ] {
            let mut declaration = source("${total-context}", None);
            declaration
                .derived_parameters
                .insert(parameter_id.to_owned(), equation.to_owned());

            assert_eq!(
                compile(&declaration, &derived_schema(), &configuration())
                    .expect_err("invalid derivation must reject")
                    .to_string(),
                expected,
                "{parameter_id}"
            );
        }

        let mut cross_reference = derived_source("${total-context}");
        cross_reference
            .derived_parameters
            .insert("total-context".to_owned(), "${cache-bytes}".to_owned());
        assert_eq!(
            compile(&cross_reference, &derived_schema(), &configuration())
                .expect_err("cross-referencing derivations must reject")
                .to_string(),
            "Catalog sizing declaration: derived parameter \"cache-bytes\" must not reference a derived parameter"
        );
    }

    fn allowance_source(
        equation: &str,
        allowance: &str,
        available_when: Option<&str>,
    ) -> SizingSource {
        let mut declaration = source(equation, available_when);
        declaration
            .estimates
            .get_mut("acceleratorMemory")
            .expect("estimate")
            .allowance = Some(allowance.to_owned());
        declaration
    }

    fn derived_schema() -> EngineParameterSchema {
        let mut schema = schema();
        schema.parameters.push(ParameterDefinition {
            parameter_id: "cache-bytes".to_owned(),
            display_name: "Cache bytes".to_owned(),
            parameter_type: Some(EngineParameterType::UnsignedInteger(
                UnsignedIntegerParameterSchema {
                    minimum: 1,
                    maximum: u64::MAX,
                    unit: "bytes".to_owned(),
                    default_value: None,
                },
            )),
            visibility: ParameterVisibility::Hidden as i32,
            description: "Cache size.".to_owned(),
        });
        schema.parameters.push(ParameterDefinition {
            parameter_id: "feature-enabled".to_owned(),
            display_name: "Feature".to_owned(),
            parameter_type: Some(EngineParameterType::Boolean(
                BooleanParameterSchema::default(),
            )),
            visibility: ParameterVisibility::Hidden as i32,
            description: "Feature toggle.".to_owned(),
        });
        schema
    }

    fn derived_source(equation: &str) -> SizingSource {
        let mut declaration = source(equation, None);
        declaration.derived_parameters.insert(
            "cache-bytes".to_owned(),
            "${baseVramBytes} + ${vramBytesPerContextToken} * ${total-context}".to_owned(),
        );
        declaration
    }

    fn cache_bytes_value(value: u64) -> ParameterValue {
        ParameterValue {
            parameter_id: "cache-bytes".to_owned(),
            value: Some(EngineParameterValueKind::UnsignedInteger(value)),
        }
    }
}
