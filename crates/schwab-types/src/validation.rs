//! Typed errors for request validation (`OrderRequest::validate` and the
//! query-parameter validators).
//!
//! The message is written for a person; [`ValidationError::kind`] and
//! [`ValidationError::field`] are for code that branches on the failure.

use std::fmt;

/// What a [`ValidationError`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ValidationErrorKind {
    /// A required field or part is missing: a symbol, a leg, the order
    /// type a leg-bearing order needs.
    Missing,
    /// A field this request does not take: a price on a market order, a
    /// mutual-fund field on an equity leg.
    NotAllowed,
    /// A value outside Schwab's bounds or tables: not positive, a reversed
    /// window, a period its period type does not list.
    OutOfRange,
    /// A value that can appear in Schwab's responses but cannot be sent: an
    /// `Unknown` enum value, or an order type, strategy or asset type that
    /// cannot be submitted.
    Unsupported,
    /// A malformed symbol, including an OSI option symbol.
    Symbol,
    /// A fractional quantity where only whole shares or contracts trade.
    NotWhole,
    /// The order's shape: strategy nesting, OCO and TRIGGER children, how
    /// legs and quantities relate, which sessions and durations fit.
    Structure,
    /// A price that is not a multiple of the supplied tick size.
    Tick,
}

/// A request refused before anything is sent, with the field at fault in
/// Schwab's wire spelling when there is one (`price`,
/// `orderLegCollection[1].quantity`, `childOrderStrategies[0].stopPrice`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    kind: ValidationErrorKind,
    field: Option<String>,
    message: String,
}

impl ValidationError {
    pub(crate) fn new(
        kind: ValidationErrorKind,
        field: Option<&str>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            field: field.map(str::to_owned),
            message: message.into(),
        }
    }

    /// What kind of problem this is.
    pub fn kind(&self) -> ValidationErrorKind {
        self.kind
    }

    /// The field at fault, when one is, in Schwab's wire spelling.
    pub fn field(&self) -> Option<&str> {
        self.field.as_deref()
    }

    /// The explanation, as [`Display`](fmt::Display) prints it.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Place a nested request's field under its parent: `prefix` is the
    /// parent's field and index, such as `orderLegCollection[1]`.
    pub(crate) fn within(mut self, prefix: &str) -> Self {
        self.field = Some(match self.field {
            Some(field) => format!("{prefix}.{field}"),
            None => prefix.to_owned(),
        });
        self
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_fields_are_placed_under_their_parents() {
        let error = ValidationError::new(ValidationErrorKind::NotWhole, Some("quantity"), "whole")
            .within("orderLegCollection[1]")
            .within("childOrderStrategies[0]");
        assert_eq!(
            error.field(),
            Some("childOrderStrategies[0].orderLegCollection[1].quantity")
        );
        assert_eq!(error.to_string(), "whole");
        assert_eq!(error.kind(), ValidationErrorKind::NotWhole);
    }
}
