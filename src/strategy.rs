use serde::{Deserialize, Serialize};

use crate::config::{StrategyConfig, TakeProfitRung};

/// One action the state machine can request per price update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StrategyAction {
    /// Hold the position — no changes.
    Hold,
    /// Sell a fraction of the remaining position.
    ///
    /// `fraction` is in (0.0, 1.0]. The `reason` is for logs/metrics.
    Sell { fraction: f64, reason: SellReason },
    /// Position fully closed; the caller can drop the state machine.
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SellReason {
    TakeProfit,
    StopLoss,
    TrailingStop,
    MaxHold,
}

/// Position state machine. One instance per open position.
///
/// The state machine is purely functional: the caller feeds it price and
/// wall-clock updates and it returns actions. It does not execute trades or
/// hit the network.
#[derive(Debug, Clone)]
pub struct PositionState {
    entry_price: f64,
    entry_ts_secs: u64,
    peak_price: f64,
    /// Remaining fraction of the original position (1.0 → full, 0.0 → closed).
    remaining: f64,
    /// Take profit rungs that have already fired (index into config ladder).
    fired_rungs: Vec<bool>,
    trailing_armed: bool,
    cfg: StrategyConfig,
}

impl PositionState {
    pub fn open(entry_price: f64, entry_ts_secs: u64, cfg: StrategyConfig) -> Self {
        let fired_rungs = vec![false; cfg.take_profit_ladder.len()];
        Self {
            entry_price,
            entry_ts_secs,
            peak_price: entry_price,
            remaining: 1.0,
            fired_rungs,
            trailing_armed: false,
            cfg,
        }
    }

    pub fn entry_price(&self) -> f64 {
        self.entry_price
    }

    pub fn remaining(&self) -> f64 {
        self.remaining
    }

    pub fn is_closed(&self) -> bool {
        self.remaining <= f64::EPSILON
    }

    /// Feed a price tick and current wall-clock time in seconds.
    pub fn on_tick(&mut self, price: f64, now_secs: u64) -> StrategyAction {
        if self.is_closed() {
            return StrategyAction::Closed;
        }
        if price > self.peak_price {
            self.peak_price = price;
        }

        // Priority order: stop-loss first (loss protection beats everything),
        // then take-profit rungs, then trailing stop, then max-hold.
        if let Some(action) = self.check_stop_loss(price) {
            return action;
        }
        if let Some(action) = self.check_take_profit(price) {
            return action;
        }
        if let Some(action) = self.check_trailing_stop(price) {
            return action;
        }
        if let Some(action) = self.check_max_hold(now_secs) {
            return action;
        }
        StrategyAction::Hold
    }

    fn check_stop_loss(&mut self, price: f64) -> Option<StrategyAction> {
        let threshold = self.entry_price * self.cfg.stop_loss_multiplier;
        if price <= threshold {
            let fraction = self.remaining;
            self.remaining = 0.0;
            return Some(StrategyAction::Sell {
                fraction,
                reason: SellReason::StopLoss,
            });
        }
        None
    }

    fn check_take_profit(&mut self, price: f64) -> Option<StrategyAction> {
        // Find the next unfired rung whose multiplier has been reached.
        for (i, rung) in self.cfg.take_profit_ladder.iter().enumerate() {
            if self.fired_rungs[i] {
                continue;
            }
            let target = self.entry_price * rung.multiplier;
            if price >= target {
                self.fired_rungs[i] = true;
                return Some(self.sell_from_rung(rung));
            }
        }
        None
    }

    fn sell_from_rung(&mut self, rung: &TakeProfitRung) -> StrategyAction {
        // `sell_pct` is a percentage of the *original* position. Convert to a
        // fraction of what's left so callers can cleanly compute token amounts.
        let wanted_of_original = rung.sell_pct / 100.0;
        let fraction_of_remaining = (wanted_of_original / self.remaining).min(1.0);
        self.remaining = (self.remaining - wanted_of_original).max(0.0);
        if self.remaining <= f64::EPSILON {
            self.remaining = 0.0;
        }
        StrategyAction::Sell {
            fraction: fraction_of_remaining,
            reason: SellReason::TakeProfit,
        }
    }

    fn check_trailing_stop(&mut self, price: f64) -> Option<StrategyAction> {
        let activation_price = self.entry_price * self.cfg.trailing_stop_activation;
        if !self.trailing_armed && self.peak_price >= activation_price {
            self.trailing_armed = true;
        }
        if !self.trailing_armed {
            return None;
        }
        let drawdown_trigger = self.peak_price * (1.0 - self.cfg.trailing_stop_drawdown);
        if price <= drawdown_trigger {
            let fraction = self.remaining;
            self.remaining = 0.0;
            return Some(StrategyAction::Sell {
                fraction,
                reason: SellReason::TrailingStop,
            });
        }
        None
    }

