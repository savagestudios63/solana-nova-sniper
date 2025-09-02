use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

use crate::config::FilterConfig;
use crate::detector::LaunchEvent;

/// Facts the filter pipeline needs on top of the raw launch event.
/// The executor populates these from on-chain reads before calling `evaluate`.
#[derive(Debug, Clone, Default)]
pub struct LaunchFacts {
    pub initial_liquidity_sol: f64,
    pub total_supply: u64,
    pub mint_authority_renounced: bool,
    pub freeze_authority_null: bool,
    pub lp_locked_pct: f64,
    pub has_socials: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum FilterVerdict {
    Accept,
    Reject { reason: String },
}

impl FilterVerdict {
    pub fn is_accept(&self) -> bool {
        matches!(self, FilterVerdict::Accept)
    }
}

/// Compiled filter ruleset. Regexes are compiled once at startup.
pub struct FilterEngine {
    cfg: FilterConfig,
    name_allow: Vec<Regex>,
    name_deny: Vec<Regex>,
    symbol_allow: Vec<Regex>,
    allowlist: Vec<Pubkey>,
    blocklist: Vec<Pubkey>,
}

impl FilterEngine {
    pub fn new(cfg: FilterConfig) -> Result<Self> {
        let name_allow = compile_regexes(&cfg.name_allow_regex)?;
        let name_deny = compile_regexes(&cfg.name_deny_regex)?;
        let symbol_allow = compile_regexes(&cfg.symbol_allow_regex)?;
        let allowlist = parse_pubkeys(&cfg.dev_allowlist)?;
        let blocklist = parse_pubkeys(&cfg.dev_blocklist)?;
        Ok(Self {
            cfg,
            name_allow,
            name_deny,
            symbol_allow,
            allowlist,
            blocklist,
        })
    }

    /// Cheap pre-check that only needs the launch event — no RPC calls.
    /// Runs in the hot path; the full `evaluate` runs after enrichment.
    pub fn prefilter(&self, event: &LaunchEvent) -> FilterVerdict {
        if !self.allowlist.is_empty() && !self.allowlist.contains(&event.creator) {
            return reject("creator not in dev allowlist");
        }
        if self.blocklist.contains(&event.creator) {
            return reject("creator in dev blocklist");
        }
        if let Some(name) = &event.name {
            for re in &self.name_deny {
                if re.is_match(name) {
                    return reject(&format!("name matches deny regex: {}", re.as_str()));
                }
            }
            if !self.name_allow.is_empty()
                && !self.name_allow.iter().any(|re| re.is_match(name))
            {
                return reject("name did not match any allow regex");
            }
        }
        if let Some(symbol) = &event.symbol {
            if !self.symbol_allow.is_empty()
                && !self.symbol_allow.iter().any(|re| re.is_match(symbol))
            {
                return reject("symbol did not match any allow regex");
            }
        }
        FilterVerdict::Accept
    }

