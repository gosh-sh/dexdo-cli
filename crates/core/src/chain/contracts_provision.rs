use anyhow::{anyhow, Result};
use base64::Engine as _;
use gosh_ackinacki::sdk::{Account, Address, KeyPair};
use tvm_block::{Deserializable, Serializable, StateInit};

/// The HTTP `User-Agent` every chain request carries. **Load-bearing, not decoration -- do not
/// delete it and do not let the header fall back to the library default.**


/// them). It does *not* test the header for browser-ness, so an explicit User-Agent of any kind is
/// sufficient and we send an honest one instead of impersonating a browser. Established against
/// that edge on 2026-08-02 (@SeHor05, PR990): `curl` -> 200, `urllib` with its default UA -> 403,
/// `urllib` with an explicit UA -> 200. The Acki Nacki side sends its own `acki-...` identifier for
/// the same reason, recorded as mandatory in their `tests/helper/common.py`.

/// The version is taken from the crate so this identifier cannot go stale.
pub(super) const DEXDO_USER_AGENT: &str = concat!("dexdo/", env!("CARGO_PKG_VERSION"));

pub(super) fn gas_health_top_up_amount(balance: u128, min: u128, target: u128) -> Option<u128> {
    debug_assert!(target >= min);
    if balance <= min {
        let amount = target.saturating_sub(balance);
        (amount > 0).then_some(amount)
    } else {
        None
    }
}

/// ABI of the deployed contracts (`contracts/compiled`), embedded for on-chain getters.
pub(super) const SUPERROOT_ABI: &str =
    include_str!("../../../../contracts/compiled/airegistry/SuperRoot.abi.json");
pub(super) const ROOTPN_ABI: &str =
    include_str!("../../../../contracts/compiled/dex/RootPN.abi.json");
pub(super) const ROOTORACLE_ABI: &str =
    include_str!("../../../../contracts/compiled/dex/RootOracle.abi.json");
pub(super) const ORACLE_ABI: &str =
    include_str!("../../../../contracts/compiled/dex/Oracle.abi.json");
pub(super) const ORACLEEVENTLIST_ABI: &str =
    include_str!("../../../../contracts/compiled/dex/OracleEventList.abi.json");
pub(super) const PMP_ABI: &str = include_str!("../../../../contracts/compiled/dex/PMP.abi.json");
pub(super) const ORDERBOOK_ABI: &str =
    include_str!("../../../../contracts/compiled/dex/OrderBook.abi.json");
pub(super) const PMP_TVC: &[u8] = include_bytes!("../../../../contracts/compiled/dex/PMP.tvc");
pub(super) const ORDERBOOK_TVC: &[u8] =
    include_bytes!("../../../../contracts/compiled/dex/OrderBook.tvc");
pub(super) const ROOTMODEL_ABI: &str =
    include_str!("../../../../contracts/compiled/airegistry/RootModel.abi.json");
pub(super) const TOKENCONTRACT_ABI: &str =
    include_str!("../../../../contracts/compiled/airegistry/TokenContract.abi.json");
/// `PrivateNote` (zk-note) -- ABI of the deal's owner methods (`deployInferenceOrderBook`,
/// `postSellOffer`, `placeInferenceBuy`, `streamStop`, getter `getInferenceOrderBookAddress`).
/// Minted via RootPN (gosh-dexdo `mint_pn_pool`); the signatures of these 5 methods match the
/// deployed code byte-for-byte -- the note accepts our calls (see the live test).
pub(super) const PRIVATENOTE_ABI: &str =
    include_str!("../../../../contracts/compiled/dex/PrivateNote.abi.json");
/// `PrivateNote` StateInit (`.tvc`). The CLI never deploys `PrivateNote` -- RootPN does, from the code
/// installed by `setPrivateNoteCode` -- so **no path may treat this vendored image as evidence about
/// the live chain**; that job belongs to the matching `private_note` value in [`GENERATION_PINS`],
/// which is read from RootPN.
/// It is listed in [`COMPILED_CONTRACT_IMAGES`] for the one statement a vendored image *can* make,
/// which is about this tree and not about the chain: the deployment manifest's `PrivateNote` pin
/// names the artifact committed beside it.
pub(super) const PRIVATENOTE_TVC: &[u8] =
    include_bytes!("../../../../contracts/compiled/dex/PrivateNote.tvc");

// The 4.0.33 generation `doctor` holds the chain to. It compares the chain against THESE, never
// against a vendored `.tvc`: an embedded image travels in the same vendoring commit as the constant
// beside it, so comparing the two proves only that they were committed together and stays green when
// the chain moves away from both.

