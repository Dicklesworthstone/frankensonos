//! Bounded RFC 6570 Level 4 URI-template parsing and expansion.
//!
//! A URI template is deliberately distinct from
//! [`crate::common_types::AbsoluteUri`]: a
//! template describes a set of references and must be expanded before it can
//! be used where an ordinary URI is required. This module owns the
//! syntax-preserving representation, forward expansion, and the separate
//! stricter reverse-routing compiler because many valid RFC 6570 templates
//! are not invertible.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

/// Maximum UTF-8 bytes accepted in a single URI template source string.
pub const MAX_URI_TEMPLATE_BYTES: usize = 16 * 1024;
/// Maximum literal/expression parts retained in one parsed template.
pub const MAX_URI_TEMPLATE_PARTS: usize = 256;
/// Maximum expressions retained in one parsed template.
pub const MAX_URI_TEMPLATE_EXPRESSIONS: usize = 128;
/// Maximum variable specifications allowed in one expression.
pub const MAX_URI_TEMPLATE_VARIABLES_PER_EXPRESSION: usize = 64;
/// Maximum UTF-8 bytes in one RFC 6570 variable name.
pub const MAX_URI_TEMPLATE_VARIABLE_NAME_BYTES: usize = 512;
/// Maximum RFC 6570 prefix modifier, which is strictly less than 10000.
pub const MAX_URI_TEMPLATE_PREFIX_LENGTH: usize = 9_999;
/// Maximum individual UTF-8 value/key bytes admitted by the bounded expander.
pub const MAX_URI_TEMPLATE_VALUE_BYTES: usize = 16 * 1024;
/// Maximum composite members inspected by one expansion.
pub const MAX_URI_TEMPLATE_COMPOSITE_ITEMS: usize = 1_024;
/// Maximum UTF-8 output bytes emitted by one expansion.
pub const MAX_URI_TEMPLATE_EXPANSION_OUTPUT_BYTES: usize = 64 * 1024;
/// Maximum UTF-8 bytes accepted by a reverse match candidate.
///
/// A successful reverse match must re-expand under the ordinary output bound,
/// so accepting a longer candidate could never produce a valid result.
pub const MAX_URI_TEMPLATE_MATCH_INPUT_BYTES: usize = MAX_URI_TEMPLATE_EXPANSION_OUTPUT_BYTES;

/// Values supplied to a [`UriTemplate`] expansion.
pub type TemplateValues = BTreeMap<String, TemplateValue>;

/// One defined RFC 6570 variable value.
///
/// An absent map entry denotes RFC 6570's undefined value. Empty list and map
/// values are likewise treated as undefined, while an empty scalar is defined.
/// Associative values retain their pair order so expansions reproduce the
/// order supplied by an RFC 6570 data model rather than silently sorting keys.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TemplateValue {
    /// A single scalar string value.
    Scalar(String),
    /// An ordered composite list value.
    List(Vec<Option<String>>),
    /// A composite associative value.
    Associative(Vec<(String, Option<String>)>),
}

impl TemplateValue {
    /// Constructs a scalar value.
    #[must_use]
    pub fn scalar(value: impl Into<String>) -> Self {
        Self::Scalar(value.into())
    }

    /// Constructs a list value.
    #[must_use]
    pub fn list(values: Vec<String>) -> Self {
        Self::List(values.into_iter().map(Some).collect())
    }

    /// Constructs a list that can retain undefined members.
    #[must_use]
    pub fn list_with_undefined(values: Vec<Option<String>>) -> Self {
        Self::List(values)
    }

    /// Constructs an associative value.
    #[must_use]
    pub fn associative(values: Vec<(String, String)>) -> Self {
        Self::Associative(
            values
                .into_iter()
                .map(|(key, value)| (key, Some(value)))
                .collect(),
        )
    }

    /// Constructs an associative value that can retain undefined members.
    #[must_use]
    pub fn associative_with_undefined(values: Vec<(String, Option<String>)>) -> Self {
        Self::Associative(values)
    }
}

impl From<String> for TemplateValue {
    fn from(value: String) -> Self {
        Self::Scalar(value)
    }
}

impl From<&str> for TemplateValue {
    fn from(value: &str) -> Self {
        Self::Scalar(value.to_owned())
    }
}

/// Hard-bounded limits used by [`UriTemplate::expand_with_limits`].
///
/// Callers can tighten the defaults for a particular boundary but cannot
/// configure away the crate-level safety caps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UriTemplateExpansionLimits {
    output_bytes: usize,
    composite_items: usize,
    value_bytes: usize,
}

impl UriTemplateExpansionLimits {
    /// Creates a bounded expansion configuration.
    pub fn new(
        max_output_bytes: usize,
        max_composite_items: usize,
        max_value_bytes: usize,
    ) -> Result<Self, UriTemplateError> {
        if max_output_bytes == 0 || max_output_bytes > MAX_URI_TEMPLATE_EXPANSION_OUTPUT_BYTES {
            return Err(UriTemplateError::InvalidExpansionLimit {
                field: "max_output_bytes",
                actual: max_output_bytes,
                maximum: MAX_URI_TEMPLATE_EXPANSION_OUTPUT_BYTES,
            });
        }
        if max_composite_items == 0 || max_composite_items > MAX_URI_TEMPLATE_COMPOSITE_ITEMS {
            return Err(UriTemplateError::InvalidExpansionLimit {
                field: "max_composite_items",
                actual: max_composite_items,
                maximum: MAX_URI_TEMPLATE_COMPOSITE_ITEMS,
            });
        }
        if max_value_bytes == 0 || max_value_bytes > MAX_URI_TEMPLATE_VALUE_BYTES {
            return Err(UriTemplateError::InvalidExpansionLimit {
                field: "max_value_bytes",
                actual: max_value_bytes,
                maximum: MAX_URI_TEMPLATE_VALUE_BYTES,
            });
        }
        Ok(Self {
            output_bytes: max_output_bytes,
            composite_items: max_composite_items,
            value_bytes: max_value_bytes,
        })
    }

    /// Returns the maximum output bytes permitted by this configuration.
    #[must_use]
    pub const fn max_output_bytes(self) -> usize {
        self.output_bytes
    }

    /// Returns the maximum composite members inspected by this configuration.
    #[must_use]
    pub const fn max_composite_items(self) -> usize {
        self.composite_items
    }

    /// Returns the maximum bytes permitted in one key or value.
    #[must_use]
    pub const fn max_value_bytes(self) -> usize {
        self.value_bytes
    }
}

impl Default for UriTemplateExpansionLimits {
    fn default() -> Self {
        Self {
            output_bytes: MAX_URI_TEMPLATE_EXPANSION_OUTPUT_BYTES,
            composite_items: MAX_URI_TEMPLATE_COMPOSITE_ITEMS,
            value_bytes: MAX_URI_TEMPLATE_VALUE_BYTES,
        }
    }
}

/// An immutable, source-preserving RFC 6570 Level 4 template AST.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UriTemplate {
    source: String,
    parts: Vec<UriTemplatePart>,
}

impl UriTemplate {
    /// Parses a complete RFC 6570 Level 4 URI template.
    pub fn parse(source: impl AsRef<str>) -> Result<Self, UriTemplateError> {
        let source = source.as_ref();
        if source.len() > MAX_URI_TEMPLATE_BYTES {
            return Err(UriTemplateError::SourceTooLong {
                actual: source.len(),
                maximum: MAX_URI_TEMPLATE_BYTES,
            });
        }

        let bytes = source.as_bytes();
        let mut index = 0;
        let mut literal_start = 0;
        let mut expressions = 0;
        let mut parts = Vec::new();

        while index < bytes.len() {
            match bytes[index] {
                b'{' => {
                    push_literal(&mut parts, &source[literal_start..index], literal_start)?;
                    let expression_start = index + 1;
                    let Some(relative_end) = bytes[expression_start..]
                        .iter()
                        .position(|byte| *byte == b'}')
                    else {
                        return Err(UriTemplateError::UnclosedExpression { offset: index });
                    };
                    let expression_end = expression_start + relative_end;
                    if let Some(relative_nested) = bytes[expression_start..expression_end]
                        .iter()
                        .position(|byte| *byte == b'{')
                    {
                        return Err(UriTemplateError::NestedExpression {
                            offset: expression_start + relative_nested,
                        });
                    }
                    if expressions == MAX_URI_TEMPLATE_EXPRESSIONS {
                        return Err(UriTemplateError::TooManyExpressions {
                            maximum: MAX_URI_TEMPLATE_EXPRESSIONS,
                        });
                    }
                    push_part(
                        &mut parts,
                        UriTemplatePart::Expression(parse_expression(
                            &source[expression_start..expression_end],
                            expression_start,
                        )?),
                    )?;
                    expressions += 1;
                    index = expression_end + 1;
                    literal_start = index;
                }
                b'}' => return Err(UriTemplateError::UnexpectedCloseBrace { offset: index }),
                _ => index += 1,
            }
        }
        push_literal(&mut parts, &source[literal_start..], literal_start)?;

        Ok(Self {
            source: source.to_owned(),
            parts,
        })
    }

    /// Returns the exact source text used to construct this template.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Returns the source-preserving AST parts in expansion order.
    #[must_use]
    pub fn parts(&self) -> &[UriTemplatePart] {
        &self.parts
    }

    /// Expands the template using the default hard-bounded limits.
    pub fn expand(&self, values: &TemplateValues) -> Result<String, UriTemplateError> {
        self.expand_with_limits(values, UriTemplateExpansionLimits::default())
    }

    /// Expands the template using caller-tightened, hard-bounded limits.
    pub fn expand_with_limits(
        &self,
        values: &TemplateValues,
        limits: UriTemplateExpansionLimits,
    ) -> Result<String, UriTemplateError> {
        let mut output = String::new();
        let mut state = ExpansionState::default();
        for part in &self.parts {
            match part {
                UriTemplatePart::Literal(literal) => {
                    append_encoded_literal(&mut output, literal, limits)?;
                }
                UriTemplatePart::Expression(expression) => {
                    expand_expression(&mut output, expression, values, limits, &mut state)?;
                }
            }
        }
        Ok(output)
    }

    /// Compiles this template for deterministic, byte-exact reverse matching.
    ///
    /// RFC 6570 permits expansions that do not have a unique inverse. This
    /// operation deliberately admits only the scalar dispatch subset whose
    /// captures can be reconstructed without normalization or guessing.
    pub fn compile_reversible(&self) -> Result<ReversibleResourceTemplate, UriTemplateError> {
        ReversibleResourceTemplate::from_template(self)
    }
}

impl AsRef<str> for UriTemplate {
    fn as_ref(&self) -> &str {
        self.source()
    }
}

impl fmt::Display for UriTemplate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.source())
    }
}

impl FromStr for UriTemplate {
    type Err = UriTemplateError;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        Self::parse(source)
    }
}

/// A compiled, deterministic RFC 6570 template suitable for local dispatch.
///
/// This is intentionally stricter than [`UriTemplate`]. It accepts scalar
/// captures only, requires unambiguous expression boundaries, and decodes
/// captured percent triplets once. Named (`;`, `?`, and `&`) expressions may
/// contain multiple scalar variables because their wire names make omission
/// and ordering deterministic. List and associative inputs are rejected
/// before expansion because their compact and exploded forms are not generally
/// invertible. An explode modifier on a scalar is admitted: RFC 6570 gives it
/// the same wire form as the unmodified scalar.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReversibleResourceTemplate {
    template: UriTemplate,
    parts: Vec<ReversibleTemplatePart>,
}

