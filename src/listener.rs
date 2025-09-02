use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use solana_sdk::pubkey::Pubkey;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, warn};
use tonic::transport::ClientTlsConfig;
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
    SubscribeRequestFilterTransactions, SubscribeRequestPing,
};

use crate::config::RpcConfig;
use crate::detector::{Detector, LaunchEvent};

/// Hot-path event emitted by the listener for downstream consumers.
#[derive(Debug, Clone)]
pub enum ListenerEvent {
    Launch(LaunchEvent),
    /// Emitted if the gRPC stream dropped and we're attempting reconnect.
    /// The caller may want to log or alert but should not treat this as fatal.
    Reconnecting { attempt: u32, reason: String },
}

pub struct Listener {
    cfg: RpcConfig,
    detector: Detector,
}

impl Listener {
    pub fn new(cfg: RpcConfig, detector: Detector) -> Self {
        Self { cfg, detector }
    }

    /// Start subscribing and return a stream of events. The caller controls
    /// lifetime via the returned `ReceiverStream`; dropping it terminates
    /// the listener task.
    pub fn start(self) -> ReceiverStream<ListenerEvent> {
        let (tx, rx) = mpsc::channel::<ListenerEvent>(1024);
        tokio::spawn(async move {
            let mut attempt: u32 = 0;
            loop {
                match run_once(&self.cfg, &self.detector, tx.clone()).await {
                    Ok(()) => {
                        info!(target: "listener", "gRPC stream ended cleanly — reconnecting");
                    }
                    Err(e) => {
                        error!(target: "listener", error = %e, "gRPC stream failed");
                        let _ = tx
                            .send(ListenerEvent::Reconnecting {
                                attempt,
                                reason: e.to_string(),
                            })
                            .await;
                    }
                }
                attempt += 1;
                let backoff = backoff_for(attempt);
                debug!(target: "listener", ?backoff, attempt, "reconnect backoff");
                tokio::time::sleep(backoff).await;
            }
        });
        ReceiverStream::new(rx)
    }
}

fn backoff_for(attempt: u32) -> Duration {
    let capped = attempt.min(6);
    Duration::from_millis(250u64 << capped)
}

async fn run_once(
    cfg: &RpcConfig,
    detector: &Detector,
    tx: mpsc::Sender<ListenerEvent>,
) -> Result<()> {
    let tls = ClientTlsConfig::new().with_native_roots();
    let token = if cfg.geyser_x_token.is_empty() {
        None
    } else {
        Some(cfg.geyser_x_token.clone())
    };

    let mut client = GeyserGrpcClient::build_from_shared(cfg.geyser_url.clone())?
        .x_token(token)?
        .tls_config(tls)?
        .connect()
        .await
        .context("connect geyser gRPC")?;

    let watched: Vec<String> = detector
        .watched_program_ids()
        .into_iter()
        .map(|p: Pubkey| p.to_string())
        .collect();

    let mut transactions = HashMap::new();
    transactions.insert(
        "launches".to_string(),
        SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: Some(false),
            signature: None,
            account_include: vec![],
            account_exclude: vec![],
            account_required: watched,
        },
    );

    let request = SubscribeRequest {
        transactions,
        commitment: Some(parse_commitment(&cfg.commitment) as i32),
        ..Default::default()
    };

    let (mut sub_tx, mut sub_rx) = client.subscribe().await.context("open subscription")?;
    sub_tx.send(request).await.context("send subscribe request")?;

    info!(target: "listener", url = %cfg.geyser_url, "subscribed to geyser stream");

    while let Some(msg) = sub_rx.next().await {
        let update = match msg {
            Ok(u) => u,
            Err(e) => {
                warn!(target: "listener", error = %e, "stream error");
                return Err(anyhow::anyhow!(e));
            }
        };

        match update.update_oneof {
            Some(UpdateOneof::Transaction(tx_update)) => {
                if let Some(info) = tx_update.transaction {
                    let slot = tx_update.slot;
                    if let Some(events) = extract_launches(detector, info, slot) {
                        for ev in events {
                            if tx.send(ListenerEvent::Launch(ev)).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                }
            }
            Some(UpdateOneof::Ping(_)) => {
                // Keep-alive from server — respond so the connection stays open.
                let _ = sub_tx
                    .send(SubscribeRequest {
                        ping: Some(SubscribeRequestPing { id: 1 }),
                        ..Default::default()
                    })
                    .await;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Walk the transaction's instructions, matching each to the detector's
/// known programs. One tx may contain multiple launches (rare but possible).
fn extract_launches(
    detector: &Detector,
    info: yellowstone_grpc_proto::geyser::SubscribeUpdateTransactionInfo,
    slot: u64,
) -> Option<Vec<LaunchEvent>> {
    let tx = info.transaction?;
    let message = tx.message?;
    let account_keys: Vec<Pubkey> = message
        .account_keys
        .iter()
        .filter_map(|bytes| Pubkey::try_from(bytes.as_slice()).ok())
        .collect();

    let signature = bs58::encode(info.signature).into_string();

    let mut out = Vec::new();
    for ix in &message.instructions {
        let program_idx = ix.program_id_index as usize;
        let Some(program_id) = account_keys.get(program_idx).copied() else {
            continue;
        };
        let accounts: Vec<Pubkey> = ix
            .accounts
            .iter()
            .filter_map(|i| account_keys.get(*i as usize).copied())
            .collect();
        if let Some(ev) =
            detector.decode(&program_id, &ix.data, &accounts, signature.clone(), slot)
        {
            out.push(ev);
        }
    }

    // Inner instructions (CPIs) — `create` on Pump.fun is often a CPI from a
    // wrapper contract, so we must walk these too.
    if let Some(meta) = info.meta {
        for inner in meta.inner_instructions {
            for ix in inner.instructions {
                let program_idx = ix.program_id_index as usize;
                let Some(program_id) = account_keys.get(program_idx).copied() else {
                    continue;
                };
                let accounts: Vec<Pubkey> = ix
                    .accounts
                    .iter()
                    .filter_map(|i| account_keys.get(*i as usize).copied())
                    .collect();
                if let Some(ev) = detector.decode(
                    &program_id,
                    &ix.data,
                    &accounts,
                    signature.clone(),
                    slot,
                ) {
                    out.push(ev);
                }
            }
        }
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn parse_commitment(s: &str) -> CommitmentLevel {
    match s.to_lowercase().as_str() {
        "finalized" => CommitmentLevel::Finalized,
        "confirmed" => CommitmentLevel::Confirmed,
        _ => CommitmentLevel::Processed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff_for(1), Duration::from_millis(500));
        assert_eq!(backoff_for(2), Duration::from_millis(1000));
        // Cap kicks in at attempt=6 → 250ms << 6 == 16000ms.
        assert_eq!(backoff_for(20), Duration::from_millis(250 << 6));
    }

    #[test]
    fn parse_commitment_defaults_to_processed() {
        assert!(matches!(parse_commitment("garbage"), CommitmentLevel::Processed));
        assert!(matches!(parse_commitment("finalized"), CommitmentLevel::Finalized));
        assert!(matches!(parse_commitment("confirmed"), CommitmentLevel::Confirmed));
    }
}
