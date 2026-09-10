//! The checklist a long command shows above its status line.

//! A command that runs for minutes has to answer two questions at a glance: what is happening now,
//! and what is this run going to do. The status line answers the first. This answers the second --
//! the steps declared up front, ticked as they are passed, so the operator can see both what is
//! already behind them and what is still ahead.

//! It matters most where the client is not working at all but WAITING on the operator: `note
//! deploy` stops until a Vault -> Hot transfer is confirmed inside the phone wallet, and a spinner
//! that says `preparing` while nothing happens for two minutes reads as a hung command rather than
//! as a request. Measured on a a live chain deploy: 147 seconds under one unchanging label.

//! Pure. The escape sequences that put this on a terminal live in `progress_draw`; here a plan
//! advances a cursor and returns only the completed lines that the caller leaves in the log.

const DONE_MARK: &str = "\u{2714}";

/// One declared step, in the two tenses it is read in.

/// A line that is still running says what is happening; a line left behind says what happened. The
/// same words in both places make a finished run read as though it were still going -- five lines
/// of "funding the wallet" with ticks in front of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Step {
    /// While it runs: "funding the wallet and proving the note".
    doing: String,
    /// Once it is behind: "the wallet was funded and the note proved".
    done: String,
}

/// The declared steps of one command, and which one it is on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Plan {
    steps: Vec<Step>,
    /// Index of the current step. Equal to `steps.len()` once everything is behind.
    at: usize,
}

impl Plan {
    /// From `(what is happening, what happened)` pairs, in the order they run.
    pub(super) fn new<S: Into<String>>(steps: impl IntoIterator<Item = (S, S)>) -> Plan {
        Plan {
            steps: steps
                .into_iter()
                .map(|(doing, done)| Step {
                    doing: doing.into(),
                    done: done.into(),
                })
                .collect(),
            at: 0,
        }
    }

    /// Move to the step called `label`, if the plan declares one, and report the steps that move
    /// left behind -- the lines the caller then ticks off into the log.

    /// Matching is by prefix, not equality: a step may refine its own label as it goes ("funding
    /// Hot" -> "funding Hot: waiting for your confirmation") without dropping off the checklist.

    /// Only forwards. A command that revisits a stage -- a resumed deploy re-reading the chain --
    /// has not undone the steps it already passed, and a checklist that un-ticked itself would say
    /// it had.
    pub(super) fn advance_to(&mut self, label: &str) -> Vec<String> {
        let Some(index) = self
            .steps
            .iter()
            .position(|step| label.starts_with(step.doing.as_str()))
        else {
            return Vec::new();
        };
        self.advance_by(index)
    }

    /// Everything is behind: the remaining steps tick, in order.
    pub(super) fn finish(&mut self) -> Vec<String> {
        self.advance_by(self.steps.len())
    }

    fn advance_by(&mut self, index: usize) -> Vec<String> {
        if index <= self.at {
            return Vec::new();
        }
        let total = self.steps.len();
        let passed = (self.at..index)
            .map(|step| {
                format!(
                    "{} [{}/{total}] {}",
                    DONE_MARK,
                    step + 1,
                    self.steps[step].done
                )
            })
            .collect();
        self.at = index;
        passed
    }

    /// Where the run stands: which step is current, and how many there are.

    /// This is what replaced printing the whole checklist up front. That block could never fill in
    /// -- nothing above the cursor may be redrawn, or the display repeats itself down a scrolling
    /// screen -- so it sat there unticked while the ticks accumulated below it, which reads as a
    /// command that has done nothing. A counter says the same thing and cannot go stale.

    /// `None` once every step is behind, and for a command that declared none.
    pub(super) fn position(&self) -> Option<(usize, usize)> {
        (self.at < self.steps.len()).then(|| (self.at + 1, self.steps.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Plan {
        Plan::new([
            ("checking the network", "network checked"),
            ("funding Hot", "Hot funded"),
            ("proving the note", "note proved"),
        ])
    }

    #[test]
    fn advancing_ticks_everything_behind_the_new_step() {
        let mut plan = plan();
        assert_eq!(
            plan.advance_to("proving the note"),
            vec![
                "\u{2714} [1/3] network checked",
                "\u{2714} [2/3] Hot funded"
            ],
            "the steps a move leaves behind are what gets ticked into the log"
        );
        assert_eq!(plan.position(), Some((3, 3)));
    }

    /// The case this exists for: the client is not working, it is waiting on the operator, and the
    /// line has to say so without the step falling off the checklist.
    #[test]
    fn a_refined_label_still_matches_its_declared_step() {
        let mut plan = plan();
        assert_eq!(
            plan.advance_to("funding Hot: confirm the transfer in the wallet"),
            vec!["\u{2714} [1/3] network checked"]
        );
        assert_eq!(plan.position(), Some((2, 3)));
        assert_eq!(
            plan.finish(),
            vec!["\u{2714} [2/3] Hot funded", "\u{2714} [3/3] note proved"],
            "the declared past-tense labels are what completed lines keep"
        );
    }

    /// A label the plan never declared -- a prover phase, say -- must not move the checklist, and
    /// must be reported as unmatched so the caller keeps showing it on the status line instead.
    #[test]
    fn an_undeclared_label_leaves_the_checklist_alone() {
        let mut plan = plan();
        assert!(plan.advance_to("proving: generate_proof (warm)").is_empty());
        assert_eq!(plan.position(), Some((1, 3)));
    }

    /// A line left behind reports what happened; a line still running says what is happening. The
    /// same words in both places make a finished run read as though it were still going.
    #[test]
    fn a_passed_step_is_reported_in_the_past_tense() {
        let mut plan = plan();
        assert_eq!(
            plan.advance_to("funding Hot"),
            vec!["\u{2714} [1/3] network checked"]
        );
        assert_eq!(plan.position(), Some((2, 3)));
    }

    /// A resumed run re-reads the chain after it has proved. Un-ticking would claim work was
    /// undone that was not.
    #[test]
    fn the_checklist_never_goes_backwards() {
        let mut plan = plan();
        plan.advance_to("proving the note");
        assert!(plan.advance_to("checking the network").is_empty());
        assert_eq!(plan.position(), Some((3, 3)));
    }

    /// The counter is what replaced the block that could never fill in: it has to name the step the
    /// run is ON, and stop naming one once everything is behind.
    #[test]
    fn the_position_names_the_current_step_and_then_nothing() {
        let mut plan = plan();
        assert_eq!(plan.position(), Some((1, 3)));
        plan.advance_to("funding Hot");
        assert_eq!(plan.position(), Some((2, 3)));
        plan.finish();
        assert_eq!(plan.position(), None);
        assert_eq!(Plan::default().position(), None);
    }
}
