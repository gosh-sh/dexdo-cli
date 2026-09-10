//! a funding-wallet lock left behind by a KILLED holder must not wedge the next spender.

//! Live on 2026-08-17 a mint process was killed while it held the funding wallet's turn. The lock it
//! left behind was a sentinel file carrying `pid=<dead pid>`, and the next `dexdo note deploy`
//! printed `already in use locally; waiting for <lockfile>` and waited with no bound at all. The
//! whole note stock stopped until an operator deleted the file by hand.

//! had already replaced that sentinel with an `fs2` advisory lock, which the KERNEL drops when
//! the holder dies by any route -- SIGKILL included -- so the reported wedge cannot form. What was
//! missing is the proof, and a fix nothing measures is a fix that can be undone by the next edit.

//! Every assertion here therefore outlives a process. `funding_wallet_turn_is_released_when_the_
//! holder_drops_1291` releases the turn by dropping the guard IN THIS process, which exercises
//! `Drop` and nothing the kernel does; a holder that runs `Drop` is exactly the holder that never
//! wedged anything. This test spawns a real child, waits until it really holds the turn, kills it
//! with SIGKILL so it can run no cleanup of any kind, and requires the next acquisition to proceed.

//! The two halves are both load-bearing:

//! * while the child is ALIVE the parent must be REFUSED -- otherwise a lock that never locks
//! would pass the second half vacuously, and must not be "fixed" by weakening the turn;
//! * the child's exit status must be death BY SIGNAL 9 -- otherwise a child that exited normally
//! would have released the lock politely, which is the case that never needed proving.

//! Bounded throughout, because the regression under test IS an unbounded wait: EVERY acquisition
//! runs on a worker thread behind `recv_timeout`, so a reintroduced wedge -- on the held turn or on
//! the dead one -- fails this test instead of hanging CI forever.

use std::os::unix::process::ExitStatusExt as _;
use std::time::{Duration, Instant};

/// Set on the child only. Its presence is what turns the child entry point below from a no-op into
/// the holder process, and it carries the wallet so both sides hash the SAME lock key.
const CHILD_WALLET_VAR: &str = "DEXDO_TEST_1410_CHILD_WALLET";
/// Where the child reports that the turn is really taken, so the parent never races the spawn.
const CHILD_READY_VAR: &str = "DEXDO_TEST_1410_CHILD_READY";
/// The child entry point, named once and used both to declare it and to filter for it.
const CHILD_TEST_NAME: &str = "child_holds_the_funding_wallet_until_it_is_killed_1410";

/// The network half of the lock key. Any label works; it only has to match on both sides.
const NETWORK: &str = "net-a";

/// `Child::kill` is documented as SIGKILL on Unix. Naming the number keeps the assertion readable
/// without pulling `libc` into this crate for one integer.
const SIGKILL: i32 = 9;

/// How long the child may wait for a turn nothing else should be holding.
const CHILD_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the child holds the turn if the parent never kills it. A self-imposed ceiling, so a
/// parent that dies mid-test cannot leak a process that sleeps forever on a machine-wide lock.
const CHILD_MAX_HOLD: Duration = Duration::from_secs(300);
/// How long the parent waits for the child to report the turn is taken. Generous: this covers
/// process spawn and test-harness startup on a loaded CI machine.
const CHILD_READY_TIMEOUT: Duration = Duration::from_secs(60);
/// The bounded wait the parent gives a turn it expects to be REFUSED, while the holder is alive.
const REFUSAL_TIMEOUT: Duration = Duration::from_secs(1);
/// The bounded wait the parent gives the turn of a DEAD holder.
const REACQUIRE_TIMEOUT: Duration = Duration::from_secs(15);
/// The ceiling on any single acquisition. Strictly greater than both waits above, so a bounded
/// refusal is always reported as a refusal and only a genuine unbounded wedge trips it.
const ACQUIRE_HANG_BOUND: Duration = Duration::from_secs(60);

/// A funding wallet nothing else on this machine can be using.

/// The lock deliberately lives in one machine-wide directory, so a fixed address here would make two
/// concurrent test binaries contend for one real lock file and turn this into a race.
fn unique_wallet_account_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default();
    format!("{:032x}{:032x}", std::process::id(), nanos)
}

/// The libtest filter that selects the child entry point in a re-executed copy of this binary.

/// Derived from `module_path!()` rather than written out, so moving or renaming this module cannot
/// leave a stale string that silently selects no test. The crate segment is dropped because libtest
/// names tests from the crate root's children down.
fn child_test_filter() -> String {
    let module = module_path!();
    let path = module
        .split_once("::")
        .map(|(_, rest)| rest)
        .unwrap_or(module);
    format!("{path}::{CHILD_TEST_NAME}")
}

/// Take the funding wallet's turn on a worker thread, under a hard ceiling.

/// NO acquisition in this test may run on the thread that owns the assertions. The defect under test
/// IS an unbounded wait, so an acquisition called directly would let a reintroduced wedge hang the
/// suite forever instead of failing it -- and a test that hangs reports nothing at all. `None` means
/// the call never came back within `ceiling`, which is the wedge itself.
fn acquire_within(
    wallet: &str,
    timeout: Duration,
    ceiling: Duration,
) -> Option<Result<(), String>> {
    let (report, collect) = std::sync::mpsc::channel();
    let wallet = wallet.to_string();
    std::thread::spawn(move || {
        let outcome = super::acquire_funding_wallet_lock_with_timeout(NETWORK, &wallet, timeout)
            // Released immediately: only whether the turn was obtainable matters here.
            .map(|_| ())
            .map_err(|error| format!("{error:#}"));
        let _ = report.send(outcome);
    });
    collect.recv_timeout(ceiling).ok()
}

