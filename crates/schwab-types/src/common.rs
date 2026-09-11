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
//! - Response headers
//! - Pagination info

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
/// in transaction instruments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
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
}

/// Instrument class as Market Data spells `assetMainType` on quotes,
/// instruments, and stream envelopes. A different value set from the
/// Trader API's [`AssetType`].
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AssetSubType {
    Adr,
    Cef,
    Coe,
    Etf,
    Etn,
    Gdr,
    Oef,
    Prf,
    Rgt,
    Uit,
    War,
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
