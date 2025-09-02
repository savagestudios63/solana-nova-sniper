use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub runtime: RuntimeConfig,
    pub rpc: RpcConfig,
    pub jito: JitoConfig,
    pub wallets: WalletConfig,
    pub sizing: SizingConfig,
    pub targets: TargetsConfig,
    pub filters: FilterConfig,
    pub strategy: StrategyConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RuntimeConfig {
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_true")]
    pub json_logs: bool,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_snipes: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RpcConfig {
    pub http_url: String,
    pub geyser_url: String,
    #[serde(default)]
    pub geyser_x_token: String,
    #[serde(default = "default_commitment")]
    pub commitment: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JitoConfig {
    pub block_engine_url: String,
    #[serde(default = "default_tip_sol")]
    pub tip_sol: f64,
    pub tip_accounts: Vec<String>,
    #[serde(default = "default_retries")]
    pub max_bundle_retries: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WalletConfig {
    pub keypair_paths: Vec<PathBuf>,
    #[serde(default)]
    pub rotation: RotationStrategy,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RotationStrategy {
    #[default]
    RoundRobin,
    Random,
    FirstAvailable,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SizingConfig {
    pub buy_sol: f64,
    pub max_slippage_bps: u32,
    pub compute_unit_price: u64,
    pub compute_unit_limit: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TargetsConfig {
    pub pumpfun: PumpFunTarget,
    pub raydium_launchlab: RaydiumTarget,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PumpFunTarget {
    pub enabled: bool,
    pub program_id: String,
    pub create_discriminator: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RaydiumTarget {
    pub enabled: bool,
    pub program_id: String,
    pub initialize_discriminator: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FilterConfig {
    #[serde(default)]
    pub min_initial_liquidity_sol: f64,
    #[serde(default)]
    pub max_supply: u64,
    #[serde(default = "default_true")]
    pub require_mint_authority_renounced: bool,
    #[serde(default = "default_true")]
    pub require_freeze_authority_null: bool,
    #[serde(default)]
    pub min_lp_locked_pct: f64,
    #[serde(default)]
    pub name_allow_regex: Vec<String>,
    #[serde(default)]
    pub name_deny_regex: Vec<String>,
    #[serde(default)]
    pub symbol_allow_regex: Vec<String>,
    #[serde(default)]
    pub require_socials: bool,
    #[serde(default)]
    pub dev_allowlist: Vec<String>,
    #[serde(default)]
    pub dev_blocklist: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StrategyConfig {
    pub take_profit_ladder: Vec<TakeProfitRung>,
    pub stop_loss_multiplier: f64,
    pub trailing_stop_drawdown: f64,
    pub trailing_stop_activation: f64,
    pub max_hold_seconds: u64,
    #[serde(default = "default_poll_ms")]
    pub price_poll_interval_ms: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
pub struct TakeProfitRung {
    pub multiplier: f64,
    pub sell_pct: f64,
}

impl Config {
    pub fn from_path(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let cfg: Config = toml::from_str(&text).context("parsing config TOML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.wallets.keypair_paths.is_empty() {
            anyhow::bail!("at least one wallet keypair path must be configured");
        }
        if self.sizing.buy_sol <= 0.0 {
            anyhow::bail!("sizing.buy_sol must be positive");
        }
        if self.sizing.max_slippage_bps > 10_000 {
            anyhow::bail!("sizing.max_slippage_bps must be <= 10000 (100%)");
        }
        if self.jito.tip_accounts.is_empty() {
            anyhow::bail!("jito.tip_accounts must contain at least one tip account");
        }
        if self.strategy.stop_loss_multiplier <= 0.0
            || self.strategy.stop_loss_multiplier >= 1.0
        {
            anyhow::bail!("strategy.stop_loss_multiplier must be in (0, 1)");
        }
        let ladder_sum: f64 = self
            .strategy
            .take_profit_ladder
            .iter()
            .map(|r| r.sell_pct)
            .sum();
        if ladder_sum > 100.0 + f64::EPSILON {
            anyhow::bail!(
                "strategy.take_profit_ladder sell_pct sums to {ladder_sum:.2} > 100",
            );
        }
        for rung in &self.strategy.take_profit_ladder {
            if rung.multiplier <= 1.0 {
                anyhow::bail!("take profit rung multiplier must be > 1.0");
            }
            if rung.sell_pct <= 0.0 || rung.sell_pct > 100.0 {
                anyhow::bail!("take profit sell_pct must be in (0, 100]");
            }
        }
        Ok(())
    }
}

fn default_log_level() -> String {
    "info".into()
}
fn default_true() -> bool {
    true
}
fn default_max_concurrent() -> usize {
    4
}
fn default_commitment() -> String {
    "processed".into()
}
fn default_tip_sol() -> f64 {
    0.001
}
fn default_retries() -> u32 {
    3
}
fn default_poll_ms() -> u64 {
    500
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> Config {
        toml::from_str(include_str!("../config.example.toml")).expect("example parses")
    }

    #[test]
    fn example_config_validates() {
        minimal().validate().expect("example config valid");
    }

    #[test]
    fn rejects_oversubscribed_ladder() {
        let mut cfg = minimal();
        cfg.strategy.take_profit_ladder = vec![
            TakeProfitRung { multiplier: 2.0, sell_pct: 60.0 },
            TakeProfitRung { multiplier: 5.0, sell_pct: 60.0 },
        ];
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_zero_buy() {
        let mut cfg = minimal();
        cfg.sizing.buy_sol = 0.0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_empty_wallets() {
        let mut cfg = minimal();
        cfg.wallets.keypair_paths.clear();
        assert!(cfg.validate().is_err());
    }
}
