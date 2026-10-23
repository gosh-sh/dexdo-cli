//! Read-only evidence for buyer-paid ECC[2], separate from the deal's seller-funded reserve.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuyerUsageOperation {
    pub source_id: String,
    pub created_at: u64,
    pub spent_raw: u128,
}

#[derive(Debug, Default)]
pub struct BuyerUsageOperations {
    pub operations: Vec<BuyerUsageOperation>,
    pub incomplete: Vec<String>,
}

fn finalized(transaction: &Value) -> bool {
    transaction["status"].as_i64() == Some(3) || transaction["status"].as_str() == Some("Finalized")
}

fn message_from_evidence(node: &Value) -> Result<tvm_block::Message> {
    let id = node["id"].as_str().context("usage message has no id")?;
    let boc = node["boc"].as_str().context("usage message has no BOC")?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(boc)?;
    let cell = tvm_types::read_single_root_boc(&bytes)?;
    if bare_hex(id) != cell.repr_hash().as_hex_string().to_ascii_lowercase() {
        return Err(anyhow!("usage message BOC does not match its queried id"));
    }
    tvm_block::Message::construct_from_cell(cell)
}

pub(super) fn verified_buy_call(
    node: &Value,
    note: &Address,
) -> Result<tvm_abi::contract::DecodedMessage> {
    let message = message_from_evidence(node)?;
    let header = message
        .ext_in_header()
        .context("buyer owner call is not external-in")?;
    if normalize_addr(&header.dst.to_string())? != normalize_addr(&note.with_workchain())? {
        return Err(anyhow!("buyer owner call targeted another note"));
    }
    let abi = tvm_abi::Contract::load(PRIVATENOTE_ABI.as_bytes())?;
    abi.decode_input(
        message.body().context("buyer owner call has no body")?,
        false,
        true,
    )
}

fn attached_shell(message: &tvm_block::Message) -> Result<u128> {
    let header = message
        .int_header()
        .context("usage message is not internal")?;
    header
        .value
        .get_other(crate::params::SHELL_CURRENCY_ID)?
        .map(|value| {
            value
                .to_string()
                .parse::<u128>()
                .context("attached SHELL exceeds uint128")
        })
        .unwrap_or(Ok(0))
}

fn reconciled_spend(node: &Value, attached: u128, refunded: Option<u128>) -> Result<Option<u128>> {
    if refunded.is_some_and(|refunded| refunded > attached) {
        return Err(anyhow!("buyer operation refund exceeds attachment"));
    }
    if !finalized(&node["dst_transaction"]) {
        return Ok(None);
    }
    if successful_inbound_call(node) {
        return Ok(Some(attached));
    }
    if node["dst_transaction"]["aborted"].as_bool() == Some(true)
        && (attached == 0 || refunded == Some(attached))
    {
        return Ok(Some(0));
    }
    Ok(None)
}

fn verified_operation(
    node: &Value,
    buyer_note: &str,
    token_contract: &str,
) -> Result<Option<(tvm_block::Message, String)>> {
    // The indexer's account list is only a search surface. The immutable BOC supplies identities,
    // currencies and call, and is hash-bound before any monetary figure is accepted.
    let message = message_from_evidence(node)?;
    let Some(header) = message.int_header() else {
        return Ok(None);
    };
    if header.bounced
        || normalize_addr(&header.src.to_string())? != normalize_addr(buyer_note)?
        || normalize_addr(&header.dst.to_string())? != normalize_addr(token_contract)?
    {
        return Ok(None);
    }
    let dapp = crate::address::DEXDO_DAPP_ID;
    if header
        .src_dapp_id
        .as_ref()
        .is_some_and(|id| id.as_hex_string() != dapp)
        || header
            .dest_dapp_id
            .as_ref()
            .is_some_and(|id| id.as_hex_string() != dapp)
    {
        return Err(anyhow!("buyer operation belongs to a foreign DApp"));
    }
    let body = message.body().context("buyer operation has no ABI body")?;
    let contract = tvm_abi::Contract::load(TOKENCONTRACT_ABI.as_bytes())?;
    let decoded = contract.decode_input(body, true, true)?;
    if !matches!(
        decoded.function_name.as_str(),
        "fundBuyerBond" | "stop" | "dispute" | "cleanupUnopened"
    ) {
        return Ok(None);
    }
    Ok(Some((message, decoded.function_name)))
}

