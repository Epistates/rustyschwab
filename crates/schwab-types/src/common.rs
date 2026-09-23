//! Common types shared across multiple API modules.
//!
//! This module contains shared enumerations and structures used throughout
//! the Schwab API, including:
//!
//! - Asset types: [`AssetType`](crate::common::AssetType) on the Trader API
//!   (positions, order legs, transactions) and
//!   [`AssetMainType`](crate::common::AssetMainType) on Market Data
//! - [`AccountsInstrument`](crate::common::AccountsInstrument), the
//!   instrument shape positions and order legs share
//! - [`schwab_timestamp`](crate::common::schwab_timestamp), the one
//!   timestamp spelling the Trader API's time-window query parameters accept
//! - Response headers
//! - Pagination info

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_with::skip_serializing_none;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ResponseHeaders {
    #[serde(rename = "Schwab-Client-CorrelId")]
    pub schwab_client_correlid: Option<String>,
    #[serde(rename = "Schwab-Resource-Version")]
    pub schwab_resource_version: Option<String>,
}

/// Instrument class as the Trader API spells `assetType` on positions,
/// order legs, and transaction items.
///
/// This is the spec's complete `assetType` enumeration; the per-instrument
/// base schemas list only the first eight, and the remaining three appear
/// in transaction instruments. See [`Wire`] for `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum AssetType {
    #[default]
    Equity,
    Option,
    Index,
    MutualFund,
    CashEquivalent,
    FixedIncome,
    Currency,
    CollectiveInvestment,
    Future,
    Forex,
    Product,
    #[serde(other)]
    #[cfg_attr(feature = "schemars", schemars(skip))]
    Unknown,
}

/// An enum that appears on both requests and broker responses.
///
/// Responses decode a value this release does not know as `Unknown`, so one
/// new spelling from Schwab cannot fail a whole order list or account. A
/// request can never carry `Unknown`: the variant is absent from the JSON
/// schemas, and [`OrderRequest::validate`](crate::trading::OrderRequest::validate)
/// and the other request validators reject it.
pub trait Wire {
    /// Whether this is the `Unknown` catch-all rather than a value Schwab
    /// documents.
    fn is_unknown(&self) -> bool;
}

/// Implements [`Wire`] for enums whose catch-all variant is `Unknown`.
macro_rules! wire_enum {
    ($($ty:ty),+ $(,)?) => {$(
        impl $crate::common::Wire for $ty {
            fn is_unknown(&self) -> bool {
                matches!(self, Self::Unknown)
            }
        }
    )+};
}
pub(crate) use wire_enum;

wire_enum!(AssetType);

/// Reject `Unknown` in a request field, naming the field.
pub(crate) fn known<T: Wire>(field: &str, value: Option<&T>) -> Result<(), String> {
    match value {
        Some(v) if v.is_unknown() => Err(format!("{field} is not a value Schwab accepts")),
        _ => Ok(()),
    }
}

/// Instrument class as Market Data spells `assetMainType` on quotes and
/// stream envelopes and `assetType` on instruments. A different value set
/// from the Trader API's [`AssetType`].
///
/// `Unknown` is both Schwab's own `UNKNOWN` and any value added after this
/// release, so decoding never fails on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AssetMainType {
    Bond,
    Equity,
    Etf,
    Extended,
    Forex,
    Future,
    FutureOption,
    Fundamental,
    Index,
    Indicator,
    MutualFund,
    Option,
    #[serde(other)]
    Unknown,
}

/// The instrument on a position or an order leg (`AccountsInstrument` in
/// the Trader API spec).
///
/// The fields Schwab documents for every asset type are typed, plus the
/// option and fixed-income extensions; anything else lands in `extra`.
#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct AccountsInstrument {
    pub asset_type: AssetType,
    pub symbol: String,
    pub cusip: Option<String>,
    pub description: Option<String>,
    pub instrument_id: Option<i64>,
    pub net_change: Option<f64>,
    /// Subtype whose value set depends on `asset_type` (`SWEEP_VEHICLE`
    /// for cash equivalents, `VANILLA` for options, …).
    #[serde(rename = "type")]
    pub type_: Option<String>,
    /// Options only. `None` when Schwab reports `UNKNOWN`.
    #[serde(default, deserialize_with = "lenient")]
    pub put_call: Option<PutCall>,
    /// Options only.
    pub underlying_symbol: Option<String>,
    /// Options only.
    pub option_multiplier: Option<i64>,
    /// Fixed income only.
    pub maturity_date: Option<String>,
    /// Fixed income only.
    pub factor: Option<f64>,
    /// Fixed income only.
    pub variable_rate: Option<f64>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Instrument subclass as Market Data spells `assetSubType` beside
/// [`AssetMainType`]: the equity subtypes (`COE` common stock, `PRF`
/// preferred, `ADR`/`GDR` depositary receipts, `CEF`, `ETF`, `ETN`, `UIT`,
/// `WAR` warrant, `RGT` right) and the mutual-fund ones (`OEF`, `CEF`,
/// `MMF`). Values outside the list decode as `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AssetSubType {
    Adr,
    Cef,
    Coe,
    Etf,
    Etn,
    Gdr,
    Mmf,
    Oef,
    Prf,
    Rgt,
    Uit,
    War,
    #[serde(other)]
    Unknown,
}

