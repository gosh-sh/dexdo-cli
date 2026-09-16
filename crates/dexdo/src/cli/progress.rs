//! What a long command is doing, while it does it.

//! A command that runs for minutes has two audiences that want opposite things. An operator
//! watching wants to know it is alive, which step it is on, and what is still ahead; a log file or
//! a machine consumer wants a record that does not depend on a cursor. Both are served here: the
//! drawing lives in [`super::progress_draw`], the checklist model in [`super::progress_plan`], and
//! this module is what a command calls.

//! Everything goes to stderr. Stdout stays the command's result, which is what a caller parses.

//! Steps a caller declares are not the only thing shown: the prover writes its own phase timings
//! to stderr and cannot be asked not to, so [`super::progress_capture`] folds those lines into the
//! same status line instead of letting them scroll.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

pub(super) use super::progress_draw::Shared;
use super::progress_draw::TICK;
use super::progress_plan::Plan;

/// Take the status display's lock, poisoned or not.

/// A mutex is poisoned when a thread panics while holding it. For state that decides money that
/// would be the right moment to stop; for a decoration it is the opposite. The one caller of this
/// module is `note deploy`, which spends from the operator's wallet and then proves for a minute
/// and a half, and a panic there is already being unwound with a message the operator needs. A
/// second panic, raised by the thing that draws the spinner, would replace that message with its
/// own and abort a command mid-mint.

/// Nothing under this lock can be left half-written by a panic: it is a label, an instant, a plan
/// cursor, a frame counter and a descriptor, each replaced by a single assignment. So the data
/// behind a poisoned lock is as usable as the data behind a healthy one, and taking it is not a
/// papered-over bug.
pub(super) fn lock(shared: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// The status display of the command running on THIS thread, for code too far from it to be handed
// one.

// `note deploy` reaches the wallet funding wait through several modules, and that wait is the step
// that most needs to name itself -- it is not work, it is the client stopped until a transfer is
// confirmed in a phone. Threading a handle through every layer in between would put a display
// parameter on functions that have nothing to do with displaying anything.

// Per THREAD, and that is the whole of it. A command and every layer it reaches are one thread:
// `#[tokio::main]` drives the command's own future with `block_on`, which polls it on the thread
// that called it and never hands it to a worker to steal. One slot per process instead says
// something that is only true of a process running one command, and this one is not always that:
// the suite runs 872 commands' worth of code in threads of a single process, and there each
// display answered for whichever command registered last. Measured on the slot this replaces: 200
// runs of the suite at `--test-threads=64` failed 47 times in this module -- a command reading a
// display it had not built -- and the same 200 runs fail none once the slot is per thread.

// A `tokio::spawn`ed task runs on another thread and so has no display -- the same silence a
// command that never built one gets, rather than a claim on someone else's line. Nothing draws
// progress from a spawned task today: the buyer's renewal task is the only one that reaches code
// which can, and it calls it with `human_model: None`, which is the condition the stepping block
// is inside.

// Weak on purpose: this must never be what keeps a status line alive past its command. A command
// with no display -- every other command in the client -- leaves this empty, and `step` is then
// a no-op rather than a reason for callers to know whether a display exists.
thread_local! {
    static CURRENT: RefCell<Option<Weak<Mutex<Shared>>>> = const { RefCell::new(None) };
}

fn current() -> Option<Arc<Mutex<Shared>>> {
    // A thread being torn down has no display to speak of, which is the answer this already has a
    // meaning for. Nothing here may raise a second panic on top of the one being unwound.
    CURRENT
        .try_with(|showing| showing.borrow().as_ref().and_then(Weak::upgrade))
        .unwrap_or(None)
}

/// A weak, command-owned route to one status display.

/// Most command work stays on the thread that registered [`CURRENT`], but a lazy local API starts
/// its first purchase from an Axum task. Carrying this handle lets that task update only the
/// command that created it, without turning the thread-local slot back into process-global state.
#[derive(Clone)]
pub(crate) struct ProgressHandle {
    shared: Weak<Mutex<Shared>>,
}

impl ProgressHandle {
    fn current(&self) -> Option<Arc<Mutex<Shared>>> {
        self.shared.upgrade()
    }

    pub(crate) fn refresh_live_step(&self, label: impl Into<String>) -> bool {
        refresh_live_step_on(self.current(), label.into())
    }

    /// Temporarily draw a spawned task's wait over the command's live label.

    /// The on-demand API can accept its first request before the command publishes the endpoint
    /// handoff. The request-owned wait must become the live line while it exists. The command may
    /// still advance while the wait is visible, so removing it reveals the owner's latest state
    /// instead of restoring a stale snapshot.
    pub(crate) fn temporary_live_step(
        &self,
        label: impl Into<String>,
    ) -> Option<TemporaryLiveStep> {
        let shared = self.current()?;
        let mut guard = lock(&shared);
        if !guard.live {
            return None;
        }
        let label = label.into();
        let position = {
            let mut plan = guard.plan.clone();
            plan.advance_to(&label);
            plan.position()
        };
        let overlay_id = guard.install_overlay(label, position);
        Some(TemporaryLiveStep {
            shared: self.shared.clone(),
            overlay_id,
        })
    }

    pub(crate) fn hold(&self) -> Hold {
        hold_shared(self.current())
    }
}

/// Removes a spawned task's overlay to reveal the command owner's latest display state.
pub(crate) struct TemporaryLiveStep {
    shared: Weak<Mutex<Shared>>,
    overlay_id: u64,
}

impl TemporaryLiveStep {
    pub(crate) fn refresh_live_step(&mut self, label: impl Into<String>) -> bool {
        let Some(shared) = self.shared.upgrade() else {
            return false;
        };
        let refreshed = lock(&shared).refresh_overlay(self.overlay_id, label.into());
        refreshed
    }
}

impl Drop for TemporaryLiveStep {
    fn drop(&mut self) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        lock(&shared).remove_overlay(self.overlay_id);
    }
}

