//! Fold the prover's own stderr chatter into the status line instead of letting it scroll.

//! The halo2 prover reports its phases by writing to stderr directly, and it takes no verbosity
//! setting: `[halo2-time] generate_proof (warm): 9.91s`, `[halo2-live] L0: target=...`. Those
//! lines are useful -- they are the only visibility into a step that runs for a minute and a half --
//! and useless as a transcript, because by the time one is printed the phase it names is over.

//! So descriptor 2 is pointed at a pipe for the duration of the prover call. A reader turns every
//! `[halo2-...]` line into the status line's label, and passes anything else through untouched: a
//! panic message, a warning, anything the prover might say that is not a timing, must still reach
//! the operator.

//! Only on unix, and only when the status line is live. A redirected or captured stderr is a
//! transcript already, and rewriting one into a cursor dance would corrupt it.

use crate::cli::progress::{lock, Shared, Status};

/// What `note deploy` prefixes its durable-state notes with.
const RECOVERY_PREFIX: &str = "note deploy recovery:";
use std::sync::{Arc, Mutex};

/// Fold the prover's stderr into `status` for as long as this is held.

/// A guard rather than a wrapped closure because the proving call is asynchronous: it is awaited
/// through a history-window bound, and there is no synchronous body to wrap. Installed once for a
/// whole command, so every phase line reaches the status line no matter which internal step
/// emitted it.

/// The redirection is process-wide, so a command that installs this must be the only thing writing
/// to stderr. `note deploy` is sequential, which is why it is the caller -- and [`StderrClaim`] is
/// what makes that a property of the code rather than of how the callers happen to be arranged.
pub(crate) struct ProverOutputFold {
    #[cfg(unix)]
    saved: libc::c_int,
    #[cfg(unix)]
    write_fd: libc::c_int,
    #[cfg(unix)]
    reader: Option<std::thread::JoinHandle<()>>,
    /// Released after [`Drop`] has put descriptor 2 back, so no second capture can start against a
    /// half-restored process.
    #[cfg(unix)]
    _claim: StderrClaim,
}

/// Sole ownership of descriptor 2 for as long as it is held.

/// Descriptor 2 is a resource of the PROCESS, not of the call that redirects it, and two overlapping
/// redirections do not merely interleave -- they destroy it. The second `dup` saves the FIRST
/// capture's pipe as "the real stderr"; when the first guard restores and closes that pipe, the
/// second guard's restore points descriptor 2 at a closed pipe, and everything the process writes to
/// stderr from then on goes nowhere. Not a panic, not an error: silence, for the rest of the run.

/// Under `cargo test` this is not hypothetical arithmetic. Test threads share one descriptor table,
/// so a capture installed by one test redirects the stderr of every test running beside it.

/// A second claim REFUSES rather than waits. Waiting would mean blocking a command behind another
/// command's whole proving run, and the cost of refusing is small and local: the prover's chatter
/// scrolls instead of folding into the status line. Losing the process's stderr is not small.
#[cfg(unix)]
struct StderrClaim;

#[cfg(unix)]
static STDERR_CLAIMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
impl StderrClaim {
    /// `None` when someone else already holds descriptor 2.
    fn take() -> Option<StderrClaim> {
        use std::sync::atomic::Ordering;
        STDERR_CLAIMED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| StderrClaim)
    }
}

#[cfg(unix)]
impl Drop for StderrClaim {
    fn drop(&mut self) {
        STDERR_CLAIMED.store(false, std::sync::atomic::Ordering::Release);
    }
}