/// Option contract right, as Schwab spells it in `putCall` fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PutCall {
    Put,
    Call,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    pub page_size: i32,
    pub page_number: i32,
    pub total_pages: i32,
    pub total_items: i32,
}

/// Broker identifiers may be JSON integers or strings. Normalize losslessly.
pub fn optional_id<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Option<String>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(de)?;
    match value {
        None => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s)),
        Some(serde_json::Value::Number(n)) if n.is_u64() || n.is_i64() => Ok(Some(n.to_string())),
        _ => Err(serde::de::Error::custom(
            "Expected an integer or string identifier",
        )),
    }
}

/// Decode an optional enum on a broker response, reading values outside
/// the enum (Schwab's `UNKNOWN`, or a value added after this release) as
/// `None` instead of failing the whole payload. Pair with
/// `#[serde(default)]` so a missing field is `None` too.
pub fn lenient<'de, D, T>(de: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = Option::<serde_json::Value>::deserialize(de)?;
    Ok(value.and_then(|v| serde_json::from_value(v).ok()))
}

/// Deserialize a request decimal exactly.
///
/// Accepts a plain decimal string (`"150.25"`) or a JSON integer. A JSON
/// fraction is accepted as its shortest round-trip spelling, but it may
/// already have lost precision in the parser, so callers should send
/// strings. Rejected rather than reinterpreted: digit separators (`"1_000"`,
/// which `rust_decimal` would read as 1000), surrounding whitespace, exponent
/// notation, and more digits than a `Decimal` holds (which it would round).
pub mod exact_decimal {
    use rust_decimal::Decimal;
    use serde::de::{self, Deserializer, Visitor};
    use std::fmt;

    pub fn parse(s: &str) -> Result<Decimal, String> {
        let plain = !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_digit() || b == b'.' || b == b'-' || b == b'+');
        if !plain {
            return Err(format!("{s:?} is not a plain decimal number"));
        }
        Decimal::from_str_exact(s).map_err(|e| format!("{s:?} is not an exact decimal: {e}"))
    }

    struct Exact;

    impl Visitor<'_> for Exact {
        type Value = Decimal;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a decimal number as a string, such as \"150.25\"")
        }

        fn visit_str<E: de::Error>(self, s: &str) -> Result<Decimal, E> {
            parse(s).map_err(E::custom)
        }

        fn visit_i64<E: de::Error>(self, n: i64) -> Result<Decimal, E> {
            Ok(Decimal::from(n))
        }

        fn visit_u64<E: de::Error>(self, n: u64) -> Result<Decimal, E> {
            Ok(Decimal::from(n))
        }

        fn visit_f64<E: de::Error>(self, n: f64) -> Result<Decimal, E> {
            if !n.is_finite() {
                return Err(E::custom("decimal must be finite"));
            }
            // `{}` on f64 is the shortest round-trip spelling, never exponent form.
            parse(&n.to_string()).map_err(E::custom)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Decimal, D::Error> {
        de.deserialize_any(Exact)
    }

    /// The same rules for an optional field; pair with `#[serde(default)]`.
    pub mod option {
        use rust_decimal::Decimal;
        use serde::{Deserialize, Deserializer};

        pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Option<Decimal>, D::Error> {
            #[derive(Deserialize)]
            struct Wrap(#[serde(deserialize_with = "super::deserialize")] Decimal);
            Ok(Option::<Wrap>::deserialize(de)?.map(|Wrap(d)| d))
        }
    }
}

/// JSON schema for a request decimal: a string of digits with an optional
/// fraction, the only spelling [`exact_decimal`] guarantees is exact.
#[cfg(feature = "schemars")]
pub struct DecimalString;

#[cfg(feature = "schemars")]
impl schemars::JsonSchema for DecimalString {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "DecimalString".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[0-9]+(\\.[0-9]+)?$",
            "description": "Exact decimal as a string, e.g. \"150.25\"; never a JSON number."
        })
    }
}

/// Format a timestamp for the Trader API's time-window query parameters
/// (`fromEnteredTime`, `toEnteredTime`, `startDate`, `endDate`).
///
/// Schwab documents exactly one shape, `yyyy-MM-dd'T'HH:mm:ss.SSSZ`: UTC,
/// a literal `Z`, and exactly three fractional digits. RFC 3339 spellings
/// that omit the fraction or carry more digits are not the documented form,
/// so the crate never emits them.
///
/// ```
/// use chrono::{TimeZone, Utc};
/// use schwab_types::common::schwab_timestamp;
///
/// let at = Utc.with_ymd_and_hms(2025, 1, 15, 9, 30, 0).unwrap();
/// assert_eq!(schwab_timestamp(at), "2025-01-15T09:30:00.000Z");
/// ```
pub fn schwab_timestamp(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// `serialize_with` adapter for [`schwab_timestamp`].
pub(crate) fn serialize_timestamp<S: serde::Serializer>(
    at: &DateTime<Utc>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&schwab_timestamp(*at))
}
