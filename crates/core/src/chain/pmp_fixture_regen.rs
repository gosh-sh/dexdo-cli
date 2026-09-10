//! Rebuild `crates/dexdo/tests/fixtures/pmp-account.boc` against the contracts this tree compiles.

//! The fixture is an ACCOUNT: an address, a data cell, and a code cell that is the compiled PMP
//! base with this deployment's salt applied. Only the code cell depends on the contract generation,
//! and only the code cell is rewritten here.

//! # Why this exists rather than a hand-made file

//! The fixture was made once, on 2026-08-14, and the 4.0.36 migration moved the compiled PMP
//! without regenerating it. `pmp_status_reads_all_compiled_getters_without_a_market_manifest_or_write`
//! then failed with

//! ```text
//! PMP...::4444... code was not produced from the current compiled base and its live salt
//! ```

//! which is the client answering correctly about a fixture that had gone stale. A file rewritten by
//! hand to make that green would stop describing anything; a file DERIVED from the tree's own
//! artifact and the fixture's own salt describes exactly what it claims to -- a PMP deployed at that
//! address from this tree's code.

//! # Running it

//! ```text
//! cargo test -p dexdo-core --lib -- --ignored pmp_account_fixture_is_rebuilt_from_this_tree
//! ```

//! Ignored because it WRITES into the working tree: a test that edits tracked files must be asked
//! for, never run as part of a sweep. Re-run it whenever the compiled contracts move, and commit
//! the result with the reason.

//! # The bug the rebuild uncovered, measured 2026-08-27

//! Applying the rebuilt account for the first time did not make that test green -- it made it fail
//! differently, with `The client could not reach the chain at http://127.0.0.1:PORT/graphql`. That
//! looked like the rebuild's fault and was not.

//! With the 4.0.35 file the client stopped EARLY: one account read, then "code was not produced
//! from the current compiled base". The loopback fixture in
//! `crates/dexdo/tests/oracle_read_command.rs` only ever had to answer that truncated exchange.
//! With the rebuilt file the client gets past the code check and reads every getter, and the
//! fixture printing its accepts showed what happens on the way: six requests served, then `accept`
//! won against the client's write by a hair and `read` returned `WouldBlock`. The fixture's
//! LISTENER is nonblocking so its loop can notice `stop`, and on macOS the accepted connection
//! inherits that flag, so `set_read_timeout` never applied to it. That error left the loop and took
//! the listener down mid-conversation.

//! A latent race, in the harness, that the old fixture was too stale to reach. Fixed there -- the
//! accepted connection is put back into blocking mode -- and the rebuilt account is applied here.

//! One more thing moves with the generation: the `version` getter answers from the CODE, so the
//! test now expects `4.0.36`.

use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use gosh_ackinacki::airegistry::deploy::local_context;
use tvm_block::{Deserializable as _, Serializable as _};

/// Where the pieces live, relative to this crate.
const FIXTURE: &str = "../dexdo/tests/fixtures/pmp-account.boc";
const COMPILED_PMP: &str = "../../contracts/compiled/dex/PMP.tvc";

/// The fixture's base64, as the test reads it: line-wrapped, joined without separators.
fn read_wrapped(path: &std::path::Path) -> Result<Vec<u8>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read fixture {}", path.display()))?;
    let joined: String = text.lines().collect();
    base64::engine::general_purpose::STANDARD
        .decode(joined)
        .with_context(|| format!("decode fixture base64 {}", path.display()))
}

/// Write base64 back in the shape the fixture already has: 100 characters a line.
fn write_wrapped(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 100 + 2);
    for (index, chunk) in encoded.as_bytes().chunks(100).enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
    }
    out.push('\n');
    std::fs::write(path, out).with_context(|| format!("write fixture {}", path.display()))
}

