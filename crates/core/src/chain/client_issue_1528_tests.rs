//! Issue: the clock-skew preflight reads both clocks inside one attempt.

//! The preflight took the local clock as its first argument and the chain time as its second, and
//! the chain read was the only bare one left of its kind -- a single dropped TLS request ended a
//! `note deploy` that had already spent the operator's time confirming a transfer in the wallet.

//! The obvious repair is to wrap the chain read in the retry that three sibling call sites already
//! use, and it is wrong. Arguments are evaluated in order, so the local reading is taken before the
//! chain call starts; retrying only the chain call moves the second reading later by the retry
//! duration, and every second of that drift is measured as "local is behind".

//! ```text
//! TRANSIENT_READ_TOTAL_BUDGET = 45s (params.rs)
//! MAX_CLOCK_BEHIND_SECS = 30s (SDK_MESSAGE_EXPIRY_SECS - safety margin)
//! MAX_CLOCK_AHEAD_SECS = 250s
//! ```

//! 45 is larger than 30. A perfectly synchronised machine that caught one slow attempt would be
//! refused and told to fix system time that was never wrong -- on the money path, once per note.

//! So what these rows hold is not "the read is retried" but **the measured skew does not grow with
//! the number of retries**. A later change to the retry budget breaks the first without touching the
//! second, which is exactly how this defect would come back.

use std::cell::Cell;

/// The chain and the machine agree exactly. Any skew these rows measure is therefore an artefact of
/// WHEN the two readings were taken, which is the whole subject.
const T0: u64 = 1_787_000_000;

/// A virtual wall clock: `T0` plus however much of the paused runtime's time has been consumed.
/// Retries consume it through the attempt timeout, so this moves exactly as a real clock would over
/// a run that had to retry.
fn virtual_now(started: tokio::time::Instant) -> u64 {
    T0 + started.elapsed().as_secs()
}

/// Both readings come from the successful attempt, so a run that retried measures the same skew as
/// a run that did not: zero, on a machine whose clock is correct.
#[tokio::test(start_paused = true)]
async fn the_measured_skew_does_not_grow_with_the_number_of_retries() {
    let started = tokio::time::Instant::now();
    let attempts = Cell::new(0_u32);

    let (local_unix, chain_unix) = super::clock_skew_sample_from_one_attempt(
        || async {
            attempts.set(attempts.get() + 1);
            if attempts.get() <= 2 {
                // An attempt that never answers: the retry wrapper's own ceiling ends it, which is
                // the shape a dropped TLS request takes. Each one burns the attempt timeout.
                std::future::pending::<()>().await;
            }
            Ok(virtual_now(started))
        },
        || Ok(virtual_now(started)),
    )
    .await
    .expect("a transient read that eventually answers must not fail the preflight");
    let check = super::clock_skew_check(local_unix, chain_unix);

    assert_eq!(attempts.get(), 3, "the fixture must actually have retried");
    let burned = started.elapsed().as_secs();
    assert!(
        burned >= 40,
        "the fixture must burn enough time for the old shape to fail: {burned}s"
    );

    assert_eq!(
        check.status,
        super::ChainDoctorStatus::Pass,
        "a correct clock must pass however many attempts were spent: {}",
        check.message
    );
    assert!(
        // `skew=`, not a bare `0s`: that substring also matches `skew=10s`, which is the very thing
        // this row exists to catch.
        check.message.contains("skew=0s"),
        "the measured skew must be zero, not the retry duration: {}",
        check.message
    );

    // The defect, stated with the same numbers: had the local reading been taken before the retries
    // instead of inside the successful attempt, this is the verdict the operator would have got.
    let old_shape = super::clock_skew_check(T0, T0 + burned);
    assert_eq!(
        old_shape.status,
        super::ChainDoctorStatus::Fail,
        "this test proves nothing unless the pre-fix ordering would have been refused: {}",
        old_shape.message
    );
    assert!(
        old_shape.message.contains("behind"),
        "and refused in the direction with no headroom: {}",
        old_shape.message
    );
}

/// The chain request's OWN duration is not measured as skew either.

/// Found by red-checking the row above: reversing the two readings inside the attempt left it green,
/// because in that fixture the successful attempt consumes no time. So the row above holds "the
/// local clock is not read before the retries" and nothing about the order within an attempt. This
/// one holds the order, by giving the chain request a duration long enough that measuring it would
/// refuse a correct clock.
#[tokio::test(start_paused = true)]
async fn the_chain_request_duration_is_not_measured_as_skew() {
    let started = tokio::time::Instant::now();
    // Under the retry wrapper's own per-attempt ceiling, so this row measures the ordering rather
    // than the timeout: a request slower than that ceiling is a different subject.
    let slow_request = 15;

    let (local_unix, chain_unix) = super::clock_skew_sample_from_one_attempt(
        || async {
            tokio::time::sleep(std::time::Duration::from_secs(slow_request)).await;
            Ok(virtual_now(started))
        },
        || Ok(virtual_now(started)),
    )
    .await
    .expect("a slow but successful read is not a failure");
    let check = super::clock_skew_check(local_unix, chain_unix);

    // Asserted on the measured value, not on the verdict: 15s is inside the permitted band, so a
    // status assertion would pass either way and hold nothing.
    assert!(
        check.message.contains("skew=0s"),
        "the request's own {slow_request}s were measured as clock skew: {}",
        check.message
    );
}

/// A run that needs no retry is unchanged: one attempt, one pair of readings, the same verdict.
#[tokio::test(start_paused = true)]
async fn a_first_attempt_answer_is_measured_exactly_as_before() {
    let started = tokio::time::Instant::now();
    let attempts = Cell::new(0_u32);

    let (local_unix, chain_unix) = super::clock_skew_sample_from_one_attempt(
        || async {
            attempts.set(attempts.get() + 1);
            Ok(virtual_now(started))
        },
        || Ok(virtual_now(started)),
    )
    .await
    .expect("a clock read that answers at once cannot fail");
    let check = super::clock_skew_check(local_unix, chain_unix);

    assert_eq!(attempts.get(), 1);
    assert_eq!(check.status, super::ChainDoctorStatus::Pass);
}

/// A clock that really is wrong is still refused. Narrowing what the preflight blames must not
/// switch off the check it exists to make.
#[tokio::test(start_paused = true)]
async fn a_genuinely_skewed_clock_is_still_refused() {
    let started = tokio::time::Instant::now();
    let behind = super::MAX_CLOCK_BEHIND_SECS + 5;

    let (local_unix, chain_unix) = super::clock_skew_sample_from_one_attempt(
        || async { Ok(virtual_now(started) + behind) },
        || Ok(virtual_now(started)),
    )
    .await
    .expect("a verdict is a verdict, not a transport error");
    let check = super::clock_skew_check(local_unix, chain_unix);

    assert_eq!(
        check.status,
        super::ChainDoctorStatus::Fail,
        "{}",
        check.message
    );
    assert!(check.message.contains("behind"), "{}", check.message);
}