    fn check_max_hold(&mut self, now_secs: u64) -> Option<StrategyAction> {
        if now_secs.saturating_sub(self.entry_ts_secs) >= self.cfg.max_hold_seconds {
            let fraction = self.remaining;
            self.remaining = 0.0;
            return Some(StrategyAction::Sell {
                fraction,
                reason: SellReason::MaxHold,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> StrategyConfig {
        StrategyConfig {
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
        }
    }

    #[test]
    fn hold_when_price_flat() {
        let mut pos = PositionState::open(1.0, 0, cfg());
        assert_eq!(pos.on_tick(1.0, 1), StrategyAction::Hold);
    }

    #[test]
    fn stop_loss_fires_and_closes() {
        let mut pos = PositionState::open(1.0, 0, cfg());
        let action = pos.on_tick(0.5, 10);
        assert!(matches!(
            action,
            StrategyAction::Sell { reason: SellReason::StopLoss, .. }
        ));
        assert!(pos.is_closed());
        assert_eq!(pos.on_tick(0.5, 11), StrategyAction::Closed);
    }

    #[test]
    fn take_profit_rungs_fire_in_order() {
        let mut pos = PositionState::open(1.0, 0, cfg());
        // Hit 2x: should fire first rung (30% of original)
        match pos.on_tick(2.1, 1) {
            StrategyAction::Sell { fraction, reason: SellReason::TakeProfit } => {
                // 30% of original = 30% of 1.0 remaining = 0.3
                assert!((fraction - 0.3).abs() < 1e-9);
            }
            other => panic!("expected TP sell, got {other:?}"),
        }
        assert!((pos.remaining() - 0.7).abs() < 1e-9);

        // Holding between rungs
        assert_eq!(pos.on_tick(3.0, 2), StrategyAction::Hold);

        // Hit 5x: should fire second rung (30% of original)
        match pos.on_tick(5.5, 3) {
            StrategyAction::Sell { reason: SellReason::TakeProfit, .. } => {}
            other => panic!("expected TP sell, got {other:?}"),
        }
        assert!((pos.remaining() - 0.4).abs() < 1e-9);

        // Hit 10x: last rung, closes position
        match pos.on_tick(10.1, 4) {
            StrategyAction::Sell { reason: SellReason::TakeProfit, .. } => {}
            other => panic!("expected TP sell, got {other:?}"),
        }
        assert!(pos.is_closed());
    }

    #[test]
    fn trailing_stop_arms_after_activation() {
        let mut pos = PositionState::open(1.0, 0, cfg());
        // Fire first TP to get the position partly sold, and push peak to 2.1.
        let _ = pos.on_tick(2.1, 1);
        // Now peak is 2.1, armed. Drawdown threshold = 2.1 * 0.65 = 1.365.
        // Drop to 1.3 — should trigger trailing stop on the remaining 0.7.
        match pos.on_tick(1.3, 2) {
            StrategyAction::Sell { fraction, reason: SellReason::TrailingStop } => {
                assert!((fraction - 0.7).abs() < 1e-6);
            }
            other => panic!("expected trailing stop, got {other:?}"),
        }
        assert!(pos.is_closed());
    }

    #[test]
    fn trailing_stop_doesnt_arm_before_activation() {
        let mut pos = PositionState::open(1.0, 0, cfg());
        // Peak at 1.8, below 2.0 activation. A 40% drawdown should NOT fire.
        let _ = pos.on_tick(1.8, 1);
        assert_eq!(pos.on_tick(1.05, 2), StrategyAction::Hold);
    }

    #[test]
    fn max_hold_forces_exit() {
        let mut pos = PositionState::open(1.0, 0, cfg());
        let action = pos.on_tick(1.1, 3600);
        assert!(matches!(
            action,
            StrategyAction::Sell { reason: SellReason::MaxHold, .. }
        ));
        assert!(pos.is_closed());
    }

    #[test]
    fn stop_loss_priority_over_take_profit() {
        // Construct a scenario where price is below stop-loss — SL must win
        // even though some logic order could have picked TP first.
        let mut pos = PositionState::open(1.0, 0, cfg());
        let action = pos.on_tick(0.4, 1);
        assert!(matches!(
            action,
            StrategyAction::Sell { reason: SellReason::StopLoss, .. }
        ));
    }

    #[test]
    fn closed_position_is_idempotent() {
        let mut pos = PositionState::open(1.0, 0, cfg());
        let _ = pos.on_tick(0.4, 1);
        for t in 2..10 {
            assert_eq!(pos.on_tick(0.1, t), StrategyAction::Closed);
        }
    }

    #[test]
    fn same_rung_does_not_double_fire() {
        let mut pos = PositionState::open(1.0, 0, cfg());
        let _ = pos.on_tick(2.5, 1);
        // Price stays above 2x but hasn't hit 5x — should just hold.
        assert_eq!(pos.on_tick(2.6, 2), StrategyAction::Hold);
        assert_eq!(pos.on_tick(4.9, 3), StrategyAction::Hold);
    }
}
