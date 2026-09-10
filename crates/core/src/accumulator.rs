//! Shell Accumulator: the SHELL <-> eccUSDC money arithmetic and the getter decoders.

//! Pure types only - no networking, no chain calls. The CLI layer supplies chain facts and this
//! module decides what may be moved.

//! The accumulator exchanges at one fixed constant ([`ACCUMULATOR_SHELL_PER_USDC_RAW`]) in both
//! directions and charges no fee, so nothing here is a quote or an estimate: every figure this
//! module produces is exact and is what the contract will do.

//! Three properties of the contract shape everything below, and each is load-bearing rather than
//! incidental (contract v1.0.2, `ShellAccumulatorRootUSDC.sol`):

//! 1. **A sell is one lot per message, in one of exactly four sizes.** `_processShellDeposit`
//! refuses any SHELL figure that is not exactly `D * SHELL_PER_USDC` for `D` in
//! {1, 10, 100, 1000} - it does not split a larger deposit and it does not keep the change. So a
//! balance is converted by sending several separately sized messages, which is what
//! [`SellPlan`] computes.
//! 2. **The refusal happens after `tvm.accept()`.** A wrongly sized deposit is not a cheap bounce,
//! so the sizes are carried as constants and checked here, before anything is sent.
//! 3. **There is no cancel and no timeout.** Once SHELL is deposited the only exit is to be matched
//! by a buyer and then claimed. That is why the planning side refuses rather than rounds.

use crate::params::{ACCUMULATOR_DENOMS, ACCUMULATOR_SHELL_PER_USDC_RAW, SHELL_UNIT, USDC_UNIT};
use serde_json::Value;

/// ABI of `ShellAccumulatorRootUSDC`, vendored from the compiled artifact.

/// Provenance: `ackinacki/ackinacki`
/// `contracts/0.79.3_compiled/accumulator/ShellAccumulatorRootUSDC.abi.json`, byte-identical to the
/// copy this client's getters were proven against. Proven rather than assumed: every getter used
/// here was executed against the LIVE roots on both one chain and another and decoded, which is the
/// check a file-level diff cannot make (the two live roots serve different code hashes).
pub const ACCUMULATOR_ROOT_ABI: &str =
    include_str!("../contracts/accumulator/ShellAccumulatorRootUSDC.abi.json");

/// ABI of `ShellSellOrderLot`, vendored from the same compiled artifact set.
pub const ACCUMULATOR_LOT_ABI: &str =
    include_str!("../contracts/accumulator/ShellSellOrderLot.abi.json");

/// One sell lot: a single message carrying exactly `shell_raw` ECC[2] SHELL to the root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SellLot {
    /// Lot size in whole eccUSDC - one of [`ACCUMULATOR_DENOMS`].
    pub denom: u16,
    /// Raw ECC[2] SHELL this one lot costs: `denom * ACCUMULATOR_SHELL_PER_USDC_RAW`.
    pub shell_raw: u128,
}

/// Everything a sell run will do, computed before it moves anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SellPlan {
    /// One entry per lot, largest denomination first. Each is a separate message.
    pub lots: Vec<SellLot>,
    /// Whole eccUSDC this plan converts.
    pub usdc_whole: u128,
    /// Raw ECC[2] SHELL that will leave the wallet in total.
    pub shell_committed_raw: u128,
    /// Raw micro-eccUSDC the lots pay out once matched and claimed. No fee is deducted.
    pub usdc_expected_raw: u128,
}

/// Why a sell plan could not be formed. Every variant is a refusal to move money.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SellPlanError {
    /// The requested conversion is zero whole eccUSDC.
    ZeroAmount,
    /// The requested conversion does not fit in the arithmetic used on chain (`uint128`).
    Overflow,
    /// The wallet holds less SHELL than the plan commits.
    InsufficientShell {
        /// Raw ECC[2] SHELL the plan needs.
        required_raw: u128,
        /// Raw ECC[2] SHELL the wallet actually holds.
        available_raw: u128,
    },
}

