//! the deal-record writer replaces the file rather than writing into it.

//! There are TWO atomic writers in this client, not one. `note.rs` and `deals.rs` each carry their
//! own `write_private_atomic`, and each holds the same property by the same means: a temp file, an
//! fsync, and a `rename` over the top (`note.rs:1660`, `deals.rs:805`). PR1605 pinned the first.
//! This pins the second, because what holds the property there is one hand-written `rename` and
//! nothing else, and the two writers can drift apart without either one looking wrong on its own.

//! THE FORM IS THE ONE PR1605 ESTABLISHED, and it is a form for a reason. Both writers leave the
//! same bytes at the path, so the finished document cannot tell them apart. What separates them is
//! the FILE the old directory entry pointed at: a second name for that file, taken before the
//! rewrite, still reads the OLD document afterwards, because `rename` rebinds a name and leaves the
//! file alone. A writer that opens the target and writes through keeps ONE file under both names,
//! so the second name reads the new document and there was no instant at which the old bytes were
//! safe. A content comparison cannot make that distinction; a second name can.

//! WHY A DEAL RECORD IS WORTH IT. It is the local record of a deal with money in escrow -- the note
//! it was funded from, the token contract holding the position, the order ids this client created.
//! `save_deal_handle` rewrites it over the top of the previous one on every restart, and
//! `persist_last_observed_promotion` rewrites it again on every observation, so the rewrite path is
//! the common one rather than the rare one. A write that lands half of it destroys the only
//! complete copy before the replacement is whole, and nothing on chain announces that.

//! Declared from `cli/mod.rs` rather than from inside `deals.rs`, so that a revert of the file this
//! guards cannot take the guard with it.

use crate::cli::deals::{
    load_deal_handle, make_handle_id, save_deal_handle, DealHandle, DealHandleRole,
    DEAL_HANDLE_VERSION,
};

/// A record whose only varying field is the one asserted at the end, so "the rewrite landed" and
/// "the rewrite landed WHOLE" are the same observation.
fn deal_record(created_at_unix: u64) -> DealHandle {
    let token_contract = format!("0:{}", "3".repeat(64));
    DealHandle {
        version: DEAL_HANDLE_VERSION,
        handle: make_handle_id(&token_contract, DealHandleRole::Buyer),
        role: DealHandleRole::Buyer,
        network: "net-a".into(),
        token_contract,
        note_addr: format!("0:{}", "4".repeat(64)),
        frame_model: "qwen/qwen3-32b".into(),
        model_hash: None,
        order_book: None,
        root_model: None,
        market: None,
        contracts: "manifest/net-a.manifest.json".into(),
        endpoint: None,
        created_order_ids: vec![],
        created_at_unix,
    }
}

#[test]
fn the_deal_record_writer_replaces_the_file_rather_than_writing_into_it() {
    let temp = tempfile::tempdir().expect("temp dir");
    let dir = temp.path().join("deals");

    let path = save_deal_handle(&dir, &deal_record(1)).expect("the first deal record write lands");
    let first = std::fs::read_to_string(&path).expect("the first write is readable");

    // A second name for the file the path points at NOW, taken before the rewrite.
    let first_entry = dir.join("first_entry_hard_link.json");
    std::fs::hard_link(&path, &first_entry).expect("a second name for the first write");

    #[cfg(unix)]
    let before = {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(&path)
            .expect("stat the first write")
            .ino()
    };

    // A different document, through the same writer, at the same path.
    let rewritten =
        save_deal_handle(&dir, &deal_record(2)).expect("the second deal record write lands");
    assert_eq!(
        rewritten, path,
        "the rewrite must be of the same record, not a second file"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let after = std::fs::metadata(&path)
            .expect("stat the second write")
            .ino();
        assert_ne!(
            before, after,
            "the writer must rename a new file over the target, not write into it: an in-place \
             write has an instant where the previous complete document is already gone"
        );
    }

    // The same property where there is no inode to name: the rewrite rebound the PATH, so the file
    // the earlier name still points at was never opened for writing.
    assert_eq!(
        std::fs::read_to_string(&first_entry).expect("the first write is still readable"),
        first,
        "the writer must rebind the path to a new file and leave the previous document intact: \
         writing through to it destroys the only complete copy before the replacement is whole"
    );

    // Whole document either way: a rename cannot leave a prefix behind.
    let second = std::fs::read_to_string(&path).expect("the second write is readable");
    assert!(
        serde_json::from_str::<serde_json::Value>(&second).is_ok(),
        "what lands at the path parses as a whole document"
    );
    assert_ne!(
        first, second,
        "the fixtures differ, so the rewrite is observable"
    );
    assert_eq!(
        load_deal_handle(&path)
            .expect("the rewritten record reads back as a deal handle")
            .created_at_unix,
        2,
        "and what lands at the path is the document the second write carried"
    );

    // And no temp file is left behind to be mistaken for a deal record.
    let strays: Vec<_> = std::fs::read_dir(&dir)
        .expect("read the deals directory back")
        .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
        .filter(|name| name.to_string_lossy().contains(".tmp."))
        .collect();
    assert!(
        strays.is_empty(),
        "a completed write leaves no temp file: {strays:?}"
    );
}

