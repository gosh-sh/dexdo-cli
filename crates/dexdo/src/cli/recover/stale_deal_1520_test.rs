struct AbsentDealChain {
    tracked: bool,
    prior_buyer: Option<String>,
    touches: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl super::RecoverChain for AbsentDealChain {
    async fn state(
        &self,
        _tc: &dexdo_core::Address,
    ) -> anyhow::Result<Option<dexdo_core::DealChainState>> {
        Ok(None)
    }

    async fn touch_stale_deal(
        &self,
        _note: &dexdo_core::Address,
        _keys: &dexdo_core::KeyPair,
        _tc: &dexdo_core::Address,
    ) -> anyhow::Result<Option<(usize, usize)>> {
        if self.tracked {
            self.touches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Some((1, 0)))
        } else {
            Ok(None)
        }
    }

    async fn buyer_note(
        &self,
        _tc: &dexdo_core::Address,
    ) -> anyhow::Result<Option<dexdo_core::Address>> {
        panic!("an absent deal has no buyer getter")
    }

    async fn buyer_pubkey(&self, _tc: &dexdo_core::Address) -> anyhow::Result<Option<[u8; 32]>> {
        panic!("an absent deal has no buyer key getter")
    }

    async fn stop(
        &self,
        _note: &dexdo_core::Address,
        _keys: &dexdo_core::KeyPair,
        _tc: &dexdo_core::Address,
    ) -> anyhow::Result<dexdo_core::SettlementActionReceipt> {
        panic!("an absent deal cannot be stopped")
    }

    async fn settlement_receipts(
        &self,
        _tc: &dexdo_core::Address,
    ) -> anyhow::Result<dexdo_core::TokenContractSettlementReceipts> {
        Ok(dexdo_core::TokenContractSettlementReceipts {
            events: self.prior_buyer.as_ref().map_or_else(Vec::new, |buyer| {
                vec![dexdo_core::TokenContractSettlementReceipt {
                    message_id: "prior-stop".to_string(),
                    created_at: 1,
                    cursor: "prior-stop".to_string(),
                    event: dexdo_core::TokenContractSettlementEvent::StreamStopped {
                        buyer: buyer.clone(),
                        to_seller: 0,
                        refund_to_buyer: 0,
                    },
                }]
            }),
        })
    }
}

#[tokio::test]
async fn recover_clears_a_recorded_deal_whose_account_is_absent() {
    let dir = reclaim_test_dir("recover-stale-deal-1520");
    let _cleanup = TempDirCleanup(dir.clone());
    let key_path = dir.join("note.key");
    crate::cli::support::write_owner_only_key_fixture(&key_path, &"2a".repeat(32));

    let chain = AbsentDealChain {
        tracked: true,
        // A seller's stale note record can have a prior STOP whose beneficiary is the buyer.
        // The stale-record path must run before buyer-only receipt reconciliation.
        prior_buyer: Some(format!("0:{}", "83".repeat(32))),
        touches: std::sync::atomic::AtomicUsize::new(0),
    };
    let result = super::run_recover_with_chain_and_marker(
        RecoverArgs {
            identity: RecoveryIdentityArgs {
                note_key: Some(key_path),
                note_addr: Some(format!("0:{}", "81".repeat(32))),
            },
            token_contract: Some(format!("0:{}", "82".repeat(32))),
            market: None,
            pool: None,
        },
        &chain,
        &|_, _| panic!("clearing a note record must not mark a buyer STOP"),
    )
    .await;

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(chain.touches.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn recover_does_not_probe_an_absent_deal_the_note_never_recorded() {
    let dir = reclaim_test_dir("recover-unrecorded-deal-1520");
    let _cleanup = TempDirCleanup(dir.clone());
    let key_path = dir.join("note.key");
    crate::cli::support::write_owner_only_key_fixture(&key_path, &"2a".repeat(32));
    let chain = AbsentDealChain {
        tracked: false,
        prior_buyer: None,
        touches: std::sync::atomic::AtomicUsize::new(0),
    };

    let error = super::run_recover_with_chain(
        RecoverArgs {
            identity: RecoveryIdentityArgs {
                note_key: Some(key_path),
                note_addr: Some(format!("0:{}", "81".repeat(32))),
            },
            token_contract: Some(format!("0:{}", "82".repeat(32))),
            market: None,
            pool: None,
        },
        &chain,
    )
    .await
    .expect_err("an unrecorded absent deal must still be refused")
    .to_string();

    assert!(error.contains("is not active"), "{error}");
    assert_eq!(chain.touches.load(std::sync::atomic::Ordering::SeqCst), 0);
}