impl std::fmt::Display for SellPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroAmount => write!(
                f,
                "refusing to sell 0 eccUSDC worth of SHELL: the accumulator's smallest lot is {} eccUSDC ({} SHELL)",
                ACCUMULATOR_DENOMS[ACCUMULATOR_DENOMS.len() - 1],
                whole_shell_from_raw(ACCUMULATOR_SHELL_PER_USDC_RAW),
            ),
            Self::Overflow => write!(
                f,
                "refusing to sell: the amount exceeds the uint128 arithmetic the accumulator uses"
            ),
            Self::InsufficientShell {
                required_raw,
                available_raw,
            } => write!(
                f,
                "funding wallet holds insufficient SHELL ECC[2]: required={required_raw} raw ({} SHELL), \
                 available={available_raw} raw ({} SHELL), missing={} raw; nothing was submitted.",
                whole_shell_from_raw(*required_raw),
                whole_shell_from_raw(*available_raw),
                required_raw.saturating_sub(*available_raw),
            ),
        }
    }
}

impl std::error::Error for SellPlanError {}

impl SellPlan {
    /// Plan the lots for exactly `usdc_whole` whole eccUSDC.

    /// The decomposition is exact for every whole figure, and that is a property of the
    /// denomination table rather than luck: {1, 10, 100, 1000} are consecutive powers of ten and
    /// include 1, so greedy largest-first decomposition is just the decimal expansion (thousands
    /// carried into 1000-lots). **No whole eccUSDC amount is ever left unconvertible** - the only
    /// residue possible anywhere in this flow is the sub-100-SHELL dust that never reached a whole
    /// eccUSDC in the first place, which [`shell_remainder_raw`] reports.
    pub fn for_whole_usdc(usdc_whole: u128) -> Result<Self, SellPlanError> {
        if usdc_whole == 0 {
            return Err(SellPlanError::ZeroAmount);
        }
        let shell_committed_raw = usdc_whole
            .checked_mul(ACCUMULATOR_SHELL_PER_USDC_RAW)
            .ok_or(SellPlanError::Overflow)?;
        let usdc_expected_raw = usdc_whole
            .checked_mul(USDC_UNIT)
            .ok_or(SellPlanError::Overflow)?;

        // Refuse impossible requests before asking the allocator for one entry per lot. In
        // particular, a maximal CLI u64 must be a normal refusal rather than an allocation abort.
        let lot_count = ACCUMULATOR_DENOMS
            .iter()
            .try_fold((0u128, usdc_whole), |(total, left), denom| {
                let denom = u128::from(*denom);
                total
                    .checked_add(left / denom)
                    .map(|next_total| (next_total, left % denom))
            })
            .ok_or(SellPlanError::Overflow)?
            .0;
        let lot_count = usize::try_from(lot_count).map_err(|_| SellPlanError::Overflow)?;

        let mut lots = Vec::new();
        lots.try_reserve_exact(lot_count)
            .map_err(|_| SellPlanError::Overflow)?;
        let mut left = usdc_whole;
        for denom in ACCUMULATOR_DENOMS {
            let denom_u128 = u128::from(denom);
            let count = left / denom_u128;
            for _ in 0..count {
                lots.push(SellLot {
                    denom,
                    // Cannot overflow: denom <= usdc_whole and the total already multiplied cleanly.
                    shell_raw: denom_u128 * ACCUMULATOR_SHELL_PER_USDC_RAW,
                });
            }
            left -= count * denom_u128;
        }
        debug_assert_eq!(left, 0, "powers-of-ten denominations decompose exactly");

        Ok(Self {
            lots,
            usdc_whole,
            shell_committed_raw,
            usdc_expected_raw,
        })
    }

    /// Plan the largest conversion a raw ECC[2] SHELL balance supports, leaving only sub-lot dust.
    pub fn for_available_shell(shell_available_raw: u128) -> Result<Self, SellPlanError> {
        Self::for_whole_usdc(max_whole_usdc_from_shell(shell_available_raw))
    }

    /// Refuse the plan unless the wallet actually holds the SHELL it commits.
    pub fn require_funded(&self, available_raw: u128) -> Result<(), SellPlanError> {
        if self.shell_committed_raw > available_raw {
            return Err(SellPlanError::InsufficientShell {
                required_raw: self.shell_committed_raw,
                available_raw,
            });
        }
        Ok(())
    }

    /// How many separate messages this plan sends. Each one is an irreversible deposit.
    pub fn lot_count(&self) -> usize {
        self.lots.len()
    }

