//! Deal money conservation -- the ledger identity a settled deal must satisfy, replayed one
//! transition at a time. Pure arithmetic over figures already read from the chain; no I/O.

//! # Why this exists

//! `TokenContract` (generation 4.0.33 onward) holds the deal's SHELL as a **scalar**,
//! `uint128 _balance`, not as currency on the account. Every movement is one of exactly three
//! primitives, and there are no others in the contract:

//! * `_balance += amount` -- the three funding entries `fundFromOrderBook`, `fundDeal`,
//! `fundBuyerBond` (`contracts/airegistry/TokenContract.sol:694`, `:894`, `:952`);
//! * `_payShell(to, amount)` -- `_balance -= amount`, then `creditFromDeal(amount,..)` to a note
//! party to this deal (`:493`). The contract calls this "the two halves of one transfer, so the
//! pair conserves: what left here arrived there, and nothing was created on the way";
//! * `_burnShell(amount)` -- `_balance -= amount`, then `reportDealWriteOff(.., amount)` to RootPN
//! (`:509`). A write-off has **no recipient by construction**: RootPN books it as
//! `_writtenOff[SHELL] += amount` (`contracts/dex/RootPN.sol:774`), a subtraction from the
//! outstanding-claims ledger rather than a payment to anybody.

//! Because the outflow side has only those two primitives, a figure that left `_balance` and did
//! not arrive at a note **was burned**. That is a theorem about the contract, not an inference from
//! a residual, and it is what lets a reader account for the last unit of a deal without a second
//! chain read.

//! # What actually was

//! The measured deal looked short by exactly `2 000 000 000` on the buyer's side and left
//! `1 024 600 000` with no named destination. Neither was a loss:

//! * the `2 000 000 000` is the **buyer's own bond**, `BUYER_BOND = 2P`, which the buyer's note
//! pays into the deal through `fundBuyerBond` as a debit **separate from the escrow**. A reader
//! who takes `deposit - refundToBuyer` for the buyer's result is reading a figure that never
//! included the bond. `deposit` is the escrow and nothing else;
//! * the `1 024 600 000` is the burn: one tick of the buyer's bond forfeited by scheme E on `stop()`
//! after an accepted probe, plus the net platform fee (fee accrued minus the seller's rebate).

//! So the defect was never in the money. It was that the deal's declared figures did not add up to
//! anything a reader could check, and both missing terms were on chain the whole time. This module
//! makes the identity explicit and checkable.

//! # The invariant

//! Per transition, against the running `_balance`:

//! | transition | conserves |
//! | --- | --- |
//! | `EscrowFunded` / `SellerBondFunded` / `BuyerBondFunded` | `after == before + amount` |
//! | `CreditedToNote` | `after == before - amount`, and the note is credited the same `amount` |
//! | `WrittenOff` | `after == before - amount`, and no account is credited |
//! | destruction | `balance == 0` -- a deal destroyed holding a figure pays nobody |

//! Folded over a whole deal that is `funded_in == credited_out + written_off + balance`.

//! An outflow larger than the running balance is a **breach**, not a saturating subtraction: it is
//! the shape a double-spend or a mis-decoded amount would take, and it must be reported rather than
//! clamped away.

use std::collections::BTreeMap;

/// One money movement across a deal's `_balance`, in the contract's own primitives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DealMoneyFlow {
    /// `fundFromOrderBook(paid,..)` -- the book forwards the buyer's fee-inclusive escrow.
    /// Reported by the deal as `StreamFunded { deposit }`.
    EscrowFunded { amount: u128 },
    /// `fundDeal(amount,..)` -- the seller's `2P` bond. Reported as `SellerBondFunded { amount }`,
    /// which carries the amount **retained** (any excess is refunded before the event is built).
    SellerBondFunded { amount: u128 },
    /// `fundBuyerBond(amount)` -- the buyer's own `2P` bond, a debit of the buyer's note separate
    /// from the escrow. Reported as `BuyerBondFunded { amount }`, likewise net of any excess.
    BuyerBondFunded { amount: u128 },
    /// `_payShell(to, amount)` -- SHELL leaving the deal for a note party to it.
    CreditedToNote { note: String, amount: u128 },
    /// `_burnShell(amount)` -- SHELL destroyed. Named, never anonymous: a write-off is a stated
    /// destination, not an unexplained remainder.
    WrittenOff { amount: u128 },
}