/// Say what is happening now. Silently does nothing when the running command has no display.
pub(crate) fn step(label: impl Into<String>) {
    if let Some(shared) = current() {
        set_step(&shared, label.into());
        lock(&shared).needs_you = false;
    }
}

/// Say what is happening now, and report whether anyone was there to see it.

/// For waits that are NOT one of the declared steps -- contending for a machine-wide lock, say.
/// `step` cannot be used blind for those: it is silent without a display, and the caller then has
/// to print the fact itself or lose it. Measured: `note deploy` queued behind another deploy's
/// prover lock kept showing "checking the network and the contracts" while it waited, so the live
/// line named a wait on the network that had already finished, and the real reason arrived as a
/// separate line printed over the top of it.
pub(crate) fn step_if_showing(label: impl Into<String>) -> bool {
    let Some(shared) = current() else {
        return false;
    };
    set_step(&shared, label.into());
    lock(&shared).needs_you = false;
    true
}

/// Advance a step only when stderr has a live status line.

/// Redirected output needs its own durable record instead of a renderer-owned line, while a live
/// terminal needs the checklist cursor and its elapsed clock to move to this step.
pub(crate) fn step_if_live(label: impl Into<String>) -> bool {
    let Some(shared) = current() else {
        return false;
    };
    if !lock(&shared).live {
        return false;
    }
    set_step(&shared, label.into());
    lock(&shared).needs_you = false;
    true
}

/// Refresh details inside the current live step without restarting its elapsed clock.
pub(crate) fn refresh_live_step(label: impl Into<String>) -> bool {
    refresh_live_step_on(current(), label.into())
}

fn refresh_live_step_on(shared: Option<Arc<Mutex<Shared>>>, label: String) -> bool {
    let Some(shared) = shared else {
        return false;
    };
    let mut guard = lock(&shared);
    if !guard.live {
        return false;
    }
    if guard.label != label {
        for passed in guard.plan.advance_to(&label) {
            guard.ticked(&passed);
        }
        guard.label = label;
        guard.render();
    }
    guard.needs_you = false;
    true
}

/// The same, for a wait that ends only when the OPERATOR does something.

/// It is drawn amber, the colour a refusal uses for "this one is yours". The two kinds of wait are
/// otherwise indistinguishable on screen -- and they are not the same thing at all: one ends by
/// itself, the other never does. Measured before the distinction existed: 147 seconds under
/// `preparing`, while the client was in fact waiting for a tap on a phone.

/// The label still has to read as an instruction, because colour is never the only signal: a piped
/// log, a screenshot and a terminal with `NO_COLOR` all show the same words.
pub(crate) fn step_needs_you(label: impl Into<String>) {
    if let Some(shared) = current() {
        set_step(&shared, label.into());
        lock(&shared).needs_you = true;
    }
}

