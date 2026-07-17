//! Protocol parameters. Fixed constants and order book deploy parameters.
//! These are pure types without networking. Values are taken from the spec.

use std::time::Duration;

/// SHELL -- the system's settlement unit. Integer count of minimal units.
pub type Shell = u64;

/// Fixed protocol constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolConsts {
    /// Platform fee, bps(on the buyer side, by-fact). `PLATFORM_FEE_BPS = 250`.
    pub platform_fee_bps: u32,
    /// Optimistic tick-acceptance window. `SETTLE_WINDOW = 180s`.
    pub settle_window: Duration,
    /// Stream inactivity timeout(no new tokens). `STREAM_TIMEOUT = 600s`.
    pub stream_timeout: Duration,
    /// Dispute window; on timeout -> split. `DISPUTE_WINDOW = 600s`.
    pub dispute_window: Duration,
    /// Rebate rate cap, bps; strictly < `platform_fee_bps`. `REBATE_MAX_BPS = 200`.
    pub rebate_max_bps: u32,
    /// Rebate rate slope, bps per tick. `REBATE_SLOPE_BPS = 4`.
    pub rebate_slope_bps: u32,
}

impl ProtocolConsts {
    /// Canonical values from / A.1.
    /// The invariant `rebate_max_bps < platform_fee_bps` is checked here:
    /// otherwise the net burn could become non-positive.
    pub const fn canonical() -> Self {
        let c = Self {
            platform_fee_bps: 250,
            settle_window: Duration::from_secs(180),
            stream_timeout: Duration::from_secs(600),
            dispute_window: Duration::from_secs(600),
            rebate_max_bps: 200,
            rebate_slope_bps: 4,
        };
        assert!(
            c.rebate_max_bps < c.platform_fee_bps,
            "anti-wash invariant: REBATE_MAX_BPS must be strictly < PLATFORM_FEE_BPS"
        );
        c
    }
}

impl Default for ProtocolConsts {
    fn default() -> Self {
        Self::canonical()
    }
}

/// Order book deploy parameters. In they are filled by a mock; in production they are read from on-chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DobParams {
    /// Tick size in tokens; reference value 1M.
    pub tick_size: u64,
    /// Seller's probe commission on the first(probe) tick. Reference: the fee from a single tick.
    pub seller_probe_commission: Shell,
}

impl DobParams {
    /// Canonical reference for: `TICK_SIZE = 1M`,
    /// `SELLER_PROBE_COMMISSION` ~ the platform fee from a single tick at `P = 1000`
    /// -- the concrete number is chosen by the deploy.
    pub const fn canonical() -> Self {
        Self {
            tick_size: 1_000_000,
            seller_probe_commission: 25, // = 250 bps * P(=1000) / 10000; reference "on the order of the fee from a tick"
        }
    }
}

impl Default for DobParams {
    fn default() -> Self {
        Self::canonical()
    }
}
