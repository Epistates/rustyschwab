# Paper capability and qualification checklist

Model: `equity-bbo-v2`. This checklist distinguishes implemented capabilities from further engineering and external qualification. It is not a certification of enterprise suitability.

| Area | Current implementation | Remaining work / required input |
| --- | --- | --- |
| Instruments | Whole-share long equities, inventory-backed sells | Options, shorts, fractional shares, futures and other products need explicit scope and account rules. |
| Orders | Market, limit, stop and stop-limit; DAY/GTC/NORMAL; replacement and cancel races | Trailing stops, IOC/FOK, auctions, extended sessions and multi-leg/conditional strategies need separate execution contracts. |
| Cash settlement | Explicit session-to-settlement calendar; pending proceeds cannot fund buys | Calendar ingestion/renewal and product exceptions; current default is immediate synthetic settlement. |
| Corporate actions | Durable dividend entitlement and split cash-in-lieu inputs | Authoritative action feed, corrections/reversals, mergers, spin-offs, adjusted option deliverables and tax treatment. |
| Financing | Cash-only ledger | Margin methodology, borrow availability/rates, recalls, dividends in lieu and forced liquidation require account/product specifications. |
| Fees and basis | Configurable per-share fee; checked decimal average-cost ledger | Effective-dated regulatory/exchange/broker fee profiles, minimums/rounding rules and selectable tax lots. |
| Live market input | Opt-in REST quotes and broker regular-hours calendar; strict normalization, deadlines and durable watermarks | Entitled end-to-end live validation, streaming adapter, signed source provenance and lossless decimal ingestion. Polling omits intermediate market events. |
| Historical input | Synthetic BBO from OHLCV candles under the declared `candle-bbo-v1` model: four evenly spaced marks per bar, configured spread and participation, New York dates as sessions | Recorded quote and trade history, calibrated intra-bar paths and sizes, and any claim that a synthetic fill would have occurred. |
| Execution fidelity | Finite observed BBO size, explicit slippage/latency/collars and deterministic priority | Exchange queues, impact, hidden liquidity and calibrated stochastic fills need reference datasets and error bounds. |
| Recovery | Durable replay cursor, duplicate protection, backup/restore, conservation verifier, transcript verification and atomic execution-command capture with backup replay | Wall-clock replay, admission-control replay, general expected-error transcripts, independent financial oracle, corruption drills across all tables and broader crash-injection matrix. |
| Retention | Backup-backed payload compaction with SHA-256 receipts | Receipts/results/audit still grow. External immutable retention, archive lifecycle policy and database space reclamation remain operational work. |
| Authorization | Shared verified gateway approvals, fencing, separate ingestion roles and atomic verified-subject attribution | Signed external feed provenance, coordinated sequencing across distinct service subjects and historical provenance backfill are not supplied. |
| Qualification | Local mock-service, arithmetic, restart and recovery tests | Live broker/reference differential scenarios, representative long-running load, deployment recovery drills and native platform runs. Earlier v1 benchmark results do not qualify v2. |

Do not silently approximate unsupported financial behavior. The unresolved instrument/account question is whether the intended scope includes listed options, shorts and margin, and which account, borrow and margin assumptions apply. Those choices also determine fee, lot and corporate-action requirements.

Most covered U.S. transactions follow [SEC T+1 guidance](https://www.sec.gov/exams/educationhelpguidesfaqs/t1-faq). Exchange calendars include [holidays and early closes](https://www.nyse.com/trade/hours-calendars?ecid=null). Options can have [adjusted deliverables following corporate actions](https://www.optionseducation.org/referencelibrary/faq/splits-mergers-spinoffs-bankruptcies); a universal 100-share contract assumption is unsuitable.
