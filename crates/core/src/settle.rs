//! Settlement: fee, rebate, net burn, probe burn (§5, A.3).
//!
//! Pure formulas, no network. Covered by golden tests against table A.5.

use crate::params::{ProtocolConsts, Shell};

/// Platform fee by-fact, on the buyer's side (§5.1).
/// `delivered_ticks` — the ticks actually delivered, `p` — the tick price.
pub fn fee(delivered_ticks: u64, p: Shell, c: &ProtocolConsts) -> Shell {
    mul_bps(c.platform_fee_bps, delivered_ticks, p)
}

/// Rebate rate in bps: `min(REBATE_MAX_BPS, SLOPE * n)` (§5.3).
pub fn rebate_rate_bps(n_clean_ticks: u64, c: &ProtocolConsts) -> u32 {
    let slope = (c.rebate_slope_bps as u64).saturating_mul(n_clean_ticks);
    (slope.min(c.rebate_max_bps as u64)) as u32
}

/// Rebate to the seller — only after a clean close without a dispute (§5.3).
/// `0` on dispute/under-delivery (the caller simply does not invoke this function).
pub fn rebate(n_clean_ticks: u64, p: Shell, c: &ProtocolConsts) -> Shell {
    mul_bps(rebate_rate_bps(n_clean_ticks, c), n_clean_ticks, p)
}

/// Net burn `= (fee_rate - rebate_rate) * n * P`. Always `> 0` when `n > 0` on the canonical
/// constants (`rebate_rate_bps < platform_fee_bps` by construction, §5.3). `saturating_sub`
/// (review #2): `ProtocolConsts` fields are public and constructed directly (adapters/tests) — on
/// a pathological `rebate_max_bps ≥ platform_fee_bps` we return `0` instead of panic(debug)/wrap(release).
pub fn net_burn(n_clean_ticks: u64, p: Shell, c: &ProtocolConsts) -> Shell {
    let rate = c
        .platform_fee_bps
        .saturating_sub(rebate_rate_bps(n_clean_ticks, c));
    mul_bps(rate, n_clean_ticks, p)
}

/// Burn on an early stop at the probe tick (§3.1.2, §5.4):
/// the buyer's probe tick (`P`) + the seller's probe commission — both burned, to no one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeBurn {
    /// The buyer's probe tick — burned.
    pub buyer: Shell,
    /// The seller's probe commission — burned.
    pub seller: Shell,
}

impl ProbeBurn {
    /// Total SHELL burned.
    pub fn total(&self) -> Shell {
        self.buyer + self.seller
    }
}

/// `probe_burn`: tick price `p` from the buyer + `seller_probe_commission` from the seller (§3.1.2).
pub fn probe_burn(p: Shell, seller_probe_commission: Shell) -> ProbeBurn {
    ProbeBurn {
        buyer: p,
        seller: seller_probe_commission,
    }
}

/// `bps / 10000 * n * value`, in whole SHELL (rounded down).
fn mul_bps(bps: u32, n: u64, value: Shell) -> Shell {
    ((bps as u128) * (n as u128) * (value as u128) / 10_000u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden table A.5: REBATE_MAX_BPS=200, REBATE_SLOPE_BPS=4, P normalized to 10000
    // (so fractions like 0.25·P etc. are whole numbers). Then P = 10000 → 1.0·P = 10000.
    const P: Shell = 10_000;

    fn c() -> ProtocolConsts {
        ProtocolConsts::canonical()
    }

    #[test]
    fn rebate_table_a5() {
        let c = c();
        // n=50: rate 2.00% (cap reached), rebate 1.0·P, net burn 0.25·P
        assert_eq!(rebate_rate_bps(50, &c), 200);
        assert_eq!(rebate(50, P, &c), P); // 1.0·P
        assert_eq!(net_burn(50, P, &c), P / 4); // 0.25·P

        // n=100: rebate 2.0·P, net burn 0.50·P (threshold N*)
        assert_eq!(rebate(100, P, &c), 2 * P);
        assert_eq!(net_burn(100, P, &c), P / 2);

        // n=150: rebate 3.0·P, net burn 0.75·P
        assert_eq!(rebate(150, P, &c), 3 * P);
        assert_eq!(net_burn(150, P, &c), 3 * P / 4);

        // n=1000: rebate 20·P, net burn 5·P
        assert_eq!(rebate(1000, P, &c), 20 * P);
        assert_eq!(net_burn(1000, P, &c), 5 * P);
    }

    #[test]
    fn rebate_strictly_below_fee_always() {
        let c = c();
        for n in 1u64..=2000 {
            assert!(
                rebate_rate_bps(n, &c) < c.platform_fee_bps,
                "rebate rate must stay strictly below platform fee"
            );
            assert!(net_burn(n, P, &c) > 0, "net burn must be positive for n>0");
        }
    }

    /// #2 (regression): `ProtocolConsts` fields are public and can be constructed directly
    /// (adapters/tests) with a pathological `rebate_max_bps ≥ platform_fee_bps`. `net_burn`
    /// then saturates to 0, instead of panicking (debug) / wrapping (release).
    #[test]
    fn net_burn_saturates_on_pathological_consts() {
        let mut c = c();
        c.rebate_max_bps = c.platform_fee_bps + 100; // rebate may exceed the fee
                                                     // Large n: the rebate rate reaches the cap (≥ fee) → net rate = 0.
        assert_eq!(
            net_burn(1_000_000, P, &c),
            0,
            "saturating_sub: burn=0 instead of underflow"
        );
    }

    #[test]
    fn probe_burn_goes_nowhere() {
        let b = probe_burn(P, 25);
        assert_eq!(b.buyer, P);
        assert_eq!(b.seller, 25);
        assert_eq!(b.total(), P + 25);
    }
}