// One `Fail` makes `ChainDoctorReport::is_ok` false and `chain_doctor_preflight` turns that
// into a `bail!` ahead of provision, seller, buyer, `note deploy` and `note withdraw` -- so a pin
// left behind here refuses a valid current note before any note guard is consulted. They are
// maintained by hand, on the same cadence as the committed manifests, and a redeploy that
// moves any of them must move it here in the same change.

/// The generation one NETWORK runs, as read off that network's accounts.

/// **A table rather than four module constants, because 4.0.36 added a fifth pin that only exists
/// from this generation** -- `token_contract_code`, the deal code RootPN bakes into every note. A flat
/// set of constants cannot say "this field does not exist on that generation", and answering it with
/// a hash anyway would be reading a storage field that is not there.

/// Every value here is READ FROM AN ACCOUNT on the network in its row. Not from a vendored `.tvc`:
/// an embedded image travels in the same commit as the constant beside it, so comparing the two
/// proves they were committed together and stays green while the chain moves away from both. That is
/// the exact false green that let 4.0.33 go live unnoticed.

/// One `Fail` makes `ChainDoctorReport::is_ok` false and `chain_doctor_preflight` turns that
/// into a `bail!` ahead of provision, seller, buyer, `note deploy` and `note withdraw` -- so a row
/// left behind refuses a valid current note before anything else runs. A redeploy moves its row in
/// the same change as `manifest/deployed.<network>.json`.
pub(super) struct GenerationPins {
    /// The contracts generation these pins are, as a manifest's `version` field spells it. It is
    /// the KEY of the row.

    /// It used to be keyed by network label instead, with the generation carried alongside. That was
    /// wrong twice over. A code hash is a property of the CODE -- the row's own comments say so --
    /// so two chains running one generation have identical pins, and the network-keyed table stored
    /// that fact twice and let the copies drift. And it made the client hold a list of chains it had
    /// "read", so a chain deployed after this binary was built had no row and failed the generation
    /// check for no reason except its own newness.

    /// Keyed by generation, a row answers the question actually being asked: this manifest declares
    /// generation X, here is what code at generation X hashes to, and the chain either serves that
    /// or it does not. Whichever chain it is.

    /// The two chains need not be on one generation, and were not: in August 2026
    /// one chain ran 4.0.36 while the other was still on 4.0.35. A test holding one literal
    /// went red the day that rollout staged -- which is what a fact written down in three places
    /// and checked in none does. Carried on the row, the generation is compared against the
    /// manifest that declares it and moves with the pins beside it.
    pub(super) version: &'static str,
    /// `SuperRoot` at `0:0c0c...`.
    pub(super) superroot: &'static str,
    /// `RootPN` at `0:1010...`. Compiled with `sold_old` (v1 ext-out), which preserves the
    /// `VoucherGenerated` format the voucher prover consumes. Several images answer the same
    /// `getVersion()`, so the hash and not the version getter identifies a generation.
    pub(super) rootpn: &'static str,
    /// `RootOracle` at `0:1515...`.
    pub(super) rootoracle: &'static str,
    /// The per-model `InferenceOrderBook` RootPN deploys from the code installed by
    /// `setInferenceOrderBookCode`.

    /// `None` means NO BOOK OF THIS GENERATION EXISTS ON THAT CHAIN YET, which is the honest state of
    /// a freshly deployed network: a book is per model, so none exists until someone provisions one.
    /// The check skips rather than comparing against a number nobody read.
    pub(super) inference_orderbook: Option<&'static str>,
    /// The `PrivateNote` code `RootPN.getDetails().privateNoteCodeHash` reports -- the code the root
    /// actually mints, which is what every note guard is held to.
    pub(super) private_note: &'static str,
    /// The `TokenContract` code RootPN bakes into every note it mints, read out of its
    /// `_tokenContractCode` storage field (contracts 4.0.36).

    /// `None` means THIS GENERATION'S RootPN HAS NO SUCH FIELD, which is the honest state of 4.0.35
    /// and earlier: the deal was deployed off-chain there, so a note never carried the deal's code
    /// and nothing was set. Decoding it anyway would be reading a field that does not exist.
    pub(super) token_contract_code: Option<&'static str>,
}

