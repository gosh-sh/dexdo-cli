use crate::cli::policy;
use anyhow::{anyhow, bail, Result};
use dexdo_core::{ChainBackend, ChainError, DealTerminalSettlement};

pub(crate) async fn apply_seller_dispute_policy(
    chain: &dyn ChainBackend,
    token_contract: &dexdo_core::TokenContract,
    policy: &policy::SellerRuntimePolicy,
    reason: &str,
) -> Result<bool> {
    let Some(state) = chain.deal_state(token_contract).await? else {
        return Ok(false);
    };
    if !state.disputed {
        return Ok(false);
    }
    match policy.dispute_against_me {
        policy::SellerDisputeAgainstMeAction::ReleaseIfClean => {
            let settlement = chain.release_dispute(token_contract).await?;
            let token_contract = dexdo_core::address::display_self_dapp(token_contract);
            println!(
                "policy_action failure_class=dispute_against_me action=release_if_clean \
                 token_contract={token_contract} state=funded/opened/disputed result=release_dispute_submitted \
                 reason={reason} settlement={settlement}"
            );
            Ok(true)
        }
        policy::SellerDisputeAgainstMeAction::Hold => {
            let token_contract = dexdo_core::address::display_self_dapp(token_contract);
            bail!(
                "policy_action failure_class=dispute_against_me action=hold token_contract={token_contract} \
                 state=funded/opened/disputed result=no_release_submitted reason={reason}"
            );
        }
    }
}

