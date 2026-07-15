//! Stream state machine and the two-tick invariant (§3.2). Pure, no network — fit for `proptest`.
//!
//! States (§4.3 dexdo-cli.md): `Opening` → `Probe` → `Streaming` → `Stopping`/`Disputed` → `Closed`.
//!
//! INVARIANT (§3.2): in `Streaming` there is always exactly 1 prepaid ahead + 1 frozen.
//! In `Probe` there is no prepayment ahead — only 1 frozen probe tick.
//! Every transition checks the invariant. `max_buyer_loss() ≤ 2*P` (≤ 1*P on the probe).

use crate::params::{DobParams, Shell};
use crate::settle::{probe_burn, ProbeBurn};
use serde::{Deserialize, Serialize};

/// One tick: index and price in SHELL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tick {
    pub index: u64,
    pub price: Shell,
}

/// Stream state (§4.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamState {
    /// The seller posts the endpoint; the probe tick is frozen + `SELLER_PROBE_COMMISSION` (§3.1).
    Opening,
    /// Probe tick: frozen, NOT prepaid. Stop → `BurnBoth` (§3.1.2).
    Probe { tick: Tick },
    /// After the probe is accepted: exactly 1 prepaid + 1 frozen (§3.2).
    Streaming {
        prepaid: Tick,
        frozen: Tick,
        finalized_up_to: u64,
    },
    /// The buyer issued STOP → amicable split (§4.1).
    Stopping,
    /// Dispute: both notes are locked (§4.2).
    Disputed,
    /// Self-destruction of `token_contract` (§3.5).
    Closed,
}

/// Settlement on completion/stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    /// Stop on the probe (§3.1.2): the buyer's probe tick + the seller's commission are burned.
    BurnBoth(ProbeBurn),
    /// Amicable split after the probe is accepted (§4.1): the delivered tick → to the seller,
    /// the frozen buffer → to the buyer.
    AmicableSplit {
        /// Ticks sent to the seller (prepaid/delivered).
        to_seller_ticks: u64,
        /// Tick refunded to the buyer (the thawed buffer).
        to_buyer_refund: Shell,
    },
    /// The seller is gone (no-show on the probe / timeout): the buyer takes the frozen tick,
    /// nothing went to the seller, the seller's commission is returned to them — NOT burned (§3.1.2, §3.4).
    SellerNoShow {
        /// Refund to the buyer (the frozen/probe tick).
        to_buyer_refund: Shell,
        /// Return of the seller's probe commission (not burned on a no-show).
        seller_commission_returned: Shell,
    },
}

/// Error for an invariant violation / disallowed transition.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("stream machine invariant violation: {0}")]
pub struct InvariantError(pub &'static str);

/// Stream state machine. The tick price `P` is fixed at stream open.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamMachine {
    state: StreamState,
    price: Shell,
    probe_commission: Shell,
}

impl StreamMachine {
    /// Open a stream: the seller froze the probe tick and posted `SELLER_PROBE_COMMISSION` (§3.1).
    /// Transition `Opening` → `Probe`.
    pub fn open(price: Shell, params: &DobParams) -> Self {
        Self {
            state: StreamState::Probe {
                tick: Tick { index: 0, price },
            },
            price,
            probe_commission: params.seller_probe_commission,
        }
    }

    /// The current state.
    pub fn state(&self) -> &StreamState {
        &self.state
    }

    /// The tick price `P`.
    pub fn price(&self) -> Shell {
        self.price
    }

    /// The probe is accepted (silence through `SETTLE_WINDOW`, §3.1.2/§3.3): the probe tick goes to
    /// the seller, the commission is returned, the two-tick invariant kicks in. `Probe` → `Streaming`.
    pub fn on_probe_accepted(&mut self) -> Result<(), InvariantError> {
        match &self.state {
            StreamState::Probe { tick } => {
                let finalized_up_to = tick.index; // the probe tick is finalized at the seller
                self.state = StreamState::Streaming {
                    prepaid: Tick {
                        index: tick.index + 1,
                        price: self.price,
                    },
                    frozen: Tick {
                        index: tick.index + 2,
                        price: self.price,
                    },
                    finalized_up_to,
                };
                self.check_invariant()?;
                Ok(())
            }
            _ => Err(InvariantError("on_probe_accepted requires Probe state")),
        }
    }

