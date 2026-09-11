//! Request-side wire shapes: every enum's `as_str()` must match what serde
//! puts on the query string, and the param validators must enforce Schwab's
//! documented combination tables before a request leaves the process.

use chrono::NaiveDate;
use schwab_types::market_data::{
    Candle, Entitlement, FrequencyType, Market, MoverFrequency, MoverIndex, MoverSort,
    OptionChainParams, OptionType, PeriodType, PriceHistoryParams, Projection, QuoteField,
};
use schwab_types::streaming::{LevelOneOptionFields, StreamService};
use serde::Serialize;

/// The serde representation of `value` as a bare JSON string.
fn wire<T: Serialize>(value: T) -> String {
    match serde_json::to_value(value).unwrap() {
        serde_json::Value::String(s) => s,
        other => panic!("expected a string on the wire, got {other}"),
    }
}

fn assert_as_str_matches_serde<T: Serialize + Copy + std::fmt::Display>(
    variants: &[T],
    as_str: fn(T) -> &'static str,
) {
    for &variant in variants {
        let expected = wire(variant);
        assert_eq!(as_str(variant), expected, "as_str() drifted from serde");
        assert_eq!(variant.to_string(), expected, "Display drifted from serde");
    }
}

#[test]
fn quote_field_wire_names() {
    assert_as_str_matches_serde(
        &[
            QuoteField::All,
            QuoteField::Quote,
            QuoteField::Fundamental,
            QuoteField::Extended,
            QuoteField::Reference,
            QuoteField::Regular,
        ],
        QuoteField::as_str,
    );
    assert_eq!(QuoteField::Fundamental.as_str(), "fundamental");
}

#[test]
fn period_and_frequency_types_are_lowercase() {
    assert_as_str_matches_serde(
        &[
            PeriodType::Day,
            PeriodType::Month,
            PeriodType::Year,
            PeriodType::Ytd,
        ],
        PeriodType::as_str,
    );
    assert_as_str_matches_serde(
        &[
            FrequencyType::Minute,
            FrequencyType::Daily,
            FrequencyType::Weekly,
            FrequencyType::Monthly,
        ],
        FrequencyType::as_str,
    );
    // Schwab rejects the upper-case spellings the old code sent.
    assert_eq!(PeriodType::Day.as_str(), "day");
    assert_eq!(FrequencyType::Minute.as_str(), "minute");
}

#[test]
fn mover_sort_index_projection_and_market_wire_names() {
    assert_as_str_matches_serde(
        &[
            MoverSort::Volume,
            MoverSort::Trades,
            MoverSort::PercentChangeUp,
            MoverSort::PercentChangeDown,
        ],
        MoverSort::as_str,
    );
    assert_as_str_matches_serde(
        &[
            MoverIndex::Dji,
            MoverIndex::Compx,
            MoverIndex::Spx,
            MoverIndex::Nyse,
            MoverIndex::Nasdaq,
            MoverIndex::Otcbb,
            MoverIndex::IndexAll,
            MoverIndex::EquityAll,
            MoverIndex::OptionAll,
            MoverIndex::OptionPut,
            MoverIndex::OptionCall,
        ],
        MoverIndex::as_str,
    );
    assert_as_str_matches_serde(
        &[
            Projection::SymbolSearch,
            Projection::SymbolRegex,
            Projection::DescSearch,
            Projection::DescRegex,
            Projection::Search,
            Projection::Fundamental,
        ],
        Projection::as_str,
    );
    assert_as_str_matches_serde(
        &[
            Market::Equity,
            Market::Option,
            Market::Bond,
            Market::Future,
            Market::Forex,
        ],
        Market::as_str,
    );
    assert_eq!(MoverIndex::Dji.as_str(), "$DJI");
    assert_eq!(MoverIndex::OptionCall.as_str(), "OPTION_CALL");
    assert_eq!(Projection::SymbolSearch.as_str(), "symbol-search");
}

