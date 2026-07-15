//! `MockChainBackend` (Directive 1) + its in-memory state — the offline e2e on-chain stand-in (PR4 move-only).
use super::types::*;
use super::{note_id_hex, ChainBackend};
use crate::machine::{Settlement, StreamMachine};
use crate::note::{Note, NotePubkey};
use crate::params::{DobParams, ProtocolConsts, Shell};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Endpoints file record: key — `token_contract`, value — the endpoint ciphertext (§3.1).
/// The same format carries over to Directive 2.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct EndpointsFile {
    /// `token_contract` → base64-independent raw ciphertext (as Vec<u8> in JSON).
    handovers: HashMap<TokenContract, Vec<u8>>,
}

/// Internal state of a single stream in the mock.
#[derive(Serialize, Deserialize)]
struct StreamCell {
    machine: StreamMachine,
    buyer_pubkey: NotePubkey,
    seller_locked: Shell,
    buyer_locked: Shell,
    seller_received: Shell,
    buyer_refunded: Shell,
    burned: Shell,
    closed: bool,
    /// The agreed ceiling on delivered ticks (from the offer, Directive 5 B2). Guard in `advance_tick`
    /// (review #4): the mock does not deliver more than the offer, just as the real TC is bounded by deposit.
    #[serde(default = "unbounded_max_ticks")]
    max_ticks: u64,
    /// A dispute is open (§4.2): the tick is frozen (not burned), the notes are locked — until `release_dispute`,
    /// which returns the tick to the buyer. Directive 5, `Dispute` mode.
    #[serde(default)]
    disputed: bool,
}

/// Default `max_ticks` for the old/carried state field — no ceiling (do not block existing streams).
fn unbounded_max_ticks() -> u64 {
    u64::MAX
}

/// Internal state of the mock on-chain. Serialized to a sidecar file — this makes the mock
/// **shared across processes** the same way the real chain is in Directive 2 (book/matches/streams
/// live outside the processes). The endpoints file (§3.1) holds ONLY the handover format SEPARATELY.
#[derive(Serialize, Deserialize, Default)]
struct MockState {
    offers: HashMap<TokenContract, SellOffer>,
    /// Filled offers are no longer active book asks, but the consumed terms remain part of the deal.
    #[serde(default)]
    matched_offers: HashMap<TokenContract, SellOffer>,
    /// Seller (hex of the note's ed-pubkey) per offer — for discovery/blacklist (Directive 5, B1/B16).
    #[serde(default)]
    offer_sellers: HashMap<TokenContract, String>,
    /// Notes locked by a dispute (Directive 5, §4.2): `TC.dispute()` locks BOTH notes — both the seller's and
    /// **the buyer's**. A locked note does not trade: a new offer (seller) / `place_buy` (buyer)
    /// are rejected with `ERR_STREAM_LOCKED`, until `release_dispute` resolves the dispute.
    #[serde(default)]
    locked_notes: HashSet<String>,
    matches: HashMap<TokenContract, Match>,
    streams: HashMap<TokenContract, StreamCell>,
}

/// Mock on-chain backend (Directive 1). Book/matches/streams — in the sidecar state file;
/// the enc-endpoint — in the endpoints file (seam §3.1, stable format for Directive 2).
#[derive(Clone)]
pub struct MockChainBackend {
    /// Serialization of critical sections (atomicity of read-modify-write over the file).
    lock: Arc<Mutex<()>>,
    endpoints_path: PathBuf,
    state_path: PathBuf,
    consts: ProtocolConsts,
    params: DobParams,
}

impl MockChainBackend {
    /// Create a mock with the given endpoints file path. The on-chain state is placed alongside
    /// in `<endpoints>.chainstate.json` — shared between the seller/buyer processes.
    pub fn new(endpoints_path: PathBuf, consts: ProtocolConsts, params: DobParams) -> Self {
        let state_path = endpoints_path.with_extension("chainstate.json");
        Self {
            lock: Arc::new(Mutex::new(())),
            endpoints_path,
            state_path,
            consts,
            params,
        }
    }

