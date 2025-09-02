use anyhow::Result;
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

use crate::config::TargetsConfig;

/// A venue-agnostic description of a token launch event emitted by the detector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchEvent {
    pub venue: Venue,
    pub signature: String,
    pub slot: u64,
    pub mint: Pubkey,
    pub creator: Pubkey,
    /// Bonding curve / pool account (venue-specific semantics).
    pub pool: Option<Pubkey>,
    /// Optional metadata scraped from the create instruction args.
    pub name: Option<String>,
    pub symbol: Option<String>,
    pub uri: Option<String>,
    /// Detector-observed timestamp (unix ms) for latency measurement.
    pub observed_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Venue {
    PumpFun,
    RaydiumLaunchLab,
}

/// Decode instruction blobs into launch events.
///
/// The detector keeps the target program ids and Anchor-style instruction
/// discriminators in memory so per-transaction work is just a `memcmp`.
pub struct Detector {
    pumpfun: Option<ProgramMatcher>,
    raydium: Option<ProgramMatcher>,
}

struct ProgramMatcher {
    program_id: Pubkey,
    discriminator: [u8; 8],
    venue: Venue,
}

impl Detector {
    pub fn new(targets: &TargetsConfig) -> Result<Self> {
        let pumpfun = if targets.pumpfun.enabled {
            Some(ProgramMatcher {
                program_id: targets.pumpfun.program_id.parse()?,
                discriminator: parse_discriminator(&targets.pumpfun.create_discriminator)?,
                venue: Venue::PumpFun,
            })
        } else {
            None
        };
        let raydium = if targets.raydium_launchlab.enabled {
            Some(ProgramMatcher {
                program_id: targets.raydium_launchlab.program_id.parse()?,
                discriminator: parse_discriminator(
                    &targets.raydium_launchlab.initialize_discriminator,
                )?,
                venue: Venue::RaydiumLaunchLab,
            })
        } else {
            None
        };
        Ok(Self { pumpfun, raydium })
    }

    /// Returns all program ids this detector watches — used to narrow the
    /// Geyser subscription filter.
    pub fn watched_program_ids(&self) -> Vec<Pubkey> {
        self.pumpfun
            .iter()
            .chain(self.raydium.iter())
            .map(|m| m.program_id)
            .collect()
    }

    /// Try to decode an instruction invocation into a launch event.
    ///
    /// `program_id` is the invoked program, `data` the raw instruction data,
    /// `accounts` the ordered account pubkeys for the instruction.
    pub fn decode(
        &self,
        program_id: &Pubkey,
        data: &[u8],
        accounts: &[Pubkey],
        signature: String,
        slot: u64,
    ) -> Option<LaunchEvent> {
        let matcher = self.matcher_for(program_id)?;
        if data.len() < 8 || data[..8] != matcher.discriminator {
            return None;
        }
        let observed_at_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
        match matcher.venue {
            Venue::PumpFun => decode_pumpfun(data, accounts, signature, slot, observed_at_ms),
            Venue::RaydiumLaunchLab => {
                decode_raydium(data, accounts, signature, slot, observed_at_ms)
            }
        }
    }

    fn matcher_for(&self, program_id: &Pubkey) -> Option<&ProgramMatcher> {
        if let Some(m) = &self.pumpfun {
            if m.program_id == *program_id {
                return Some(m);
            }
        }
        if let Some(m) = &self.raydium {
            if m.program_id == *program_id {
                return Some(m);
            }
        }
        None
    }
}

fn parse_discriminator(hex_str: &str) -> Result<[u8; 8]> {
    let bytes = hex::decode(hex_str.trim_start_matches("0x"))?;
    if bytes.len() != 8 {
        anyhow::bail!(
            "discriminator must be 8 hex-encoded bytes, got {}",
            bytes.len()
        );
    }
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes);
    Ok(out)
}

// ---- venue-specific decoders ----------------------------------------------
//
// These decoders are intentionally defensive: if the instruction shape drifts
// (e.g. Pump.fun ships a new IDL) the decoder returns `None` rather than
// crashing, and the detector will simply miss that launch until configured.

