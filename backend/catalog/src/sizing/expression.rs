//! Compiles and evaluates bounded typed sizing expressions.
//!
//! Expressions support checked unsigned arithmetic, typed option symbols,
//! boolean logic, comparisons, and lazy conditional branches. The byte unit
//! constants `KiB`, `MiB`, and `GiB` are unsigned literals. Compilation
//! validates every symbol and branch even when evaluation will not select it.

use std::collections::{BTreeMap, BTreeSet};

const MAX_EXPRESSION_BYTES: usize = 1024;
const MAX_TOKENS: usize = 128;
const MAX_SYMBOLS: usize = 64;
const MAX_SYMBOL_BYTES: usize = 191;
const MAX_STRING_BYTES: usize = 128;
const MAX_NESTING_DEPTH: usize = 8;
const MAX_AST_NODES: usize = 256;

/// Static expression and symbol types.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ValueType {
    Unsigned,
    Boolean,
    String,
}

impl ValueType {
    fn name(self) -> &'static str {
        match self {
            Self::Unsigned => "unsigned integer",
            Self::Boolean => "boolean",
            Self::String => "string",
        }
    }
}

/// One resolved typed expression symbol.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Value {
    Unsigned(u64),
    Boolean(bool),
    String(String),
}

impl Value {
    fn value_type(&self) -> ValueType {
        match self {
            Self::Unsigned(_) => ValueType::Unsigned,
            Self::Boolean(_) => ValueType::Boolean,
            Self::String(_) => ValueType::String,
        }
    }
}

/// A parsed expression that can be evaluated repeatedly against resolved values.
#[derive(Clone, Debug)]
pub(super) struct CompiledExpression {
    root: Expression,
}

impl CompiledExpression {
    /// Evaluates an unsigned expression with checked arithmetic.
    pub(super) fn evaluate_unsigned(
        &self,
        values: &BTreeMap<String, Value>,
    ) -> Result<u64, ExpressionError> {
        let evaluated = self.root.evaluate(values)?;
        let EvaluatedValue::Unsigned(value) = evaluated.value else {
            return Err(ExpressionError::at(
                evaluated.offset,
                "expression did not evaluate to an unsigned integer",
            ));
        };
        u64::try_from(value)
            .map_err(|_| ExpressionError::at(evaluated.offset, "final result exceeds u64"))
    }

    /// Splits an unsigned expression into terms that are linear in the unknown symbols.
    /// Each unknown may appear only as a factor of sums and products, at most once per
    /// product, and never under a condition or comparison.
    pub(super) fn linear_terms(
        &self,
        unknowns: &BTreeSet<String>,
    ) -> Result<Vec<LinearTerm>, ExpressionError> {
        self.root.linear_terms(unknowns)
    }

    /// Reports whether any of the symbols appears in the expression.
    pub(super) fn references_any(&self, symbols: &BTreeSet<String>) -> bool {
        self.root.references_any(symbols)
    }

    /// Boolean and string symbols read by any conditional branch's condition: the categorical
    /// choices that select which terms of the expression apply.
    pub(super) fn categorical_condition_symbols(&self) -> BTreeSet<String> {
        let mut symbols = BTreeSet::new();
        self.root.collect_condition_symbols(false, &mut symbols);
        symbols
    }

    /// Evaluates a boolean predicate.
    pub(super) fn evaluate_boolean(
        &self,
        values: &BTreeMap<String, Value>,
    ) -> Result<bool, ExpressionError> {
        let evaluated = self.root.evaluate(values)?;
        match evaluated.value {
            EvaluatedValue::Boolean(value) => Ok(value),
            _ => Err(ExpressionError::at(
                evaluated.offset,
                "expression did not evaluate to a boolean",
            )),
        }
    }
}

/// Compiles the bounded grammar against a complete typed symbol table.
pub(super) fn compile(
    source: &str,
    symbols: &BTreeMap<String, ValueType>,
    expected_type: ValueType,
) -> Result<CompiledExpression, ExpressionError> {
    validate_symbols(symbols)?;
    let tokens = lex(source)?;
    let mut parser = Parser::new(&tokens, symbols);
    let root = parser.parse()?;
    if root.value_type != expected_type {
        return Err(ExpressionError::at(
            root.offset,
            format!(
                "expected {} result, found {}",
                expected_type.name(),
                root.value_type.name()
            ),
        ));
    }
    Ok(CompiledExpression { root })
}

/// A deterministic source-oriented sizing expression failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("sizing expression at byte {offset}: {message}")]
pub struct ExpressionError {
    offset: usize,
    message: String,
}

impl ExpressionError {
    fn at(offset: usize, message: impl Into<String>) -> Self {
        Self {
            offset,
            message: message.into(),
        }
    }
}

/// One product term of a linear expression: an optional unknown times known factors.
#[derive(Clone, Debug)]
pub(super) struct LinearTerm {
    unknown: Option<String>,
    factors: Vec<Expression>,
}

impl LinearTerm {
    /// The unknown this term multiplies, or none for a constant term.
    pub(super) fn unknown(&self) -> Option<&str> {
        self.unknown.as_deref()
    }

    /// Evaluates the product of the known factors.
    pub(super) fn coefficient(
        &self,
        values: &BTreeMap<String, Value>,
    ) -> Result<u128, ExpressionError> {
        self.factors.iter().try_fold(1_u128, |product, factor| {
            product
                .checked_mul(factor.evaluate(values)?.unsigned()?)
                .ok_or_else(|| ExpressionError::at(factor.offset, "multiplication overflow"))
        })
    }
}