    fn load_state(&self) -> Result<MockState, ChainError> {
        match std::fs::read(&self.state_path) {
            Ok(bytes) if !bytes.is_empty() => {
                serde_json::from_slice(&bytes).map_err(|e| ChainError::EndpointsFile(e.to_string()))
            }
            Ok(_) => Ok(MockState::default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(MockState::default()),
            Err(e) => Err(ChainError::EndpointsFile(e.to_string())),
        }
    }

    fn store_state(&self, st: &MockState) -> Result<(), ChainError> {
        let bytes = serde_json::to_vec(st).map_err(|e| ChainError::EndpointsFile(e.to_string()))?;
        std::fs::write(&self.state_path, bytes)
            .map_err(|e| ChainError::EndpointsFile(e.to_string()))
    }

    fn read_endpoints(&self) -> Result<EndpointsFile, ChainError> {
        match std::fs::read(&self.endpoints_path) {
            Ok(bytes) if !bytes.is_empty() => {
                serde_json::from_slice(&bytes).map_err(|e| ChainError::EndpointsFile(e.to_string()))
            }
            Ok(_) => Ok(EndpointsFile::default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(EndpointsFile::default()),
            Err(e) => Err(ChainError::EndpointsFile(e.to_string())),
        }
    }

    fn write_endpoints(&self, f: &EndpointsFile) -> Result<(), ChainError> {
        let bytes = serde_json::to_vec(f).map_err(|e| ChainError::EndpointsFile(e.to_string()))?;
        std::fs::write(&self.endpoints_path, bytes)
            .map_err(|e| ChainError::EndpointsFile(e.to_string()))
    }
}

#[async_trait]
impl ChainBackend for MockChainBackend {
    async fn discover_offers(&self) -> Result<Vec<OfferListing>, ChainError> {
        let _g = self.lock.lock().unwrap();
        let st = self.load_state()?;
        Ok(st
            .offers
            .values()
            // Notes locked by a dispute do not trade (Directive 5, §4.2) — their offers are not in discovery.
            .filter(|o| {
                st.offer_sellers
                    .get(&o.token_contract)
                    .map(|s| !st.locked_notes.contains(s))
                    .unwrap_or(true)
            })
            .map(|o| OfferListing {
                seller_id: st
                    .offer_sellers
                    .get(&o.token_contract)
                    .cloned()
                    .unwrap_or_default(),
                token_contract: o.token_contract.clone(),
                price_per_tick: o.price_per_tick,
                max_ticks: o.max_ticks,
            })
            .collect())
    }

    async fn post_offer(&self, offer: SellOffer, note: &dyn Note) -> Result<(), ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        // Seller = hex of the note's ed-pubkey (B16: blacklist key during discovery, Directive 5).
        let seller_id: String = note
            .pubkey()
            .ed
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        // §4.2: a note locked by a dispute cannot trade (ERR_STREAM_LOCKED).
        if st.locked_notes.contains(&seller_id) {
            return Err(ChainError::Locked(format!(
                "seller {seller_id} note locked by dispute"
            )));
        }
        if st.offers.contains_key(&offer.token_contract) {
            return Err(ChainError::Chain(format!(
                "duplicate active sell order for TokenContract {}: cancel/fill the old order before reposting",
                offer.token_contract
            )));
        }
        st.offer_sellers
            .insert(offer.token_contract.clone(), seller_id);
        st.offers.insert(offer.token_contract.clone(), offer);
        self.store_state(&st)
    }

    async fn place_buy(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
    ) -> Result<(), ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        // §4.2: a buyer's note locked by a dispute cannot trade (failover in `dispute` mode is
        // impossible until `release_dispute` unlocks it) — `ERR_STREAM_LOCKED`.
        let buyer_id = note_id_hex(&note.pubkey());
        if st.locked_notes.contains(&buyer_id) {
            return Err(ChainError::Locked(format!(
                "buyer note {buyer_id} locked by dispute"
            )));
        }
        let offer = st
            .offers
            .get(token_contract)
            .ok_or_else(|| ChainError::NoMatch(token_contract.clone()))?
            .clone();
        st.offers.remove(token_contract);
        st.matched_offers
            .insert(token_contract.clone(), offer.clone());
        // The order book records the buyer's pubkey into token_contract (§2.3). The order book's role is done.
        st.matches.insert(
            token_contract.clone(),
            Match {
                token_contract: token_contract.clone(),
                buyer_pubkey: note.pubkey(),
                price_per_tick: offer.price_per_tick,
            },
        );
        self.store_state(&st)
    }