#[test]
fn mover_frequency_is_a_bare_integer() {
    for (variant, minutes) in [
        (MoverFrequency::All, 0),
        (MoverFrequency::OneMinute, 1),
        (MoverFrequency::FiveMinutes, 5),
        (MoverFrequency::TenMinutes, 10),
        (MoverFrequency::ThirtyMinutes, 30),
        (MoverFrequency::SixtyMinutes, 60),
    ] {
        assert_eq!(variant.as_minutes(), minutes);
        assert_eq!(
            serde_json::to_value(variant).unwrap(),
            serde_json::json!(minutes)
        );
        assert_eq!(
            serde_json::from_value::<MoverFrequency>(serde_json::json!(minutes)).unwrap(),
            variant
        );
    }
    let err = serde_json::from_value::<MoverFrequency>(serde_json::json!(7)).unwrap_err();
    assert!(
        err.to_string().contains("frequency 7 is not valid"),
        "{err}"
    );
}

#[test]
fn entitlement_and_option_type_wire_names() {
    assert_eq!(wire(Entitlement::NonPayingPro), "PN");
    assert_eq!(wire(Entitlement::NonPro), "NP");
    assert_eq!(wire(Entitlement::PayingPro), "PP");
    assert_eq!(wire(OptionType::Standard), "S");
    assert_eq!(wire(OptionType::NonStandard), "NS");
    assert_eq!(wire(OptionType::All), "ALL");
}

#[test]
fn price_history_validate_accepts_documented_combinations() {
    let mut params = PriceHistoryParams::new("AAPL");
    assert_eq!(params.validate(), Ok(()));

    params.period_type = Some(PeriodType::Day);
    params.period = Some(10);
    params.frequency_type = Some(FrequencyType::Minute);
    params.frequency = Some(5);
    assert_eq!(params.validate(), Ok(()));

    // Period without a period type is left to Schwab's default.
    let params = PriceHistoryParams {
        period: Some(10),
        ..PriceHistoryParams::new("AAPL")
    };
    assert_eq!(params.validate(), Ok(()));

    // A frequency with no explicit frequency type is checked against the
    // default for the period type (day -> minute).
    let params = PriceHistoryParams {
        period_type: Some(PeriodType::Day),
        frequency: Some(5),
        ..PriceHistoryParams::new("AAPL")
    };
    assert_eq!(params.validate(), Ok(()));

    for period_type in [
        PeriodType::Day,
        PeriodType::Month,
        PeriodType::Year,
        PeriodType::Ytd,
    ] {
        for &period in period_type.valid_periods() {
            for &frequency_type in period_type.valid_frequency_types() {
                for &frequency in frequency_type.valid_frequencies() {
                    let params = PriceHistoryParams {
                        period_type: Some(period_type),
                        period: Some(period),
                        frequency_type: Some(frequency_type),
                        frequency: Some(frequency),
                        ..PriceHistoryParams::new("SPY")
                    };
                    assert_eq!(
                        params.validate(),
                        Ok(()),
                        "{period_type}/{period}/{frequency_type}/{frequency}"
                    );
                }
            }
        }
    }
}

#[test]
fn price_history_validate_rejects_bad_combinations() {
    let err = PriceHistoryParams::new("  ").validate().unwrap_err();
    assert!(err.contains("symbol"), "{err}");

    let params = PriceHistoryParams {
        period_type: Some(PeriodType::Day),
        period: Some(7),
        ..PriceHistoryParams::new("AAPL")
    };
    let err = params.validate().unwrap_err();
    assert!(err.contains("period 7") && err.contains("day"), "{err}");

    let params = PriceHistoryParams {
        period_type: Some(PeriodType::Month),
        frequency_type: Some(FrequencyType::Minute),
        ..PriceHistoryParams::new("AAPL")
    };
    let err = params.validate().unwrap_err();
    assert!(
        err.contains("frequencyType minute") && err.contains("daily, weekly"),
        "{err}"
    );

    // year defaults to monthly, which only accepts frequency 1.
    let params = PriceHistoryParams {
        period_type: Some(PeriodType::Year),
        frequency: Some(5),
        ..PriceHistoryParams::new("AAPL")
    };
    let err = params.validate().unwrap_err();
    assert!(
        err.contains("frequency 5") && err.contains("monthly"),
        "{err}"
    );

    let params = PriceHistoryParams {
        frequency_type: Some(FrequencyType::Minute),
        frequency: Some(7),
        ..PriceHistoryParams::new("AAPL")
    };
    let err = params.validate().unwrap_err();
    assert!(err.contains("frequency 7"), "{err}");

    let params = PriceHistoryParams {
        start_date: Some(2_000),
        end_date: Some(1_000),
        ..PriceHistoryParams::new("AAPL")
    };
    let err = params.validate().unwrap_err();
    assert!(err.contains("startDate"), "{err}");
}