#[derive(Clone, Debug)]
struct Expression {
    kind: ExpressionKind,
    value_type: ValueType,
    offset: usize,
}

impl Expression {
    fn linear_terms(
        &self,
        unknowns: &BTreeSet<String>,
    ) -> Result<Vec<LinearTerm>, ExpressionError> {
        if !self.references_any(unknowns) {
            return Ok(vec![LinearTerm {
                unknown: None,
                factors: vec![self.clone()],
            }]);
        }
        match &self.kind {
            ExpressionKind::Symbol(name) => Ok(vec![LinearTerm {
                unknown: Some(name.clone()),
                factors: Vec::new(),
            }]),
            ExpressionKind::Add(left, right) => {
                let mut terms = left.linear_terms(unknowns)?;
                terms.extend(right.linear_terms(unknowns)?);
                Ok(terms)
            }
            ExpressionKind::Multiply(left, right) => {
                let left = left.linear_terms(unknowns)?;
                let right = right.linear_terms(unknowns)?;
                let mut terms = Vec::with_capacity(left.len() * right.len());
                for left in &left {
                    for right in &right {
                        let unknown = match (&left.unknown, &right.unknown) {
                            (Some(_), Some(_)) => {
                                return Err(ExpressionError::at(
                                    self.offset,
                                    "a product multiplies two fitted parameters",
                                ));
                            }
                            (unknown @ Some(_), None) | (None, unknown) => unknown.clone(),
                        };
                        terms.push(LinearTerm {
                            unknown,
                            factors: left.factors.iter().chain(&right.factors).cloned().collect(),
                        });
                    }
                }
                Ok(terms)
            }
            _ => Err(ExpressionError::at(
                self.offset,
                "a fitted parameter appears under a condition or comparison",
            )),
        }
    }

    fn references_any(&self, symbols: &BTreeSet<String>) -> bool {
        match &self.kind {
            ExpressionKind::UnsignedLiteral(_)
            | ExpressionKind::BooleanLiteral(_)
            | ExpressionKind::StringLiteral(_) => false,
            ExpressionKind::Symbol(name) => symbols.contains(name),
            ExpressionKind::Not(expression) => expression.references_any(symbols),
            ExpressionKind::Add(left, right)
            | ExpressionKind::Multiply(left, right)
            | ExpressionKind::And(left, right)
            | ExpressionKind::Or(left, right)
            | ExpressionKind::Equal(left, right)
            | ExpressionKind::NotEqual(left, right)
            | ExpressionKind::Less(left, right)
            | ExpressionKind::LessOrEqual(left, right)
            | ExpressionKind::Greater(left, right)
            | ExpressionKind::GreaterOrEqual(left, right) => {
                left.references_any(symbols) || right.references_any(symbols)
            }
            ExpressionKind::If {
                condition,
                when_true,
                when_false,
            } => {
                condition.references_any(symbols)
                    || when_true.references_any(symbols)
                    || when_false.references_any(symbols)
            }
        }
    }

    fn collect_condition_symbols(&self, in_condition: bool, symbols: &mut BTreeSet<String>) {
        match &self.kind {
            ExpressionKind::UnsignedLiteral(_)
            | ExpressionKind::BooleanLiteral(_)
            | ExpressionKind::StringLiteral(_) => {}
            ExpressionKind::Symbol(name) => {
                if in_condition && self.value_type != ValueType::Unsigned {
                    symbols.insert(name.clone());
                }
            }
            ExpressionKind::Not(expression) => {
                expression.collect_condition_symbols(in_condition, symbols);
            }
            ExpressionKind::Add(left, right)
            | ExpressionKind::Multiply(left, right)
            | ExpressionKind::And(left, right)
            | ExpressionKind::Or(left, right)
            | ExpressionKind::Equal(left, right)
            | ExpressionKind::NotEqual(left, right)
            | ExpressionKind::Less(left, right)
            | ExpressionKind::LessOrEqual(left, right)
            | ExpressionKind::Greater(left, right)
            | ExpressionKind::GreaterOrEqual(left, right) => {
                left.collect_condition_symbols(in_condition, symbols);
                right.collect_condition_symbols(in_condition, symbols);
            }
            ExpressionKind::If {
                condition,
                when_true,
                when_false,
            } => {
                condition.collect_condition_symbols(true, symbols);
                when_true.collect_condition_symbols(in_condition, symbols);
                when_false.collect_condition_symbols(in_condition, symbols);
            }
        }
    }