fn decode_pumpfun(
    data: &[u8],
    accounts: &[Pubkey],
    signature: String,
    slot: u64,
    observed_at_ms: u64,
) -> Option<LaunchEvent> {
    // Pump.fun `create` instruction layout (observed):
    //   [0..8]   discriminator
    //   [8..]    Borsh: string name, string symbol, string uri
    //
    // Accounts (observed order): mint, mint_authority, bonding_curve,
    // associated_bonding_curve, global, mpl_token_metadata, metadata,
    // user (creator/payer), system_program, token_program, ...
    let mint = accounts.first().copied()?;
    let pool = accounts.get(2).copied();
    let creator = accounts.get(7).copied()?;
    let (name, symbol, uri) = decode_borsh_strings(&data[8..]);
    Some(LaunchEvent {
        venue: Venue::PumpFun,
        signature,
        slot,
        mint,
        creator,
        pool,
        name,
        symbol,
        uri,
        observed_at_ms,
    })
}

fn decode_raydium(
    _data: &[u8],
    accounts: &[Pubkey],
    signature: String,
    slot: u64,
    observed_at_ms: u64,
) -> Option<LaunchEvent> {
    // Raydium LaunchLab initialize layout varies by pool type; the detector
    // extracts only the identifying accounts and defers deeper parsing to the
    // filter stage which reads the pool account directly.
    let pool = accounts.first().copied();
    let mint = accounts.get(1).copied()?;
    let creator = accounts.last().copied()?;
    Some(LaunchEvent {
        venue: Venue::RaydiumLaunchLab,
        signature,
        slot,
        mint,
        creator,
        pool,
        name: None,
        symbol: None,
        uri: None,
        observed_at_ms,
    })
}

fn decode_borsh_strings(mut bytes: &[u8]) -> (Option<String>, Option<String>, Option<String>) {
    let name = read_borsh_string(&mut bytes);
    let symbol = read_borsh_string(&mut bytes);
    let uri = read_borsh_string(&mut bytes);
    (name, symbol, uri)
}

fn read_borsh_string(bytes: &mut &[u8]) -> Option<String> {
    if bytes.len() < 4 {
        return None;
    }
    let len = u32::from_le_bytes(bytes[..4].try_into().ok()?) as usize;
    if bytes.len() < 4 + len {
        return None;
    }
    let s = String::from_utf8(bytes[4..4 + len].to_vec()).ok()?;
    *bytes = &bytes[4 + len..];
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PumpFunTarget, RaydiumTarget};

    fn targets() -> TargetsConfig {
        TargetsConfig {
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
        }
    }

    #[test]
    fn parses_discriminator_hex() {
        let d = parse_discriminator("181ec828051c0777").unwrap();
        assert_eq!(d[0], 0x18);
        assert_eq!(d[7], 0x77);
    }

    #[test]
    fn detector_reports_watched_programs() {
        let det = Detector::new(&targets()).unwrap();
        assert_eq!(det.watched_program_ids().len(), 2);
    }

    #[test]
    fn non_matching_discriminator_returns_none() {
        let det = Detector::new(&targets()).unwrap();
        let program: Pubkey = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P".parse().unwrap();
        let wrong = [0u8; 16];
        let accounts: Vec<Pubkey> = (0..10).map(|_| Pubkey::new_unique()).collect();
        assert!(det
            .decode(&program, &wrong, &accounts, "sig".into(), 1)
            .is_none());
    }

    #[test]
    fn decodes_pumpfun_create_with_metadata() {
        let det = Detector::new(&targets()).unwrap();
        let program: Pubkey = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P".parse().unwrap();
        let mut data = Vec::from(parse_discriminator("181ec828051c0777").unwrap());
        // Borsh strings: name="Doge", symbol="DOGE", uri="https://x"
        for s in ["Doge", "DOGE", "https://x"] {
            data.extend_from_slice(&(s.len() as u32).to_le_bytes());
            data.extend_from_slice(s.as_bytes());
        }
        let accounts: Vec<Pubkey> = (0..10).map(|_| Pubkey::new_unique()).collect();
        let ev = det
            .decode(&program, &data, &accounts, "sig".into(), 42)
            .expect("decoded");
        assert_eq!(ev.venue, Venue::PumpFun);
        assert_eq!(ev.name.as_deref(), Some("Doge"));
        assert_eq!(ev.symbol.as_deref(), Some("DOGE"));
        assert_eq!(ev.slot, 42);
    }

    #[test]
    fn unknown_program_returns_none() {
        let det = Detector::new(&targets()).unwrap();
        let other = Pubkey::new_unique();
        assert!(det.decode(&other, &[0; 8], &[], "sig".into(), 0).is_none());
    }
}