impl ReversibleResourceTemplate {
    /// Compiles an owned template for deterministic reverse matching.
    pub fn compile(template: UriTemplate) -> Result<Self, UriTemplateError> {
        Self::from_template(&template)
    }

    /// Compiles a borrowed template for deterministic reverse matching.
    pub fn from_template(template: &UriTemplate) -> Result<Self, UriTemplateError> {
        let mut parts = Vec::with_capacity(template.parts.len());
        let mut variable_names = BTreeSet::new();

        for (index, part) in template.parts.iter().enumerate() {
            match part {
                UriTemplatePart::Literal(literal) => {
                    let mut encoded = String::new();
                    append_encoded_literal(
                        &mut encoded,
                        literal,
                        UriTemplateExpansionLimits::default(),
                    )?;
                    parts.push(ReversibleTemplatePart::Literal(encoded));
                }
                UriTemplatePart::Expression(expression) => {
                    if expression.variables().is_empty()
                        || (expression.variables().len() != 1
                            && !expression.operator().properties().named)
                    {
                        return Err(UriTemplateError::NonReversibleTemplate {
                            reason: UriTemplateMatchRejection::MultipleVariables,
                        });
                    }

                    let mut variables = Vec::with_capacity(expression.variables().len());
                    for variable in expression.variables() {
                        if matches!(variable.modifier(), Some(UriTemplateModifier::Prefix(_))) {
                            return Err(UriTemplateError::NonReversibleTemplate {
                                reason: UriTemplateMatchRejection::LossyPrefix {
                                    variable: variable.name().to_owned(),
                                },
                            });
                        }
                        if !variable_names.insert(variable.name().to_owned()) {
                            return Err(UriTemplateError::NonReversibleTemplate {
                                reason: UriTemplateMatchRejection::DuplicateVariable {
                                    variable: variable.name().to_owned(),
                                },
                            });
                        }
                        let mut encoded_name = String::new();
                        append_variable_name(
                            &mut encoded_name,
                            variable.name(),
                            UriTemplateExpansionLimits::default(),
                        )?;
                        variables.push(ReversibleTemplateVariable {
                            name: variable.name().to_owned(),
                            encoded_name,
                        });
                    }

                    let next_boundary = match template.parts.get(index + 1) {
                        Some(UriTemplatePart::Literal(literal)) => {
                            let mut encoded = String::new();
                            append_encoded_literal(
                                &mut encoded,
                                literal,
                                UriTemplateExpansionLimits::default(),
                            )?;
                            Some(ReversibleBoundary::Literal(encoded))
                        }
                        Some(UriTemplatePart::Expression(next)) => {
                            Some(reversible_adjacent_expression_boundary(expression, next)?)
                        }
                        None => None,
                    };

                    validate_reversible_expression(expression, next_boundary.as_ref())?;
                    parts.push(ReversibleTemplatePart::Expression(
                        ReversibleTemplateExpression {
                            operator: expression.operator(),
                            variables,
                            next_boundary,
                        },
                    ));
                }
            }
        }

        Ok(Self {
            template: template.clone(),
            parts,
        })
    }

    /// Returns the original RFC 6570 template.
    #[must_use]
    pub fn template(&self) -> &UriTemplate {
        &self.template
    }

    /// Expands only the scalar value shape declared by this compiled matcher.
    pub fn expand(&self, values: &TemplateValues) -> Result<String, UriTemplateError> {
        for part in &self.parts {
            let ReversibleTemplatePart::Expression(expression) = part else {
                continue;
            };
            for variable in &expression.variables {
                let Some(value) = values.get(&variable.name) else {
                    continue;
                };
                let TemplateValue::Scalar(value) = value else {
                    return Err(UriTemplateError::NonScalarMatchValue {
                        variable: variable.name.clone(),
                    });
                };
                if value.is_empty()
                    && matches!(
                        expression.operator,
                        UriTemplateOperator::Simple | UriTemplateOperator::Reserved
                    )
                {
                    return Err(UriTemplateError::AmbiguousEmptyScalar {
                        variable: variable.name.clone(),
                    });
                }
                if matches!(
                    expression.operator,
                    UriTemplateOperator::Reserved | UriTemplateOperator::Fragment
                ) && contains_pct_encoded_triplet(value)
                {
                    return Err(UriTemplateError::PreescapedReservedMatchValue {
                        variable: variable.name.clone(),
                    });
                }
            }
        }
        self.template.expand(values)
    }

    /// Reverse-matches an exact URI wire string and returns its scalar bindings.
    ///
    /// `Ok(None)` means the URI is outside this template's language. Any
    /// successful result is replayed through [`Self::expand`] and compared to
    /// the original bytes before it is returned.
    pub fn match_uri(&self, uri: &str) -> Result<Option<TemplateValues>, UriTemplateError> {
        if uri.len() > MAX_URI_TEMPLATE_MATCH_INPUT_BYTES {
            return Err(UriTemplateError::MatchInputTooLong {
                actual: uri.len(),
                maximum: MAX_URI_TEMPLATE_MATCH_INPUT_BYTES,
            });
        }

        let mut offset = 0;
        let mut values = TemplateValues::new();
        for part in &self.parts {
            match part {
                ReversibleTemplatePart::Literal(literal) => {
                    let Some(remainder) = uri.get(offset..) else {
                        return Ok(None);
                    };
                    let Some(remainder) = remainder.strip_prefix(literal) else {
                        return Ok(None);
                    };
                    offset = uri.len() - remainder.len();
                }
                ReversibleTemplatePart::Expression(expression) => {
                    let Some(remainder) = uri.get(offset..) else {
                        return Ok(None);
                    };
                    let Some((captures, consumed)) =
                        reverse_match_expression(expression, remainder)
                    else {
                        return Ok(None);
                    };
                    offset = offset.saturating_add(consumed);
                    for (name, capture) in captures {
                        values.insert(name, TemplateValue::Scalar(capture));
                    }
                }
            }
        }
        if offset != uri.len() {
            return Ok(None);
        }
        if self.expand(&values)? != uri {
            return Ok(None);
        }
        Ok(Some(values))
    }
}

impl TryFrom<UriTemplate> for ReversibleResourceTemplate {
    type Error = UriTemplateError;

    fn try_from(template: UriTemplate) -> Result<Self, Self::Error> {
        Self::compile(template)
    }
}

impl TryFrom<&UriTemplate> for ReversibleResourceTemplate {
    type Error = UriTemplateError;

    fn try_from(template: &UriTemplate) -> Result<Self, Self::Error> {
        Self::from_template(template)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReversibleTemplatePart {
    Literal(String),
    Expression(ReversibleTemplateExpression),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReversibleTemplateExpression {
    operator: UriTemplateOperator,
    variables: Vec<ReversibleTemplateVariable>,
    next_boundary: Option<ReversibleBoundary>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReversibleTemplateVariable {
    name: String,
    encoded_name: String,
}

/// A wire boundary that separates one reversible capture from the next part.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ReversibleBoundary {
    /// Literal source text following the capture.
    Literal(String),
    /// The distinctive opening bytes of a following expression.
    ExpressionPrefix(String),
}

impl ReversibleBoundary {
    // Not const: `String` -> `&str` deref coercion is non-const on the pinned nightly.
    fn as_str(&self) -> &str {
        match self {
            Self::Literal(value) | Self::ExpressionPrefix(value) => value,
        }
    }

    const fn permits_absent_following_expression(&self) -> bool {
        matches!(self, Self::ExpressionPrefix(_))
    }
}

/// One ordered segment in a [`UriTemplate`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UriTemplatePart {
    /// Literal source text outside a template expression.
    Literal(String),
    /// A parsed expression.
    Expression(UriTemplateExpression),
}

/// One parsed RFC 6570 expression.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UriTemplateExpression {
    operator: UriTemplateOperator,
    variables: Vec<UriTemplateVariable>,
}

impl UriTemplateExpression {
    /// Returns the expression operator.
    #[must_use]
    pub const fn operator(&self) -> UriTemplateOperator {
        self.operator
    }

    /// Returns the variable specifications in source order.
    #[must_use]
    pub fn variables(&self) -> &[UriTemplateVariable] {
        &self.variables
    }
}

/// The Level 4 expression operator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UriTemplateOperator {
    /// `{var}` simple string expansion.
    Simple,
    /// `{+var}` reserved string expansion.
    Reserved,
    /// `{#var}` fragment expansion.
    Fragment,
    /// `{.var}` label expansion.
    Label,
    /// `{/var}` path-segment expansion.
    Path,
    /// `{;var}` matrix/path-parameter expansion.
    PathParameter,
    /// `{?var}` form-style query expansion.
    Query,
    /// `{&var}` form-style query continuation.
    QueryContinuation,
}

impl UriTemplateOperator {
    /// Returns the operator character, or `None` for simple expansion.
    #[must_use]
    pub const fn character(self) -> Option<char> {
        match self {
            Self::Simple => None,
            Self::Reserved => Some('+'),
            Self::Fragment => Some('#'),
            Self::Label => Some('.'),
            Self::Path => Some('/'),
            Self::PathParameter => Some(';'),
            Self::Query => Some('?'),
            Self::QueryContinuation => Some('&'),
        }
    }

    const fn properties(self) -> OperatorProperties {
        match self {
            Self::Simple => OperatorProperties::new("", ",", false, "", false),
            Self::Reserved => OperatorProperties::new("", ",", false, "", true),
            Self::Fragment => OperatorProperties::new("#", ",", false, "", true),
            Self::Label => OperatorProperties::new(".", ".", false, "", false),
            Self::Path => OperatorProperties::new("/", "/", false, "", false),
            Self::PathParameter => OperatorProperties::new(";", ";", true, "", false),
            Self::Query => OperatorProperties::new("?", "&", true, "=", false),
            Self::QueryContinuation => OperatorProperties::new("&", "&", true, "=", false),
        }
    }
}

/// One named variable and optional Level 4 modifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UriTemplateVariable {
    name: String,
    modifier: Option<UriTemplateModifier>,
}

impl UriTemplateVariable {
    /// Returns the exact, case-sensitive RFC 6570 variable name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the optional Level 4 modifier.
    #[must_use]
    pub const fn modifier(&self) -> Option<UriTemplateModifier> {
        self.modifier
    }
}

/// One RFC 6570 Level 4 variable modifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UriTemplateModifier {
    /// Restrict a scalar variable to this many Unicode code points.
    Prefix(usize),
    /// Expand every list member or associative pair independently.
    Explode,
}

/// A reason a syntactically valid template cannot be used for reverse match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UriTemplateMatchRejection {
    /// An expression binds more than one value without an invertible shape.
    MultipleVariables,
    /// Two captures touch without a literal boundary.
    AdjacentCaptures,
    /// A variable is bound more than once by the same matcher.
    DuplicateVariable {
        /// The repeated variable name.
        variable: String,
    },
    /// A prefix modifier discards data that a reverse match cannot restore.
    LossyPrefix {
        /// The affected variable name.
        variable: String,
    },
    /// A composite explode modifier has no declared, unique inverse.
    ///
    /// Scalar `*` values are admitted because their RFC 6570 expansion is
    /// identical to an unmodified scalar; composites are rejected at the
    /// scalar dispatch boundary instead.
    ExplodedComposite {
        /// The affected variable name.
        variable: String,
    },
    /// A following literal can also occur inside the capture language.
    AmbiguousBoundary,
    /// A reserved or fragment expansion has a following capture boundary.
    UnboundedReservedCapture,
}