pub(super) const GENERATION_PINS: &[GenerationPins] = &[
    GenerationPins {
        // 4.0.36, and the development chain serves it -- measured 2026-08-28: `doctor` against
        // that chain's endpoint reports 21 PASS / 0 FAIL and `SuperRoot: 4.0.36` /
        // `RootPN: 4.0.36` under `versions:`. This comment used to say "once 4.0.36 is deployed
        // there; until it is, `doctor` reports the live 4.0.35 hashes as a mismatch", which was
        // written before the rollout and stayed after it. A code hash is still a property
        // of the code and not of the chain -- that is why one row can serve two chains -- but the
        // sentence about a deploy that has already happened had become false. The generation it
        // replaces read:
        // superroot 7591c2b58646b793d01965e123603c879f125d875f47da8d612224ea0589b1ea
        // rootpn 8ee7225d4e928296e92c76b0d00efc181a4d7f47ba2ce8825d5fb935658f9703
        // rootoracle 7876890031636ab669fd488e12009e43a3cc8cadb3dce975e11b18bfb8e7e84d
        // inference_orderbook Some("2fa52109d6f38fc3640f35febcb73300a9f96a7a3558bb4ae6b4e00374420016")
        // private_note 57e85fa67cc90284b907ea7e9d8c6d35830c02d14bd04d4be6ec884b5748ca0c
        // token_contract_code None (4.0.35's RootPN predates the field)
        version: "4.0.36",
        superroot: "295b0f05b571273d7b01e3ea9566bfee4340ed2a7cdd59b21242c555925e10d0",
        rootpn: "2d577219df058ec0f6ea09dad204b13398342ef7cd5c66e843049ae2380aa928",
        rootoracle: "227d5b86dd309a757e0ff5977ebffc3065d269d80acb830336a7a3d21213d489",
        // The code hash of the book RootPN deploys, which is the vendored image's -- a book is
        // created from the code installed by `setInferenceOrderBookCode`, so its hash is decided by
        // the tree and not by whether an instance happens to exist yet. `None` here used to mean
        // "nothing provisioned", but a `None` turns the check into a `Skip` and `is_ok()` counts a
        // `Skip` as passing, so the honest-looking answer was a hole: the right value was in the
        // tree and nothing read it. Held to the artifact by
        // `the_book_and_deal_pins_are_the_images_this_tree_ships`.
        inference_orderbook: Some(
            "e97227c5d1a8fff171e0c5a1f6aa3e063f663bfcb5c86757392aef82a8775954",
        ),
        private_note: "acf19e140b58469a50165bcbda88cca952b2036678f1f5823b6a6bebd3fc32b1",
        token_contract_code: Some(
            "ee4105b4800d852dde1a86cec4e270ecfa2ae0e199f05a46823aed792933e711",
        ),
    },
    // There is no second row, and its absence is the point of keying this table by VERSION.

    // It used to hold the other chain's own generation, 4.0.35, while this one ran 4.0.36. On
    // 2026-08-28 mainnet served 4.0.36 and its five chain-read hashes came back
    // byte-identical to the row above -- superroot, rootpn, rootoracle, private_note and
    // token_contract_code, each compared literal for literal before this row was removed.

    // A table keyed by network has to write those seven values down twice and keep the copies in
    // step; the defect that costs is a repin that touches one and not the other, which looks like
    // progress and is not. Keyed by version there is nothing to keep in step: one generation, one
    // row, and two chains running it match it because it is the same code.

    // 4.0.35 is gone rather than kept for old manifests. A manifest declaring it now finds no row
    // and is told so by name, which is the honest answer -- this build has not measured it.
];

/// The row for a contracts generation, or `None` when this build has not measured that generation.
pub(super) fn generation_pins(version: &str) -> Option<&'static GenerationPins> {
    GENERATION_PINS.iter().find(|row| row.version == version)
}

// Test code names the active table row without repeating its hash literal.
#[cfg(test)]
pub(super) const PRIVATENOTE_PINNED_CODE_HASH: &str = GENERATION_PINS[0].private_note;

/// `TokenContract` StateInit (`.tvc`) -- deployed via `build_deploy` (step 2: the seller provisions
/// the per-deal TC). Its code-hash == the `RootModel.TOKEN_CONTRACT_CODE_HASH` pin (offline guard), so
/// the derived address matches `RootModel.getTokenContractAddress` and registration is accepted.
pub(super) const TOKENCONTRACT_TVC: &[u8] =
    include_bytes!("../../../../contracts/compiled/airegistry/TokenContract.tvc");
/// `RootModel` StateInit (`.tvc`).