    /// Full evaluation with rug checks — called after on-chain facts fetched.
    pub fn evaluate(&self, event: &LaunchEvent, facts: &LaunchFacts) -> FilterVerdict {
        let pre = self.prefilter(event);
        if !pre.is_accept() {
            return pre;
        }
        if self.cfg.min_initial_liquidity_sol > 0.0
            && facts.initial_liquidity_sol < self.cfg.min_initial_liquidity_sol
        {
            return reject(&format!(
                "liquidity {:.3} SOL below min {:.3}",
                facts.initial_liquidity_sol, self.cfg.min_initial_liquidity_sol
            ));
        }
        if self.cfg.max_supply > 0 && facts.total_supply > self.cfg.max_supply {
            return reject(&format!(
                "supply {} exceeds max {}",
                facts.total_supply, self.cfg.max_supply
            ));
        }
        if self.cfg.require_mint_authority_renounced && !facts.mint_authority_renounced {
            return reject("mint authority not renounced");
        }
        if self.cfg.require_freeze_authority_null && !facts.freeze_authority_null {
            return reject("freeze authority not null");
        }
        if self.cfg.min_lp_locked_pct > 0.0
            && facts.lp_locked_pct < self.cfg.min_lp_locked_pct
        {
            return reject(&format!(
                "LP locked {:.2}% below min {:.2}%",
                facts.lp_locked_pct, self.cfg.min_lp_locked_pct
            ));
        }
        if self.cfg.require_socials && !facts.has_socials {
            return reject("missing required social links");
        }
        FilterVerdict::Accept
    }
}

fn compile_regexes(patterns: &[String]) -> Result<Vec<Regex>> {
    patterns
        .iter()
        .map(|p| Regex::new(p).map_err(Into::into))
        .collect()
}

fn parse_pubkeys(items: &[String]) -> Result<Vec<Pubkey>> {
    items.iter().map(|s| s.parse().map_err(Into::into)).collect()
}

fn reject(reason: &str) -> FilterVerdict {
    FilterVerdict::Reject {
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::Venue;

    fn base_cfg() -> FilterConfig {
        FilterConfig {
            min_initial_liquidity_sol: 1.0,
            max_supply: 1_000_000_000,
            require_mint_authority_renounced: true,
            require_freeze_authority_null: true,
            min_lp_locked_pct: 90.0,
            name_allow_regex: vec![],
            name_deny_regex: vec!["(?i)scam".into()],
            symbol_allow_regex: vec![],
            require_socials: false,
            dev_allowlist: vec![],
            dev_blocklist: vec![],
        }
    }

    fn event(name: &str, creator: Pubkey) -> LaunchEvent {
        LaunchEvent {
            venue: Venue::PumpFun,
            signature: "sig".into(),
            slot: 1,
            mint: Pubkey::new_unique(),
            creator,
            pool: None,
            name: Some(name.into()),
            symbol: Some("TKN".into()),
            uri: None,
            observed_at_ms: 0,
        }
    }

    fn good_facts() -> LaunchFacts {
        LaunchFacts {
            initial_liquidity_sol: 5.0,
            total_supply: 100_000,
            mint_authority_renounced: true,
            freeze_authority_null: true,
            lp_locked_pct: 100.0,
            has_socials: true,
        }
    }

    #[test]
    fn accepts_clean_launch() {
        let eng = FilterEngine::new(base_cfg()).unwrap();
        let ev = event("Doge", Pubkey::new_unique());
        assert_eq!(eng.evaluate(&ev, &good_facts()), FilterVerdict::Accept);
    }

    #[test]
    fn deny_regex_rejects() {
        let eng = FilterEngine::new(base_cfg()).unwrap();
        let ev = event("Big SCAM Coin", Pubkey::new_unique());
        assert!(matches!(
            eng.prefilter(&ev),
            FilterVerdict::Reject { .. }
        ));
    }

    #[test]
    fn low_liquidity_rejects() {
        let eng = FilterEngine::new(base_cfg()).unwrap();
        let ev = event("Doge", Pubkey::new_unique());
        let mut f = good_facts();
        f.initial_liquidity_sol = 0.1;
        assert!(matches!(
            eng.evaluate(&ev, &f),
            FilterVerdict::Reject { .. }
        ));
    }

    #[test]
    fn mint_authority_not_renounced_rejects() {
        let eng = FilterEngine::new(base_cfg()).unwrap();
        let ev = event("Doge", Pubkey::new_unique());
        let mut f = good_facts();
        f.mint_authority_renounced = false;
        match eng.evaluate(&ev, &f) {
            FilterVerdict::Reject { reason } => assert!(reason.contains("mint authority")),
            _ => panic!("expected reject"),
        }
    }

    #[test]
    fn freeze_authority_rejects() {
        let eng = FilterEngine::new(base_cfg()).unwrap();
        let ev = event("Doge", Pubkey::new_unique());
        let mut f = good_facts();
        f.freeze_authority_null = false;
        match eng.evaluate(&ev, &f) {
            FilterVerdict::Reject { reason } => assert!(reason.contains("freeze")),
            _ => panic!("expected reject"),
        }
    }

    #[test]
    fn lp_locked_threshold_rejects() {
        let eng = FilterEngine::new(base_cfg()).unwrap();
        let ev = event("Doge", Pubkey::new_unique());
        let mut f = good_facts();
        f.lp_locked_pct = 50.0;
        assert!(matches!(
            eng.evaluate(&ev, &f),
            FilterVerdict::Reject { .. }
        ));
    }

    #[test]
    fn blocklist_rejects() {
        let dev = Pubkey::new_unique();
        let mut cfg = base_cfg();
        cfg.dev_blocklist = vec![dev.to_string()];
        let eng = FilterEngine::new(cfg).unwrap();
        let ev = event("Doge", dev);
        assert!(matches!(
            eng.prefilter(&ev),
            FilterVerdict::Reject { .. }
        ));
    }

    #[test]
    fn allowlist_restricts() {
        let allowed = Pubkey::new_unique();
        let stranger = Pubkey::new_unique();
        let mut cfg = base_cfg();
        cfg.dev_allowlist = vec![allowed.to_string()];
        let eng = FilterEngine::new(cfg).unwrap();
        assert_eq!(
            eng.prefilter(&event("Doge", allowed)),
            FilterVerdict::Accept
        );
        assert!(matches!(
            eng.prefilter(&event("Doge", stranger)),
            FilterVerdict::Reject { .. }
        ));
    }

    #[test]
    fn disabled_thresholds_are_no_ops() {
        let mut cfg = base_cfg();
        cfg.min_initial_liquidity_sol = 0.0;
        cfg.max_supply = 0;
        cfg.min_lp_locked_pct = 0.0;
        cfg.require_mint_authority_renounced = false;
        cfg.require_freeze_authority_null = false;
        let eng = FilterEngine::new(cfg).unwrap();
        let ev = event("Doge", Pubkey::new_unique());
        let facts = LaunchFacts::default();
        assert_eq!(eng.evaluate(&ev, &facts), FilterVerdict::Accept);
    }
}
