//! Common types shared across multiple API modules.
//!
//! This module contains shared enumerations and structures used throughout
//! the Schwab API, including:
//!
//! - Asset types (Equity, Option, Future, etc.)
//! - Response headers
//! - Pagination info

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ResponseHeaders {
    #[serde(rename = "Schwab-Client-CorrelId")]
    pub schwab_client_correlid: Option<String>,
    #[serde(rename = "Schwab-Resource-Version")]
    pub schwab_resource_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AssetType {
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
    Unknown,
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