    async fn read_match(&self, token_contract: &TokenContract) -> Result<Match, ChainError> {
        let _g = self.lock.lock().unwrap();
        let st = self.load_state()?;
        st.matches
            .get(token_contract)
            .cloned()
            .ok_or_else(|| ChainError::NoMatch(token_contract.clone()))
    }

    async fn read_openable_match_now(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<Match>, ChainError> {
        let _g = self.lock.lock().unwrap();
        let st = self.load_state()?;
        Ok(st.matches.get(token_contract).cloned())
    }

    async fn open_stream(
        &self,
        token_contract: &TokenContract,
        enc_endpoint: Vec<u8>,
        _note: &dyn Note,
    ) -> Result<(), ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        let m = st
            .matches
            .get(token_contract)
            .cloned()
            .ok_or_else(|| ChainError::NoMatch(token_contract.clone()))?;

        // Ceiling on delivered ticks from the consumed offer (Directive 5 B2; #4-guard in advance_tick).
        let max_ticks = st
            .matched_offers
            .get(token_contract)
            .or_else(|| st.offers.get(token_contract))
            .map(|o| o.max_ticks)
            .unwrap_or(u64::MAX);
        // §3.1.2: the first tick is frozen (the buyer locked 1 tick), the seller posted
        // SELLER_PROBE_COMMISSION from the stake. There is no prepayment ahead.
        let machine = StreamMachine::open(m.price_per_tick, &self.params);
        let cell = StreamCell {
            machine,
            buyer_pubkey: m.buyer_pubkey.clone(),
            seller_locked: self.params.seller_probe_commission,
            buyer_locked: m.price_per_tick, // probe tick frozen
            seller_received: 0,
            buyer_refunded: 0,
            burned: 0,
            closed: false,
            max_ticks,
            disputed: false,
        };
        st.streams.insert(token_contract.clone(), cell);
        self.store_state(&st)?;

        // Seam §3.1: the enc-endpoint is placed into the endpoints file (the same format in Directive 2).
        let mut ef = self.read_endpoints()?;
        ef.handovers.insert(token_contract.clone(), enc_endpoint);
        self.write_endpoints(&ef)?;
        Ok(())
    }

    async fn read_handover(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<Vec<u8>>, ChainError> {
        let _g = self.lock.lock().unwrap();
        let ef = self.read_endpoints()?;
        Ok(ef.handovers.get(token_contract).cloned())
    }

    async fn accept_probe(&self, token_contract: &TokenContract) -> Result<(), ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        let cell = st
            .streams
            .get_mut(token_contract)
            .ok_or_else(|| ChainError::NoStream(token_contract.clone()))?;
        // §3.1.2: the probe is accepted → the probe tick goes to the seller, its commission is returned.
        cell.machine
            .on_probe_accepted()
            .map_err(|e| ChainError::EndpointsFile(e.0.to_string()))?;
        let p = cell.machine.price();
        cell.buyer_locked = cell.buyer_locked.saturating_sub(p);
        cell.seller_received += p;
        cell.seller_locked = cell
            .seller_locked
            .saturating_sub(self.params.seller_probe_commission);
        // The two-tick invariant kicks in: the next tick is prepaid, one more is frozen.
        cell.buyer_locked += 2 * p;
        self.store_state(&st)
    }