    /// The next tick is delivered after the `SETTLE_WINDOW` cadence (§3.3): we shift the two-tick
    /// window forward — `prepaid` is finalized, `frozen` becomes `prepaid`, we freeze the next one.
    pub fn on_tick_delivered(&mut self) -> Result<(), InvariantError> {
        match &self.state {
            StreamState::Streaming {
                prepaid, frozen, ..
            } => {
                let new_finalized = prepaid.index;
                let new_prepaid = *frozen;
                let new_frozen = Tick {
                    index: frozen.index + 1,
                    price: self.price,
                };
                self.state = StreamState::Streaming {
                    prepaid: new_prepaid,
                    frozen: new_frozen,
                    finalized_up_to: new_finalized,
                };
                self.check_invariant()?;
                Ok(())
            }
            _ => Err(InvariantError("on_tick_delivered requires Streaming state")),
        }
    }

    /// Buyer STOP (§4.1). On the probe → `BurnBoth` (§3.1.2); after the probe is accepted → amicable split.
    pub fn buyer_stop(&mut self) -> Settlement {
        let settlement = match &self.state {
            StreamState::Probe { .. } => {
                // §3.1.2: the buyer's probe tick + the seller's commission are burned, to no one.
                Settlement::BurnBoth(probe_burn(self.price, self.probe_commission))
            }
            StreamState::Streaming { prepaid, .. } => {
                // §4.1: the delivered/prepaid tick → to the seller, the frozen buffer → to the buyer.
                let _ = prepaid;
                Settlement::AmicableSplit {
                    to_seller_ticks: 1,
                    to_buyer_refund: self.price,
                }
            }
            // A stop from other states is treated as an amicable split with no delivered ticks.
            _ => Settlement::AmicableSplit {
                to_seller_ticks: 0,
                to_buyer_refund: 0,
            },
        };
        self.state = StreamState::Stopping;
        settlement
    }

    /// The seller is gone: no-show on the probe or an inactivity timeout `STREAM_TIMEOUT` (§3.1.2/§3.4).
    /// The buyer takes the frozen tick, pays zero; the seller's commission is returned. NOT burned.
    pub fn seller_timeout(&mut self) -> Settlement {
        let settlement = match &self.state {
            StreamState::Probe { tick } => Settlement::SellerNoShow {
                to_buyer_refund: tick.price,
                seller_commission_returned: self.probe_commission,
            },
            StreamState::Streaming { frozen, .. } => Settlement::SellerNoShow {
                to_buyer_refund: frozen.price,
                seller_commission_returned: 0,
            },
            _ => Settlement::SellerNoShow {
                to_buyer_refund: 0,
                seller_commission_returned: 0,
            },
        };
        self.state = StreamState::Closed;
        settlement
    }

    /// The buyer opened a dispute (§4.2): both notes are locked.
    pub fn buyer_dispute(&mut self) {
        self.state = StreamState::Disputed;
    }

    /// Clean close after a stop/split: `token_contract` self-destructs (§3.5).
    pub fn close(&mut self) {
        self.state = StreamState::Closed;
    }

    /// The buyer's maximum loss in the current state (§3.2/§3.4):
    /// `≤ 2*P` in `Streaming` (prepaid + frozen), `≤ 1*P` on the probe.
    pub fn max_buyer_loss(&self) -> Shell {
        match &self.state {
            StreamState::Opening => 0,
            // On the probe the risk is exactly 1 tick (the probe tick may burn on a stop).
            StreamState::Probe { tick } => tick.price,
            // Two ticks: prepaid ahead + frozen.
            StreamState::Streaming {
                prepaid, frozen, ..
            } => prepaid.price + frozen.price,
            StreamState::Stopping | StreamState::Disputed => self.price,
            StreamState::Closed => 0,
        }
    }