/// Record something that has just been done, as a tick in the log above the live line.

/// `false` when no command is showing a display, which is the caller's cue to print whatever it
/// printed before this existed -- a tick that goes nowhere would lose the fact entirely.
pub(crate) fn tick(text: impl AsRef<str>) -> bool {
    let Some(shared) = current() else {
        return false;
    };
    let mut guard = lock(&shared);
    let text = format!("\u{2714} {}", text.as_ref());
    guard.ticked(&text);
    true
}

/// Every remaining step ticks and the live line goes, because what comes next is the command's
/// RESULT -- printed on stdout, which no display controls.

/// Without this the result lands on top of a live line that is still being redrawn, and the two
/// interleave: `recording the note in the pool 0snote deployed -> PrivateNote...`. Called by the
/// command itself, immediately before it prints, so nothing is between the two.
pub(crate) fn complete() {
    let Some(shared) = current() else { return };
    let mut guard = lock(&shared);
    for passed in guard.plan.finish() {
        guard.ticked(&passed);
    }
    guard.erase();
}

/// Stop redrawing the live line and take it off the screen until this is dropped.

/// For anything that OWNS the terminal for a while: an interview reading an answer, a menu drawing
/// its rows. Those write to `/dev/tty` and wait for a person; the ticker redraws stderr on its own
/// schedule and knows nothing about either, so its frames land on top of the prompt. Measured on a
/// real buy: seven policy questions were asked while `[1/4] checking the network and contracts`
/// kept overwriting them, and the operator typed every answer blind.

/// A guard rather than a pair of calls, because the wait it covers can end by `?` on a read error
/// or by the operator pressing Ctrl-C -- and a live line that is never restored leaves the rest of
/// the command silent.
pub(crate) fn hold() -> Hold {
    hold_shared(current())
}

fn hold_shared(shared: Option<Arc<Mutex<Shared>>>) -> Hold {
    let weak = shared.as_ref().map(Arc::downgrade);
    if let Some(shared) = shared {
        let mut guard = lock(&shared);
        guard.erase();
        guard.held = true;
    }
    Hold { shared: weak }
}

/// Restores the live line when it goes. See [`hold`].
pub(crate) struct Hold {
    shared: Option<Weak<Mutex<Shared>>>,
}

impl Drop for Hold {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.as_ref().and_then(Weak::upgrade) {
            lock(&shared).held = false;
        }
    }
}

/// Take the live line off the screen without touching the checklist.

/// For a result printed to ORDINARY output while the command carries on -- the buy's own block, say,
/// with the wait for a seller still ahead of it. Both streams share a terminal, and a block written
/// under a line that is still redrawing itself lands inside it.

/// [`complete`] is the other one and must not be used here: it also ticks every step the plan has
/// left, which claims work that has not happened. The next frame redraws the live line under
/// whatever was printed, which is where it belongs.
pub(crate) fn clear_live_line() {
    let Some(shared) = current() else { return };
    lock(&shared).erase();
}

fn set_step(shared: &Mutex<Shared>, label: String) {
    let mut guard = lock(shared);
    // Saying the same thing again is not a new step. The funding wait re-announces itself on every
    // poll -- it is a loop, and the announcement belongs inside it -- and restarting the clock
    // there would peg the seconds at one poll interval, hiding exactly the number that tells the
    // operator this has been waiting on them for two minutes.
    if guard.label == label {
        return;
    }
    // A step this move leaves behind is ticked into the log, once, where it stays. Nothing above
    // the live line is ever rewritten.
    // A step change drops whatever the last step was measuring.
    guard.measure = None;
    for passed in guard.plan.advance_to(&label) {
        guard.ticked(&passed);
    }
    guard.label = label;
    guard.started = Instant::now();
    if guard.live {
        guard.render();
    } else {
        let label = guard.label.clone();
        guard.line(&label);
    }
}

/// A status display for one command. Dropping it leaves the terminal clean.
pub(crate) struct Status {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    /// The display this one covered up, handed back when this one goes.
    displaced: Option<std::sync::Weak<Mutex<Shared>>>,
    ticker: Option<std::thread::JoinHandle<()>>,
}

