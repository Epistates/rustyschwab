//! Common types shared across multiple API modules.
//!
//! This module contains shared enumerations and structures used throughout
//! the Schwab API, including:
//!
//! - Asset types (Equity, Option, Future, etc.)
//! - Market types
//! - Response headers
//! - Pagination info

#![allow(missing_docs)] // DTO fields are self-documenting via Schwab API docs

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseHeaders {
    #[serde(rename = "Schwab-Client-CorrelId")]
    pub schwab_client_correlid: Option<String>,
    #[serde(rename = "Schwab-Resource-Version")]
    pub schwab_resource_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MarketType {
    Equity,
    Option,
    Future,
    Bond,
    Forex,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    pub page_size: i32,
    pub page_number: i32,
    pub total_pages: i32,
    pub total_items: i32,
}