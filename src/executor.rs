use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::system_instruction;
use solana_sdk::transaction::Transaction;
use tracing::{info, warn};

use crate::config::{JitoConfig, SizingConfig};
use crate::detector::LaunchEvent;
use crate::wallet::WalletGuard;

/// Outcome of a buy attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BuyOutcome {
    /// Bundle accepted by Jito block engine (but not yet landed).
    BundleSubmitted {
        bundle_id: String,
        signature: String,
    },
    /// Submitted via Jupiter/normal RPC path.
    DirectSubmitted { signature: String },
    /// Dry-run mode — nothing was signed or sent.
    Simulated {
        summary: String,
    },
    /// All retry attempts exhausted.
    Dropped { reason: String },
}

/// How the executor should actually submit transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Sign and submit for real.
    Live,
    /// Log what would happen, never sign.
    DryRun,
}

pub struct Executor {
    http_client: reqwest::Client,
    rpc: Arc<RpcClient>,
    jito: JitoConfig,
    sizing: SizingConfig,
    mode: ExecutionMode,
}

impl Executor {
    pub fn new(
        rpc: Arc<RpcClient>,
        jito: JitoConfig,
        sizing: SizingConfig,
        mode: ExecutionMode,
    ) -> Result<Self> {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .context("build HTTP client")?;
        Ok(Self {
            http_client,
            rpc,
            jito,
            sizing,
            mode,
        })
    }

    /// Build and submit a buy for the given launch event using `wallet`.
    pub async fn buy(
        &self,
        event: &LaunchEvent,
        wallet: &WalletGuard<'_>,
    ) -> Result<BuyOutcome> {
        if self.mode == ExecutionMode::DryRun {
            let summary = format!(
                "[dry-run] would buy {} SOL of mint {} via {:?} (sig-src={}, slot={})",
                self.sizing.buy_sol, event.mint, event.venue, event.signature, event.slot
            );
            info!(target: "executor", "{summary}");
            return Ok(BuyOutcome::Simulated { summary });
        }

        let mut last_err: Option<String> = None;
        for attempt in 0..=self.jito.max_bundle_retries {
            match self.submit_bundle(event, wallet).await {
                Ok(outcome) => return Ok(outcome),
                Err(e) => {
                    warn!(target: "executor",
                        attempt, error = %e, "bundle submission failed");
                    last_err = Some(e.to_string());
                    tokio::time::sleep(Duration::from_millis(200 * (attempt as u64 + 1))).await;
                }
            }
        }

        // Fallback: send via Jupiter/standard RPC path. This trades execution
        // quality for reliability when Jito is flaky.
        match self.submit_direct(event, wallet).await {
            Ok(outcome) => Ok(outcome),
            Err(e) => Ok(BuyOutcome::Dropped {
                reason: format!(
                    "jito failed ({}); direct fallback failed ({})",
                    last_err.unwrap_or_else(|| "unknown".into()),
                    e
                ),
            }),
        }
    }

    async fn submit_bundle(
        &self,
        event: &LaunchEvent,
        wallet: &WalletGuard<'_>,
    ) -> Result<BuyOutcome> {
        let tip_account = self.pick_tip_account()?;
        let tip_lamports = sol_to_lamports(self.jito.tip_sol);

        let recent_blockhash = self
            .rpc
            .get_latest_blockhash()
            .await
            .context("fetch recent blockhash")?;

        let ixs = self.build_buy_instructions(event, wallet.keypair(), &tip_account, tip_lamports)?;
        let msg = Message::new(&ixs, Some(&wallet.pubkey()));
        let mut tx = Transaction::new_unsigned(msg);
        tx.sign(&[wallet.keypair()], recent_blockhash);

        let signature = tx.signatures[0];
        let serialized = bincode::serialize(&tx).context("serialize tx")?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&serialized);

        let url = format!("{}/api/v1/bundles", self.jito.block_engine_url.trim_end_matches('/'));
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": [[b64]],
        });
        let resp = self
            .http_client
            .post(&url)
            .json(&req)
            .send()
            .await
            .context("POST bundle")?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.context("parse bundle response")?;
        if !status.is_success() {
            anyhow::bail!("bundle HTTP {}: {}", status, body);
        }
        if let Some(err) = body.get("error") {
            anyhow::bail!("bundle RPC error: {}", err);
        }
        let bundle_id = body
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>")
            .to_string();

        info!(
            target: "executor",
            bundle_id,
            signature = %signature,
            mint = %event.mint,
            "bundle submitted"
        );

        Ok(BuyOutcome::BundleSubmitted {
            bundle_id,
            signature: signature.to_string(),
        })
    }

    async fn submit_direct(
        &self,
        event: &LaunchEvent,
        wallet: &WalletGuard<'_>,
    ) -> Result<BuyOutcome> {
        let recent_blockhash = self
            .rpc
            .get_latest_blockhash()
            .await
            .context("fetch recent blockhash")?;
        // For direct path we skip the tip transfer.
        let ixs = self.build_buy_instructions(
            event,
            wallet.keypair(),
            &Pubkey::default(),
            0,
        )?;
        let msg = Message::new(&ixs, Some(&wallet.pubkey()));
        let mut tx = Transaction::new_unsigned(msg);
        tx.sign(&[wallet.keypair()], recent_blockhash);
        let sig = self
            .rpc
            .send_transaction(&tx)
            .await
            .context("send direct transaction")?;
        Ok(BuyOutcome::DirectSubmitted {
            signature: sig.to_string(),
        })
    }

    /// Build the instruction list for a buy. This is the venue-specific piece
    /// and is where most real-world integration lives. We construct a minimal
    /// set (compute budget + optional tip + swap placeholder) and leave the
    /// actual swap instruction to be filled in per venue.
    fn build_buy_instructions(
        &self,
        event: &LaunchEvent,
        payer: &Keypair,
        tip_account: &Pubkey,
        tip_lamports: u64,
    ) -> Result<Vec<Instruction>> {
        let mut ixs = Vec::with_capacity(4);
        ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(
            self.sizing.compute_unit_limit,
        ));
        ixs.push(ComputeBudgetInstruction::set_compute_unit_price(
            self.sizing.compute_unit_price,
        ));

        // Venue-specific swap instruction goes here. We delegate to per-venue
        // builders so the executor stays flat. These builders are the integration
        // seam — fill them in with the actual program CPI for each venue.
        match event.venue {
            crate::detector::Venue::PumpFun => {
                ixs.extend(build_pumpfun_buy(event, &payer.pubkey(), &self.sizing)?);
            }
            crate::detector::Venue::RaydiumLaunchLab => {
                ixs.extend(build_raydium_buy(event, &payer.pubkey(), &self.sizing)?);
            }
        }

        if tip_lamports > 0 {
            ixs.push(system_instruction::transfer(
                &payer.pubkey(),
                tip_account,
                tip_lamports,
            ));
        }
        Ok(ixs)
    }

    fn pick_tip_account(&self) -> Result<Pubkey> {
        let mut rng = rand::thread_rng();
        let s = self
            .jito
            .tip_accounts
            .choose(&mut rng)
            .context("no tip accounts configured")?;
        Ok(s.parse()?)
    }
}

