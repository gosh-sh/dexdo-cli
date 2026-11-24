//! Hand one idle note to the launcher for replenishment while other notes keep their gateway.
use super::*;

pub(super) const HANDOFF_VAR: &str = "DEXDO_SELLER_REPLENISH_HANDOFF";
pub(super) const HELD_TC_VAR: &str = "DEXDO_SELLER_REPLENISH_HELD_TC";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum HandoffState {
    Idle { token_contract: String },
    Activate { token_contract: String },
    Failed,
}

pub(super) struct Handoff<'a> {
    pub(super) path: std::path::PathBuf,
    pub(super) market_path: &'a std::path::Path,
    pub(super) market: dexdo_core::MarketManifest,
    pub(super) lock: SellerPoolLock,
    pub(super) mock_token_count: u64,
    pub(super) hold_initial_market: bool,
}

async fn stopped(mut stop: tokio::sync::watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopped| *stopped).await;
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run<F, Fut, B, S>(
    seller: &dexdo::seller::RunningSeller,
    context: SellerPoolContext<'_>,
    policy: &policy::SellerRuntimePolicy,
    primary: Vec<SellerPoolDeal>,
    others: Vec<SellerPoolDeal>,
    mut provision: F,
    handoff: Handoff<'_>,
    backend: B,
    shutdown: Pin<&mut S>,
) -> Result<()>
where
    F: Clone + FnMut(String, String, u64, u64, u64) -> Fut,
    Fut: Future<
        Output = Result<(
            dexdo_core::MarketManifest,
            Arc<dyn dexdo_core::ChainBackend>,
        )>,
    >,
    B: AsyncFn(
        &dexdo_core::MarketManifest,
    ) -> Result<(
        Arc<dyn dexdo_core::ChainBackend>,
        dexdo::seller::UpstreamConfig,
    )>,
    S: futures::future::FusedFuture<Output = ()> + ?Sized,
{
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let primary_path = handoff.path.clone();
    let provision_primary = provision.clone();
    let renewed_note = async {
        let result = Box::pin(renew_note(
            seller,
            context,
            policy,
            primary,
            provision_primary,
            handoff,
            backend,
            stop_rx.clone(),
        ))
        .await;
        if let Err(error) = &result {
            tracing::error!(%error, "seller note replenishment stopped");
            deals::write_private_atomic(
                &primary_path,
                &serde_json::to_vec(&HandoffState::Failed)?,
            )?;
        }
        result
    };
    let other_notes = async {
        if others.is_empty() {
            return Ok(());
        }
        let stop = stopped(stop_rx.clone()).fuse();
        tokio::pin!(stop);
        let policy = SellerStartupPolicy {
            runtime: policy,
            retained_deals: others.len(),
            owns_gateway: false,
        };
        let mut shutdown_requested = false;
        Box::pin(run_seller_pool(
            seller,
            others,
            context,
            &policy,
            &mut provision,
            stop.as_mut(),
            &mut shutdown_requested,
        ))
        .await
    };
    let lanes = async { tokio::join!(renewed_note, other_notes) };
    tokio::pin!(lanes);
    let (primary, others) = tokio::select! {
        result = &mut lanes => result,
        _ = shutdown => {
            let _ = stop_tx.send(true);
            lanes.await
        }
    };
    primary.and(others)
}