#[derive(Debug)]
pub(crate) enum SellerTerminalPolicyOutcome {
    StopServing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AdvanceFailureDisposition {
    BenignTerminal { reason: String },
    Fault { reason: String },
}

pub(crate) fn is_err_not_open(error: &ChainError) -> bool {
    fn valid_code_terminator(suffix: &str) -> bool {
        let mut chars = suffix.chars();
        match chars.next() {
            None => true,
            Some(ch) if ch.is_alphanumeric() || ch == '_' => false,
            Some('.' | ':') => !chars.next().is_some_and(|ch| ch.is_ascii_digit()),
            Some(_) => true,
        }
    }

    fn numeric_fields(message: &str, field: &str, numeric_required: bool) -> Option<Vec<u32>> {
        let mut values = Vec::new();
        for (index, _) in message.match_indices(field) {
            if message[..index]
                .chars()
                .next_back()
                .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            {
                continue;
            }
            let suffix = &message[index + field.len()..];
            let digits = suffix
                .as_bytes()
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if digits == 0 {
                if numeric_required {
                    return None;
                }
                continue;
            }
            if !valid_code_terminator(&suffix[digits..]) {
                return None;
            }
            values.push(suffix[..digits].parse::<u32>().ok()?);
        }
        Some(values)
    }

    fn has_exact_error_name(message: &str, name: &str) -> bool {
        message.match_indices(name).any(|(index, _)| {
            !message[..index]
                .chars()
                .next_back()
                .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                && !message[index + name.len()..]
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        })
    }

    match error {
        ChainError::Chain(msg) | ChainError::Contract(msg) => {
            let Some(mut exit_codes) = numeric_fields(msg, "exit_code=", true) else {
                return false;
            };
            let Some(spaced_exit_codes) = numeric_fields(msg, "exit code ", true) else {
                return false;
            };
            exit_codes.extend(spaced_exit_codes);
            let Some(camel_exit_codes) = numeric_fields(msg, "exitCode=", true) else {
                return false;
            };
            exit_codes.extend(camel_exit_codes);
            let Some(generic_codes) = numeric_fields(msg, "code=", false) else {
                return false;
            };
            let Some(mut action_codes) = numeric_fields(msg, "action_result_code=", true) else {
                return false;
            };
            for alias in ["actionResultCode=", "result_code=", "resultCode="] {
                let Some(codes) = numeric_fields(msg, alias, true) else {
                    return false;
                };
                action_codes.extend(codes);
            }
            if !generic_codes.is_empty() {
                return false;
            }
            if exit_codes.iter().any(|code| *code != 320)
                || action_codes.iter().any(|code| *code != 0)
            {
                return false;
            }
            if !exit_codes.is_empty() {
                return true;
            }
            has_exact_error_name(msg, "airegistry::ERR_NOT_OPEN")
        }
        _ => false,
    }
}

/// Recognise an advance failure that is really the deal's immutable terminal settlement.

/// Every terminal destroys the TokenContract. Its getters disappear, so a late reconciliation can
/// fail even though settlement already completed. Immutable receipts outlive the account and are
/// the only authoritative fact still available.

/// Proving the terminal here only retires the deal; the seller submits nothing further against a
/// contract that no longer exists. Anything short of one exact terminal leaves the existing
/// failure classification untouched.
pub(crate) async fn classify_terminal_settlement(
    chain: &dyn ChainBackend,
    token_contract: &dexdo_core::TokenContract,
) -> Result<Option<String>> {
    let Some(settlement) = chain.terminal_settlement(token_contract).await? else {
        return Ok(None);
    };
    Ok(Some(match settlement {
        DealTerminalSettlement::ProbeBurned {
            burned_probe,
            burned_bond,
            refund_to_buyer,
        } => format!(
            "reason=probe_burned_terminal burnedProbe={burned_probe} burnedBond={burned_bond} \
             refundToBuyer={refund_to_buyer}"
        ),
        DealTerminalSettlement::StreamStopped {
            to_seller,
            refund_to_buyer,
        } => format!(
            "reason=stream_stopped_terminal toSeller={to_seller} refundToBuyer={refund_to_buyer}"
        ),
        DealTerminalSettlement::DisputeResolved {
            to_seller,
            refund_to_buyer,
            released,
        } => format!(
            "reason=dispute_resolved_terminal toSeller={to_seller} \
             refundToBuyer={refund_to_buyer} released={released}"
        ),
    }))
}

pub(crate) async fn classify_by_fact_advance_failure(
    chain: &dyn ChainBackend,
    token_contract: &dexdo_core::TokenContract,
    error: &ChainError,
) -> Result<AdvanceFailureDisposition> {
    if !is_err_not_open(error) {
        return Ok(AdvanceFailureDisposition::Fault {
            reason: "reason=not_err_not_open".to_string(),
        });
    }

    let state = chain.deal_state(token_contract).await?.ok_or_else(|| {
        anyhow!("reason=state_unavailable cannot prove ERR_NOT_OPEN is terminal/no-money")
    })?;
    // ERR_NOT_OPEN is only benign on a deal that never ran. Promoted consumption or a settled close (drained
    // deposit) both mean money already moved, so treat either as a fault rather than a harmless terminal.
    if state.opened || state.disputed || state.tokens_final > 0 || state.is_stopped() {
        return Ok(AdvanceFailureDisposition::Fault {
            reason: unsafe_lifecycle_reason(&state),
        });
    }

    let snapshot = chain.snapshot(token_contract).await.ok_or_else(|| {
        anyhow!("reason=snapshot_unavailable cannot prove ERR_NOT_OPEN has no locked/owed money")
    })?;
    if snapshot.buyer_locked != 0
        || snapshot.seller_locked != 0
        || snapshot.buyer_lead != 0
        || snapshot.seller_received != 0
        || snapshot.burned != 0
    {
        return Ok(AdvanceFailureDisposition::Fault {
            reason: money_or_locks_reason(&snapshot),
        });
    }

    Ok(AdvanceFailureDisposition::BenignTerminal {
        reason: unopened_no_money_reason(&state, &snapshot),
    })
}

/// Why an ERR_NOT_OPEN deal is a fault: it ran. Money in it is stated in SHELL, counts stay counts.
pub(crate) fn unsafe_lifecycle_reason(state: &dexdo_core::DealChainState) -> String {
    format!(
        "reason=unsafe_lifecycle funded={} opened={} disputed={} tokens_final={} deposit={}",
        state.funded,
        state.opened,
        state.disputed,
        state.tokens_final,
        dexdo_core::shell_amount(state.deposit)
    )
}

/// Why an ERR_NOT_OPEN deal is a fault: money or locks are still in it.
pub(crate) fn money_or_locks_reason(snapshot: &dexdo_core::StreamSnapshot) -> String {
    format!(
        "reason=money_or_locks_present buyer_locked={} buyer_lead={} seller_locked={} \
         finalized_owed={} burned={}",
        dexdo_core::shell_amount(snapshot.buyer_locked),
        dexdo_core::shell_amount(snapshot.buyer_lead),
        dexdo_core::shell_amount(snapshot.seller_locked),
        dexdo_core::shell_amount(snapshot.seller_received),
        dexdo_core::shell_amount(snapshot.burned)
    )
}

/// Why an ERR_NOT_OPEN deal is harmless: it never opened and holds nothing.
pub(crate) fn unopened_no_money_reason(
    state: &dexdo_core::DealChainState,
    snapshot: &dexdo_core::StreamSnapshot,
) -> String {
    format!(
        "reason=err_not_open_unopened_no_money funded={} opened={} disputed={} tokens_final={} \
         buyer_locked={} buyer_lead={} seller_locked={} finalized_owed={} burned={}",
        state.funded,
        state.opened,
        state.disputed,
        state.tokens_final,
        dexdo_core::shell_amount(snapshot.buyer_locked),
        dexdo_core::shell_amount(snapshot.buyer_lead),
        dexdo_core::shell_amount(snapshot.seller_locked),
        dexdo_core::shell_amount(snapshot.seller_received),
        dexdo_core::shell_amount(snapshot.burned)
    )
}

/// The two consumption figures a terminal must state separately, and the tail between them.

/// `claimed_tokens` is the cumulative the seller CLAIMED. `finalized_tokens` is the part the contract
/// PROMOTED, and money is computed from that one alone (`TokenContract._payFinalAndClose` reads
/// `_tokensFinal`). The two are equal only when every claim served its `CLAIM_PROMOTE_WINDOW` before the
/// deal ended. A buyer STOP inside that window closes on the promoted figure and refunds the rest
/// (`TokenContract.stop` -> `_closeClean`), so the difference is delivered, claimed, verified work that
/// was not paid -- the seller's only chance to recognise it is this line.

/// Both figures are in TOKENS. The key this replaces printed the CLAIMED figure, in tokens, under the
/// name `finalized_ticks` -- wrong in the quantity AND wrong in the unit, so a deal that paid one tick
/// against three million claimed tokens reported `finalized_ticks=3000000` and read as success.
fn terminal_consumption_fields(claimed_tokens: u128, promoted_tokens: u128) -> String {
    let unpromoted = claimed_tokens.saturating_sub(promoted_tokens);
    let mut fields = format!("finalized_tokens={promoted_tokens} claimed_tokens={claimed_tokens}");
    if unpromoted > 0 {
        // Named on the same line rather than in a separate print: three of the four branches below
        // return this text as an error, where a second `println!` would not travel with it.
        fields.push_str(&format!(
            " unpromoted_tokens={unpromoted} \
             unpromoted_reason=claims_that_did_not_serve_claim_promote_window_are_not_paid_at_close"
        ));
    }
    fields
}

pub(crate) fn apply_seller_terminal_policy(
    token_contract: &dexdo_core::TokenContract,
    policy: &policy::SellerRuntimePolicy,
    claimed_tokens: u128,
    state: dexdo_core::DealChainState,
) -> Result<SellerTerminalPolicyOutcome> {
    let token_contract = dexdo_core::address::display_self_dapp(token_contract);
    // Promoted consumption comes from the authoritative terminal read, never from the driver's own
    // cursor: the cursor tracks what this process CLAIMED, and what was paid is a different fact.
    let consumption = terminal_consumption_fields(claimed_tokens, state.tokens_final);
    // The driver can return zero when it observes a buyer STOP before its local claim cursor advances. The
    // contract's accepted-probe latch is the authority for whether service crossed the paid-probe boundary;
    // never turn an after-probe close into a buyer no-show from that scalar alone.
    if !state.probe_accepted {
        match policy.buyer_no_show {
            policy::SellerBuyerNoShowAction::CleanupAndRepublish => {
                bail!(
                    "policy_action failure_class=buyer_no_show action=cleanup_and_republish \
                     token_contract={token_contract} state=funded/opened result=policy_action_unsupported; \
                     seller runtime has no buyer-side cleanup_unopened signer or fresh TC/nonce republish factory"
                );
            }
            policy::SellerBuyerNoShowAction::CleanupAndRetire => {
                bail!(
                    "policy_action failure_class=buyer_no_show action=cleanup_and_retire \
                     token_contract={token_contract} state=funded/opened result=policy_action_unsupported; \
                     cleanup_unopened is buyer-side and was not submitted by seller"
                );
            }
            policy::SellerBuyerNoShowAction::RetireGateway => {
                println!(
                    "policy_action failure_class=buyer_no_show action=retire_gateway \
                     token_contract={token_contract} state=closed result=retiring_gateway {consumption}; \
                     no cleanup_unopened submitted by seller"
                );
                return Ok(SellerTerminalPolicyOutcome::StopServing);
            }
        }
    }
    match policy.after_deal_done {
        policy::SellerAfterDealDoneAction::Retire => {
            println!(
                "policy_action failure_class=after_deal_done action=retire token_contract={token_contract} \
                 state=closed result=retiring_gateway {consumption}"
            );
            Ok(SellerTerminalPolicyOutcome::StopServing)
        }
        policy::SellerAfterDealDoneAction::Republish => {
            bail!(
                "policy_action failure_class=after_deal_done action=republish token_contract={token_contract} \
                 state=closed result=policy_action_unsupported {consumption}; \
                 current seller runtime cannot safely republish without a fresh per-deal TC/nonce"
            );
        }
        policy::SellerAfterDealDoneAction::RepublishWithBackoff => {
            bail!(
                "policy_action failure_class=after_deal_done action=republish_with_backoff \
                 token_contract={token_contract} state=closed result=policy_action_unsupported \
                 {consumption}; current seller runtime cannot safely republish without a fresh \
                 per-deal TC/nonce"
            );
        }
    }
}

#[cfg(test)]
mod terminal_consumption_tests {
    use super::*;