    /// Raw ECC[2] SHELL left in the wallet after this plan, given the balance it was planned from.
    pub fn shell_left_raw(&self, available_raw: u128) -> u128 {
        available_raw.saturating_sub(self.shell_committed_raw)
    }

    /// Lot counts per denomination, largest first, skipping denominations this plan does not use.
    pub fn denomination_counts(&self) -> Vec<(u16, usize)> {
        ACCUMULATOR_DENOMS
            .iter()
            .filter_map(|denom| {
                let count = self.lots.iter().filter(|lot| lot.denom == *denom).count();
                (count > 0).then_some((*denom, count))
            })
            .collect()
    }
}

/// A buy: one message carrying eccUSDC, answered with SHELL at the fixed rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuyPlan {
    /// Whole eccUSDC being spent.
    pub usdc_whole: u128,
    /// Raw micro-eccUSDC that will leave the wallet.
    pub usdc_raw: u128,
    /// Raw ECC[2] SHELL that will arrive.

    /// Exact, not an estimate: whatever the resting lots do not cover, the root mints, so a buyer
    /// always receives the full amount at the fixed rate (`_processUsdcDeposit`).
    pub shell_expected_raw: u128,
}

/// Why a buy plan could not be formed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuyPlanError {
    /// The requested buy is zero whole eccUSDC.
    ZeroAmount,
    /// The requested buy does not fit in the arithmetic used on chain (`uint128`).
    Overflow,
    /// The wallet holds less eccUSDC than the plan spends.
    InsufficientUsdc {
        /// Raw micro-eccUSDC the plan needs.
        required_raw: u128,
        /// Raw micro-eccUSDC the wallet actually holds.
        available_raw: u128,
    },
}

impl std::fmt::Display for BuyPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroAmount => write!(
                f,
                "refusing to buy with 0 eccUSDC: the accumulator refuses any amount that is not a whole eccUSDC"
            ),
            Self::Overflow => write!(
                f,
                "refusing to buy: the amount exceeds the uint128 arithmetic the accumulator uses"
            ),
            Self::InsufficientUsdc {
                required_raw,
                available_raw,
            } => write!(
                f,
                "funding wallet holds insufficient eccUSDC ECC[3]: required={required_raw} raw ({} eccUSDC), \
                 available={available_raw} raw ({} eccUSDC), missing={} raw; nothing was submitted.",
                whole_usdc_from_raw(*required_raw),
                whole_usdc_from_raw(*available_raw),
                required_raw.saturating_sub(*available_raw),
            ),
        }
    }
}

impl std::error::Error for BuyPlanError {}

impl BuyPlan {
    /// Plan a buy of exactly `usdc_whole` whole eccUSDC.
    pub fn for_whole_usdc(usdc_whole: u128) -> Result<Self, BuyPlanError> {
        if usdc_whole == 0 {
            return Err(BuyPlanError::ZeroAmount);
        }
        let usdc_raw = usdc_whole
            .checked_mul(USDC_UNIT)
            .ok_or(BuyPlanError::Overflow)?;
        let shell_expected_raw = usdc_whole
            .checked_mul(ACCUMULATOR_SHELL_PER_USDC_RAW)
            .ok_or(BuyPlanError::Overflow)?;
        Ok(Self {
            usdc_whole,
            usdc_raw,
            shell_expected_raw,
        })
    }

    /// Refuse the plan unless the wallet actually holds the eccUSDC it spends.
    pub fn require_funded(&self, available_raw: u128) -> Result<(), BuyPlanError> {
        if self.usdc_raw > available_raw {
            return Err(BuyPlanError::InsufficientUsdc {
                required_raw: self.usdc_raw,
                available_raw,
            });
        }
        Ok(())
    }
}

/// The whole eccUSDC a raw ECC[2] SHELL balance can convert, discarding sub-lot dust.
pub fn max_whole_usdc_from_shell(shell_raw: u128) -> u128 {
    shell_raw / ACCUMULATOR_SHELL_PER_USDC_RAW
}

/// The raw ECC[2] SHELL that cannot reach a whole eccUSDC and therefore stays in the wallet.
pub fn shell_remainder_raw(shell_raw: u128) -> u128 {
    shell_raw % ACCUMULATOR_SHELL_PER_USDC_RAW
}