/// **NOBODY DEPLOYS THIS FROM HERE ANY MORE.** The comment used to read "the seller (model owner)
/// deploys their own RootModel under SuperRoot themselves (self-register: the ctor calls
/// `SuperRoot.registerRoot`)", and both halves of that are gone: `SuperRoot.deployRootModel` performs
/// the deploy from the code SuperRoot itself pins (`contracts/airegistry/SuperRoot.sol:189`), and
/// `registerRoot` -- the announcement a self-deployed root made -- was removed along with the interface
/// that declared it. An external deploy of this image is refused, `ERR_INVALID_SENDER = 302`
/// (`contracts/airegistry/RootModel.sol:67`).

/// The image is kept as the offline counterpart of [`SUPERROOT_PINNED_RM_CODE_HASH`]: hashing it is
/// what proves the RootModel artifact in this tree is the one SuperRoot's pin names. It is no longer
/// deployment input, and it is read only by that comparison and by [`COMPILED_CONTRACT_IMAGES`].
pub(super) const ROOTMODEL_TVC: &[u8] =
    include_bytes!("../../../../contracts/compiled/airegistry/RootModel.tvc");
/// The `TOKEN_CONTRACT_CODE_HASH` pin from `contracts/airegistry/RootModel.sol` -- against it RootModel
/// checks the TC code when registering a deal. The embedded `TokenContract.tvc` must yield this hash.
pub(super) const ROOTMODEL_PINNED_TC_CODE_HASH: &str =
    "ee4105b4800d852dde1a86cec4e270ecfa2ae0e199f05a46823aed792933e711";
/// The `ROOT_MODEL_CODE_HASH` pin from `contracts/airegistry/SuperRoot.sol` -- the hash SuperRoot's own
/// `_rootModelCode` must carry, and therefore the hash of the code `deployRootModel` puts on chain. The
/// embedded `RootModel.tvc` must yield it, otherwise this tree's idea of a RootModel is not the one the
/// live SuperRoot deploys and every address derived from it is wrong.

/// It used to say SuperRoot "checks the RootModel code at `registerRoot`". There is no such check and
/// no such entry: `registerRoot` verified a self-deployed root's *address*, and it was removed when
/// SuperRoot took over the deploy.
pub(super) const SUPERROOT_PINNED_RM_CODE_HASH: &str =
    "e92a14cb9c5ac757e16be2f453d5c3a25e7bec90044a1389b97414d1b785cac8";
pub(super) fn normalize_code_hash(raw: &str) -> Option<String> {
    let h = raw.trim().strip_prefix("0x").unwrap_or(raw.trim());
    if h.is_empty() || h.len() > 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("{h:0>64}").to_lowercase())
}

/// (pure, offline-testable): an account must carry the current pinned `PrivateNote`
/// code. The async seller guard and read-only note balance command share this identity check.
fn private_note_code_hash_is_current(expected: &str, code_hash: Option<&str>) -> bool {
    code_hash
        .and_then(normalize_code_hash)
        .is_some_and(|hash| hash == expected)
}

/// `expected` is the PrivateNote code the ROOT ON THIS NETWORK mints, from
/// [`generation_pins`] -- not a module constant, because the two chains this client can dial are on
/// different generations and a global would refuse every valid note on one of them.
pub(super) fn note_code_hash_current(
    expected: &str,
    note: &Address,
    code_hash: Option<&str>,
) -> Result<()> {
    let note = crate::address::display(&note.to_string());
    if private_note_code_hash_is_current(expected, code_hash) {
        Ok(())
    } else {
        Err(anyhow!(
            "seller note {note} code_hash {} != the current PrivateNote code {expected} \
             -- the pn_pool predates a contract redeploy (orphaned). Re-mint against the current contracts \
             (`mint_pn_pool`) and point DEXDO_PN_POOL at the fresh pool.",
            code_hash.unwrap_or("<none>")
        ))
    }
}

pub(super) fn seller_note_account_current(
    expected: &str,
    note: &Address,
    account: Option<&Account>,
) -> Result<()> {
    let note_display = crate::address::display(&note.to_string());
    let account = account.ok_or_else(|| {
        anyhow!(
            "seller note {note_display} is not on-chain -- the pn_pool is likely orphaned by a contract redeploy \
             (SuperRoot/PrivateNote rotation). Re-mint against the current contracts (`mint_pn_pool`) and \
             point DEXDO_PN_POOL at the fresh pool."
        )
    })?;
    if !account.is_active() {
        return Err(anyhow!(
            "seller note {note_display} is {}, not Active -- re-mint the pn_pool against the current contracts \
             (`mint_pn_pool`); a pool minted before a SuperRoot redeploy is orphaned.",
            account.status
        ));
    }
    note_code_hash_current(expected, note, account.code_hash.as_deref())
}