impl Status {
    /// A display with no checklist: one live line and nothing above it.
    pub(crate) fn new(first: impl Into<String>) -> Self {
        Self::planned(first, Plan::default())
    }

    /// A display whose steps are declared up front, as `(what is happening, what happened)` pairs:
    /// the first is what the live line says while the step runs, the second what the tick says once
    /// it is behind. `first` is the opening line, and naming the first step's running form is the
    /// usual choice.
    pub(crate) fn with_plan<S: Into<String>>(
        first: impl Into<String>,
        steps: impl IntoIterator<Item = (S, S)>,
    ) -> Self {
        Self::planned(first, Plan::new(steps))
    }

    fn planned(first: impl Into<String>, plan: Plan) -> Self {
        let shared = Arc::new(Mutex::new(Shared::new(first.into(), plan)));
        // What was showing before this one, so dropping this display gives it back rather than
        // leaving the thread with none. A command that builds a second display inside the first --
        // `note deploy` around a funding wait, say -- used to silence the outer one for the rest of
        // the run: `progress::step`, `tick` and the `complete()` that takes the live line down all
        // read this one slot, and the inner `Drop` cleared it unconditionally.
        let displaced = CURRENT
            .try_with(|showing| showing.borrow_mut().replace(Arc::downgrade(&shared)))
            .unwrap_or(None);
        let stop = Arc::new(AtomicBool::new(false));
        let live = lock(&shared).live;
        if !live {
            let mut guard = lock(&shared);
            let label = guard.label.clone();
            guard.line(&label);
            drop(guard);
            return Self {
                shared,
                stop,
                displaced,
                ticker: None,
            };
        }
        let ticker = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    lock(&shared).render();
                    std::thread::sleep(TICK);
                }
            })
        };
        Self {
            shared,
            stop,
            displaced,
            ticker: Some(ticker),
        }
    }

    /// Replace what the live line says, ticking the checklist if the label is a declared step. The
    /// clock restarts: the number beside a step is that step's own duration, not the command's.
    pub(crate) fn step(&self, label: impl Into<String>) {
        set_step(&self.shared, label.into());
    }

    /// Everything the plan declared is behind: the last step ticks.

    /// For the success path only. Dropping the display deliberately does not do this, because it
    /// also happens when a command failed, and a checklist that ticked its final step on the way
    /// out would claim work the error printed under it says never happened.
    pub(crate) fn finish(&self) {
        let mut guard = lock(&self.shared);
        for passed in guard.plan.finish() {
            guard.ticked(&passed);
        }
    }

    /// Keep a caller-rendered line above the live status line without adding another glyph.
    pub(crate) fn keep_exact(&self, line: impl AsRef<str>) {
        lock(&self.shared).line(line.as_ref());
    }

    /// A weak handle for work that legitimately crosses the command thread boundary.
    pub(crate) fn handle(&self) -> ProgressHandle {
        ProgressHandle {
            shared: Arc::downgrade(&self.shared),
        }
    }

    /// The state this display shares with [`super::progress_capture`], which needs to write through
    /// the same descriptor and to replace the label from a reader thread.
    pub(super) fn shared(&self) -> Arc<Mutex<Shared>> {
        Arc::clone(&self.shared)
    }
}