/// One typed reason template parsing or expansion failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UriTemplateError {
    /// Source text exceeded the fixed parser bound.
    SourceTooLong {
        /// Observed UTF-8 bytes.
        actual: usize,
        /// Maximum UTF-8 bytes.
        maximum: usize,
    },
    /// Parsing would retain too many parts.
    TooManyParts {
        /// Maximum part count.
        maximum: usize,
    },
    /// Parsing would retain too many expressions.
    TooManyExpressions {
        /// Maximum expression count.
        maximum: usize,
    },
    /// An expression exceeded its variable-specification bound.
    TooManyVariables {
        /// Maximum variable specifications.
        maximum: usize,
    },
    /// A variable name exceeded its byte bound.
    VariableNameTooLong {
        /// Observed UTF-8 bytes.
        actual: usize,
        /// Maximum UTF-8 bytes.
        maximum: usize,
    },
    /// A literal used a character outside RFC 6570's literal grammar.
    InvalidLiteral {
        /// Byte offset in the source text.
        offset: usize,
    },
    /// An expression opened without a matching closing brace.
    UnclosedExpression {
        /// Byte offset of the opening brace.
        offset: usize,
    },
    /// An expression attempted to nest another expression.
    NestedExpression {
        /// Byte offset of the nested opening brace.
        offset: usize,
    },
    /// A closing brace appeared outside an expression.
    UnexpectedCloseBrace {
        /// Byte offset of the closing brace.
        offset: usize,
    },
    /// An expression did not contain a variable specification.
    EmptyExpression {
        /// Byte offset immediately after the opening brace.
        offset: usize,
    },
    /// An expression used an RFC-reserved, unsupported operator.
    UnsupportedOperator {
        /// The reserved operator character.
        operator: char,
        /// Byte offset of the operator.
        offset: usize,
    },
    /// A variable specification did not match the RFC 6570 grammar.
    InvalidVariable {
        /// Byte offset of the variable specification.
        offset: usize,
    },
    /// A prefix modifier did not match the RFC's positive four-digit grammar.
    InvalidPrefix {
        /// Byte offset of the prefix modifier.
        offset: usize,
    },
    /// A caller tried to relax a fixed expansion limit or selected zero.
    InvalidExpansionLimit {
        /// The invalid configuration field.
        field: &'static str,
        /// Requested value.
        actual: usize,
        /// Fixed ceiling.
        maximum: usize,
    },
    /// A supplied scalar, list member, map key, or map value was too large.
    ValueTooLong {
        /// Observed UTF-8 bytes.
        actual: usize,
        /// Configured maximum UTF-8 bytes.
        maximum: usize,
    },
    /// Expansion would inspect too many composite members.
    TooManyCompositeItems {
        /// Observed total members.
        actual: usize,
        /// Configured maximum members.
        maximum: usize,
    },
    /// Expansion output would exceed the configured byte bound.
    ExpansionTooLarge {
        /// Bytes that would be present after the append.
        actual: usize,
        /// Configured maximum output bytes.
        maximum: usize,
    },
    /// A prefix modifier was applied to a non-scalar value.
    PrefixAppliedToComposite {
        /// The affected variable name.
        variable: String,
    },
    /// An associative value supplied the same key more than once.
    DuplicateAssociativeKey {
        /// The repeated key.
        key: String,
    },
    /// A template is valid RFC 6570 syntax but has no deterministic inverse.
    NonReversibleTemplate {
        /// The specific reason compilation was rejected.
        reason: UriTemplateMatchRejection,
    },
    /// A reversible expansion received a list or associative value.
    NonScalarMatchValue {
        /// The variable that requires a scalar value.
        variable: String,
    },
    /// An empty simple or reserved scalar is indistinguishable from undefined.
    AmbiguousEmptyScalar {
        /// The affected variable name.
        variable: String,
    },
    /// A candidate URI exceeded the fixed reverse-match input bound.
    MatchInputTooLong {
        /// Observed UTF-8 bytes.
        actual: usize,
        /// Fixed maximum UTF-8 bytes.
        maximum: usize,
    },
    /// A reversible reserved or fragment value contained a pre-escaped triplet.
    PreescapedReservedMatchValue {
        /// The affected variable name.
        variable: String,
    },
}

impl fmt::Display for UriTemplateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceTooLong { actual, maximum } => {
                write!(
                    formatter,
                    "URI template is {actual} bytes; maximum is {maximum}"
                )
            }
            Self::TooManyParts { maximum } => {
                write!(formatter, "URI template exceeds {maximum} parsed parts")
            }
            Self::TooManyExpressions { maximum } => {
                write!(formatter, "URI template exceeds {maximum} expressions")
            }
            Self::TooManyVariables { maximum } => {
                write!(
                    formatter,
                    "URI template expression exceeds {maximum} variables"
                )
            }
            Self::VariableNameTooLong { actual, maximum } => {
                write!(
                    formatter,
                    "URI template variable is {actual} bytes; maximum is {maximum}"
                )
            }
            Self::InvalidLiteral { offset } => {
                write!(formatter, "invalid URI template literal at byte {offset}")
            }
            Self::UnclosedExpression { offset } => {
                write!(
                    formatter,
                    "unclosed URI template expression at byte {offset}"
                )
            }
            Self::NestedExpression { offset } => {
                write!(formatter, "nested URI template expression at byte {offset}")
            }
            Self::UnexpectedCloseBrace { offset } => {
                write!(
                    formatter,
                    "unexpected URI template closing brace at byte {offset}"
                )
            }
            Self::EmptyExpression { offset } => {
                write!(formatter, "empty URI template expression at byte {offset}")
            }
            Self::UnsupportedOperator { operator, offset } => write!(
                formatter,
                "unsupported URI template operator {operator:?} at byte {offset}"
            ),
            Self::InvalidVariable { offset } => {
                write!(formatter, "invalid URI template variable at byte {offset}")
            }
            Self::InvalidPrefix { offset } => {
                write!(
                    formatter,
                    "invalid URI template prefix modifier at byte {offset}"
                )
            }
            Self::InvalidExpansionLimit {
                field,
                actual,
                maximum,
            } => write!(formatter, "{field} is {actual}; maximum is {maximum}"),
            Self::ValueTooLong { actual, maximum } => {
                write!(
                    formatter,
                    "URI template value is {actual} bytes; maximum is {maximum}"
                )
            }
            Self::TooManyCompositeItems { actual, maximum } => write!(
                formatter,
                "URI template expansion has {actual} composite members; maximum is {maximum}"
            ),
            Self::ExpansionTooLarge { actual, maximum } => {
                write!(
                    formatter,
                    "URI template expansion is {actual} bytes; maximum is {maximum}"
                )
            }
            Self::PrefixAppliedToComposite { variable } => write!(
                formatter,
                "URI template prefix modifier cannot be applied to composite variable {variable:?}"
            ),
            Self::DuplicateAssociativeKey { key } => {
                write!(
                    formatter,
                    "URI template associative key {key:?} is duplicated"
                )
            }
            Self::NonReversibleTemplate { reason } => {
                write!(
                    formatter,
                    "URI template cannot be reverse matched: {reason}"
                )
            }
            Self::NonScalarMatchValue { variable } => write!(
                formatter,
                "reversible URI template variable {variable:?} requires a scalar value"
            ),
            Self::AmbiguousEmptyScalar { variable } => write!(
                formatter,
                "reversible URI template variable {variable:?} cannot use an empty scalar"
            ),
            Self::MatchInputTooLong { actual, maximum } => write!(
                formatter,
                "URI template match input is {actual} bytes; maximum is {maximum}"
            ),
            Self::PreescapedReservedMatchValue { variable } => write!(
                formatter,
                "reversible URI template variable {variable:?} cannot contain a pre-escaped percent triplet"
            ),
        }
    }
}

impl std::error::Error for UriTemplateError {}

impl fmt::Display for UriTemplateMatchRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MultipleVariables => {
                formatter.write_str("an expression binds multiple variables")
            }
            Self::AdjacentCaptures => formatter.write_str("adjacent expressions have no boundary"),
            Self::DuplicateVariable { variable } => {
                write!(formatter, "variable {variable:?} is bound more than once")
            }
            Self::LossyPrefix { variable } => {
                write!(
                    formatter,
                    "variable {variable:?} uses a lossy prefix modifier"
                )
            }
            Self::ExplodedComposite { variable } => {
                write!(
                    formatter,
                    "variable {variable:?} uses an exploded composite modifier"
                )
            }
            Self::AmbiguousBoundary => {
                formatter.write_str("a capture overlaps its following literal boundary")
            }
            Self::UnboundedReservedCapture => {
                formatter.write_str("a reserved capture must be terminal")
            }
        }
    }
}

#[derive(Clone, Copy)]
struct OperatorProperties {
    first: &'static str,
    separator: &'static str,
    named: bool,
    if_empty: &'static str,
    allow_reserved: bool,
}

impl OperatorProperties {
    const fn new(
        first: &'static str,
        separator: &'static str,
        named: bool,
        if_empty: &'static str,
        allow_reserved: bool,
    ) -> Self {
        Self {
            first,
            separator,
            named,
            if_empty,
            allow_reserved,
        }
    }

    fn is_form_style(self) -> bool {
        matches!(self.first, "?" | "&")
    }
}

#[derive(Default)]
struct ExpansionState {
    composite_items: usize,
}

impl ExpansionState {
    fn inspect_composite(
        &mut self,
        count: usize,
        limits: UriTemplateExpansionLimits,
    ) -> Result<(), UriTemplateError> {
        let actual = self.composite_items.saturating_add(count);
        if actual > limits.max_composite_items() {
            return Err(UriTemplateError::TooManyCompositeItems {
                actual,
                maximum: limits.max_composite_items(),
            });
        }
        self.composite_items = actual;
        Ok(())
    }
}

struct ExpressionWriter<'a> {
    output: &'a mut String,
    properties: OperatorProperties,
    wrote_value: bool,
    limits: UriTemplateExpansionLimits,
}

impl<'a> ExpressionWriter<'a> {
    fn new(
        output: &'a mut String,
        properties: OperatorProperties,
        limits: UriTemplateExpansionLimits,
    ) -> Self {
        Self {
            output,
            properties,
            wrote_value: false,
            limits,
        }
    }

    fn begin_value(&mut self) -> Result<(), UriTemplateError> {
        let delimiter = if self.wrote_value {
            self.properties.separator
        } else {
            self.properties.first
        };
        append_output(self.output, delimiter, self.limits)?;
        self.wrote_value = true;
        Ok(())
    }

    fn write_value(
        &mut self,
        name: &str,
        value: &str,
        logically_empty: bool,
    ) -> Result<(), UriTemplateError> {
        self.begin_value()?;
        if self.properties.named {
            append_variable_name(self.output, name, self.limits)?;
            append_output(
                self.output,
                if logically_empty {
                    self.properties.if_empty
                } else {
                    "="
                },
                self.limits,
            )?;
        }
        append_encoded_value(
            self.output,
            value,
            self.properties.allow_reserved,
            self.limits,
        )
    }

    fn write_associative_pair(&mut self, key: &str, value: &str) -> Result<(), UriTemplateError> {
        self.begin_value()?;
        if self.properties.named {
            append_encoded_value(
                self.output,
                key,
                self.properties.allow_reserved,
                self.limits,
            )?;
            append_output(
                self.output,
                if value.is_empty() {
                    self.properties.if_empty
                } else {
                    "="
                },
                self.limits,
            )?;
            return append_encoded_value(
                self.output,
                value,
                self.properties.allow_reserved,
                self.limits,
            );
        }

        append_encoded_value(
            self.output,
            key,
            self.properties.allow_reserved,
            self.limits,
        )?;
        if !value.is_empty() || self.properties.is_form_style() {
            append_output(self.output, "=", self.limits)?;
        }
        append_encoded_value(
            self.output,
            value,
            self.properties.allow_reserved,
            self.limits,
        )
    }
}