/// Render raw ECC[2] SHELL as whole SHELL with its nine-decimal fraction.
pub fn whole_shell_from_raw(raw: u128) -> String {
    decimal_units(raw, SHELL_UNIT, 9)
}

/// Render raw micro-eccUSDC as whole eccUSDC with its six-decimal fraction.
pub fn whole_usdc_from_raw(raw: u128) -> String {
    decimal_units(raw, USDC_UNIT, 6)
}

fn decimal_units(raw: u128, unit: u128, decimals: usize) -> String {
    format!("{}.{:0width$}", raw / unit, raw % unit, width = decimals)
}

/// Identity of one sell lot, and the only thing needed to find it again from the chain.

/// The address is NOT derived locally. On chain it is
/// `makeAddrStd(0, hash(stateInit))` over the lot code salted with `(versionLib, root)` plus the
/// static pair `(_denom, _orderId)`, so recomputing it off-chain would mean reimplementing
/// `setCodeSalt` and pinning the library's version string - and a library version bump silently
/// re-keys every lot. The root publishes `getSellOrderAddress(D, orderId)` for exactly this reason,
/// and the reference off-chain client resolves addresses that way too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LotId {
    /// Lot size in whole eccUSDC.
    pub denom: u16,
    /// Contract-assigned FIFO position within its denomination's queue. First lot is 1.
    pub order_id: u64,
}

/// One denomination's FIFO queue, as the root reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueState {
    /// Order id the NEXT lot created in this queue will receive.
    pub next_id: u64,
    /// Lots resting unmatched.
    pub available: u64,
    /// Every lot with `order_id <= sold_prefix` has been matched by a buyer.
    pub sold_prefix: u64,
    /// Matched lots whose eccUSDC has not been claimed yet.
    pub owed_count: u64,
}

impl QueueState {
    /// Decode `getQueueState(D)`. Fails closed on a shape that is not exactly the deployed ABI.
    pub fn decode_getter(value: &Value) -> Result<Self, String> {
        Ok(Self {
            next_id: getter_u64(value, "nextId")?,
            available: getter_u64(value, "available")?,
            sold_prefix: getter_u64(value, "soldPrefix")?,
            owed_count: getter_u64(value, "owedCount")?,
        })
    }

    /// Whether a lot in this queue has been matched and can be claimed.
    pub fn is_sold(&self, order_id: u64) -> bool {
        order_id != 0 && order_id <= self.sold_prefix
    }

    /// Order ids ever issued in this queue, oldest first. Empty before the first lot.
    pub fn issued_order_ids(&self) -> std::ops::Range<u64> {
        1..self.next_id
    }
}

/// A lot's own view of itself, from `ShellSellOrderLot.getDetails()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LotDetails {
    /// Accumulator root that deployed the lot.
    pub root: String,
    /// Address the eccUSDC payout goes to. Fixed at construction.
    pub owner: String,
    /// Lot size in whole eccUSDC.
    pub denom: u16,
    /// FIFO position within its denomination's queue.
    pub order_id: u64,
    /// Whether `claim()` has already been called and not bounced back.
    pub claimed: bool,
}

impl LotDetails {
    /// Decode `getDetails()`. Fails closed on a shape that is not exactly the deployed ABI.
    pub fn decode_getter(value: &Value) -> Result<Self, String> {
        Ok(Self {
            root: getter_string(value, "root")?,
            owner: getter_string(value, "owner")?,
            denom: u16::try_from(getter_u64(value, "denom")?)
                .map_err(|_| "getDetails.denom does not fit a uint16".to_string())?,
            order_id: getter_u64(value, "orderId")?,
            claimed: value
                .get("claimed")
                .and_then(Value::as_bool)
                .ok_or_else(|| "getDetails.claimed missing or not a bool".to_string())?,
        })
    }

    /// This lot's identity.
    pub fn id(&self) -> LotId {
        LotId {
            denom: self.denom,
            order_id: self.order_id,
        }
    }
}

/// The root's headline figures, from `getDetails()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootDetails {
    /// Raw ECC[2] SHELL held on behalf of sellers whose lots have not been matched out.
    pub seller_shell_pool_raw: u128,
    /// Raw micro-eccUSDC the root holds.
    pub usdc_balance_raw: u128,
    /// Raw micro-eccUSDC already owed to sellers whose lots are matched but unclaimed.
    pub owed_total_raw: u128,
}

