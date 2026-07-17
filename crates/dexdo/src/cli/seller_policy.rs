use crate::cli::policy;
use anyhow::{anyhow, bail, Result};
use dexdo_core::{ChainBackend, ChainError};

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
            println!(
                "policy_action failure_class=dispute_against_me action=release_if_clean \
                 token_contract={token_contract} state=funded/opened/disputed result=release_dispute_submitted \
                 reason={reason} settlement={settlement:?}"
            );
            Ok(true)
        }
        policy::SellerDisputeAgainstMeAction::Hold => {
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
    if state.opened || state.probe_accepted || state.disputed {
        return Ok(AdvanceFailureDisposition::Fault {
            reason: format!(
                "reason=unsafe_lifecycle funded={} opened={} probe_accepted={} disputed={}",
                state.funded, state.opened, state.probe_accepted, state.disputed
            ),
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
            reason: format!(
                "reason=money_or_locks_present buyer_locked={} buyer_lead={} seller_locked={} \
                 finalized_owed={} burned={}",
                snapshot.buyer_locked,
                snapshot.buyer_lead,
                snapshot.seller_locked,
                snapshot.seller_received,
                snapshot.burned
            ),
        });
    }

    Ok(AdvanceFailureDisposition::BenignTerminal {
        reason: format!(
            "reason=err_not_open_unopened_no_money funded={} opened={} probe_accepted={} disputed={} \
             buyer_locked={} buyer_lead={} seller_locked={} finalized_owed={} burned={}",
            state.funded,
            state.opened,
            state.probe_accepted,
            state.disputed,
            snapshot.buyer_locked,
            snapshot.buyer_lead,
            snapshot.seller_locked,
            snapshot.seller_received,
            snapshot.burned
        ),
    })
}

pub(crate) fn apply_seller_terminal_policy(
    token_contract: &dexdo_core::TokenContract,
    policy: &policy::SellerRuntimePolicy,
    finalized: u128,
) -> Result<SellerTerminalPolicyOutcome> {
    if finalized == 0 {
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
                     token_contract={token_contract} state=closed result=retiring_gateway finalized_ticks=0; \
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
                 state=closed result=retiring_gateway finalized_ticks={finalized}"
            );
            Ok(SellerTerminalPolicyOutcome::StopServing)
        }
        policy::SellerAfterDealDoneAction::Republish => {
            bail!(
                "policy_action failure_class=after_deal_done action=republish token_contract={token_contract} \
                 state=closed result=policy_action_unsupported finalized_ticks={finalized}; \
                 current seller runtime cannot safely republish without a fresh per-deal TC/nonce"
            );
        }
        policy::SellerAfterDealDoneAction::RepublishWithBackoff => {
            bail!(
                "policy_action failure_class=after_deal_done action=republish_with_backoff \
                 token_contract={token_contract} state=closed result=policy_action_unsupported \
                 finalized_ticks={finalized}; current seller runtime cannot safely republish without a fresh \
                 per-deal TC/nonce"
            );
        }
    }
}