impl Drop for Status {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(ticker) = self.ticker.take() {
            let _ = ticker.join();
        }
        // Only the live line goes. Every tick already printed stays in the log, which is what says
        // how far a failed command got -- and nothing is ticked here, because this runs on the
        // failure path too and a step marked done on the way out would claim work the error
        // underneath it says never happened.
        lock(&self.shared).erase();
        // Back to whatever was showing before, which is `None` for the outermost display and the
        // outer command's own for a nested one.
        let _ = CURRENT.try_with(|showing| *showing.borrow_mut() = self.displaced.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `CURRENT` is one slot for the one command a process runs. The suite runs its tests in
    /// threads of that same process, so two of these building a display at once would each be
    /// looking at the other's -- a race in the test harness, not in the client, and this is what
    /// keeps it out of the results.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    fn alone() -> std::sync::MutexGuard<'static, ()> {
        ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A display built inside another gives the outer one back when it goes.

    /// The slot is one per process, and everything that speaks to "the display" -- `step`, `tick`,
    /// `complete` -- reads it. An inner display that cleared the slot on the way out left the outer
    /// command mute for the rest of its run, including the `complete()` that takes its live line down
    /// before a result is printed.
    #[test]
    fn a_nested_display_hands_the_outer_one_back() {
        let _alone = alone();
        let outer = Status::new("outer");
        assert!(
            current().is_some_and(|showing| Arc::ptr_eq(&showing, &outer.shared)),
            "the outer display is the one showing"
        );
        {
            let inner = Status::new("inner");
            assert!(
                current().is_some_and(|showing| Arc::ptr_eq(&showing, &inner.shared)),
                "while it lives, the inner display is the one showing"
            );
        }
        assert!(
            current().is_some_and(|showing| Arc::ptr_eq(&showing, &outer.shared)),
            "the outer display is showing again"
        );
        drop(outer);
        assert!(
            current().is_none(),
            "the outermost display leaves nothing showing"
        );
    }

    /// Captured or redirected output must carry no escapes: a log file and a machine consumer read
    /// the same bytes, and a cursor movement in either is corruption.
    #[test]
    fn a_non_terminal_destination_renders_plain_lines() {
        let _alone = alone();
        // stderr under `cargo test` is captured, so this is the real non-terminal path.
        let status = Status::new("first");
        status.step("second");
        let guard = lock(&status.shared);
        assert!(!guard.live, "a captured stderr must not be treated as live");
        assert!(!guard.colour);
        assert_eq!(guard.label, "second");
    }

    /// While something else owns the terminal, the live line stops redrawing -- and starts again.

    /// Reported by the operator on a real buy: the policy interview asked seven questions on
    /// `/dev/tty` while the ticker kept redrawing `[1/4] checking the network and contracts` over
    /// them on stderr. The question scrolled away, the answer was typed blind, and the validation
    /// message ("a whole number, please") arrived between two spinner frames.

    /// The restore half is what makes this a guard rather than two calls: the wait it covers can
    /// end by an error or by Ctrl-C, and a live line never turned back on leaves the rest of a
    /// command mute.
    #[test]
    fn something_that_owns_the_terminal_stops_the_live_line_and_gives_it_back() {
        let _alone = alone();
        let status = Status::new("checking the network and contracts");
        assert!(!lock(&status.shared).held, "nothing is holding it yet");
        {
            let _hold = hold();
            assert!(
                lock(&status.shared).held,
                "the ticker is still free to draw over a prompt the operator is answering"
            );
        }
        assert!(
            !lock(&status.shared).held,
            "the live line never came back, so everything after the question is silent"
        );
    }

    /// A wait that is not a declared step still replaces the live line, and says so to its caller.

    /// The defect this pins was seen by the operator, not by a test: `note deploy` queued behind
    /// another deploy's machine-wide prover lock went on showing `[1/5] checking the network and
    /// the contracts` for as long as it waited, so the live line blamed the network for a wait on
    /// a lock -- while the real reason was printed once, as a separate line over the top of it.
    /// `step` alone cannot fix that, because it is silent when nothing is showing and the caller
    /// then loses the fact entirely; the return value is what lets the caller fall back to
    /// printing.
    #[test]
    fn a_wait_that_is_not_a_step_still_renames_the_live_line() {
        let _alone = alone();
        assert!(
            !step_if_showing("waiting for another note deploy"),
            "with nothing showing there is nobody to tell, and the caller has to print it itself"
        );

        let status = Status::with_plan(
            "checking the network and the contracts",
            [("checking the network and the contracts", "checked")],
        );
        assert!(
            step_if_showing("waiting for another note deploy on this machine to finish"),
            "a display is showing, so the live line is where this belongs"
        );
        let guard = lock(&status.shared);
        assert_eq!(
            guard.label, "waiting for another note deploy on this machine to finish",
            "the live line still names the step that had already finished"
        );
        assert!(
            !guard.needs_you,
            "this wait ends by itself; amber is reserved for the ones only the operator can end"
        );
    }

    /// The point of the whole module's locking discipline: a panic somewhere else must not turn
    /// into a second panic here. `note deploy` spends from the operator's wallet before it proves,
    /// and a spinner has no business aborting that.
    #[test]
    fn a_poisoned_lock_is_still_usable() {
        let _alone = alone();
        let status = Status::new("first");
        let shared = status.shared();
        let poisoner = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                let _guard = shared.lock().expect("held");
                panic!("poison the status line");
            })
        };
        assert!(
            poisoner.join().is_err(),
            "the helper thread must have panicked"
        );
        assert!(
            shared.is_poisoned(),
            "the lock must be poisoned for this to test anything"
        );

        // Every production entry point, on a poisoned lock.
        status.step("second");
        status.keep_exact("kept");
        step("third");
        assert_eq!(lock(&shared).label, "third");
        drop(status);
    }

    /// A step's number is its own age, so a caller can read where the time went.
    #[test]
    fn each_step_restarts_the_clock() {
        let _alone = alone();
        let status = Status::new("first");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let before = lock(&status.shared).started;
        status.step("second");
        assert!(lock(&status.shared).started > before);
    }

    #[test]
    fn issue_1446_refreshing_a_live_step_keeps_its_elapsed_clock() {
        let _alone = alone();
        let status = Status::new("waiting for a seller: funded=true opened=false reclaim_in=600s");
        let mut guard = lock(&status.shared);
        guard.live = true;
        let before = guard.started;
        drop(guard);

        assert!(refresh_live_step(
            "waiting for a seller: funded=true opened=false reclaim_in=570s"
        ));
        let guard = lock(&status.shared);
        assert_eq!(
            guard.label,
            "waiting for a seller: funded=true opened=false reclaim_in=570s"
        );
        assert_eq!(guard.started, before);
    }

    #[test]
    fn issue_1446_plain_destination_does_not_emit_live_step_refreshes() {
        let _alone = alone();
        let status = Status::new("checking the network and contracts");

        assert!(!step_if_live(
            "waiting for a seller: funded=true opened=false reclaim_in=600s"
        ));
        assert!(!refresh_live_step(
            "waiting for a seller: funded=true opened=false reclaim_in=570s"
        ));
        assert_eq!(
            lock(&status.shared).label,
            "checking the network and contracts",
            "redirected output is owned by the durable initial record and compact heartbeat"
        );
    }

    #[test]
    fn issue_1446_explicit_handle_updates_only_its_live_display_across_threads() {
        let _alone = alone();
        let status = Status::new("bringing the local endpoint up");
        lock(&status.shared).live = true;
        let handle = status.handle();

        let (handle, mut temporary) = std::thread::spawn(move || {
            assert!(
                !step_if_live("must not find the command through another thread's CURRENT"),
                "the spawned task must not inherit a thread-local display"
            );
            let temporary = handle
                .temporary_live_step(
                    "waiting for a seller: funded=true opened=false reclaim_in=600s",
                )
                .expect("the explicit handle reaches its live display");
            (handle, temporary)
        })
        .join()
        .expect("cross-thread progress update joins");

        assert_eq!(
            lock(&status.shared).displayed_label(),
            "waiting for a seller: funded=true opened=false reclaim_in=600s"
        );
        let wait_started = lock(&status.shared).displayed_started();
        assert!(temporary
            .refresh_live_step("waiting for a seller: funded=true opened=false reclaim_in=570s"));
        let guard = lock(&status.shared);
        assert_eq!(
            guard.displayed_label(),
            "waiting for a seller: funded=true opened=false reclaim_in=570s"
        );
        assert_eq!(
            guard.displayed_started(),
            wait_started,
            "refreshing the overlay must preserve the wait's elapsed clock"
        );
        drop(guard);
        drop(temporary);
        assert_eq!(lock(&status.shared).label, "bringing the local endpoint up");
        drop(status);
        assert!(
            !handle.refresh_live_step(
                "waiting for a seller: funded=true opened=false reclaim_in=570s"
            ),
            "the weak handle must not keep a completed command's display alive"
        );
    }

    /// The free entry point exists for code that never sees the display; it must reach the same
    /// state the method does, and must tick the checklist the same way.
    #[test]
    fn the_free_step_reaches_the_running_display_and_ticks_its_plan() {
        let _alone = alone();
        let status = Status::with_plan(
            "checking",
            [
                ("checking", "checked"),
                ("funding Hot", "Hot funded"),
                ("proving", "proved"),
            ],
        );
        step("funding Hot: confirm the transfer in the wallet");
        let guard = lock(&status.shared);
        assert_eq!(
            guard.label,
            "funding Hot: confirm the transfer in the wallet"
        );
        drop(guard);
        drop(status);
        // Dropped: nothing left registered, and a later step is a no-op rather than a panic.
        step("nobody is listening");
        assert!(current().is_none());
    }
}