    /// The exact shape: three ticks delivered and claimed, only the `acceptProbe` seed promoted.
    fn state_979() -> dexdo_core::DealChainState {
        dexdo_core::DealChainState {
            funded: true,
            opened: false,
            probe_accepted: true,
            disputed: false,
            deposit: 0,
            finalized_owed: 0,
            // What the buyer's STOP paid on: still the probe seed, because neither pending claim
            // served CLAIM_PROMOTE_WINDOW before the deal closed.
            tokens_final: dexdo_core::TICK_SIZE,
            tokens_pending: 3 * dexdo_core::TICK_SIZE,
            probe_tick: 0,
            funded_time: Some(1),
            probe_time: 1,
            last_claim_time: 3,
            dispute_time: 0,
        }
    }

    fn policy_with(
        after_deal_done: policy::SellerAfterDealDoneAction,
    ) -> policy::SellerRuntimePolicy {
        policy::SellerRuntimePolicy {
            after_deal_done,
            buyer_no_show: policy::SellerBuyerNoShowAction::RetireGateway,
            dispute_against_me: policy::SellerDisputeAgainstMeAction::ReleaseIfClean,
            max_open_deals: 1,
        }
    }

    /// regression, value half: the terminal must state BOTH figures, each under its own name.

    /// Asserting only the promoted value would pass against the defect, because the defect printed a
    /// number that was numerically fine for the quantity it was NOT labelled as. What fails on the old
    /// code is the pair: `finalized_*` must carry the promoted figure and nothing else must be called
    /// finalized, while the claimed figure must appear under a name that says claimed.
    #[test]
    fn a_terminal_states_promoted_and_claimed_separately_and_in_tokens() {
        let state = state_979();
        let fields = terminal_consumption_fields(state.tokens_pending, state.tokens_final);

        assert!(
            fields.contains("finalized_tokens=1000000"),
            "the promoted figure the chain actually paid on must be the one called finalized: {fields}"
        );
        assert!(
            fields.contains("claimed_tokens=3000000"),
            "the claimed cumulative must appear under a name that says claimed: {fields}"
        );
        assert!(
            !fields.contains("finalized_tokens=3000000"),
            "the claimed figure must never be printed as the finalized one -- that is the defect: {fields}"
        );
        assert!(
            !fields.contains("finalized_ticks"),
            "a token count must not be labelled with the tick unit: {fields}"
        );
        assert!(
            fields.contains("unpromoted_tokens=2000000")
                && fields
                    .contains("unpromoted_reason=claims_that_did_not_serve_claim_promote_window"),
            "the gap must be named, with the reason it was not paid: {fields}"
        );
    }

