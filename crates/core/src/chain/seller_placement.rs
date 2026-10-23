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
                    post.rejection = Some(format!("rejected message_id={id} transaction_id={} compute_exit={} action_result={}",
                        transaction["id"], transaction["compute"]["exit_code"], transaction["action"]["result_code"]));
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
        Ok(())
    }
}