    fn evaluate(&self, values: &BTreeMap<String, Value>) -> Result<Evaluated, ExpressionError> {
        let value = match &self.kind {
            ExpressionKind::UnsignedLiteral(value) => EvaluatedValue::Unsigned(*value),
            ExpressionKind::BooleanLiteral(value) => EvaluatedValue::Boolean(*value),
            ExpressionKind::StringLiteral(value) => EvaluatedValue::String(value.clone()),
            ExpressionKind::Symbol(name) => {
                let value = values.get(name).ok_or_else(|| {
                    ExpressionError::at(self.offset, format!("missing value for symbol {name:?}"))
                })?;
                if value.value_type() != self.value_type {
                    return Err(ExpressionError::at(
                        self.offset,
                        format!("value for symbol {name:?} has the wrong type"),
                    ));
                }
                match value {
                    Value::Unsigned(value) => EvaluatedValue::Unsigned(u128::from(*value)),
                    Value::Boolean(value) => EvaluatedValue::Boolean(*value),
                    Value::String(value) => EvaluatedValue::String(value.clone()),
                }
            }
            ExpressionKind::Add(left, right) => EvaluatedValue::Unsigned(
                left.evaluate(values)?
                    .unsigned()?
                    .checked_add(right.evaluate(values)?.unsigned()?)
                    .ok_or_else(|| ExpressionError::at(self.offset, "addition overflow"))?,
            ),
            ExpressionKind::Multiply(left, right) => EvaluatedValue::Unsigned(
                left.evaluate(values)?
                    .unsigned()?
                    .checked_mul(right.evaluate(values)?.unsigned()?)
                    .ok_or_else(|| ExpressionError::at(self.offset, "multiplication overflow"))?,
            ),
            ExpressionKind::Not(expression) => {
                EvaluatedValue::Boolean(!expression.evaluate(values)?.boolean()?)
            }
            ExpressionKind::And(left, right) => {
                let left = left.evaluate(values)?.boolean()?;
                EvaluatedValue::Boolean(left && right.evaluate(values)?.boolean()?)
            }
            ExpressionKind::Or(left, right) => {
                let left = left.evaluate(values)?.boolean()?;
                EvaluatedValue::Boolean(left || right.evaluate(values)?.boolean()?)
            }
            ExpressionKind::Equal(left, right) => EvaluatedValue::Boolean(
                left.evaluate(values)?.value == right.evaluate(values)?.value,
            ),
            ExpressionKind::NotEqual(left, right) => EvaluatedValue::Boolean(
                left.evaluate(values)?.value != right.evaluate(values)?.value,
            ),
            ExpressionKind::Less(left, right) => EvaluatedValue::Boolean(
                left.evaluate(values)?.unsigned()? < right.evaluate(values)?.unsigned()?,
            ),
            ExpressionKind::LessOrEqual(left, right) => EvaluatedValue::Boolean(
                left.evaluate(values)?.unsigned()? <= right.evaluate(values)?.unsigned()?,
            ),
            ExpressionKind::Greater(left, right) => EvaluatedValue::Boolean(
                left.evaluate(values)?.unsigned()? > right.evaluate(values)?.unsigned()?,
            ),
            ExpressionKind::GreaterOrEqual(left, right) => EvaluatedValue::Boolean(
                left.evaluate(values)?.unsigned()? >= right.evaluate(values)?.unsigned()?,
            ),
            ExpressionKind::If {
                condition,
                when_true,
                when_false,
            } => {
                if condition.evaluate(values)?.boolean()? {
                    when_true.evaluate(values)?.value
                } else {
                    when_false.evaluate(values)?.value
                }
            }
        };
        Ok(Evaluated {
            value,
            offset: self.offset,
        })
    }
}

