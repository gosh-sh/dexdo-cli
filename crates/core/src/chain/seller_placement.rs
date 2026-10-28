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
    pending: Vec<(String, Option<String>)>,
    seen: BTreeSet<String>,
    pub accepted_order_id: Option<u128>,
    pub terminal: Option<String>,
    rejection: Option<String>,
}

impl SellerPlacement {
    pub fn new(
        network: &str,
        note: &Address,
        tc: &Address,
        book: &Address,
        message_id: String,
        since: u64,
    ) -> Self {
        Self {
            network: network.to_string(),
            note: note.with_workchain(),
            token_contract: tc.with_workchain(),
            order_book: book.with_workchain(),
            pending: vec![(message_id.clone(), None)],
            message_id,
            since,
            seen: BTreeSet::new(),
            accepted_order_id: None,
            terminal: None,
            rejection: None,
        }
    }

    pub(in crate::chain) fn tc_rejection(&self) -> Option<&str> {
        self.rejection.as_deref().filter(|reason| {
            reason.contains("postFromNote was submitted")
                || reason.contains("postFromNote was rejected")
        })
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
    async fn seller_placement_message(&self, id: &str) -> Result<Value> {
        let query = r#"query($hash: String!) { blockchain { message(hash: $hash) {
                id src dst body
                dst_transaction { id status aborted out_msgs
                    compute { success exit_code } action { success result_code } }
            } } }"#;
        let response: Value = retry_transient_read(|| async {
            self.client.gate().admit().await;
            let response = self
                .http
                .post(format!(
                    "{}/graphql",
                    self.client.endpoint().trim_end_matches('/')
                ))
                .json(&json!({"query": query, "variables": {"hash": bare_hex(id)}}))
                .send()
                .await?;
            Ok(chain_response_for_status(response).await?.json().await?)
        })
        .await?;
        if let Some(errors) = response.get("errors") {
            return Err(anyhow!("SELL execution GraphQL errors for {id}: {errors}"));
        }
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
            let node = self.seller_placement_message(&id).await?;
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
            let root = id == post.message_id;
            if post.seen.contains(&id) {
                return Err(anyhow!("SELL execution message cycle at {id}"));
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
                if transaction["status"].as_i64() != Some(3)
                    && transaction["status"].as_str() != Some("Finalized")
                {
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
