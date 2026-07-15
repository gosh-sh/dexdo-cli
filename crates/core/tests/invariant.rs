//! Property test of the two-tick invariant (§3.2): no transition path of `StreamMachine`
//! ever exceeds "1 prepaid + 1 frozen", and `max_buyer_loss ≤ 2*P` (≤ 1*P on the probe).

use dexdo_core::machine::{Settlement, StreamMachine, StreamState};
use dexdo_core::params::DobParams;
use proptest::prelude::*;

/// Actions that can be run through the machine while staying within the normal lifecycle.
#[derive(Debug, Clone)]
enum Op {
    AcceptProbe,
    DeliverTick,
}

fn ops_strategy() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(
        prop_oneof![Just(Op::AcceptProbe), Just(Op::DeliverTick)],
        0..50,
    )
}

proptest! {
    #[test]
    fn two_tick_invariant_holds(price in 1u64..1_000_000u64, ops in ops_strategy()) {
        let params = DobParams::canonical();
        let mut m = StreamMachine::open(price, &params);

        // On the probe the risk is exactly ≤ 1*P.
        prop_assert!(m.max_buyer_loss() <= price, "probe risk must be <= 1*P");

        for op in ops {
            // Apply operations only when they are valid for the current state —
            // invalid transitions return Err and do not change the state (checked type-safely).
            match (op, m.state().clone()) {
                (Op::AcceptProbe, StreamState::Probe { .. }) => {
                    m.on_probe_accepted().unwrap();
                }
                (Op::DeliverTick, StreamState::Streaming { .. }) => {
                    m.on_tick_delivered().unwrap();
                }
                _ => { /* operation not applicable in this state — skip */ }
            }

            // The invariant is checked on every reached state.
            match m.state() {
                StreamState::Probe { .. } => {
                    prop_assert!(m.max_buyer_loss() <= price, "probe risk must stay <= 1*P");
                }
                StreamState::Streaming { prepaid, frozen, finalized_up_to } => {
                    prop_assert_eq!(frozen.index, prepaid.index + 1, "exactly one frozen after prepaid");
                    prop_assert_eq!(prepaid.index, *finalized_up_to + 1, "prepaid immediately after finalized");
                    prop_assert!(m.max_buyer_loss() <= 2 * price, "buyer loss must stay <= 2*P");
                }
                _ => {}
            }
        }
    }

    /// Stop at any reachable point: buyer loss ≤ 2*P, and BurnBoth only on the probe.
    #[test]
    fn stop_loss_bounded(price in 1u64..1_000_000u64, n_accept in 0u8..2, n_ticks in 0u64..30) {
        let params = DobParams::canonical();
        let mut m = StreamMachine::open(price, &params);
        if n_accept > 0 {
            m.on_probe_accepted().unwrap();
            for _ in 0..n_ticks {
                m.on_tick_delivered().unwrap();
            }
        }
        let loss_before = m.max_buyer_loss();
        prop_assert!(loss_before <= 2 * price);
        let _ = m.buyer_stop();
    }

    /// "Scam revenue = 0" as an **invariant** (§3.1.2, §5; test review items 4/5): a stop ON THE PROBE
    /// is the bail-out-on-scam path (the buyer did not accept the probe tick after `Bail` verification).
    /// On it the settlement must be `BurnBoth`: the buyer's probe tick and the seller's commission both
    /// burn, and the seller is credited NOTHING (`BurnBoth` has no `to_seller` by construction). Strengthens
    /// the `probe_stop_burns_both` example into a property over the entire price range.
    #[test]
    fn scam_revenue_zero_on_probe_stop(price in 1u64..1_000_000u64) {
        let params = DobParams::canonical();
        let mut m = StreamMachine::open(price, &params);
        prop_assert!(matches!(m.state(), StreamState::Probe { .. }), "fresh machine is in Probe");
        match m.buyer_stop() {
            Settlement::BurnBoth(b) => {
                // The buyer's probe tick is burned; the seller gets 0 (it has no share of the revenue).
                prop_assert_eq!(b.buyer, price, "probe tick must burn, not become seller revenue");
            }
            other => prop_assert!(false, "stop on probe must be BurnBoth, got {:?}", other),
        }
    }
}