#[derive(Clone, Debug)]
enum ExpressionKind {
    UnsignedLiteral(u128),
    BooleanLiteral(bool),
    StringLiteral(String),
    Symbol(String),
    Add(Box<Expression>, Box<Expression>),
    Multiply(Box<Expression>, Box<Expression>),
    Not(Box<Expression>),
    And(Box<Expression>, Box<Expression>),
    Or(Box<Expression>, Box<Expression>),
    Equal(Box<Expression>, Box<Expression>),
    NotEqual(Box<Expression>, Box<Expression>),
    Less(Box<Expression>, Box<Expression>),
    LessOrEqual(Box<Expression>, Box<Expression>),
    Greater(Box<Expression>, Box<Expression>),
    GreaterOrEqual(Box<Expression>, Box<Expression>),
    If {
        condition: Box<Expression>,
        when_true: Box<Expression>,
        when_false: Box<Expression>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum EvaluatedValue {
    Unsigned(u128),
    Boolean(bool),
    String(String),
}

#[derive(Clone, Debug)]
struct Evaluated {
    value: EvaluatedValue,
    offset: usize,
}

impl Evaluated {
    fn unsigned(self) -> Result<u128, ExpressionError> {
        match self.value {
            EvaluatedValue::Unsigned(value) => Ok(value),
            _ => Err(ExpressionError::at(
                self.offset,
                "expected unsigned integer value",
            )),
        }
    }

    fn boolean(self) -> Result<bool, ExpressionError> {
        match self.value {
            EvaluatedValue::Boolean(value) => Ok(value),
            _ => Err(ExpressionError::at(self.offset, "expected boolean value")),
        }
    }
}

#[derive(Clone, Debug)]
struct Token {
    kind: TokenKind,
    offset: usize,
    end: usize,
}

#[derive(Clone, Debug)]
enum TokenKind {
    Symbol(String),
    UnsignedLiteral(u128),
    BooleanLiteral(bool),
    StringLiteral(String),
    If,
    Add,
    Multiply,
    Not,
    And,
    Or,
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    LeftParenthesis,
    RightParenthesis,
    Comma,
}

struct Parser<'a> {
    tokens: &'a [Token],
    symbols: &'a BTreeMap<String, ValueType>,
    index: usize,
    nodes: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token], symbols: &'a BTreeMap<String, ValueType>) -> Self {
        Self {
            tokens,
            symbols,
            index: 0,
            nodes: 0,
        }
    }

    fn parse(&mut self) -> Result<Expression, ExpressionError> {
        let expression = self.or_expression(0)?;
        if let Some(token) = self.tokens.get(self.index) {
            return Err(ExpressionError::at(
                token.offset,
                "expected end of expression",
            ));
        }
        Ok(expression)
    }

    fn or_expression(&mut self, depth: usize) -> Result<Expression, ExpressionError> {
        let mut expression = self.and_expression(depth)?;
        while self.take(|kind| matches!(kind, TokenKind::Or)).is_some() {
            let offset = self.tokens[self.index - 1].offset;
            let right = self.and_expression(depth)?;
            self.require_types(&expression, &right, ValueType::Boolean, offset, "'||'")?;
            expression = self.node(
                ExpressionKind::Or(Box::new(expression), Box::new(right)),
                ValueType::Boolean,
                offset,
            )?;
        }
        Ok(expression)
    }

    fn and_expression(&mut self, depth: usize) -> Result<Expression, ExpressionError> {
        let mut expression = self.equality(depth)?;
        while self.take(|kind| matches!(kind, TokenKind::And)).is_some() {
            let offset = self.tokens[self.index - 1].offset;
            let right = self.equality(depth)?;
            self.require_types(&expression, &right, ValueType::Boolean, offset, "'&&'")?;
            expression = self.node(
                ExpressionKind::And(Box::new(expression), Box::new(right)),
                ValueType::Boolean,
                offset,
            )?;
        }
        Ok(expression)
    }

    fn equality(&mut self, depth: usize) -> Result<Expression, ExpressionError> {
        let mut expression = self.comparison(depth)?;
        while let Some(token) = self.tokens.get(self.index) {
            let kind = match token.kind {
                TokenKind::Equal => 0,
                TokenKind::NotEqual => 1,
                _ => break,
            };
            let offset = token.offset;
            self.index += 1;
            let right = self.comparison(depth)?;
            if expression.value_type != right.value_type {
                return Err(ExpressionError::at(
                    offset,
                    "equality operands must have the same type",
                ));
            }
            let node = if kind == 0 {
                ExpressionKind::Equal(Box::new(expression), Box::new(right))
            } else {
                ExpressionKind::NotEqual(Box::new(expression), Box::new(right))
            };
            expression = self.node(node, ValueType::Boolean, offset)?;
        }
        Ok(expression)
    }

    fn comparison(&mut self, depth: usize) -> Result<Expression, ExpressionError> {
        let expression = self.sum(depth)?;
        let Some(token) = self.tokens.get(self.index) else {
            return Ok(expression);
        };
        let kind = match token.kind {
            TokenKind::Less => 0,
            TokenKind::LessOrEqual => 1,
            TokenKind::Greater => 2,
            TokenKind::GreaterOrEqual => 3,
            _ => return Ok(expression),
        };
        let offset = token.offset;
        self.index += 1;
        let right = self.sum(depth)?;
        self.require_types(
            &expression,
            &right,
            ValueType::Unsigned,
            offset,
            "comparison",
        )?;
        let node = match kind {
            0 => ExpressionKind::Less(Box::new(expression), Box::new(right)),
            1 => ExpressionKind::LessOrEqual(Box::new(expression), Box::new(right)),
            2 => ExpressionKind::Greater(Box::new(expression), Box::new(right)),
            _ => ExpressionKind::GreaterOrEqual(Box::new(expression), Box::new(right)),
        };
        self.node(node, ValueType::Boolean, offset)
    }

    fn sum(&mut self, depth: usize) -> Result<Expression, ExpressionError> {
        let mut expression = self.product(depth)?;
        while self.take(|kind| matches!(kind, TokenKind::Add)).is_some() {
            let offset = self.tokens[self.index - 1].offset;
            let right = self.product(depth)?;
            self.require_types(&expression, &right, ValueType::Unsigned, offset, "'+'")?;
            expression = self.node(
                ExpressionKind::Add(Box::new(expression), Box::new(right)),
                ValueType::Unsigned,
                offset,
            )?;
        }
        Ok(expression)
    }

    fn product(&mut self, depth: usize) -> Result<Expression, ExpressionError> {
        let mut expression = self.unary(depth)?;
        while self
            .take(|kind| matches!(kind, TokenKind::Multiply))
            .is_some()
        {
            let offset = self.tokens[self.index - 1].offset;
            let right = self.unary(depth)?;
            self.require_types(&expression, &right, ValueType::Unsigned, offset, "'*'")?;
            expression = self.node(
                ExpressionKind::Multiply(Box::new(expression), Box::new(right)),
                ValueType::Unsigned,
                offset,
            )?;
        }
        Ok(expression)
    }

    fn unary(&mut self, depth: usize) -> Result<Expression, ExpressionError> {
        if let Some(token) = self.take(|kind| matches!(kind, TokenKind::Not)) {
            let expression = self.unary(depth)?;
            if expression.value_type != ValueType::Boolean {
                return Err(ExpressionError::at(
                    token.offset,
                    "'!' requires a boolean operand",
                ));
            }
            return self.node(
                ExpressionKind::Not(Box::new(expression)),
                ValueType::Boolean,
                token.offset,
            );
        }
        self.primary(depth)
    }

    fn primary(&mut self, depth: usize) -> Result<Expression, ExpressionError> {
        let Some(token) = self.tokens.get(self.index).cloned() else {
            return Err(ExpressionError::at(
                self.end_offset(),
                "expected expression",
            ));
        };
        match token.kind {
            TokenKind::UnsignedLiteral(value) => {
                self.index += 1;
                self.node(
                    ExpressionKind::UnsignedLiteral(value),
                    ValueType::Unsigned,
                    token.offset,
                )
            }
            TokenKind::BooleanLiteral(value) => {
                self.index += 1;
                self.node(
                    ExpressionKind::BooleanLiteral(value),
                    ValueType::Boolean,
                    token.offset,
                )
            }
            TokenKind::StringLiteral(value) => {
                self.index += 1;
                self.node(
                    ExpressionKind::StringLiteral(value),
                    ValueType::String,
                    token.offset,
                )
            }
            TokenKind::Symbol(name) => {
                let value_type = *self.symbols.get(&name).ok_or_else(|| {
                    ExpressionError::at(token.offset, format!("unknown symbol {name:?}"))
                })?;
                self.index += 1;
                self.node(ExpressionKind::Symbol(name), value_type, token.offset)
            }
            TokenKind::If => self.if_expression(depth, token.offset),
            TokenKind::LeftParenthesis => {
                self.check_depth(depth, token.offset)?;
                self.index += 1;
                let expression = self.or_expression(depth + 1)?;
                self.expect(
                    |kind| matches!(kind, TokenKind::RightParenthesis),
                    "expected ')'",
                )?;
                Ok(expression)
            }
            _ => Err(ExpressionError::at(token.offset, "expected expression")),
        }
    }

    fn if_expression(
        &mut self,
        depth: usize,
        offset: usize,
    ) -> Result<Expression, ExpressionError> {
        self.check_depth(depth, offset)?;
        self.index += 1;
        self.expect(
            |kind| matches!(kind, TokenKind::LeftParenthesis),
            "expected '(' after 'if'",
        )?;
        let condition = self.or_expression(depth + 1)?;
        self.expect(|kind| matches!(kind, TokenKind::Comma), "expected ','")?;
        let when_true = self.or_expression(depth + 1)?;
        self.expect(|kind| matches!(kind, TokenKind::Comma), "expected ','")?;
        let when_false = self.or_expression(depth + 1)?;
        self.expect(
            |kind| matches!(kind, TokenKind::RightParenthesis),
            "expected ')'",
        )?;
        if condition.value_type != ValueType::Boolean {
            return Err(ExpressionError::at(
                condition.offset,
                "if condition must be boolean",
            ));
        }
        if when_true.value_type != when_false.value_type {
            return Err(ExpressionError::at(
                offset,
                "if branches must have the same type",
            ));
        }
        let value_type = when_true.value_type;
        self.node(
            ExpressionKind::If {
                condition: Box::new(condition),
                when_true: Box::new(when_true),
                when_false: Box::new(when_false),
            },
            value_type,
            offset,
        )
    }

    fn require_types(
        &self,
        left: &Expression,
        right: &Expression,
        expected: ValueType,
        offset: usize,
        operator: &str,
    ) -> Result<(), ExpressionError> {
        if left.value_type != expected || right.value_type != expected {
            return Err(ExpressionError::at(
                offset,
                format!("{operator} requires {} operands", expected.name()),
            ));
        }
        Ok(())
    }

    fn node(
        &mut self,
        kind: ExpressionKind,
        value_type: ValueType,
        offset: usize,
    ) -> Result<Expression, ExpressionError> {
        self.nodes += 1;
        if self.nodes > MAX_AST_NODES {
            return Err(ExpressionError::at(
                offset,
                "expression exceeds 256 AST nodes",
            ));
        }
        Ok(Expression {
            kind,
            value_type,
            offset,
        })
    }

    fn check_depth(&self, depth: usize, offset: usize) -> Result<(), ExpressionError> {
        if depth == MAX_NESTING_DEPTH {
            return Err(ExpressionError::at(offset, "nesting depth exceeds 8"));
        }
        Ok(())
    }

    fn take(&mut self, predicate: impl FnOnce(&TokenKind) -> bool) -> Option<Token> {
        let token = self.tokens.get(self.index)?;
        if !predicate(&token.kind) {
            return None;
        }
        self.index += 1;
        Some(token.clone())
    }

    fn expect(
        &mut self,
        predicate: impl FnOnce(&TokenKind) -> bool,
        message: &'static str,
    ) -> Result<(), ExpressionError> {
        if self.take(predicate).is_some() {
            Ok(())
        } else {
            Err(ExpressionError::at(self.current_offset(), message))
        }
    }

    fn current_offset(&self) -> usize {
        self.tokens
            .get(self.index)
            .map_or_else(|| self.end_offset(), |token| token.offset)
    }

    fn end_offset(&self) -> usize {
        self.tokens.last().map_or(0, |token| token.end)
    }
}