fn push_part(
    parts: &mut Vec<UriTemplatePart>,
    part: UriTemplatePart,
) -> Result<(), UriTemplateError> {
    if parts.len() == MAX_URI_TEMPLATE_PARTS {
        return Err(UriTemplateError::TooManyParts {
            maximum: MAX_URI_TEMPLATE_PARTS,
        });
    }
    parts.push(part);
    Ok(())
}

fn push_literal(
    parts: &mut Vec<UriTemplatePart>,
    literal: &str,
    offset: usize,
) -> Result<(), UriTemplateError> {
    if literal.is_empty() {
        return Ok(());
    }
    if let Some(relative_offset) = invalid_literal_offset(literal) {
        return Err(UriTemplateError::InvalidLiteral {
            offset: offset + relative_offset,
        });
    }
    push_part(parts, UriTemplatePart::Literal(literal.to_owned()))
}

fn parse_expression(
    source: &str,
    source_offset: usize,
) -> Result<UriTemplateExpression, UriTemplateError> {
    let bytes = source.as_bytes();
    let Some(&first) = bytes.first() else {
        return Err(UriTemplateError::EmptyExpression {
            offset: source_offset,
        });
    };
    let (operator, variable_start) = match first {
        b'+' => (UriTemplateOperator::Reserved, 1),
        b'#' => (UriTemplateOperator::Fragment, 1),
        b'.' => (UriTemplateOperator::Label, 1),
        b'/' => (UriTemplateOperator::Path, 1),
        b';' => (UriTemplateOperator::PathParameter, 1),
        b'?' => (UriTemplateOperator::Query, 1),
        b'&' => (UriTemplateOperator::QueryContinuation, 1),
        b'=' | b',' | b'!' | b'@' | b'|' => {
            return Err(UriTemplateError::UnsupportedOperator {
                operator: char::from(first),
                offset: source_offset,
            });
        }
        _ => (UriTemplateOperator::Simple, 0),
    };
    let variables_source = &source[variable_start..];
    if variables_source.is_empty() {
        return Err(UriTemplateError::EmptyExpression {
            offset: source_offset + variable_start,
        });
    }

    let mut variables = Vec::new();
    let mut variable_offset = source_offset + variable_start;
    for variable_source in variables_source.split(',') {
        if variables.len() == MAX_URI_TEMPLATE_VARIABLES_PER_EXPRESSION {
            return Err(UriTemplateError::TooManyVariables {
                maximum: MAX_URI_TEMPLATE_VARIABLES_PER_EXPRESSION,
            });
        }
        variables.push(parse_variable(variable_source, variable_offset)?);
        variable_offset += variable_source.len() + 1;
    }
    Ok(UriTemplateExpression {
        operator,
        variables,
    })
}

fn parse_variable(
    source: &str,
    source_offset: usize,
) -> Result<UriTemplateVariable, UriTemplateError> {
    if source.is_empty() {
        return Err(UriTemplateError::InvalidVariable {
            offset: source_offset,
        });
    }

    let (name, modifier) = if let Some(name) = source.strip_suffix('*') {
        if name.is_empty() || name.contains(':') {
            return Err(UriTemplateError::InvalidVariable {
                offset: source_offset,
            });
        }
        (name, Some(UriTemplateModifier::Explode))
    } else if let Some((name, prefix)) = source.split_once(':') {
        let prefix_offset = source_offset + name.len();
        if !is_valid_prefix(prefix) {
            return Err(UriTemplateError::InvalidPrefix {
                offset: prefix_offset,
            });
        }
        let length = prefix
            .parse::<usize>()
            .map_err(|_| UriTemplateError::InvalidPrefix {
                offset: prefix_offset,
            })?;
        (name, Some(UriTemplateModifier::Prefix(length)))
    } else {
        (source, None)
    };

    if name.len() > MAX_URI_TEMPLATE_VARIABLE_NAME_BYTES {
        return Err(UriTemplateError::VariableNameTooLong {
            actual: name.len(),
            maximum: MAX_URI_TEMPLATE_VARIABLE_NAME_BYTES,
        });
    }
    if !is_valid_variable_name(name) {
        return Err(UriTemplateError::InvalidVariable {
            offset: source_offset,
        });
    }

    Ok(UriTemplateVariable {
        name: name.to_owned(),
        modifier,
    })
}

fn is_valid_prefix(prefix: &str) -> bool {
    let bytes = prefix.as_bytes();
    bytes.len() <= 4
        && bytes
            .first()
            .is_some_and(|byte| matches!(*byte, b'1'..=b'9'))
        && bytes.iter().all(u8::is_ascii_digit)
        && prefix
            .parse::<usize>()
            .is_ok_and(|length| length <= MAX_URI_TEMPLATE_PREFIX_LENGTH)
}

fn is_valid_variable_name(name: &str) -> bool {
    !name.is_empty() && name.split('.').all(is_valid_variable_name_segment)
}

fn is_valid_variable_name_segment(segment: &str) -> bool {
    if segment.is_empty() {
        return false;
    }
    let bytes = segment.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_' {
            index += 1;
        } else if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit()
        {
            index += 3;
        } else {
            return false;
        }
    }
    true
}

fn invalid_literal_offset(literal: &str) -> Option<usize> {
    let bytes = literal.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii() {
            if byte == b'%' {
                if index + 2 >= bytes.len()
                    || !bytes[index + 1].is_ascii_hexdigit()
                    || !bytes[index + 2].is_ascii_hexdigit()
                {
                    return Some(index);
                }
                index += 3;
            } else if is_valid_literal_byte(byte) {
                index += 1;
            } else {
                return Some(index);
            }
        } else {
            let Some(character) = literal[index..].chars().next() else {
                return Some(index);
            };
            if !is_valid_literal_character(character) {
                return Some(index);
            }
            index += character.len_utf8();
        }
    }
    None
}

fn is_valid_literal_character(character: char) -> bool {
    matches!(
        u32::from(character),
        0x00A0..=0xD7FF
            | 0xE000..=0xF8FF
            | 0xF900..=0xFDCF
            | 0xFDF0..=0xFFEF
            | 0x10000..=0x1FFFD
            | 0x20000..=0x2FFFD
            | 0x30000..=0x3FFFD
            | 0x40000..=0x4FFFD
            | 0x50000..=0x5FFFD
            | 0x60000..=0x6FFFD
            | 0x70000..=0x7FFFD
            | 0x80000..=0x8FFFD
            | 0x90000..=0x9FFFD
            | 0xA0000..=0xAFFFD
            | 0xB0000..=0xBFFFD
            | 0xC0000..=0xCFFFD
            | 0xD0000..=0xDFFFD
            | 0xE1000..=0xEFFFD
            | 0xF0000..=0xFFFFD
            | 0x100000..=0x10FFFD
    )
}

fn is_valid_literal_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'!'
            | b'#'..=b'$'
            | b'&'
            | b'('..=b';'
            | b'='
            | b'?'..=b'['
            | b']'
            | b'_'
            | b'a'..=b'z'
            | b'~'
    )
}

fn expand_expression(
    output: &mut String,
    expression: &UriTemplateExpression,
    values: &TemplateValues,
    limits: UriTemplateExpansionLimits,
    state: &mut ExpansionState,
) -> Result<(), UriTemplateError> {
    let properties = expression.operator.properties();
    let mut writer = ExpressionWriter::new(output, properties, limits);
    for variable in &expression.variables {
        let Some(value) = values.get(variable.name()) else {
            continue;
        };
        expand_variable(&mut writer, variable, value, state)?;
    }
    Ok(())
}

fn expand_variable(
    writer: &mut ExpressionWriter<'_>,
    variable: &UriTemplateVariable,
    value: &TemplateValue,
    state: &mut ExpansionState,
) -> Result<(), UriTemplateError> {
    match value {
        TemplateValue::Scalar(value) => {
            let value = scalar_prefix(value, variable, writer.limits)?;
            writer.write_value(variable.name(), value, value.is_empty())
        }
        TemplateValue::List(values) => {
            if values.is_empty() {
                return Ok(());
            }
            state.inspect_composite(values.len(), writer.limits)?;
            for value in values.iter().flatten() {
                check_value_bound(value, writer.limits)?;
            }
            if !values.iter().any(Option::is_some) {
                return Ok(());
            }
            reject_prefix_on_composite(variable)?;
            if variable.modifier() == Some(UriTemplateModifier::Explode) {
                for value in values.iter().flatten() {
                    writer.write_value(variable.name(), value, value.is_empty())?;
                }
                return Ok(());
            }

            writer.begin_value()?;
            if writer.properties.named {
                append_variable_name(writer.output, variable.name(), writer.limits)?;
                append_output(writer.output, "=", writer.limits)?;
            }
            let mut first = true;
            for value in values.iter().flatten() {
                if !first {
                    append_output(writer.output, ",", writer.limits)?;
                }
                append_encoded_value(
                    writer.output,
                    value,
                    writer.properties.allow_reserved,
                    writer.limits,
                )?;
                first = false;
            }
            Ok(())
        }
        TemplateValue::Associative(values) => {
            if values.is_empty() {
                return Ok(());
            }
            state.inspect_composite(values.len(), writer.limits)?;
            let mut unique_keys = BTreeSet::new();
            for (key, value) in values {
                if !unique_keys.insert(key.as_str()) {
                    return Err(UriTemplateError::DuplicateAssociativeKey { key: key.clone() });
                }
                check_value_bound(key, writer.limits)?;
                if let Some(value) = value {
                    check_value_bound(value, writer.limits)?;
                }
            }
            if !values.iter().any(|(_, value)| value.is_some()) {
                return Ok(());
            }
            reject_prefix_on_composite(variable)?;
            if variable.modifier() == Some(UriTemplateModifier::Explode) {
                for (key, value) in values {
                    let Some(value) = value else {
                        continue;
                    };
                    writer.write_associative_pair(key, value)?;
                }
                return Ok(());
            }

            writer.begin_value()?;
            if writer.properties.named {
                append_variable_name(writer.output, variable.name(), writer.limits)?;
                append_output(writer.output, "=", writer.limits)?;
            }
            let mut first = true;
            for (key, value) in values {
                let Some(value) = value else {
                    continue;
                };
                if !first {
                    append_output(writer.output, ",", writer.limits)?;
                }
                append_encoded_value(
                    writer.output,
                    key,
                    writer.properties.allow_reserved,
                    writer.limits,
                )?;
                append_output(writer.output, ",", writer.limits)?;
                append_encoded_value(
                    writer.output,
                    value,
                    writer.properties.allow_reserved,
                    writer.limits,
                )?;
                first = false;
            }
            Ok(())
        }
    }
}

fn scalar_prefix<'a>(
    value: &'a str,
    variable: &UriTemplateVariable,
    limits: UriTemplateExpansionLimits,
) -> Result<&'a str, UriTemplateError> {
    check_value_bound(value, limits)?;
    let value = match variable.modifier() {
        Some(UriTemplateModifier::Prefix(length)) => &value[..prefix_end(value, length)],
        Some(UriTemplateModifier::Explode) | None => value,
    };
    Ok(value)
}