pub(super) fn note_balance_private_note_account(
    expected: &str,
    note: &Address,
    account: Option<&Account>,
) -> Result<()> {
    let note_display = crate::address::display(&note.to_string());
    let account = account.ok_or_else(|| {
        anyhow!(
            "PrivateNote account {note_display} is not Active/not found (account snapshot absent)"
        )
    })?;
    if !account.is_active() {
        return Err(anyhow!(
            "PrivateNote account {note_display} is not Active/not found (status: {})",
            account.status
        ));
    }
    if !private_note_code_hash_is_current(expected, account.code_hash.as_deref()) {
        return Err(anyhow!(
            "note {note_display} is not current PrivateNote: actual code_hash {}, expected code_hash \
             {expected}",
            account.code_hash.as_deref().unwrap_or("<none>")
        ));
    }
    Ok(())
}

/// Fund-safety guard for `note withdraw`: pure code-hash generation check.
/// A note whose on-chain `code_hash` is not the current generation's `private_note` pin was
/// deployed by a previous contract generation; the current-generation `withdrawTokens` zeroes it
/// without crediting the destination, so the SHELL is lost. Refuse before any on-chain write.
pub(super) fn note_withdraw_generation_ok(
    expected: &str,
    note: &Address,
    code_hash: Option<&str>,
) -> Result<()> {
    let note = crate::address::display(&note.to_string());
    match code_hash {
        Some(h) if h == expected => Ok(()),
        other => Err(anyhow!(
            "REFUSING to withdraw from note {note}: it was deployed by a PREVIOUS contract generation \
             (code_hash {}, current is {expected}). Withdrawing from a \
             previous-generation note with this CLI zeroes the note WITHOUT crediting the destination \
             -- the SHELL is lost (dexdo-cli). This CLI will not submit the withdraw.",
            other.unwrap_or("<none>")
        )),
    }
}

#[cfg(test)]
mod withdraw_generation_tests {
    use super::*;

    fn any_note() -> Address {
        Address::parse(&format!("0:{}", "1".repeat(64))).unwrap()
    }

    #[test]
    fn withdraw_allows_current_generation_note() {
        assert!(note_withdraw_generation_ok(
            PRIVATENOTE_PINNED_CODE_HASH,
            &any_note(),
            Some(PRIVATENOTE_PINNED_CODE_HASH)
        )
        .is_ok());
    }

    #[test]
    fn withdraw_refuses_previous_generation_note() {
        // The two previous-generation hashes from dexdo-cli that zeroed notes without crediting.
        for stale in [
            "210add370000000000000000000000000000000000000000000000000000000a",
            "76acd39200000000000000000000000000000000000000000000000000000007",
        ] {
            let err =
                note_withdraw_generation_ok(PRIVATENOTE_PINNED_CODE_HASH, &any_note(), Some(stale))
                    .unwrap_err()
                    .to_string();
            assert!(err.contains("REFUSING to withdraw"), "message: {err}");
            assert!(err.contains(stale), "must name the stale hash: {err}");
            assert!(
                err.contains(PRIVATENOTE_PINNED_CODE_HASH),
                "must name the current hash: {err}"
            );
        }
    }

    #[test]
    fn withdraw_refuses_note_with_no_code_hash() {
        assert!(
            note_withdraw_generation_ok(PRIVATENOTE_PINNED_CODE_HASH, &any_note(), None).is_err()
        );
    }
}

#[cfg(test)]
mod note_balance_identity_tests {
    use super::*;

    fn note() -> Address {
        Address::parse(&format!("0:{}", "2".repeat(64))).unwrap()
    }

    /// The same note in the canonical `<dapp_id>::<account_id>` form the diagnostics render:
    /// a `PrivateNote` is a system contract of the shared dexdo DApp.
    fn canonical_note() -> String {
        format!("{}::{}", crate::address::DEXDO_DAPP_ID, "2".repeat(64))
    }

    fn account(
        status: &str,
        code_hash: Option<&str>,
        balance: u128,
        ecc: Vec<(u32, u128)>,
    ) -> Account {
        Account {
            address: note(),
            status: status.to_string(),
            balance,
            ecc,
            code_hash: code_hash.map(str::to_string),
            boc: None,
        }
    }