#[test]
fn price_history_params_serialize_with_schwab_query_names() {
    let params = PriceHistoryParams {
        period_type: Some(PeriodType::Day),
        period: Some(10),
        frequency_type: Some(FrequencyType::Minute),
        frequency: Some(5),
        need_extended_hours_data: Some(false),
        ..PriceHistoryParams::new("AAPL")
    };
    let value = serde_json::to_value(&params).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "symbol": "AAPL",
            "periodType": "day",
            "period": 10,
            "frequencyType": "minute",
            "frequency": 5,
            "needExtendedHoursData": false,
        })
    );
}

#[test]
fn option_chain_validate() {
    assert_eq!(OptionChainParams::new("AAPL").validate(), Ok(()));
    assert!(
        OptionChainParams::new("")
            .validate()
            .unwrap_err()
            .contains("symbol")
    );

    let params = OptionChainParams {
        strike_count: Some(0),
        ..OptionChainParams::new("AAPL")
    };
    assert!(params.validate().unwrap_err().contains("strikeCount"));

    let params = OptionChainParams {
        days_to_expiration: Some(-1),
        ..OptionChainParams::new("AAPL")
    };
    assert!(params.validate().unwrap_err().contains("daysToExpiration"));

    let params = OptionChainParams {
        from_date: NaiveDate::from_ymd_opt(2025, 2, 1),
        to_date: NaiveDate::from_ymd_opt(2025, 1, 1),
        ..OptionChainParams::new("AAPL")
    };
    assert!(params.validate().unwrap_err().contains("fromDate"));

    let params = OptionChainParams {
        entitlement: Some(Entitlement::PayingPro),
        option_type: Some(OptionType::Standard),
        from_date: NaiveDate::from_ymd_opt(2025, 1, 15),
        ..OptionChainParams::new("AAPL")
    };
    assert_eq!(params.validate(), Ok(()));
    let value = serde_json::to_value(&params).unwrap();
    assert_eq!(value["entitlement"], "PP");
    assert_eq!(value["optionType"], "S");
    assert_eq!(value["fromDate"], "2025-01-15");
    assert!(value.get("toDate").is_none(), "None fields must be omitted");
}

#[test]
fn level_one_option_fields_cover_the_full_documented_range() {
    let fields: Vec<u32> = LevelOneOptionFields::all_fields()
        .split(',')
        .map(|f| f.parse().unwrap())
        .collect();
    assert_eq!(fields.len(), 56);
    assert_eq!(fields.first(), Some(&0));
    assert_eq!(fields.last(), Some(&LevelOneOptionFields::EXERCISE_TYPE));
    assert_eq!(LevelOneOptionFields::EXERCISE_TYPE, 55);
    assert_eq!(LevelOneOptionFields::UV_EXPIRATION_TYPE, 36);
    assert_eq!(LevelOneOptionFields::EXCHANGE, 40);
    assert_eq!(
        StreamService::LeveloneOptions.default_fields(),
        LevelOneOptionFields::all_fields()
    );
}

#[test]
fn futures_options_stop_at_field_31() {
    let fields = StreamService::LeveloneFuturesOptions.default_fields();
    assert_eq!(fields.split(',').count(), 32);
    assert!(fields.ends_with(",31"), "{fields}");
}

#[test]
fn candle_volume_exceeds_i32() {
    let candle: Candle = serde_json::from_value(serde_json::json!({
        "open": 1.0, "high": 2.0, "low": 0.5, "close": 1.5,
        "volume": 3_000_000_000_i64, "datetime": 1_700_000_000_000_i64
    }))
    .unwrap();
    assert_eq!(candle.volume, 3_000_000_000);
    assert!(candle.volume > i64::from(i32::MAX));
}