impl DealMoneyFlow {
    /// The figure this movement carries, whichever direction it goes.
    pub fn amount(&self) -> u128 {
        match self {
            Self::EscrowFunded { amount }
            | Self::SellerBondFunded { amount }
            | Self::BuyerBondFunded { amount }
            | Self::CreditedToNote { amount, .. }
            | Self::WrittenOff { amount } => *amount,
        }
    }

    /// Whether this movement adds to the deal's balance.
    pub fn is_inflow(&self) -> bool {
        matches!(
            self,
            Self::EscrowFunded { .. }
                | Self::SellerBondFunded { .. }
                | Self::BuyerBondFunded { .. }
        )
    }

    /// Stable label used in reports and breach messages.
    pub fn label(&self) -> &'static str {
        match self {
            Self::EscrowFunded { .. } => "escrow_funded",
            Self::SellerBondFunded { .. } => "seller_bond_funded",
            Self::BuyerBondFunded { .. } => "buyer_bond_funded",
            Self::CreditedToNote { .. } => "credited_to_note",
            Self::WrittenOff { .. } => "written_off",
        }
    }
}

/// A conservation failure. Every variant names the figures that disagree, because a breach that
/// only says "does not balance" cannot be acted on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConservationBreach {
    /// An outflow larger than the deal is holding. The contract's `_balance -= amount` would
    /// underflow and revert; a reader that clamped it would silently invent money.
    Overdraft {
        transition: &'static str,
        balance: u128,
        amount: u128,
    },
    /// An inflow that does not fit in `u128` -- a mis-decoded figure, never a real deal.
    Overflow {
        transition: &'static str,
        balance: u128,
        amount: u128,
    },
    /// The deal was destroyed still holding SHELL. `selfdestruct` does not carry a scalar anywhere:
    /// the figure is annihilated, paying nobody, with no failed call and nothing in a log.
    ResidualAtDestruction { balance: u128 },
    /// Funded in does not equal paid out plus burned plus what is still held.
    NotConserved {
        funded_in: u128,
        credited_out: u128,
        written_off: u128,
        balance: u128,
    },
    /// The deal's own terminal event and the notes' own credits disagree about the payout. These
    /// are two independent statements on chain about one settlement; a reader that picked the
    /// prettier one would be inventing a third.
    DeclaredPayoutDisagreesWithCredits { declared: u128, credited: u128 },
    /// The declared payout exceeds everything the deal was ever funded, so no burn can reconcile
    /// it. The deal cannot have paid out more than it took in.
    PayoutExceedsFunding { funded_in: u128, declared: u128 },
}

impl std::fmt::Display for ConservationBreach {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Overdraft {
                transition,
                balance,
                amount,
            } => write!(
                formatter,
                "deal conservation: {transition} moves {amount} out of a balance of {balance}"
            ),
            Self::Overflow {
                transition,
                balance,
                amount,
            } => write!(
                formatter,
                "deal conservation: {transition} adds {amount} to a balance of {balance} and overflows u128"
            ),
            Self::ResidualAtDestruction { balance } => write!(
                formatter,
                "deal conservation: deal destroyed still holding {balance} SHELL, which pays nobody"
            ),
            Self::NotConserved {
                funded_in,
                credited_out,
                written_off,
                balance,
            } => write!(
                formatter,
                "deal conservation: funded in {funded_in} != credited out {credited_out} + written off {written_off} + held {balance}"
            ),
            Self::DeclaredPayoutDisagreesWithCredits { declared, credited } => write!(
                formatter,
                "deal conservation: the deal declared a payout of {declared} but the notes report being credited {credited}"
            ),
            Self::PayoutExceedsFunding {
                funded_in,
                declared,
            } => write!(
                formatter,
                "deal conservation: declared payout {declared} exceeds everything the deal was funded, {funded_in}"
            ),
        }
    }
}