    #[test]
    fn missing_and_inactive_accounts_are_not_private_notes() {
        let note = note();
        let missing = note_balance_private_note_account(PRIVATENOTE_PINNED_CODE_HASH, &note, None)
            .unwrap_err()
            .to_string();
        assert!(missing.contains(&canonical_note()), "{missing}");
        assert!(missing.contains("not Active/not found"), "{missing}");

        for status in ["NonExist", "Uninit"] {
            let account = account(status, None, 0, Vec::new());
            let error = note_balance_private_note_account(
                PRIVATENOTE_PINNED_CODE_HASH,
                &note,
                Some(&account),
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains(&canonical_note()), "{error}");
            assert!(error.contains("not Active/not found"), "{error}");
            assert!(error.contains(status), "{error}");
        }
    }

    #[test]
    fn active_wrong_code_hash_names_actual_and_expected_identity() {
        let note = note();
        let actual = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let account = account("Active", Some(actual), 0, Vec::new());
        let error =
            note_balance_private_note_account(PRIVATENOTE_PINNED_CODE_HASH, &note, Some(&account))
                .unwrap_err()
                .to_string();
        assert!(error.contains(&canonical_note()), "{error}");
        assert!(error.contains("not current PrivateNote"), "{error}");
        assert!(error.contains(actual), "{error}");
        assert!(error.contains(PRIVATENOTE_PINNED_CODE_HASH), "{error}");
    }

    #[test]
    fn current_private_note_accepts_zero_and_funded_balances() {
        let note = note();
        for account in [
            account("Active", Some(PRIVATENOTE_PINNED_CODE_HASH), 0, Vec::new()),
            account(
                "Active",
                Some(&format!(
                    "0x{}",
                    PRIVATENOTE_PINNED_CODE_HASH.to_uppercase()
                )),
                5_000_000_123,
                vec![(2, 1_234_567_890)],
            ),
        ] {
            note_balance_private_note_account(PRIVATENOTE_PINNED_CODE_HASH, &note, Some(&account))
                .unwrap();
        }
    }

    #[test]
    fn seller_missing_and_inactive_keep_orphan_remint_diagnostics() {
        let address = note();
        // the diagnostic names the note canonically; every other byte of it is unchanged.
        let note = canonical_note();
        let missing = seller_note_account_current(PRIVATENOTE_PINNED_CODE_HASH, &address, None)
            .unwrap_err()
            .to_string();
        assert_eq!(
            missing,
            format!(
                "seller note {note} is not on-chain -- the pn_pool is likely orphaned by a contract redeploy \
                 (SuperRoot/PrivateNote rotation). Re-mint against the current contracts (`mint_pn_pool`) and \
                 point DEXDO_PN_POOL at the fresh pool."
            )
        );

        let account = account("Uninit", None, 0, Vec::new());
        let inactive =
            seller_note_account_current(PRIVATENOTE_PINNED_CODE_HASH, &address, Some(&account))
                .unwrap_err()
                .to_string();
        assert_eq!(
            inactive,
            format!(
                "seller note {note} is Uninit, not Active -- re-mint the pn_pool against the current contracts \
                 (`mint_pn_pool`); a pool minted before a SuperRoot redeploy is orphaned."
            )
        );
    }
}

/// `InferenceOrderBook` -- ABI of the on-chain offer/order book.
pub(super) const INFERENCE_ORDERBOOK_ABI: &str =
    include_str!("../../../../contracts/compiled/airegistry/InferenceOrderBook.abi.json");
/// `InferenceOrderBook` StateInit (`.tvc`) -- the **code-cell** is extracted from it, which the note
/// passes to `deployInferenceOrderBook(code,...)` (the book address is deterministic from code+params).
pub(super) const INFERENCE_ORDERBOOK_TVC: &[u8] =
    include_bytes!("../../../../contracts/compiled/airegistry/InferenceOrderBook.tvc");
pub(super) const ROOTPN_ADDR: &str =
    "0:1010101010101010101010101010101010101010101010101010101010101010";
pub(super) const ROOTORACLE_ADDR: &str =
    "0:1515151515151515151515151515151515151515151515151515151515151515";
/// Decode a hex string (TVM ABI `bytes` output) without an external dependency.
pub(super) fn decode_hex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(anyhow!("odd hex length"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(Into::into))
        .collect()
}