impl RootDetails {
    /// Decode `getDetails()`. Fails closed on a shape that is not exactly the deployed ABI.
    pub fn decode_getter(value: &Value) -> Result<Self, String> {
        Ok(Self {
            seller_shell_pool_raw: getter_u128(value, "sellerShellPool")?,
            usdc_balance_raw: getter_u128(value, "usdcBalance")?,
            owed_total_raw: getter_u128(value, "owedTotal")?,
        })
    }

    /// eccUSDC not owed to any seller - what NACKL redemption draws on.
    pub fn free_reserve_raw(&self) -> u128 {
        self.usdc_balance_raw.saturating_sub(self.owed_total_raw)
    }
}

fn getter_field<'a>(value: &'a Value, field: &str) -> Result<&'a Value, String> {
    value
        .get(field)
        .ok_or_else(|| format!("getter field '{field}' missing"))
}

fn getter_u64(value: &Value, field: &str) -> Result<u64, String> {
    let raw = getter_field(value, field)?;
    if let Some(n) = raw.as_u64() {
        return Ok(n);
    }
    raw.as_str()
        .ok_or_else(|| format!("getter field '{field}' is neither a string nor an integer"))?
        .parse::<u64>()
        .map_err(|e| format!("getter field '{field}': {e}"))
}

fn getter_u128(value: &Value, field: &str) -> Result<u128, String> {
    let raw = getter_field(value, field)?;
    if let Some(n) = raw.as_u64() {
        return Ok(u128::from(n));
    }
    raw.as_str()
        .ok_or_else(|| format!("getter field '{field}' is neither a string nor an integer"))?
        .parse::<u128>()
        .map_err(|e| format!("getter field '{field}': {e}"))
}