/// Kills the holder however this test leaves, so a failed assertion cannot leak a process still
/// sitting on a real machine-wide lock.
struct HolderProcess(std::process::Child);

impl Drop for HolderProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The child entry point: take the funding wallet's turn for real, say so, then never let go.

/// A no-op in an ordinary run. Only a parent that sets `CHILD_WALLET_VAR` turns it into the holder,
/// and the turn is taken through the SAME production helper `note deploy` and `note topup` use --
/// not a hand-rolled `flock` that would prove something about this test instead of about the client.
#[test]
fn child_holds_the_funding_wallet_until_it_is_killed_1410() {
    let Ok(wallet) = std::env::var(CHILD_WALLET_VAR) else {
        return;
    };
    let ready = std::env::var(CHILD_READY_VAR).expect("the parent names the readiness file");

    let _held =
        super::acquire_funding_wallet_lock_with_timeout(NETWORK, &wallet, CHILD_ACQUIRE_TIMEOUT)
            .expect("the holder takes the funding wallet's turn");

    // Only AFTER the turn is really held, so the parent can never mistake a spawned process for a
    // holding one.
    std::fs::write(&ready, "held").expect("report that the turn is taken");

    // Held until killed. There is no release path on purpose: the point of the test is a holder that
    // runs no cleanup, so `Drop` here must never get the chance to run.
    let deadline = Instant::now() + CHILD_MAX_HOLD;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The regression: a turn whose holder was SIGKILLed is free, and the next spender takes it.
#[test]
fn a_killed_holder_does_not_wedge_the_next_funding_wallet_spender_1410() {
    let wallet = format!("0:{}", unique_wallet_account_id());
    let temp = tempfile::tempdir().expect("temp dir");
    let ready = temp.path().join("holder-took-the-turn");

    let mut holder = HolderProcess(
        std::process::Command::new(std::env::current_exe().expect("this test binary"))
            .args([
                "--exact",
                &child_test_filter(),
                "--test-threads=1",
                "--nocapture",
            ])
            .env(CHILD_WALLET_VAR, &wallet)
            .env(CHILD_READY_VAR, &ready)
            // The harness chatter is noise; a panic in the child is not, and must stay visible.
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("spawn a real holder process"),
    );

    // Wait for the turn to be really taken. Liveness is checked on every pass: a child that died at
    // launch never creates the readiness file and would otherwise look exactly like a slow one.
    let ready_deadline = Instant::now() + CHILD_READY_TIMEOUT;
    while !ready.exists() {
        if let Some(status) = holder.0.try_wait().expect("poll the holder") {
            panic!(
                "the holder exited before it took the funding wallet's turn ({status:?}); \
                 this test proves nothing unless a real process really holds the lock"
            );
        }
        assert!(
            Instant::now() < ready_deadline,
            "the holder never reported taking the funding wallet's turn within {}s",
            CHILD_READY_TIMEOUT.as_secs()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // A LIVE holder is still respected. Without this, a lock that never locks would sail through the
    // second half, and would be "fixed" by removing the protection it is about.
    match acquire_within(&wallet, REFUSAL_TIMEOUT, ACQUIRE_HANG_BOUND) {
        Some(Err(refused)) => assert!(
            refused.contains("funding wallet busy"),
            "the refusal must name the funding wallet's turn, got: {refused}"
        ),
        Some(Ok(())) => panic!(
            "a second spender took the funding wallet while a LIVE holder had it: the turn must not \
             be weakened to cure "
        ),
        None => panic!(
            "the refusal did not return within {}s while the holder was alive: the wait on a held \
             turn is unbounded, which is the shape of ",
            ACQUIRE_HANG_BOUND.as_secs()
        ),
    }

    // The holder dies without running a single line of cleanup.
    holder.0.kill().expect("SIGKILL the holder");
    let status = holder.0.wait().expect("reap the killed holder");
    assert_eq!(
        status.signal(),
        Some(SIGKILL),
        "the holder must die BY SIGKILL, or it released the turn politely and this proves nothing; \
         got {status:?}"
    );

    // The turn of a dead holder must be free.
    match acquire_within(&wallet, REACQUIRE_TIMEOUT, ACQUIRE_HANG_BOUND) {
        Some(Ok(())) => {}
        Some(Err(refusal)) => panic!(
            "the funding wallet's turn is still refused {}s after its holder was SIGKILLed: \
             {refusal}",
            REACQUIRE_TIMEOUT.as_secs()
        ),
        None => panic!(
            "acquiring the funding wallet did not return within {}s after the holder was SIGKILLed: \
             a lock a dead process still owns wedges every later command, which is  exactly",
            ACQUIRE_HANG_BOUND.as_secs()
        ),
    }

    let path = super::funding_wallet_lock_path(NETWORK, &wallet).expect("lock path");
    let _ = std::fs::remove_file(&path);
}