impl std::error::Error for ConservationBreach {}

/// One replayed transition, with the balance either side of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DealLedgerStep {
    pub flow: DealMoneyFlow,
    pub balance_before: u128,
    pub balance_after: u128,
}

/// The whole-deal fold, once every transition has been replayed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DealSettlement {
    pub funded_in: u128,
    pub credited_out: u128,
    pub written_off: u128,
    /// Still held by the deal. Must be zero on a deal that has been destroyed.
    pub balance: u128,
    /// Per-note credit totals, so a payout can be attributed to an address rather than aggregated.
    pub credited_by_note: BTreeMap<String, u128>,
}

impl DealSettlement {
    /// What one note was credited across the whole deal. Attribution by address is the difference
    /// between accounting for a payout and estimating one.
    pub fn credited_to(&self, note: &str) -> u128 {
        self.credited_by_note.get(note).copied().unwrap_or(0)
    }
}

/// A deal's `_balance`, replayed transition by transition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DealLedger {
    balance: u128,
    funded_in: u128,
    credited_out: u128,
    written_off: u128,
    credited_by_note: BTreeMap<String, u128>,
    steps: Vec<DealLedgerStep>,
}

impl DealLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replay every transition in order, stopping at the first breach.
    pub fn replay<I>(flows: I) -> Result<Self, ConservationBreach>
    where
        I: IntoIterator<Item = DealMoneyFlow>,
    {
        let mut ledger = Self::new();
        for flow in flows {
            ledger.apply(flow)?;
        }
        Ok(ledger)
    }

    /// Apply one transition. This is the per-transition invariant: the balance moves by exactly the
    /// figure the transition carries, in the direction the contract's primitive moves it, and an
    /// outflow that the deal cannot cover is a breach rather than a clamp.
    pub fn apply(&mut self, flow: DealMoneyFlow) -> Result<&DealLedgerStep, ConservationBreach> {
        let amount = flow.amount();
        let balance_before = self.balance;
        let balance_after = if flow.is_inflow() {
            let after = balance_before
                .checked_add(amount)
                .ok_or(ConservationBreach::Overflow {
                    transition: flow.label(),
                    balance: balance_before,
                    amount,
                })?;
            self.funded_in =
                self.funded_in
                    .checked_add(amount)
                    .ok_or(ConservationBreach::Overflow {
                        transition: flow.label(),
                        balance: balance_before,
                        amount,
                    })?;
            after
        } else {
            let after =
                balance_before
                    .checked_sub(amount)
                    .ok_or(ConservationBreach::Overdraft {
                        transition: flow.label(),
                        balance: balance_before,
                        amount,
                    })?;
            match &flow {
                DealMoneyFlow::CreditedToNote { note, .. } => {
                    self.credited_out = self.credited_out.saturating_add(amount);
                    let entry = self.credited_by_note.entry(note.clone()).or_insert(0);
                    *entry = entry.saturating_add(amount);
                }
                _ => self.written_off = self.written_off.saturating_add(amount),
            }
            after
        };
        self.balance = balance_after;
        self.steps.push(DealLedgerStep {
            flow,
            balance_before,
            balance_after,
        });
        self.steps.last().ok_or(ConservationBreach::NotConserved {
            funded_in: self.funded_in,
            credited_out: self.credited_out,
            written_off: self.written_off,
            balance: self.balance,
        })
    }

    pub fn balance(&self) -> u128 {
        self.balance
    }

    pub fn funded_in(&self) -> u128 {
        self.funded_in
    }

    pub fn credited_out(&self) -> u128 {
        self.credited_out
    }

    pub fn written_off(&self) -> u128 {
        self.written_off
    }

    pub fn steps(&self) -> &[DealLedgerStep] {
        &self.steps
    }

    /// What one note was credited across the whole deal.
    pub fn credited_to(&self, note: &str) -> u128 {
        self.credited_by_note.get(note).copied().unwrap_or(0)
    }

    /// Fold the replay into the whole-deal identity and check it.
    pub fn settle(&self) -> Result<DealSettlement, ConservationBreach> {
        let out = self
            .credited_out
            .checked_add(self.written_off)
            .and_then(|out| out.checked_add(self.balance))
            .ok_or(ConservationBreach::NotConserved {
                funded_in: self.funded_in,
                credited_out: self.credited_out,
                written_off: self.written_off,
                balance: self.balance,
            })?;
        if out != self.funded_in {
            return Err(ConservationBreach::NotConserved {
                funded_in: self.funded_in,
                credited_out: self.credited_out,
                written_off: self.written_off,
                balance: self.balance,
            });
        }
        Ok(DealSettlement {
            funded_in: self.funded_in,
            credited_out: self.credited_out,
            written_off: self.written_off,
            balance: self.balance,
            credited_by_note: self.credited_by_note.clone(),
        })
    }

    /// The same fold, for a deal the chain has destroyed. Destruction adds one requirement the
    /// running identity does not have: nothing may still be held, because a destroyed deal's
    /// `_balance` is annihilated rather than paid.
    pub fn settle_destroyed(&self) -> Result<DealSettlement, ConservationBreach> {
        let settlement = self.settle()?;
        if settlement.balance != 0 {
            return Err(ConservationBreach::ResidualAtDestruction {
                balance: settlement.balance,
            });
        }
        Ok(settlement)
    }
}