/// Derive a note's ed25519 public key (`[u8; 32]`) from its owner [`KeyPair`] -- the same derivation
/// `RealNote::pubkey` uses (`KeyPair::public_hex` -> bytes). Used by `dexdo recover` to verify the
/// recover note is the deal's recorded buyer (`getBuyerPubkey`) before signing STOP.
pub fn keypair_ed_pubkey(keys: &KeyPair) -> Result<[u8; 32]> {
    let bytes = decode_hex(keys.public_hex().trim_start_matches("0x"))?;
    if bytes.len() != 32 {
        return Err(anyhow!(
            "ed25519 public key: expected 32 bytes, got {}",
            bytes.len()
        ));
    }
    let mut ed = [0u8; 32];
    ed.copy_from_slice(&bytes);
    Ok(ed)
}

/// Encode bytes to hex (a TVM ABI `bytes` argument, e.g. `endpointCipher`; and code-hash comparison).
/// `write!` directly into the buffer -- without allocating a `String` per byte.
pub(super) fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Extract the **code-cell** from a `.tvc` (StateInit BOC) -- the same logic as
/// `airegistry::abi::Contract::code_cell` in the SDK: `read_single_root_boc` -> `StateInit` -> `.code`.
pub(super) fn code_cell(tvc: &[u8]) -> Result<tvm_types::Cell> {
    let cell = tvm_types::read_single_root_boc(tvc).map_err(|e| anyhow!("read tvc BOC: {e}"))?;
    let state_init =
        StateInit::construct_from_cell(cell).map_err(|e| anyhow!("parse StateInit: {e}"))?;
    state_init
        .code
        .ok_or_else(|| anyhow!("no code-cell in StateInit"))
}

/// The `.tvc` code-cell as base64-BOC -- the encoding of a `cell` argument in TVM ABI (`call`/`run_getter`).
pub(super) fn code_boc_b64(tvc: &[u8]) -> Result<String> {
    let boc = tvm_types::write_boc(&code_cell(tvc)?).map_err(|e| anyhow!("write code BOC: {e}"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(boc))
}

/// Hex `tvm.hash` of a `.tvc` code-cell.

/// **A vendored image is never evidence about the chain**, and no generation pin takes its expected
/// value from one -- see the block of `PINNED_*_CODE_HASH` constants above and
/// `doctor_compares_every_generation_pin_and_never_a_vendored_image`. What an image *is* evidence
/// about is this tree, which is the only thing [`COMPILED_CONTRACT_IMAGES`] uses it for.
pub(super) fn code_hash(tvc: &[u8]) -> Result<String> {
    Ok(encode_hex(code_cell(tvc)?.repr_hash().as_slice()))
}

/// Every compiled contract image this tree vendors, keyed by the name the deployment manifest pins
/// it under (a committed manifest's `contract_hashes`). The two sides are produced by
/// different hands -- the `.tvc` by the contracts compiler, the pin by whoever wrote the manifest --
/// which is what lets `Deployed::validate` compare them and get an answer that is not a foregone
/// conclusion.

/// is why the table exists. The 4.0.36 manifest declared Oracle, OracleEventList, PMP,
/// OrderBook and Nullifier "carried over from 4.0.35 unchanged"; all five artifacts had in fact
/// moved, and three of the five kept the same file SIZE, so `git show --stat` showed nothing. The
/// manifest-pin check could not see it either: it compared the manifest loaded at runtime against a
/// copy of the manifest embedded at build time, so on the ordinary case -- a binary run out of the
/// tree it was built from -- the two sides were the same bytes and the check could only pass. The
/// disagreement surfaced on a live run, in the teardown after the assertion, on the client gate that
/// does read the chain.

/// Six of the thirteen already had a reader for other reasons and are named above; the other seven
/// are only ever hashed here, so they are embedded at their single use.
pub(super) const COMPILED_CONTRACT_IMAGES: &[(&str, &[u8])] = &[
    ("InferenceOrderBook", INFERENCE_ORDERBOOK_TVC),
    (
        "ModelRegistry",
        include_bytes!("../../../../contracts/compiled/airegistry/ModelRegistry.tvc"),
    ),
    (
        "Nullifier",
        include_bytes!("../../../../contracts/compiled/dex/Nullifier.tvc"),
    ),
    (
        "Oracle",
        include_bytes!("../../../../contracts/compiled/dex/Oracle.tvc"),
    ),
    (
        "OracleEventList",
        include_bytes!("../../../../contracts/compiled/dex/OracleEventList.tvc"),
    ),
    ("OrderBook", ORDERBOOK_TVC),
    ("PMP", PMP_TVC),
    ("PrivateNote", PRIVATENOTE_TVC),
    ("RootModel", ROOTMODEL_TVC),
    (
        "RootOracle",
        include_bytes!("../../../../contracts/compiled/dex/RootOracle.tvc"),
    ),
    (
        "RootPN",
        include_bytes!("../../../../contracts/compiled/dex/RootPN.tvc"),
    ),
    (
        "SuperRoot",
        include_bytes!("../../../../contracts/compiled/airegistry/SuperRoot.tvc"),
    ),
    ("TokenContract", TOKENCONTRACT_TVC),
];

/// The code hash of the compiled `contract` this binary carries.

/// **This is the source every preflight that reads a deployed account uses to say what that account
/// is supposed to be**, and it replaces a copy of the same numbers that used to live in the
/// deployment manifest under `contract_hashes`. The two were produced from one origin -- the
/// compiler wrote the `.tvc`, a person retyped its hash into the file -- so the file could only ever
/// agree or be stale, never inform. It went stale on mainnet in August 2026 and took the client down
/// with it.

/// **What this does NOT say is anything about the chain.** It answers "what does this build know how
/// to talk to", which is exactly the question a caller about to decode an account's storage is
/// asking. "What is deployed on this network" is a different question, answered by the per-network
/// generation pins in [`GENERATION_PINS`]: one artifact set cannot express two chains sitting on
/// different generations, and in August 2026 they did -- the tree recorded shellnet on 4.0.36 from
/// `1d4b985c` while mainnet stayed on 4.0.35 until moved it.
pub fn compiled_contract_hash(contract: &str) -> Result<String> {
    let image = COMPILED_CONTRACT_IMAGES
        .iter()
        .find(|(name, _)| *name == contract)
        .map(|(_, image)| *image)
        .ok_or_else(|| {
            anyhow!("this build carries no compiled {contract} artifact to check against")
        })?;
    code_hash(image).map_err(|error| anyhow!("hash the compiled {contract} artifact: {error}"))
}

/// Every compiled contract name this build carries, with the hash of its image.

/// The reverse lookup: "this account serves code -- is it one of ours, and which?". It used to be
/// answered by walking the manifest's `contract_hashes`, which is the copy removed; the images
/// are the origin those numbers were retyped from.
pub fn compiled_contract_hashes() -> Vec<(&'static str, String)> {
    COMPILED_CONTRACT_IMAGES
        .iter()
        .filter_map(|(name, image)| code_hash(image).ok().map(|hash| (*name, hash)))
        .collect()
}

/// The `PrivateNote` code the root on `network` mints -- what every note guard on that chain is held
/// to.

/// Public because the manifest no longer carries a copy of it: callers outside this module
/// that used to read `contract_hashes.PrivateNote` ask here instead.

/// Keyed by GENERATION, not by network: a code hash is a property of the code, so the
/// question "what should a note on this chain be" is answered by "which generation does that
/// chain's manifest declare". The caller supplies the generation it read from the manifest.
pub fn private_note_pin_of_generation(generation: &str) -> Option<&'static str> {
    generation_pins(generation).map(|row| row.private_note)
}