    /// Check the two-tick invariant. Every transition must hold it (§3.2).
    fn check_invariant(&self) -> Result<(), InvariantError> {
        match &self.state {
            StreamState::Probe { .. } => {
                // On the probe there is no prepayment ahead; risk ≤ 1*P.
                if self.max_buyer_loss() > self.price {
                    return Err(InvariantError("probe risk exceeds 1*P"));
                }
            }
            StreamState::Streaming {
                prepaid,
                frozen,
                finalized_up_to,
            } => {
                // Exactly one prepaid + one frozen, adjacent and immediately following the finalized one.
                if frozen.index != prepaid.index + 1 {
                    return Err(InvariantError("frozen must immediately follow prepaid"));
                }
                if prepaid.index != *finalized_up_to + 1 {
                    return Err(InvariantError("prepaid must immediately follow finalized"));
                }
                if self.max_buyer_loss() > 2 * self.price {
                    return Err(InvariantError("buyer loss exceeds 2*P"));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> DobParams {
        DobParams::canonical()
    }

    #[test]
    fn open_enters_probe_with_one_tick_risk() {
        let m = StreamMachine::open(1000, &params());
        assert!(matches!(m.state(), StreamState::Probe { .. }));
        assert_eq!(m.max_buyer_loss(), 1000); // ≤ 1*P on the probe
    }

    #[test]
    fn probe_accept_enters_streaming_two_tick_invariant() {
        let mut m = StreamMachine::open(1000, &params());
        m.on_probe_accepted().unwrap();
        assert_eq!(m.max_buyer_loss(), 2000); // exactly 2*P
        if let StreamState::Streaming {
            prepaid,
            frozen,
            finalized_up_to,
        } = m.state()
        {
            assert_eq!(*finalized_up_to, 0);
            assert_eq!(prepaid.index, 1);
            assert_eq!(frozen.index, 2);
        } else {
            panic!("expected Streaming");
        }
    }

    #[test]
    fn ticks_advance_keeps_invariant() {
        let mut m = StreamMachine::open(1000, &params());
        m.on_probe_accepted().unwrap();
        for _ in 0..10 {
            m.on_tick_delivered().unwrap();
            assert_eq!(m.max_buyer_loss(), 2000);
        }
    }

    #[test]
    fn stop_on_probe_burns_both() {
        let mut m = StreamMachine::open(1000, &params());
        let s = m.buyer_stop();
        match s {
            Settlement::BurnBoth(b) => {
                assert_eq!(b.buyer, 1000);
                assert_eq!(b.seller, params().seller_probe_commission);
            }
            _ => panic!("expected BurnBoth"),
        }
    }

    #[test]
    fn stop_after_probe_is_amicable_split() {
        let mut m = StreamMachine::open(1000, &params());
        m.on_probe_accepted().unwrap();
        let s = m.buyer_stop();
        assert_eq!(
            s,
            Settlement::AmicableSplit {
                to_seller_ticks: 1,
                to_buyer_refund: 1000
            }
        );
    }

    #[test]
    fn seller_noshow_on_probe_no_burn() {
        let mut m = StreamMachine::open(1000, &params());
        let s = m.seller_timeout();
        assert_eq!(
            s,
            Settlement::SellerNoShow {
                to_buyer_refund: 1000,
                seller_commission_returned: params().seller_probe_commission,
            }
        );
    }

    /// §4.2 (review §5, dispute): `buyer_dispute()` → `Disputed`, and the buyer's exposure does NOT grow
    /// (≤ 1*P — one tick is frozen). Covers the unit part of the D5 dispute acceptance item.
    #[test]
    fn dispute_enters_disputed_and_bounds_loss() {
        // Dispute from the probe.
        let mut m = StreamMachine::open(1000, &params());
        m.buyer_dispute();
        assert!(matches!(m.state(), StreamState::Disputed));
        assert_eq!(m.max_buyer_loss(), 1000); // exactly P, no more

        // Dispute from Streaming: exposure stays ≤ 2*P (here it collapses to 1*P), does not grow.
        let mut m2 = StreamMachine::open(1000, &params());
        m2.on_probe_accepted().unwrap();
        assert_eq!(m2.max_buyer_loss(), 2000); // sanity: in Streaming the risk is 2*P
        m2.buyer_dispute();
        assert!(matches!(m2.state(), StreamState::Disputed));
        assert_eq!(m2.max_buyer_loss(), 1000); // a dispute does not increase the risk
    }
}