/// What the buyer actually put into one deal.

/// **This is the figure was missing.** The escrow (`StreamFunded.deposit`) is not the buyer's
/// exposure: since generation 4.0.35 an ordinary deal also debits the buyer's note `BUYER_BOND = 2P`
/// through `fundBuyerBond`, a second, separate message. A receipt that prints only `deposit` and
/// `refundToBuyer` invites the reader to compute `deposit - refundToBuyer` and call it the buyer's
/// result -- which understates the buyer's outlay by the whole bond and made a correct settlement
/// look short by exactly `2 000 000 000`.
pub fn buyer_total_debit(deposit: u128, buyer_bond: u128) -> Option<u128> {
    deposit.checked_add(buyer_bond)
}

/// The buyer's result for one deal: what the notes credited back, less everything the buyer put in.
/// Negative is the ordinary case -- it is the price of the service.
pub fn buyer_net_result(credited_to_buyer: u128, total_debit: u128) -> i128 {
    let credited = i128::try_from(credited_to_buyer).unwrap_or(i128::MAX);
    let debited = i128::try_from(total_debit).unwrap_or(i128::MAX);
    credited.saturating_sub(debited)
}

/// SHELL burned by a deal, derived from figures the deal itself declared.

/// The deal's outflow side has exactly two primitives, `_payShell` and `_burnShell`, so whatever was
/// funded in and did not reach a note **was written off**. `declared_payout` is the deal's own
/// terminal split (`toSeller + refundToBuyer`), which is why this is an accounting identity rather
/// than a leftover: the destination is named by the contract's construction, not guessed.

/// Verified against the chain's own write-off messages on mainnet deal
/// `0:a71399a3606cb32292628d37518d7983c430420febd0b57585eabd9ca1a3a83a`: funded in
/// `6 050 000 000`, declared payout `5 015 121 275`, so this returns `1 034 878 725` -- the exact sum
/// of that deal's two `reportDealWriteOff` messages, `1 000 000 000` and `34 878 725`.
pub fn implied_write_off(
    funded_in: u128,
    declared_payout: u128,
) -> Result<u128, ConservationBreach> {
    funded_in
        .checked_sub(declared_payout)
        .ok_or(ConservationBreach::PayoutExceedsFunding {
            funded_in,
            declared: declared_payout,
        })
}

/// Cross-check the two independent chain statements about one settlement: what the deal declared it
/// paid, and what the notes report being credited. They are produced by different accounts, so
/// agreement is evidence and disagreement is a finding.
pub fn check_declared_payout_against_credits(
    declared_payout: u128,
    credited: u128,
) -> Result<(), ConservationBreach> {
    (declared_payout == credited).then_some(()).ok_or(
        ConservationBreach::DeclaredPayoutDisagreesWithCredits {
            declared: declared_payout,
            credited,
        },
    )
}
