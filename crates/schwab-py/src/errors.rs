//! Turning SDK failures into the Python exception hierarchy.
//!
//! The classes live in `rustyschwab/errors.py` so their attributes, docstrings
//! and type stubs are plain Python. This module only classifies a failure
//! (the SDK's stable [`ErrorKind`](schwab_rs::ErrorKind) name) and gathers the
//! details each class carries; `rustyschwab.errors.from_native` picks the class.
//!
//! Messages are the SDK's own `Display` text, which is written to be safe to
//! log: it omits tokens, secrets and broker response bodies. The bodies that
//! `Display` leaves out travel as separate attributes (`body`, `errors`) for
//! callers that want them, and never reach `str()` or `repr()`.

use pyo3::exceptions::PyStopAsyncIteration;
use pyo3::prelude::*;
use pythonize::pythonize;
use schwab_rs::Error;
use serde_json::{Map, Value, json};

/// Why a call into the SDK did not produce a value.
#[derive(Debug)]
pub enum Failure {
    /// The SDK refused or failed the call.
    Sdk(Error),
    /// An argument could not be turned into the request it describes. Nothing
    /// was sent. `field` names the offending part when it is known.
    Input {
        field: Option<String>,
        message: String,
    },
    /// An async iterator has nothing more to deliver.
    EndOfStream,
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self::Sdk(error)
    }
}

impl Failure {
    pub fn input(message: impl Into<String>) -> Self {
        Self::Input {
            field: None,
            message: message.into(),
        }
    }

    pub fn field(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Input {
            field: Some(field.into()),
            message: message.into(),
        }
    }
}

/// The classification, message and attributes of a failure.
fn describe(failure: &Failure) -> (&'static str, String, Map<String, Value>) {
    let mut details = Map::new();
    match failure {
        Failure::Input { field, message } => {
            details.insert("field".into(), json!(field));
            ("validation", message.clone(), details)
        }
        Failure::EndOfStream => ("end_of_stream", String::new(), details),
        Failure::Sdk(error) => {
            details.insert("retryable".into(), json!(error.is_retryable()));
            match error {
                Error::RateLimit { retry_after } => {
                    details.insert("retry_after".into(), json!(retry_after));
                }
                Error::Timeout { duration } => {
                    details.insert("duration".into(), json!(duration));
                }
                Error::Http { status, message } | Error::Remote { status, message } => {
                    details.insert("status".into(), json!(status.as_u16()));
                    details.insert("body".into(), json!(message));
                }
                Error::Broker {
                    status,
                    details: errors,
                    request_id,
                } => {
                    details.insert("status".into(), json!(status.as_u16()));
                    details.insert("request_id".into(), json!(request_id));
                    details.insert(
                        "errors".into(),
                        serde_json::to_value(errors).unwrap_or(Value::Null),
                    );
                }
                Error::Api { code, .. } => {
                    details.insert("code".into(), json!(code));
                }
                Error::Auth(auth) => {
                    details.insert(
                        "reauthorization_required".into(),
                        json!(auth.requires_reauthorization()),
                    );
                }
                Error::ReconciliationRequired { attempt_id } => {
                    details.insert("attempt_id".into(), json!(attempt_id));
                }
                Error::SubmissionUnknown { attempt_id, source } => {
                    details.insert("attempt_id".into(), json!(attempt_id));
                    details.insert("cause".into(), json!(source.to_string()));
                }
                Error::BaselineStale { account, reason } => {
                    details.insert("account".into(), json!(account));
                    details.insert("reason".into(), json!(reason));
                }
                Error::Validation(invalid) => {
                    details.insert("field".into(), json!(invalid.field()));
                }
                _ => {}
            }
            (error.kind().as_str(), error.to_string(), details)
        }
    }
}

/// The Python exception for `failure`, built by `rustyschwab.errors`.
///
/// If that module cannot be imported or fails, the import error is returned
/// instead, which is louder than a wrong class but never loses the failure.
pub fn to_pyerr(py: Python<'_>, failure: Failure) -> PyErr {
    if matches!(failure, Failure::EndOfStream) {
        return PyStopAsyncIteration::new_err(());
    }
    let (kind, message, details) = describe(&failure);
    let build = || -> PyResult<PyErr> {
        let details = pythonize(py, &details)?;
        let exception = py
            .import("rustyschwab.errors")?
            .getattr("from_native")?
            .call1((kind, message, details))?;
        Ok(PyErr::from_value(exception))
    };
    build().unwrap_or_else(|error| error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use schwab_rs::error::AuthError;

    #[test]
    fn rate_limits_carry_the_wait() {
        let (kind, _, details) = describe(&Failure::Sdk(Error::RateLimit { retry_after: 7 }));
        assert_eq!(kind, "rate_limit");
        assert_eq!(details["retry_after"], json!(7));
        assert_eq!(details["retryable"], json!(true));
    }

    #[test]
    fn unknown_submissions_carry_the_attempt() {
        let error = Error::SubmissionUnknown {
            attempt_id: "a-1".into(),
            source: Box::new(Error::ConnectionClosed),
        };
        let (kind, message, details) = describe(&Failure::Sdk(error));
        assert_eq!(kind, "submission_unknown");
        assert!(message.contains("a-1"));
        assert_eq!(details["attempt_id"], json!("a-1"));
    }

    #[test]
    fn broker_bodies_stay_out_of_the_message() {
        let error = Error::Http {
            status: reqwest::StatusCode::BAD_REQUEST,
            message: "echoes the account number 1234".into(),
        };
        let (kind, message, details) = describe(&Failure::Sdk(error));
        assert_eq!(kind, "http");
        assert!(!message.contains("1234"));
        assert_eq!(details["body"], json!("echoes the account number 1234"));
        assert_eq!(details["status"], json!(400));
    }

    #[test]
    fn reauthorization_is_flagged() {
        let error = Error::Auth(AuthError::ReauthorizationRequired("expired".into()));
        let (kind, _, details) = describe(&Failure::Sdk(error));
        assert_eq!(kind, "auth");
        assert_eq!(details["reauthorization_required"], json!(true));
    }

    #[test]
    fn bad_input_is_a_validation_failure() {
        let (kind, message, details) = describe(&Failure::field("price", "must be positive"));
        assert_eq!(kind, "validation");
        assert_eq!(message, "must be positive");
        assert_eq!(details["field"], json!("price"));
    }
}