fn validate_symbols(symbols: &BTreeMap<String, ValueType>) -> Result<(), ExpressionError> {
    if symbols.len() > MAX_SYMBOLS {
        return Err(ExpressionError::at(0, "exceeds 64 declared symbols"));
    }
    for symbol in symbols.keys() {
        validate_symbol(symbol, 0)?;
    }
    Ok(())
}

fn lex(source: &str) -> Result<Vec<Token>, ExpressionError> {
    if source.len() > MAX_EXPRESSION_BYTES {
        return Err(ExpressionError::at(
            MAX_EXPRESSION_BYTES,
            "expression exceeds 1024 bytes",
        ));
    }
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if !byte.is_ascii() {
            return Err(ExpressionError::at(
                index,
                "source must contain only ASCII characters",
            ));
        }
        let offset = index;
        let kind = match byte {
            b'$' => {
                let (name, end) = lex_symbol(source, offset)?;
                index = end;
                TokenKind::Symbol(name)
            }
            b'0'..=b'9' => {
                index += 1;
                while index < bytes.len() && bytes[index].is_ascii_digit() {
                    index += 1;
                }
                let literal = source[offset..index].parse::<u128>().map_err(|_| {
                    ExpressionError::at(offset, "unsigned integer literal exceeds u128")
                })?;
                TokenKind::UnsignedLiteral(literal)
            }
            b'"' => {
                let (value, end) = lex_string(source, offset)?;
                index = end;
                TokenKind::StringLiteral(value)
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                {
                    index += 1;
                }
                match &source[offset..index] {
                    "true" => TokenKind::BooleanLiteral(true),
                    "false" => TokenKind::BooleanLiteral(false),
                    "if" => TokenKind::If,
                    "KiB" => TokenKind::UnsignedLiteral(1 << 10),
                    "MiB" => TokenKind::UnsignedLiteral(1 << 20),
                    "GiB" => TokenKind::UnsignedLiteral(1 << 30),
                    value => {
                        return Err(ExpressionError::at(
                            offset,
                            format!("unexpected identifier {value:?}"),
                        ));
                    }
                }
            }
            b'+' => {
                index += 1;
                TokenKind::Add
            }
            b'*' => {
                index += 1;
                TokenKind::Multiply
            }
            b'!' if bytes.get(index + 1) == Some(&b'=') => {
                index += 2;
                TokenKind::NotEqual
            }
            b'!' => {
                index += 1;
                TokenKind::Not
            }
            b'&' if bytes.get(index + 1) == Some(&b'&') => {
                index += 2;
                TokenKind::And
            }
            b'|' if bytes.get(index + 1) == Some(&b'|') => {
                index += 2;
                TokenKind::Or
            }
            b'=' if bytes.get(index + 1) == Some(&b'=') => {
                index += 2;
                TokenKind::Equal
            }
            b'<' if bytes.get(index + 1) == Some(&b'=') => {
                index += 2;
                TokenKind::LessOrEqual
            }
            b'<' => {
                index += 1;
                TokenKind::Less
            }
            b'>' if bytes.get(index + 1) == Some(&b'=') => {
                index += 2;
                TokenKind::GreaterOrEqual
            }
            b'>' => {
                index += 1;
                TokenKind::Greater
            }
            b'(' => {
                index += 1;
                TokenKind::LeftParenthesis
            }
            b')' => {
                index += 1;
                TokenKind::RightParenthesis
            }
            b',' => {
                index += 1;
                TokenKind::Comma
            }
            _ => {
                return Err(ExpressionError::at(
                    offset,
                    format!("unexpected character {:?}", char::from(byte)),
                ));
            }
        };
        tokens.push(Token {
            kind,
            offset,
            end: index,
        });
        if tokens.len() > MAX_TOKENS {
            return Err(ExpressionError::at(
                offset,
                "expression exceeds 128 lexical tokens",
            ));
        }
    }
    Ok(tokens)
}