const USAGE_MESSAGE_QUERY: &str = r#"
query($hash: String!) {
  blockchain { message(hash: $hash) {
    id boc created_at dst_transaction {
      status aborted out_msgs compute { success exit_code } action { success result_code }
    }
  } }
}"#;

const USAGE_FUNDING_SOURCE_QUERY: &str = r#"
query($hash: String!) {
  blockchain { message(hash: $hash) { id src src_transaction { id } } }
}"#;

fn verified_buy_model_name(model_hash: &str, name: &str) -> Result<String> {
    anyhow::ensure!(
        bare_hex(&model_hash_for(name)) == bare_hex(model_hash),
        "BUY book name does not match original modelHash"
    );
    Ok(name.to_owned())
}

fn verified_deal_receipt(
    node: &Value,
    deal: &Address,
    receipt: &TokenContractSettlementReceipt,
) -> Result<()> {
    anyhow::ensure!(
        node["id"].as_str().map(bare_hex) == Some(bare_hex(&receipt.message_id)),
        "accrual event identity differs"
    );
    let transaction = &node["src_transaction"];
    anyhow::ensure!(
        finalized(transaction) && successful_inbound_call(&json!({"dst_transaction":transaction})),
        "accrual execution is not finalized successfully"
    );
    let message = message_from_evidence(node)?;
    anyhow::ensure!(
        message.is_outbound_external(),
        "accrual is not external-out"
    );
    anyhow::ensure!(
        message
            .src()
            .and_then(|src| normalize_addr(&src.to_string()).ok())
            == Some(normalize_addr(&deal.with_workchain())?),
        "accrual belongs to another deal"
    );
    anyhow::ensure!(
        message.at_and_lt().map(|(time, _)| u64::from(time)) == Some(receipt.created_at),
        "accrual chain time differs from message"
    );
    let body = message.body().context("accrual event has no body")?;
    let body =
        base64::engine::general_purpose::STANDARD.encode(tvm_types::write_boc(&body.into_cell())?);
    anyhow::ensure!(
        decode_token_contract_settlement_event(&body)? == Some(receipt.event.clone()),
        "accrual amount differs from immutable event"
    );
    Ok(())
}