impl ProverOutputFold {
    /// `None` when there is nothing to fold into -- a captured or redirected stderr is a
    /// transcript already, and rewriting one into a cursor dance would corrupt it.
    #[cfg(unix)]
    pub(crate) fn install(status: &Status) -> Option<Self> {
        use std::io::{BufRead as _, BufReader};
        use std::os::fd::FromRawFd as _;

        let shared = status.shared();
        if !lock(&shared).live {
            return None;
        }
        // Before a single descriptor moves: if anything else in this process already redirected
        // stderr, the prover's chatter scrolls and nothing is touched.
        let claim = StderrClaim::take()?;

        let mut fds = [0; 2];
        // SAFETY: `pipe` fills the two-element array it is given.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return None;
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // SAFETY: descriptor 2 is open; `dup` returns an owned copy or -1.
        let saved = unsafe { libc::dup(libc::STDERR_FILENO) };
        if saved < 0 {
            // SAFETY: both came from `pipe` above and are owned by nothing yet.
            unsafe {
                libc::close(read_fd);
                libc::close(write_fd);
            }
            return None;
        }
        // SAFETY: both are valid open descriptors.
        unsafe { libc::dup2(write_fd, libc::STDERR_FILENO) };

        let reader = {
            let shared = Arc::clone(&shared);
            // SAFETY: `read_fd` came from `pipe` and is handed to this File exclusively.
            let pipe = unsafe { std::fs::File::from_raw_fd(read_fd) };
            std::thread::spawn(move || drain(BufReader::new(pipe).lines(), &shared))
        };

        Some(Self {
            saved,
            write_fd,
            reader: Some(reader),
            _claim: claim,
        })
    }

    #[cfg(not(unix))]
    pub(crate) fn install(_status: &Status) -> Option<Self> {
        None
    }
}

