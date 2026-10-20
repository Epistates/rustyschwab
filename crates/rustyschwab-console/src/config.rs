//! Where the console finds the state it watches: the same environment the
//! MCP server reads, so pointing both at one `.env` is enough.

use std::path::PathBuf;
use std::time::Duration;

/// The paper venue's configuration and state files
/// (`SCHWAB_MCP_PAPER_CONFIG`, `SCHWAB_MCP_PAPER_STATE`).
#[derive(Debug, Clone)]
pub struct PaperFiles {
    pub config: PathBuf,
    pub state: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ConsoleConfig {
    /// The paper venue to watch; absent, the live account the Schwab
    /// configuration names (`SCHWAB_TOKENS_FILE`, `SCHWAB_APP_KEY`, ...).
    pub paper: Option<PaperFiles>,
    /// The principals (`SCHWAB_MCP_POLICY_FILE`); absent, there are none
    /// to follow, and the console shows the accounts alone.
    pub policy_file: Option<PathBuf>,
    /// Who commands from the console, as approvals, halts and delegations
    /// record it. It must differ from every principal's name.
    pub operator: String,
    /// Symbols to quote besides those held or ordered.
    pub watchlist: Vec<String>,
    /// How often state files are read.
    pub refresh: Duration,
    /// How often quotes are read; in live mode each read is one Schwab
    /// request from the shared budget.
    pub quote_refresh: Duration,
    /// Shown in live mode without a Schwab login: quotes and kill need one.
    pub live_quotes: bool,
}

impl ConsoleConfig {
    /// From the environment, after loading `.env` as the server does.
    /// `SCHWAB_OPERATOR` names the operator (else the login user);
    /// `SCHWAB_DASH_WATCHLIST` is a comma-separated symbol list;
    /// `SCHWAB_DASH_QUOTES=0` stops live quote reads.
    pub fn from_env() -> anyhow::Result<Self> {
        let _ = dotenvy::dotenv();
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
        let paper = match (
            var("SCHWAB_MCP_PAPER_CONFIG"),
            var("SCHWAB_MCP_PAPER_STATE"),
        ) {
            (Some(config), Some(state)) => Some(PaperFiles { config, state }),
            (None, None) => None,
            _ => anyhow::bail!(
                "SCHWAB_MCP_PAPER_CONFIG and SCHWAB_MCP_PAPER_STATE are set together or not at all"
            ),
        };
        let operator = ["SCHWAB_OPERATOR", "USER", "USERNAME"]
            .iter()
            .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
            .ok_or_else(|| anyhow::anyhow!("Set SCHWAB_OPERATOR to name the operator"))?;
        let watchlist = std::env::var("SCHWAB_DASH_WATCHLIST")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_uppercase())
            .filter(|s| !s.is_empty())
            .collect();
        Ok(Self {
            paper,
            policy_file: var("SCHWAB_MCP_POLICY_FILE"),
            operator,
            watchlist,
            refresh: Duration::from_secs(1),
            quote_refresh: Duration::from_secs(5),
            live_quotes: std::env::var("SCHWAB_DASH_QUOTES").map_or(true, |v| v != "0"),
        })
    }
}
