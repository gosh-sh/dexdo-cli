//! One outstanding SELL, identified before POST and resumed through exact-hash reads.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::chain) struct SellerPlacement {
    pub network: String,
    pub note: String,
    pub token_contract: String,
    pub order_book: String,
    pub message_id: String,
    pub since: u64,
    // Older journals lack the signed header. Keep those under exact-message recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expire_at_unix: Option<u32>,
    // Hash and signed expiry of the zero-credit call used only to advance a quiet note's history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expiry_anchor: Option<(String, u32)>,
    // Accept journals from the pre-submit anchor implementation.
    #[serde(default, rename = "before_submit", skip_serializing)]
    _legacy_before_submit: Option<Value>,
    pending: Vec<(String, Option<String>)>,
    seen: BTreeSet<String>,
    pub accepted_order_id: Option<u128>,
    pub terminal: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replacement_pending: bool,
    rejection: Option<String>,
}

const UNVERIFIED_EXPIRY: &str = "signed_message_expired_without_execution";

fn finalized(transaction: &Value) -> bool {
    transaction["id"].as_str().is_some_and(|id| !id.is_empty())
        && (transaction["status"].as_i64() == Some(3)
            || transaction["status"].as_str() == Some("Finalized"))
}

fn valid_history_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

fn decode_note_transaction(
    node: &Value,
    note: &str,
    expected_hash: Option<&str>,
) -> Result<tvm_block::Transaction> {
    let id = node["id"]
        .as_str()
        .filter(|id| valid_history_hash(id))
        .ok_or_else(|| anyhow!("SELL note transaction has no valid id"))?;
    if !finalized(node)
        || expected_hash.is_some_and(|hash| !id.eq_ignore_ascii_case(hash))
        || node["account_addr"].as_str().map(bare_hex).as_deref() != Some(note)
    {
        return Err(anyhow!(
            "SELL note transaction identity or finalization is unproven"
        ));
    }
    let boc = node["boc"]
        .as_str()
        .ok_or_else(|| anyhow!("SELL note transaction has no BOC"))?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(boc)?;
    let root = tvm_types::read_single_root_boc(&bytes)
        .map_err(|error| anyhow!("decode SELL note transaction BOC: {error}"))?;
    if !root.repr_hash().to_hex_string().eq_ignore_ascii_case(id) {
        return Err(anyhow!("SELL note transaction BOC does not match its id"));
    }
    let tx = tvm_block::Transaction::construct_from_cell(root)
        .map_err(|error| anyhow!("decode SELL note transaction: {error}"))?;
    if !tx.account_id().to_hex_string().eq_ignore_ascii_case(note) {
        return Err(anyhow!(
            "SELL note transaction BOC is for a different account"
        ));
    }
    Ok(tx)
}

impl SellerPlacement {
    pub fn new(
        network: &str,
        note: &Address,
        tc: &Address,
        book: &Address,
        message_id: String,
        since: u64,
        expire_at_unix: Option<u32>,
    ) -> Self {
        Self {
            network: network.to_string(),
            note: note.with_workchain(),
            token_contract: tc.with_workchain(),
            order_book: book.with_workchain(),
            pending: vec![(message_id.clone(), None)],
            message_id,
            since,
            expire_at_unix,
            expiry_anchor: None,
            _legacy_before_submit: None,
            seen: BTreeSet::new(),
            accepted_order_id: None,
            terminal: None,
            replacement_pending: false,
            rejection: None,
        }
    }

    pub(in crate::chain) fn tc_rejection(&self) -> Option<&str> {
        self.rejection.as_deref().filter(|reason| {
            reason.contains("postFromNote was submitted")
                || reason.contains("postFromNote was rejected")
        })
    }

    pub fn needs_recovery(&self) -> bool {
        self.replacement_pending
            || self
                .terminal
                .as_deref()
                .is_none_or(|result| result == UNVERIFIED_EXPIRY)
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                format!("read SELL submission journal {}", path.display())
            })?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self, path: Option<&Path>) -> Result<()> {
        let Some(path) = path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        let mut file = std::fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut file, &serde_json::to_vec_pretty(self)?)?;
        file.sync_all()?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn validate(
        &self,
        network: &str,
        note: &Address,
        tc: &Address,
        book: &Address,
    ) -> Result<()> {
        if self.network != network
            || self.note != note.with_workchain()
            || self.token_contract != tc.with_workchain()
            || self.order_book != book.with_workchain()
            || self.message_id.is_empty()
        {
            return Err(anyhow!(
                "SELL submission journal identity does not match this network/note/TC/book"
            ));
        }
        Ok(())
    }
}