/// What the guard above cannot see, asserted rather than described, so nobody has to trust a header
/// or a review comment.

/// The two observations both guards make -- the inode changed, and a second name taken before the
/// rewrite still reads the old document -- answer "was the target opened and written THROUGH?".
/// That is narrower than "was the previous complete document safe at every instant?", and a writer
/// that UNLINKS the target and creates it again falls straight through the gap: there is a window
/// in which the path holds nothing, then a growing prefix, and both observations are satisfied
/// anyway.

/// So this runs BOTH writers through one observation -- the real production one via
/// `save_deal_handle`, and an unlink-then-create stand-in for a third writer added without the
/// rename -- and asserts the observations return the SAME verdict. They cannot separate them. The
/// production arm is the real writer rather than a copy of it, so this stays tied to what the guard
/// actually watches.

/// WHEN THIS TEST STARTS FAILING, the guard has become stronger than its own documentation and both
/// headers must be rewritten. Closing this needs a DIFFERENT MECHANISM -- observing the replacement
/// being built before the target is touched at all, or fault injection at the interrupted instant --
/// not one more assertion about the finished state, because the finished states are identical. An
/// assertion added to the guard must be added to `Observed` here too, and if it starts telling the
/// two writers apart, this test is what says so.

/// Read together with `a_torn_recovery_is_kept_and_left_exactly_as_found` in the note bundle, which
/// has a related blind spot: it stays green against a writer with no temp-and-rename at all, because
/// it pins how a torn file is HANDLED rather than how a write is INTERRUPTED -- its green is not
/// coverage of the writer.

/// Unix only, because the identity being compared is the Unix spelling; the ceiling itself is not
/// platform-specific.
#[cfg(unix)]
#[test]
fn neither_observation_catches_an_unlink_then_create_writer_and_this_is_the_known_ceiling() {
    use std::os::unix::fs::MetadataExt as _;
    use std::path::{Path, PathBuf};

    /// Exactly what the guard above looks at across a rewrite, as a value that can be compared.
    #[derive(Debug, PartialEq, Eq)]
    struct Observed {
        inode_changed: bool,
        second_name_still_reads_the_old_document: bool,
    }

    fn observe(rewrite: impl Fn(u64) -> PathBuf, link_at: &Path) -> Observed {
        let path = rewrite(1);
        let first = std::fs::read_to_string(&path).expect("the first write is readable");
        std::fs::hard_link(&path, link_at).expect("a second name for the first write");
        let before = std::fs::metadata(&path)
            .expect("stat the first write")
            .ino();

        let rewritten = rewrite(2);
        assert_eq!(rewritten, path, "both writes must land at the same path");
        let after = std::fs::metadata(&path)
            .expect("stat the second write")
            .ino();

        Observed {
            inode_changed: before != after,
            second_name_still_reads_the_old_document: std::fs::read_to_string(link_at)
                .expect("the first write is still readable")
                == first,
        }
    }

    let temp = tempfile::tempdir().expect("temp dir");

    // The real writer, driven through the production path.
    let atomic_dir = temp.path().join("atomic");
    let atomic = observe(
        |generation| {
            save_deal_handle(&atomic_dir, &deal_record(generation))
                .expect("the production write lands")
        },
        &temp.path().join("atomic_first_entry.json"),
    );

    // A third writer, added without the temp-and-rename.
    let unlinked_dir = temp.path().join("unlinked");
    std::fs::create_dir_all(&unlinked_dir).expect("create the directory");
    let unlinked = observe(
        |generation| {
            let path = unlinked_dir.join("record.json");
            let _ = std::fs::remove_file(&path);
            std::fs::write(&path, format!("{{\"generation\":{generation}}}"))
                .expect("the replacement lands");
            path
        },
        &temp.path().join("unlinked_first_entry.json"),
    );

    assert_eq!(
        atomic, unlinked,
        "the guard's observations return the same verdict for the atomic writer and for one that \
         unlinks and recreates, so they cannot separate them: that is the ceiling"
    );
    assert!(
        atomic.inode_changed && atomic.second_name_still_reads_the_old_document,
        "and the verdict they share is the PASSING one, which is what makes the ceiling matter"
    );

    // The instant the observations are taken too late to see, made visible by performing only the
    // FIRST half of that writer. The path holds no document at all here; in production no second
    // name is holding the previous one either, so at this instant the only complete copy is gone.
    let path = unlinked_dir.join("record.json");
    std::fs::remove_file(&path).expect("the writer's first step");
    assert!(
        !path.exists(),
        "between the two steps the path holds nothing, and no assertion about the finished \
         document can reach this state"
    );
}
