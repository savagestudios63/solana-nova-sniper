//! Library crate for the nova-sniper binary.
//!
//! Having these as a library makes them usable from benches, integration
//! tests, and any future companion binaries (e.g. a backtesting harness).

pub mod config;
pub mod detector;
pub mod executor;
pub mod filters;
pub mod listener;
pub mod strategy;
pub mod wallet;

/// Test/bench-only facade. Kept behind `pub mod` without a feature gate so
/// the existing `benches/` directory can link against it directly without
/// needing a `[features]` table. Not part of the semver surface.
pub mod bench_api {
    use solana_sdk::pubkey::Pubkey;

    use crate::config::{
        FilterConfig, PumpFunTarget, RaydiumTarget, StrategyConfig, TakeProfitRung, TargetsConfig,
    };
    use crate::detector::{Detector, LaunchEvent};
    use crate::filters::{FilterEngine, FilterVerdict, LaunchFacts};
    use crate::strategy::{PositionState, StrategyAction};

    pub struct BenchFixture {
        pub detector: Detector,
        pub filters: FilterEngine,
        pub sample_event: LaunchEvent,
        pub sample_ix_data: Vec<u8>,
        pub sample_accounts: Vec<Pubkey>,
        pub sample_program: Pubkey,
        pub facts: LaunchFacts,
        pub strategy_cfg: StrategyConfig,
    }

    impl BenchFixture {
        pub fn new() -> Self {
            let targets = TargetsConfig {
                pumpfun: PumpFunTarget {
                    enabled: true,
                    program_id: "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P".into(),
                    create_discriminator: "181ec828051c0777".into(),
                },
                raydium_launchlab: RaydiumTarget {
                    enabled: true,
                    program_id: "LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj".into(),
                    initialize_discriminator: "afaf6d1f0d989bed".into(),
                },
            };
            let detector = Detector::new(&targets).unwrap();

            let filter_cfg = FilterConfig {
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
            };
            let filters = FilterEngine::new(filter_cfg).unwrap();

            let sample_program: Pubkey =
                "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P".parse().unwrap();
            let mut sample_ix_data = Vec::from(hex::decode("181ec828051c0777").unwrap());
            for s in ["DogeMoon", "DOGE", "https://example.com/m.json"] {
                sample_ix_data.extend_from_slice(&(s.len() as u32).to_le_bytes());
                sample_ix_data.extend_from_slice(s.as_bytes());
            }
            let sample_accounts: Vec<Pubkey> =
                (0..10).map(|_| Pubkey::new_unique()).collect();

            let sample_event = LaunchEvent {
                venue: crate::detector::Venue::PumpFun,
                signature: "sig".into(),
                slot: 1,
                mint: Pubkey::new_unique(),
                creator: Pubkey::new_unique(),
                pool: None,
                name: Some("DogeMoon".into()),
                symbol: Some("DOGE".into()),
                uri: None,
                observed_at_ms: 0,
            };

            let facts = LaunchFacts {
                initial_liquidity_sol: 5.0,
                total_supply: 1_000,
                mint_authority_renounced: true,
                freeze_authority_null: true,
                lp_locked_pct: 100.0,
                has_socials: true,
            };

            let strategy_cfg = StrategyConfig {
                take_profit_ladder: vec![
                    TakeProfitRung { multiplier: 2.0, sell_pct: 30.0 },
                    TakeProfitRung { multiplier: 5.0, sell_pct: 30.0 },
                    TakeProfitRung { multiplier: 10.0, sell_pct: 40.0 },
                ],
                stop_loss_multiplier: 0.5,
                trailing_stop_drawdown: 0.35,
                trailing_stop_activation: 2.0,
                max_hold_seconds: 3600,
                price_poll_interval_ms: 500,
            };

            Self {
                detector,
                filters,
                sample_event,
                sample_ix_data,
                sample_accounts,
                sample_program,
                facts,
                strategy_cfg,
            }
        }
    }

    impl Default for BenchFixture {
        fn default() -> Self {
            Self::new()
        }
    }

    pub fn bench_decode(fx: &BenchFixture) -> Option<LaunchEvent> {
        fx.detector.decode(
            &fx.sample_program,
            &fx.sample_ix_data,
            &fx.sample_accounts,
            "sig".into(),
            1,
        )
    }

    pub fn bench_prefilter(fx: &BenchFixture) -> FilterVerdict {
        fx.filters.prefilter(&fx.sample_event)
    }

    pub fn bench_evaluate(fx: &BenchFixture) -> FilterVerdict {
        fx.filters.evaluate(&fx.sample_event, &fx.facts)
    }

    pub fn bench_strategy_tick(fx: &BenchFixture, price: f64, now: u64) -> StrategyAction {
        let mut pos = PositionState::open(1.0, 0, fx.strategy_cfg.clone());
        pos.on_tick(price, now)
    }
}