impl RealChainBackend {
    async fn seller_placement_query(&self, query: &str, variables: Value) -> Result<Value> {
        let response: Value = retry_chain_read(Some(self.client.gate()), || async {
            let response = self
                .http
                .post(format!(
                    "{}/graphql",
                    self.client.endpoint().trim_end_matches('/')
                ))
                .json(&json!({"query": query, "variables": variables}))
                .send()
                .await?;
            Ok(chain_response_for_status(response).await?.json().await?)
        })
        .await?;
        if let Some(errors) = response.get("errors").filter(|errors| !errors.is_null()) {
            return Err(anyhow!("SELL execution GraphQL errors: {errors}"));
        }
        Ok(response)
    }

    async fn seller_placement_message(&self, id: &str) -> Result<Value> {
        let query = r#"query($hash: String!) { blockchain { message(hash: $hash) {
                id src dst body boc
                dst_transaction { id status aborted out_msgs
                    compute { success exit_code } action { success result_code no_funds } }
            } } }"#;
        let response = self
            .seller_placement_query(query, json!({"hash": bare_hex(id)}))
            .await
            .with_context(|| format!("read SELL message {id}"))?;
        let node = response
            .pointer("/data/blockchain/message")
            .ok_or_else(|| anyhow!("SELL execution GraphQL shape changed for message {id}"))?;
        if !node.is_null()
            && node["id"].as_str().map(bare_hex).as_deref() != Some(bare_hex(id).as_str())
        {
            return Err(anyhow!(
                "SELL execution returned a different message for {id}"
            ));
        }
        Ok(node.clone())
    }

    async fn seller_latest_note_transaction(
        &self,
        note: &str,
    ) -> Result<Option<tvm_block::Transaction>> {
        let response = self
            .seller_placement_query(
                r#"query($account: String!, $dapp: String!) { blockchain {
                account(account_id: $account, dapp_id: $dapp) {
                    transactions(last: 1) { edges { node { id status account_addr boc } } }
                }
            } }"#,
                json!({"account": note, "dapp": crate::address::DEXDO_DAPP_ID}),
            )
            .await?;
        let edges = response
            .pointer("/data/blockchain/account/transactions/edges")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("SELL note history GraphQL response is incomplete"))?;
        let [edge] = edges.as_slice() else {
            return Ok(None);
        };
        decode_note_transaction(&edge["node"], note, None).map(Some)
    }

    /// Advance a quiet note without changing its trading state. This NEVER authorizes a SELL:
    /// the original hash still has to pass the complete linked-history proof on a later poll.
    pub(in crate::chain) async fn ensure_seller_expiry_anchor(
        &self,
        post: &mut SellerPlacement,
        path: Option<&Path>,
        keys: &KeyPair,
    ) -> Result<()> {
        let Some(expiry) = post.expire_at_unix else {
            return Ok(());
        };
        let now = local_unix_secs()?;
        if post.terminal.is_some()
            || !post.seen.is_empty()
            || now <= u64::from(expiry)
            || !self
                .seller_placement_message(&post.message_id)
                .await?
                .is_null()
        {
            return Ok(());
        }
        let note = Address::parse(&post.note)?;
        let Some(latest) = self.seller_latest_note_transaction(note.bare()).await? else {
            return Ok(());
        };
        if latest.now() > expiry {
            return Ok(());
        }
        if let Some((hash, anchor_expiry)) = &post.expiry_anchor {
            // Preserve an uncertain send across restart; an indexed anchor needs no more writes.
            if now <= u64::from(*anchor_expiry)
                || !self.seller_placement_message(hash).await?.is_null()
            {
                return Ok(());
            }
        }
        self.assert_seller_note_current(&note).await?;
        self.clock_skew_preflight().await?;
        let window = self
            .generation_pins_for_this_network()?
            .private_note_expiry_window_secs;
        let earliest_expiry = expiry
            .checked_add(window)
            .ok_or_else(|| anyhow!("SELL expiry anchor overflows the signed expiry"))?;
        let fresh_expiry = now
            .checked_add(SDK_MESSAGE_EXPIRY_SECS)
            .and_then(|time| u32::try_from(time).ok())
            .ok_or_else(|| anyhow!("SELL expiry anchor clock overflows the signed expiry"))?;
        let anchor_expiry = earliest_expiry.max(fresh_expiry);
        // The note accepts only expire < block.timestamp + window. This bound makes an
        // accepted anchor strictly later than the old SELL expiry, independent of client clocks.
        let boc = Self::seller_expiry_anchor_boc(&note, keys, anchor_expiry).await?;
        let previous = post
            .expiry_anchor
            .replace((external_message_hash(&boc)?, anchor_expiry));
        if let Err(error) = post.save(path) {
            post.expiry_anchor = previous;
            return Err(error);
        }
        self.retry_submit_preflighted(
            &boc,
            false,
            Some(crate::address::DEXDO_DAPP_ID),
            SubmitKind::Ordinary,
            &mut false,
        )
        .await?;
        Ok(())
    }

    pub(super) async fn seller_expiry_anchor_boc(
        note: &Address,
        keys: &KeyPair,
        expire: u32,
    ) -> Result<String> {
        use tvm_client::abi::{Abi, CallSet, FunctionHeader, ParamsOfEncodeMessage, Signer};
        let context = local_context()?;
        let signature_id = gosh_ackinacki::airegistry::calls::network_signature_id(&context).await;
        let message = tvm_client::abi::encode_message(
            context,
            ParamsOfEncodeMessage {
                abi: Abi::Json(PRIVATENOTE_ABI.into()),
                address: Some(note.with_workchain()),
                call_set: Some(CallSet {
                    function_name: "creditFromBook".into(),
                    header: Some(FunctionHeader {
                        expire: Some(expire),
                        ..Default::default()
                    }),
                    input: Some(json!({"amount":"0", "modelHash":"0"})),
                }),
                signer: Signer::Keys {
                    keys: tvm_client::crypto::KeyPair {
                        public: keys.public_hex().into(),
                        secret: keys.secret_hex().into(),
                    },
                },
                signature_id,
                ..Default::default()
            },
        )
        .await?;
        Ok(message.message)
    }

    /// Called before retrying a SELL, after the caller has checked the deployed note code pin.
    async fn seller_expiry_proven(
        &self,
        note: &str,
        expiry: u32,
        original_hash: &str,
    ) -> Result<bool> {
        let window = self
            .generation_pins_for_this_network()?
            .private_note_expiry_window_secs;
        let Some(valid_after) = expiry.checked_sub(window) else {
            return Ok(false);
        };
        if !valid_history_hash(original_hash) {
            return Ok(false);
        }
        let Some(mut tx) = self.seller_latest_note_transaction(note).await? else {
            return Ok(false);
        };
        if tx.now() <= expiry {
            return Ok(false);
        }

        // The pinned note accepts only block.timestamp < expiry < block.timestamp + window.
        // Walk verified predecessors to a block outside that window. Index gaps and a short
        // history never authorize a replacement, even if the exact message lookup was empty.
        let limit = crate::params::SELLER_EXPIRY_HISTORY_LIMIT;
        for count in 1..=limit {
            if tx.in_msg_cell().is_some_and(|cell| {
                cell.repr_hash()
                    .to_hex_string()
                    .eq_ignore_ascii_case(original_hash)
            }) {
                return Ok(false);
            }
            if tx.now() <= valid_after {
                return Ok(true);
            }
            let previous_lt = tx.prev_trans_lt();
            if previous_lt == 0 {
                return Ok(tx.prev_trans_hash().is_zero());
            }
            if count == limit || previous_lt >= tx.logical_time() {
                return Ok(false);
            }
            let hash = tx.prev_trans_hash().to_hex_string();
            let response = self
                .seller_placement_query(
                    r#"query($hash: String!) { blockchain {
                    transaction(hash: $hash) { id status account_addr boc }
                } }"#,
                    json!({"hash": hash}),
                )
                .await?;
            let previous = decode_note_transaction(
                &response["data"]["blockchain"]["transaction"],
                note,
                Some(&hash),
            )?;
            if previous.logical_time() != previous_lt || previous.now() > tx.now() {
                return Ok(false);
            }
            tx = previous;
        }
        Ok(false)
    }

    /// Re-read the exact old execution before allowing reserve recovery. An empty book or a
    /// diagnostic string alone cannot prove that an earlier SELL will never reach the book.
    pub(in crate::chain) async fn verified_seller_reserve_rejection(
        &self,
        post: &SellerPlacement,
        flags: u8,
        nonce: u64,
    ) -> Result<SellerPlacement> {
        let root = self.seller_placement_message(&post.message_id).await?;
        let transaction = &root["dst_transaction"];
        let call = root["body"]
            .as_str()
            .and_then(|body| decode_external_abi_message(body, PRIVATENOTE_ABI, true))
            .or_else(|| {
                root["boc"]
                    .as_str()
                    .filter(|boc| {
                        external_message_hash(boc).ok().as_deref() == Some(post.message_id.as_str())
                    })
                    .and_then(|boc| decode_external_abi_message_boc(boc, PRIVATENOTE_ABI, true))
            });
        if root["dst"].as_str() != Some(post.note.as_str())
            || !finalized(transaction)
            || !successful_inbound_call(&root)
            || !call.is_some_and(|call| {
                call.function_name == "postSellOffer"
                    && decoded_u128(&call.tokens, "flags") == Some(u128::from(flags))
                    && decoded_u128(&call.tokens, "nonce") == Some(u128::from(nonce))
            })
        {
            return Err(anyhow!(
                "original finalized postSellOffer and unchanged flags/nonce are unproven"
            ));
        }
        // This entry emits one note -> TC call. More outputs could hide another publication.
        let outputs = transaction["out_msgs"]
            .as_array()
            .filter(|outputs| outputs.len() == 1)
            .ok_or_else(|| anyhow!("original SELL does not have exactly one TC dispatch"))?;
        let id = outputs[0]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("original SELL TC dispatch identity is missing"))?;
        let child = self.seller_placement_message(id).await?;
        let transaction = &child["dst_transaction"];
        if child["src"].as_str() != Some(post.note.as_str())
            || child["dst"].as_str() != Some(post.token_contract.as_str())
            || !child["body"]
                .as_str()
                .and_then(|body| decode_internal_abi_call(body, TOKENCONTRACT_ABI))
                .is_some_and(|call| {
                    call.function_name == "postFromNote"
                        && decoded_u128(&call.tokens, "flags") == Some(u128::from(flags))
                })
            || !finalized(transaction)
            || transaction["aborted"].as_bool() != Some(true)
            || transaction["compute"]["exit_code"].as_i64() != Some(0)
            || transaction["action"]["success"].as_bool() != Some(false)
            || transaction["action"]["result_code"].as_i64() != Some(38)
            || transaction["action"]["no_funds"].as_bool() != Some(true)
            || !transaction["out_msgs"]
                .as_array()
                .is_some_and(Vec::is_empty)
        {
            return Err(anyhow!(
                "original postFromNote reserve rejection without outgoing messages is unproven"
            ));
        }
        let mut verified = post.clone();
        verified.rejection = Some(format!(
            "{}; recovery verified finalized reserve rejection message_hash={id} transaction_hash={} \
             aborted=true compute_exit=0 action_success=false action_result_code=38 no_funds=true out_msgs=[]",
            post.rejection.as_deref().unwrap_or_default(), transaction["id"],
        ));
        Ok(verified)
    }

    /// Read-only diagnosis after a note call failed to produce the expected TC state.
    pub(in crate::chain) async fn diagnose_note_tc_failure(
        &self,
        note: &Address,
        tc: &Address,
        operation: &str,
        note_message_hash: &str,
        error: anyhow::Error,
    ) -> anyhow::Error {
        let observation: Result<_> = async {
            let root = self.seller_placement_message(note_message_hash).await?;
            let transaction = &root["dst_transaction"];
            if root["dst"].as_str().map(bare_hex) != Some(bare_hex(note.bare()))
                || transaction["id"].as_str().is_none_or(str::is_empty)
                || !(transaction["status"].as_i64() == Some(3)
                    || transaction["status"].as_str() == Some("Finalized"))
                || !successful_inbound_call(&root)
            {
                return Err(anyhow!("successful finalized note submission is unproven"));
            }
            let outputs = transaction["out_msgs"]
                .as_array()
                .ok_or_else(|| anyhow!("note transaction has no outgoing message identities"))?;
            let mut matched = None;
            for output in outputs {
                let id = output
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| anyhow!("invalid note outgoing message identity"))?;
                let child = self.seller_placement_message(id).await?;
                if child["src"].as_str().map(bare_hex) == Some(bare_hex(note.bare()))
                    && child["dst"].as_str().map(bare_hex) == Some(bare_hex(tc.bare()))
                    && child["body"]
                        .as_str()
                        .and_then(|body| decode_internal_abi_call(body, TOKENCONTRACT_ABI))
                        .is_some_and(|call| call.function_name == operation)
                {
                    if matched.is_some() {
                        return Err(anyhow!("multiple matching internal TC calls"));
                    }
                    matched = Some(id.to_string());
                }
            }
            let id = matched.ok_or_else(|| anyhow!("no correlated note -> TC {operation} call"))?;
            let receipt = poll_finalized_destination_receipt(
                &self.http,
                self.client.endpoint(),
                tc.bare(),
                crate::address::DEXDO_DAPP_ID,
                &id,
            )
            .await?;
            Ok((id, receipt))
        }
        .await;
        let error = error.context(format!(
            "TokenContract {}: {operation} was submitted via seller note {}; \
             note_message_hash={note_message_hash}; required_operation_charge_raw={}; \
             recovery state must be retained; settlement is unconfirmed until an authoritative terminal fact",
            display_token_contract(tc), display_dexdo_address(note),
            tc_operation_charge(operation).map_or_else(|| "unavailable".to_string(), |value| value.to_string()),
        ));
        match observation {
            Ok((id, Some(receipt))) if receipt.aborted == Some(true) => {
                tc_reserve_receipt_error(error, tc, operation, &id, Some(&receipt))
            }
            observation => error.context(format!(
                "reserve cause unproven; internal receipt evidence={observation:?}"
            )),
        }
    }

    /// Follow only the submitted note call, its TC dispatch and book continuations. A successful
    /// ingress is not acceptance: the book can enqueue work for a later self-call.
    pub(in crate::chain) async fn reconcile_seller_placement(
        &self,
        post: &mut SellerPlacement,
        path: Option<&Path>,
    ) -> Result<()> {
        self.reconcile_seller_placement_with_expiry(post, path, false)
            .await
    }

    pub(in crate::chain) async fn reconcile_seller_placement_with_expiry(
        &self,
        post: &mut SellerPlacement,
        path: Option<&Path>,
        prove_expiry: bool,
    ) -> Result<()> {
        // Early builds used a wall-clock estimate as a history watermark. An old
        // checkpoint of that kind must still resolve the original exact hash.
        if post.terminal.as_deref() == Some(UNVERIFIED_EXPIRY) {
            post.terminal = None;
        }
        // A prior save may have failed after advancing the in-memory cursor or terminal fact.
        // Retry that checkpoint before skipping terminal work or reading any newer receipts.
        post.save(path)?;
        if let Some(reason) = post.tc_rejection() {
            return Err(anyhow!("{reason}"));
        }
        while post.terminal.is_none() {
            let Some((id, expected_source)) = post.pending.last().cloned() else {
                post.terminal =
                    Some(post.rejection.clone().unwrap_or_else(|| {
                        "execution_completed_without_observed_placement".into()
                    }));
                post.save(path)?;
                break;
            };
            if post.seen.contains(&id) {
                return Err(anyhow!("SELL execution message cycle at {id}"));
            }
            let root = id == post.message_id;
            let mut node = self.seller_placement_message(&id).await?;
            if root && node.is_null() && prove_expiry {
                if let Some(expiry) = post.expire_at_unix {
                    if self
                        .seller_expiry_proven(&bare_hex(&post.note), expiry, &id)
                        .await?
                    {
                        node = self.seller_placement_message(&id).await?;
                        if node.is_null() {
                            post.terminal =
                                Some("finalized_note_history_proves_expired_sell".into());
                            post.replacement_pending = true;
                            post.save(path)?;
                            return Ok(());
                        }
                    }
                }
            }
            if node.is_null() {
                return Err(anyhow!("missing SELL message {id}"));
            }
            let source = node["src"].as_str().unwrap_or("");
            let destination = node["dst"].as_str().unwrap_or("");
            if expected_source.is_some_and(|expected| !source.eq_ignore_ascii_case(&expected)) {
                return Err(anyhow!(
                    "SELL message {id} source does not match its finalized parent transaction"
                ));
            }
            if root && !destination.eq_ignore_ascii_case(&post.note) {
                return Err(anyhow!("SELL message {id} has the wrong note destination"));
            }
            let relevant = root
                || (source.eq_ignore_ascii_case(&post.note)
                    && destination.eq_ignore_ascii_case(&post.token_contract))
                || (source.eq_ignore_ascii_case(&post.token_contract)
                    && destination.eq_ignore_ascii_case(&post.order_book))
                || (source.eq_ignore_ascii_case(&post.order_book)
                    && destination.eq_ignore_ascii_case(&post.order_book));
            if source.eq_ignore_ascii_case(&post.order_book) {
                if let Some(body) = node["body"].as_str() {
                    if let Some(order_id) = super::super::book_events::decode_seller_placement(
                        body,
                        &post.note,
                        &post.token_contract,
                    )? {
                        post.accepted_order_id = Some(order_id);
                        post.terminal =
                            Some(format!("accepted order_id={order_id} message_id={id}"));
                        post.save(path)?;
                        return Ok(());
                    }
                }
            }
            let mut children = Vec::new();
            if relevant {
                let transaction = &node["dst_transaction"];
                if transaction["id"].as_str().is_none_or(str::is_empty) {
                    return Err(anyhow!(
                        "missing SELL destination transaction id for message {id}"
                    ));
                }
                if !finalized(transaction) {
                    return Err(anyhow!(
                        "missing finalized SELL destination transaction for message {id}"
                    ));
                }
                let outputs = transaction["out_msgs"]
                    .as_array()
                    .ok_or_else(|| anyhow!("SELL transaction for {id} has no out_msgs"))?;
                if transaction["aborted"].as_bool() == Some(true) && outputs.is_empty() {
                    let reason = format!("rejected message_id={id} transaction_id={} compute_exit={} action_result={}",
                        transaction["id"], transaction["compute"]["exit_code"], transaction["action"]["result_code"]);
                    post.rejection = Some(
                        if source.eq_ignore_ascii_case(&post.note)
                            && destination.eq_ignore_ascii_case(&post.token_contract)
                            && transaction["compute"]["exit_code"].as_i64() == Some(0)
                            && transaction["action"]["result_code"].as_i64() == Some(38)
                        {
                            // Preserve the finalized rejection before cancellable enrichment.
                            post.rejection = Some(format!(
                                "TokenContract {}: postFromNote was rejected; \
                                 note_message_hash={} message_hash={id} transaction_hash={}; \
                                 finalized aborted=true compute_exit=0 action_result=38; \
                                 reserve cause unproven; observed_reserve_raw=unavailable \
                                 required_operation_charge_raw={}; recovery state must be retained; \
                                 settlement is unconfirmed until an authoritative terminal fact",
                                display_token_contract(&post.token_contract), post.message_id,
                                transaction["id"].as_str().expect("validated above"),
                                crate::params::DEAL_BURN_POST_FROM_NOTE_RAW,
                            ));
                            post.save(path)?;
                            let error = self
                                .diagnose_note_tc_failure(
                                    &Address::parse(&post.note)?,
                                    &Address::parse(&post.token_contract)?,
                                    "postFromNote",
                                    &post.message_id,
                                    anyhow!(reason),
                                )
                                .await;
                            format!("{error:#}")
                        } else {
                            reason
                        },
                    );
                } else if !successful_inbound_call(&node) {
                    return Err(anyhow!(
                        "SELL transaction execution is unproven for message {id}: {transaction}"
                    ));
                }
                for output in outputs {
                    let child = output.as_str().filter(|id| !id.is_empty()).ok_or_else(|| {
                        anyhow!("SELL transaction {id} has an invalid outgoing message id")
                    })?;
                    if post.seen.contains(child)
                        || post.pending.iter().any(|(pending, _)| pending == child)
                        || children.iter().any(|(pending, _)| pending == child)
                    {
                        return Err(anyhow!("SELL execution message cycle at {child}"));
                    }
                    children.push((child.to_string(), Some(destination.to_string())));
                }
            }
            post.pending.pop();
            post.seen.insert(id);
            post.pending.extend(children);
            post.save(path)?;
        }
        if let Some(reason) = post.tc_rejection() {
            // The execution rejection is terminal, but the deal/recovery identity remains live.
            // Keep reporting it on resume instead of silently submitting another SELL.
            return Err(anyhow!("{reason}"));
        }
        Ok(())
    }
}