fn lex_symbol(source: &str, offset: usize) -> Result<(String, usize), ExpressionError> {
    let bytes = source.as_bytes();
    if bytes.get(offset + 1) != Some(&b'{') {
        return Err(ExpressionError::at(
            offset,
            "symbol must use ${name} syntax",
        ));
    }
    let name_offset = offset + 2;
    let Some(relative_end) = source[name_offset..].find('}') else {
        return Err(ExpressionError::at(offset, "symbol is missing closing '}'"));
    };
    let name_end = name_offset + relative_end;
    let name = &source[name_offset..name_end];
    validate_symbol(name, offset)?;
    Ok((name.to_owned(), name_end + 1))
}

fn lex_string(source: &str, offset: usize) -> Result<(String, usize), ExpressionError> {
    let bytes = source.as_bytes();
    let mut end = offset + 1;
    while end < bytes.len() && bytes[end] != b'"' {
        if !bytes[end].is_ascii() || bytes[end].is_ascii_control() || bytes[end] == b'\\' {
            return Err(ExpressionError::at(
                end,
                "string literal contains an unsupported character",
            ));
        }
        end += 1;
    }
    if end == bytes.len() {
        return Err(ExpressionError::at(
            offset,
            "string literal is missing closing quote",
        ));
    }
    if end - offset - 1 > MAX_STRING_BYTES {
        return Err(ExpressionError::at(
            offset,
            "string literal exceeds 128 bytes",
        ));
    }
    Ok((source[offset + 1..end].to_owned(), end + 1))
}

fn validate_symbol(symbol: &str, offset: usize) -> Result<(), ExpressionError> {
    if symbol.len() > MAX_SYMBOL_BYTES {
        return Err(ExpressionError::at(offset, "symbol exceeds 191 bytes"));
    }
    if symbol.is_empty() || !symbol.split('.').all(valid_symbol_segment) {
        return Err(ExpressionError::at(
            offset,
            format!("invalid symbol {symbol:?}"),
        ));
    }
    Ok(())
}

