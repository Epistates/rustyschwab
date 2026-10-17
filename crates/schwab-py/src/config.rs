//! Configuration dicts laid over the SDK's defaults (or its environment).
//!
//! A dict has the shape of `SchwabConfig` serialized: `oauth`, `client` and
//! `streaming` sections with the SDK's own field names. Only the keys given
//! change; the rest keep the base value. A key the SDK does not know is an
//! error, not a silent no-op, because a misspelled `app_secrte` or
//! `rate_limit` would otherwise run a client with the wrong settings.

use crate::convert::{from_value, to_json};
use crate::errors::Failure;
use pyo3::prelude::*;
use rustyschwab::{SchwabConfig, StreamConfig};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

/// Fields the SDK reads as durations ("30s", "250ms"). A Python number is
/// taken as seconds, which is what `timeout=30` means to a Python caller.
const DURATION_KEYS: &[&str] = &[
    "timeout",
    "initial_backoff",
    "max_backoff",
    "open_duration",
    "heartbeat_interval",
    "ping_timeout",
];

/// Where a `SchwabConfig` starts before the dict is applied.
#[derive(Debug, Clone, Copy)]
pub enum Base {
    /// `SchwabConfig::default()`, which has no credentials.
    Defaults,
    /// The `SCHWAB_*` environment, as `SchwabConfig::from_env`.
    Env,
}

/// The client configuration: `base`, then `overrides` on top.
pub fn schwab_config(
    base: Base,
    overrides: Option<&Bound<'_, PyAny>>,
) -> Result<SchwabConfig, Failure> {
    let base = match base {
        Base::Defaults => SchwabConfig::default(),
        Base::Env => SchwabConfig::from_env()?,
    };
    let overrides = overrides.map(to_json).transpose()?;
    if let Some(Value::Object(root)) = &overrides {
        // A value of the wrong type would otherwise be quoted back by serde,
        // and for these two fields that quote is a credential.
        for field in ["app_key", "app_secret"] {
            let given = root.get("oauth").and_then(|oauth| oauth.get(field));
            if given.is_some_and(|value| !value.is_string()) {
                return Err(Failure::field(
                    format!("oauth.{field}"),
                    format!("oauth.{field} must be a string"),
                ));
            }
        }
    }
    overlay(&base, overrides, "configuration")
}

/// The streaming configuration: `base`, then `overrides` on top.
pub fn stream_config(
    base: &StreamConfig,
    overrides: Option<&Bound<'_, PyAny>>,
) -> Result<StreamConfig, Failure> {
    overlay(
        base,
        overrides.map(to_json).transpose()?,
        "stream configuration",
    )
}

/// `base` with `overrides` merged over it, checked against `base`'s own keys.
fn overlay<T: Serialize + DeserializeOwned>(
    base: &T,
    overrides: Option<Value>,
    what: &str,
) -> Result<T, Failure> {
    let mut merged = serde_json::to_value(base)
        .map_err(|error| Failure::input(format!("cannot read the base {what}: {error}")))?;
    match overrides {
        None | Some(Value::Null) => {}
        Some(Value::Object(overrides)) => merge(&mut merged, overrides, "")?,
        Some(_) => {
            return Err(Failure::input(format!("the {what} must be a dict")));
        }
    }
    from_value(merged, what)
}

fn merge(into: &mut Value, overrides: Map<String, Value>, path: &str) -> Result<(), Failure> {
    let Value::Object(known) = into else {
        // An absent optional section (`streaming: None`) has no keys to check.
        *into = Value::Object(overrides);
        return Ok(());
    };
    for (key, value) in overrides {
        let at = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        let Some(slot) = known.get_mut(&key) else {
            return Err(Failure::field(
                at.clone(),
                format!("unknown configuration key `{at}`"),
            ));
        };
        match value {
            Value::Object(children) if slot.is_object() => merge(slot, children, &at)?,
            Value::Number(seconds) if DURATION_KEYS.contains(&key.as_str()) => {
                *slot = Value::String(duration(&seconds, &at)?);
            }
            value => *slot = value,
        }
    }
    Ok(())
}

/// Seconds as the SDK's duration spelling, to the millisecond.
fn duration(seconds: &serde_json::Number, at: &str) -> Result<String, Failure> {
    let millis = seconds
        .as_f64()
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map(|seconds| (seconds * 1000.0).round() as u64)
        .ok_or_else(|| {
            Failure::field(at, format!("{at} must be a non-negative number of seconds"))
        })?;
    Ok(format!("{millis}ms"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn apply(overrides: Value) -> Result<SchwabConfig, Failure> {
        overlay(&SchwabConfig::default(), Some(overrides), "configuration")
    }

    #[test]
    fn named_keys_change_and_the_rest_stay_default() {
        let config = apply(json!({
            "oauth": {"app_key": "k", "app_secret": "s", "token_store_kind": "file"},
            "client": {"base_url": "http://127.0.0.1:9", "retry": {"max_retries": 1}},
        }))
        .unwrap();
        assert_eq!(config.oauth.app_key, "k");
        assert_eq!(config.client.base_url, "http://127.0.0.1:9");
        assert_eq!(config.client.retry.max_retries, 1);
        assert_eq!(config.client.rate_limit.requests_per_second, 2);
    }

    #[test]
    fn numbers_are_seconds_for_durations() {
        let config =
            apply(json!({"client": {"timeout": 2.5}, "streaming": {"ping_timeout": 40}})).unwrap();
        assert_eq!(config.client.timeout.as_millis(), 2500);
        assert_eq!(config.streaming.unwrap().ping_timeout.as_secs(), 40);
        let config = apply(json!({"client": {"timeout": "1m"}})).unwrap();
        assert_eq!(config.client.timeout.as_secs(), 60);
    }

    #[test]
    fn unknown_keys_are_rejected_with_their_path() {
        let Failure::Input { field, message } =
            apply(json!({"client": {"rate_limt": {"enabled": false}}})).unwrap_err()
        else {
            panic!("expected an input failure");
        };
        assert_eq!(field.as_deref(), Some("client.rate_limt"));
        assert!(message.contains("unknown configuration key"));
    }

    #[test]
    fn streaming_can_be_switched_off() {
        assert!(
            apply(json!({"streaming": null}))
                .unwrap()
                .streaming
                .is_none()
        );
    }

    #[test]
    fn a_non_dict_is_refused() {
        assert!(matches!(
            overlay(&SchwabConfig::default(), Some(json!([1])), "configuration"),
            Err(Failure::Input { .. })
        ));
    }
}