fn reject_prefix_on_composite(variable: &UriTemplateVariable) -> Result<(), UriTemplateError> {
    if matches!(variable.modifier(), Some(UriTemplateModifier::Prefix(_))) {
        return Err(UriTemplateError::PrefixAppliedToComposite {
            variable: variable.name().to_owned(),
        });
    }
    Ok(())
}

fn prefix_end(value: &str, maximum_characters: usize) -> usize {
    let bytes = value.as_bytes();
    let mut index = 0;
    let mut characters = 0;
    while index < bytes.len() && characters < maximum_characters {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit()
        {
            index += pct_encoded_character_width(&bytes[index..]);
        } else {
            let Some(character) = value[index..].chars().next() else {
                break;
            };
            index += character.len_utf8();
        }
        characters += 1;
    }
    index
}

fn pct_encoded_character_width(source: &[u8]) -> usize {
    let first = decode_pct_encoded_octet(source);
    let utf8_octets = first.map_or(1, utf8_sequence_width);
    if !(2..=4).contains(&utf8_octets) || source.len() < utf8_octets * 3 {
        return 3;
    }

    let mut decoded = [0_u8; 4];
    for (index, slot) in decoded.iter_mut().take(utf8_octets).enumerate() {
        let offset = index * 3;
        let Some(octet) = decode_pct_encoded_octet(&source[offset..]) else {
            return 3;
        };
        *slot = octet;
    }
    if std::str::from_utf8(&decoded[..utf8_octets])
        .is_ok_and(|decoded| decoded.chars().count() == 1)
    {
        utf8_octets * 3
    } else {
        3
    }
}

fn decode_pct_encoded_octet(source: &[u8]) -> Option<u8> {
    if source.len() < 3 || source[0] != b'%' {
        return None;
    }
    Some((hex_value(source[1])? << 4) | hex_value(source[2])?)
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

const fn utf8_sequence_width(first: u8) -> usize {
    match first {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 1,
    }
}

fn check_value_bound(
    value: &str,
    limits: UriTemplateExpansionLimits,
) -> Result<(), UriTemplateError> {
    if value.len() > limits.max_value_bytes() {
        return Err(UriTemplateError::ValueTooLong {
            actual: value.len(),
            maximum: limits.max_value_bytes(),
        });
    }
    Ok(())
}

fn append_encoded_literal(
    output: &mut String,
    literal: &str,
    limits: UriTemplateExpansionLimits,
) -> Result<(), UriTemplateError> {
    append_percent_encoded(output, literal, true, true, limits)
}

fn append_variable_name(
    output: &mut String,
    name: &str,
    limits: UriTemplateExpansionLimits,
) -> Result<(), UriTemplateError> {
    append_percent_encoded(output, name, false, true, limits)
}

fn append_encoded_value(
    output: &mut String,
    value: &str,
    allow_reserved: bool,
    limits: UriTemplateExpansionLimits,
) -> Result<(), UriTemplateError> {
    append_percent_encoded(output, value, allow_reserved, allow_reserved, limits)
}

fn append_percent_encoded(
    output: &mut String,
    source: &str,
    allow_reserved: bool,
    preserve_pct_encoded: bool,
    limits: UriTemplateExpansionLimits,
) -> Result<(), UriTemplateError> {
    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if preserve_pct_encoded
            && byte == b'%'
            && index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit()
        {
            append_output(output, &source[index..index + 3], limits)?;
            index += 3;
        } else if is_unreserved(byte) || (allow_reserved && is_reserved(byte)) {
            append_byte(output, byte, limits)?;
            index += 1;
        } else {
            append_percent_triplet(output, byte, limits)?;
            index += 1;
        }
    }
    Ok(())
}

fn append_output(
    output: &mut String,
    fragment: &str,
    limits: UriTemplateExpansionLimits,
) -> Result<(), UriTemplateError> {
    let actual = output.len().saturating_add(fragment.len());
    if actual > limits.max_output_bytes() {
        return Err(UriTemplateError::ExpansionTooLarge {
            actual,
            maximum: limits.max_output_bytes(),
        });
    }
    output.push_str(fragment);
    Ok(())
}

fn append_byte(
    output: &mut String,
    byte: u8,
    limits: UriTemplateExpansionLimits,
) -> Result<(), UriTemplateError> {
    let actual = output.len().saturating_add(1);
    if actual > limits.max_output_bytes() {
        return Err(UriTemplateError::ExpansionTooLarge {
            actual,
            maximum: limits.max_output_bytes(),
        });
    }
    output.push(char::from(byte));
    Ok(())
}

fn append_percent_triplet(
    output: &mut String,
    byte: u8,
    limits: UriTemplateExpansionLimits,
) -> Result<(), UriTemplateError> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let actual = output.len().saturating_add(3);
    if actual > limits.max_output_bytes() {
        return Err(UriTemplateError::ExpansionTooLarge {
            actual,
            maximum: limits.max_output_bytes(),
        });
    }
    output.push('%');
    output.push(char::from(HEX[usize::from(byte >> 4)]));
    output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    Ok(())
}

const fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

const fn is_reserved(byte: u8) -> bool {
    matches!(
        byte,
        b':' | b'/'
            | b'?'
            | b'#'
            | b'['
            | b']'
            | b'@'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'='
    )
}

fn validate_reversible_expression(
    expression: &UriTemplateExpression,
    next_boundary: Option<&ReversibleBoundary>,
) -> Result<(), UriTemplateError> {
    match expression.operator() {
        UriTemplateOperator::Reserved | UriTemplateOperator::Fragment
            if next_boundary.is_some() =>
        {
            Err(UriTemplateError::NonReversibleTemplate {
                reason: UriTemplateMatchRejection::UnboundedReservedCapture,
            })
        }
        UriTemplateOperator::Simple
        | UriTemplateOperator::Label
        | UriTemplateOperator::Path
        | UriTemplateOperator::PathParameter
        | UriTemplateOperator::Query
        | UriTemplateOperator::QueryContinuation => {
            if next_boundary.is_some_and(|boundary| {
                !boundary
                    .as_str()
                    .as_bytes()
                    .first()
                    .is_some_and(|byte| is_reserved(*byte))
            }) {
                return Err(UriTemplateError::NonReversibleTemplate {
                    reason: UriTemplateMatchRejection::AmbiguousBoundary,
                });
            }
            Ok(())
        }
        UriTemplateOperator::Reserved | UriTemplateOperator::Fragment => Ok(()),
    }
}

fn reversible_adjacent_expression_boundary(
    expression: &UriTemplateExpression,
    next: &UriTemplateExpression,
) -> Result<ReversibleBoundary, UriTemplateError> {
    let prefix = match next.operator() {
        // These expansions emit no leading wire marker, so an adjacent prior
        // scalar has no unique point at which to stop.
        UriTemplateOperator::Simple | UriTemplateOperator::Reserved => {
            return Err(UriTemplateError::NonReversibleTemplate {
                reason: UriTemplateMatchRejection::AdjacentCaptures,
            });
        }
        UriTemplateOperator::Fragment => "#".to_owned(),
        UriTemplateOperator::Label => ".".to_owned(),
        UriTemplateOperator::Path => {
            // `{/first}{/second}` cannot distinguish a present first value
            // from a present second value when the other value is absent.
            if expression.operator() == UriTemplateOperator::Path {
                return Err(UriTemplateError::NonReversibleTemplate {
                    reason: UriTemplateMatchRejection::AmbiguousBoundary,
                });
            }
            "/".to_owned()
        }
        UriTemplateOperator::PathParameter => ";".to_owned(),
        UriTemplateOperator::Query => "?".to_owned(),
        UriTemplateOperator::QueryContinuation => "&".to_owned(),
    };

    Ok(ReversibleBoundary::ExpressionPrefix(prefix))
}

fn reverse_match_expression(
    expression: &ReversibleTemplateExpression,
    remainder: &str,
) -> Option<(Vec<(String, String)>, usize)> {
    if expression.variables.len() > 1 {
        return reverse_match_named_expression(expression, remainder);
    }
    let variable = expression.variables.first()?;
    let (capture, consumed) = match expression.operator {
        UriTemplateOperator::Simple => {
            reverse_match_unprefixed(remainder, expression.next_boundary.as_ref())
        }
        UriTemplateOperator::Reserved => reverse_match_unprefixed(remainder, None),
        UriTemplateOperator::Fragment => {
            reverse_match_prefixed(remainder, "#", expression.next_boundary.as_ref())
        }
        UriTemplateOperator::Label => {
            reverse_match_prefixed(remainder, ".", expression.next_boundary.as_ref())
        }
        UriTemplateOperator::Path => {
            reverse_match_prefixed(remainder, "/", expression.next_boundary.as_ref())
        }
        UriTemplateOperator::PathParameter => reverse_match_named(
            remainder,
            ";",
            variable,
            false,
            expression.next_boundary.as_ref(),
        ),
        UriTemplateOperator::Query => reverse_match_named(
            remainder,
            "?",
            variable,
            true,
            expression.next_boundary.as_ref(),
        ),
        UriTemplateOperator::QueryContinuation => reverse_match_named(
            remainder,
            "&",
            variable,
            true,
            expression.next_boundary.as_ref(),
        ),
    }?;
    let captures = capture
        .map(|capture| vec![(variable.name.clone(), capture)])
        .unwrap_or_default();
    Some((captures, consumed))
}

fn reverse_match_unprefixed(
    remainder: &str,
    next_boundary: Option<&ReversibleBoundary>,
) -> Option<(Option<String>, usize)> {
    let (capture, consumed) = reverse_capture_to_boundary(remainder, next_boundary)?;
    if capture.is_empty() {
        return Some((None, 0));
    }
    Some((Some(decode_percent_triplets_once(capture)?), consumed))
}

fn reverse_match_prefixed(
    remainder: &str,
    prefix: &str,
    next_boundary: Option<&ReversibleBoundary>,
) -> Option<(Option<String>, usize)> {
    let Some(after_prefix) = remainder.strip_prefix(prefix) else {
        return Some((None, 0));
    };
    let Some((capture, consumed)) = reverse_capture_to_boundary(after_prefix, next_boundary) else {
        // A path expression and its following literal may both begin with
        // `/`. If no complete literal boundary follows the consumed marker,
        // leave it for the literal part: the expression is undefined.
        return Some((None, 0));
    };
    Some((
        Some(decode_percent_triplets_once(capture)?),
        prefix.len().saturating_add(consumed),
    ))
}

fn reverse_match_named(
    remainder: &str,
    marker: &str,
    variable: &ReversibleTemplateVariable,
    requires_equals: bool,
    next_boundary: Option<&ReversibleBoundary>,
) -> Option<(Option<String>, usize)> {
    let mut prefix = String::with_capacity(marker.len() + variable.encoded_name.len() + 1);
    prefix.push_str(marker);
    prefix.push_str(&variable.encoded_name);
    let Some(after_name) = remainder.strip_prefix(&prefix) else {
        return Some((None, 0));
    };

    let (after_equals, equals_len) = if requires_equals {
        if let Some(after_equals) = after_name.strip_prefix('=') {
            (after_equals, 1)
        } else if after_name.is_empty() {
            (after_name, 0)
        } else {
            return Some((None, 0));
        }
    } else if let Some(after_equals) = after_name.strip_prefix('=') {
        (after_equals, 1)
    } else if after_name.is_empty()
        || after_name.starts_with(';')
        || next_boundary.is_some_and(|boundary| after_name.starts_with(boundary.as_str()))
    {
        // Empty matrix values omit '=', even immediately before the next part.
        (after_name, 0)
    } else {
        return Some((None, 0));
    };
    let Some((capture, consumed)) = reverse_capture_to_boundary(after_equals, next_boundary) else {
        return Some((None, 0));
    };
    Some((
        Some(decode_percent_triplets_once(capture)?),
        prefix
            .len()
            .saturating_add(equals_len)
            .saturating_add(consumed),
    ))
}