fn valid_symbol_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    !bytes.is_empty()
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::{Value, ValueType, compile, lex};

    fn symbols(values: &[(&str, ValueType)]) -> BTreeMap<String, ValueType> {
        values
            .iter()
            .map(|(name, value_type)| ((*name).to_owned(), *value_type))
            .collect()
    }

    fn values(values: &[(&str, Value)]) -> BTreeMap<String, Value> {
        values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect()
    }

    #[test]
    fn linear_terms_distribute_products_over_sums() {
        let expression = compile(
            "(${base} + ${perToken} * ${context}) * 2 + if(${enabled}, 5, 7)",
            &symbols(&[
                ("base", ValueType::Unsigned),
                ("perToken", ValueType::Unsigned),
                ("context", ValueType::Unsigned),
                ("enabled", ValueType::Boolean),
            ]),
            ValueType::Unsigned,
        )
        .expect("expression compiles");
        let unknowns = BTreeSet::from(["base".to_owned(), "perToken".to_owned()]);
        let values = values(&[
            ("context", Value::Unsigned(100)),
            ("enabled", Value::Boolean(true)),
        ]);

        let terms = expression
            .linear_terms(&unknowns)
            .expect("linear expression")
            .into_iter()
            .map(|term| {
                (
                    term.unknown().map(str::to_owned),
                    term.coefficient(&values).expect("coefficient"),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            terms,
            vec![
                (Some("base".to_owned()), 2),
                (Some("perToken".to_owned()), 200),
                (None, 5),
            ]
        );
    }

    #[test]
    fn categorical_condition_symbols_name_boolean_and_string_choices_in_conditions() {
        let expression = compile(
            r#"if(${precision} == "q8_0" && ${count} > 0, ${count}, 1) * if(!${enabled}, 2, 3) + if(${other}, 1, 0) * 0 + if(${count} > 1, if(${nested}, 1, 2), 0)"#,
            &symbols(&[
                ("precision", ValueType::String),
                ("count", ValueType::Unsigned),
                ("enabled", ValueType::Boolean),
                ("other", ValueType::Boolean),
                ("nested", ValueType::Boolean),
            ]),
            ValueType::Unsigned,
        )
        .expect("expression compiles");

        assert_eq!(
            expression.categorical_condition_symbols(),
            BTreeSet::from(["enabled", "nested", "other", "precision"].map(str::to_owned))
        );
    }

    #[test]
    fn linear_terms_reject_nonlinear_uses_of_unknowns() {
        let symbols = symbols(&[
            ("a", ValueType::Unsigned),
            ("b", ValueType::Unsigned),
            ("enabled", ValueType::Boolean),
        ]);
        let unknowns = BTreeSet::from(["a".to_owned(), "b".to_owned()]);
        for (source, expected) in [
            ("${a} * ${b}", "multiplies two fitted parameters"),
            ("${a} * (${b} + 1)", "multiplies two fitted parameters"),
            ("if(${enabled}, ${a}, 1)", "under a condition or comparison"),
            ("if(${a} > 1, 1, 2)", "under a condition or comparison"),
        ] {
            let error = compile(source, &symbols, ValueType::Unsigned)
                .expect("expression compiles")
                .linear_terms(&unknowns)
                .expect_err("nonlinear expression");
            assert!(error.to_string().contains(expected), "{source}: {error}");
        }
    }

    #[test]
    fn evaluates_backward_compatible_unsigned_arithmetic() {
        let expression = compile(
            "${baseVramBytes} + ${vramBytesPerContextToken} * (${total-context} + 2)",
            &symbols(&[
                ("baseVramBytes", ValueType::Unsigned),
                ("vramBytesPerContextToken", ValueType::Unsigned),
                ("total-context", ValueType::Unsigned),
            ]),
            ValueType::Unsigned,
        )
        .expect("expression compiles");

        assert_eq!(
            expression
                .evaluate_unsigned(&values(&[
                    ("baseVramBytes", Value::Unsigned(100)),
                    ("vramBytesPerContextToken", Value::Unsigned(3)),
                    ("total-context", Value::Unsigned(8)),
                ]))
                .expect("expression evaluates"),
            130
        );
    }

    #[test]
    fn evaluates_typed_option_symbols_conditions_and_enum_comparisons() {
        let symbols = symbols(&[
            ("options.vision.enabled", ValueType::Boolean),
            ("options.vision.token-budget", ValueType::Unsigned),
            ("options.vision.projector", ValueType::String),
        ]);
        let predicate = compile(
            "${options.vision.enabled} && ${options.vision.token-budget} <= 4096 && ${options.vision.projector} == \"bf16\"",
            &symbols,
            ValueType::Boolean,
        )
        .expect("predicate compiles");
        assert!(
            predicate
                .evaluate_boolean(&values(&[
                    ("options.vision.enabled", Value::Boolean(true)),
                    ("options.vision.token-budget", Value::Unsigned(0)),
                    ("options.vision.projector", Value::String("bf16".to_owned()),),
                ]))
                .expect("predicate evaluates")
        );
    }

    #[test]
    fn lazy_if_and_boolean_branches_do_not_evaluate_unselected_failures() {
        let unsigned = compile(
            "if(false, 18446744073709551615 * 18446744073709551615, 7)",
            &BTreeMap::new(),
            ValueType::Unsigned,
        )
        .expect("if compiles");
        assert_eq!(
            unsigned
                .evaluate_unsigned(&BTreeMap::new())
                .expect("false branch evaluates"),
            7
        );

        let boolean = compile(
            "true || ${options.vision.enabled}",
            &symbols(&[("options.vision.enabled", ValueType::Boolean)]),
            ValueType::Boolean,
        )
        .expect("boolean compiles");
        assert!(
            boolean
                .evaluate_boolean(&BTreeMap::new())
                .expect("short circuit evaluates")
        );
    }

    #[test]
    fn byte_unit_constants_are_unsigned_literals() {
        let declared = symbols(&[("x", ValueType::Unsigned)]);
        let values = values(&[("x", Value::Unsigned(3))]);
        for (source, expected) in [
            ("KiB", 1024),
            ("MiB", 1_048_576),
            ("GiB", 1_073_741_824),
            ("${x} * GiB", 3 * 1_073_741_824),
            ("${x} * MiB + 2 * KiB", 3 * 1_048_576 + 2048),
        ] {
            let expression = compile(source, &declared, ValueType::Unsigned)
                .unwrap_or_else(|error| panic!("{source} compiles: {error}"));
            assert_eq!(
                expression.evaluate_unsigned(&values).expect("evaluates"),
                expected,
                "{source}"
            );
        }
    }

    #[test]
    fn byte_unit_constants_keep_linear_terms_coefficients() {
        let expression = compile(
            "${a} * MiB + 5 * KiB",
            &symbols(&[("a", ValueType::Unsigned)]),
            ValueType::Unsigned,
        )
        .expect("expression compiles");
        let unknowns = BTreeSet::from(["a".to_owned()]);

        let terms = expression
            .linear_terms(&unknowns)
            .expect("linear expression")
            .into_iter()
            .map(|term| {
                (
                    term.unknown().map(str::to_owned),
                    term.coefficient(&BTreeMap::new()).expect("coefficient"),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(terms, vec![(Some("a".to_owned()), 1_048_576), (None, 5120)]);
    }

    #[test]
    fn rejects_other_identifiers_and_unit_lookalikes() {
        for source in ["TiB", "gib", "mib", "GB", "GiBs", "foo"] {
            let error = compile(source, &BTreeMap::new(), ValueType::Unsigned)
                .expect_err("identifier is rejected");
            assert!(
                error.to_string().contains("unexpected identifier"),
                "{source}: {error}"
            );
        }
    }

    #[test]
    fn rejects_unknown_or_wrongly_typed_symbols_in_all_branches() {
        let declared = symbols(&[("enabled", ValueType::Boolean)]);
        for (source, expected) in [
            ("if(true, 1, ${unknown})", "unknown symbol \"unknown\""),
            (
                "if(true, 1, ${enabled})",
                "if branches must have the same type",
            ),
            ("${enabled} + 1", "'+' requires unsigned integer operands"),
            ("${enabled}", "expected unsigned integer result"),
        ] {
            assert!(
                compile(source, &declared, ValueType::Unsigned)
                    .expect_err("expression must reject")
                    .to_string()
                    .contains(expected),
                "{source:?} must report {expected:?}"
            );
        }
    }

    #[test]
    fn enforces_source_token_symbol_and_depth_bounds() {
        let max_source = format!("1{}", " ".repeat(1023));
        assert_eq!(max_source.len(), 1024);
        compile(&max_source, &BTreeMap::new(), ValueType::Unsigned).expect("source at byte bound");

        let long_source = "1".repeat(1025);
        assert!(
            compile(&long_source, &BTreeMap::new(), ValueType::Unsigned)
                .expect_err("source bound")
                .to_string()
                .contains("expression exceeds 1024 bytes")
        );

        let max_tokens = format!("1{} + if(true, 1, 1)", " + 1".repeat(59));
        assert_eq!(lex(&max_tokens).expect("lex token bound").len(), 128);
        compile(&max_tokens, &BTreeMap::new(), ValueType::Unsigned).expect("tokens at bound");

        let many_tokens = std::iter::repeat_n("1", 65).collect::<Vec<_>>().join(" + ");
        assert!(
            compile(&many_tokens, &BTreeMap::new(), ValueType::Unsigned)
                .expect_err("token bound")
                .to_string()
                .contains("expression exceeds 128 lexical tokens")
        );

        let many_symbols = (0..65)
            .map(|index| (format!("symbol{index}"), ValueType::Unsigned))
            .collect::<BTreeMap<_, _>>();
        assert!(
            compile("1", &many_symbols, ValueType::Unsigned)
                .expect_err("symbol bound")
                .to_string()
                .contains("exceeds 64 declared symbols")
        );

        assert!(
            compile("(((((((((1)))))))))", &BTreeMap::new(), ValueType::Unsigned,)
                .expect_err("depth bound")
                .to_string()
                .contains("nesting depth exceeds 8")
        );
    }

    #[test]
    fn checked_arithmetic_rejects_overflow_and_missing_values() {
        let expression = compile(
            "${left} * ${right}",
            &symbols(&[
                ("left", ValueType::Unsigned),
                ("right", ValueType::Unsigned),
            ]),
            ValueType::Unsigned,
        )
        .expect("expression compiles");
        assert_eq!(
            expression
                .evaluate_unsigned(&values(&[
                    ("left", Value::Unsigned(u64::MAX)),
                    ("right", Value::Unsigned(u64::MAX)),
                ]))
                .expect_err("final result must reject")
                .to_string(),
            "sizing expression at byte 8: final result exceeds u64"
        );
        assert_eq!(
            expression
                .evaluate_unsigned(&values(&[("left", Value::Unsigned(1))]))
                .expect_err("missing symbol must reject")
                .to_string(),
            "sizing expression at byte 10: missing value for symbol \"right\""
        );
    }
}