    /// The control: when every claim was promoted the two figures agree and no gap is reported, so the
    /// warning above cannot be a line that is always printed.
    #[test]
    fn a_fully_promoted_terminal_reports_no_unpromoted_tail() {
        let fields =
            terminal_consumption_fields(3 * dexdo_core::TICK_SIZE, 3 * dexdo_core::TICK_SIZE);

        assert!(fields.contains("finalized_tokens=3000000"), "{fields}");
        assert!(fields.contains("claimed_tokens=3000000"), "{fields}");
        assert!(
            !fields.contains("unpromoted"),
            "nothing is unpaid when the whole claim pipeline was promoted: {fields}"
        );
    }

    /// regression, dispatch half: the fields must actually REACH the operator, not merely be
    /// computable. `Republish` returns the terminal text as an error, so this observes the real output
    /// of the real entry point rather than the helper it happens to call.
    #[test]
    fn the_terminal_the_operator_receives_carries_both_figures() {
        let error = apply_seller_terminal_policy(
            &"0:tc979".to_string(),
            &policy_with(policy::SellerAfterDealDoneAction::Republish),
            state_979().tokens_pending,
            state_979(),
        )
        .expect_err("republish is fail-closed unsupported and returns the terminal text")
        .to_string();

        assert!(error.contains("finalized_tokens=1000000"), "{error}");
        assert!(error.contains("claimed_tokens=3000000"), "{error}");
        assert!(error.contains("unpromoted_tokens=2000000"), "{error}");
        assert!(
            !error.contains("finalized_ticks"),
            "the mislabelled key must be gone from the shipped line: {error}"
        );
    }
}
