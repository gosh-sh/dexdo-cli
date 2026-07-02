use super::backends::note_owner_mismatch_reason;
use super::contracts_provision::*;
use super::*;
use crate::manifest::model_hash_for;
use crate::params::Shell;
use anyhow::{anyhow, Result};
use gosh_ackinacki::airegistry::calls::{encode_external_call, encode_internal_payload};
use gosh_ackinacki::airegistry::deploy::local_context;
use gosh_ackinacki::sdk::{Address, KeyPair};
use gosh_ackinacki::wallet::contracts::MULTISIG_ABI_JSON;
use serde_json::{json, Value};

/// LIVE: the seller note posts the probe-commission to its already-deployed per-deal
/// `TokenContract` from its OWN ECC[2](`postProbeCommission` -> `TC.fundProbeCommission`) -- **no operator
/// wallet**. Asserts `getProbe().probeFunded == true`. Needs a note that already provisioned the deal TC
/// (`dexdo provision`), passed via env. Run:
/// `DEXDO_PROOF_NOTE_ADDR=0:.. DEXDO_PROOF_NOTE_KEY=/path/key DEXDO_PROOF_NONCE=7 \
/// cargo test -p dexdo-core --features shellnet,test-giver live_post_probe_commission -- --ignored --nocapture`
#[tokio::test]
#[ignore = "live : postProbeCommission funds the probe on the deployed TC from the note (no wallet)"]
async fn live_post_probe_commission_note_funded() {
    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let note_addr = std::env::var("DEXDO_PROOF_NOTE_ADDR").expect("DEXDO_PROOF_NOTE_ADDR");
    let key_file = std::env::var("DEXDO_PROOF_NOTE_KEY").expect("DEXDO_PROOF_NOTE_KEY");
    let nonce: u64 = std::env::var("DEXDO_PROOF_NONCE")
        .expect("DEXDO_PROOF_NONCE")
        .parse()
        .expect("nonce u64");
    let secret = std::fs::read_to_string(&key_file).expect("read note key file");
    let keys = KeyPair::from_secret_hex(secret.trim()).expect("note keypair");
    let seller_pubkey = json!(format!("0x{}", keys.public_hex()));
    let be = RealChainBackend::connect(manifest).expect("connect to shellnet");
    let note = Address::parse(&note_addr).expect("note addr");
    let rm = be
        .root_model_address_for(&seller_pubkey)
        .await
        .expect("RootModel addr");
    let tc = be
        .resolve_token_contract(&rm, &seller_pubkey, nonce)
        .await
        .expect("TC addr (provision the market first)");

    // Post the probe-commission from the note's OWN ECC[2] -- no operator wallet.
    be.note_post_probe_commission(&note, &keys, nonce, 10_000_000)
        .await
        .expect("postProbeCommission");

    // The internal message settles in a few blocks -> poll getProbe() until probeFunded.
    let mut funded = false;
    for _ in 0..20 {
        if let Ok(Some(p)) = be.token_contract_probe(&tc).await {
            if p["probeFunded"].as_bool() == Some(true) {
                println!("=== TC {tc} getProbe = {p} ===");
                funded = true;
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(
        funded,
        "postProbeCommission set probeFunded==true on the deployed TC (note-funded, no wallet)"
    );
}

/// A LIVE read-only test against the real Acki Nacki testnet(shellnet): connection,
/// chain liveness, and the fact that the `SuperRoot` from the manifest is an active deployed account.
/// Without keys/gas. The `#[ignore]` gate -- a normal offline `cargo test` stays green.
/// Run: `cargo test -p dexdo-core --features shellnet -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "live: hits the real Acki Nacki shellnet Block Manager (read-only)"]
async fn live_shellnet_connect_and_read() {
    // The manifest lives at the workspace root; cargo runs the test from the crate's directory.
    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let be = RealChainBackend::connect(manifest).expect("connect to shellnet + load manifest");

    let live = be.liveness().await.expect("chain_liveness");
    println!("=== shellnet liveness: {live:?} ===");

    // The SuperRoot from the manifest must be an active account on shellnet.
    let acc = be
        .client()
        .get_account(be.superroot())
        .await
        .expect("get_account(SuperRoot)");
    match &acc {
        Some(a) => println!(
            "=== SuperRoot {} : active={} shell={} ===",
            be.superroot(),
            a.is_active(),
            a.shell()
        ),
        None => println!("=== SuperRoot {} : account not found ===", be.superroot()),
    }
    assert!(
        acc.map(|a| a.is_active()).unwrap_or(false),
        "SuperRoot must be active on shellnet"
    );

    // Address derivation from SuperRoot -- on-chain getters(address resolution for ChainBackend).
    let owner = be.superroot_owner_pubkey().await.expect("getOwnerPubkey");
    println!("=== SuperRoot owner pubkey: {owner} ===");
    let rm = be
        .resolve_root_model()
        .await
        .expect("getRootModelAddress (derive)");
    println!("=== RootModel derived from SuperRoot: {rm} ===");
    let rm_acc = be
        .client()
        .get_account(&rm)
        .await
        .expect("get_account(RootModel)");
    println!(
        "=== RootModel deployed/active = {} ===",
        rm_acc.map(|a| a.is_active()).unwrap_or(false)
    );
}

/// A LIVE write test: the executor **provisions** the wallet itself -- generates a key, computes
/// the deterministic multisig deploy address and **mints test SHELL from the shellnet giver**
/// (`0:1111...`, keys in the SDK config). Removes the former blocker "a funded wallet from the
/// coordinator is required".
/// This is the first REAL write against shellnet -- it checks that submit goes through from this environment.
#[tokio::test]
#[ignore = "live: mints testnet SHELL from the shellnet Giver (a real write submit)"]
async fn live_giver_funds_fresh_wallet() {
    use gosh_ackinacki::airegistry::deploy::local_context;
    use gosh_ackinacki::config::AiRegistryConfig;
    use gosh_ackinacki::wallet::deploy::{prepare_deploy, DeployParams};
    use gosh_ackinacki::wallet::giver::GiverClient;

    let cfg = AiRegistryConfig::shellnet();
    let endpoint = "https://shellnet.ackinacki.org";
    let ctx = local_context().expect("client context");
    // A write(`/v2/messages`) behind Cloudflare: the default reqwest UA is blocked -> browser UA
    // (per the access-notes from the coordinator's comment on 82dbe51).
    let http = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36")
        .build()
        .expect("http client");
    let giver = GiverClient::new(
        ctx,
        cfg.giver_address.as_deref().expect("giver address"),
        cfg.giver_pubkey.as_deref().expect("giver pubkey"),
        cfg.giver_secret.as_deref().expect("giver secret"),
        endpoint,
        http.clone(),
    );

    // A fresh key + the deterministic multisig deploy address(one key for all 3 owners -- for the probe).
    let kp = KeyPair::generate();
    let params = DeployParams {
        agent_pubkey: kp.public_hex().to_string(),
        controller_pubkey: kp.public_hex().to_string(),
        owner_pubkey: kp.public_hex().to_string(),
        initial_value: 0,
    };
    let prepared = prepare_deploy(&params, kp.secret_hex()).expect("prepare deploy");
    println!("=== fresh wallet deploy address: {} ===", prepared.address);

    // Giver diagnostics: whether it exists, whether it is funded, and whether the keys in the SDK config are stale
    // (after an emergency shellnet restart the genesis contracts were re-keyed -- comment on 82dbe51).
    {
        let be0 = RealChainBackend::connect(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contracts/deployed.shellnet.json"
        ))
        .expect("connect");
        let gaddr = Address::parse(cfg.giver_address.as_deref().unwrap()).unwrap();
        match be0.client().get_account(&gaddr).await.expect("get giver") {
            Some(a) => println!(
                "=== Giver {gaddr}: active={} shell={} ecc2={} (SDK giver_pubkey={}) ===",
                a.is_active(),
                a.shell(),
                a.ecc_balance(2),
                cfg.giver_pubkey.as_deref().unwrap_or("?")
            ),
            None => println!("=== Giver {gaddr}: NOT FOUND on shellnet ==="),
        }
    }

    // The e2e_airegistry pattern: **fund -> deploy -> wait Active** (on an uninit address the balance reads 0;
    // the criterion is wallet activation after the deploy). `DEPLOY_FUND` = 200 vmshell.
    giver
        .fund_deploy_address(&prepared.address, 200_000_000_000)
        .await
        .expect("giver fund_deploy_address");
    // Deploy the multisig(the deploy spends the funded gas).
    gosh_ackinacki::wallet::query::send_message(&http, endpoint, &prepared.message_boc_base64)
        .await
        .expect("deploy submit");

    // Wait for wallet activation.
    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let be = RealChainBackend::connect(manifest).expect("connect");
    let addr = Address::parse(&prepared.address).expect("parse addr");
    let mut active = false;
    let mut bal: u128 = 0;
    for _ in 0..30 {
        if let Some(acc) = be.client().get_account(&addr).await.expect("get_account") {
            if acc.is_active() {
                active = true;
                bal = acc.shell().max(acc.ecc_balance(2));
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    println!("=== self-provisioned wallet {addr}: active={active} balance={bal} ===");
    assert!(
        active,
        "the wallet is deployed and active -- the giver funds, self-provisioning works"
    );
}

/// A LIVE deal test, step 1: a minted note(`mint_pn_pool`) is an **inference `PrivateNote`**
/// (has the owner method `getInferenceOrderBookAddress`), and it **deploys `InferenceOrderBook`**
/// on shellnet. The path to the pool(owner keys; the secrets are gitignored, outside the repo) is taken from
/// `DEXDO_PN_POOL`; without it the test is skipped. Run:
/// `DEXDO_PN_POOL=/abs/pn_pool.json cargo test -p dexdo-core --features shellnet -- --ignored --nocapture live_minted_note`.
#[tokio::test]
#[ignore = "live: deploys an InferenceOrderBook on shellnet from a minted note (a real write)"]
async fn live_minted_note_deploys_inference_orderbook() {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set -- skipping (needs pn_pool.json with owner keys)");
        return;
    };
    let pool: Value = serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool"))
        .expect("parse pool json");
    let n0 = &pool["notes"].as_array().expect("pool.notes[]")[0];
    let note_addr = Address::parse(n0["address"].as_str().expect("note0 address")).expect("addr");
    let owner = KeyPair::from_secret_hex(n0["owner_secret_key_hex"].as_str().expect("secret"))
        .expect("note0 keypair");

    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let be = RealChainBackend::connect(manifest).expect("connect");

    // The note is active; we print the on-chain code_hash against the embedded PrivateNote.tvc (diagnostic --
    // the deployed gosh-dexdo code may differ by build, but the signatures of the 5 methods are identical).
    let acc = be
        .client()
        .get_account(&note_addr)
        .await
        .expect("get note")
        .expect("note exists");
    assert!(acc.is_active(), "the minted note is active");
    println!(
        "note0 {note_addr}: active={} shell={} code_hash={:?}",
        acc.is_active(),
        acc.shell(),
        acc.code_hash
    );
    println!(
        "specs PrivateNote.tvc code_hash = {}",
        code_hash(PRIVATENOTE_TVC).expect("pn hash")
    );

    // Book parameters: a per-model name (4.0.6: the book verifies sha256(modelName)==modelHash) + tickSize.
    let frame_model = format!("dexdo-d-{}--book-derivation", std::process::id());
    let model_hash = model_hash_for(&frame_model);
    let tick_size: u128 = MODEL_TICK_SIZE;

    // (1) Proof of the inference variant: the getter getInferenceOrderBookAddress exists and
    // returns a deterministic book address(a non-inference note lacks the method -> it would fail).
    let ob_addr = be
        .inference_orderbook_address(&note_addr, &model_hash, tick_size)
        .await
        .expect("getInferenceOrderBookAddress -- meaning the note is an inference note");
    println!("=== InferenceOrderBook address (derived) = {ob_addr} ===");
    let before = be
        .client()
        .get_account(&ob_addr)
        .await
        .expect("get ob pre")
        .map(|a| a.is_active())
        .unwrap_or(false);
    println!("OB active before deploy = {before}");

    // (2) The note deploys the book(a write, gas from the note's budget).
    let resp = be
        .deploy_inference_orderbook(&note_addr, &owner, &model_hash, &frame_model, tick_size)
        .await
        .expect("deployInferenceOrderBook submit");
    println!("=== deploy submit resp = {resp} ===");

    // (3) Wait for book activation on the chain.
    let mut active = false;
    for _ in 0..40 {
        if let Some(a) = be.client().get_account(&ob_addr).await.expect("get ob") {
            if a.is_active() {
                active = true;
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    println!("=== InferenceOrderBook {ob_addr}: active={active} ===");
    assert!(
        active,
        "InferenceOrderBook is deployed and active on shellnet"
    );

    // The parameters of the book that came up(modelHash/tickSize/platformFeeBps) -- confirm that
    // the book is deployed with the expected parameters and the getters are readable.
    let params = be
        .inference_orderbook_params(&ob_addr)
        .await
        .expect("getParams");
    println!("=== InferenceOrderBook params = {params:?} ===");
    assert!(params.is_some(), "getParams is readable on an active book");
}

/// A LIVE deal test, step 2: the seller(note0) **provisions a per-deal `TokenContract`** and
/// **posts an offer into the on-chain book** `InferenceOrderBook`. Checks that the offer actually landed
/// in the order book(`getStats.orderCount>=1`, `getBestBidAsk.hasAsk`). Deploys the TC at an address that
/// matches `RootModel.getTokenContractAddress`(cross-check). Requires `DEXDO_PN_POOL`.
/// DIAGNOSTIC(live, read-only): query the on-chain RootPN `0:1010...1010` for the
/// `privateNoteCodeHash` it pins. This decides which PrivateNote code freshly-minted notes get:
/// `934cf19c...` => the repo(DEXDO) set is current (my pool is current; the path fix is a contract-logic
/// derivation matter); anything else => the contracts were redeployed and my pool is stale.
#[tokio::test]
#[ignore = "live (diag, read-only): RootPN.getDetails().privateNoteCodeHash"]
async fn diag_rootpn_pinned_code_hash() {
    const ROOTPN_ABI: &str =
        include_str!("../../../../contracts/compiled_0.79.3/dex/RootPN.abi.json");
    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let be = RealChainBackend::connect(manifest).expect("connect");
    let rootpn =
        Address::parse("0:1010101010101010101010101010101010101010101010101010101010101010")
            .expect("rootpn addr");
    let details = be
        .client()
        .run_getter(&rootpn, ROOTPN_ABI, "getDetails", json!({}))
        .await
        .expect("RootPN.getDetails");
    println!("=== RootPN 0:1010 getDetails = {details:?} ===");
    println!("(repo deployed.json claims privateNoteCodeHash = 934cf19c... = DEXDO set)");
}

/// DIAGNOSTIC(live, writes): prove the wallet-funded voucher leg before running the full
/// `dexdo note deploy` flow. This mirrors `fund_probe_commission`: internal payload + multisig
/// `sendTransaction` + attached ECC[2] SHELL, then waits for RootPN's `VoucherGenerated` ext-out
/// carrying our unique `skUCommit`.
#[tokio::test]
#[ignore = "live (diag, writes): wallet sendTransaction -> RootPN.generateVoucher"]
#[cfg(feature = "test-giver")]
async fn live_multisig_send_transaction_generates_rootpn_voucher() {
    use base64::Engine as _;
    use rand::RngCore;

    const ROOTPN_ABI: &str =
        include_str!("../../../../contracts/compiled_0.79.3/dex/RootPN.abi.json");
    const ROOTPN: &str = "0:1010101010101010101010101010101010101010101010101010101010101010";
    const VOUCHER_EVENT_DST: &str =
        ":0000000000000000000000000000000000000000000000000000000000000087";
    const SHELL_N100_RAW: u128 = 100_000_000_000;

    async fn wait_voucher_extout(
        be: &RealChainBackend,
        commit: &[u8; 32],
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value> {
        let query = r#"
            query($accountId: String!, $dappId: String!, $last: Int!) {
              blockchain {
                account(account_id: $accountId, dapp_id: $dappId) {
                  messages(msg_type: [ExtOut], last: $last) {
                    edges { node { id body dst created_at src_transaction { id } } }
                  }
                }
              }
            }
        "#;
        let start = std::time::Instant::now();
        loop {
            let resp: serde_json::Value = be
                .http
                .post("https://shellnet.ackinacki.org/graphql")
                .json(&json!({
                    "query": query,
                    "variables": {
                        "accountId": "1010101010101010101010101010101010101010101010101010101010101010",
                        "dappId": "0000000000000000000000000000000000000000000000000000000000000000",
                        "last": 200,
                    }
                }))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let edges = resp["data"]["blockchain"]["account"]["messages"]["edges"]
                .as_array()
                .ok_or_else(|| anyhow!("RootPN ext-out GraphQL shape changed: {resp}"))?;
            for edge in edges {
                let node = &edge["node"];
                if node["dst"].as_str() != Some(VOUCHER_EVENT_DST) {
                    continue;
                }
                let Some(body) = node["body"].as_str() else {
                    continue;
                };
                let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(body) else {
                    continue;
                };
                if bytes.windows(commit.len()).any(|w| w == commit) {
                    return Ok(node.clone());
                }
            }
            if start.elapsed() >= timeout {
                return Err(anyhow!(
                    "timed out waiting for RootPN.VoucherGenerated with skUCommit 0x{}",
                    bytes_to_hex(commit)
                ));
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    fn bytes_to_hex(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
        out
    }

    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let be = RealChainBackend::connect(manifest).expect("connect");
    let wallet_keys = KeyPair::generate();
    let wallet = be
        .deploy_multisig(&wallet_keys)
        .await
        .expect("deploy operational multisig");
    be.giver_send_shell(&wallet.with_workchain(), 400_000_000_000)
        .await
        .expect("fund wallet ECC[2] SHELL");
    for _ in 0..20 {
        if be
            .client()
            .get_account(&wallet)
            .await
            .expect("wallet account")
            .map(|a| a.shell())
            .unwrap_or(0)
            >= SHELL_N100_RAW
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }

    let mut commit = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut commit);
    let commit_hex = bytes_to_hex(&commit);
    let ctx = local_context().expect("local tvm context");
    let payload = encode_internal_payload(
        &ctx,
        ROOTPN_ABI,
        "generateVoucher",
        json!({ "skUCommit": format!("0x{commit_hex}"), "isFee": false }),
    )
    .await
    .expect("encode RootPN.generateVoucher payload");
    let mut cc = serde_json::Map::new();
    cc.insert("2".to_string(), json!(SHELL_N100_RAW.to_string()));
    let msg = encode_external_call(
        &ctx,
        MULTISIG_ABI_JSON,
        &wallet.with_workchain(),
        "sendTransaction",
        json!({
            "dest": ROOTPN,
            "value": "2000000000",
            "cc": Value::Object(cc),
            "bounce": true,
            "flags": 1,
            "payload": payload,
        }),
        wallet_keys.public_hex(),
        wallet_keys.secret_hex(),
    )
    .await
    .expect("encode multisig sendTransaction");
    let submit = be
        .send_with_retry(&msg)
        .await
        .expect("submit wallet forward");
    println!("=== RootPN.generateVoucher via wallet sendTransaction submit = {submit} ===");

    let event = wait_voucher_extout(&be, &commit, std::time::Duration::from_secs(180))
        .await
        .expect("VoucherGenerated ext-out for skUCommit");
    println!("=== RootPN.VoucherGenerated found for skUCommit 0x{commit_hex}: {event} ===");
}

/// DIAGNOSTIC(live, read-only) for: the buyer never placed a buy. Reads the Fresh3 buyer + the
/// (working) seller note `getDetails()` -- `ephemeralPubkey`/`lockedInOrders`/`balance` -- to test the
/// `onlyOwnerPubkey(_ephemeralPubkey)`(ERR_INVALID_SENDER 101, dex table) / note-state asymmetry, and the IOB
/// `getBestBidAsk`/`getStats` to confirm whether the delivered seller ask actually rests. No key/write.
#[tokio::test]
#[ignore = "live (diag, read-only):  buyer-note getDetails + IOB resting ask"]
async fn diag_128_buyer_note_and_iob() {
    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let be = RealChainBackend::connect(manifest).expect("connect");
    for (lbl, addr) in [
        (
            "buyerF3",
            "0:fe1253f64e31ee5db207475235faf55d8a901462b02b0e1314ca4ff30c96b0c7",
        ),
        (
            "sellerF3(worked)",
            "0:2710e55bc91b882122d4622ef787feada60fbeee2cca6a107baf1dfd91e19d3a",
        ),
    ] {
        let a = Address::parse(addr).expect("note addr");
        let d = be
            .client()
            .run_getter(&a, PRIVATENOTE_ABI, "getDetails", json!({}))
            .await;
        println!("=== {lbl} {addr} getDetails = {d:?} ===");
    }
    let iob = Address::parse("0:050fe79be57746f4ffe959f61b951ebd1d44bf905e0435dbff3978268ae195c9")
        .expect("iob addr");
    let bba = be
        .client()
        .run_getter(&iob, INFERENCE_ORDERBOOK_ABI, "getBestBidAsk", json!({}))
        .await;
    let stats = be
        .client()
        .run_getter(&iob, INFERENCE_ORDERBOOK_ABI, "getStats", json!({}))
        .await;
    println!("=== IOB getBestBidAsk = {bba:?} ===");
    println!("=== IOB getStats = {stats:?} ===");
}

/// DIAGNOSTIC(live, read-only): the note's `ephemeralPubkey` -- the seller key the deployed verifier
/// uses BOTH for the TC derivation (`computeTokenContractAddressFromHash(..., _ephemeralPubkey, nonce)`)
/// AND for `postSellOffer` auth (`onlyOwnerPubkey(_ephemeralPubkey)`) -- vs the pool's owner pubkey.
/// If they differ, the TC must deploy with `_sellerPubkey = ephemeralPubkey` and postSellOffer must be
/// signed with the matching key -- using the owner pubkey would fail `ERR_BAD_TOKEN_CONTRACT`/auth.
#[tokio::test]
#[ignore = "live (diag, read-only): PrivateNote.getDetails().ephemeralPubkey vs owner pubkey"]
async fn diag_note_ephemeral_pubkey() {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set -- skipping");
        return;
    };
    let pool: Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let n0 = &pool["notes"].as_array().expect("notes")[0];
    let note_addr = Address::parse(n0["address"].as_str().expect("addr")).expect("addr");
    let owner_pub = n0["owner_public_key_hex"].as_str().expect("owner pub");

    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let be = RealChainBackend::connect(manifest).expect("connect");
    let details = be
        .client()
        .run_getter(&note_addr, PRIVATENOTE_ABI, "getDetails", json!({}))
        .await
        .expect("note.getDetails");
    let ephemeral = details
        .as_ref()
        .and_then(|d| d["ephemeralPubkey"].as_str())
        .unwrap_or("<none>");
    println!("=== note.ephemeralPubkey = {ephemeral} ===");
    println!("=== pool owner_public_key_hex = 0x{owner_pub} ===");
    println!(
        "=== MATCH = {} ===",
        ephemeral
            .trim_start_matches("0x")
            .eq_ignore_ascii_case(owner_pub)
    );
}

/// (live, read-only): the guard's behavior on a REAL on-chain note (`DEXDO_PN_POOL` note[0] -- the
/// conforming 4.0.11 capture `0:cc625238`). Proves the guard does NOT false-positive on a healthy note
/// (`getDetails().ephemeralPubkey` matches the owner key -> `note_owner_mismatch_reason` == None, so
/// `place_buy`/`post_offer` proceed and `onlyOwnerPubkey` would pass) AND fires fail-closed against a wrong
/// signing key(the orphaned/rotated-note case -> an actionable re-mint reason). The full gold-standard (a
/// live `placeInferenceBuy` committing past `onlyOwnerPubkey`) folds into the happy-path; this pins the
/// guard's two branches against live on-chain state. Read-only -- no giver, no deploy.
#[tokio::test]
#[ignore = "live (, read-only):  guard vs DEXDO_PN_POOL note[0] on-chain getDetails"]
async fn diag_128_guard_live_conforming_note() {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set -- skipping");
        return;
    };
    let pool: Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let n0 = &pool["notes"].as_array().expect("notes")[0];
    let note = Address::parse(n0["address"].as_str().expect("addr")).expect("addr");
    let owner_pub = n0["owner_public_key_hex"].as_str().expect("owner pub");

    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let be = RealChainBackend::connect(manifest).expect("connect");
    let details = be
        .client()
        .run_getter(&note, PRIVATENOTE_ABI, "getDetails", json!({}))
        .await
        .expect("note.getDetails")
        .expect("note active (getDetails non-empty)");
    let ephemeral = details["ephemeralPubkey"].as_str();
    println!(
        "=== note {note} on-chain ephemeralPubkey = {} ===",
        ephemeral.unwrap_or("<none>")
    );
    println!("=== pool owner_public_key_hex = 0x{owner_pub} ===");

    // (a) the guard PASSES on the real conforming note -- no false-positive; `onlyOwnerPubkey` would pass.
    let pass = note_owner_mismatch_reason("buyer place_buy", &note, ephemeral, owner_pub);
    println!("=== guard vs owner key (expect None/pass) = {pass:?} ===");
    assert!(
        pass.is_none(),
        "guard false-fired on the conforming note: {pass:?}"
    );

    // (b) the guard FIRES fail-closed against a wrong signing key(the orphaned/rotated-note case).
    let wrong = "deadbeef00000000000000000000000000000000000000000000000000000000";
    let fire = note_owner_mismatch_reason("buyer place_buy", &note, ephemeral, wrong)
        .expect("guard must fire on a mismatched signing key");
    println!("=== guard vs wrong key (expect Some/fire) = {fire} ===");
    assert!(
        fire.contains("Re-mint") && fire.contains("ERR_INVALID_SENDER 101"),
        "{fire}"
    );
}

/// a note minted JUST NOW by live `mint_pn_pool` (deployed
/// via RootPN `9ab11582`, 4.0.15) passes the re-pinned note-current guard. Point `DEXDO_PN_POOL` at
/// the fresh pool. Proves by-fact that the 4.0.15 re-pin accepts current-chain notes (the exact
/// `assert_seller_note_current` -> `note_code_hash_current` check that `dexdo provision` runs).
#[tokio::test]
#[ignore = "live (, read-only): a fresh 4.0.15 note passes the re-pinned note_code_hash_current"]
async fn diag_412_repin_fresh_note_passes() {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set -- skipping");
        return;
    };
    let pool: Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let note = Address::parse(
        pool["notes"].as_array().expect("notes")[0]["address"]
            .as_str()
            .expect("addr"),
    )
    .expect("parse fresh note address");
    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let be = RealChainBackend::connect(manifest).expect("connect");
    let acc = be
        .client()
        .get_account(&note)
        .await
        .expect("get_account")
        .expect("fresh note must be on-chain/active");
    let ch = acc.code_hash.as_deref().unwrap_or("<none>");
    println!(
        "=== fresh 4.0.15 note {note}: active={}, on-chain code_hash={ch} ===",
        acc.is_active()
    );
    println!("=== re-pinned PRIVATENOTE_PINNED_CODE_HASH = {PRIVATENOTE_PINNED_CODE_HASH} ===");
    let res = note_code_hash_current(&note, acc.code_hash.as_deref());
    println!("=== note_code_hash_current (4.0.15 re-pinned guard) => {res:?} ===");
    assert!(
        res.is_ok(),
        "fresh 4.0.15 note must PASS the re-pinned note-current guard: {res:?}"
    );
}

/// DIAGNOSTIC(offline): the actual `tvm.hash`(code_hash) of my embedded TC/IOB/PrivateNote TVCs vs
/// the `deployed.json` claim. The deployed note's verifier derives the deal address from its **pinned**
/// `TOKEN_CONTRACT_CODE_HASH`; if my embedded TC code hash differs, the address I deploy at can never
/// match -- that is the source/artifact mismatch.
#[test]
fn diag_embedded_code_hashes() {
    let tc = encode_hex(
        code_cell(TOKENCONTRACT_TVC)
            .expect("tc code cell")
            .repr_hash()
            .as_slice(),
    );
    let ob = encode_hex(
        code_cell(INFERENCE_ORDERBOOK_TVC)
            .expect("ob code cell")
            .repr_hash()
            .as_slice(),
    );
    let pn = encode_hex(
        code_cell(PRIVATENOTE_TVC)
            .expect("pn code cell")
            .repr_hash()
            .as_slice(),
    );
    println!("=== embedded TOKENCONTRACT code_hash    = {tc} ===");
    println!("=== embedded INFERENCE_ORDERBOOK code_hash = {ob} ===");
    println!("=== embedded PRIVATENOTE code_hash      = {pn} ===");
    println!("(4.0.6 deployed.json claims TC=2a1172e2, IOB=4fbb8caf, PrivateNote=c8a81f54)");
}

#[tokio::test]
#[ignore = "live: deploys a TokenContract + posts a sell offer on shellnet (real writes)"]
async fn live_seller_posts_offer() {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set -- skipping");
        return;
    };
    let pool: Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let n0 = &pool["notes"].as_array().expect("notes")[0];
    let note_addr = Address::parse(n0["address"].as_str().expect("addr")).expect("addr");
    let owner =
        KeyPair::from_secret_hex(n0["owner_secret_key_hex"].as_str().expect("sec")).expect("kp");

    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let be = RealChainBackend::connect(manifest).expect("connect");

    // The book(per-model) -- a deterministic address; we deploy it if it is not yet active.
    let frame_model = format!("dexdo-d-{}--book-redeploy", std::process::id());
    let model_hash = model_hash_for(&frame_model);
    let ob_tick_size: u128 = MODEL_TICK_SIZE;
    let ob = be
        .inference_orderbook_address(&note_addr, &model_hash, ob_tick_size)
        .await
        .expect("ob addr");
    let ob_active = be
        .client()
        .get_account(&ob)
        .await
        .expect("get ob")
        .map(|a| a.is_active())
        .unwrap_or(false);
    if !ob_active {
        be.deploy_inference_orderbook(&note_addr, &owner, &model_hash, &frame_model, ob_tick_size)
            .await
            .expect("deploy ob");
        for _ in 0..40 {
            if be
                .client()
                .get_account(&ob)
                .await
                .expect("ob")
                .map(|a| a.is_active())
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    }
    println!("=== OB {ob} active ===");

    // The seller is the model owner: their RootModel is derived from their pubkey. We provision it if absent.
    let seller_pubkey = json!(format!("0x{}", owner.public_hex()));
    let rm = be
        .root_model_address_for(&seller_pubkey)
        .await
        .expect("rm addr");
    let rm_active = be
        .client()
        .get_account(&rm)
        .await
        .expect("get rm")
        .map(|a| a.is_active())
        .unwrap_or(false);
    println!("=== seller RootModel {rm} active={rm_active} ===");
    if !rm_active {
        let deployed_rm = be
            .deploy_root_model(&owner)
            .await
            .expect("deploy RootModel");
        assert_eq!(
            deployed_rm.with_workchain(),
            rm.with_workchain(),
            "RootModel address == getRootModelAddress derivation"
        );
        println!("=== seller RootModel deployed+active = {deployed_rm} ===");
    }

    // Derive the TC address from(RootModel, sellerPubkey, nonce) -- cross-check against build_deploy.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let derived = be
        .resolve_token_contract(&rm, &seller_pubkey, nonce)
        .await
        .expect("derive TC");
    println!("=== TC derived (RootModel) = {derived} (nonce={nonce}) ===");

    // The seller provisions the TC(build_deploy + giver-fund + submit + wait for Active), passing the
    // SAME `frame_model` as the book so the TC's `getModelHash()` == the book `model_hash` (4.0.6 ctor
    // requires `sha256(modelName) == modelHash`). On 4.0.6 the IOB DOES re-derive the canonical TC from
    // `(sellerPubkey, nonce)` and rejects a non-canonical `tokenContract`, so the offer only rests with
    // the RootModel-backed canonical TC.
    let deal_price: u128 = 10;
    let deal_max_ticks: u128 = 10;
    let tc = be
        .deploy_token_contract(
            &owner,
            &rm,
            nonce,
            &frame_model,
            1,
            deal_price,
            deal_max_ticks,
            &note_addr,
        )
        .await
        .expect("deploy TC");
    println!("=== TC deployed+active = {tc} ===");
    assert_eq!(
        tc.with_workchain(),
        derived.with_workchain(),
        "build_deploy address == RootModel derivation (the deal address converges)"
    );

    // 4.0.6 model-name invariant: the deployed TC must report the SAME model as the order book -- i.e. the
    // TC was provisioned with the book's `frame_model`, so `getModelHash() == sha256(frame_model) == model_hash`.
    let tc_model_hash = be
        .token_contract_model_hash(&tc)
        .await
        .expect("TC getModelHash")
        .expect("TC has a modelHash");
    assert_eq!(
        tc_model_hash,
        model_hash.to_lowercase(),
        "4.0.6 model-name invariant: deployed TC getModelHash() must equal the book model_hash"
    );
    println!("=== TC getModelHash() == book model_hash {model_hash} -- 4.0.6 model identity consistent ===");

    // The seller posts the offer into the book(a note write via submit).
    let resp = be
        .post_sell_offer(
            &note_addr,
            &owner,
            &model_hash,
            deal_price,
            deal_max_ticks,
            &tc,
            0,
            nonce,
        )
        .await
        .expect("post sell offer");
    println!("=== postSellOffer resp = {resp} ===");

    // The offer must land in the order book: orderCount>=1(or hasAsk).
    let mut landed = false;
    for _ in 0..20 {
        let stats = be.inference_orderbook_stats(&ob).await.expect("stats");
        let bba = be.inference_orderbook_best_bid_ask(&ob).await.expect("bba");
        println!("stats={stats:?} bba={bba:?}");
        let oc = stats
            .as_ref()
            .and_then(|s| s["orderCount"].as_str())
            .and_then(|x| x.parse::<u128>().ok())
            .unwrap_or(0);
        if oc >= 1 {
            landed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(
        landed,
        "the offer actually landed in the on-chain book (orderCount>=1)"
    );
}

/// ADVERSARIAL. A seller
/// posts an offer whose `tokenContract` is NOT their canonical TC(an attacker-controlled address). The
/// 4.0.6 IOB(`4fbb8caf`) enforces `require(tokenContract == _tokenContractAddr(sellerPubkey, nonce))`
/// derived from the seller's REAL RootModel, so a non-canonical TC is REJECTED: the note's `postSellOffer`
/// succeeds(exit_code 0, the note forwards the call) but the IOB never rests the ask(orderCount stays 0).
/// This is the live proof that the PR-escalated 4.0.5 gap(deployed `36404a04` ACCEPTED the fake TC) is
/// CLOSED on 4.0.6 -- buyer SHELL can never be routed to a non-canonical tokenContract.
#[tokio::test]
#[ignore = "live (adversarial): 4.0.6 IOB REJECTS a non-canonical tokenContract (no ask rests)"]
async fn live_offer_fake_tc_rejected_by_iob_4_0_6() {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set -- skipping");
        return;
    };
    let pool: Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let n0 = &pool["notes"].as_array().expect("notes")[0];
    let note_addr = Address::parse(n0["address"].as_str().expect("addr")).expect("addr");
    let owner =
        KeyPair::from_secret_hex(n0["owner_secret_key_hex"].as_str().expect("sec")).expect("kp");
    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let be = RealChainBackend::connect(manifest).expect("connect");

    // Fresh per-run OB(unique model hash) -- an empty book, so orderCount is unambiguous.
    let frame = format!("dexdo-fake-tc-{}", std::process::id());
    let model_hash = model_hash_for(&frame);
    let ob = be
        .inference_orderbook_address(&note_addr, &model_hash, MODEL_TICK_SIZE)
        .await
        .expect("ob addr");
    if !be
        .client()
        .get_account(&ob)
        .await
        .expect("get ob")
        .map(|a| a.is_active())
        .unwrap_or(false)
    {
        be.deploy_inference_orderbook(&note_addr, &owner, &model_hash, &frame, MODEL_TICK_SIZE)
            .await
            .expect("deploy ob");
        for _ in 0..40 {
            if be
                .client()
                .get_account(&ob)
                .await
                .expect("ob")
                .map(|a| a.is_active())
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    }
    println!("=== fresh OB {ob} active ===");

    // The seller's canonical TC(derive only, for comparison).
    let seller_pubkey = json!(format!("0x{}", owner.public_hex()));
    let rm = be
        .root_model_address_for(&seller_pubkey)
        .await
        .expect("rm addr");
    // 4.0.6: the IOB derives the canonical TC from the seller's REAL RootModel, so it must be active for
    // the on-chain canonical-TC check to run(and for the canonical comparison below). Provision if absent.
    if !be
        .client()
        .get_account(&rm)
        .await
        .expect("get rm")
        .map(|a| a.is_active())
        .unwrap_or(false)
    {
        be.deploy_root_model(&owner)
            .await
            .expect("deploy RootModel");
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let canonical_tc = be
        .resolve_token_contract(&rm, &seller_pubkey, nonce)
        .await
        .expect("derive canonical TC");

    // The FAKE tokenContract: a fixed attacker address, NOT the seller's canonical TC.
    let fake_tc =
        Address::parse("0:dead00000000000000000000000000000000000000000000000000000000beef")
            .expect("fake tc");
    assert_ne!(
        fake_tc.with_workchain(),
        canonical_tc.with_workchain(),
        "the fake tc must differ from the canonical TC"
    );
    println!(
        "=== posting offer with FAKE tokenContract {fake_tc} (canonical = {canonical_tc}) ==="
    );

    let resp = be
        .post_sell_offer(&note_addr, &owner, &model_hash, 10, 10, &fake_tc, 0, nonce)
        .await;
    println!("=== post_sell_offer(fake tc) result = {resp:?} ===");

    // Does the fake-tc offer rest in the book?
    let mut rested = false;
    for _ in 0..20 {
        let stats = be.inference_orderbook_stats(&ob).await.expect("stats");
        let oc = stats
            .as_ref()
            .and_then(|s| s["orderCount"].as_str())
            .and_then(|x| x.parse::<u128>().ok())
            .unwrap_or(0);
        println!("stats={stats:?}");
        if oc >= 1 {
            let order = be.inference_orderbook_order(&ob, 1).await.expect("order");
            println!("=== fake-tc offer RESTED -- order = {order:?} ===");
            rested = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }

    // 4.0.6 SECURE INVARIANT(live-proven): the on-chain IOB(`4fbb8caf`) ENFORCES the canonical-TC check --
    // `placeSellOffer` requires `tokenContract == _tokenContractAddr(sellerPubkey, nonce)` derived from the
    // seller's REAL RootModel under this SuperRoot. A non-canonical(fake) `tokenContract` is REJECTED: the
    // note's `postSellOffer` succeeds(exit_code 0, the note forwards the call) but the IOB never rests the
    // ask(orderCount stays 0). This closes the PR-escalated 4.0.5 gap (deployed `36404a04` accepted the
    // fake TC); buyer SHELL can never be routed to a non-canonical tokenContract.
    assert!(
        !rested,
        "4.0.6 canonical-TC invariant: a non-canonical tokenContract MUST be rejected (no ask rests). If \
         this fails, the IOB has stopped enforcing `tokenContract == _tokenContractAddr(sellerPubkey, nonce)`."
    );
    println!(
        "=== 4.0.6 SECURE: the non-canonical tokenContract was REJECTED by the IOB -- no ask rested (PR gap closed) ==="
    );

    // do NOT add a buyer-side shared-book scan that compares every ask to one expected TC. A shared
    // book legitimately contains many sellers and many canonical TCs; the invariant belongs at
    // `placeSellOffer`(proved above). The buyer happy-path -- a canonical ask, escrow proceeds -- is covered
    // by `live_buyer_matches_offer`. `canonical_tc` is exercised by the `assert_ne!` above(fake != canonical).
}

/// **LEGACY.** Exercises the OLD operator-wallet/giver provisioning path, kept
/// as `test-giver` regression coverage -- **NOT** the canonical proof. The canonical note-funded seller
/// proof is [`provision_market`](Self::provision_market) + `live_post_probe_commission_note_funded` + the
/// note-funded `live_cli` harness.
/// A LIVE test of the issue operator path: provision a per-deal market -- OB(note-funded) +
/// RootModel + `TokenContract`(**wallet-funded**, no giver) -- and assemble a `MarketManifest`.
/// The operator multisig is funded here by the giver to stand in for the operator topping up their
/// own wallet externally; the deploys themselves use `*_from_wallet`, never the giver. This verifies
/// the `sendTransaction` funding ABI of [`RealChainBackend::fund_deploy_from_wallet`].
#[tokio::test]
#[ignore = "live:  wallet-funded market provisioning on shellnet (writes; deploys OB/RM/TC)"]
async fn live_provision_wallet_funded_market() {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set -- skipping");
        return;
    };
    let pool: Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let n0 = &pool["notes"].as_array().expect("notes")[0];
    let note_addr = Address::parse(n0["address"].as_str().expect("addr")).expect("addr");
    let owner =
        KeyPair::from_secret_hex(n0["owner_secret_key_hex"].as_str().expect("sec")).expect("kp");

    let manifest_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let be = RealChainBackend::connect(manifest_path).expect("connect");

    // Operator wallet. In production the operator funds it
    // externally; here the giver stands in for that top-up(native gas, to pay the deploys).
    let wallet = RealChainBackend::multisig_address(&owner).expect("wallet addr");
    be.giver_fund(&wallet.with_workchain(), 800_000_000_000)
        .await
        .expect("giver -> operator wallet (test stand-in for external top-up)");
    let wallet_active = be
        .client()
        .get_account(&wallet)
        .await
        .expect("get wallet")
        .map(|a| a.is_active())
        .unwrap_or(false);
    if !wallet_active {
        let w = be
            .deploy_multisig_self_funded(&owner)
            .await
            .expect("deploy multisig (self-funded)");
        assert_eq!(w.with_workchain(), wallet.with_workchain());
    }
    println!("=== operator wallet {wallet} active ===");

    // (4.0.5): the self-dapp RootModel + per-deal TC deploys are funded with ECC[2] SHELL from the
    // operator wallet(native to an uninit cross-dapp address needs the giver). Seed the wallet's ECC[2]
    // balance.
    be.giver_send_shell(&wallet.with_workchain(), 5_000_000_000_000)
        .await
        .expect("seed operator wallet ECC[2] SHELL");

    // A unique frame model per run -> a fresh OB/RootModel/TC(deterministic, no leftover state).
    let frame_model = format!("dexdo-d24-{}", std::process::id());
    let model_hash = model_hash_for(&frame_model);
    let gas: u128 = 200_000_000_000;

    // 1) OB(note-funded, no giver) -- deploy-if-absent(note-funded deploy does not wait).
    let ob = be
        .inference_orderbook_address(&note_addr, &model_hash, MODEL_TICK_SIZE)
        .await
        .expect("ob addr");
    if !be
        .client()
        .get_account(&ob)
        .await
        .expect("ob")
        .map(|a| a.is_active())
        .unwrap_or(false)
    {
        be.deploy_inference_orderbook(
            &note_addr,
            &owner,
            &model_hash,
            &frame_model,
            MODEL_TICK_SIZE,
        )
        .await
        .expect("deploy OB");
        let mut active = false;
        for _ in 0..40 {
            if be
                .client()
                .get_account(&ob)
                .await
                .expect("ob")
                .map(|a| a.is_active())
                .unwrap_or(false)
            {
                active = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        assert!(active, "OB activated");
    }
    println!("=== OB {ob} active (note-funded) ===");

    // 2) RootModel(WALLET-funded) -- deploy-if-absent. Verifies fund_deploy_from_wallet.
    let seller_pubkey = json!(format!("0x{}", owner.public_hex()));
    let rm = be
        .root_model_address_for(&seller_pubkey)
        .await
        .expect("rm addr");
    if !be
        .client()
        .get_account(&rm)
        .await
        .expect("rm")
        .map(|a| a.is_active())
        .unwrap_or(false)
    {
        let deployed = be
            .deploy_root_model_from_wallet(&owner, &wallet, &owner, gas)
            .await
            .expect("deploy RootModel from wallet");
        assert_eq!(
            deployed.with_workchain(),
            rm.with_workchain(),
            "RootModel addr == derivation"
        );
    }
    println!("=== RootModel {rm} active (wallet-funded) ===");

    // 3) Per-deal TokenContract address -- derived from RootModel(`getTokenContractAddress`).
    // FINDING: the wallet-funded TC *deploy* is blocked by dapp isolation. The TC is a
    // self-dapp deal contract; the operator multisig's `sendTransaction` funds same-dapp (RootModel
    // + above) but NOT the cross-dapp TC, so the deploy never activates. The giver works only because
    // it is privileged(`fund_deploy_address` routes cross-dapp), and RootModel has no
    // The per-deal TC is deployed by `provision_market()` ITSELF(ECC[2] wallet-funded, NO giver) --
    // see below. Here we only derive the expected RootModel-backed address to assert convergence.
    let nonce = std::process::id() as u64;
    let (price, max_ticks): (u128, u128) = (1000, 1024);
    let tc = be
        .resolve_token_contract(&rm, &seller_pubkey, nonce)
        .await
        .expect("derive TC address");

    // 4) The provision_market orchestrator(the actual `dexdo provision` code path) provisions the FULL
    // market -- deploy-if-absent the OB/RootModel(already up) AND the per-deal TC (ECC[2] wallet-funded,
    // NO giver) -- and assembles the MarketManifest. This verifies the shipped orchestrator deploys an
    // ACTIVE TC end-to-end, not just a derived address.
    let m = be
        .provision_market(
            &owner,
            &note_addr,
            &frame_model,
            nonce,
            price,
            max_ticks,
            gas,
        )
        .await
        .expect("provision_market");
    assert_eq!(m.inference_order_book, ob.with_workchain(), "OB");
    assert_eq!(m.root_model, rm.with_workchain(), "RootModel");
    assert_eq!(m.token_contract, tc.with_workchain(), "TC derived");
    // provision_market DEPLOYED the TC(ECC[2] wallet-funded, NO giver) -- assert it is active on-chain,
    // i.e. the orchestrator returns a TC-active market, not just a derived address.
    let tc_active = be
        .client()
        .get_account(&tc)
        .await
        .expect("get TC")
        .map(|a| a.is_active())
        .unwrap_or(false);
    assert!(
        tc_active,
        "provision_market deployed the per-deal TC ACTIVE (wallet ECC[2]-funded, NO giver)"
    );
    println!("=== TC {tc} ACTIVE via provision_market (wallet ECC[2], NO giver) ===");
    assert_eq!(m.frame_model, frame_model);
    // provision_market is note-funded -- `MarketManifest` no longer carries an operator wallet.
    println!(
        "=== provision_market MarketManifest:\n{} ===",
        m.to_json().unwrap()
    );
}

/// A LIVE deal test, step 3: the buyer(note1) **matches the seller's offer** in the book. Uses
/// a fresh `modelHash`(-> an empty book for the run), so the match is deterministic (the buy takes
/// exactly our ask, not a leftover from past runs). Match criterion: the book funded the
/// seller `TokenContract` via `fundFromOrderBook` -> `getState.funded == true`. Requires `DEXDO_PN_POOL`.
#[tokio::test]
#[ignore = "live: buyer matches a resting offer on shellnet (writes; match funds seller TC)"]
async fn live_buyer_matches_offer() {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set -- skipping");
        return;
    };
    let pool: Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let notes = pool["notes"].as_array().expect("notes");
    let seller_addr = Address::parse(notes[0]["address"].as_str().expect("a0")).expect("a0");
    let seller = KeyPair::from_secret_hex(notes[0]["owner_secret_key_hex"].as_str().expect("s0"))
        .expect("k0");
    let buyer_addr = Address::parse(notes[1]["address"].as_str().expect("a1")).expect("a1");
    let buyer = KeyPair::from_secret_hex(notes[1]["owner_secret_key_hex"].as_str().expect("s1"))
        .expect("k1");

    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let be = RealChainBackend::connect(manifest).expect("connect");

    // A unique nonce -> a fresh modelHash(empty book) and a fresh TC(deterministic match).
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let frame_model = format!("dexdo-d-stream--{nonce:016x}");
    let model_hash = model_hash_for(&frame_model);
    let ob_tick_size: u128 = MODEL_TICK_SIZE;
    let price: u128 = 10;
    let max_ticks: u128 = 2;

    // A fresh book -- we deploy it and wait for Active.
    let ob = be
        .inference_orderbook_address(&seller_addr, &model_hash, ob_tick_size)
        .await
        .expect("ob addr");
    be.deploy_inference_orderbook(
        &seller_addr,
        &seller,
        &model_hash,
        &frame_model,
        ob_tick_size,
    )
    .await
    .expect("deploy ob");
    let mut ob_ok = false;
    for _ in 0..40 {
        if be
            .client()
            .get_account(&ob)
            .await
            .expect("ob")
            .map(|a| a.is_active())
            .unwrap_or(false)
        {
            ob_ok = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(ob_ok, "the fresh book is active");
    println!("=== fresh OB {ob} active (model={model_hash}) ===");

    // Seller RootModel(we provision it if absent) + a fresh per-deal TC.
    let seller_pubkey = json!(format!("0x{}", seller.public_hex()));
    let rm = be.root_model_address_for(&seller_pubkey).await.expect("rm");
    let rm_active = be
        .client()
        .get_account(&rm)
        .await
        .expect("rm acc")
        .map(|a| a.is_active())
        .unwrap_or(false);
    if !rm_active {
        be.deploy_root_model(&seller).await.expect("deploy rm");
    }
    let tc = be
        .deploy_token_contract(
            &seller,
            &rm,
            nonce,
            &frame_model,
            1,
            price,
            max_ticks,
            &seller_addr,
        )
        .await
        .expect("deploy tc");
    println!("=== seller TC {tc} active ===");

    // The seller posts the offer; we wait until it lands in the book as an ask.
    be.post_sell_offer(
        &seller_addr,
        &seller,
        &model_hash,
        price,
        max_ticks,
        &tc,
        0,
        nonce,
    )
    .await
    .expect("offer");
    let mut rested = false;
    for _ in 0..20 {
        let stats = be.inference_orderbook_stats(&ob).await.expect("stats");
        let oc = stats
            .as_ref()
            .and_then(|s| s["orderCount"].as_str())
            .and_then(|x| x.parse::<u128>().ok())
            .unwrap_or(0);
        if oc >= 1 {
            rested = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(rested, "the offer landed in the book before the match");
    println!("=== offer rests; buyer now matches ===");

    // The buyer matches: maxPrice >= ask, ticks <= maxTicks, a generous escrow(ECC SHELL from note1's balance).
    let ticks: u128 = max_ticks;
    let escrow: u128 = 1_000_000;
    let resp = be
        .place_inference_buy(&buyer_addr, &buyer, &model_hash, price, ticks, escrow, 0, 0)
        .await
        .expect("buy");
    println!("=== placeInferenceBuy resp = {resp} ===");

    // Match criterion: the book funded the seller TC(getState.funded == true).
    let mut funded = false;
    for _ in 0..30 {
        let st = be.token_contract_state(&tc).await.expect("tc state");
        let stats = be.inference_orderbook_stats(&ob).await.expect("stats");
        println!("TC state={st:?} stats={stats:?}");
        if st
            .as_ref()
            .and_then(|s| s["funded"].as_bool())
            .unwrap_or(false)
        {
            funded = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(
        funded,
        "the match happened: the book funded the seller TC (getState.funded==true)"
    );
}

/// LIVE: buyer funds a fresh seller TC through the book, the seller never opens it, then after
/// MATCH_OPEN_TIMEOUT the buyer note sends `streamCleanup` -> `TC.cleanupUnopened()`. This is the
/// funded-but-never-opened money proof: the deal closes and the buyer's ECC[2] escrow is no longer locked.
#[tokio::test]
#[ignore = "live : waits MATCH_OPEN_TIMEOUT (~600s) then streamCleanup refunds a never-opened deal"]
async fn live_never_opened_cleanup_refunds_buyer() {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set -- skipping");
        return;
    };
    let pool: Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let notes = pool["notes"].as_array().expect("notes");
    let seller_addr = Address::parse(notes[0]["address"].as_str().expect("a0")).expect("a0");
    let seller = KeyPair::from_secret_hex(notes[0]["owner_secret_key_hex"].as_str().expect("s0"))
        .expect("k0");
    let buyer_addr = Address::parse(notes[1]["address"].as_str().expect("a1")).expect("a1");
    let buyer = KeyPair::from_secret_hex(notes[1]["owner_secret_key_hex"].as_str().expect("s1"))
        .expect("k1");

    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let be = RealChainBackend::connect(manifest).expect("connect");

    let nonce = now_secs();
    let frame_model = format!("dexdo-d149-cleanup--{nonce:016x}");
    let model_hash = model_hash_for(&frame_model);
    let price: u128 = 10;
    let max_ticks: u128 = 5;
    let gas: u128 = 9_000_000_000;
    let market = be
        .provision_market(
            &seller,
            &seller_addr,
            &frame_model,
            nonce,
            price,
            max_ticks,
            gas,
        )
        .await
        .expect("note-funded provision_market");
    assert_eq!(market.model_hash, model_hash);
    let ob = Address::parse(&market.inference_order_book).expect("ob addr");
    let tc = Address::parse(&market.token_contract).expect("tc addr");
    println!("===  fresh market via note-funded provision_market: OB {ob} TC {tc} ===");

    be.post_sell_offer(
        &seller_addr,
        &seller,
        &model_hash,
        price,
        max_ticks,
        &tc,
        0,
        nonce,
    )
    .await
    .expect("offer");
    let mut rested = false;
    for _ in 0..20 {
        let order_count = be
            .inference_orderbook_stats(&ob)
            .await
            .expect("stats")
            .as_ref()
            .and_then(|s| s["orderCount"].as_str())
            .and_then(|x| x.parse::<u128>().ok())
            .unwrap_or(0);
        if order_count >= 1 {
            rested = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(rested, " offer rests before buyer match");

    let ticks: u128 = 2;
    let escrow: u128 = 1_000_000;
    let resp = be
        .place_inference_buy(&buyer_addr, &buyer, &model_hash, price, ticks, escrow, 0, 0)
        .await
        .expect("buy");
    println!("===  placeInferenceBuy resp = {resp} ===");

    let mut funded_time = None;
    for _ in 0..40 {
        let st = be.token_contract_state(&tc).await.expect("tc state");
        println!("===  pre-cleanup TC state={st:?} ===");
        if let Some(st) = st.as_ref() {
            let funded = st["funded"].as_bool().unwrap_or(false);
            let opened = st["opened"].as_bool().unwrap_or(false);
            if funded {
                assert!(
                    !opened,
                    "seller must not open in the  never-opened scenario"
                );
                funded_time = st["fundedTime"]
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok());
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    let funded_time = funded_time.expect("match funded the TC and getState exposed fundedTime");
    let cleanup_at = funded_time.saturating_add(crate::chain::MATCH_OPEN_TIMEOUT_SECS);
    let now = now_secs();
    if now < cleanup_at {
        let wait = cleanup_at - now + 3;
        println!(
            "===  waiting {wait}s for MATCH_OPEN_TIMEOUT: fundedTime={funded_time} cleanup_at={cleanup_at} now={now} ==="
        );
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
    }

    be.stream_cleanup(&buyer_addr, &buyer, &tc)
        .await
        .expect("streamCleanup");
    for _ in 0..40 {
        let st = be.token_contract_state(&tc).await.expect("tc state");
        println!("===  post-cleanup TC state={st:?} ===");
        let Some(st) = st.as_ref() else {
            println!("===  cleanup applied: TokenContract closed (state=None) ===");
            return;
        };
        let funded = st["funded"].as_bool().unwrap_or(false);
        let opened = st["opened"].as_bool().unwrap_or(false);
        let deposit = st["deposit"]
            .as_str()
            .and_then(|s| s.parse::<u128>().ok())
            .unwrap_or(0);
        if !funded && !opened && deposit == 0 {
            println!("===  cleanup applied: funded=false opened=false deposit=0 ===");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    panic!(" streamCleanup did not close/refund the never-opened TC");
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

async fn is_active(be: &RealChainBackend, addr: &Address) -> bool {
    be.client()
        .get_account(addr)
        .await
        .expect("get_account")
        .map(|a| a.is_active())
        .unwrap_or(false)
}

async fn wait_active(be: &RealChainBackend, addr: &Address) {
    for _ in 0..40 {
        if is_active(be, addr).await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    panic!("{addr} did not activate");
}

/// Provisions a fresh deal up to the "TC funded" state(steps 1-3): a fresh book +
/// the seller's RootModel + a per-deal TC + an offer + a buyer match. Returns(ob, tc).
#[allow(clippy::too_many_arguments)]
async fn setup_funded_deal(
    be: &RealChainBackend,
    seller_addr: &Address,
    seller: &KeyPair,
    buyer_addr: &Address,
    buyer: &KeyPair,
    price: u128,
    max_ticks: u128,
    ticks: u128,
    escrow: u128,
) -> (Address, Address) {
    let nonce = now_secs();
    let frame_model = format!("dexdo-d-deal--{nonce:016x}");
    let model_hash = model_hash_for(&frame_model);
    let ob = be
        .inference_orderbook_address(seller_addr, &model_hash, 1000)
        .await
        .expect("ob addr");
    be.deploy_inference_orderbook(seller_addr, seller, &model_hash, &frame_model, 1000)
        .await
        .expect("deploy ob");
    wait_active(be, &ob).await;
    let seller_pubkey = json!(format!("0x{}", seller.public_hex()));
    let rm = be.root_model_address_for(&seller_pubkey).await.expect("rm");
    if !is_active(be, &rm).await {
        be.deploy_root_model(seller).await.expect("deploy rm");
    }
    let tc = be
        .deploy_token_contract(
            seller,
            &rm,
            nonce,
            &frame_model,
            1,
            price,
            max_ticks,
            seller_addr,
        )
        .await
        .expect("deploy tc");
    be.post_sell_offer(
        seller_addr,
        seller,
        &model_hash,
        price,
        max_ticks,
        &tc,
        0,
        nonce,
    )
    .await
    .expect("offer");
    let mut rested = false;
    for _ in 0..20 {
        let oc = be
            .inference_orderbook_stats(&ob)
            .await
            .expect("stats")
            .as_ref()
            .and_then(|s| s["orderCount"].as_str())
            .and_then(|x| x.parse::<u128>().ok())
            .unwrap_or(0);
        if oc >= 1 {
            rested = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(rested, "the offer landed in the book");
    be.place_inference_buy(buyer_addr, buyer, &model_hash, price, ticks, escrow, 0, 0)
        .await
        .expect("buy");
    let mut funded = false;
    for _ in 0..30 {
        if be
            .token_contract_state(&tc)
            .await
            .expect("st")
            .as_ref()
            .and_then(|s| s["funded"].as_bool())
            .unwrap_or(false)
        {
            funded = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(funded, "the TC is funded by the match");
    (ob, tc)
}

/// **LEGACY.** Its deal SETUP routes through the OLD operator-wallet/giver path
/// (`deploy_multisig` + `fund_probe_commission`), kept as `test-giver` regression coverage -- **NOT** the
/// canonical proof. The canonical note-funded probe-commission proof is
/// `live_post_probe_commission_note_funded`(and `RealDealBackend::open_stream` itself is note-funded now).
/// A LIVE deal test, step 4(open + stop on the probe): from a funded deal the seller
/// posts the probe-commission(via the operational wallet) and opens the stream `open(endpointCipher)`;
/// the handover **round-trips through the chain**(the buyer decrypts the endpoint); then the buyer
/// **stops on the probe** `streamStop` -> `ProbeBurned`. Requires `DEXDO_PN_POOL`.
#[tokio::test]
#[ignore = "live: stream open + handover + buyer stop-on-probe (ProbeBurned) on shellnet"]
async fn live_stream_open_and_probe_burn() {
    use crate::note::Note;
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set -- skipping");
        return;
    };
    let pool: Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let notes = pool["notes"].as_array().expect("notes");
    let s_sec = notes[0]["owner_secret_key_hex"].as_str().expect("s0");
    let b_sec = notes[1]["owner_secret_key_hex"].as_str().expect("s1");
    let seller_addr = Address::parse(notes[0]["address"].as_str().expect("a0")).expect("a0");
    let seller = KeyPair::from_secret_hex(s_sec).expect("k0");
    let buyer_addr = Address::parse(notes[1]["address"].as_str().expect("a1")).expect("a1");
    let buyer = KeyPair::from_secret_hex(b_sec).expect("k1");

    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let be = RealChainBackend::connect(manifest).expect("connect");

    // A deal with a NON-ZERO probe commission: price=10000 -> commission=250(= price*250bps).
    let price: u128 = 10_000;
    let (_ob, tc) = setup_funded_deal(
        &be,
        &seller_addr,
        &seller,
        &buyer_addr,
        &buyer,
        price,
        5,
        2,
        100_000_000,
    )
    .await;
    println!("=== funded TC {tc} (price={price}) ===");

    // The seller posts the probe-commission from the operational wallet(an internal call with SHELL ECC).
    let wallet_keys = KeyPair::generate();
    let wallet = be
        .deploy_multisig(&wallet_keys)
        .await
        .expect("deploy wallet");
    // The wallet needs ECC[2] SHELL to attach to `fundProbeCommission` -- we top it up from the
    // giver(flag 1; deploy-funding only gives native gas).
    be.giver_send_shell(&wallet.with_workchain(), 100_000_000)
        .await
        .expect("send shell to wallet");
    let mut wshell = 0u128;
    for _ in 0..20 {
        wshell = be
            .client()
            .get_account(&wallet)
            .await
            .expect("wacc")
            .map(|a| a.shell())
            .unwrap_or(0);
        if wshell > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    println!("=== seller wallet {wallet} active; ecc[2] SHELL = {wshell} ===");
    assert!(
        wshell > 0,
        "the wallet received ECC[2] SHELL from the giver"
    );
    let fresp = be
        .fund_probe_commission(&wallet, &wallet_keys, &tc, 1_000_000)
        .await
        .expect("fund probe");
    println!("=== fund_probe_commission resp = {fresp} ===");
    let mut probe_funded = false;
    for _ in 0..20 {
        let p = be.token_contract_probe(&tc).await.expect("probe");
        println!("probe={p:?}");
        if p.as_ref()
            .and_then(|x| x["probeFunded"].as_bool())
            .unwrap_or(false)
        {
            probe_funded = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(
        probe_funded,
        "the probe-commission is posted (probeFunded==true)"
    );

    // Handover: the seller encrypts the endpoint to the buyer's x25519 pubkey; open writes the cipher.
    let buyer_rn = RealNote::from_keypair(KeyPair::from_secret_hex(b_sec).expect("brn"))
        .expect("buyer real note");
    let seller_rn = RealNote::from_keypair(KeyPair::from_secret_hex(s_sec).expect("srn"))
        .expect("seller real note");
    let endpoint = b"https://seller.example/v1|fingerprint";
    let cipher = seller_rn.encrypt_to(&buyer_rn.pubkey(), endpoint);
    be.open_stream(&tc, &seller, &cipher).await.expect("open");
    let mut opened = false;
    for _ in 0..20 {
        if be
            .token_contract_state(&tc)
            .await
            .expect("st")
            .as_ref()
            .and_then(|s| s["opened"].as_bool())
            .unwrap_or(false)
        {
            opened = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(opened, "the stream is open (opened==true)");

    // The handover is actually on the chain: we read the cipher, the buyer decrypts it into the endpoint.
    let onchain = be
        .read_handover(&tc)
        .await
        .expect("read handover")
        .expect("cipher present");
    assert_eq!(
        buyer_rn.decrypt(&onchain).expect("decrypt"),
        endpoint,
        "the handover round-trips through the chain -- the buyer decrypted the endpoint"
    );
    println!("=== handover round-trips through chain ===");

    // The buyer stops ON THE PROBE(before accept): stop -> ProbeBurned.
    be.stream_stop(&buyer_addr, &buyer, &tc)
        .await
        .expect("stop");
    let mut closed = false;
    for _ in 0..20 {
        let st = be.token_contract_state(&tc).await.expect("st");
        let pr = be.token_contract_probe(&tc).await.expect("pr");
        println!("after stop: state={st:?} probe={pr:?}");
        if st
            .as_ref()
            .and_then(|s| s["opened"].as_bool())
            .map(|o| !o)
            .unwrap_or(false)
        {
            closed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(
        closed,
        "stop on the probe: the stream is closed (opened==false), the probe is burned (ProbeBurned)"
    );
}

/// A LIVE anti-scam test of: from an open stream the buyer opens a dispute
/// `streamDispute` -> `TC.dispute()` -> **both notes are actually locked**. We check `disputed==true`
/// on the TC and `disputeCount>0` on the seller's note -- direct on-chain proof of `ERR_STREAM_LOCKED`
/// (a new offer/withdrawal from a locked note is rejected until the dispute is resolved). Requires `DEXDO_PN_POOL`.
#[tokio::test]
#[ignore = "live: buyer streamDispute -> TC.dispute() locks both notes (disputeCount>0) on shellnet"]
async fn live_stream_dispute_locks_note() {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set -- skipping");
        return;
    };
    let pool: Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let notes = pool["notes"].as_array().expect("notes");
    let s_sec = notes[0]["owner_secret_key_hex"].as_str().expect("s0");
    let b_sec = notes[1]["owner_secret_key_hex"].as_str().expect("s1");
    let seller_addr = Address::parse(notes[0]["address"].as_str().expect("a0")).expect("a0");
    let seller = KeyPair::from_secret_hex(s_sec).expect("k0");
    let buyer_addr = Address::parse(notes[1]["address"].as_str().expect("a1")).expect("a1");
    let buyer = KeyPair::from_secret_hex(b_sec).expect("k1");

    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let be = RealChainBackend::connect(manifest).expect("connect");

    let price: u128 = 10_000;
    let (_ob, tc) = setup_funded_deal(
        &be,
        &seller_addr,
        &seller,
        &buyer_addr,
        &buyer,
        price,
        5,
        2,
        100_000_000,
    )
    .await;
    println!("=== funded TC {tc} (price={price}) ===");

    open_stream_with_probe(&be, &tc, &seller, s_sec, b_sec).await;
    assert!(
        be.token_contract_state(&tc)
            .await
            .expect("st")
            .as_ref()
            .and_then(|s| s["opened"].as_bool())
            .unwrap_or(false),
        "the stream is open before the dispute"
    );

    // The buyer opens a dispute: streamDispute -> TC.dispute() locks BOTH notes.
    be.stream_dispute(&buyer_addr, &buyer, &tc)
        .await
        .expect("dispute");
    let mut disputed = false;
    for _ in 0..20 {
        let st = be.token_contract_state(&tc).await.expect("st");
        println!("after dispute: state={st:?}");
        if st
            .as_ref()
            .and_then(|s| s["disputed"].as_bool())
            .unwrap_or(false)
        {
            disputed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(disputed, "TC.dispute() went through: disputed==true");

    // Direct proof that "the note is locked": the seller has disputeCount > 0(streamDisputeLock).
    let dispute_count = |locks: &Option<Value>| -> u64 {
        locks
            .as_ref()
            .map(|l| &l["disputeCount"])
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(0)
    };
    let mut seller_locked = false;
    for _ in 0..20 {
        let locks = be.note_stream_locks(&seller_addr).await.expect("locks");
        println!("seller note locks={locks:?}");
        if dispute_count(&locks) > 0 {
            seller_locked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(
        seller_locked,
        "the seller's note is actually locked by the dispute (disputeCount>0 -> ERR_STREAM_LOCKED)"
    );
    println!("=== seller note dispute-locked on chain (ERR_STREAM_LOCKED) ===");

    // the seller CONCEDES -- releaseDispute() unlocks both notes and returns the tick to the buyer.
    be.release_dispute(&tc, &seller)
        .await
        .expect("releaseDispute");
    let mut resolved = false;
    for _ in 0..20 {
        let st = be.token_contract_state(&tc).await.expect("st");
        let locks = be.note_stream_locks(&seller_addr).await.expect("locks");
        println!("after release: state={st:?} seller_locks={locks:?}");
        let still_disputed = st
            .as_ref()
            .and_then(|s| s["disputed"].as_bool())
            .unwrap_or(true);
        let unlocked = dispute_count(&locks) == 0;
        if !still_disputed && unlocked {
            resolved = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(
        resolved,
        "releaseDispute resolved the dispute: disputed==false AND the seller's note is unlocked (disputeCount==0)"
    );
    println!("=== releaseDispute resolved: notes unlocked, tick returned to buyer ===");
}

/// Helper: the seller posts the probe-commission(a fresh operational wallet + ECC[2] from the giver) and
/// opens the stream with the handover cipher of the endpoint to the buyer. On exit: TC `opened`, `probeFunded`.
async fn open_stream_with_probe(
    be: &RealChainBackend,
    tc: &Address,
    seller: &KeyPair,
    s_sec: &str,
    b_sec: &str,
) {
    use crate::note::Note;
    let wallet_keys = KeyPair::generate();
    let wallet = be
        .deploy_multisig(&wallet_keys)
        .await
        .expect("deploy wallet");
    be.giver_send_shell(&wallet.with_workchain(), 100_000_000)
        .await
        .expect("send shell");
    for _ in 0..20 {
        if be
            .client()
            .get_account(&wallet)
            .await
            .expect("w")
            .map(|a| a.shell())
            .unwrap_or(0)
            > 0
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    be.fund_probe_commission(&wallet, &wallet_keys, tc, 1_000_000)
        .await
        .expect("fund probe");
    for _ in 0..20 {
        if be
            .token_contract_probe(tc)
            .await
            .expect("p")
            .and_then(|x| x["probeFunded"].as_bool())
            .unwrap_or(false)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    let buyer_rn = RealNote::from_keypair(KeyPair::from_secret_hex(b_sec).expect("brn"))
        .expect("buyer real note");
    let seller_rn = RealNote::from_keypair(KeyPair::from_secret_hex(s_sec).expect("srn"))
        .expect("seller real note");
    let cipher = seller_rn.encrypt_to(&buyer_rn.pubkey(), b"https://seller.example/v1|fp");
    be.open_stream(tc, seller, &cipher).await.expect("open");
    for _ in 0..20 {
        if be
            .token_contract_state(tc)
            .await
            .expect("s")
            .and_then(|s| s["opened"].as_bool())
            .unwrap_or(false)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

fn u128_field(v: &Option<Value>, key: &str) -> u128 {
    v.as_ref()
        .and_then(|s| s[key].as_str())
        .and_then(|x| x.parse::<u128>().ok())
        .unwrap_or(0)
}

/// A LIVE deal test, step 4b(accept path + two-tick invariant + clean stop): from an open
/// stream the seller waits for `SETTLE_WINDOW` and `advance` -- the probe is accepted (probe-tick -> seller,
/// the commission is returned), and the **two-tick invariant** is established(`prepaid==P && frozen==P`);
/// then the buyer `stop` -- a standard split, the stream is closed. Slow(~5 min due to 180s).
#[tokio::test]
#[ignore = "live: stream advance(accept) + two-tick invariant + stop on shellnet (~5min: 180s settle)"]
async fn live_stream_advance_and_stop() {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set -- skipping");
        return;
    };
    let pool: Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let notes = pool["notes"].as_array().expect("notes");
    let s_sec = notes[0]["owner_secret_key_hex"].as_str().expect("s0");
    let b_sec = notes[1]["owner_secret_key_hex"].as_str().expect("s1");
    let seller_addr = Address::parse(notes[0]["address"].as_str().expect("a0")).expect("a0");
    let seller = KeyPair::from_secret_hex(s_sec).expect("k0");
    let buyer_addr = Address::parse(notes[1]["address"].as_str().expect("a1")).expect("a1");
    let buyer = KeyPair::from_secret_hex(b_sec).expect("k1");

    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let be = RealChainBackend::connect(manifest).expect("connect");

    // ticks=4(depo=4*P=40000) -- enough for the probe-tick at open(P) + the two-tick invariant(2P+fee).
    let price: u128 = 10_000;
    let (_ob, tc) = setup_funded_deal(
        &be,
        &seller_addr,
        &seller,
        &buyer_addr,
        &buyer,
        price,
        5,
        4,
        100_000_000,
    )
    .await;
    println!(
        "=== funded TC {tc} deposit={} (price={price}) ===",
        u128_field(&be.token_contract_state(&tc).await.expect("st"), "deposit")
    );

    open_stream_with_probe(&be, &tc, &seller, s_sec, b_sec).await;
    let st = be.token_contract_state(&tc).await.expect("st");
    println!(
        "=== opened: opened={:?} frozen={} deposit={} ===",
        st.as_ref().and_then(|s| s["opened"].as_bool()),
        u128_field(&st, "frozen"),
        u128_field(&st, "deposit")
    );

    // advance requires block.timestamp >= _prepaidTime + SETTLE_WINDOW(180s).
    println!("=== waiting for SETTLE_WINDOW (185s) ===");
    tokio::time::sleep(std::time::Duration::from_secs(185)).await;
    be.advance_stream(&tc, &seller).await.expect("advance");

    let mut accepted = false;
    let (mut prepaid, mut frozen) = (0u128, 0u128);
    for _ in 0..20 {
        let st = be.token_contract_state(&tc).await.expect("st");
        println!(
            "after advance: probeAccepted={:?} prepaid={} frozen={} deposit={} ticksFinalized={}",
            st.as_ref().and_then(|s| s["probeAccepted"].as_bool()),
            u128_field(&st, "prepaid"),
            u128_field(&st, "frozen"),
            u128_field(&st, "deposit"),
            u128_field(&st, "finalizedOwed"),
        );
        if st
            .as_ref()
            .and_then(|s| s["probeAccepted"].as_bool())
            .unwrap_or(false)
        {
            accepted = true;
            prepaid = u128_field(&st, "prepaid");
            frozen = u128_field(&st, "frozen");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(accepted, "the probe is accepted (probeAccepted==true)");
    assert_eq!(prepaid, price, "two-tick invariant: prepaid == P");
    assert_eq!(frozen, price, "two-tick invariant: frozen == P");
    println!(
        "=== two-tick invariant established: prepaid={prepaid} frozen={frozen} (P={price}) ==="
    );

    // The buyer closes the stream -- a standard split: the delivered tick to the seller, the remainder to the buyer.
    be.stream_stop(&buyer_addr, &buyer, &tc)
        .await
        .expect("stop");
    let mut closed = false;
    for _ in 0..20 {
        let st = be.token_contract_state(&tc).await.expect("st");
        println!(
            "after stop: opened={:?} prepaid={} frozen={} deposit={}",
            st.as_ref().and_then(|s| s["opened"].as_bool()),
            u128_field(&st, "prepaid"),
            u128_field(&st, "frozen"),
            u128_field(&st, "deposit"),
        );
        if st
            .as_ref()
            .and_then(|s| s["opened"].as_bool())
            .map(|o| !o)
            .unwrap_or(false)
        {
            closed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(
        closed,
        "the stream is closed by a standard split (opened==false)"
    );
}

/// A LIVE test, the **seller no-show** scenario: the deal is open, but the seller does NOT advance
/// the ticks; after `STREAM_TIMEOUT` the buyer reclaims. Expectation: the stream is closed, the probe is **not accepted**,
/// the commission(250) -> the seller as `finalizedOwed`, probe+deposit -> the buyer, **no burn** (a no-show is not
/// slashed). Slow(~12 min due to 600s). Requires `DEXDO_PN_POOL`.
#[tokio::test]
#[ignore = "live: seller no-show reclaim on shellnet (~12min: 600s STREAM_TIMEOUT)"]
async fn live_stream_seller_no_show() {
    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set -- skipping");
        return;
    };
    let pool: Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let notes = pool["notes"].as_array().expect("notes");
    let s_sec = notes[0]["owner_secret_key_hex"].as_str().expect("s0");
    let b_sec = notes[1]["owner_secret_key_hex"].as_str().expect("s1");
    let seller_addr = Address::parse(notes[0]["address"].as_str().expect("a0")).expect("a0");
    let seller = KeyPair::from_secret_hex(s_sec).expect("k0");
    let buyer_addr = Address::parse(notes[1]["address"].as_str().expect("a1")).expect("a1");
    let buyer = KeyPair::from_secret_hex(b_sec).expect("k1");

    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let be = RealChainBackend::connect(manifest).expect("connect");

    let price: u128 = 10_000;
    let (_ob, tc) = setup_funded_deal(
        &be,
        &seller_addr,
        &seller,
        &buyer_addr,
        &buyer,
        price,
        5,
        4,
        100_000_000,
    )
    .await;
    open_stream_with_probe(&be, &tc, &seller, s_sec, b_sec).await;
    println!(
        "=== opened (seller will STAY SILENT): probeAccepted={:?} frozen={} deposit={} ===",
        be.token_contract_state(&tc)
            .await
            .expect("st")
            .as_ref()
            .and_then(|s| s["probeAccepted"].as_bool()),
        u128_field(&be.token_contract_state(&tc).await.expect("st"), "frozen"),
        u128_field(&be.token_contract_state(&tc).await.expect("st"), "deposit"),
    );

    // The seller stays silent for exactly STREAM_TIMEOUT(600s) -- then the buyer reclaims.
    println!("=== waiting for STREAM_TIMEOUT (605s), the seller does not advance the ticks ===");
    tokio::time::sleep(std::time::Duration::from_secs(605)).await;
    be.reclaim_on_timeout(&buyer_addr, &buyer, &tc)
        .await
        .expect("reclaim");

    let mut closed = false;
    let (mut accepted_after, mut finalized) = (None, 0u128);
    for _ in 0..20 {
        let st = be.token_contract_state(&tc).await.expect("st");
        println!(
            "after reclaim: opened={:?} probeAccepted={:?} finalizedOwed={} frozen={} deposit={}",
            st.as_ref().and_then(|s| s["opened"].as_bool()),
            st.as_ref().and_then(|s| s["probeAccepted"].as_bool()),
            u128_field(&st, "finalizedOwed"),
            u128_field(&st, "frozen"),
            u128_field(&st, "deposit"),
        );
        if st
            .as_ref()
            .and_then(|s| s["opened"].as_bool())
            .map(|o| !o)
            .unwrap_or(false)
        {
            closed = true;
            accepted_after = st.as_ref().and_then(|s| s["probeAccepted"].as_bool());
            finalized = u128_field(&st, "finalizedOwed");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(
        closed,
        "no-show: the stream is closed by the reclaim (opened==false)"
    );
    assert_eq!(
        accepted_after,
        Some(false),
        "the probe is NOT accepted (the seller did not show up)"
    );
    // to the seller -- only the returned commission(250 = price*250bps); probe+deposit to the buyer; no burn.
    assert_eq!(
        finalized, 250,
        "no-show: the seller's finalizedOwed == the returned commission (250), the tick did NOT go to the seller"
    );
    println!(
        "=== seller no-show: reclaimed, no burn, commission(250)->seller, probe+deposit->buyer ==="
    );
}

/// **LEGACY.** Its deal SETUP routes through the OLD operator-wallet/giver path
/// (`deploy_multisig`), kept as `test-giver` regression coverage -- **NOT** the canonical proof (though
/// `RealDealBackend::open_stream` itself posts the probe-commission note-funded). The canonical note-funded
/// seller proof is [`provision_market`](Self::provision_market) + `live_post_probe_commission_note_funded`.
/// A LIVE trait-level smoke test: drives a deal **through the adapter methods** of `RealDealBackend`
/// (`post_offer`->`place_buy`->`read_match`->`open_stream`->`read_handover`->`accept_probe`->`stop`)
/// on shellnet -- checks the wiring of step 5 end-to-end. Slow(~6 min: 185s SETTLE_WINDOW). Requires `DEXDO_PN_POOL`.
#[tokio::test]
#[ignore = "live: drive a deal through the RealDealBackend ChainBackend trait on shellnet (~6min)"]
async fn live_real_deal_backend_trait() {
    use crate::chain::{ChainBackend, SellOffer};
    use crate::machine::Settlement;
    use crate::note::Note;

    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set -- skipping");
        return;
    };
    let pool: Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let notes = pool["notes"].as_array().expect("notes");
    let s_sec = notes[0]["owner_secret_key_hex"].as_str().expect("s0");
    let b_sec = notes[1]["owner_secret_key_hex"].as_str().expect("s1");
    let kp = |h: &str| KeyPair::from_secret_hex(h).expect("kp");
    let seller_addr = Address::parse(notes[0]["address"].as_str().expect("a0")).expect("a0");
    let buyer_addr = Address::parse(notes[1]["address"].as_str().expect("a1")).expect("a1");

    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let chain = RealChainBackend::connect(manifest).expect("connect");

    let price: u128 = 10_000;
    let max_ticks: u128 = 5;
    let ticks: u128 = 4;
    let nonce = now_secs();
    let frame_model = format!("dexdo-d-deal--{nonce:016x}");
    let model_hash = model_hash_for(&frame_model);

    // Provisioning the deal with low-level helpers: the book, the seller's RootModel, the per-deal TC,
    // the operational wallet(+ECC[2] SHELL).
    let ob = chain
        .inference_orderbook_address(&seller_addr, &model_hash, 1000)
        .await
        .expect("ob addr");
    chain
        .deploy_inference_orderbook(&seller_addr, &kp(s_sec), &model_hash, &frame_model, 1000)
        .await
        .expect("deploy ob");
    wait_active(&chain, &ob).await;
    let seller_pubkey_v = json!(format!("0x{}", kp(s_sec).public_hex()));
    let rm = chain
        .root_model_address_for(&seller_pubkey_v)
        .await
        .expect("rm");
    if !is_active(&chain, &rm).await {
        chain
            .deploy_root_model(&kp(s_sec))
            .await
            .expect("deploy rm");
    }
    let tc = chain
        .deploy_token_contract(
            &kp(s_sec),
            &rm,
            nonce,
            &frame_model,
            1,
            price,
            max_ticks,
            &seller_addr,
        )
        .await
        .expect("deploy tc");
    let wallet_keys = KeyPair::generate();
    let wallet = chain
        .deploy_multisig(&wallet_keys)
        .await
        .expect("deploy wallet");
    chain
        .giver_send_shell(&wallet.with_workchain(), 100_000_000)
        .await
        .expect("send shell");
    for _ in 0..20 {
        if chain
            .client()
            .get_account(&wallet)
            .await
            .expect("w")
            .map(|a| a.shell())
            .unwrap_or(0)
            > 0
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }

    // The actors' notes for the x25519 handover(encrypt/decrypt).
    let seller_rn = RealNote::from_keypair(kp(s_sec)).expect("seller real note");
    let buyer_rn = RealNote::from_keypair(kp(b_sec)).expect("buyer real note");

    let ctx = DealContext {
        order_book: ob,
        model_hash: model_hash.clone(),
        nonce,
        seller_note: seller_addr,
        seller_keys: kp(s_sec),
        buyer_note: buyer_addr,
        buyer_keys: kp(b_sec),
        buyer_pubkey: buyer_rn.pubkey(),
        price_per_tick: price,
        max_ticks,
        ticks,
        escrow: 100_000_000,
        probe_shell: 1_000_000,
    };
    let backend = RealDealBackend::new(chain, ctx);
    let tc_s = tc.with_workchain();

    // --- Drive THROUGH the ChainBackend trait ---
    backend
        .post_offer(
            SellOffer {
                price_per_tick: price as u64,
                max_ticks: max_ticks as u64,
                token_contract: tc_s.clone(),
            },
            &seller_rn,
        )
        .await
        .expect("post_offer");
    backend
        .place_buy(&tc_s, &buyer_rn)
        .await
        .expect("place_buy");
    let m = backend.read_match(&tc_s).await.expect("read_match");
    println!(
        "=== read_match via trait: price_per_tick={} ===",
        m.price_per_tick
    );
    assert_eq!(m.price_per_tick, price as Shell, "read_match price");

    let endpoint = b"https://seller.example/v1|fp".to_vec();
    let enc = seller_rn.encrypt_to(&buyer_rn.pubkey(), &endpoint);
    backend
        .open_stream(&tc_s, enc, &seller_rn)
        .await
        .expect("open_stream");
    let ho = backend
        .read_handover(&tc_s)
        .await
        .expect("read_handover")
        .expect("cipher present");
    assert_eq!(
        buyer_rn.decrypt(&ho).expect("decrypt"),
        endpoint,
        "the handover round-trips through the trait adapter"
    );
    println!("=== handover via trait round-trips ===");

    println!("=== waiting for SETTLE_WINDOW (185s) ===");
    tokio::time::sleep(std::time::Duration::from_secs(185)).await;
    backend.accept_probe(&tc_s).await.expect("accept_probe");
    let mut accepted = false;
    for _ in 0..20 {
        let snap = backend.snapshot(&tc_s).await;
        println!("snapshot via trait: {snap:?}");
        if snap.map(|s| s.seller_received > 0).unwrap_or(false) {
            accepted = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(
        accepted,
        "accept_probe via the trait: the probe is accepted (seller_received>0)"
    );

    let settlement = backend.stop(&tc_s, &buyer_rn).await.expect("stop");
    println!("=== Settlement via trait = {settlement:?} ===");
    assert!(
        matches!(settlement, Settlement::AmicableSplit { .. }),
        "a clean stop after accept -> AmicableSplit"
    );
    let snap = backend.snapshot(&tc_s).await.expect("snapshot");
    assert!(snap.closed, "snapshot.closed after stop via the trait");
    println!(
        "=== RealDealBackend trait drive OK: closed={} ===",
        snap.closed
    );
}

/// live: `dexdo recover`'s STOP path cleanly closes an **orphaned OPEN** deal(AmicableSplit).
/// Drives a real deal to STREAMING(probe accepted) exactly like `live_real_deal_backend_trait`, then --
/// instead of the buyer's normal stop -- the buyer process "dies" and we invoke the RECOVER path from a
/// FRESH connection: `RealChainBackend::stream_stop(buyer_note, buyer_keys, tc)`, the exact call
/// `dexdo recover` makes after its preflight. Asserts the deal STOPs cleanly (opened=false, the snapshot
/// is closed -> AmicableSplit: the seller is paid the accepted tick, the buyer refunded the rest, no
/// BurnBoth), and that `destroy` then closes the TC. The CLI preflight (not-OPEN / disputed / wrong note
/// addr / wrong key) is offline-proven. Slow(~6min: 185s SETTLE_WINDOW).
#[tokio::test]
#[ignore = "live:  recover STOPs an orphaned OPEN streaming deal (AmicableSplit) on shellnet (~6min)"]
async fn live_recover_stops_orphaned_open_deal() {
    use crate::chain::{ChainBackend, SellOffer};
    use crate::note::Note;

    let Ok(pool_path) = std::env::var("DEXDO_PN_POOL") else {
        eprintln!("DEXDO_PN_POOL not set -- skipping");
        return;
    };
    let pool: Value =
        serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool")).expect("parse pool");
    let notes = pool["notes"].as_array().expect("notes");
    let s_sec = notes[0]["owner_secret_key_hex"].as_str().expect("s0");
    let b_sec = notes[1]["owner_secret_key_hex"].as_str().expect("s1");
    let kp = |h: &str| KeyPair::from_secret_hex(h).expect("kp");
    let seller_addr = Address::parse(notes[0]["address"].as_str().expect("a0")).expect("a0");
    let buyer_addr = Address::parse(notes[1]["address"].as_str().expect("a1")).expect("a1");

    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/deployed.shellnet.json"
    );
    let chain = RealChainBackend::connect(manifest).expect("connect");

    let price: u128 = 10_000;
    let max_ticks: u128 = 5;
    let ticks: u128 = 4;
    let nonce = now_secs();
    let frame_model = format!("dexdo-recover--{nonce:016x}");
    let model_hash = model_hash_for(&frame_model);

    // -- provision the deal(OB + RootModel + per-deal TC) --
    let ob = chain
        .inference_orderbook_address(&seller_addr, &model_hash, 1000)
        .await
        .expect("ob addr");
    chain
        .deploy_inference_orderbook(&seller_addr, &kp(s_sec), &model_hash, &frame_model, 1000)
        .await
        .expect("deploy ob");
    wait_active(&chain, &ob).await;
    let seller_pubkey_v = json!(format!("0x{}", kp(s_sec).public_hex()));
    let rm = chain
        .root_model_address_for(&seller_pubkey_v)
        .await
        .expect("rm");
    if !is_active(&chain, &rm).await {
        chain
            .deploy_root_model(&kp(s_sec))
            .await
            .expect("deploy rm");
    }
    let tc = chain
        .deploy_token_contract(
            &kp(s_sec),
            &rm,
            nonce,
            &frame_model,
            1,
            price,
            max_ticks,
            &seller_addr,
        )
        .await
        .expect("deploy tc");
    let wallet_keys = KeyPair::generate();
    let wallet = chain
        .deploy_multisig(&wallet_keys)
        .await
        .expect("deploy wallet");
    chain
        .giver_send_shell(&wallet.with_workchain(), 100_000_000)
        .await
        .expect("send shell");
    for _ in 0..20 {
        if chain
            .client()
            .get_account(&wallet)
            .await
            .expect("w")
            .map(|a| a.shell())
            .unwrap_or(0)
            > 0
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }

    let seller_rn = RealNote::from_keypair(kp(s_sec)).expect("seller real note");
    let buyer_rn = RealNote::from_keypair(kp(b_sec)).expect("buyer real note");
    let ctx = DealContext {
        order_book: ob,
        model_hash: model_hash.clone(),
        nonce,
        seller_note: seller_addr.clone(),
        seller_keys: kp(s_sec),
        buyer_note: buyer_addr.clone(),
        buyer_keys: kp(b_sec),
        buyer_pubkey: buyer_rn.pubkey(),
        price_per_tick: price,
        max_ticks,
        ticks,
        escrow: 100_000_000,
        probe_shell: 1_000_000,
    };
    let backend = RealDealBackend::new(chain, ctx);
    let tc_s = tc.with_workchain();

    backend
        .post_offer(
            SellOffer {
                price_per_tick: price as u64,
                max_ticks: max_ticks as u64,
                token_contract: tc_s.clone(),
            },
            &seller_rn,
        )
        .await
        .expect("post_offer");
    backend
        .place_buy(&tc_s, &buyer_rn)
        .await
        .expect("place_buy");
    let m = backend.read_match(&tc_s).await.expect("read_match");
    assert_eq!(m.price_per_tick, price as Shell, "read_match price");

    let endpoint = b"https://seller.example/v1|fp".to_vec();
    let enc = seller_rn.encrypt_to(&buyer_rn.pubkey(), &endpoint);
    backend
        .open_stream(&tc_s, enc, &seller_rn)
        .await
        .expect("open_stream");

    println!("=== deal OPEN; wait SETTLE_WINDOW (185s) then accept the probe (-> streaming) ===");
    tokio::time::sleep(std::time::Duration::from_secs(185)).await;
    backend.accept_probe(&tc_s).await.expect("accept_probe");
    let mut accepted = false;
    for _ in 0..20 {
        if backend
            .snapshot(&tc_s)
            .await
            .map(|s| s.seller_received > 0)
            .unwrap_or(false)
        {
            accepted = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    assert!(
        accepted,
        "probe accepted (seller_received>0) -> the deal is STREAMING (post-probe)"
    );

    // -- the buyer process "dies" here(no normal stop) -> the deal is ORPHANED OPEN. RECOVER from a FRESH
    // connection -- the exact call `dexdo recover` makes after its preflight. --
    let recover_chain = RealChainBackend::connect(manifest).expect("recover connect");
    let pre = recover_chain
        .token_contract_state(&tc)
        .await
        .expect("getState")
        .expect("TC active");
    assert_eq!(
        pre["opened"].as_bool(),
        Some(true),
        "the deal is OPEN before recover (orphaned)"
    );
    recover_chain
        .stream_stop(&buyer_addr, &kp(b_sec), &tc)
        .await
        .expect("recover: streamStop -> TC.stop()");

    // -- the streamStop submits async; wait for the stop to APPLY(opened->false), exactly as the buyer-STOP
    // path does(`RealBuyerBackend::stop` blocks on opened=false). Then verify AmicableSplit. --
    let mut post = None;
    for _ in 0..30 {
        let st = recover_chain
            .token_contract_state(&tc)
            .await
            .expect("getState")
            .expect("TC active");
        if st["opened"].as_bool() == Some(false) {
            post = Some(st);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    let post = post.expect("recover: the deal STOPped (opened=false) within the deadline");
    println!(
        "post-recover getState: opened={:?} disputed={:?} finalizedOwed={:?} frozen={:?} deposit={:?}",
        post["opened"], post["disputed"], post["finalizedOwed"], post["frozen"], post["deposit"]
    );
    assert_eq!(
        post["opened"].as_bool(),
        Some(false),
        "recover STOPped the orphaned OPEN deal (opened=false)"
    );
    assert_eq!(
        post["disputed"].as_bool(),
        Some(false),
        "recover is a clean STOP, not a dispute (no BurnBoth)"
    );
    // AmicableSplit: post-probe-accept stop pays the seller for the delivered tick(finalizedOwed>0); a
    // BurnBoth/ProbeBurn would not credit the seller. This is the directive's "AmicableSplit, zero BurnBoth".
    let owed: u128 = post["finalizedOwed"]
        .as_str()
        .and_then(|x| x.parse().ok())
        .unwrap_or(0);
    assert!(
        owed > 0,
        "AmicableSplit: the seller is owed the accepted tick (finalizedOwed={owed} > 0), not BurnBoth"
    );
    let snap = backend.snapshot(&tc_s).await.expect("snapshot");
    assert!(
        snap.closed,
        "recover closed the deal (AmicableSplit: seller paid the accepted tick, buyer refunded the rest)"
    );
    println!(
        "===: `dexdo recover` (stream_stop) STOPped the orphaned OPEN deal -- closed={} ===",
        snap.closed
    );

    // -- destroy closes the STOPped TC(only succeeds when !_opened && !_disputed -> confirms a clean STOP). --
    recover_chain
        .destroy_token_contract(&tc, &seller_addr, &kp(s_sec))
        .await
        .expect("destroy after recover");
    println!("=== destroy OK: the recovered+STOPped deal's TC is closed ===");
}