fn getter_string(value: &Value, field: &str) -> Result<String, String> {
    getter_field(value, field)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("getter field '{field}' is not a string"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abi_function<'a>(abi: &'a Value, name: &str) -> &'a Value {
        abi["functions"]
            .as_array()
            .expect("ABI functions")
            .iter()
            .find(|function| function["name"] == name)
            .unwrap_or_else(|| panic!("{name} must exist in the vendored ABI"))
    }

    fn field_names(list: &Value) -> Vec<String> {
        list.as_array()
            .expect("ABI parameter list")
            .iter()
            .map(|param| param["name"].as_str().expect("param name").to_string())
            .collect()
    }

    /// Pin every decoder and every call this client makes against the VENDORED ABI's own field
    /// lists. A decoder that reads a field the deployed ABI does not serve passes all the unit
    /// tests above - which build their input by hand - while being dead on chain. This is the test
    /// that fails when the two drift apart.
    #[test]
    fn decoders_and_calls_match_the_vendored_abi_field_lists() {
        let root: Value = serde_json::from_str(ACCUMULATOR_ROOT_ABI).expect("parse root ABI");
        let lot: Value = serde_json::from_str(ACCUMULATOR_LOT_ABI).expect("parse lot ABI");

        // Reads: the field names QueueState/RootDetails/LotDetails decode.
        assert_eq!(
            field_names(&abi_function(&root, "getQueueState")["outputs"]),
            ["nextId", "available", "soldPrefix", "owedCount"]
        );
        assert_eq!(
            field_names(&abi_function(&root, "getDetails")["outputs"]),
            ["ownerPubkey", "sellerShellPool", "usdcBalance", "owedTotal"]
        );
        assert_eq!(
            field_names(&abi_function(&lot, "getDetails")["outputs"]),
            ["root", "owner", "denom", "orderId", "claimed"]
        );
        assert_eq!(
            field_names(&abi_function(&root, "getSellOrderAddress")["outputs"]),
            ["sellOrderAddr"]
        );
        assert_eq!(
            field_names(&abi_function(&root, "getVersion")["outputs"]),
            ["value0", "value1"]
        );
        assert_eq!(
            field_names(&abi_function(&lot, "getVersion")["outputs"]),
            ["value0", "value1"]
        );

        // Calls: the argument names the CLI encodes.
        assert_eq!(
            field_names(&abi_function(&root, "getQueueState")["inputs"]),
            ["D"]
        );
        assert_eq!(
            field_names(&abi_function(&root, "getSellOrderAddress")["inputs"]),
            ["D", "orderId"]
        );
        assert_eq!(
            field_names(&abi_function(&root, "buyShellFor")["inputs"]),
            ["buyer"]
        );
        assert_eq!(
            field_names(&abi_function(&lot, "claim")["inputs"]),
            [] as [&str; 0]
        );

        // The sell direction has NO named method: a lot is created by a bare ECC[2] transfer into
        // `receive()`. If a future generation adds one, this assertion is where we find out.
        assert!(
            root["functions"]
                .as_array()
                .expect("functions")
                .iter()
                .all(|function| function["name"] != "sellShell"
                    && function["name"] != "createSellOrder"),
            "a named sell entry point would change how `accumulator sell` must submit"
        );
    }

    #[test]
    fn the_four_denominations_are_the_contract_table_largest_first() {
        assert_eq!(ACCUMULATOR_DENOMS, [1000, 100, 10, 1]);
        assert_eq!(ACCUMULATOR_SHELL_PER_USDC_RAW, 100_000_000_000);
        assert_eq!(USDC_UNIT, 1_000_000);
    }

    #[test]
    fn one_lot_of_each_denomination_costs_the_contract_figure() {
        // The nanoSHELL column of the denomination table in the contract docs.
        for (denom, shell_raw) in [
            (1u16, 100_000_000_000u128),
            (10, 1_000_000_000_000),
            (100, 10_000_000_000_000),
            (1000, 100_000_000_000_000),
        ] {
            let plan = SellPlan::for_whole_usdc(u128::from(denom)).expect("plan");
            assert_eq!(plan.lots, vec![SellLot { denom, shell_raw }]);
            assert_eq!(plan.shell_committed_raw, shell_raw);
        }
    }

    #[test]
    fn a_whole_usdc_amount_always_decomposes_exactly_so_only_sub_lot_dust_remains() {
        // The property the operator is entitled to rely on: no whole eccUSDC is ever stranded.
        for usdc in [1u128, 5, 9, 10, 11, 99, 123, 154, 999, 1000, 1001, 12_345] {
            let plan = SellPlan::for_whole_usdc(usdc).expect("plan");
            let summed: u128 = plan.lots.iter().map(|lot| u128::from(lot.denom)).sum();
            assert_eq!(
                summed, usdc,
                "lots must sum to the requested amount ({usdc})"
            );
            assert_eq!(
                plan.shell_committed_raw,
                plan.lots.iter().map(|lot| lot.shell_raw).sum::<u128>()
            );
            assert!(plan
                .lots
                .iter()
                .all(|lot| ACCUMULATOR_DENOMS.contains(&lot.denom)));
        }
    }

    #[test]
    fn the_documented_154_usdc_example_matches_the_contract_docs() {
        // "A 154 eccUSDC buy matches 1x100 + 5x10 + 4x1 lots" - overview.
        let plan = SellPlan::for_whole_usdc(154).expect("plan");
        assert_eq!(plan.denomination_counts(), vec![(100, 1), (10, 5), (1, 4)]);
        assert_eq!(plan.lot_count(), 10);
    }

    #[test]
    fn lots_are_ordered_largest_denomination_first() {
        let plan = SellPlan::for_whole_usdc(1111).expect("plan");
        let denoms: Vec<u16> = plan.lots.iter().map(|lot| lot.denom).collect();
        assert_eq!(denoms, vec![1000, 100, 10, 1]);
    }

    #[test]
    fn a_remainder_that_cannot_reach_a_whole_lot_is_reported_and_left_behind() {
        // 12_345.678 SHELL: 123 whole eccUSDC convert, 45.678 SHELL cannot.
        let balance_raw = 12_345_678_000_000u128;
        let plan = SellPlan::for_available_shell(balance_raw).expect("plan");
        assert_eq!(plan.usdc_whole, 123);
        assert_eq!(plan.shell_committed_raw, 12_300_000_000_000);
        assert_eq!(shell_remainder_raw(balance_raw), 45_678_000_000);
        assert_eq!(plan.shell_left_raw(balance_raw), 45_678_000_000);
        assert_eq!(whole_shell_from_raw(45_678_000_000), "45.678000000");
    }

    #[test]
    fn a_balance_under_one_lot_converts_nothing_and_says_so() {
        // 99.9 SHELL is short of the 100 SHELL minimum lot.
        let err = SellPlan::for_available_shell(99_900_000_000).expect_err("must refuse");
        assert_eq!(err, SellPlanError::ZeroAmount);
        assert!(err.to_string().contains("smallest lot is 1 eccUSDC"));
    }

    #[test]
    fn a_plan_the_wallet_cannot_fund_is_refused_without_submitting() {
        let plan = SellPlan::for_whole_usdc(10).expect("plan");
        let err = plan
            .require_funded(999_999_999_999)
            .expect_err("must refuse");
        assert!(matches!(err, SellPlanError::InsufficientShell { .. }));
        let rendered = err.to_string();
        assert!(rendered.contains("nothing was submitted"), "{rendered}");
        assert!(rendered.contains("missing=1"), "{rendered}");
        plan.require_funded(1_000_000_000_000)
            .expect("exact funding");
    }

    #[test]
    fn an_impossibly_large_sell_is_refused_without_allocation_abort() {
        assert_eq!(
            SellPlan::for_whole_usdc(u128::from(u64::MAX)).expect_err("must refuse"),
            SellPlanError::Overflow
        );
    }

    #[test]
    fn a_buy_is_exact_because_the_shortfall_is_minted() {
        let plan = BuyPlan::for_whole_usdc(154).expect("plan");
        assert_eq!(plan.usdc_raw, 154_000_000);
        assert_eq!(plan.shell_expected_raw, 15_400_000_000_000);
        assert_eq!(BuyPlan::for_whole_usdc(0), Err(BuyPlanError::ZeroAmount));
    }

    #[test]
    fn a_buy_the_wallet_cannot_fund_is_refused_without_submitting() {
        let plan = BuyPlan::for_whole_usdc(100).expect("plan");
        let err = plan.require_funded(99_999_999).expect_err("must refuse");
        assert!(matches!(err, BuyPlanError::InsufficientUsdc { .. }));
        assert!(err.to_string().contains("nothing was submitted"));
        plan.require_funded(100_000_000).expect("exact funding");
    }

    #[test]
    fn shell_and_usdc_scales_are_not_interchangeable() {
        // Nine decimals against six: the same raw figure means different money.
        assert_eq!(whole_shell_from_raw(1_000_000_000), "1.000000000");
        assert_eq!(whole_usdc_from_raw(1_000_000_000), "1000.000000");
    }

    #[test]
    fn queue_state_decodes_the_deployed_shape_and_rejects_anything_else() {
        let live = serde_json::json!({
            "nextId": "103", "available": "0", "soldPrefix": "102", "owedCount": "25"
        });
        let state = QueueState::decode_getter(&live).expect("deployed shape");
        assert_eq!(state.next_id, 103);
        assert_eq!(state.sold_prefix, 102);
        assert_eq!(state.owed_count, 25);
        assert_eq!(state.issued_order_ids(), 1..103);
        assert!(state.is_sold(102));
        assert!(!state.is_sold(103));
        assert!(!state.is_sold(0));

        assert!(QueueState::decode_getter(&serde_json::json!({
            "nextId": "1", "available": "0", "soldPrefix": "0"
        }))
        .is_err());
    }

    #[test]
    fn lot_details_decode_the_deployed_shape() {
        let details = LotDetails::decode_getter(&serde_json::json!({
            "root": "0:3535353535353535353535353535353535353535353535353535353535353535",
            "owner": "0:00000000000000000000000000000000000000000000000000000000000000ab",
            "denom": "10",
            "orderId": "7",
            "claimed": false
        }))
        .expect("deployed shape");
        assert_eq!(
            details.id(),
            LotId {
                denom: 10,
                order_id: 7
            }
        );
        assert!(!details.claimed);
    }

    #[test]
    fn root_details_free_reserve_is_balance_less_what_sellers_are_owed() {
        let details = RootDetails::decode_getter(&serde_json::json!({
            "ownerPubkey": "0x00",
            "sellerShellPool": "10000000000000",
            "usdcBalance": "3882958294",
            "owedTotal": "275000000"
        }))
        .expect("deployed shape");
        assert_eq!(details.free_reserve_raw(), 3_607_958_294);
        assert_eq!(whole_usdc_from_raw(details.owed_total_raw), "275.000000");
    }
}