#[cfg(unix)]
impl Drop for ProverOutputFold {
    fn drop(&mut self) {
        // Closing the write end is what ends the reader's loop: a pipe reports end of file only
        // once no descriptor can still write to it, so BOTH copies have to go -- the one this
        // guard holds and the one that replaced descriptor 2.
        // SAFETY: both are owned descriptors of this guard.
        unsafe {
            libc::dup2(self.saved, libc::STDERR_FILENO);
            libc::close(self.saved);
            libc::close(self.write_fd);
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(not(unix))]
impl Drop for ProverOutputFold {
    fn drop(&mut self) {}
}

#[cfg(unix)]
fn drain(lines: std::io::Lines<std::io::BufReader<std::fs::File>>, shared: &Arc<Mutex<Shared>>) {
    // The distance the chain had to cover when this wait was first seen. The prover reports the
    // remaining distance and never the span, so the first sighting is the only denominator there
    // is -- and a bar drawn from an invented one would be a lie told in a picture.
    for line in lines.map_while(Result::ok) {
        // Poisoned or not: a panic elsewhere is exactly when the pass-through below has to keep
        // working, because the panic's own message is arriving on this pipe.
        let mut guard = lock(shared);
        if let Some((target, latest)) = chain_wait(&line) {
            // Recorded, not drawn. A bar needs a denominator that means something, and the prover
            // reports a target that is recomputed between prints -- the remaining distance seen on
            // one line is not a fraction of the distance seen on the last. Drawing one from the
            // first sighting produced a bar that sat at nothing while the number beside it fell.

            // The raw line goes to `info` so a run under `RUST_LOG=info` records the whole
            // sequence, and the bar can be built from what the prover actually does rather than
            // from what it looked like it did.
            let togo = target.saturating_sub(latest);
            tracing::info!(target: "halo2_chain_wait", togo, target, latest, "{line}");
        }
        match phase_of(&line) {
            Some(phase) => {
                // A phase that is not the chain wait has no measure, and must not keep the last
                // one: a bar left standing under a different step reads as that step's progress.
                guard.label = phase;
                guard.started = std::time::Instant::now();
            }
            // Not a timing: it is something the prover wanted the operator to read.
            None if !line.trim().is_empty() => guard.line(&line),
            None => {}
        }
    }
}

/// Turn one prover line into what the status line should say, or `None` if it is not a phase
/// report and has to be passed through.

/// Kept deliberately shallow: it recognises the tag and shows the rest. A parser that understood
/// the prover's message shapes would silently start hiding lines the day the prover changed one.
/// The single exception is a trailing duration, which is dropped because the status line keeps its
/// own clock -- and it is dropped only when the tail really is one, so a `[halo2-live]` line whose
/// last colon sits inside `L0: target=...` keeps everything after it.
fn phase_of(line: &str) -> Option<String> {
    if let Some(phase) = prover_chatter(line) {
        return Some(phase);
    }
    if let Some((target, latest)) = chain_wait(line) {
        let togo = target.saturating_sub(latest);
        return Some(format!("waiting for the chain: {togo} blocks to go"));
    }
    if let Some(note) = line.trim().strip_prefix(RECOVERY_PREFIX) {
        // A recovery note says what was just written down so a rerun does not repeat a spend. That
        // is progress, not a result.

        // Only its first clause reaches the line. The note names the file it wrote and then
        // explains what a rerun will do -- a hundred and sixty characters, of which the live line
        // shows the first eighty and a path the operator is not being asked to open. The whole note
        // is in the log; the line says which voucher was written down.
        let head = note
            .split_once(" in /")
            .map(|(head, _)| head)
            .unwrap_or(note)
            .trim_end_matches([';', '.', ' '])
            .trim();
        return Some(format!("recovery: {head}"));
    }
    let rest = line
        .trim()
        .strip_prefix("[halo2-time]")
        .or_else(|| line.trim().strip_prefix("[halo2-live]"))?;
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    let described = match rest.rsplit_once(':') {
        Some((head, tail)) if is_duration(tail) => head.trim(),
        _ => rest,
    };
    Some(format!("proving: {described}"))
}

/// The chain wait the prover reports while it holds for a block height: `[halo2-live] L0:
/// target=8942208 (W=128, +227 blocks from event), latest=Some(8942017), wait=needed`.

/// Both numbers are in the line, so this is the one prover phase whose progress is knowable: how
/// far the chain still has to go. Everything else it says is a phase name with no measure in it,
/// and inventing one would be a bar that means nothing.
fn chain_wait(line: &str) -> Option<(u64, u64)> {
    let rest = line.trim().strip_prefix("[halo2-live]")?;
    let target = field_after(rest, "target=")?;
    let latest = field_after(rest, "latest=Some(")?;
    Some((target, latest))
}

/// The number that follows `key`, up to the first character that cannot be part of it.
fn field_after(text: &str, key: &str) -> Option<u64> {
    let start = text.find(key)? + key.len();
    let digits: String = text[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// The prover's untagged running commentary, recognised by its opening words.

/// It comes from the proving library itself, not from this repository, and carries no `[halo2-...]`
/// tag: `Loading cached KZG SRS from params/...`, `Generating proof...`, ` Proof: 2080 bytes`. A
/// note deploy proves three times, so each of these arrives three times, and each was landing in
/// the log as its own line.

/// An ALLOWLIST rather than a filter, deliberately. Everything not named here still passes through
/// untouched, which is the one property this module must not lose: a panic, a warning or anything
/// else the prover has to say reaches the operator. Recognising a fixed set of known chatter cannot
/// hide a message that was never on it.
fn prover_chatter(line: &str) -> Option<String> {
    let line = line.trim();
    for (opening, phase) in [
        (
            "Loading cached KZG SRS",
            "proving: loading the reference string",
        ),
        ("Generating proof", "proving: generating the proof"),
        ("Proof:", "proving: proof produced"),
    ] {
        if line.starts_with(opening) {
            return Some(phase.to_string());
        }
    }
    None
}

/// `" 9.91s (2080 bytes)"`, `" 10.16s"` -- a duration, optionally followed by a parenthesised aside.
fn is_duration(tail: &str) -> bool {
    let tail = tail.trim();
    let number = tail.split_whitespace().next().unwrap_or_default();
    number
        .strip_suffix('s')
        .is_some_and(|value| !value.is_empty() && value.parse::<f64>().is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every shape the prover is known to emit has to be recognised as a phase, or it scrolls.
    #[test]
    fn known_prover_lines_become_phases() {
        for (line, expected) in [
            (
                "[halo2-time] generate_proof (warm): 9.91s (2080 bytes)",
                "proving: generate_proof (warm)",
            ),
            (
                "[halo2-time] L1 prover wall-clock total: 10.16s",
                "proving: L1 prover wall-clock total",
            ),
            (
                "[halo2-time] L0 chain height >= target: 57.66s",
                "proving: L0 chain height >= target",
            ),
        ] {
            assert_eq!(phase_of(line).as_deref(), Some(expected), "{line}");
        }
    }

    /// Recovery notes are progress, not results, and must not accumulate on the screen.

    /// Only the clause that says WHAT was written down reaches the line. The path and the sentence
    /// about what a rerun does are for the log: on an 80-column window the note's own first eighty
    /// characters were a file path the operator is not being asked to open.
    #[test]
    fn a_recovery_note_keeps_its_first_clause_and_drops_the_path() {
        let line =
            "note deploy recovery: recorded deposit voucher proof in /tmp/pool.recovery.json; \
                    reruns will not re-spend this voucher.";
        let phase = phase_of(line).expect("a recovery note is a phase");
        assert_eq!(phase, "recovery: recorded deposit voucher proof");

        // The live one from a deploy, which is what sent this test here.
        let live = "note deploy recovery: marked SHELL gas voucher wallet submit as uncertain in \
                    /Users/somebody/Documents/GitHub/dexdo-cli-private-2/.dexdo-test2/pn_pool.json.recovery.json; \
                    a rerun will reconcile it.";
        assert_eq!(
            phase_of(live).as_deref(),
            Some("recovery: marked SHELL gas voucher wallet submit as uncertain")
        );
    }

    /// The one prover line with a measure in it: both the target height and the chain's current
    /// one. A wait of a hundred and ninety-one blocks is a wait an operator can sit through; "L0:
    /// target=8942208 (W=128, +227 blocks from event), latest=Some(8942017), wait=needed" is not.
    #[test]
    fn the_chain_wait_is_read_as_how_far_there_is_to_go() {
        let line = "[halo2-live] L0: target=8942208 (W=128, +227 blocks from event), latest=Some(8942017), wait=needed";
        assert_eq!(chain_wait(line), Some((8942208, 8942017)));
        assert_eq!(
            phase_of(line).as_deref(),
            Some("waiting for the chain: 191 blocks to go")
        );
    }

    /// A line of the same family without both numbers must not be read as a wait: half a measure
    /// is worse than none, because it renders as progress that is not being measured.
    #[test]
    fn a_line_without_both_numbers_is_not_a_chain_wait() {
        assert_eq!(
            chain_wait("[halo2-live] L0: target=8942208, latest=None"),
            None
        );
        assert_eq!(
            chain_wait("[halo2-time] generate_proof (warm): 9.91s"),
            None
        );
    }

    /// A colon that is part of the message, not a duration separator, must not truncate it.
    #[test]
    fn a_colon_inside_the_message_is_not_a_duration_separator() {
        assert_eq!(
            phase_of("[halo2-live] L0: target=8700416, wait=needed").as_deref(),
            Some("proving: L0: target=8700416, wait=needed")
        );
    }

    /// The prover's own untagged commentary, which a deploy emits three times over -- once per
    /// proof -- and which was landing in the log line by line.
    #[test]
    fn the_provers_untagged_chatter_becomes_a_phase() {
        for (line, expected) in [
            (
                "Loading cached KZG SRS from params/halo2_cache/hermez_kzg_srs_k19.bin",
                "proving: loading the reference string",
            ),
            ("Generating proof...", "proving: generating the proof"),
            ("  Proof: 2080 bytes", "proving: proof produced"),
        ] {
            assert_eq!(phase_of(line).as_deref(), Some(expected), "{line}");
        }
    }

    /// Two captures must never overlap: the second one would save the first one's pipe as "the real
    /// stderr", and the first one's restore would then leave descriptor 2 pointing at a closed pipe
    /// -- the process writes to nowhere for the rest of its run, silently.

    /// The claim is asserted directly rather than through `install`, which needs a live status line
    /// and a terminal; the invariant under test is ownership of the descriptor, and it holds whether
    /// or not there is anything to draw.
    #[cfg(unix)]
    #[test]
    fn a_second_capture_is_refused_while_the_first_holds_the_descriptor() {
        let first = StderrClaim::take().expect("nothing else holds stderr in this test");
        assert!(
            StderrClaim::take().is_none(),
            "a second capture must refuse rather than redirect on top of the first"
        );
        drop(first);
        assert!(
            StderrClaim::take().is_some(),
            "the claim must be released once the capture is gone, or one deploy silences the rest"
        );
    }

    /// Anything that is not a timing must pass through: hiding a message the prover meant for the
    /// operator is the one failure this module must not have.
    #[test]
    fn anything_else_is_passed_through() {
        for line in [
            "thread 'main' panicked at prover.rs:12",
            "warning: falling back",
            "",
        ] {
            assert_eq!(phase_of(line), None, "{line}");
        }
    }
}