fn reverse_match_named_expression(
    expression: &ReversibleTemplateExpression,
    remainder: &str,
) -> Option<(Vec<(String, String)>, usize)> {
    let properties = expression.operator.properties();
    debug_assert!(properties.named);

    let requires_equals = matches!(
        expression.operator,
        UriTemplateOperator::Query | UriTemplateOperator::QueryContinuation
    );
    let mut captures = Vec::with_capacity(expression.variables.len());
    let mut offset = 0;
    let mut wrote_value = false;

    for variable in &expression.variables {
        let marker = if wrote_value {
            properties.separator
        } else {
            properties.first
        };
        let remainder = remainder.get(offset..)?;
        let Some(after_marker) = remainder.strip_prefix(marker) else {
            continue;
        };
        let Some(after_name) = after_marker.strip_prefix(&variable.encoded_name) else {
            continue;
        };

        let (after_equals, equals_len) = if requires_equals {
            let Some(after_equals) = after_name.strip_prefix('=') else {
                // A shared name prefix does not bind this variable. For example,
                // absent `id` must not prevent `identifier` from being matched.
                continue;
            };
            (after_equals, 1)
        } else if let Some(after_equals) = after_name.strip_prefix('=') {
            (after_equals, 1)
        } else if after_name.is_empty()
            || after_name.starts_with(properties.separator)
            || expression
                .next_boundary
                .as_ref()
                .is_some_and(|boundary| after_name.starts_with(boundary.as_str()))
        {
            // Leave the following literal or expression marker unconsumed.
            (after_name, 0)
        } else {
            continue;
        };

        let capture_len = reversible_named_capture_len(
            after_equals,
            properties.separator,
            expression.next_boundary.as_ref(),
        );
        let capture = after_equals.get(..capture_len)?;
        let value = decode_percent_triplets_once(capture)?;
        offset = offset
            .saturating_add(marker.len())
            .saturating_add(variable.encoded_name.len())
            .saturating_add(equals_len)
            .saturating_add(capture_len);
        captures.push((variable.name.clone(), value));
        wrote_value = true;
    }

    Some((captures, offset))
}

fn reversible_named_capture_len(
    remainder: &str,
    separator: &str,
    next_boundary: Option<&ReversibleBoundary>,
) -> usize {
    let mut capture_len = remainder.len();
    if let Some(offset) = remainder.find(separator) {
        capture_len = capture_len.min(offset);
    }
    if let Some(offset) = next_boundary.and_then(|boundary| remainder.find(boundary.as_str())) {
        capture_len = capture_len.min(offset);
    }
    capture_len
}

fn reverse_capture_to_boundary<'a>(
    remainder: &'a str,
    next_boundary: Option<&ReversibleBoundary>,
) -> Option<(&'a str, usize)> {
    match next_boundary {
        Some(boundary) => {
            if let Some(offset) = remainder.find(boundary.as_str()) {
                Some((&remainder[..offset], offset))
            } else if boundary.permits_absent_following_expression() {
                Some((remainder, remainder.len()))
            } else {
                None
            }
        }
        None => Some((remainder, remainder.len())),
    }
}

