//! `dexdo note deploy` (#176): deploy a wallet-funded `PrivateNote` on shellnet in-process through
//! `gosh.ackinacki`, then fold the CLI-compatible result into a `DEXDO_PN_POOL` pool the `seller`/`buyer`
//! already consume. The chain call lives in `commands.rs::run_note_deploy`; the pure schema adapters
//! (offline §5) live here.

use anyhow::{anyhow, bail, Result};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

#[allow(dead_code)]
const UNIT_SCALE: u128 = 1_000_000_000;
const SHELL_ECC_ID: u32 = 2;
const NOTE_DEPLOY_RECOVERY_VERSION: u32 = 1;

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoteAccountSnapshot {
    pub(crate) address: String,
    pub(crate) status: String,
    pub(crate) native_raw: u128,
    pub(crate) ecc: Vec<(u32, u128)>,
    pub(crate) code_hash: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NoteBalanceMap {
    Known(Vec<(u32, u128)>),
    Unknown(String),
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoteGetterBalanceMaps {
    pub(crate) balance: NoteBalanceMap,
    pub(crate) locked_in_orders: NoteBalanceMap,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoteBalanceView {
    pub(crate) account: NoteAccountSnapshot,
    pub(crate) getters: NoteGetterBalanceMaps,
}

#[allow(dead_code)]
pub(crate) fn build_note_balance_view(
    note_addr: &str,
    account: Option<NoteAccountSnapshot>,
    getters: NoteGetterBalanceMaps,
) -> Result<NoteBalanceView> {
    let account = account.ok_or_else(|| {
        anyhow!(
            "cannot read PrivateNote account {note_addr}: account not found/null; refusing to report zero balance"
        )
    })?;
    Ok(NoteBalanceView { account, getters })
}

#[allow(dead_code)]
pub(crate) fn note_getter_balance_maps(details: Option<&Value>) -> NoteGetterBalanceMaps {
    let Some(details) = details else {
        return NoteGetterBalanceMaps {
            balance: NoteBalanceMap::Unknown("getDetails returned no data".to_string()),
            locked_in_orders: NoteBalanceMap::Unknown("getDetails returned no data".to_string()),
        };
    };
    NoteGetterBalanceMaps {
        balance: parse_balance_map(&details["balance"], "balance"),
        locked_in_orders: parse_balance_map(&details["lockedInOrders"], "lockedInOrders"),
    }
}

#[allow(dead_code)]
pub(crate) fn unknown_note_getter_balance_maps(reason: impl Into<String>) -> NoteGetterBalanceMaps {
    let reason = reason.into();
    NoteGetterBalanceMaps {
        balance: NoteBalanceMap::Unknown(reason.clone()),
        locked_in_orders: NoteBalanceMap::Unknown(reason),
    }
}

#[allow(dead_code)]
pub(crate) fn render_note_balance(view: &NoteBalanceView) -> String {
    let mut out = String::new();
    let account = &view.account;
    writeln!(&mut out, "PrivateNote {}", account.address).unwrap();
    writeln!(&mut out, "status: {}", account.status).unwrap();
    if let Some(code_hash) = account.code_hash.as_deref() {
        writeln!(&mut out, "code_hash: {code_hash}").unwrap();
    } else {
        writeln!(&mut out, "code_hash: unknown").unwrap();
    }
    writeln!(
        &mut out,
        "SHELL ECC[2]: {} SHELL (raw {})",
        decimal_units(account.ecc_value(SHELL_ECC_ID)),
        account.ecc_value(SHELL_ECC_ID)
    )
    .unwrap();
    writeln!(
        &mut out,
        "VMSHELL native gas: {} vmshell (raw {})",
        decimal_units(account.native_raw),
        account.native_raw
    )
    .unwrap();
    render_ecc_map(
        &mut out,
        "account ECC balances",
        &NoteBalanceMap::Known(account.ecc.clone()),
    );
    render_ecc_map(
        &mut out,
        "PrivateNote.getDetails balance",
        &view.getters.balance,
    );
    render_ecc_map(
        &mut out,
        "PrivateNote.getDetails lockedInOrders",
        &view.getters.locked_in_orders,
    );
    out
}

impl NoteAccountSnapshot {
    #[allow(dead_code)]
    fn ecc_value(&self, id: u32) -> u128 {
        self.ecc
            .iter()
            .find(|(currency, _)| *currency == id)
            .map(|(_, value)| *value)
            .unwrap_or(0)
    }
}

#[allow(dead_code)]
fn render_ecc_map(out: &mut String, title: &str, map: &NoteBalanceMap) {
    writeln!(out, "{title}:").unwrap();
    match map {
        NoteBalanceMap::Known(entries) if entries.is_empty() => {
            writeln!(out, "  none reported").unwrap();
        }
        NoteBalanceMap::Known(entries) => {
            let mut entries = entries.clone();
            entries.sort_by_key(|(id, _)| *id);
            for (id, value) in entries {
                if id == SHELL_ECC_ID {
                    writeln!(
                        out,
                        "  ECC[2] SHELL: {} SHELL (raw {value})",
                        decimal_units(value)
                    )
                    .unwrap();
                } else {
                    writeln!(out, "  ECC[{id}]: raw {value}").unwrap();
                }
            }
        }
        NoteBalanceMap::Unknown(reason) => {
            writeln!(out, "  unknown ({reason})").unwrap();
        }
    }
}

#[allow(dead_code)]
fn decimal_units(raw: u128) -> String {
    format!("{}.{:09}", raw / UNIT_SCALE, raw % UNIT_SCALE)
}

#[allow(dead_code)]
fn parse_balance_map(value: &Value, name: &str) -> NoteBalanceMap {
    if value.is_null() {
        return NoteBalanceMap::Unknown(format!("{name} field unavailable"));
    }
    if let Some(object) = value.as_object() {
        let mut out = Vec::new();
        for (id, amount) in object {
            let Some(id) = parse_u32_key(id) else {
                return NoteBalanceMap::Unknown(format!("{name} contains non-numeric currency id"));
            };
            let Some(amount) = parse_u128_value(amount) else {
                return NoteBalanceMap::Unknown(format!("{name}[{id}] is not a u128"));
            };
            out.push((id, amount));
        }
        return NoteBalanceMap::Known(out);
    }
    if let Some(array) = value.as_array() {
        let mut out = Vec::new();
        for item in array {
            let Some(id) = item
                .get("currency")
                .or_else(|| item.get("id"))
                .and_then(parse_u32_value)
            else {
                return NoteBalanceMap::Unknown(format!("{name} array entry missing currency id"));
            };
            let Some(amount) = item
                .get("value")
                .or_else(|| item.get("amount"))
                .and_then(parse_u128_value)
            else {
                return NoteBalanceMap::Unknown(format!("{name}[{id}] is not a u128"));
            };
            out.push((id, amount));
        }
        return NoteBalanceMap::Known(out);
    }
    NoteBalanceMap::Unknown(format!("{name} has unexpected JSON shape"))
}

#[allow(dead_code)]
fn parse_u32_key(raw: &str) -> Option<u32> {
    raw.parse::<u32>().ok().or_else(|| {
        raw.strip_prefix("0x")
            .or_else(|| raw.strip_prefix("0X"))
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
    })
}

#[allow(dead_code)]
fn parse_u32_value(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .or_else(|| value.as_str().and_then(parse_u32_key))
}

#[allow(dead_code)]
fn parse_u128_value(value: &Value) -> Option<u128> {
    value.as_u64().map(u128::from).or_else(|| {
        let raw = value.as_str()?.trim();
        raw.parse::<u128>().ok().or_else(|| {
            raw.strip_prefix("0x")
                .or_else(|| raw.strip_prefix("0X"))
                .and_then(|hex| u128::from_str_radix(hex, 16).ok())
        })
    })
}

pub(crate) fn normalize_owner_pubkey_hex(raw: &str, label: &str) -> Result<String> {
    let key = raw.trim().trim_start_matches("0x").trim_start_matches("0X");
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("{label} must be a 32-byte hex public key, got `{raw}`");
    }
    Ok(key.to_ascii_lowercase())
}

pub(crate) fn derive_owner_pubkey_from_secret_hex(secret_hex: &str) -> Result<String> {
    let secret = secret_hex
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    let bytes = hex::decode(secret)
        .map_err(|e| anyhow!("owner_secret_key_hex must be 32-byte hex: {e}"))?;
    let seed: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("owner_secret_key_hex must be exactly 32 bytes"))?;
    let signing = SigningKey::from_bytes(&seed);
    Ok(hex::encode(signing.verifying_key().as_bytes()))
}

pub(crate) fn ensure_pool_note_keypair_matches(
    note_addr: &str,
    owner_public_key_hex: &str,
    owner_secret_key_hex: &str,
) -> Result<String> {
    let recorded = normalize_owner_pubkey_hex(owner_public_key_hex, "owner_public_key_hex")?;
    let derived = derive_owner_pubkey_from_secret_hex(owner_secret_key_hex)?;
    if recorded != derived {
        bail!(
            "note deploy aborted before writing DEXDO_PN_POOL: stored owner_secret_key_hex derives pubkey \
             0x{derived}, but the pool entry for PrivateNote {note_addr} records owner_public_key_hex \
             0x{recorded}. That note would later fail owner-signed writes with ERR_INVALID_SENDER 101 \
             because --note-key does not match the note owner. Deploy into a fresh --pool <new_file> or use \
             the correct pool/key material."
        );
    }
    Ok(derived)
}

#[allow(dead_code)]
pub(crate) fn ensure_onchain_owner_matches_pool_key(
    role: &str,
    note_addr: &str,
    onchain_owner_pubkey: Option<&str>,
    derived_owner_pubkey: &str,
) -> Result<()> {
    let derived = normalize_owner_pubkey_hex(derived_owner_pubkey, "derived owner pubkey")?;
    let Some(onchain_raw) = onchain_owner_pubkey else {
        bail!(
            "{role} aborted before writing DEXDO_PN_POOL: PrivateNote {note_addr} getDetails exposes no \
             ephemeralPubkey. Refusing to leave a pool entry that may fail later with ERR_INVALID_SENDER 101. \
             Deploy a fresh note with --pool <new_file> after verifying shellnet contracts."
        );
    };
    let onchain =
        normalize_owner_pubkey_hex(onchain_raw, "PrivateNote.getDetails().ephemeralPubkey")?;
    if onchain != derived {
        bail!(
            "{role} aborted before writing DEXDO_PN_POOL: PrivateNote {note_addr} on-chain owner key \
             _ephemeralPubkey 0x{onchain} does not match the stored owner_secret_key_hex-derived pubkey \
             0x{derived}. The --note-key would not match this note's owner and provision/sell/withdraw would \
             fail with ERR_INVALID_SENDER 101. Deploy a fresh note with --pool <new_file> and do not reuse the \
             stale/mismatched pool."
        );
    }
    Ok(())
}

/// #344: crash-safe state for wallet-funded note deploy. This file carries the randomly generated note owner
/// secret and is written before any wallet spend. Later deploy steps add the on-chain note identifiers so
/// `dexdo note recover` can finalize the pool without repeating an already completed deploy.
#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct NoteDeployRecoveryState {
    pub version: u32,
    pub endpoint: String,
    pub nominal: String,
    pub token_type: u32,
    pub raw_value: u64,
    pub ecc_shell_deposit: u64,
    pub funding_multisig_address: String,
    pub owner_public_key_hex: String,
    pub owner_secret_key_hex: String,
    pub pn_address: Option<String>,
    pub deposit_identifier_hash: Option<String>,
    pub deployed_at_unix: Option<u64>,
    #[serde(default)]
    pub deposit_voucher: Option<NoteDeployVoucherCheckpoint>,
    #[serde(default)]
    pub shell_voucher: Option<NoteDeployVoucherCheckpoint>,
    pub shell_funded: bool,
    pub sanity_checked: bool,
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
#[derive(Clone, Copy)]
pub(crate) struct NoteDeployRecoveryRequest<'a> {
    pub endpoint: &'a str,
    pub nominal: &'a str,
    pub token_type: u32,
    pub raw_value: u64,
    pub ecc_shell_deposit: u64,
    pub funding_multisig_address: &'a str,
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoteDeployVoucherKind {
    Deposit,
    ShellGas,
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct NoteDeployVoucherCheckpoint {
    pub sk_u_hex: String,
    pub sk_u_commit_hex: String,
    pub recipient_ephemeral_pubkey_hex: String,
    pub token_type: u32,
    pub raw_value: u64,
    pub is_fee: bool,
    #[serde(default)]
    pub submit_maybe_sent: bool,
    #[serde(default)]
    pub event: Option<NoteDeployVoucherEvent>,
    #[serde(default)]
    pub proof: Option<NoteDeployVoucherProof>,
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct NoteDeployVoucherEvent {
    pub id: String,
    pub boc: String,
    pub body: String,
    pub dst: String,
    pub created_at: u64,
    pub block_id: Option<String>,
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct NoteDeployVoucherProof {
    pub proof: String,
    pub deposit_identifier_hash_hex: String,
    pub final_layer_historical_hash_root_hex: String,
    pub voucher_nominal_fr_hex: String,
    pub token_type_fr_hex: String,
    pub ephemeral_pubkey_hex: String,
    pub voucher_value: u64,
    pub voucher_token_type: u32,
    pub layer_number: u8,
    pub sk_u_hex: String,
    pub sk_u_commit_hex: String,
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
impl NoteDeployVoucherKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Deposit => "deposit",
            Self::ShellGas => "SHELL gas",
        }
    }

    fn field_name(self) -> &'static str {
        match self {
            Self::Deposit => "deposit_voucher",
            Self::ShellGas => "shell_voucher",
        }
    }
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
impl NoteDeployVoucherCheckpoint {
    pub(crate) fn new(
        recipient_ephemeral_pubkey_hex: &str,
        token_type: u32,
        raw_value: u64,
        is_fee: bool,
        sk_u_hex: String,
        sk_u_commit_hex: String,
    ) -> Result<Self> {
        let checkpoint = Self {
            sk_u_hex: normalize_secret_like_hex(&sk_u_hex, "sk_u_hex")?,
            sk_u_commit_hex: normalize_secret_like_hex(&sk_u_commit_hex, "sk_u_commit_hex")?,
            recipient_ephemeral_pubkey_hex: normalize_secret_like_hex(
                recipient_ephemeral_pubkey_hex,
                "recipient_ephemeral_pubkey_hex",
            )?,
            token_type,
            raw_value,
            is_fee,
            submit_maybe_sent: false,
            event: None,
            proof: None,
        };
        checkpoint.validate("voucher checkpoint")?;
        Ok(checkpoint)
    }

    pub(crate) fn validate(&self, label: &str) -> Result<()> {
        normalize_secret_like_hex(&self.sk_u_hex, "sk_u_hex")
            .map_err(|e| anyhow!("{label}: {e}"))?;
        normalize_secret_like_hex(&self.sk_u_commit_hex, "sk_u_commit_hex")
            .map_err(|e| anyhow!("{label}: {e}"))?;
        normalize_secret_like_hex(
            &self.recipient_ephemeral_pubkey_hex,
            "recipient_ephemeral_pubkey_hex",
        )
        .map_err(|e| anyhow!("{label}: {e}"))?;
        if self.raw_value == 0 {
            bail!("{label}: raw_value must be positive");
        }
        if !self.submit_maybe_sent && (self.event.is_some() || self.proof.is_some()) {
            bail!("{label}: event/proof cannot exist before voucher submit is marked uncertain");
        }
        if let Some(event) = &self.event {
            event.validate(label)?;
        }
        if let Some(proof) = &self.proof {
            proof.validate(label)?;
            if proof.sk_u_hex != self.sk_u_hex {
                bail!("{label}: proof sk_u_hex does not match checkpoint");
            }
            if proof.sk_u_commit_hex != self.sk_u_commit_hex {
                bail!("{label}: proof sk_u_commit_hex does not match checkpoint");
            }
            if proof.ephemeral_pubkey_hex != self.recipient_ephemeral_pubkey_hex {
                bail!("{label}: proof ephemeral_pubkey_hex does not match checkpoint");
            }
            if proof.voucher_value != self.raw_value {
                bail!("{label}: proof voucher_value does not match checkpoint");
            }
            if proof.voucher_token_type != self.token_type {
                bail!("{label}: proof voucher_token_type does not match checkpoint");
            }
        }
        Ok(())
    }

    pub(crate) fn ensure_matches(
        &self,
        kind: NoteDeployVoucherKind,
        recipient_ephemeral_pubkey_hex: &str,
        token_type: u32,
        raw_value: u64,
        is_fee: bool,
    ) -> Result<()> {
        self.validate(kind.field_name())?;
        let recipient_ephemeral_pubkey_hex = normalize_secret_like_hex(
            recipient_ephemeral_pubkey_hex,
            "recipient_ephemeral_pubkey_hex",
        )?;
        if self.recipient_ephemeral_pubkey_hex != recipient_ephemeral_pubkey_hex
            || self.token_type != token_type
            || self.raw_value != raw_value
            || self.is_fee != is_fee
        {
            bail!(
                "note deploy recovery {} does not match this {} voucher request; refusing to mix \
                 voucher recovery state with a different owner/value/token/isFee.",
                kind.field_name(),
                kind.label()
            );
        }
        Ok(())
    }
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
impl NoteDeployVoucherEvent {
    pub(crate) fn validate(&self, label: &str) -> Result<()> {
        if self.id.trim().is_empty() {
            bail!("{label}: VoucherGenerated event id is empty");
        }
        if self.boc.trim().is_empty() {
            bail!("{label}: VoucherGenerated event boc is empty");
        }
        if self.body.trim().is_empty() {
            bail!("{label}: VoucherGenerated event body is empty");
        }
        if self.dst.trim().is_empty() {
            bail!("{label}: VoucherGenerated event dst is empty");
        }
        Ok(())
    }
}

#[cfg(feature = "shellnet")]
impl NoteDeployVoucherEvent {
    pub(crate) fn from_sdk(
        event: dexdo_core::private_note::voucher_event::VoucherExtoutMessage,
    ) -> Self {
        Self {
            id: event.id,
            boc: event.boc,
            body: event.body,
            dst: event.dst,
            created_at: event.created_at,
            block_id: event.block_id,
        }
    }

    pub(crate) fn to_sdk(&self) -> dexdo_core::private_note::voucher_event::VoucherExtoutMessage {
        dexdo_core::private_note::voucher_event::VoucherExtoutMessage {
            id: self.id.clone(),
            boc: self.boc.clone(),
            body: self.body.clone(),
            dst: self.dst.clone(),
            created_at: self.created_at,
            block_id: self.block_id.clone(),
        }
    }
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
impl NoteDeployVoucherProof {
    pub(crate) fn validate(&self, label: &str) -> Result<()> {
        if self.proof.trim().is_empty() {
            bail!("{label}: halo2 proof is empty");
        }
        validate_hex_u256(
            &self.deposit_identifier_hash_hex,
            "deposit_identifier_hash_hex",
        )
        .map_err(|e| anyhow!("{label}: {e}"))?;
        validate_hex_u256(
            &self.final_layer_historical_hash_root_hex,
            "final_layer_historical_hash_root_hex",
        )
        .map_err(|e| anyhow!("{label}: {e}"))?;
        validate_hex_u256(&self.voucher_nominal_fr_hex, "voucher_nominal_fr_hex")
            .map_err(|e| anyhow!("{label}: {e}"))?;
        validate_hex_u256(&self.token_type_fr_hex, "token_type_fr_hex")
            .map_err(|e| anyhow!("{label}: {e}"))?;
        normalize_secret_like_hex(&self.ephemeral_pubkey_hex, "ephemeral_pubkey_hex")
            .map_err(|e| anyhow!("{label}: {e}"))?;
        normalize_secret_like_hex(&self.sk_u_hex, "sk_u_hex")
            .map_err(|e| anyhow!("{label}: {e}"))?;
        normalize_secret_like_hex(&self.sk_u_commit_hex, "sk_u_commit_hex")
            .map_err(|e| anyhow!("{label}: {e}"))?;
        if self.voucher_value == 0 {
            bail!("{label}: voucher_value must be positive");
        }
        if self.layer_number == 0 {
            bail!("{label}: layer_number must be positive");
        }
        Ok(())
    }
}

#[cfg(feature = "shellnet")]
impl NoteDeployVoucherProof {
    pub(crate) fn from_halo2(proof: &dexdo_core::private_note::halo2::live::Halo2Proof) -> Self {
        Self {
            proof: proof.proof.clone(),
            deposit_identifier_hash_hex: proof.deposit_identifier_hash_hex.clone(),
            final_layer_historical_hash_root_hex: proof
                .final_layer_historical_hash_root_hex
                .clone(),
            voucher_nominal_fr_hex: proof.voucher_nominal_fr_hex.clone(),
            token_type_fr_hex: proof.token_type_fr_hex.clone(),
            ephemeral_pubkey_hex: proof.ephemeral_pubkey_hex.clone(),
            voucher_value: proof.voucher_value,
            voucher_token_type: proof.voucher_token_type,
            layer_number: proof.layer_number,
            sk_u_hex: proof.sk_u_hex.clone(),
            sk_u_commit_hex: proof.sk_u_commit_hex.clone(),
        }
    }

    pub(crate) fn to_halo2(&self) -> dexdo_core::private_note::halo2::live::Halo2Proof {
        dexdo_core::private_note::halo2::live::Halo2Proof {
            proof: self.proof.clone(),
            deposit_identifier_hash_hex: self.deposit_identifier_hash_hex.clone(),
            final_layer_historical_hash_root_hex: self.final_layer_historical_hash_root_hex.clone(),
            voucher_nominal_fr_hex: self.voucher_nominal_fr_hex.clone(),
            token_type_fr_hex: self.token_type_fr_hex.clone(),
            ephemeral_pubkey_hex: self.ephemeral_pubkey_hex.clone(),
            voucher_value: self.voucher_value,
            voucher_token_type: self.voucher_token_type,
            layer_number: self.layer_number,
            sk_u_hex: self.sk_u_hex.clone(),
            sk_u_commit_hex: self.sk_u_commit_hex.clone(),
        }
    }
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
impl NoteDeployRecoveryState {
    pub(crate) fn new(
        request: NoteDeployRecoveryRequest<'_>,
        owner_public_key_hex: &str,
        owner_secret_key_hex: &str,
    ) -> Result<Self> {
        let funding_multisig_address =
            dexdo_core::normalize_wallet_address(request.funding_multisig_address)
                .map_err(|e| anyhow!("{e}"))?;
        let owner_public_key_hex =
            normalize_owner_pubkey_hex(owner_public_key_hex, "owner_public_key_hex")?;
        let owner_secret_key_hex = normalize_secret_hex(owner_secret_key_hex)?;
        let state = Self {
            version: NOTE_DEPLOY_RECOVERY_VERSION,
            endpoint: request.endpoint.to_string(),
            nominal: request.nominal.to_string(),
            token_type: request.token_type,
            raw_value: request.raw_value,
            ecc_shell_deposit: request.ecc_shell_deposit,
            funding_multisig_address,
            owner_public_key_hex,
            owner_secret_key_hex,
            pn_address: None,
            deposit_identifier_hash: None,
            deployed_at_unix: None,
            deposit_voucher: None,
            shell_voucher: None,
            shell_funded: false,
            sanity_checked: false,
        };
        state.validate()?;
        Ok(state)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.version != NOTE_DEPLOY_RECOVERY_VERSION {
            bail!(
                "note deploy recovery file version {} is unsupported; expected {}",
                self.version,
                NOTE_DEPLOY_RECOVERY_VERSION
            );
        }
        if self.endpoint.trim().is_empty() {
            bail!("note deploy recovery file has empty endpoint");
        }
        if self.nominal.trim().is_empty() {
            bail!("note deploy recovery file has empty nominal");
        }
        let normalized_wallet =
            dexdo_core::normalize_wallet_address(&self.funding_multisig_address)
                .map_err(|e| anyhow!("note deploy recovery funding_multisig_address: {e}"))?;
        if normalized_wallet != self.funding_multisig_address {
            bail!(
                "note deploy recovery funding_multisig_address must be normalized as {normalized_wallet}"
            );
        }
        ensure_pool_note_keypair_matches(
            self.pn_address.as_deref().unwrap_or("pending"),
            &self.owner_public_key_hex,
            &self.owner_secret_key_hex,
        )?;
        if self.pn_address.is_some() && self.deposit_identifier_hash.is_none() {
            bail!(
                "note deploy recovery file has pn_address but no deposit_identifier_hash; refusing to guess"
            );
        }
        if let Some(voucher) = &self.deposit_voucher {
            voucher.ensure_matches(
                NoteDeployVoucherKind::Deposit,
                &self.owner_public_key_hex,
                self.token_type,
                self.raw_value,
                false,
            )?;
        }
        if let Some(voucher) = &self.shell_voucher {
            voucher.ensure_matches(
                NoteDeployVoucherKind::ShellGas,
                &self.owner_public_key_hex,
                SHELL_ECC_ID,
                self.ecc_shell_deposit,
                true,
            )?;
        }
        Ok(())
    }

    pub(crate) fn ensure_matches_request(
        &self,
        request: NoteDeployRecoveryRequest<'_>,
    ) -> Result<()> {
        let funding_multisig_address =
            dexdo_core::normalize_wallet_address(request.funding_multisig_address)
                .map_err(|e| anyhow!("{e}"))?;
        if self.endpoint != request.endpoint
            || self.nominal != request.nominal
            || self.token_type != request.token_type
            || self.raw_value != request.raw_value
            || self.ecc_shell_deposit != request.ecc_shell_deposit
            || self.funding_multisig_address != funding_multisig_address
        {
            bail!(
                "note deploy recovery file does not match this deploy request. Refusing to mix recovery state \
                 with a different wallet/endpoint/nominal/token-type; pass the matching --recovery file or \
                 deploy into a fresh --pool/--recovery pair."
            );
        }
        Ok(())
    }

    pub(crate) fn mark_private_note_deployed(
        &mut self,
        pn_address: String,
        deposit_identifier_hash: String,
        deployed_at_unix: u64,
    ) -> Result<()> {
        self.pn_address = Some(pn_address);
        self.deposit_identifier_hash = Some(deposit_identifier_hash);
        self.deployed_at_unix = Some(deployed_at_unix);
        self.validate()
    }

    pub(crate) fn voucher_checkpoint(
        &self,
        kind: NoteDeployVoucherKind,
    ) -> Option<&NoteDeployVoucherCheckpoint> {
        match kind {
            NoteDeployVoucherKind::Deposit => self.deposit_voucher.as_ref(),
            NoteDeployVoucherKind::ShellGas => self.shell_voucher.as_ref(),
        }
    }

    pub(crate) fn set_voucher_checkpoint(
        &mut self,
        kind: NoteDeployVoucherKind,
        checkpoint: NoteDeployVoucherCheckpoint,
    ) -> Result<()> {
        let (token_type, raw_value, is_fee) = match kind {
            NoteDeployVoucherKind::Deposit => (self.token_type, self.raw_value, false),
            NoteDeployVoucherKind::ShellGas => (SHELL_ECC_ID, self.ecc_shell_deposit, true),
        };
        checkpoint.ensure_matches(
            kind,
            &self.owner_public_key_hex,
            token_type,
            raw_value,
            is_fee,
        )?;
        match kind {
            NoteDeployVoucherKind::Deposit => self.deposit_voucher = Some(checkpoint),
            NoteDeployVoucherKind::ShellGas => self.shell_voucher = Some(checkpoint),
        }
        self.validate()
    }

    pub(crate) fn mark_shell_funded_and_checked(&mut self) -> Result<()> {
        self.shell_funded = true;
        self.sanity_checked = true;
        self.validate()
    }

    pub(crate) fn ensure_ready_for_pool(&self) -> Result<()> {
        if self.pn_address.is_none() || self.deposit_identifier_hash.is_none() {
            bail!(
                "note deploy recovery state contains the owner key but no deployed PrivateNote address yet; \
                 refusing to write a pool entry or guess. Re-run `dexdo note deploy --recovery <this-file> \
                 --pool <pool>` to continue with the persisted owner key."
            );
        }
        if !self.shell_funded || !self.sanity_checked {
            bail!(
                "note deploy recovery state is not finalized for pooling (shell_funded={}, sanity_checked={}); \
                 re-run `dexdo note deploy --recovery <this-file> --pool <pool>` to resume before using \
                 `dexdo note recover`.",
                self.shell_funded,
                self.sanity_checked
            );
        }
        Ok(())
    }

    pub(crate) fn to_onboard_state(&self) -> Result<OnboardPnState> {
        self.validate()?;
        Ok(OnboardPnState {
            endpoint: self.endpoint.clone(),
            nominal: self.nominal.clone(),
            token_type: self.token_type,
            raw_value: self.raw_value,
            ecc_shell_deposit: self.ecc_shell_deposit,
            pn_address: self.pn_address.clone(),
            deposit_identifier_hash: self.deposit_identifier_hash.clone(),
            owner_public_key_hex: Some(self.owner_public_key_hex.clone()),
            owner_secret_key_hex: Some(self.owner_secret_key_hex.clone()),
            deployed_at_unix: self.deployed_at_unix,
            shell_funded: self.shell_funded,
            sanity_checked: self.sanity_checked,
        })
    }
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
pub(crate) fn default_note_deploy_recovery_path(pool: &Path) -> PathBuf {
    let mut path = pool.as_os_str().to_os_string();
    path.push(".recovery.json");
    PathBuf::from(path)
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
pub(crate) fn resolve_private_file_path(path: &Path, label: &str) -> Result<PathBuf> {
    let resolved = match std::fs::canonicalize(path) {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let parent = std::fs::canonicalize(parent).map_err(|e| {
                anyhow!(
                    "resolve parent directory for {label} {}: {e}",
                    path.display()
                )
            })?;
            let name = path
                .file_name()
                .ok_or_else(|| anyhow!("{label} path {} has no file name", path.display()))?;
            parent.join(name)
        }
        Err(e) => bail!("resolve {label} {}: {e}", path.display()),
    };

    match std::fs::symlink_metadata(&resolved) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(resolved),
        Ok(_) => bail!("{label} {} must resolve to a regular file", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(resolved),
        Err(e) => bail!("inspect {label} {}: {e}", path.display()),
    }
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
pub(crate) fn load_note_deploy_recovery(path: &Path) -> Result<Option<NoteDeployRecoveryState>> {
    let path = resolve_private_file_path(path, "note deploy recovery")?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => bail!("read note deploy recovery {}: {e}", path.display()),
    };
    let state: NoteDeployRecoveryState = serde_json::from_slice(&bytes).map_err(|e| {
        anyhow!(
            "note deploy recovery {} is not valid JSON: {e}",
            path.display()
        )
    })?;
    state.validate()?;
    Ok(Some(state))
}

struct NoteDeployRecoveryWriteLock {
    path: PathBuf,
}

impl Drop for NoteDeployRecoveryWriteLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn acquire_note_deploy_recovery_write_lock(
    recovery_path: &Path,
) -> Result<NoteDeployRecoveryWriteLock> {
    use std::io::Write;

    let mut lock_name = recovery_path.as_os_str().to_os_string();
    lock_name.push(".lock");
    let lock_path = PathBuf::from(lock_name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&lock_path) {
        Ok(mut lock) => {
            if let Err(e) = writeln!(lock, "{}", std::process::id()) {
                let _ = std::fs::remove_file(&lock_path);
                return Err(anyhow!(
                    "write note deploy recovery lock {}: {e}",
                    lock_path.display()
                ));
            }
            Ok(NoteDeployRecoveryWriteLock { path: lock_path })
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => bail!(
            "note deploy recovery {} is being updated by another process; refusing a concurrent overwrite. \
             Retry after that deploy exits; remove lock {} only after confirming no note deploy is running.",
            recovery_path.display(),
            lock_path.display()
        ),
        Err(e) => bail!(
            "create note deploy recovery lock {}: {e}",
            lock_path.display()
        ),
    }
}

fn load_existing_note_deploy_recovery_for_write(
    path: &Path,
) -> Result<Option<NoteDeployRecoveryState>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => bail!("read existing note deploy recovery {}: {e}", path.display()),
    };
    let state: NoteDeployRecoveryState = serde_json::from_slice(&bytes).map_err(|e| {
        anyhow!(
            "existing note deploy recovery {} is invalid JSON; refusing to overwrite it: {e}. \
             Preserve it and pass --recovery <different-file>.",
            path.display()
        )
    })?;
    state.validate().map_err(|e| {
        anyhow!(
            "existing note deploy recovery {} is invalid; refusing to overwrite it: {e}. \
             Preserve it and pass --recovery <different-file>.",
            path.display()
        )
    })?;
    Ok(Some(state))
}

fn same_note_deploy_owner(left: &NoteDeployRecoveryState, right: &NoteDeployRecoveryState) -> bool {
    left.owner_public_key_hex == right.owner_public_key_hex
        && left.owner_secret_key_hex == right.owner_secret_key_hex
}

fn note_deploy_request_fields_match(
    left: &NoteDeployRecoveryState,
    right: &NoteDeployRecoveryState,
) -> bool {
    left.endpoint == right.endpoint
        && left.nominal == right.nominal
        && left.token_type == right.token_type
        && left.raw_value == right.raw_value
        && left.ecc_shell_deposit == right.ecc_shell_deposit
        && left.funding_multisig_address == right.funding_multisig_address
}

fn note_deploy_recovery_has_no_possible_spend(state: &NoteDeployRecoveryState) -> bool {
    fn voucher_has_no_possible_spend(voucher: Option<&NoteDeployVoucherCheckpoint>) -> bool {
        voucher.is_none_or(|voucher| {
            !voucher.submit_maybe_sent && voucher.event.is_none() && voucher.proof.is_none()
        })
    }

    state.pn_address.is_none()
        && state.deposit_identifier_hash.is_none()
        && state.deployed_at_unix.is_none()
        && !state.shell_funded
        && !state.sanity_checked
        && voucher_has_no_possible_spend(state.deposit_voucher.as_ref())
        && voucher_has_no_possible_spend(state.shell_voucher.as_ref())
}

fn ensure_same_recovery_can_advance(
    path: &Path,
    existing: &NoteDeployRecoveryState,
    next: &NoteDeployRecoveryState,
) -> Result<()> {
    if !same_note_deploy_owner(existing, next) || !note_deploy_request_fields_match(existing, next)
    {
        bail!(
            "note deploy recovery {} belongs to a different deploy owner or request; refusing to overwrite it. \
             Resume the existing state or pass --recovery <different-file>.",
            path.display()
        );
    }
    if existing
        .pn_address
        .as_ref()
        .is_some_and(|address| next.pn_address.as_ref() != Some(address))
        || existing
            .deposit_identifier_hash
            .as_ref()
            .is_some_and(|hash| next.deposit_identifier_hash.as_ref() != Some(hash))
    {
        bail!(
            "note deploy recovery {} already holds a different deployed PrivateNote identity; refusing to \
             clobber its recovery key. Pass --recovery <different-file>.",
            path.display()
        );
    }
    Ok(())
}

fn write_note_deploy_recovery_locked(path: &Path, state: &NoteDeployRecoveryState) -> Result<()> {
    let json = serde_json::to_vec_pretty(state)?;
    write_private_atomic(path, &json)
        .map_err(|e| anyhow!("write note deploy recovery {}: {e}", path.display()))
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
pub(crate) fn write_note_deploy_recovery(
    path: &Path,
    state: &NoteDeployRecoveryState,
) -> Result<()> {
    state.validate()?;
    let path = resolve_private_file_path(path, "note deploy recovery")?;
    let _lock = acquire_note_deploy_recovery_write_lock(&path)?;
    if let Some(existing) = load_existing_note_deploy_recovery_for_write(&path)? {
        ensure_same_recovery_can_advance(&path, &existing, state)?;
    }
    write_note_deploy_recovery_locked(&path, state)
}

/// Refresh the recovery file only after the deployed note's on-chain owner was validated.
/// A different recorded note or a different owner's possibly submitted spend is never overwritten.
#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
pub(crate) fn refresh_note_deploy_recovery_after_success(
    path: &Path,
    state: &NoteDeployRecoveryState,
) -> Result<()> {
    state.validate()?;
    state.ensure_ready_for_pool()?;
    let path = resolve_private_file_path(path, "note deploy recovery")?;
    let _lock = acquire_note_deploy_recovery_write_lock(&path)?;
    if let Some(existing) = load_existing_note_deploy_recovery_for_write(&path)? {
        if same_note_deploy_owner(&existing, state) {
            ensure_same_recovery_can_advance(&path, &existing, state)?;
        } else if let Some(existing_note) = existing.pn_address.as_deref() {
            if state.pn_address.as_deref() != Some(existing_note) {
                bail!(
                    "note deploy recovery {} already holds recovery for different deployed PrivateNote \
                     {existing_note}; refusing to clobber its only recovery key. Keep this file and pass \
                     --recovery <different-file> for the successful deploy.",
                    path.display()
                );
            }
        } else if !note_deploy_recovery_has_no_possible_spend(&existing) {
            bail!(
                "note deploy recovery {} holds possible wallet-spend recovery material for a different owner; \
                 refusing to clobber it. Resume that attempt with this file, or pass \
                 --recovery <different-file> for the successful deploy.",
                path.display()
            );
        }
    }
    write_note_deploy_recovery_locked(&path, state)
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
pub(crate) fn ensure_recovery_owner_matches_target_note(
    path: &Path,
    state: &NoteDeployRecoveryState,
    onchain_owner_pubkey: Option<&str>,
) -> Result<()> {
    state.validate()?;
    let note_addr = state.pn_address.as_deref().ok_or_else(|| {
        anyhow!(
            "note recovery {} has no target PrivateNote address; refusing to guess",
            path.display()
        )
    })?;
    let derived_owner = derive_owner_pubkey_from_secret_hex(&state.owner_secret_key_hex)?;
    ensure_onchain_owner_matches_pool_key(
        "note recover",
        note_addr,
        onchain_owner_pubkey,
        &derived_owner,
    )
    .map_err(|e| {
        anyhow!(
            "{e} Recovery file {} was left unchanged because its owner key does not own target PrivateNote \
             {note_addr}; pass the recovery file that belongs to this note.",
            path.display()
        )
    })
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
pub(crate) fn recovery_owner_key_written_message(path: &Path) -> String {
    format!(
        "note deploy recovery: owner key persisted to {} (0600) before wallet spend. If interrupted before \
         recovery is finalized, rerun `dexdo note deploy --recovery {} --pool <pool>`; if recovery is already \
         finalized but pn_pool.json is missing, run `dexdo note recover --recovery {} --pool <pool>`.",
        path.display(),
        path.display(),
        path.display()
    )
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
pub(crate) fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let path = resolve_private_file_path(path, "secret file")?;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("secret.json");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow!("system clock before epoch: {e}"))?
        .as_nanos();
    let tmp = dir.join(format!(".{name}.tmp.{}.{nanos}", std::process::id()));
    write_private_atomic_via_temp(&path, &tmp, bytes)
}

#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
pub(crate) fn write_private_atomic_via_temp(path: &Path, tmp: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(tmp)
        .map_err(|e| anyhow!("create temp secret file {}: {e}", tmp.display()))?;
    if let Err(e) = f.write_all(bytes).and_then(|()| f.sync_all()) {
        let _ = std::fs::remove_file(tmp);
        return Err(anyhow!("write temp secret file {}: {e}", tmp.display()));
    }
    std::fs::rename(tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(tmp);
        anyhow!("rename temp secret file into {}: {e}", path.display())
    })?;
    sync_parent_dir(path)?;
    Ok(())
}

pub(crate) fn sync_parent_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::File::open(dir)
            .and_then(|f| f.sync_all())
            .map_err(|e| anyhow!("fsync parent directory {}: {e}", dir.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn normalize_secret_hex(secret_hex: &str) -> Result<String> {
    let secret = secret_hex
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    let bytes = hex::decode(secret)
        .map_err(|e| anyhow!("owner_secret_key_hex must be 32-byte hex: {e}"))?;
    if bytes.len() != 32 {
        bail!("owner_secret_key_hex must be exactly 32 bytes");
    }
    Ok(secret.to_ascii_lowercase())
}

fn normalize_secret_like_hex(raw: &str, label: &str) -> Result<String> {
    let value = raw.trim().trim_start_matches("0x").trim_start_matches("0X");
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("{label} must be a 32-byte hex value");
    }
    Ok(value.to_ascii_lowercase())
}

fn validate_hex_u256(raw: &str, label: &str) -> Result<()> {
    normalize_secret_like_hex(raw, label).map(|_| ())
}

/// CLI-compatible note deploy state. A subset of its fields — exactly those the pool needs. **Carries the owner
/// secret key** — never log it.
///
/// `allow(dead_code)` off `shellnet`: the only non-test consumer (`run_note_deploy`) is shellnet-gated, and the
/// `cfg(test)` §5 suite does not save these from clippy's non-test `-D warnings` pass on the default bin.
#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OnboardPnState {
    pub endpoint: String,
    pub nominal: String,
    pub token_type: u32,
    pub raw_value: u64,
    pub ecc_shell_deposit: u64,
    pub pn_address: Option<String>,
    pub deposit_identifier_hash: Option<String>,
    pub owner_public_key_hex: Option<String>,
    pub owner_secret_key_hex: Option<String>,
    pub deployed_at_unix: Option<u64>,
    pub shell_funded: bool,
    pub sanity_checked: bool,
}

#[cfg(feature = "shellnet")]
impl From<dexdo_core::private_note::DeployPrivateNoteResult> for OnboardPnState {
    fn from(s: dexdo_core::private_note::DeployPrivateNoteResult) -> Self {
        Self {
            endpoint: s.endpoint,
            nominal: s.nominal,
            token_type: s.token_type,
            raw_value: s.raw_value,
            ecc_shell_deposit: s.ecc_shell_deposit,
            pn_address: Some(s.pn_address),
            deposit_identifier_hash: Some(s.deposit_identifier_hash),
            owner_public_key_hex: Some(s.owner_public_key_hex),
            owner_secret_key_hex: Some(s.owner_secret_key_hex),
            deployed_at_unix: Some(s.deployed_at_unix),
            shell_funded: s.shell_funded,
            sanity_checked: s.sanity_checked,
        }
    }
}

/// #137/#176 output adapter: build a single DEXDO_PN_POOL **note** object from a fully deployed note state. Fails
/// loud if deploy did not complete (missing `pn_address`/keys, or not `shell_funded`/`sanity_checked`) — folding a
/// half-deployed note into the pool would later strand the `seller`/`buyer` on an unusable note.
#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
pub(crate) fn pn_state_to_pool_note(s: &OnboardPnState) -> Result<Value> {
    let address = s.pn_address.as_deref().ok_or_else(|| {
        anyhow!("pn_state has no pn_address — note deploy did not reach deployPrivateNote (step 1)")
    })?;
    let dih = s.deposit_identifier_hash.as_deref().ok_or_else(|| {
        anyhow!("pn_state has no deposit_identifier_hash — incomplete note deploy")
    })?;
    let pubkey = s
        .owner_public_key_hex
        .as_deref()
        .ok_or_else(|| anyhow!("pn_state has no owner_public_key_hex — incomplete note deploy"))?;
    let seckey = s
        .owner_secret_key_hex
        .as_deref()
        .ok_or_else(|| anyhow!("pn_state has no owner_secret_key_hex — incomplete note deploy"))?;
    ensure_pool_note_keypair_matches(address, pubkey, seckey)?;
    if !s.shell_funded || !s.sanity_checked {
        bail!(
            "note deploy state not fully deployed (shell_funded={}, sanity_checked={}) — the PN has no gas / failed its \
             getDetails check; re-run `dexdo note deploy` (idempotent at the step boundary) before pooling it.",
            s.shell_funded,
            s.sanity_checked
        );
    }
    Ok(json!({
        "address": address,
        "deposit_identifier_hash": dih,
        "owner_public_key_hex": pubkey,
        "owner_secret_key_hex": seckey,
        "deployed_at_unix": s.deployed_at_unix.unwrap_or(0),
        "shell_funded": s.shell_funded,
        "native_funded": s.sanity_checked,
    }))
}

/// #137/#176 output adapter: append `note` to a `DEXDO_PN_POOL` JSON, creating the pool with the pool-level fields
/// from the deploy state (endpoint/nominal/token_type/raw_value/ecc) when it does not yet exist, or appending to an
/// existing matching pool. Refuses to mix nominals/token-types in one pool (the consumers assume a homogeneous
/// pool), and refuses to add a duplicate note `address`. Pure (takes the existing pool JSON, returns the new one).
#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
pub(crate) fn pool_with_note_added(
    existing: Option<Value>,
    s: &OnboardPnState,
    note: Value,
    created_at_unix: u64,
    funding_multisig_address: &str,
) -> Result<Value> {
    let funding_multisig_address = dexdo_core::normalize_wallet_address(funding_multisig_address)
        .map_err(|e| anyhow!("{e}"))?;
    let mut pool = match existing {
        Some(p) => p,
        None => json!({
            "endpoint": s.endpoint,
            "created_at_unix": created_at_unix,
            "nominal": s.nominal,
            "token_type": s.token_type,
            "raw_value_per_pn": s.raw_value,
            "ecc_shell_deposit_per_pn": s.ecc_shell_deposit,
            "funding_multisig_address": funding_multisig_address,
            "notes": [],
        }),
    };
    // Homogeneity: a pool is one nominal + token_type (the seller/buyer pick any note assuming uniform value).
    if pool["nominal"] != json!(s.nominal) || pool["token_type"] != json!(s.token_type) {
        bail!(
            "pool nominal/token_type ({}/{}) != this note's ({}/{}): the consumers assume a homogeneous pool — \
             use a separate --pool file per nominal/token-type.",
            pool["nominal"],
            pool["token_type"],
            s.nominal,
            s.token_type
        );
    }
    match pool.get("funding_multisig_address").and_then(Value::as_str) {
        Some(existing) => {
            let existing = dexdo_core::normalize_wallet_address(existing).map_err(|e| {
                anyhow!("--pool: malformed funding_multisig_address `{existing}`: {e}")
            })?;
            if existing != funding_multisig_address {
                bail!(
                    "pool funding_multisig_address {existing} != this note's {funding_multisig_address}: \
                     rewards provenance must not mix PrivateNotes funded by different multisigs. Use a separate \
                     --pool file for each funding multisig."
                );
            }
            pool["funding_multisig_address"] = json!(existing);
        }
        None => {
            let has_existing_notes = pool["notes"]
                .as_array()
                .map(|notes| !notes.is_empty())
                .unwrap_or(false);
            if has_existing_notes {
                bail!(
                    "--pool has existing notes but no funding_multisig_address: refusing to attach new rewards \
                     provenance to older notes of unknown origin. Create a fresh --pool or migrate the old pool \
                     explicitly after verifying its funding multisig."
                );
            }
            pool["funding_multisig_address"] = json!(funding_multisig_address);
        }
    }
    let notes = pool["notes"]
        .as_array_mut()
        .ok_or_else(|| anyhow!("--pool: malformed (\"notes\" is not an array)"))?;
    let new_addr = note["address"].as_str().unwrap_or_default();
    if notes
        .iter()
        .any(|n| n["address"].as_str() == Some(new_addr))
    {
        bail!("note {new_addr} is already in the pool — refusing to add a duplicate");
    }
    notes.push(note);
    Ok(pool)
}

#[allow(dead_code)]
pub(crate) fn pool_with_note_token_contract_recorded(
    mut pool: Value,
    note_addr: &str,
    token_contract: &str,
    role: &str,
    updated_at_unix: u64,
) -> Result<Value> {
    if role != "buyer" && role != "seller" {
        bail!("token_contract_role must be buyer or seller, got `{role}`");
    }
    let note_addr = dexdo_core::normalize_wallet_address(note_addr)
        .map_err(|e| anyhow!("note address {note_addr}: {e}"))?;
    let token_contract = dexdo_core::normalize_wallet_address(token_contract)
        .map_err(|e| anyhow!("token_contract {token_contract}: {e}"))?;
    let notes = pool["notes"]
        .as_array_mut()
        .ok_or_else(|| anyhow!("DEXDO_PN_POOL: malformed (\"notes\" is not an array)"))?;
    let mut matched = 0usize;
    for note in notes {
        let Some(address) = note["address"].as_str() else {
            continue;
        };
        let normalized = dexdo_core::normalize_wallet_address(address)
            .unwrap_or_else(|_| address.trim().to_ascii_lowercase());
        if normalized == note_addr {
            matched += 1;
            note["address"] = json!(note_addr);
            note["token_contract"] = json!(token_contract);
            note["token_contract_role"] = json!(role);
            note["token_contract_updated_at_unix"] = json!(updated_at_unix);
        }
    }
    match matched {
        1 => Ok(pool),
        0 => bail!(
            "DEXDO_PN_POOL has no note entry for {note_addr}; refusing to claim TokenContract recovery metadata \
             was persisted"
        ),
        _ => bail!(
            "DEXDO_PN_POOL has {matched} entries for note {note_addr}; refusing ambiguous TokenContract metadata"
        ),
    }
}

#[allow(dead_code)]
pub(crate) fn pool_has_unique_note_entry(pool: &Value, note_addr: &str) -> Result<()> {
    let note_addr = dexdo_core::normalize_wallet_address(note_addr)
        .map_err(|e| anyhow!("note address {note_addr}: {e}"))?;
    let notes = pool["notes"]
        .as_array()
        .ok_or_else(|| anyhow!("DEXDO_PN_POOL: malformed (\"notes\" is not an array)"))?;
    let matched = notes
        .iter()
        .filter_map(|note| note["address"].as_str())
        .filter(|address| {
            dexdo_core::normalize_wallet_address(address)
                .unwrap_or_else(|_| address.trim().to_ascii_lowercase())
                == note_addr
        })
        .count();
    match matched {
        1 => Ok(()),
        0 => bail!("DEXDO_PN_POOL has no note entry for {note_addr}"),
        _ => bail!("DEXDO_PN_POOL has {matched} entries for note {note_addr}"),
    }
}

#[allow(dead_code)]
pub(crate) fn pool_note_recovery_records(
    pool: &Value,
) -> Result<Vec<(String, String, String, String)>> {
    let notes = pool["notes"]
        .as_array()
        .ok_or_else(|| anyhow!("DEXDO_PN_POOL: malformed (\"notes\" is not an array)"))?;
    let mut out = Vec::new();
    for note in notes {
        let Some(note_addr) = note["address"].as_str() else {
            continue;
        };
        let Some(owner_secret) = note["owner_secret_key_hex"].as_str() else {
            continue;
        };
        let Some(token_contract) = note["token_contract"].as_str() else {
            continue;
        };
        let role = note["token_contract_role"].as_str().unwrap_or("unknown");
        if role != "buyer" && role != "seller" && role != "unknown" {
            bail!(
                "DEXDO_PN_POOL token_contract_role must be buyer, seller, or unknown, got `{role}`"
            );
        }
        out.push((
            dexdo_core::normalize_wallet_address(note_addr)
                .map_err(|e| anyhow!("DEXDO_PN_POOL note address {note_addr}: {e}"))?,
            owner_secret.to_string(),
            dexdo_core::normalize_wallet_address(token_contract)
                .map_err(|e| anyhow!("DEXDO_PN_POOL token_contract {token_contract}: {e}"))?,
            role.to_string(),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod note_deploy_tests {
    use super::*;

    fn fixture_secret_hex() -> String {
        "2a".repeat(32)
    }

    fn complete_state() -> OnboardPnState {
        let secret = fixture_secret_hex();
        let public = derive_owner_pubkey_from_secret_hex(&secret).expect("fixture key derives");
        OnboardPnState {
            endpoint: "shellnet.ackinacki.org".into(),
            nominal: "N100".into(),
            token_type: 1,
            raw_value: 100_000_000_000,
            ecc_shell_deposit: 100_000_000_000,
            pn_address: Some("0:abc".into()),
            deposit_identifier_hash: Some("123".into()),
            owner_public_key_hex: Some(public),
            owner_secret_key_hex: Some(secret),
            deployed_at_unix: Some(1000),
            shell_funded: true,
            sanity_checked: true,
        }
    }

    #[cfg(feature = "shellnet")]
    fn tvm_tonos_fixture_phrase() -> String {
        const WORD_INDICES: [u16; 12] = [
            1636, 1293, 905, 102, 1057, 1956, 1247, 1750, 597, 881, 1302, 3,
        ];
        WORD_INDICES
            .iter()
            .map(|i| bip39::Language::English.wordlist().get_word((*i).into()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn recovery_request<'a>(
        endpoint: &'a str,
        funding_multisig_address: &'a str,
    ) -> NoteDeployRecoveryRequest<'a> {
        NoteDeployRecoveryRequest {
            endpoint,
            nominal: "N100",
            token_type: 1,
            raw_value: 100_000_000_000,
            ecc_shell_deposit: 100_000_000_000,
            funding_multisig_address,
        }
    }

    fn complete_recovery_state() -> NoteDeployRecoveryState {
        let state = complete_state();
        NoteDeployRecoveryState {
            version: NOTE_DEPLOY_RECOVERY_VERSION,
            endpoint: state.endpoint,
            nominal: state.nominal,
            token_type: state.token_type,
            raw_value: state.raw_value,
            ecc_shell_deposit: state.ecc_shell_deposit,
            funding_multisig_address: format!("0:{}", "a".repeat(64)),
            owner_public_key_hex: state.owner_public_key_hex.unwrap(),
            owner_secret_key_hex: state.owner_secret_key_hex.unwrap(),
            pn_address: state.pn_address,
            deposit_identifier_hash: state.deposit_identifier_hash,
            deployed_at_unix: state.deployed_at_unix,
            deposit_voucher: None,
            shell_voucher: None,
            shell_funded: state.shell_funded,
            sanity_checked: state.sanity_checked,
        }
    }

    fn recovery_state_for_owner(
        secret: &str,
        note_address: Option<&str>,
    ) -> NoteDeployRecoveryState {
        let mut state = complete_recovery_state();
        state.owner_secret_key_hex = secret.to_string();
        state.owner_public_key_hex = derive_owner_pubkey_from_secret_hex(secret).unwrap();
        state.pn_address = note_address.map(ToOwned::to_owned);
        state.deposit_identifier_hash =
            note_address.map(|address| address.trim_start_matches("0:").chars().take(64).collect());
        state.deployed_at_unix = note_address.map(|_| 1000);
        state.shell_funded = note_address.is_some();
        state.sanity_checked = note_address.is_some();
        state.validate().unwrap();
        state
    }

    struct TempDirCleanup(std::path::PathBuf);

    impl Drop for TempDirCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir(prefix: &str) -> (std::path::PathBuf, TempDirCleanup) {
        let dir = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        (dir.clone(), TempDirCleanup(dir))
    }

    /// #205: account-reader data formats SHELL/ECC[2] and native gas in readable units plus raw units.
    #[test]
    fn note_balance_formats_shell_and_native_balances() {
        let view = build_note_balance_view(
            "0:abc",
            Some(NoteAccountSnapshot {
                address: "0:abc".into(),
                status: "Active".into(),
                native_raw: 5_000_000_123,
                ecc: vec![(7, 42), (2, 1_234_567_890)],
                code_hash: Some("cafe".into()),
            }),
            NoteGetterBalanceMaps {
                balance: NoteBalanceMap::Known(vec![(2, 2_000_000_001), (1, 10)]),
                locked_in_orders: NoteBalanceMap::Unknown("getter unavailable".into()),
            },
        )
        .unwrap();
        let out = render_note_balance(&view);
        assert!(out.contains("PrivateNote 0:abc"), "{out}");
        assert!(
            out.contains("SHELL ECC[2]: 1.234567890 SHELL (raw 1234567890)"),
            "{out}"
        );
        assert!(
            out.contains("VMSHELL native gas: 5.000000123 vmshell (raw 5000000123)"),
            "{out}"
        );
        assert!(
            out.contains("ECC[2] SHELL: 2.000000001 SHELL (raw 2000000001)"),
            "{out}"
        );
        assert!(out.contains("ECC[7]: raw 42"), "{out}");
        assert!(out.contains("unknown (getter unavailable)"), "{out}");
    }

    /// #205 negative: a null/unreadable account is not rendered as zero.
    #[test]
    fn note_balance_null_account_fails_loud() {
        let err = build_note_balance_view(
            "0:missing",
            None,
            unknown_note_getter_balance_maps("not queried"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("cannot read PrivateNote account"), "{err}");
        assert!(err.contains("refusing to report zero"), "{err}");
    }

    /// #205: `getDetails` maps preserve unknown vs empty and parse known token maps.
    #[test]
    fn note_balance_getter_maps_preserve_unknown() {
        let maps = note_getter_balance_maps(Some(&json!({
            "balance": {"2": "3000000000", "7": "9"},
            "lockedInOrders": null
        })));
        assert_eq!(
            maps.balance,
            NoteBalanceMap::Known(vec![(2, 3_000_000_000), (7, 9)])
        );
        assert!(matches!(maps.locked_in_orders, NoteBalanceMap::Unknown(_)));

        let maps = note_getter_balance_maps(Some(&json!({
            "balance": {},
            "lockedInOrders": {}
        })));
        assert_eq!(maps.balance, NoteBalanceMap::Known(vec![]));
        assert_eq!(maps.locked_in_orders, NoteBalanceMap::Known(vec![]));
    }

    /// #205: the command body is read-only and address-only: no key read and no signed/write helper.
    #[test]
    fn note_balance_command_path_is_read_only() {
        let source = include_str!("commands.rs");
        let start = source
            .find("pub(crate) async fn run_note_balance")
            .expect("run_note_balance present");
        let end = source[start..]
            .find("/// `dexdo note withdraw`")
            .map(|offset| start + offset)
            .expect("run_note_balance end marker present");
        let body = &source[start..end];
        assert!(body.contains(".get_account("), "{body}");
        assert!(body.contains(".private_note_details("), "{body}");
        for forbidden in [
            "read_secret_hex",
            "note_key",
            "KeyPair",
            ".submit(",
            ".call(",
            "withdraw_note_tokens",
        ] {
            assert!(
                !body.contains(forbidden),
                "run_note_balance contains forbidden write/key path {forbidden}: {body}"
            );
        }
    }

    /// #137/#176 §5: a fully deployed note state maps to the exact pool note schema the seller/buyer consume.
    #[test]
    fn pn_state_to_note_exact_schema() {
        let state = complete_state();
        let public = state.owner_public_key_hex.clone().unwrap();
        let secret = state.owner_secret_key_hex.clone().unwrap();
        let n = pn_state_to_pool_note(&state).unwrap();
        assert_eq!(n["address"], "0:abc");
        assert_eq!(n["deposit_identifier_hash"], "123");
        assert_eq!(n["owner_public_key_hex"].as_str(), Some(public.as_str()));
        assert_eq!(n["owner_secret_key_hex"].as_str(), Some(secret.as_str()));
        assert_eq!(n["deployed_at_unix"], 1000);
        assert_eq!(n["shell_funded"], true);
        assert_eq!(n["native_funded"], true);
    }

    /// #137/#176 §5 (negatives): an incomplete deploy state fails loud — never pooled.
    #[test]
    fn incomplete_onboard_fails_loud() {
        let mut s = complete_state();
        s.pn_address = None;
        assert!(pn_state_to_pool_note(&s)
            .unwrap_err()
            .to_string()
            .contains("pn_address"));
        let mut s = complete_state();
        s.shell_funded = false;
        assert!(pn_state_to_pool_note(&s)
            .unwrap_err()
            .to_string()
            .contains("not fully deployed"));
        let mut s = complete_state();
        s.sanity_checked = false;
        assert!(pn_state_to_pool_note(&s)
            .unwrap_err()
            .to_string()
            .contains("not fully deployed"));
    }

    /// #19/#338 regression: a pool entry whose stored secret cannot derive the recorded owner pubkey is
    /// rejected before the bad DEXDO_PN_POOL entry is serialized. Without this, later owner-signed writes fail
    /// opaquely with ERR_INVALID_SENDER 101.
    #[test]
    fn pn_state_to_note_rejects_owner_secret_public_mismatch() {
        let mut s = complete_state();
        s.owner_public_key_hex = Some("11".repeat(32));

        let err = pn_state_to_pool_note(&s).unwrap_err().to_string();

        assert!(err.contains("DEXDO_PN_POOL"), "{err}");
        assert!(err.contains("owner_secret_key_hex derives pubkey"), "{err}");
        assert!(err.contains("ERR_INVALID_SENDER 101"), "{err}");
        assert!(err.contains("--pool <new_file>"), "{err}");
    }

    /// #19/#338 regression: deploy must compare the freshly deployed PrivateNote's on-chain owner key
    /// (`getDetails().ephemeralPubkey`) against the saved pool key before writing the pool file.
    #[test]
    fn onchain_owner_check_rejects_mismatched_pool_key() {
        let derived = derive_owner_pubkey_from_secret_hex(&fixture_secret_hex()).unwrap();
        assert!(ensure_onchain_owner_matches_pool_key(
            "note deploy",
            "0:abc",
            Some(&format!("0x{}", derived.to_ascii_uppercase())),
            &derived,
        )
        .is_ok());

        let err = ensure_onchain_owner_matches_pool_key(
            "note deploy",
            "0:abc",
            Some(&format!("0x{}", "11".repeat(32))),
            &derived,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("_ephemeralPubkey"), "{err}");
        assert!(err.contains("provision/sell/withdraw"), "{err}");
        assert!(err.contains("ERR_INVALID_SENDER 101"), "{err}");
        assert!(err.contains("--pool <new_file>"), "{err}");
    }

    /// #137 §5: a fresh pool is created from the pn_state pool-level fields; a second note appends.
    #[test]
    fn pool_create_then_append() {
        let s = complete_state();
        let n1 = pn_state_to_pool_note(&s).unwrap();
        let wallet = format!("0:{}", "a".repeat(64));
        let pool = pool_with_note_added(None, &s, n1, 42, &wallet).unwrap();
        assert_eq!(pool["nominal"], "N100");
        assert_eq!(pool["raw_value_per_pn"], 100_000_000_000u64);
        assert_eq!(pool["funding_multisig_address"], wallet);
        assert_eq!(pool["notes"].as_array().unwrap().len(), 1);

        let mut s2 = complete_state();
        s2.pn_address = Some("0:def".into());
        let n2 = pn_state_to_pool_note(&s2).unwrap();
        let pool = pool_with_note_added(Some(pool), &s2, n2, 43, &wallet).unwrap();
        assert_eq!(pool["funding_multisig_address"], wallet);
        assert_eq!(pool["notes"].as_array().unwrap().len(), 2);
    }

    /// #338 residual: the pool entry itself carries the current TokenContract so buyer recovery/reclaim does not
    /// depend on a side manifest or scraped logs.
    #[test]
    fn pool_records_token_contract_next_to_note_entry() {
        let mut s = complete_state();
        s.pn_address = Some(format!("0:{}", "1".repeat(64)));
        let wallet = format!("0:{}", "a".repeat(64));
        let pool =
            pool_with_note_added(None, &s, pn_state_to_pool_note(&s).unwrap(), 1, &wallet).unwrap();
        let note_addr = s.pn_address.as_deref().unwrap();
        let tc = format!("0:{}", "b".repeat(64));

        let pool =
            pool_with_note_token_contract_recorded(pool, note_addr, &tc, "buyer", 99).unwrap();

        let note = &pool["notes"].as_array().unwrap()[0];
        assert_eq!(note["token_contract"], tc);
        assert_eq!(note["token_contract_role"], "buyer");
        assert_eq!(note["token_contract_updated_at_unix"], 99);
        assert_eq!(
            pool_note_recovery_records(&pool).unwrap(),
            vec![(
                note_addr.to_string(),
                s.owner_secret_key_hex.clone().unwrap(),
                tc,
                "buyer".to_string(),
            )]
        );
    }

    /// #338 negative: do not silently claim recovery metadata was persisted if the active pool is not the note's
    /// pool.
    #[test]
    fn pool_token_contract_record_requires_matching_note() {
        let mut s = complete_state();
        s.pn_address = Some(format!("0:{}", "1".repeat(64)));
        let wallet = format!("0:{}", "a".repeat(64));
        let pool =
            pool_with_note_added(None, &s, pn_state_to_pool_note(&s).unwrap(), 1, &wallet).unwrap();
        let err = pool_with_note_token_contract_recorded(
            pool,
            &format!("0:{}", "c".repeat(64)),
            &format!("0:{}", "b".repeat(64)),
            "buyer",
            99,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no note entry"), "{err}");
    }

    #[test]
    fn pool_note_entry_preflight_requires_unique_note() {
        let mut s = complete_state();
        s.pn_address = Some(format!("0:{}", "1".repeat(64)));
        let wallet = format!("0:{}", "a".repeat(64));
        let pool =
            pool_with_note_added(None, &s, pn_state_to_pool_note(&s).unwrap(), 1, &wallet).unwrap();
        pool_has_unique_note_entry(&pool, s.pn_address.as_deref().unwrap()).unwrap();
        let err = pool_has_unique_note_entry(&pool, &format!("0:{}", "c".repeat(64)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no note entry"), "{err}");
    }

    /// #137 §5 (negatives): duplicate address + mixed nominal are refused.
    #[test]
    fn pool_refuses_duplicate_and_mixed() {
        let s = complete_state();
        let wallet = format!("0:{}", "a".repeat(64));
        let pool =
            pool_with_note_added(None, &s, pn_state_to_pool_note(&s).unwrap(), 1, &wallet).unwrap();
        // duplicate address
        let dup = pn_state_to_pool_note(&s).unwrap();
        assert!(
            pool_with_note_added(Some(pool.clone()), &s, dup, 2, &wallet)
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
        // mixed nominal
        let mut s2 = complete_state();
        s2.nominal = "N1000".into();
        s2.pn_address = Some("0:xyz".into());
        let n2 = pn_state_to_pool_note(&s2).unwrap();
        assert!(pool_with_note_added(Some(pool), &s2, n2, 3, &wallet)
            .unwrap_err()
            .to_string()
            .contains("homogeneous pool"));
    }

    /// #170: rewards provenance is root-level and cannot silently mix funding multisigs or backfill legacy pools.
    #[test]
    fn pool_records_and_guards_funding_multisig_provenance() {
        let s = complete_state();
        let h1 = "1".repeat(64);
        let h2 = "B".repeat(64);
        let wallet_half_form = format!("{h1}::{h2}");
        let wallet = format!("0:{}", h2.to_ascii_lowercase());
        let other_wallet = format!("0:{}", "c".repeat(64));

        let pool = pool_with_note_added(
            None,
            &s,
            pn_state_to_pool_note(&s).unwrap(),
            1,
            &wallet_half_form,
        )
        .unwrap();
        assert_eq!(pool["funding_multisig_address"], wallet);

        let mut s2 = complete_state();
        s2.pn_address = Some("0:def".into());
        let err = pool_with_note_added(
            Some(pool.clone()),
            &s2,
            pn_state_to_pool_note(&s2).unwrap(),
            2,
            &other_wallet,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("funding_multisig_address"), "{err}");

        let mut legacy = pool;
        legacy
            .as_object_mut()
            .unwrap()
            .remove("funding_multisig_address");
        let err = pool_with_note_added(
            Some(legacy),
            &s2,
            pn_state_to_pool_note(&s2).unwrap(),
            3,
            &wallet,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown origin"), "{err}");
    }

    /// #204: the funding wallet seed phrase is an input-only credential. The pool stores only the deployed note
    /// material the runtime consumes; seed words must not appear in serialized pool output.
    #[cfg(feature = "shellnet")]
    #[test]
    fn pool_output_does_not_contain_seed_words() {
        let phrase = tvm_tonos_fixture_phrase();
        let derived = dexdo::wallet_seed::derive_multisig_key_from_seed_phrase(&phrase).unwrap();
        let mut state = complete_state();
        state.owner_public_key_hex = Some(derived.public_hex().to_string());
        state.owner_secret_key_hex = Some(derived.secret_hex().to_string());
        let wallet = format!("0:{}", "a".repeat(64));
        let pool = pool_with_note_added(
            None,
            &state,
            pn_state_to_pool_note(&state).unwrap(),
            1,
            &wallet,
        )
        .unwrap();
        let json = serde_json::to_string(&pool).unwrap();
        for word in phrase.split_whitespace() {
            assert!(!json.contains(word), "pool output contains a seed word");
        }
    }

    /// #344: the recovery file is the durable owner-key copy and must be private, atomic JSON.
    #[test]
    fn recovery_file_writes_owner_key_with_private_mode() {
        let (dir, _cleanup) = temp_dir("dexdo-note-recovery-test");
        let path = dir.join("pn_pool.json.recovery.json");
        let state = NoteDeployRecoveryState::new(
            recovery_request(
                "https://shellnet.ackinacki.org",
                &format!("0:{}", "a".repeat(64)),
            ),
            &derive_owner_pubkey_from_secret_hex(&fixture_secret_hex()).unwrap(),
            &fixture_secret_hex(),
        )
        .unwrap();

        write_note_deploy_recovery(&path, &state).unwrap();
        let loaded = load_note_deploy_recovery(&path).unwrap().unwrap();

        assert_eq!(loaded.owner_secret_key_hex, fixture_secret_hex());
        assert_eq!(loaded.pn_address, None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "recovery file must be 0600");
        }
    }

    /// Public #33 regression: a successful deploy may replace an owner-only stale attempt because no wallet
    /// spend or live note can depend on that old key.
    #[test]
    fn successful_deploy_refreshes_stale_unspent_recovery_owner() {
        let (dir, _cleanup) = temp_dir("dexdo-note-recovery-refresh-test");
        let path = dir.join("pn_pool.json.recovery.json");
        let stale = recovery_state_for_owner(&"31".repeat(32), None);
        let successful =
            recovery_state_for_owner(&"42".repeat(32), Some(&format!("0:{}", "2".repeat(64))));
        write_note_deploy_recovery(&path, &stale).unwrap();

        refresh_note_deploy_recovery_after_success(&path, &successful).unwrap();
        let loaded = load_note_deploy_recovery(&path).unwrap().unwrap();

        assert_eq!(loaded.owner_public_key_hex, successful.owner_public_key_hex);
        assert_eq!(loaded.owner_secret_key_hex, successful.owner_secret_key_hex);
        assert_eq!(loaded.pn_address, successful.pn_address);
        assert_eq!(
            loaded.deposit_identifier_hash,
            successful.deposit_identifier_hash
        );
    }

    /// Public #33 happy path: final success writes the deployed note recovery when no prior file exists.
    #[test]
    fn successful_deploy_writes_recovery_when_path_is_absent() {
        let (dir, _cleanup) = temp_dir("dexdo-note-recovery-success-test");
        let path = dir.join("pn_pool.json.recovery.json");
        let successful =
            recovery_state_for_owner(&"42".repeat(32), Some(&format!("0:{}", "2".repeat(64))));

        refresh_note_deploy_recovery_after_success(&path, &successful).unwrap();
        let loaded = load_note_deploy_recovery(&path).unwrap().unwrap();

        assert_eq!(loaded.owner_secret_key_hex, successful.owner_secret_key_hex);
        assert_eq!(loaded.pn_address, successful.pn_address);
    }

    /// Public #33 money-safety: a recovery path that already holds another deployed note is never clobbered.
    #[test]
    fn successful_deploy_refuses_different_live_note_without_clobber() {
        let (dir, _cleanup) = temp_dir("dexdo-note-recovery-live-key-test");
        let path = dir.join("pn_pool.json.recovery.json");
        let existing =
            recovery_state_for_owner(&"31".repeat(32), Some(&format!("0:{}", "1".repeat(64))));
        let successful =
            recovery_state_for_owner(&"42".repeat(32), Some(&format!("0:{}", "2".repeat(64))));
        write_note_deploy_recovery(&path, &existing).unwrap();
        let before = std::fs::read(&path).unwrap();

        let err = refresh_note_deploy_recovery_after_success(&path, &successful)
            .unwrap_err()
            .to_string();

        assert!(err.contains("different deployed PrivateNote"), "{err}");
        assert!(err.contains("refusing to clobber"), "{err}");
        assert!(err.contains("--recovery <different-file>"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let loaded = load_note_deploy_recovery(&path).unwrap().unwrap();
        assert_eq!(loaded.owner_secret_key_hex, existing.owner_secret_key_hex);
        assert_eq!(loaded.pn_address, existing.pn_address);
    }

    /// Public #33 money-safety: an address-less state can still carry an uncertain wallet spend and must not be
    /// treated as an unspent stale attempt.
    #[test]
    fn successful_deploy_refuses_different_owner_with_uncertain_spend() {
        let (dir, _cleanup) = temp_dir("dexdo-note-recovery-pending-spend-test");
        let path = dir.join("pn_pool.json.recovery.json");
        let mut existing = recovery_state_for_owner(&"31".repeat(32), None);
        let mut voucher = NoteDeployVoucherCheckpoint::new(
            &existing.owner_public_key_hex,
            existing.token_type,
            existing.raw_value,
            false,
            "51".repeat(32),
            "61".repeat(32),
        )
        .unwrap();
        voucher.submit_maybe_sent = true;
        existing
            .set_voucher_checkpoint(NoteDeployVoucherKind::Deposit, voucher)
            .unwrap();
        let successful =
            recovery_state_for_owner(&"42".repeat(32), Some(&format!("0:{}", "2".repeat(64))));
        write_note_deploy_recovery(&path, &existing).unwrap();
        let before = std::fs::read(&path).unwrap();

        let err = refresh_note_deploy_recovery_after_success(&path, &successful)
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("possible wallet-spend recovery material"),
            "{err}"
        );
        assert!(err.contains("--recovery <different-file>"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// Public #33 load-time safety: recovery must refuse a target note whose on-chain owner is not the saved key.
    #[test]
    fn loaded_recovery_refuses_target_note_owner_mismatch() {
        let (dir, _cleanup) = temp_dir("dexdo-note-recovery-owner-check-test");
        let path = dir.join("pn_pool.json.recovery.json");
        let state =
            recovery_state_for_owner(&"42".repeat(32), Some(&format!("0:{}", "2".repeat(64))));
        write_note_deploy_recovery(&path, &state).unwrap();
        let before = std::fs::read(&path).unwrap();
        let loaded = load_note_deploy_recovery(&path).unwrap().unwrap();

        let err = ensure_recovery_owner_matches_target_note(
            &path,
            &loaded,
            Some(&format!("0x{}", "99".repeat(32))),
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("does not own target PrivateNote"), "{err}");
        assert!(err.contains("pass the recovery file"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// #377 regression: recovery read, write, and cleanup use one canonical target, leaving no secret-bearing
    /// target behind when the CLI was given a symlink alias.
    #[cfg(unix)]
    #[test]
    fn recovery_symlink_resolves_once_for_read_write_and_cleanup() {
        let (dir, _cleanup) = temp_dir("dexdo-note-recovery-symlink-test");
        let target = dir.join("recovery-target.json");
        let alias = dir.join("recovery-alias.json");
        let mut state = complete_recovery_state();
        write_note_deploy_recovery(&target, &state).unwrap();
        std::os::unix::fs::symlink(&target, &alias).unwrap();

        let resolved = resolve_private_file_path(&alias, "--recovery").unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&target).unwrap());
        let loaded = load_note_deploy_recovery(&resolved).unwrap().unwrap();
        assert_eq!(loaded.owner_secret_key_hex, state.owner_secret_key_hex);

        state.sanity_checked = false;
        write_note_deploy_recovery(&resolved, &state).unwrap();
        assert!(std::fs::symlink_metadata(&alias)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(
            !load_note_deploy_recovery(&target)
                .unwrap()
                .unwrap()
                .sanity_checked
        );

        std::fs::remove_file(&resolved).unwrap();
        assert!(!target.exists());
        assert!(std::fs::symlink_metadata(&alias)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    /// #377 negative regression: a recovery target that resolves to a directory is rejected before use.
    #[test]
    fn recovery_non_regular_target_is_rejected() {
        let (dir, _cleanup) = temp_dir("dexdo-note-recovery-nonregular-test");
        let sentinel = dir.join("recovery-directory");
        std::fs::create_dir(&sentinel).unwrap();

        let err = resolve_private_file_path(&sentinel, "--recovery")
            .unwrap_err()
            .to_string();
        assert!(err.contains("regular file"), "{err}");
    }

    /// #344: recovery state contains note recovery material, but never the funding wallet secret.
    #[test]
    fn recovery_contents_exclude_funding_wallet_secret() {
        let state = complete_recovery_state();
        let wallet_secret = "f1".repeat(32);
        let json = serde_json::to_string_pretty(&state).unwrap();

        assert!(json.contains("owner_secret_key_hex"), "{json}");
        assert!(json.contains(&state.owner_secret_key_hex), "{json}");
        assert!(
            !json.contains(&wallet_secret),
            "wallet secret leaked into recovery JSON"
        );
        assert!(
            !json.contains("multisig_key") && !json.contains("multisig_seed"),
            "recovery JSON must not serialize funding wallet credential fields: {json}"
        );
    }

    /// #344: a complete recovery state can rebuild the exact pool entry without wallet credentials/spend.
    #[test]
    fn recovery_state_finalizes_pool_entry_without_wallet_secret() {
        let state = complete_recovery_state();
        state.ensure_ready_for_pool().unwrap();
        let onboard = state.to_onboard_state().unwrap();
        let note = pn_state_to_pool_note(&onboard).unwrap();
        let pool =
            pool_with_note_added(None, &onboard, note, 1234, &state.funding_multisig_address)
                .unwrap();

        assert_eq!(pool["notes"].as_array().unwrap().len(), 1);
        assert_eq!(pool["notes"][0]["address"], state.pn_address.unwrap());
        assert_eq!(
            pool["notes"][0]["owner_secret_key_hex"],
            state.owner_secret_key_hex
        );
    }

    /// #344 negative: owner-key-only recovery is useful for resume, but not enough to write a pool entry.
    #[test]
    fn incomplete_recovery_refuses_finalize_with_clear_message() {
        let mut state = complete_recovery_state();
        state.pn_address = None;
        state.deposit_identifier_hash = None;
        state.shell_funded = false;
        state.sanity_checked = false;

        let err = state.ensure_ready_for_pool().unwrap_err().to_string();

        assert!(err.contains("owner key"), "{err}");
        assert!(err.contains("no deployed PrivateNote address"), "{err}");
        assert!(err.contains("note deploy --recovery"), "{err}");
        assert!(
            !err.contains(&state.owner_secret_key_hex),
            "secret leaked in error: {err}"
        );
    }

    /// #344 regression: voucher-level recovery may contain a wallet-submitted deposit voucher, but without a
    /// deployed PrivateNote it must resume through `note deploy`, not be folded into a pool.
    #[test]
    fn voucher_submitted_recovery_refuses_pool_finalize_without_note_deploy() {
        let mut state = complete_recovery_state();
        state.pn_address = None;
        state.deposit_identifier_hash = None;
        state.shell_funded = false;
        state.sanity_checked = false;
        let mut voucher = NoteDeployVoucherCheckpoint::new(
            &state.owner_public_key_hex,
            state.token_type,
            state.raw_value,
            false,
            "11".repeat(32),
            "22".repeat(32),
        )
        .unwrap();
        voucher.submit_maybe_sent = true;
        state
            .set_voucher_checkpoint(NoteDeployVoucherKind::Deposit, voucher)
            .unwrap();

        let err = state.ensure_ready_for_pool().unwrap_err().to_string();
        let json = serde_json::to_string_pretty(&state).unwrap();

        assert!(err.contains("no deployed PrivateNote address"), "{err}");
        assert!(json.contains("\"deposit_voucher\""), "{json}");
        assert!(json.contains("\"submit_maybe_sent\": true"), "{json}");
        assert!(
            !err.contains(&state.deposit_voucher.unwrap().sk_u_hex),
            "voucher secret leaked in error: {err}"
        );
    }

    /// #344 regression: voucher checkpoints serialize the recovery material required to avoid a second wallet
    /// spend, but never serialize the funding-wallet credential names or values.
    #[test]
    fn recovery_contents_include_voucher_checkpoint_without_wallet_secret() {
        let mut state = complete_recovery_state();
        let wallet_secret = "f1".repeat(32);
        let mut voucher = NoteDeployVoucherCheckpoint::new(
            &state.owner_public_key_hex,
            state.token_type,
            state.raw_value,
            false,
            "33".repeat(32),
            "44".repeat(32),
        )
        .unwrap();
        voucher.submit_maybe_sent = true;
        voucher.event = Some(NoteDeployVoucherEvent {
            id: "event-id".to_string(),
            boc: "boc".to_string(),
            body: "body".to_string(),
            dst: ":0000000000000000000000000000000000000000000000000000000000000087".to_string(),
            created_at: 1234,
            block_id: Some("block".to_string()),
        });
        state
            .set_voucher_checkpoint(NoteDeployVoucherKind::Deposit, voucher)
            .unwrap();

        let json = serde_json::to_string_pretty(&state).unwrap();

        assert!(json.contains("\"sk_u_hex\""), "{json}");
        assert!(json.contains("\"sk_u_commit_hex\""), "{json}");
        assert!(json.contains("\"event\""), "{json}");
        assert!(
            !json.contains(&wallet_secret),
            "wallet secret leaked: {json}"
        );
        assert!(
            !json.contains("multisig_key") && !json.contains("multisig_seed"),
            "wallet credential field leaked: {json}"
        );
    }

    /// #344 negative: an existing recovery file is tied to the deploy request and cannot be silently reused.
    #[test]
    fn recovery_rejects_mismatched_request() {
        let state = complete_recovery_state();

        let err = state
            .ensure_matches_request(recovery_request(
                "https://other-shellnet.example",
                &state.funding_multisig_address,
            ))
            .unwrap_err()
            .to_string();

        assert!(err.contains("does not match this deploy request"), "{err}");
        assert!(err.contains("fresh --pool/--recovery"), "{err}");
        assert!(
            !err.contains(&state.owner_secret_key_hex),
            "secret leaked in error: {err}"
        );
    }

    /// #344: user-facing recovery guidance names only paths and actions, not raw key material.
    #[test]
    fn recovery_user_message_does_not_log_secret() {
        let path = std::path::Path::new("pn_pool.json.recovery.json");
        let state = complete_recovery_state();
        let msg = recovery_owner_key_written_message(path);

        assert!(msg.contains("note recover"), "{msg}");
        assert!(msg.contains("pn_pool.json.recovery.json"), "{msg}");
        assert!(
            !msg.contains(&state.owner_secret_key_hex),
            "secret leaked in message: {msg}"
        );
        assert!(
            !msg.contains(&state.owner_public_key_hex),
            "owner key material should not be printed in recovery guidance: {msg}"
        );
    }
}