impl RealChainBackend {
    /// Bind an accrual or its original escrow base to the immutable finalized deal event.
    pub async fn buyer_usage_verify_deal_receipt(
        &self,
        deal: &Address,
        receipt: &TokenContractSettlementReceipt,
    ) -> Result<()> {
        let query = r#"query($hash: String!) { blockchain { message(hash: $hash) {
            id boc src_transaction { status aborted compute { success exit_code }
                action { success result_code } }
        } } }"#;
        let gql = format!("{}/graphql", self.client.endpoint().trim_end_matches('/'));
        self.client.gate().admit().await;
        let response = post_message_query(
            &self.http,
            &gql,
            query,
            &receipt.message_id,
            "buyer accrual",
        )
        .await?;
        let node = response
            .pointer("/data/blockchain/message")
            .context("accrual event absent")?;
        verified_deal_receipt(node, deal, receipt)
    }

    /// The canonical model book exists independently of any BUY fill or deal.
    pub async fn buyer_usage_buy_model_name(&self, model_hash: &str) -> Result<Option<String>> {
        let book = Self::canonical_inference_orderbook_address(model_hash)?;
        let Some(value) = self
            .run_getter_retrying(&book, INFERENCE_ORDERBOOK_ABI, "getModelName", json!({}))
            .await?
        else {
            return Ok(None);
        };
        let name = value["value0"].as_str().context("model book name absent")?;
        verified_buy_model_name(model_hash, name).map(Some)
    }

    /// The logical amount the successful transaction behind a funding event actually accepted.
    /// StreamFunded excludes a subscription bond; BuyerBondFunded excludes refunded excess. The
    /// inbound call is therefore required to reconstruct the buyer's gross contribution exactly.
    pub async fn buyer_usage_funding_contribution(
        &self,
        token_contract: &Address,
        buyer_note: &Address,
        event_message_id: &str,
        buyer_bond: bool,
    ) -> Result<u128> {
        let gql = format!("{}/graphql", self.client.endpoint().trim_end_matches('/'));
        self.client.gate().admit().await;
        let anchor = post_message_query(
            &self.http,
            &gql,
            USAGE_FUNDING_SOURCE_QUERY,
            event_message_id,
            "buyer funding event",
        )
        .await?;
        let anchor = anchor
            .pointer("/data/blockchain/message")
            .context("funding event absent")?;
        if anchor["id"].as_str().map(bare_hex) != Some(bare_hex(event_message_id))
            || anchor["src"]
                .as_str()
                .and_then(|src| normalize_addr(src).ok())
                != Some(normalize_addr(&token_contract.with_workchain())?)
        {
            return Err(anyhow!(
                "funding event identity differs from requested deal"
            ));
        }
        let transaction_id = anchor["src_transaction"]["id"]
            .as_str()
            .context("funding event has no emitting transaction")?;
        let query = r#"
        query($accountId: String!, $dappId: String!, $last: Int!, $before: String) {
          blockchain { account(account_id: $accountId, dapp_id: $dappId) {
            messages(msg_type: [IntIn], last: $last, before: $before) {
              pageInfo { startCursor hasPreviousPage }
              edges { node { id boc dst_transaction {
                id status aborted compute { success exit_code } action { success result_code }
              } } }
            }
          } }
        }"#;
        let mut before: Option<String> = None;
        loop {
            self.client.gate().admit().await;
            let response = self.http.post(&gql).json(&json!({"query":query,
                "variables":{"accountId":token_contract.bare(),"dappId":crate::address::DEXDO_DAPP_ID,
                    "last":crate::params::EXT_OUT_PAGE_SIZE,"before":before}})).send().await?;
            let response: Value = chain_response_for_status(response).await?.json().await?;
            if response.get("errors").is_some() {
                return Err(anyhow!("funding evidence query refused"));
            }
            let messages = response
                .pointer("/data/blockchain/account/messages")
                .context("funding history shape changed")?;
            for edge in messages["edges"]
                .as_array()
                .context("funding history has no edges")?
            {
                let node = &edge["node"];
                if node["dst_transaction"]["id"].as_str() != Some(transaction_id) {
                    continue;
                }
                if !finalized(&node["dst_transaction"]) || !successful_inbound_call(node) {
                    return Err(anyhow!(
                        "funding event execution is not finalized successfully"
                    ));
                }
                let message = message_from_evidence(node)?;
                let header = message
                    .int_header()
                    .context("funding call is not internal")?;
                if header.bounced
                    || normalize_addr(&header.dst.to_string())?
                        != normalize_addr(&token_contract.with_workchain())?
                {
                    return Err(anyhow!("funding call targets another deal"));
                }
                let body = message.body().context("funding call has no ABI body")?;
                let abi = tvm_abi::Contract::load(TOKENCONTRACT_ABI.as_bytes())?;
                let decoded = abi.decode_input(body, true, true)?;
                let (function, field) = if buyer_bond {
                    ("fundBuyerBond", "amount")
                } else {
                    ("fundFromOrderBook", "paid")
                };
                if decoded.function_name != function {
                    return Err(anyhow!("funding event came from an unexpected entry"));
                }
                let payer = if buyer_bond {
                    header.src.to_string()
                } else {
                    decoded_address(&decoded.tokens, "buyerNote")
                        .context("funding has no buyerNote")?
                };
                if normalize_addr(&payer)? != normalize_addr(&buyer_note.with_workchain())? {
                    return Err(anyhow!("funding belongs to another buyer note"));
                }
                return decoded_u128(&decoded.tokens, field)
                    .context("funding call amount is absent");
            }
            let Some(next) =
                previous_page_cursor("buyer funding evidence", messages, before.as_deref())?
            else {
                break;
            };
            before = Some(next);
        }
        Err(anyhow!(
            "funding event has no matching successful inbound transaction"
        ))
    }

    /// Confirm the immutable note code before interpreting its direct BUY-order burn.
    pub async fn buyer_usage_note_code_verified(&self, note: &Address) -> Result<bool> {
        let (active, hash) = self.account_active_code_hash(note, false).await?;
        let expected = compiled_contract_hash("PrivateNote")?;
        Ok(active
            && hash.as_deref().and_then(normalize_code_hash) == normalize_code_hash(&expected))
    }

    /// Actual attached ECC[2] for buyer-note calls, not calls multiplied by contract tariffs.
    /// An aborted call is final zero only after its full ECC has demonstrably bounced home.
    pub async fn buyer_usage_deal_operations(
        &self,
        buyer_note: &Address,
        token_contract: &Address,
    ) -> Result<BuyerUsageOperations> {
        let query = r#"
        query($accountId: String!, $dappId: String!, $last: Int!, $before: String) {
          blockchain { account(account_id: $accountId, dapp_id: $dappId) {
            messages(msg_type: [IntIn], last: $last, before: $before) {
              pageInfo { startCursor hasPreviousPage }
              edges { node { id boc created_at dst_transaction {
                status aborted out_msgs compute { success exit_code } action { success result_code }
              } } }
            }
          } }
        }"#;
        let gql = format!("{}/graphql", self.client.endpoint().trim_end_matches('/'));
        let mut report = BuyerUsageOperations::default();
        let mut seen = BTreeSet::new();
        let mut before: Option<String> = None;
        loop {
            self.client.gate().admit().await;
            let response = self
                .http
                .post(&gql)
                .json(&json!({
                    "query": query,
                    "variables": {"accountId": token_contract.bare(),
                        "dappId": crate::address::DEXDO_DAPP_ID,
                        "last": crate::params::EXT_OUT_PAGE_SIZE, "before": before}
                }))
                .send()
                .await?;
            let response: Value = chain_response_for_status(response).await?.json().await?;
            if response.get("errors").is_some() {
                return Err(anyhow!(
                    "buyer operation history GraphQL refused the evidence query"
                ));
            }
            let messages = response
                .pointer("/data/blockchain/account/messages")
                .context("buyer operation history shape changed")?;
            let edges = messages["edges"]
                .as_array()
                .context("buyer operation history has no edges")?;
            for edge in edges {
                let node = &edge["node"];
                let id = node["id"]
                    .as_str()
                    .context("buyer operation history has no message id")?;
                if !seen.insert(id.to_owned()) {
                    continue;
                }
                // The list also contains seller and book calls. Avoid interpreting their money.
                let Some((message, _function)) = verified_operation(
                    node,
                    &buyer_note.with_workchain(),
                    &token_contract.with_workchain(),
                )?
                else {
                    continue;
                };
                let attached = attached_shell(&message)?;
                let transaction = &node["dst_transaction"];
                let spent = if finalized(transaction) && successful_inbound_call(node) {
                    reconciled_spend(node, attached, None)?
                } else if finalized(transaction) && transaction["aborted"].as_bool() == Some(true) {
                    if attached == 0 {
                        Some(0)
                    } else {
                        let mut refunded = 0u128;
                        if let Some(ids) = transaction["out_msgs"].as_array() {
                            for bounce in ids {
                                let bounce =
                                    bounce.as_str().context("buyer bounce has no message id")?;
                                self.client.gate().admit().await;
                                let response = post_message_query(
                                    &self.http,
                                    &gql,
                                    USAGE_MESSAGE_QUERY,
                                    bounce,
                                    "buyer operation bounce",
                                )
                                .await?;
                                let bounce_node = response
                                    .pointer("/data/blockchain/message")
                                    .context("buyer bounce shape changed")?;
                                let bounced = message_from_evidence(bounce_node)?;
                                let Some(header) = bounced.int_header() else {
                                    continue;
                                };
                                if header.bounced
                                    && normalize_addr(&header.src.to_string())?
                                        == normalize_addr(&token_contract.with_workchain())?
                                    && normalize_addr(&header.dst.to_string())?
                                        == normalize_addr(&buyer_note.with_workchain())?
                                    && finalized(&bounce_node["dst_transaction"])
                                    && successful_inbound_call(bounce_node)
                                {
                                    refunded = refunded
                                        .checked_add(attached_shell(&bounced)?)
                                        .context("buyer bounce total overflow")?;
                                }
                            }
                        }
                        reconciled_spend(node, attached, Some(refunded))?
                    }
                } else {
                    None
                };
                if let Some(spent_raw) = spent {
                    report.operations.push(BuyerUsageOperation {
                        source_id: format!("buyer-operation:{id}"),
                        created_at: value_u64(&node["created_at"])
                            .context("buyer operation has no timestamp")?,
                        spent_raw,
                    });
                } else {
                    report
                        .incomplete
                        .push(format!("operation_not_reconciled:{id}"));
                }
            }
            let Some(next) =
                previous_page_cursor("buyer usage operations", messages, before.as_deref())?
            else {
                break;
            };
            before = Some(next);
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn buy_model_name_is_bound_to_the_original_model_hash_without_a_deal() {
        let name = "unfilled-model";
        let hash = crate::model_hash_for(name);
        assert_eq!(super::verified_buy_model_name(&hash, name).unwrap(), name);
        assert!(super::verified_buy_model_name(&hash, "another-model").is_err());
    }

    #[tokio::test]
    async fn signed_unfilled_buy_preserves_its_model_identity() {
        let note = Address::parse(&format!("0:{}", "a".repeat(64))).unwrap();
        let hash = model_hash_for("unfilled-model");
        let boc = RealChainBackend::encode_signed_call_boc(
            &note,
            PRIVATENOTE_ABI,
            "placeInferenceBuy",
            place_inference_buy_payload(&hash, crate::PRICE_STEP, 2, 3 * crate::PRICE_STEP, 0, 0),
            &KeyPair::from_secret_hex(&"22".repeat(32)).unwrap(),
        )
        .await
        .unwrap();
        let node = json!({"id":external_message_hash(&boc).unwrap(),"boc":boc});
        let call = verified_buy_call(&node, &note).unwrap();
        assert_eq!(call.function_name, "placeInferenceBuy");
        let original = decoded_uint256_hex(&call.tokens, "modelHash").unwrap();
        assert_eq!(bare_hex(&original), bare_hex(&hash));
        assert_eq!(
            verified_buy_model_name(&original, "unfilled-model").unwrap(),
            "unfilled-model"
        );
        let other = Address::parse(&format!("0:{}", "b".repeat(64))).unwrap();
        assert!(verified_buy_call(&node, &other).is_err());
    }

    use super::*;
    use tvm_block::Serializable;

    fn internal_message(shell: u128, native: u64) -> tvm_block::Message {
        let src = format!("0:{}", "1".repeat(64))
            .parse::<tvm_block::MsgAddressInt>()
            .unwrap();
        let dst = format!("0:{}", "2".repeat(64))
            .parse::<tvm_block::MsgAddressInt>()
            .unwrap();
        let mut value = tvm_block::CurrencyCollection::with_grams(native);
        value
            .set_other(crate::params::SHELL_CURRENCY_ID, shell)
            .unwrap();
        tvm_block::Message::with_int_header(tvm_block::InternalMessageHeader::with_addresses(
            src, dst, value,
        ))
    }

    fn node_for(message: &tvm_block::Message) -> Value {
        let cell = message.serialize().unwrap();
        json!({"id":cell.repr_hash().as_hex_string(),
            "boc":base64::engine::general_purpose::STANDARD.encode(tvm_types::write_boc(&cell).unwrap())})
    }

    #[test]
    fn accrual_requires_hash_bound_event_amount_time_deal_and_final_execution() {
        use tvm_abi::token::Tokenizer;
        use tvm_types::IBitstring;
        let deal = Address::parse(&format!("0:{}", "2".repeat(64))).unwrap();
        let buyer = format!("0:{}", "1".repeat(64));
        let abi = tvm_abi::Contract::load(TOKENCONTRACT_ABI.as_bytes()).unwrap();
        let event = abi.event("ProbeAccepted").unwrap();
        let tokens = Tokenizer::tokenize_all_params(
            &event.inputs,
            &json!({"buyer":buyer,"toSeller":"100","bondReturned":"0"}),
        )
        .unwrap();
        let mut prefix = tvm_types::BuilderData::new();
        prefix.append_u32(event.get_id()).unwrap();
        let body = tvm_abi::TokenValue::pack_values_into_chain(
            &tokens,
            vec![prefix.into()],
            &event.abi_version,
        )
        .unwrap();
        let header = tvm_block::ExtOutMessageHeader::with_addresses(
            deal.with_workchain().parse().unwrap(),
            tvm_block::MsgAddressExt::default(),
        );
        let mut message = tvm_block::Message::with_ext_out_header(header);
        message.set_at_and_lt(100, 1);
        message.set_body(tvm_types::SliceData::load_cell(body.into_cell().unwrap()).unwrap());
        let mut node = node_for(&message);
        node["src_transaction"] = json!({"status":3,"aborted":false,
            "compute":{"success":true,"exit_code":0},"action":{"success":true,"result_code":0}});
        let receipt = TokenContractSettlementReceipt {
            message_id: node["id"].as_str().unwrap().into(),
            created_at: 100,
            cursor: "c".into(),
            event: TokenContractSettlementEvent::ProbeAccepted {
                buyer,
                to_seller: 100,
                bond_returned: 0,
            },
        };
        verified_deal_receipt(&node, &deal, &receipt).unwrap();
        let other = Address::parse(&format!("0:{}", "3".repeat(64))).unwrap();
        assert!(verified_deal_receipt(&node, &other, &receipt).is_err());
        let mut changed = receipt.clone();
        changed.created_at += 1;
        assert!(verified_deal_receipt(&node, &deal, &changed).is_err());
        if let TokenContractSettlementEvent::ProbeAccepted { to_seller, .. } = &mut changed.event {
            *to_seller += 1;
        }
        changed.created_at = 100;
        assert!(verified_deal_receipt(&node, &deal, &changed).is_err());
        let mut changed = node.clone();
        changed["boc"] = node_for(&internal_message(100, 0))["boc"].clone();
        assert!(verified_deal_receipt(&changed, &deal, &receipt).is_err());
        let mut changed = node.clone();
        changed["src_transaction"]["status"] = json!(1);
        assert!(verified_deal_receipt(&changed, &deal, &receipt).is_err());
        node["src_transaction"]["aborted"] = json!(true);
        assert!(verified_deal_receipt(&node, &deal, &receipt).is_err());
    }

    #[test]
    fn foreign_buyer_or_deal_cannot_supply_an_operation() {
        let mut message = internal_message(45, 1);
        let contract = tvm_abi::Contract::load(TOKENCONTRACT_ABI.as_bytes()).unwrap();
        let function = contract.function("stop").unwrap();
        let body = function
            .encode_input(&std::collections::HashMap::new(), &[], true, None, None)
            .unwrap();
        message.set_body(tvm_types::SliceData::load_cell(body.into_cell().unwrap()).unwrap());
        let buyer = format!("0:{}", "1".repeat(64));
        let deal = format!("0:{}", "2".repeat(64));
        let other = format!("0:{}", "3".repeat(64));
        let node = node_for(&message);
        assert!(verified_operation(&node, &buyer, &deal).unwrap().is_some());
        assert!(verified_operation(&node, &other, &deal).unwrap().is_none());
        assert!(verified_operation(&node, &buyer, &other).unwrap().is_none());
        message.int_header_mut().unwrap().src_dapp_id = Some(tvm_types::UInt256::from([9; 32]));
        assert!(verified_operation(&node_for(&message), &buyer, &deal).is_err());
        message.int_header_mut().unwrap().src_dapp_id = None;
        message.int_header_mut().unwrap().dest_dapp_id = Some(tvm_types::UInt256::from([9; 32]));
        assert!(verified_operation(&node_for(&message), &buyer, &deal).is_err());
        message.int_header_mut().unwrap().dest_dapp_id = None;
        message.int_header_mut().unwrap().bounced = true;
        assert!(verified_operation(&node_for(&message), &buyer, &deal)
            .unwrap()
            .is_none());
    }

    #[test]
    fn shell_comes_from_ecc_not_native_value_and_boc_is_hash_bound() {
        let message = internal_message(123, 987_654);
        assert_eq!(attached_shell(&message).unwrap(), 123);
        let cell = message.serialize().unwrap();
        let mut node = json!({"id":cell.repr_hash().as_hex_string(),
            "boc":base64::engine::general_purpose::STANDARD.encode(tvm_types::write_boc(&cell).unwrap())});
        assert_eq!(
            attached_shell(&message_from_evidence(&node).unwrap()).unwrap(),
            123
        );
        node["id"] = json!("00".repeat(32));
        assert!(message_from_evidence(&node).is_err());
    }

    #[test]
    fn bounced_expense_is_zero_only_after_full_refund() {
        let node = json!({"dst_transaction":{"status":3,"aborted":true}});
        assert_eq!(reconciled_spend(&node, 45, None).unwrap(), None);
        assert_eq!(reconciled_spend(&node, 45, Some(44)).unwrap(), None);
        assert_eq!(reconciled_spend(&node, 45, Some(45)).unwrap(), Some(0));
        assert!(reconciled_spend(&node, 45, Some(46)).is_err());
    }

    #[test]
    fn successful_zero_attachment_never_charges_seller_reserve_to_buyer() {
        let node = json!({"dst_transaction":{"status":3,"aborted":false,
            "compute":{"success":true,"exit_code":0},"action":{"success":true,"result_code":0}}});
        assert_eq!(reconciled_spend(&node, 0, None).unwrap(), Some(0));
        assert_eq!(reconciled_spend(&node, 45, None).unwrap(), Some(45));
        let mut failed = node.clone();
        failed["dst_transaction"]["action"]["success"] = json!(false);
        assert_eq!(reconciled_spend(&failed, 45, None).unwrap(), None);
        failed["dst_transaction"]["status"] = json!(1);
        assert_eq!(reconciled_spend(&failed, 45, Some(45)).unwrap(), None);
    }
    #[test]
    fn message_amount_requires_hash_bound_boc() {
        assert!(message_from_evidence(&json!({"id":"foreign","boc":"AAAA"})).is_err());
        assert!(message_from_evidence(&json!({"id":"foreign"})).is_err());
    }
    #[test]
    fn finality_does_not_accept_submitted_transactions() {
        assert!(!finalized(&json!({"status": 1, "aborted": false})));
        assert!(finalized(&json!({"status": 3})));
        assert!(finalized(&json!({"status": "Finalized"})));
    }
}