#[allow(clippy::too_many_arguments)]
async fn renew_note<F, Fut, B>(
    seller: &dexdo::seller::RunningSeller,
    context: SellerPoolContext<'_>,
    runtime: &policy::SellerRuntimePolicy,
    mut primary: Vec<SellerPoolDeal>,
    mut provision: F,
    Handoff {
        path,
        market_path,
        mut market,
        mut lock,
        mock_token_count,
        mut hold_initial_market,
    }: Handoff<'_>,
    backend: B,
    stop: tokio::sync::watch::Receiver<bool>,
) -> Result<()>
where
    F: FnMut(String, String, u64, u64, u64) -> Fut,
    Fut: Future<
        Output = Result<(
            dexdo_core::MarketManifest,
            Arc<dyn dexdo_core::ChainBackend>,
        )>,
    >,
    B: AsyncFn(
        &dexdo_core::MarketManifest,
    ) -> Result<(
        Arc<dyn dexdo_core::ChainBackend>,
        dexdo::seller::UpstreamConfig,
    )>,
{
    let pool_dir = seller_pool_dir(context.deals_dir, &market.seller_note)?;
    if !market.all_or_none
        || primary.iter().any(|deal| {
            deal.cfg.subscription
                && deals::normalize_addr(&deal.cfg.token_contract)
                    == deals::normalize_addr(&market.token_contract)
        })
    {
        bail!("seller replenishment requires an ordinary all-or-none market");
    }
    loop {
        if *stop.borrow() {
            return Ok(());
        }
        if !primary.is_empty() {
            let policy = SellerStartupPolicy {
                runtime,
                retained_deals: primary.len(),
                owns_gateway: false,
            };
            let shutdown = stopped(stop.clone()).fuse();
            tokio::pin!(shutdown);
            let mut shutdown_requested = false;
            Box::pin(run_seller_pool(
                seller,
                primary,
                context,
                &policy,
                &mut provision,
                shutdown.as_mut(),
                &mut shutdown_requested,
            ))
            .await?;
            if shutdown_requested || *stop.borrow() {
                return Ok(());
            }
        }

        // The note's pool no longer has any paid or possibly executable deals. The launcher uses
        // the normal provision lock and its existing settlement / profit / nonce journal.
        drop(lock);
        deals::write_private_atomic(
            &path,
            &serde_json::to_vec(&HandoffState::Idle {
                token_contract: market.token_contract.clone(),
            })?,
        )?;
        let next = loop {
            tokio::select! {
                _ = stopped(stop.clone()) => return Ok(()),
                _ = tokio::time::sleep(dexdo_core::params::MATCH_CONFIRM_POLL_INTERVAL) => {}
            }
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            if let HandoffState::Activate { token_contract } = serde_json::from_slice(&bytes)? {
                let next = load_market(market_path)?;
                if deals::normalize_addr(&token_contract)
                    != deals::normalize_addr(&next.token_contract)
                {
                    bail!("replenishment handoff disagrees with the installed market");
                }
                break next;
            }
        };

        let mut expected = market.clone();
        expected.nonce = next.nonce;
        expected.token_contract.clone_from(&next.token_contract);
        // On crash recovery the launcher may have installed this exact market before
        // confirming its receipt. That same market can now be activated once.
        if !(hold_initial_market && next == market)
            && (expected != next
                || market.nonce == next.nonce
                || deals::normalize_addr(&market.token_contract)
                    == deals::normalize_addr(&next.token_contract))
        {
            bail!("replenishment changed the market identity or reused its deal");
        }
        lock = acquire_seller_pool_lock(&pool_dir)?;
        let records = seller_market_handle_records_matching(
            context.deals_dir,
            Some(&next.network),
            |owner| owner == deals::normalize_addr(&next.seller_note),
        )?;
        let record = records
            .get(&deals::normalize_addr(&next.token_contract))
            .filter(|record| record.market == next)
            .context("replenishment has no matching provision record")?;
        let (chain, upstream) = backend(&next).await?;
        if record.provision_pending {
            deals::persist_provision_pending(&record.path, false)?;
        }
        primary = vec![SellerPoolDeal {
            chain,
            cfg: dexdo::seller::SellerConfig {
                token_contract: next.token_contract.clone(),
                price_per_tick: next.price_per_tick.try_into()?,
                max_ticks: next.max_ticks.try_into()?,
                subscription: false,
                all_or_none: next.all_or_none,
                gateway_advertise: context.gateway_advertise.to_owned(),
                mock_token_count,
            },
            watch: dexdo::seller::SellerMatchWatchConfig {
                cursor_path: seller_watch_cursor_path(context.deals_dir, &next.token_contract)?,
                poll_interval: DEFAULT_MATCH_POLL_INTERVAL,
            },
            upstream,
            nonce: next.nonce,
            market: Some(next.clone()),
        }];
        market = next;
        hold_initial_market = false;
    }
}