fn decode_percent_triplets_once(capture: &str) -> Option<String> {
    let bytes = capture.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = *bytes.get(index + 1)?;
            let low = *bytes.get(index + 2)?;
            decoded.push((hex_value(high)? << 4) | hex_value(low)?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn contains_pct_encoded_triplet(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.windows(3).any(|window| {
        window[0] == b'%' && window[1].is_ascii_hexdigit() && window[2].is_ascii_hexdigit()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reversible_named_expressions_round_trip_optional_prefix_names() {
        let samples = [None, Some(""), Some("42"), Some("café/a"), Some("%2F")];
        for source in [
            "mcp://items{?id,identifier,id.tag}",
            "mcp://items?fixed=1{&id,identifier,id.tag}",
            "mcp://items{;id,identifier,id.tag}",
        ] {
            let matcher = ReversibleResourceTemplate::compile(
                UriTemplate::parse(source).expect("named template parses"),
            )
            .expect("named scalar expressions are reversible");
            for id in samples {
                for identifier in samples {
                    for tag in samples {
                        let values: TemplateValues = [
                            ("id", id),
                            ("identifier", identifier),
                            ("id.tag", tag),
                        ]
                        .into_iter()
                        .filter_map(|(name, value)| {
                            value.map(|value| (name.to_owned(), TemplateValue::scalar(value)))
                        })
                        .collect();
                        let uri = matcher.expand(&values).expect("scalar values expand");
                        assert_eq!(
                            matcher.match_uri(&uri).expect("matching stays within bounds"),
                            Some(values),
                            "optional bindings must survive round-trip: {source} -> {uri}",
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn reversible_empty_matrix_parameters_before_literal_boundaries() {
        for names in ["flag", "unused,flag", "flag,unused"] {
            for suffix in ["/tail", "?fixed=1", "#view"] {
                let source = format!("mcp://items{{;{names}}}{suffix}");
                let matcher = ReversibleResourceTemplate::compile(
                    UriTemplate::parse(&source).expect("matrix template parses"),
                )
                .expect("the suffix supplies a reversible boundary");
                let values = TemplateValues::from([(
                    "flag".to_owned(),
                    TemplateValue::scalar(""),
                )]);
                let uri = format!("mcp://items;flag{suffix}");
                assert_eq!(matcher.expand(&values).expect("empty scalar expands"), uri);
                assert_eq!(
                    matcher.match_uri(&uri).expect("matching stays within bounds"),
                    Some(values),
                    "an empty matrix parameter must survive before {suffix}",
                );
                let absent = TemplateValues::new();
                let absent_uri = format!("mcp://items{suffix}");
                assert_eq!(
                    matcher.expand(&absent).expect("undefined value expands"),
                    absent_uri,
                );
                assert_eq!(
                    matcher.match_uri(&absent_uri).expect("undefined value matches"),
                    Some(absent),
                    "omitting the parameter must not manufacture an empty binding",
                );
            }
        }
    }

    #[test]
    fn reversible_empty_matrix_parameters_before_optional_query_expressions() {
        for names in ["flag", "unused,flag", "flag,unused"] {
            let source = format!("mcp://items{{;{names}}}{{?page}}");
            let matcher = ReversibleResourceTemplate::compile(
                UriTemplate::parse(&source).expect("adjacent named expressions parse"),
            )
            .expect("distinct expression markers are reversible");
            for page in [None, Some("2")] {
                let mut values = TemplateValues::from([(
                    "flag".to_owned(),
                    TemplateValue::scalar(""),
                )]);
                let uri = if let Some(page) = page {
                    values.insert("page".to_owned(), TemplateValue::scalar(page));
                    format!("mcp://items;flag?page={page}")
                } else {
                    "mcp://items;flag".to_owned()
                };
                assert_eq!(matcher.expand(&values).expect("named values expand"), uri);
                assert_eq!(
                    matcher.match_uri(&uri).expect("matching stays within bounds"),
                    Some(values),
                    "the following query must remain optional: {source} -> {uri}",
                );
            }
        }
    }

    #[test]
    fn reversible_query_prefix_matching_keeps_invalid_candidates_rejected() {
        let matcher = ReversibleResourceTemplate::compile(
            UriTemplate::parse("mcp://items{?id,identifier}").expect("query template parses"),
        )
        .expect("named query variables are reversible");
        let values = TemplateValues::from([(
            "identifier".to_owned(),
            TemplateValue::scalar("42"),
        )]);
        assert_eq!(
            matcher
                .match_uri("mcp://items?identifier=42")
                .expect("valid URI matches"),
            Some(values),
        );
        for uri in [
            "mcp://items?identifierExtra=42",
            "mcp://items?identifier",
            "mcp://items?identifier=42&id=1",
            "mcp://items?identifier=%Q0",
            "mcp://items?identifier=%FF",
            "mcp://items?identifier=%34%32",
        ] {
            assert_eq!(
                matcher.match_uri(uri).expect("invalid URI is a bounded non-match"),
                None,
                "unknown names, malformed values, and noncanonical order remain rejected: {uri}",
            );
        }
    }

    #[test]
    fn reversible_matrix_boundary_matching_keeps_invalid_candidates_rejected() {
        let matcher = ReversibleResourceTemplate::compile(
            UriTemplate::parse("mcp://items{;flag}/tail").expect("matrix template parses"),
        )
        .expect("literal suffix supplies a reversible boundary");
        let values = TemplateValues::from([(
            "flag".to_owned(),
            TemplateValue::scalar(""),
        )]);
        assert_eq!(
            matcher
                .match_uri("mcp://items;flag/tail")
                .expect("valid URI matches"),
            Some(values),
        );
        for uri in [
            "mcp://items;flag=/tail",
            "mcp://items;flagship/tail",
            "mcp://items;flag/other",
            "mcp://items;flag/tail/trailing",
            "mcp://items;flag=%FF/tail",
            "mcp://items;flag=%/tail",
        ] {
            assert_eq!(
                matcher.match_uri(uri).expect("invalid URI is a bounded non-match"),
                None,
                "wrong names, malformed values, and changed suffixes remain rejected: {uri}",
            );
        }
    }

    fn values() -> TemplateValues {
        let mut values = TemplateValues::new();
        values.insert("var".to_owned(), TemplateValue::scalar("value"));
        values.insert("hello".to_owned(), TemplateValue::scalar("Hello World!"));
        values.insert("path".to_owned(), TemplateValue::scalar("/foo/bar"));
        values.insert(
            "list".to_owned(),
            TemplateValue::list(vec![
                "red".to_owned(),
                "green".to_owned(),
                "blue".to_owned(),
            ]),
        );
        values.insert(
            "keys".to_owned(),
            TemplateValue::associative(vec![
                ("semi".to_owned(), ";".to_owned()),
                ("dot".to_owned(), ".".to_owned()),
                ("comma".to_owned(), ",".to_owned()),
            ]),
        );
        values
    }

    #[test]
    fn rfc6570_level_four_expansion_positive() {
        let values = values();
        let examples = [
            ("{var:3}", "val"),
            ("{+path}", "/foo/bar"),
            ("{#hello}", "#Hello%20World!"),
            ("X{.list*}", "X.red.green.blue"),
            ("{/list*,path:4}", "/red/green/blue/%2Ffoo"),
            ("{;keys*}", ";semi=%3B;dot=.;comma=%2C"),
            ("{?keys*}", "?semi=%3B&dot=.&comma=%2C"),
            (
                "?fixed=yes{&list*}",
                "?fixed=yes&list=red&list=green&list=blue",
            ),
            ("{keys}", "semi,%3B,dot,.,comma,%2C"),
            ("{+keys*}", "semi=;,dot=.,comma=,"),
            ("{hello}", "Hello%20World%21"),
        ];

        for (template, expected) in examples {
            let parsed = UriTemplate::parse(template).expect("RFC 6570 example parses");
            assert_eq!(parsed.source(), template);
            assert_eq!(
                parsed.expand(&values).expect("RFC example expands"),
                expected
            );
        }
    }

    #[test]
    fn rfc6570_unicode_and_existing_percent_triplets_positive() {
        let template = UriTemplate::parse("https://example.test/é%27s/{term}/{+encoded}")
            .expect("printable Unicode literal and RFC expressions parse");
        let mut values = TemplateValues::new();
        values.insert("term".to_owned(), TemplateValue::scalar("café"));
        values.insert("encoded".to_owned(), TemplateValue::scalar("50%25"));

        assert_eq!(
            template
                .expand(&values)
                .expect("Unicode expands as UTF-8 percent triplets"),
            "https://example.test/%C3%A9%27s/caf%C3%A9/50%25"
        );
        assert_eq!(
            UriTemplate::parse("{encoded}")
                .expect("simple expansion parses")
                .expand(&values)
                .expect("simple expansion percent-encodes a supplied triplet once"),
            "50%2525"
        );
    }

    #[test]
    fn rfc6570_literal_percent_triplets_are_admitted_without_normalizing_bare_percent() {
        let template = UriTemplate::parse("mcp://resource/reports%2Fdaily")
            .expect("a complete literal percent triplet is valid RFC 6570");
        assert_eq!(
            template
                .expand(&TemplateValues::new())
                .expect("a valid literal expands unchanged"),
            "mcp://resource/reports%2Fdaily"
        );

        assert_eq!(
            UriTemplate::parse("mcp://resource/reports%Qdaily"),
            Err(UriTemplateError::InvalidLiteral {
                offset: "mcp://resource/reports".len(),
            }),
            "changing only the final percent triplet to a bare percent rejects rather than rewriting it"
        );
    }

    #[test]
    fn rfc6570_literal_grammar_accepts_final_ucschar_and_rejects_nearby_exclusions() {
        let admitted = format!(
            "mcp://resource/{}",
            char::from_u32(0xE1000).expect("E1000 is a Unicode scalar")
        );
        UriTemplate::parse(&admitted).expect("the first scalar in RFC 6570's final ucschar range");

        let excluded_ucschar = format!(
            "mcp://resource/{}",
            char::from_u32(0xE0FFF).expect("E0FFF is a Unicode scalar")
        );
        assert!(matches!(
            UriTemplate::parse(&excluded_ucschar),
            Err(UriTemplateError::InvalidLiteral { .. })
        ));
        assert!(matches!(
            UriTemplate::parse("mcp://resource/raw'apostrophe"),
            Err(UriTemplateError::InvalidLiteral { .. })
        ));
        UriTemplate::parse("mcp://resource/encoded%27apostrophe")
            .expect("pct-encoded apostrophe remains a valid literal");
    }

    #[test]
    fn rfc6570_literal_grammar_admits_supplementary_iprivate_ranges() {
        for scalar in [0xF0000, 0xFFFFD, 0x100000, 0x10FFFD] {
            let template = format!(
                "mcp://resource/{}",
                char::from_u32(scalar).expect("the RFC 3987 iprivate scalar is valid Unicode"),
            );
            UriTemplate::parse(&template)
                .expect("RFC 6570 literals admit every supplementary iprivate endpoint");
        }

        for scalar in [0xEFFFE, 0xEFFFF, 0xFFFFE, 0x10FFFE] {
            let template = format!(
                "mcp://resource/{}",
                char::from_u32(scalar).expect("the adjacent scalar is valid Unicode"),
            );
            assert!(matches!(
                UriTemplate::parse(&template),
                Err(UriTemplateError::InvalidLiteral { .. })
            ));
        }
    }

    #[test]
    fn rfc6570_bounds_apply_before_ownership_and_prefix_projection() {
        let oversized_source = "x".repeat(MAX_URI_TEMPLATE_BYTES + 1);
        assert_eq!(
            UriTemplate::parse(&oversized_source),
            Err(UriTemplateError::SourceTooLong {
                actual: MAX_URI_TEMPLATE_BYTES + 1,
                maximum: MAX_URI_TEMPLATE_BYTES,
            })
        );

        let template = UriTemplate::parse("{value:1}").expect("prefix expression parses");
        let mut values = TemplateValues::new();
        values.insert("value".to_owned(), TemplateValue::scalar("ab"));
        let limits =
            UriTemplateExpansionLimits::new(16, 1, 1).expect("one-byte scalar limit is valid");
        assert_eq!(
            template.expand_with_limits(&values, limits),
            Err(UriTemplateError::ValueTooLong {
                actual: 2,
                maximum: 1,
            })
        );
    }

    #[test]
    fn rfc6570_prefix_counts_percent_encoded_utf8_as_one_unicode_scalar() {
        let template = UriTemplate::parse("{+value:1}").expect("prefix expression parses");
        let mut values = TemplateValues::new();
        values.insert("value".to_owned(), TemplateValue::scalar("%C3%A9clair"));

        assert_eq!(
            template
                .expand(&values)
                .expect("encoded Unicode scalar is not split between octets"),
            "%C3%A9"
        );
    }

    #[test]
    fn rfc6570_undefined_composite_members_are_ignored_without_losing_empty_values() {
        let template = UriTemplate::parse("{?list*,keys*}").expect("composite expression parses");
        let mut values = TemplateValues::new();
        values.insert(
            "list".to_owned(),
            TemplateValue::list_with_undefined(vec![
                None,
                Some("one".to_owned()),
                Some(String::new()),
                None,
            ]),
        );
        values.insert(
            "keys".to_owned(),
            TemplateValue::associative_with_undefined(vec![
                ("ignored".to_owned(), None),
                ("second".to_owned(), Some("2".to_owned())),
            ]),
        );

        assert_eq!(
            template
                .expand(&values)
                .expect("only defined composite members expand"),
            "?list=one&list=&second=2"
        );

        values.insert(
            "list".to_owned(),
            TemplateValue::list_with_undefined(vec![None]),
        );
        values.insert(
            "keys".to_owned(),
            TemplateValue::associative_with_undefined(vec![("ignored".to_owned(), None)]),
        );
        assert_eq!(
            template
                .expand(&values)
                .expect("all-undefined composites are undefined variables"),
            ""
        );
        assert_eq!(
            UriTemplate::parse("{list:1}{keys:1}")
                .expect("prefix modifiers parse independently of runtime value types")
                .expand(&values)
                .expect("an undefined composite is ignored before modifier semantics"),
            ""
        );
    }

    #[test]
    fn rfc6570_associative_order_is_lossless_and_duplicates_fail_closed() {
        let template = UriTemplate::parse("{keys}").expect("associative expression parses");
        let mut values = TemplateValues::new();
        values.insert(
            "keys".to_owned(),
            TemplateValue::associative(vec![
                ("second".to_owned(), "2".to_owned()),
                ("first".to_owned(), "1".to_owned()),
            ]),
        );
        assert_eq!(
            template.expand(&values).expect("ordered pairs expand"),
            "second,2,first,1"
        );

        let before = values.clone();
        values.insert(
            "keys".to_owned(),
            TemplateValue::associative(vec![
                ("same".to_owned(), "1".to_owned()),
                ("same".to_owned(), "2".to_owned()),
            ]),
        );
        let planted_before = values.clone();
        assert_eq!(
            template.expand(&values),
            Err(UriTemplateError::DuplicateAssociativeKey {
                key: "same".to_owned(),
            })
        );
        assert_ne!(
            planted_before, before,
            "negative changes only the associative input"
        );
        assert_eq!(
            values, planted_before,
            "rejected expansion leaves caller values unchanged"
        );
    }

    #[test]
    fn rfc6570_level_four_near_negative_rejects_invalid_prefix_without_mutation() {
        let accepted = "mcp://resources/{term:3}{?cursor}";
        let planted = "mcp://resources/{term:0}{?cursor}";
        let parsed = UriTemplate::parse(accepted).expect("positive control parses");
        let before = parsed.clone();

        let error = UriTemplate::parse(planted)
            .expect_err("changing only the positive prefix length to zero must reject");
        assert!(matches!(error, UriTemplateError::InvalidPrefix { .. }));
        assert_eq!(
            parsed, before,
            "rejected input cannot mutate an admitted AST"
        );
        assert_eq!(parsed.source(), accepted);
    }

    #[test]
    fn rfc6570_level_four_near_negative_rejects_composite_overflow_without_mutation() {
        let template = UriTemplate::parse("{?items*}").expect("positive control parses");
        let mut values = TemplateValues::new();
        values.insert(
            "items".to_owned(),
            TemplateValue::list(vec!["one".to_owned(), "two".to_owned()]),
        );
        let before = values.clone();
        let limits = UriTemplateExpansionLimits::new(128, 1, 16)
            .expect("the stricter configured limits are valid");

        let error = template
            .expand_with_limits(&values, limits)
            .expect_err("changing only the allowed member count must reject the same input");
        assert_eq!(
            error,
            UriTemplateError::TooManyCompositeItems {
                actual: 2,
                maximum: 1,
            }
        );
        assert_eq!(
            values, before,
            "rejected expansion leaves caller values unchanged"
        );
    }

    #[test]
    fn reversible_resource_template_decodes_populated_and_absent_path_captures() {
        let template = UriTemplate::parse("mcp://resource{/collection}/manifest{?revision}")
            .expect("the RFC 6570 template parses");
        let matcher = template
            .compile_reversible()
            .expect("separated scalar captures compile deterministically");
        let mut values = TemplateValues::new();
        values.insert(
            "collection".to_owned(),
            TemplateValue::scalar("books/fiction"),
        );
        values.insert("revision".to_owned(), TemplateValue::scalar("2026-08-09"));

        let uri = matcher
            .expand(&values)
            .expect("declared scalar values expand");
        assert_eq!(
            uri,
            "mcp://resource/books%2Ffiction/manifest?revision=2026-08-09"
        );
        assert_eq!(
            matcher
                .match_uri(&uri)
                .expect("reverse matching is bounded"),
            Some(values),
            "a populated path capture decodes its percent triplet exactly once"
        );

        let omitted = TemplateValues::new();
        let omitted_uri = matcher
            .expand(&omitted)
            .expect("undefined values omit their whole expressions");
        assert_eq!(omitted_uri, "mcp://resource/manifest");
        assert_eq!(
            matcher
                .match_uri(&omitted_uri)
                .expect("omitted values reverse match"),
            Some(omitted),
            "an omitted path capture does not consume its following literal"
        );
    }

    #[test]
    fn reversible_resource_template_round_trips_raw_scalar_input() {
        let matcher = UriTemplate::parse("mcp://resource/{value}")
            .expect("the simple template parses")
            .compile_reversible()
            .expect("a terminal simple capture is deterministic");
        let mut values = TemplateValues::new();
        values.insert("value".to_owned(), TemplateValue::scalar("books/fiction"));

        let uri = matcher
            .expand(&values)
            .expect("simple expansion percent-encodes a raw slash");
        assert_eq!(uri, "mcp://resource/books%2Ffiction");
        assert_eq!(
            matcher
                .match_uri(&uri)
                .expect("reverse matching is bounded"),
            Some(values),
            "reverse matching decodes the URI triplet back to the raw scalar"
        );
    }

    #[test]
    fn reversible_resource_template_preescaped_simple_scalar_is_not_preescaped() {
        let matcher = UriTemplate::parse("mcp://resource/{value}")
            .expect("the simple template parses")
            .compile_reversible()
            .expect("a terminal simple capture is deterministic");
        let mut values = TemplateValues::new();
        values.insert("value".to_owned(), TemplateValue::scalar("books%2Ffiction"));

        let uri = matcher
            .expand(&values)
            .expect("simple expansion encodes a percent as data");
        assert_eq!(uri, "mcp://resource/books%252Ffiction");
        assert_ne!(uri, "mcp://resource/books%2Ffiction");
        assert_eq!(
            matcher
                .match_uri(&uri)
                .expect("reverse matching is bounded"),
            Some(values),
            "one decode restores the preescaped scalar without treating it as wire syntax"
        );
    }

    #[test]
    fn reversible_resource_template_reserved_and_fragment_raw_values_round_trip() {
        let cases = [
            (
                "mcp://resource/{+value}",
                "docs/guide?draft=true",
                "mcp://resource/docs/guide?draft=true",
            ),
            (
                "mcp://resource{#value}",
                "docs/guide?draft=true",
                "mcp://resource#docs/guide?draft=true",
            ),
        ];

        for (template, raw_value, expected_uri) in cases {
            let matcher = UriTemplate::parse(template)
                .expect("the reserved or fragment template parses")
                .compile_reversible()
                .expect("a terminal reserved or fragment capture is deterministic");
            let mut values = TemplateValues::new();
            values.insert("value".to_owned(), TemplateValue::scalar(raw_value));

            let uri = matcher
                .expand(&values)
                .expect("raw reserved characters expand without pre-escaping");
            assert_eq!(uri, expected_uri);
            assert_eq!(
                matcher
                    .match_uri(&uri)
                    .expect("reverse matching is bounded"),
                Some(values),
                "the raw scalar round-trips through the {template} inverse"
            );
        }
    }

    #[test]
    fn reversible_resource_template_reserved_and_fragment_preescaped_scalars_reject_without_mutation()
     {
        for template in ["mcp://resource/{+value}", "mcp://resource{#value}"] {
            let matcher = UriTemplate::parse(template)
                .expect("the reserved or fragment template parses")
                .compile_reversible()
                .expect("a terminal reserved or fragment capture is deterministic");
            let matcher_before = matcher.clone();
            let mut values = TemplateValues::new();
            values.insert("value".to_owned(), TemplateValue::scalar("docs%2Fguide"));
            let values_before = values.clone();

            assert_eq!(
                matcher.expand(&values),
                Err(UriTemplateError::PreescapedReservedMatchValue {
                    variable: "value".to_owned(),
                }),
                "a preescaped scalar is ambiguous for the {template} inverse"
            );
            assert_eq!(
                matcher, matcher_before,
                "rejected expansion changes no matcher state"
            );
            assert_eq!(
                values, values_before,
                "rejected expansion changes no caller values"
            );
        }
    }

    #[test]
    fn reversible_resource_template_scalar_explode_operator_matrix_round_trips() {
        let cases = [
            ("mcp://resource/{value*}", "mcp://resource/one%2Ftwo"),
            ("mcp://resource/{+value*}", "mcp://resource/one/two"),
            ("mcp://resource{#value*}", "mcp://resource#one/two"),
            ("mcp://resource{.value*}", "mcp://resource.one%2Ftwo"),
            ("mcp://resource{/value*}", "mcp://resource/one%2Ftwo"),
            ("mcp://resource{;value*}", "mcp://resource;value=one%2Ftwo"),
            ("mcp://resource{?value*}", "mcp://resource?value=one%2Ftwo"),
            (
                "mcp://resource?fixed=true{&value*}",
                "mcp://resource?fixed=true&value=one%2Ftwo",
            ),
        ];

        for (source, expected_uri) in cases {
            let matcher = UriTemplate::parse(source)
                .expect("each RFC 6570 operator parses")
                .compile_reversible()
                .expect("an exploded scalar has the same reversible wire form as a scalar");
            let values =
                TemplateValues::from([("value".to_owned(), TemplateValue::scalar("one/two"))]);
            let uri = matcher
                .expand(&values)
                .expect("the admitted scalar values expand");
            assert_eq!(uri, expected_uri, "{source}");
            assert_eq!(
                matcher.match_uri(&uri).expect("matching is bounded"),
                Some(values),
                "{source} extracts the same scalar binding it expanded"
            );
        }
    }

    #[test]
    fn reversible_resource_template_named_multi_variable_expression_round_trips_omission() {
        let matcher = UriTemplate::parse("mcp://resource{?first*,second*}")
            .expect("the named Level 4 expression parses")
            .compile_reversible()
            .expect("wire names make named scalar omissions reversible");
        let cases = [
            (TemplateValues::new(), "mcp://resource"),
            (
                TemplateValues::from([("first".to_owned(), TemplateValue::scalar("one/two"))]),
                "mcp://resource?first=one%2Ftwo",
            ),
            (
                TemplateValues::from([("second".to_owned(), TemplateValue::scalar("three"))]),
                "mcp://resource?second=three",
            ),
            (
                TemplateValues::from([
                    ("first".to_owned(), TemplateValue::scalar("one/two")),
                    ("second".to_owned(), TemplateValue::scalar("three")),
                ]),
                "mcp://resource?first=one%2Ftwo&second=three",
            ),
        ];

        for (values, expected_uri) in cases {
            let uri = matcher.expand(&values).expect("named scalar values expand");
            assert_eq!(uri, expected_uri);
            assert_eq!(
                matcher.match_uri(&uri).expect("matching is bounded"),
                Some(values),
                "the names distinguish omitted variables from present ones"
            );
        }
    }

    #[test]
    fn reversible_resource_template_named_multi_variable_stops_at_next_expression() {
        let matcher = UriTemplate::parse("mcp://resource{?first*,second*}{#fragment*}")
            .expect("the adjacent named and fragment expressions parse")
            .compile_reversible()
            .expect("their distinct wire markers make the boundary reversible");
        let values = TemplateValues::from([
            ("first".to_owned(), TemplateValue::scalar("one")),
            ("second".to_owned(), TemplateValue::scalar("two")),
            ("fragment".to_owned(), TemplateValue::scalar("three/four")),
        ]);
        let uri = matcher
            .expand(&values)
            .expect("the named values and following fragment expand");
        assert_eq!(uri, "mcp://resource?first=one&second=two#three/four");
        assert_eq!(
            matcher.match_uri(&uri).expect("matching is bounded"),
            Some(values),
            "the query capture stops before the following fragment marker"
        );
    }

    #[test]
    fn reversible_resource_template_near_negative_rejects_lossy_or_composite_without_mutation() {
        let accepted = "mcp://resource{/collection}/manifest{?revision}";
        let planted_lossy = "mcp://resource{/collection:3}/manifest{?revision}";
        let matcher = UriTemplate::parse(accepted)
            .expect("positive control parses")
            .compile_reversible()
            .expect("positive control compiles");
        let matcher_before = matcher.clone();
        let mut values = TemplateValues::new();
        values.insert("collection".to_owned(), TemplateValue::scalar("books"));
        values.insert("revision".to_owned(), TemplateValue::scalar("1"));
        let values_before = values.clone();

        let error = UriTemplate::parse(planted_lossy)
            .expect("changing only the scalar modifier remains valid RFC 6570")
            .compile_reversible()
            .expect_err("a lossy prefix has no deterministic reverse match");
        assert_eq!(
            error,
            UriTemplateError::NonReversibleTemplate {
                reason: UriTemplateMatchRejection::LossyPrefix {
                    variable: "collection".to_owned(),
                },
            }
        );
        assert_eq!(
            matcher, matcher_before,
            "rejected compilation changes no matcher state"
        );
        assert_eq!(
            values, values_before,
            "rejected compilation changes no caller values"
        );

        values.insert(
            "collection".to_owned(),
            TemplateValue::list(vec!["books".to_owned()]),
        );
        let planted_values_before = values.clone();
        assert_eq!(
            matcher.expand(&values),
            Err(UriTemplateError::NonScalarMatchValue {
                variable: "collection".to_owned(),
            }),
            "changing only the declared scalar into a composite must reject"
        );
        assert_eq!(
            values, planted_values_before,
            "rejected composite expansion leaves caller values unchanged"
        );
    }

    #[test]
    fn reversible_resource_template_match_input_bound_rejects_before_capture() {
        let matcher = UriTemplate::parse("mcp://resource/{value*}")
            .expect("the scalar Level 4 template parses")
            .compile_reversible()
            .expect("the scalar Level 4 template compiles");
        let matcher_before = matcher.clone();
        let prefix = "mcp://resource/";
        let oversized_uri = format!(
            "{prefix}{}",
            "x".repeat(MAX_URI_TEMPLATE_MATCH_INPUT_BYTES - prefix.len() + 1)
        );

        assert_eq!(
            matcher.match_uri(&oversized_uri),
            Err(UriTemplateError::MatchInputTooLong {
                actual: MAX_URI_TEMPLATE_MATCH_INPUT_BYTES + 1,
                maximum: MAX_URI_TEMPLATE_MATCH_INPUT_BYTES,
            })
        );
        assert_eq!(
            matcher, matcher_before,
            "a rejected candidate cannot mutate the compiled matcher"
        );
    }

    #[test]
    fn reversible_resource_template_adjacent_path_query_round_trips_exactly() {
        let accepted = "mcp://resource{/collection}{?revision}";
        let planted_ambiguous = "mcp://resource{/collection}{/revision}";
        let matcher = UriTemplate::parse(accepted)
            .expect("the positive control parses")
            .compile_reversible()
            .expect("a path capture is separable from the following query marker");
        let matcher_before = matcher.clone();
        let mut values = TemplateValues::new();
        values.insert(
            "collection".to_owned(),
            TemplateValue::scalar("books/fiction"),
        );
        values.insert("revision".to_owned(), TemplateValue::scalar("2026-08-09"));
        let values_before = values.clone();

        let uri = matcher
            .expand(&values)
            .expect("the separated scalar values expand");
        assert_eq!(uri, "mcp://resource/books%2Ffiction?revision=2026-08-09");
        let round_tripped = matcher
            .match_uri(&uri)
            .expect("reverse matching is bounded")
            .expect("the exact expansion belongs to the matcher language");
        assert_eq!(round_tripped, values);
        assert_eq!(
            matcher
                .expand(&round_tripped)
                .expect("the recovered bindings re-expand"),
            uri,
            "a reversible match must preserve the exact wire URI"
        );

        let mut path_only = TemplateValues::new();
        path_only.insert("collection".to_owned(), TemplateValue::scalar("books"));
        let path_only_uri = matcher
            .expand(&path_only)
            .expect("the query expression may be absent");
        assert_eq!(path_only_uri, "mcp://resource/books");
        assert_eq!(
            matcher
                .match_uri(&path_only_uri)
                .expect("matching an absent following expression is bounded"),
            Some(path_only),
            "the first capture remains exact when the query marker is absent"
        );

        let error = UriTemplate::parse(planted_ambiguous)
            .expect(
                "changing only the following query marker to a path marker remains valid RFC 6570",
            )
            .compile_reversible()
            .expect_err("identical path markers cannot distinguish an omitted adjacent scalar");
        assert_eq!(
            error,
            UriTemplateError::NonReversibleTemplate {
                reason: UriTemplateMatchRejection::AmbiguousBoundary,
            }
        );
        assert_eq!(
            matcher, matcher_before,
            "rejected adjacent syntax changes no admitted matcher state"
        );
        assert_eq!(
            values, values_before,
            "rejected adjacent syntax changes no caller bindings"
        );
    }

    #[test]
    fn reversible_resource_template_rejects_adjacent_captures() {
        let template = UriTemplate::parse("mcp://resource/{first}{second}")
            .expect("the syntactically valid template parses");
        assert_eq!(
            template.compile_reversible(),
            Err(UriTemplateError::NonReversibleTemplate {
                reason: UriTemplateMatchRejection::AdjacentCaptures,
            }),
            "adjacent captures admit multiple splits and cannot become a handler matcher"
        );
    }
}