    async fn advance_tick(
        &self,
        token_contract: &TokenContract,
        _note: &dyn Note,
    ) -> Result<(), ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        let cell = st
            .streams
            .get_mut(token_contract)
            .ok_or_else(|| ChainError::NoStream(token_contract.clone()))?;
        // #4: the mock does not deliver more than the offer's `max_ticks` (the real TC is bounded by deposit). We count
        // finalized ticks by-fact (`seller_received / p`) and reject delivery beyond the ceiling.
        let p = cell.machine.price();
        let delivered = if p > 0 { cell.seller_received / p } else { 0 };
        if delivered >= cell.max_ticks {
            return Err(ChainError::Limit(format!(
                "advance_tick: max_ticks ({}) reached — the mock does not deliver beyond the offer",
                cell.max_ticks
            )));
        }
        cell.machine
            .on_tick_delivered()
            .map_err(|e| ChainError::EndpointsFile(e.0.to_string()))?;
        cell.seller_received += p;
        // `buyer_locked` does NOT change: the 2P window holds — 1 tick is finalized to the seller, the next
        // is already frozen (deposited by the previous transition). Previously there was a `sub(p).add(p)` (no-op)
        // here, which read as a bug (review #3) — the arithmetic was removed.
        self.store_state(&st)
    }

    async fn stop(
        &self,
        token_contract: &TokenContract,
        _note: &dyn Note,
    ) -> Result<Settlement, ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        let cell = st
            .streams
            .get_mut(token_contract)
            .ok_or_else(|| ChainError::NoStream(token_contract.clone()))?;
        let settlement = cell.machine.buyer_stop();
        match &settlement {
            Settlement::BurnBoth(b) => {
                // §3.1.2/§5.4: the buyer's probe tick + the seller's commission — burned, to no one.
                cell.burned += b.total();
                cell.buyer_locked = cell.buyer_locked.saturating_sub(b.buyer);
                cell.seller_locked = cell.seller_locked.saturating_sub(b.seller);
                // Neither seller_received nor buyer_refunded grows: scam revenue = 0.
            }
            Settlement::AmicableSplit {
                to_seller_ticks,
                to_buyer_refund,
            } => {
                // §4.1: the prepaid (delivered) tick → to the seller, the frozen buffer → to the buyer.
                // Both of the buyer's still-locked ticks are resolved: the lock is released entirely.
                let to_seller = to_seller_ticks * cell.machine.price();
                cell.seller_received += to_seller;
                cell.buyer_refunded += *to_buyer_refund;
                cell.buyer_locked = cell
                    .buyer_locked
                    .saturating_sub(to_seller)
                    .saturating_sub(*to_buyer_refund);
            }
            Settlement::SellerNoShow { .. } => {}
        }
        cell.machine.close();
        cell.closed = true;
        // Finalize the net fee by-fact: burn the net from what was delivered (§5.1/§5.4).
        burn_net_fee(cell, &self.consts);
        self.store_state(&st)?;
        Ok(settlement)
    }

    async fn dispute(
        &self,
        token_contract: &TokenContract,
        _note: &dyn Note,
    ) -> Result<Settlement, ChainError> {
        // §4.2: a dispute FREEZES (does NOT burn, unlike `stop`) — the buyer's tick is locked until
        // `release_dispute`, which returns it to the buyer; we lock the seller's note (`ERR_STREAM_LOCKED`
        // on new offers/discovery). Scam revenue stays 0. We return the EXPECTED post-release outcome.
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        let (to_buyer, commission, buyer_id) = {
            let cell = st
                .streams
                .get_mut(token_contract)
                .ok_or_else(|| ChainError::NoStream(token_contract.clone()))?;
            cell.machine.buyer_dispute();
            cell.disputed = true;
            (
                cell.buyer_locked,
                cell.seller_locked,
                note_id_hex(&cell.buyer_pubkey),
            )
        };
        // §4.2: `TC.dispute()` locks BOTH notes — the seller's AND the buyer's. The buyer's note is locked →
        // failover of this request is impossible (`place_buy` will be rejected), the request waits for `release_dispute`.
        if let Some(seller) = st.offer_sellers.get(token_contract).cloned() {
            st.locked_notes.insert(seller);
        }
        st.locked_notes.insert(buyer_id);
        self.store_state(&st)?;
        Ok(Settlement::SellerNoShow {
            to_buyer_refund: to_buyer,
            seller_commission_returned: commission,
        })
    }

    async fn release_dispute(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError> {
        // §4.2: the seller concedes → the frozen tick is returned to the buyer, the commission — to the seller
        // (unlock), WITHOUT burn; the seller's notes are unlocked. Scam revenue = 0.
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        let settlement = {
            let cell = st
                .streams
                .get_mut(token_contract)
                .ok_or_else(|| ChainError::NoStream(token_contract.clone()))?;
            if !cell.disputed {
                return Err(ChainError::Chain(format!(
                    "release_dispute: {token_contract} not in dispute"
                )));
            }
            let to_buyer = cell.buyer_locked;
            let commission = cell.seller_locked;
            cell.buyer_refunded += to_buyer;
            cell.buyer_locked = 0;
            cell.seller_locked = cell.seller_locked.saturating_sub(commission);
            cell.disputed = false;
            cell.machine.close();
            cell.closed = true;
            let settlement = Settlement::SellerNoShow {
                to_buyer_refund: to_buyer,
                seller_commission_returned: commission,
            };
            (settlement, note_id_hex(&cell.buyer_pubkey))
        };
        let (settlement, buyer_id) = settlement;
        // Unlock BOTH notes — the dispute is resolved.
        if let Some(seller) = st.offer_sellers.get(token_contract).cloned() {
            st.locked_notes.remove(&seller);
        }
        st.locked_notes.remove(&buyer_id);
        self.store_state(&st)?;
        Ok(settlement)
    }

    async fn seller_timeout(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        let cell = st
            .streams
            .get_mut(token_contract)
            .ok_or_else(|| ChainError::NoStream(token_contract.clone()))?;
        let settlement = cell.machine.seller_timeout();
        if let Settlement::SellerNoShow {
            to_buyer_refund,
            seller_commission_returned,
        } = &settlement
        {
            // §3.1.2/§3.4: the buyer takes the frozen tick, pays zero; the seller's
            // commission is returned to them — NOT burned.
            cell.buyer_refunded += *to_buyer_refund;
            cell.buyer_locked = cell.buyer_locked.saturating_sub(*to_buyer_refund);
            cell.seller_locked = cell
                .seller_locked
                .saturating_sub(*seller_commission_returned);
        }
        cell.machine.close();
        cell.closed = true;
        self.store_state(&st)?;
        Ok(settlement)
    }

    async fn cleanup_unopened(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError> {
        let _g = self.lock.lock().unwrap();
        let mut st = self.load_state()?;
        if st.streams.contains_key(token_contract) {
            return Err(ChainError::Chain(format!(
                "cleanup_unopened: {token_contract} is already opened"
            )));
        }
        if !st.matches.contains_key(token_contract) {
            return Err(ChainError::NoMatch(token_contract.clone()));
        }
        st.matches.remove(token_contract);
        st.matched_offers.remove(token_contract);
        self.store_state(&st)?;
        Ok(Settlement::SellerNoShow {
            to_buyer_refund: 0,
            seller_commission_returned: 0,
        })
    }

    async fn deal_state(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<DealChainState>, ChainError> {
        let _g = self.lock.lock().unwrap();
        let st = self.load_state()?;
        if let Some(cell) = st.streams.get(token_contract) {
            return Ok(Some(DealChainState {
                funded: true,
                opened: !cell.closed,
                disputed: cell.disputed,
                probe_accepted: cell.seller_received > 0,
                funded_time: Some(0),
                last_advance: 0,
            }));
        }
        if st.matches.contains_key(token_contract) {
            return Ok(Some(DealChainState {
                funded: true,
                opened: false,
                disputed: false,
                probe_accepted: false,
                funded_time: Some(0),
                last_advance: 0,
            }));
        }
        Ok(None)
    }

    async fn snapshot(&self, token_contract: &TokenContract) -> Option<StreamSnapshot> {
        let _g = self.lock.lock().unwrap();
        let st = self.load_state().ok()?;
        st.streams.get(token_contract).map(|c| StreamSnapshot {
            seller_locked: c.seller_locked,
            buyer_locked: c.buyer_locked,
            // #126: the mock holds no separate unspent deposit — its lock IS the at-risk lead.
            buyer_lead: c.buyer_locked,
            seller_received: c.seller_received,
            buyer_refunded: c.buyer_refunded,
            burned: c.burned,
            closed: c.closed,
        })
    }

    /// Full scan of the note's state (Directive 7, R11): own offers + deals (as seller/buyer)
    /// with the anonymous counterparty and by-fact settlement + exposure (locked in open deals).
    /// The lists are taken under the lock, then the lock is released — by-fact snapshots are pulled via separate
    /// `snapshot` calls (the sync Mutex is not reentrant). Read only.
    async fn note_snapshot(&self, note: &NotePubkey) -> Result<NoteSnapshot, ChainError> {
        let note_id = note_id_hex(note);
        let (offers, deal_keys) = {
            let _g = self.lock.lock().unwrap();
            let st = self.load_state()?;
            let mut offers = Vec::new();
            for (tc, o) in &st.offers {
                if st.offer_sellers.get(tc) == Some(&note_id) {
                    offers.push(OfferListing {
                        seller_id: note_id.clone(),
                        token_contract: tc.clone(),
                        price_per_tick: o.price_per_tick,
                        max_ticks: o.max_ticks,
                    });
                }
            }
            let mut deal_keys: Vec<(TokenContract, DealRole, Option<String>, Shell)> = Vec::new();
            let mut seen = HashSet::new();
            for (tc, seller) in &st.offer_sellers {
                if seller == &note_id {
                    let counterparty = st.matches.get(tc).map(|m| note_id_hex(&m.buyer_pubkey));
                    let price = st
                        .matches
                        .get(tc)
                        .map(|m| m.price_per_tick)
                        .or_else(|| st.matched_offers.get(tc).map(|o| o.price_per_tick))
                        .or_else(|| st.offers.get(tc).map(|o| o.price_per_tick))
                        .unwrap_or(0);
                    deal_keys.push((tc.clone(), DealRole::Seller, counterparty, price));
                    seen.insert(tc.clone());
                }
            }
            for (tc, m) in &st.matches {
                if note_id_hex(&m.buyer_pubkey) == note_id && seen.insert(tc.clone()) {
                    let counterparty = st.offer_sellers.get(tc).cloned();
                    deal_keys.push((tc.clone(), DealRole::Buyer, counterparty, m.price_per_tick));
                }
            }
            (offers, deal_keys)
        };
        let mut deals = Vec::new();
        let mut exposure: Shell = 0;
        for (tc, role, counterparty, price) in deal_keys {
            let snapshot = self.snapshot(&tc).await;
            if let Some(s) = &snapshot {
                if !s.closed {
                    let locked = match role {
                        DealRole::Buyer => s.buyer_locked,
                        DealRole::Seller => s.seller_locked,
                    };
                    exposure = exposure.saturating_add(locked);
                }
            }
            deals.push(DealView {
                token_contract: tc,
                role,
                counterparty,
                price_per_tick: price,
                // The mock book carries no per-deal model (the offer has none); real model names are
                // resolved by the real-chain reader from the TC's RootModel (issue #23 follow-up).
                model: None,
                snapshot,
            });
        }
        Ok(NoteSnapshot {
            note_id,
            offers,
            deals,
            exposure,
        })
    }
}

/// Burn the net fee (after-rebate) by-fact from the delivered volume (§5.1/§5.4).
/// Clean close without a dispute → the rebate is accounted for; here the stream is already closed amicably.
fn burn_net_fee(cell: &mut StreamCell, consts: &ProtocolConsts) {
    let p = cell.machine.price();
    if p == 0 {
        return;
    }
    let delivered_ticks = cell.seller_received / p;
    if delivered_ticks == 0 {
        return;
    }
    cell.burned += crate::settle::net_burn(delivered_ticks, p, consts);
}