fn sol_to_lamports(sol: f64) -> u64 {
    (sol * 1_000_000_000.0).round() as u64
}

// ---- per-venue buy builders ------------------------------------------------
//
// These return the *swap* instructions for the venue. They're stubs that you
// must wire to the real program's CPI interface — the concrete instruction
// layouts change over time and live outside this repo's scope. Keeping them
// as their own functions lets integrators replace them without touching the
// executor state machine.

fn build_pumpfun_buy(
    event: &LaunchEvent,
    _payer: &Pubkey,
    sizing: &SizingConfig,
) -> Result<Vec<Instruction>> {
    // TODO: construct the real Pump.fun `buy` CPI. Accounts (observed order):
    //   [global, fee_recipient, mint, bonding_curve, associated_bonding_curve,
    //    associated_user, user, system_program, token_program, rent, event_authority, program]
    // Data: [discriminator(8), amount(u64), max_sol_cost(u64)]
    let _ = event;
    let _ = sizing;
    warn!(
        target: "executor",
        "pumpfun buy builder is a stub — wire it to the real program CPI before going live",
    );
    Ok(Vec::new())
}

fn build_raydium_buy(
    event: &LaunchEvent,
    _payer: &Pubkey,
    sizing: &SizingConfig,
) -> Result<Vec<Instruction>> {
    // TODO: construct the real Raydium LaunchLab swap_in CPI. Alternatively,
    // route through Jupiter's /swap endpoint and deserialize the returned
    // versioned transaction — that path is provider-hosted and handles
    // AMM+CLMM+LaunchLab uniformly.
    let _ = event;
    let _ = sizing;
    warn!(
        target: "executor",
        "raydium buy builder is a stub — wire to program CPI or Jupiter /swap",
    );
    Ok(Vec::new())
}

// ---- Jupiter fallback helpers ---------------------------------------------

/// Minimal Jupiter /quote response shape. Used when the direct CPI path is
/// unavailable or the venue is non-canonical.
#[derive(Debug, Deserialize)]
pub struct JupiterQuote {
    #[serde(rename = "outAmount")]
    pub out_amount: String,
    #[serde(rename = "inAmount")]
    pub in_amount: String,
    #[serde(rename = "priceImpactPct")]
    pub price_impact_pct: Option<String>,
}

pub async fn jupiter_quote(
    http: &reqwest::Client,
    input_mint: &Pubkey,
    output_mint: &Pubkey,
    amount_in: u64,
    slippage_bps: u32,
) -> Result<JupiterQuote> {
    let url = format!(
        "https://quote-api.jup.ag/v6/quote?inputMint={}&outputMint={}&amount={}&slippageBps={}",
        input_mint, output_mint, amount_in, slippage_bps
    );
    let resp = http.get(&url).send().await?.error_for_status()?;
    let quote: JupiterQuote = resp.json().await?;
    Ok(quote)
}

/// Helper used by strategy layer to sign and submit a plain swap tx that
/// Jupiter returns pre-built.
pub async fn submit_signed_tx(
    rpc: &RpcClient,
    tx: &Transaction,
) -> Result<Signature> {
    let sig = rpc
        .send_transaction_with_config(
            tx,
            solana_client::rpc_config::RpcSendTransactionConfig {
                skip_preflight: true,
                preflight_commitment: Some(CommitmentConfig::processed().commitment),
                ..Default::default()
            },
        )
        .await?;
    Ok(sig)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sol_to_lamports_round_trip() {
        assert_eq!(sol_to_lamports(0.001), 1_000_000);
        assert_eq!(sol_to_lamports(1.0), 1_000_000_000);
    }

    #[test]
    fn buy_outcome_serializes() {
        let o = BuyOutcome::Simulated { summary: "x".into() };
        let s = serde_json::to_string(&o).unwrap();
        assert!(s.contains("Simulated"));
    }
}
