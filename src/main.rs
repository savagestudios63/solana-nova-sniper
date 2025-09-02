use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use futures_util::StreamExt;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use tokio::signal;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

use solana_nova_sniper::config::Config;
use solana_nova_sniper::detector::Detector;
use solana_nova_sniper::executor::{ExecutionMode, Executor};
use solana_nova_sniper::filters::{FilterEngine, FilterVerdict, LaunchFacts};
use solana_nova_sniper::listener::{Listener, ListenerEvent};
use solana_nova_sniper::wallet::WalletPool;

#[derive(Debug, Parser)]
#[command(name = "nova-sniper", version, about = "Solana token sniper for Pump.fun + Raydium LaunchLab")]
struct Args {
    /// Path to TOML config
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,

    /// Do not sign or send any transaction — log what would happen.
    #[arg(long)]
    simulate: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = Config::from_path(&args.config)
        .with_context(|| format!("loading config from {}", args.config.display()))?;

    init_tracing(&cfg.runtime.log_level, cfg.runtime.json_logs);

    let mode = if args.simulate {
        ExecutionMode::DryRun
    } else {
        ExecutionMode::Live
    };
    info!(?mode, "nova-sniper starting");

    let wallets = Arc::new(
        WalletPool::load(&cfg.wallets).context("loading wallets")?,
    );
    info!(wallet_count = wallets.len(), "wallets loaded");

    let rpc = Arc::new(RpcClient::new_with_commitment(
        cfg.rpc.http_url.clone(),
        parse_commitment(&cfg.rpc.commitment),
    ));

    let detector = Detector::new(&cfg.targets).context("building detector")?;
    let filters = Arc::new(FilterEngine::new(cfg.filters.clone())?);
    let executor = Arc::new(Executor::new(
        rpc.clone(),
        cfg.jito.clone(),
        cfg.sizing.clone(),
        mode,
    )?);

    let concurrency = Arc::new(Semaphore::new(cfg.runtime.max_concurrent_snipes.max(1)));

    let listener = Listener::new(cfg.rpc.clone(), detector);
    let mut events = listener.start();

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                info!("shutdown signal received — exiting");
                break;
            }
            next = events.next() => {
                let Some(event) = next else {
                    warn!("listener stream ended");
                    break;
                };
                match event {
                    ListenerEvent::Launch(launch) => {
                        let Ok(permit) = concurrency.clone().acquire_owned().await else {
                            continue;
                        };
                        let filters = filters.clone();
                        let wallets = wallets.clone();
                        let executor = executor.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            if let Err(e) =
                                handle_launch(launch, filters, wallets, executor).await
                            {
                                error!(error = %e, "snipe task failed");
                            }
                        });
                    }
                    ListenerEvent::Reconnecting { attempt, reason } => {
                        warn!(attempt, %reason, "listener reconnecting");
                    }
                }
            }
        }
    }

    Ok(())
}

async fn handle_launch(
    launch: solana_nova_sniper::detector::LaunchEvent,
    filters: Arc<FilterEngine>,
    wallets: Arc<WalletPool>,
    executor: Arc<Executor>,
) -> Result<()> {
    // Cheap pre-check first — avoid the RPC round-trip for obvious rejects.
    if let FilterVerdict::Reject { reason } = filters.prefilter(&launch) {
        info!(
            mint = %launch.mint,
            signature = %launch.signature,
            %reason,
            "launch pre-filtered out"
        );
        return Ok(());
    }

    // In a real deployment this is where we'd fetch on-chain state: mint
    // account for authorities, bonding curve / pool for liquidity + LP lock,
    // and metadata URI for socials. For now we pass neutral facts so the
    // downstream filter logic can be exercised in dev without RPC access.
    let facts = fetch_launch_facts(&launch).await.unwrap_or_default();

    if let FilterVerdict::Reject { reason } = filters.evaluate(&launch, &facts) {
        info!(
            mint = %launch.mint,
            signature = %launch.signature,
            %reason,
            "launch filtered out after enrichment"
        );
        return Ok(());
    }

    let Some(wallet) = wallets.acquire() else {
        warn!(mint = %launch.mint, "no wallet available — skipping");
        return Ok(());
    };

    let outcome = executor.buy(&launch, &wallet).await?;
    info!(
        mint = %launch.mint,
        signature = %launch.signature,
        ?outcome,
        "buy attempt finished"
    );

    // TODO: hand the position to a strategy task that polls price and runs
    // the PositionState machine. Left as an integration hook so the buy path
    // can be exercised independently.
    Ok(())
}

/// Stub that would enrich a launch with on-chain facts (mint authority,
/// pool reserves, LP lock status, metadata socials). Returns `None` when the
/// bot is operating in an environment without RPC connectivity — the caller
/// treats that as "no facts, use defaults".
async fn fetch_launch_facts(
    _launch: &solana_nova_sniper::detector::LaunchEvent,
) -> Option<LaunchFacts> {
    // Integration seam: read mint account + pool account + metadata URI.
    // Wire this up before enabling filter thresholds in live mode.
    None
}

fn parse_commitment(s: &str) -> CommitmentConfig {
    match s.to_lowercase().as_str() {
        "finalized" => CommitmentConfig::finalized(),
        "confirmed" => CommitmentConfig::confirmed(),
        _ => CommitmentConfig::processed(),
    }
}

fn init_tracing(level: &str, json: bool) {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level));

    if json {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().json().with_current_span(true))
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().with_target(true))
            .init();
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("install ctrl-c handler");
    };
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
