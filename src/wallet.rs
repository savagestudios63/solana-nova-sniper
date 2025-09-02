use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use parking_lot::RwLock;
use rand::seq::SliceRandom;
use solana_sdk::signature::{Keypair, Signer};

use crate::config::{RotationStrategy, WalletConfig};

/// Thread-safe rotating wallet pool.
///
/// Keypairs are loaded once at startup from Solana-CLI-format JSON files
/// (array of 64 bytes). The pool hands out references according to the
/// configured rotation strategy.
pub struct WalletPool {
    wallets: Vec<Keypair>,
    strategy: RotationStrategy,
    cursor: AtomicUsize,
    // Tracks in-flight usage so `FirstAvailable` can pick an idle wallet.
    busy: RwLock<Vec<bool>>,
}

impl WalletPool {
    pub fn load(cfg: &WalletConfig) -> Result<Self> {
        let mut wallets = Vec::with_capacity(cfg.keypair_paths.len());
        for path in &cfg.keypair_paths {
            wallets.push(load_keypair(path)?);
        }
        let busy = vec![false; wallets.len()];
        Ok(Self {
            wallets,
            strategy: cfg.rotation,
            cursor: AtomicUsize::new(0),
            busy: RwLock::new(busy),
        })
    }

    pub fn len(&self) -> usize {
        self.wallets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.wallets.is_empty()
    }

    /// Acquire a wallet for a snipe. Returns a guard that releases on drop.
    pub fn acquire(&self) -> Option<WalletGuard<'_>> {
        let idx = match self.strategy {
            RotationStrategy::RoundRobin => {
                let i = self.cursor.fetch_add(1, Ordering::Relaxed) % self.wallets.len();
                self.mark_busy(i);
                i
            }
            RotationStrategy::Random => {
                let mut rng = rand::thread_rng();
                let indices: Vec<usize> = (0..self.wallets.len()).collect();
                let &i = indices.choose(&mut rng)?;
                self.mark_busy(i);
                i
            }
            RotationStrategy::FirstAvailable => {
                let mut busy = self.busy.write();
                let i = busy.iter().position(|b| !*b)?;
                busy[i] = true;
                i
            }
        };
        Some(WalletGuard { pool: self, idx })
    }

    fn mark_busy(&self, idx: usize) {
        let mut busy = self.busy.write();
        if idx < busy.len() {
            busy[idx] = true;
        }
    }

    fn release(&self, idx: usize) {
        let mut busy = self.busy.write();
        if idx < busy.len() {
            busy[idx] = false;
        }
    }
}

pub struct WalletGuard<'a> {
    pool: &'a WalletPool,
    idx: usize,
}

impl WalletGuard<'_> {
    pub fn keypair(&self) -> &Keypair {
        &self.pool.wallets[self.idx]
    }

    pub fn pubkey(&self) -> solana_sdk::pubkey::Pubkey {
        self.keypair().pubkey()
    }

    pub fn index(&self) -> usize {
        self.idx
    }
}

impl Drop for WalletGuard<'_> {
    fn drop(&mut self) {
        self.pool.release(self.idx);
    }
}

fn load_keypair(path: &Path) -> Result<Keypair> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading keypair file {}", path.display()))?;
    let bytes: Vec<u8> = serde_json::from_str(&text)
        .with_context(|| format!("parsing keypair JSON at {}", path.display()))?;
    if bytes.len() != 64 {
        anyhow::bail!(
            "keypair at {} has {} bytes, expected 64",
            path.display(),
            bytes.len(),
        );
    }
    Keypair::from_bytes(&bytes)
        .with_context(|| format!("invalid keypair bytes in {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signature::Signer;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_temp_keypair(kp: &Keypair) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        let bytes: Vec<u8> = kp.to_bytes().to_vec();
        let json = serde_json::to_string(&bytes).unwrap();
        f.write_all(json.as_bytes()).unwrap();
        f
    }

    #[test]
    fn round_robin_rotates() {
        let kp1 = Keypair::new();
        let kp2 = Keypair::new();
        let f1 = write_temp_keypair(&kp1);
        let f2 = write_temp_keypair(&kp2);
        let cfg = WalletConfig {
            keypair_paths: vec![f1.path().to_path_buf(), f2.path().to_path_buf()],
            rotation: RotationStrategy::RoundRobin,
        };
        let pool = WalletPool::load(&cfg).unwrap();
        let g1 = pool.acquire().unwrap();
        let i1 = g1.index();
        drop(g1);
        let g2 = pool.acquire().unwrap();
        let i2 = g2.index();
        assert_ne!(i1, i2);
    }

    #[test]
    fn first_available_respects_busy() {
        let kp1 = Keypair::new();
        let f1 = write_temp_keypair(&kp1);
        let cfg = WalletConfig {
            keypair_paths: vec![f1.path().to_path_buf()],
            rotation: RotationStrategy::FirstAvailable,
        };
        let pool = WalletPool::load(&cfg).unwrap();
        let g1 = pool.acquire().unwrap();
        // Only wallet is busy now.
        assert!(pool.acquire().is_none());
        drop(g1);
        assert!(pool.acquire().is_some());
    }

    #[test]
    fn rejects_bad_keypair_file() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"[1,2,3]").unwrap();
        let cfg = WalletConfig {
            keypair_paths: vec![f.path().to_path_buf()],
            rotation: RotationStrategy::RoundRobin,
        };
        assert!(WalletPool::load(&cfg).is_err());
    }
}
