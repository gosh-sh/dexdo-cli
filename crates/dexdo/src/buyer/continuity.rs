//! Buyer continuity planner for the long-running local API mode.
//! The planner is deliberately side-effect free: callers read chain facts, pass them in, and execute the
//! returned action through existing primitives (`place_buy`, handover resolution, `streamCleanup`,
//! `streamReclaim`, `SessionSettle`). This keeps duplicate monitor ticks and process restarts idempotent.

use dexdo_core::{TokenContract, MATCH_OPEN_TIMEOUT_SECS};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuyerRunMode {
    OneShot,
    Service,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuyerLifecycle {
    ExitAfterCurrentDeal,
    KeepServingAndRenew,
}

pub fn lifecycle_for_mode(mode: BuyerRunMode) -> BuyerLifecycle {
    match mode {
        BuyerRunMode::OneShot => BuyerLifecycle::ExitAfterCurrentDeal,
        BuyerRunMode::Service => BuyerLifecycle::KeepServingAndRenew,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DealOwner {
    ThisBuyer,
    WrongBuyer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DealPhase {
    FundedNeverOpened { funded_age_secs: u64 },
    Opened { idle_secs: u64 },
    HandoverReady,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DealFacts {
    pub token_contract: TokenContract,
    pub owner: DealOwner,
    pub phase: DealPhase,
    pub remaining_tokens: Option<u64>,
}

impl DealFacts {
    pub fn open(token_contract: impl Into<TokenContract>, remaining_tokens: u64) -> Self {
        Self {
            token_contract: token_contract.into(),
            owner: DealOwner::ThisBuyer,
            phase: DealPhase::Opened { idle_secs: 0 },
            remaining_tokens: Some(remaining_tokens),
        }
    }

    pub fn handover_ready(token_contract: impl Into<TokenContract>, remaining_tokens: u64) -> Self {
        Self {
            token_contract: token_contract.into(),
            owner: DealOwner::ThisBuyer,
            phase: DealPhase::HandoverReady,
            remaining_tokens: Some(remaining_tokens),
        }
    }

    pub fn funded_never_opened(token_contract: impl Into<TokenContract>, age: u64) -> Self {
        Self {
            token_contract: token_contract.into(),
            owner: DealOwner::ThisBuyer,
            phase: DealPhase::FundedNeverOpened {
                funded_age_secs: age,
            },
            remaining_tokens: None,
        }
    }

    pub fn opened_idle(token_contract: impl Into<TokenContract>, idle_secs: u64) -> Self {
        Self {
            token_contract: token_contract.into(),
            owner: DealOwner::ThisBuyer,
            phase: DealPhase::Opened { idle_secs },
            remaining_tokens: None,
        }
    }

    pub fn closed(token_contract: impl Into<TokenContract>) -> Self {
        Self {
            token_contract: token_contract.into(),
            owner: DealOwner::ThisBuyer,
            phase: DealPhase::Closed,
            remaining_tokens: None,
        }
    }

    pub fn wrong_owner(mut self) -> Self {
        self.owner = DealOwner::WrongBuyer;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuyerAction {
    ServeCurrent {
        token_contract: TokenContract,
    },
    PrepareNextDeal {
        current: TokenContract,
    },
    SwitchToNextDeal {
        previous: TokenContract,
        next: TokenContract,
    },
    CleanupUnopened {
        token_contract: TokenContract,
    },
    ReclaimOpened {
        token_contract: TokenContract,
    },
    PlaceNextDeal {
        reason: &'static str,
    },
    IgnoreStale {
        token_contract: TokenContract,
    },
    FailClosed {
        token_contract: TokenContract,
        reason: &'static str,
    },
    Noop {
        reason: &'static str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumerDemand {
    Idle,
    ActiveOrRecent,
}

impl ConsumerDemand {
    fn is_active_or_recent(self) -> bool {
        matches!(self, Self::ActiveOrRecent)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ContinuityMode {
    #[default]
    Proactive,
    OnDemand,
}

impl ContinuityMode {
    pub fn planner_demand(self, observed: ConsumerDemand) -> ConsumerDemand {
        match self {
            Self::Proactive => ConsumerDemand::ActiveOrRecent,
            Self::OnDemand => observed,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ContinuityConfig {
    pub renewal_threshold_tokens: u64,
    pub match_open_timeout_secs: u64,
    pub stream_timeout_secs: u64,
}

impl Default for ContinuityConfig {
    fn default() -> Self {
        Self {
            renewal_threshold_tokens: 64,
            match_open_timeout_secs: MATCH_OPEN_TIMEOUT_SECS,
            stream_timeout_secs: 600,
        }
    }
}

#[derive(Default)]
pub struct BuyerContinuity {
    stale: HashSet<TokenContract>,
    pending_after: HashMap<TokenContract, TokenContract>,
}

impl BuyerContinuity {
    pub fn stale_token_contracts(&self) -> &HashSet<TokenContract> {
        &self.stale
    }

    pub fn note_pending_next(
        &mut self,
        current: impl Into<TokenContract>,
        next: impl Into<TokenContract>,
    ) {
        self.pending_after.insert(current.into(), next.into());
    }

    pub fn clear_pending_next(&mut self, current: &TokenContract) {
        self.pending_after.remove(current);
    }

    pub fn tick(
        &mut self,
        current: Option<DealFacts>,
        ready_next: Option<DealFacts>,
        cfg: ContinuityConfig,
    ) -> BuyerAction {
        self.tick_with_demand(current, ready_next, cfg, ConsumerDemand::ActiveOrRecent)
    }

    pub fn tick_with_demand(
        &mut self,
        current: Option<DealFacts>,
        ready_next: Option<DealFacts>,
        cfg: ContinuityConfig,
        consumer_demand: ConsumerDemand,
    ) -> BuyerAction {
        let Some(current) = current else {
            return if consumer_demand.is_active_or_recent() {
                BuyerAction::PlaceNextDeal {
                    reason: "no-current-deal",
                }
            } else {
                BuyerAction::Noop {
                    reason: "no-current-no-consumer-demand",
                }
            };
        };
        if current.owner != DealOwner::ThisBuyer {
            return BuyerAction::FailClosed {
                token_contract: current.token_contract,
                reason: "wrong-owner",
            };
        }
        if let Some(next) = ready_next {
            if next.owner != DealOwner::ThisBuyer {
                return BuyerAction::FailClosed {
                    token_contract: next.token_contract,
                    reason: "next-wrong-owner",
                };
            }
            if matches!(
                next.phase,
                DealPhase::HandoverReady | DealPhase::Opened { .. }
            ) {
                self.pending_after.remove(&current.token_contract);
                return BuyerAction::SwitchToNextDeal {
                    previous: current.token_contract,
                    next: next.token_contract,
                };
            }
        }
        match current.phase {
            DealPhase::Closed => {
                if !consumer_demand.is_active_or_recent() {
                    BuyerAction::Noop {
                        reason: "closed-current-no-consumer-demand",
                    }
                } else if self.pending_after.contains_key(&current.token_contract) {
                    BuyerAction::Noop {
                        reason: "next-deal-already-pending",
                    }
                } else {
                    self.pending_after
                        .insert(current.token_contract.clone(), String::new());
                    BuyerAction::PlaceNextDeal {
                        reason: "closed-current",
                    }
                }
            }
            _ if self.stale.contains(&current.token_contract) => BuyerAction::IgnoreStale {
                token_contract: current.token_contract,
            },
            DealPhase::FundedNeverOpened { funded_age_secs }
                if funded_age_secs >= cfg.match_open_timeout_secs =>
            {
                self.stale.insert(current.token_contract.clone());
                BuyerAction::CleanupUnopened {
                    token_contract: current.token_contract,
                }
            }
            DealPhase::FundedNeverOpened { .. } => BuyerAction::Noop {
                reason: "waiting-for-handover",
            },
            DealPhase::Opened { idle_secs } if idle_secs >= cfg.stream_timeout_secs => {
                self.stale.insert(current.token_contract.clone());
                BuyerAction::ReclaimOpened {
                    token_contract: current.token_contract,
                }
            }
            DealPhase::Opened { .. } | DealPhase::HandoverReady => {
                if current
                    .remaining_tokens
                    .is_some_and(|r| r <= cfg.renewal_threshold_tokens)
                {
                    if !consumer_demand.is_active_or_recent() {
                        BuyerAction::Noop {
                            reason: "low-remaining-no-consumer-demand",
                        }
                    } else if self.pending_after.contains_key(&current.token_contract) {
                        BuyerAction::Noop {
                            reason: "next-deal-already-pending",
                        }
                    } else {
                        self.pending_after
                            .insert(current.token_contract.clone(), String::new());
                        BuyerAction::PrepareNextDeal {
                            current: current.token_contract,
                        }
                    }
                } else {
                    BuyerAction::ServeCurrent {
                        token_contract: current.token_contract,
                    }
                }
            }
        }
    }

    pub fn tick_with_mode(
        &mut self,
        current: Option<DealFacts>,
        ready_next: Option<DealFacts>,
        cfg: ContinuityConfig,
        mode: ContinuityMode,
        observed_demand: ConsumerDemand,
    ) -> BuyerAction {
        self.tick_with_demand(
            current,
            ready_next,
            cfg,
            mode.planner_demand(observed_demand),
        )
    }

    pub fn restart_from_handle(&mut self, facts: DealFacts, cfg: ContinuityConfig) -> BuyerAction {
        self.tick(Some(facts), None, cfg)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SellerDealOutcome {
    NoBuyerYet,
    Completed,
    CleanedUnopened,
    ReclaimedOpened,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SellerAction {
    KeepResting,
    PublishFreshOffer { retired: TokenContract },
}

pub fn seller_market_action(
    current_offer: impl Into<TokenContract>,
    outcome: SellerDealOutcome,
) -> SellerAction {
    let current_offer = current_offer.into();
    match outcome {
        SellerDealOutcome::NoBuyerYet => SellerAction::KeepResting,
        SellerDealOutcome::Completed
        | SellerDealOutcome::CleanedUnopened
        | SellerDealOutcome::ReclaimedOpened => SellerAction::PublishFreshOffer {
            retired: current_offer,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ContinuityConfig {
        ContinuityConfig {
            renewal_threshold_tokens: 10,
            match_open_timeout_secs: 600,
            stream_timeout_secs: 600,
        }
    }

    #[test]
    fn service_mode_stays_alive_but_oneshot_exits() {
        assert_eq!(
            lifecycle_for_mode(BuyerRunMode::OneShot),
            BuyerLifecycle::ExitAfterCurrentDeal
        );
        assert_eq!(
            lifecycle_for_mode(BuyerRunMode::Service),
            BuyerLifecycle::KeepServingAndRenew
        );
    }

    #[test]
    fn preemptive_renewal_prepares_then_switches_to_ready_handover() {
        let mut c = BuyerContinuity::default();
        assert_eq!(
            c.tick(Some(DealFacts::open("tc-a", 10)), None, cfg()),
            BuyerAction::PrepareNextDeal {
                current: "tc-a".to_string()
            }
        );
        assert_eq!(
            c.tick(
                Some(DealFacts::open("tc-a", 9)),
                Some(DealFacts::handover_ready("tc-b", 100)),
                cfg()
            ),
            BuyerAction::SwitchToNextDeal {
                previous: "tc-a".to_string(),
                next: "tc-b".to_string()
            }
        );
    }

    #[test]
    fn funded_never_opened_cleans_once_then_marks_stale() {
        let mut c = BuyerContinuity::default();
        assert_eq!(
            c.tick(
                Some(DealFacts::funded_never_opened("tc-stale", 600)),
                None,
                cfg()
            ),
            BuyerAction::CleanupUnopened {
                token_contract: "tc-stale".to_string()
            }
        );
        assert!(c.stale_token_contracts().contains("tc-stale"));
        assert_eq!(
            c.tick(
                Some(DealFacts::funded_never_opened("tc-stale", 601)),
                None,
                cfg()
            ),
            BuyerAction::IgnoreStale {
                token_contract: "tc-stale".to_string()
            }
        );
    }

    #[test]
    fn opened_idle_reclaims_and_does_not_reuse_failed_stream() {
        let mut c = BuyerContinuity::default();
        assert_eq!(
            c.tick(Some(DealFacts::opened_idle("tc-idle", 600)), None, cfg()),
            BuyerAction::ReclaimOpened {
                token_contract: "tc-idle".to_string()
            }
        );
        assert_eq!(
            c.tick(Some(DealFacts::opened_idle("tc-idle", 700)), None, cfg()),
            BuyerAction::IgnoreStale {
                token_contract: "tc-idle".to_string()
            }
        );
    }

    #[test]
    fn proactive_mode_keeps_warm_after_no_current_closed_or_low_remaining() {
        let mut c = BuyerContinuity::default();
        assert_eq!(
            c.tick_with_mode(
                None,
                None,
                cfg(),
                ContinuityMode::Proactive,
                ConsumerDemand::Idle
            ),
            BuyerAction::PlaceNextDeal {
                reason: "no-current-deal"
            }
        );

        let mut c = BuyerContinuity::default();
        assert_eq!(
            c.tick_with_mode(
                Some(DealFacts::closed("tc-closed")),
                None,
                cfg(),
                ContinuityMode::Proactive,
                ConsumerDemand::Idle
            ),
            BuyerAction::PlaceNextDeal {
                reason: "closed-current"
            }
        );

        let mut c = BuyerContinuity::default();
        assert_eq!(
            c.tick_with_mode(
                Some(DealFacts::open("tc-low", 10)),
                None,
                cfg(),
                ContinuityMode::Proactive,
                ConsumerDemand::Idle
            ),
            BuyerAction::PrepareNextDeal {
                current: "tc-low".to_string()
            }
        );
    }

    #[test]
    fn on_demand_idle_closed_current_does_not_rebuy_or_leave_pending() {
        let mut c = BuyerContinuity::default();
        assert_eq!(
            c.tick_with_mode(
                Some(DealFacts::opened_idle("tc-idle", 600)),
                None,
                cfg(),
                ContinuityMode::OnDemand,
                ConsumerDemand::Idle
            ),
            BuyerAction::ReclaimOpened {
                token_contract: "tc-idle".to_string()
            }
        );
        assert_eq!(
            c.tick_with_mode(
                Some(DealFacts::closed("tc-idle")),
                None,
                cfg(),
                ContinuityMode::OnDemand,
                ConsumerDemand::Idle
            ),
            BuyerAction::Noop {
                reason: "closed-current-no-consumer-demand"
            }
        );
        assert_eq!(
            c.tick_with_mode(
                Some(DealFacts::closed("tc-idle")),
                None,
                cfg(),
                ContinuityMode::OnDemand,
                ConsumerDemand::ActiveOrRecent
            ),
            BuyerAction::PlaceNextDeal {
                reason: "closed-current"
            },
            "idle no-op must not create a pending renewal"
        );
    }

    #[test]
    fn on_demand_active_or_recent_closed_current_rebuys_once() {
        let mut c = BuyerContinuity::default();
        assert_eq!(
            c.tick_with_mode(
                Some(DealFacts::closed("tc-closed")),
                None,
                cfg(),
                ContinuityMode::OnDemand,
                ConsumerDemand::ActiveOrRecent
            ),
            BuyerAction::PlaceNextDeal {
                reason: "closed-current"
            }
        );
        assert_eq!(
            c.tick_with_mode(
                Some(DealFacts::closed("tc-closed")),
                None,
                cfg(),
                ContinuityMode::OnDemand,
                ConsumerDemand::ActiveOrRecent
            ),
            BuyerAction::Noop {
                reason: "next-deal-already-pending"
            }
        );
    }

    #[test]
    fn on_demand_idle_low_remaining_does_not_prepare_or_leave_pending() {
        let mut c = BuyerContinuity::default();
        assert_eq!(
            c.tick_with_mode(
                Some(DealFacts::open("tc-low-open", 10)),
                None,
                cfg(),
                ContinuityMode::OnDemand,
                ConsumerDemand::Idle
            ),
            BuyerAction::Noop {
                reason: "low-remaining-no-consumer-demand"
            }
        );
        assert_eq!(
            c.tick_with_mode(
                Some(DealFacts::handover_ready("tc-low-handover", 10)),
                None,
                cfg(),
                ContinuityMode::OnDemand,
                ConsumerDemand::Idle
            ),
            BuyerAction::Noop {
                reason: "low-remaining-no-consumer-demand"
            }
        );
        assert_eq!(
            c.tick_with_mode(
                Some(DealFacts::open("tc-low-open", 10)),
                None,
                cfg(),
                ContinuityMode::OnDemand,
                ConsumerDemand::ActiveOrRecent
            ),
            BuyerAction::PrepareNextDeal {
                current: "tc-low-open".to_string()
            },
            "idle low-remaining must not leave a pending renewal behind"
        );
    }

    #[test]
    fn on_demand_active_or_recent_low_remaining_prepares_once() {
        let mut c = BuyerContinuity::default();
        assert_eq!(
            c.tick_with_mode(
                Some(DealFacts::open("tc-demand-low", 10)),
                None,
                cfg(),
                ContinuityMode::OnDemand,
                ConsumerDemand::ActiveOrRecent
            ),
            BuyerAction::PrepareNextDeal {
                current: "tc-demand-low".to_string()
            }
        );
        assert_eq!(
            c.tick_with_mode(
                Some(DealFacts::open("tc-demand-low", 9)),
                None,
                cfg(),
                ContinuityMode::OnDemand,
                ConsumerDemand::ActiveOrRecent
            ),
            BuyerAction::Noop {
                reason: "next-deal-already-pending"
            }
        );
    }

    #[test]
    fn chain_fact_recovery_actions_ignore_continuity_mode_and_demand() {
        for (mode, demand) in [
            (ContinuityMode::Proactive, ConsumerDemand::Idle),
            (ContinuityMode::Proactive, ConsumerDemand::ActiveOrRecent),
            (ContinuityMode::OnDemand, ConsumerDemand::Idle),
            (ContinuityMode::OnDemand, ConsumerDemand::ActiveOrRecent),
        ] {
            let mut c = BuyerContinuity::default();
            assert_eq!(
                c.tick_with_mode(
                    Some(DealFacts::funded_never_opened("tc-clean", 600)),
                    None,
                    cfg(),
                    mode,
                    demand
                ),
                BuyerAction::CleanupUnopened {
                    token_contract: "tc-clean".to_string()
                }
            );

            let mut c = BuyerContinuity::default();
            assert_eq!(
                c.tick_with_mode(
                    Some(DealFacts::opened_idle("tc-reclaim", 600)),
                    None,
                    cfg(),
                    mode,
                    demand
                ),
                BuyerAction::ReclaimOpened {
                    token_contract: "tc-reclaim".to_string()
                }
            );
        }
    }

    #[test]
    fn restart_resumes_from_chain_facts_and_fails_closed_on_wrong_owner() {
        let mut c = BuyerContinuity::default();
        assert_eq!(
            c.restart_from_handle(DealFacts::handover_ready("tc-open", 100), cfg()),
            BuyerAction::ServeCurrent {
                token_contract: "tc-open".to_string()
            }
        );
        assert_eq!(
            c.restart_from_handle(DealFacts::funded_never_opened("tc-no-open", 700), cfg()),
            BuyerAction::CleanupUnopened {
                token_contract: "tc-no-open".to_string()
            }
        );
        assert_eq!(
            c.restart_from_handle(DealFacts::closed("tc-closed"), cfg()),
            BuyerAction::PlaceNextDeal {
                reason: "closed-current"
            }
        );
        assert!(matches!(
            c.restart_from_handle(DealFacts::open("tc-wrong", 100).wrong_owner(), cfg()),
            BuyerAction::FailClosed {
                token_contract,
                reason: "wrong-owner"
            } if token_contract == "tc-wrong"
        ));
    }

    #[test]
    fn duplicated_monitor_tick_cannot_duplicate_place_buy() {
        let mut c = BuyerContinuity::default();
        assert!(matches!(
            c.tick(Some(DealFacts::open("tc-a", 9)), None, cfg()),
            BuyerAction::PrepareNextDeal { .. }
        ));
        assert_eq!(
            c.tick(Some(DealFacts::open("tc-a", 8)), None, cfg()),
            BuyerAction::Noop {
                reason: "next-deal-already-pending"
            }
        );
    }

    #[test]
    fn seller_keeps_idle_offer_and_republishes_after_terminal_deals() {
        assert_eq!(
            seller_market_action("tc-resting", SellerDealOutcome::NoBuyerYet),
            SellerAction::KeepResting
        );
        for outcome in [
            SellerDealOutcome::Completed,
            SellerDealOutcome::CleanedUnopened,
            SellerDealOutcome::ReclaimedOpened,
        ] {
            assert_eq!(
                seller_market_action("tc-old", outcome),
                SellerAction::PublishFreshOffer {
                    retired: "tc-old".to_string()
                }
            );
        }
    }
}