#[test]
#[ignore = "writes into the working tree: run it on purpose when the compiled contracts move"]
fn pmp_account_fixture_is_rebuilt_from_this_tree() {
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture_path = here.join(FIXTURE);
    let compiled_path = here.join(COMPILED_PMP);

    let account_bytes = read_wrapped(&fixture_path).expect("fixture reads");
    let root = tvm_types::read_single_root_boc(&account_bytes).expect("fixture is one BOC root");
    let mut account = tvm_block::Account::construct_from_cell(root).expect("fixture is an account");

    let current_code = account
        .get_code()
        .ok_or_else(|| anyhow!("the fixture account carries no code"))
        .expect("fixture has code");

    // THE SALT AND THE STORAGE BOTH CARRY THE GENERATION, and only the code cell used to be rebuilt.

    // The reasoning here was that a salt belongs to the DEPLOYMENT -- the address and its config --
    // and not to the generation. True of everything in it but one field: a PMP's salt carries
    // `privateNoteCode`, the note code this PMP mints against, and so does its storage under
    // `_privateNoteCode`. Both are the generation.

    // It went unnoticed because the fixture's own manifest declared the matching 4.0.35 hash, so
    // both sides of `validate_private_note_generation` came from the fixture and could not disagree.
    // removed the manifest copy, the expected side became the network's pin, and the stale
    // fields surfaced at once -- one after the other, because fixing the salt alone leaves the
    // storage disagreeing with it, which is a PMP no chain can produce.

    // So all three move together here: base code from the compiled artifact, salt rebuilt around the
    // vendored PrivateNote, storage re-encoded with the same cell in `_privateNoteCode`. The storage
    // round-trip is checked before anything is changed in it -- decode and re-encode unchanged must
    // reproduce the original cell -- so a re-encoding that quietly loses a field cannot pass.
    let context = local_context().expect("SDK context");
    let _carried_over = tvm_client::boc::get_code_salt(
        context.clone(),
        tvm_client::boc::ParamsOfGetCodeSalt {
            code: base64::engine::general_purpose::STANDARD
                .encode(tvm_types::write_boc(&current_code).expect("serialise the fixture's code")),
            ..Default::default()
        },
    )
    .expect("read the fixture's salt")
    .salt
    .expect("the fixture's code is salted");

    let private_note =
        super::contracts_provision::code_cell(super::contracts_provision::PRIVATENOTE_TVC)
            .expect("vendored PrivateNote is a code cell");
    let salt_cell = tvm_abi::TokenValue::pack_values_into_chain(
        &[tvm_abi::Token::new(
            "privateNoteCode",
            tvm_abi::TokenValue::Cell(private_note.clone()),
        )],
        Vec::new(),
        &tvm_abi::contract::ABI_VERSION_2_4,
    )
    .expect("pack the PMP code salt")
    .into_cell()
    .expect("build the PMP code salt cell");
    let salt = base64::engine::general_purpose::STANDARD
        .encode(tvm_types::write_boc(&salt_cell).expect("serialise the rebuilt salt"));

    // The storage carries the same code again, under `_privateNoteCode`. Re-encode it with the
    // vendored cell -- after proving the round-trip is faithful on the ORIGINAL, so a re-encoding
    // that drops or reorders a field cannot slip through as a rebuild.
    let abi = tvm_abi::Contract::load(super::contracts_provision::PMP_ABI.as_bytes())
        .expect("load the PMP ABI");
    let data = account
        .get_data()
        .expect("the fixture account carries data");
    let mut fields = abi
        .decode_storage_fields(
            tvm_types::SliceData::load_cell(data.clone()).expect("load the fixture data slice"),
            true,
        )
        .expect("decode the fixture storage");
    let round_tripped = tvm_abi::TokenValue::pack_values_into_chain(
        &fields,
        Vec::new(),
        &tvm_abi::contract::ABI_VERSION_2_4,
    )
    .expect("re-pack the fixture storage")
    .into_cell()
    .expect("build the re-packed storage cell");
    assert_eq!(
        round_tripped.repr_hash(),
        data.repr_hash(),
        "re-encoding the storage unchanged did not reproduce it, so a rebuilt storage cell would be \
         a different account rather than the same one with a newer note code"
    );

    let mut replaced = 0usize;
    for field in &mut fields {
        if field.name == "_privateNoteCode" {
            field.value = tvm_abi::TokenValue::Cell(private_note.clone());
            replaced += 1;
        }
    }
    assert_eq!(
        replaced, 1,
        "expected exactly one `_privateNoteCode` field in the PMP storage, found {replaced}"
    );
    let rebuilt_data = tvm_abi::TokenValue::pack_values_into_chain(
        &fields,
        Vec::new(),
        &tvm_abi::contract::ABI_VERSION_2_4,
    )
    .expect("pack the rebuilt storage")
    .into_cell()
    .expect("build the rebuilt storage cell");
    assert!(
        account.set_data(rebuilt_data),
        "the account refused the rebuilt storage"
    );

    let compiled = std::fs::read(&compiled_path).expect("compiled PMP reads");
    let base =
        super::contracts_provision::code_cell(&compiled).expect("compiled PMP is a code cell");

    let salted = tvm_client::boc::set_code_salt(
        context,
        tvm_client::boc::ParamsOfSetCodeSalt {
            code: base64::engine::general_purpose::STANDARD
                .encode(tvm_types::write_boc(&base).expect("serialise the compiled base")),
            salt,
            ..Default::default()
        },
    )
    .expect("apply the fixture's salt to the compiled base")
    .code;

    let salted_cell = tvm_types::read_single_root_boc(
        base64::engine::general_purpose::STANDARD
            .decode(salted)
            .expect("salted code decodes"),
    )
    .expect("salted code is one BOC root");

    assert!(
        account.set_code(salted_cell),
        "the account refused the rebuilt code, so it is not an active account with a state init"
    );
    let rebuilt = account
        .serialize()
        .and_then(|cell| tvm_types::write_boc(&cell))
        .expect("serialise the rebuilt account");

    write_wrapped(&fixture_path, &rebuilt).expect("fixture writes");

    // The test that reads this fixture also ADVERTISES its code hash, because that is what a chain
    // serves alongside an account. Print it: the two have to move together, and a rebuilt fixture
    // whose advertised hash was left behind fails with "account BOC code hash does not match its
    // advertised code hash" -- true, and about nothing the test is for.
    let rebuilt_code = tvm_block::Account::construct_from_cell(
        tvm_types::read_single_root_boc(&rebuilt).expect("rebuilt account is one BOC root"),
    )
    .expect("rebuilt account decodes")
    .get_code()
    .expect("rebuilt account carries code");
    println!(
        "rebuilt {} from {}\nPMP_CODE_HASH = \"{}\"",
        fixture_path.display(),
        compiled_path.display(),
        rebuilt_code.repr_hash().to_hex_string()
    );
}