/// Derive the canonical per-model order-book address from the pinned TVC and model hash.
pub(super) fn inference_orderbook_address_from_model_hash(model_hash: &str) -> Result<Address> {
    let hash = model_hash
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    if hash.len() != 64 || !hash.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        return Err(anyhow!("model hash must be exactly 32 bytes of hex"));
    }
    let root = tvm_types::read_single_root_boc(INFERENCE_ORDERBOOK_TVC)
        .map_err(|error| anyhow!("read InferenceOrderBook TVC: {error}"))?;
    let mut state_init = StateInit::construct_from_cell(root)
        .map_err(|error| anyhow!("parse InferenceOrderBook StateInit: {error}"))?;
    let fields = serde_json::json!({
        "_pubkey": "0x0",
        "_modelHash": format!("0x{hash}"),
    });
    let data = tvm_abi::json_abi::encode_storage_fields(
        INFERENCE_ORDERBOOK_ABI,
        Some(&fields.to_string()),
    )
    .map_err(|error| anyhow!("encode InferenceOrderBook static fields: {error}"))?
    .into_cell()
    .map_err(|error| anyhow!("build InferenceOrderBook data cell: {error}"))?;
    state_init.data = Some(data);
    let state_init = state_init
        .serialize()
        .map_err(|error| anyhow!("serialize InferenceOrderBook StateInit: {error}"))?;
    Address::parse(&format!(
        "0:{}",
        encode_hex(state_init.repr_hash().as_slice())
    ))
}
