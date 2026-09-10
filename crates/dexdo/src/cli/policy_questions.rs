//! The rules of engagement, asked as situations instead of as fields.

//! `policy.json` is a set of answers given in advance to one question: what should the client do
//! when the other side vanishes or misbehaves. It is required because every one of those cases
//! happens while money is already escrowed in a deal, and the client has to act rather than wake
//! the operator.

//! It is filled in by hand today, along paths like `seller.on.buyer_no_show`, whose values are
//! `cleanup_and_republish` and `retire_gateway`. An operator reading that is being asked to know
//! the client's vocabulary before they can sell anything.

//! So the same file is filled from questions: a situation the operator can picture, answers that
//! are actions rather than enum variants, and one of them marked as the suggestion. The paths and
//! the values are unchanged -- this module only decides how they are asked for, and the answers it
//! produces are exactly the strings [`super::policy`] already validates.

/// One answer an operator can choose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Answer {
    /// What the operator reads.
    pub(crate) says: &'static str,
    /// What goes into the file. Must be a value `super::policy` accepts for this path.
    pub(crate) value: &'static str,
}

/// One question: a situation, and the answers to it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Question {
    /// The field this answers, e.g. `seller.on.buyer_no_show`.
    pub(crate) path: &'static str,
    /// The situation, in the words of someone who has not read the source.
    pub(crate) situation: &'static str,
    /// Why it is being asked at all -- one line, shown under the situation.
    pub(crate) because: &'static str,
    pub(crate) answers: &'static [Answer],
    /// Index into `answers` of the suggestion. Whatever an operator picks is theirs; the suggestion
    /// is what a first-time seller or buyer would be served by.
    pub(crate) suggested: usize,
}

impl Question {
    pub(crate) fn suggestion(&self) -> Answer {
        self.answers[self.suggested]
    }

    /// The answers worth offering, paired with whether each is the suggestion.

    /// `executable` narrows them to what the running code can actually carry out, where that is
    /// narrower than what the file accepts. An answer the runtime refuses is a valid value and a
    /// broken choice: the file loads and the command then will not start.

    /// The suggestion moves with the narrowing: if it was cut, the first survivor takes its place,
    /// because "suggested" means "what a first-time operator should take", not an index.
    pub(crate) fn offering(&self, executable: Option<&[&str]>) -> Vec<(Answer, bool)> {
        let kept: Vec<Answer> = self
            .answers
            .iter()
            .copied()
            .filter(|answer| executable.is_none_or(|values| values.contains(&answer.value)))
            .collect();
        let suggested = kept
            .iter()
            .position(|answer| answer.value == self.suggestion().value)
            .unwrap_or(0);
        kept.into_iter()
            .enumerate()
            .map(|(index, answer)| (answer, index == suggested))
            .collect()
    }
}

/// A number the operator gives instead of choosing a row.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Count {
    pub(crate) path: &'static str,
    pub(crate) situation: &'static str,
    pub(crate) because: &'static str,
    /// What is being counted, written at the prompt. Without it "how many [3]:" under a buy that
    /// already named its ticks reads as a second demand for the same number.
    pub(crate) unit: &'static str,
    pub(crate) suggested: u64,
    /// Below this the answer is refused rather than accepted and quietly clamped.
    pub(crate) least: u64,
}

/// What a seller is asked, in the order it is asked.
pub(crate) const SELLER_QUESTIONS: &[Question] = &[
    Question {
        path: "seller.on.after_deal_done",
        situation: "A deal has run to the end and the buyer has what they paid for.",
        because: "The offer is gone once it is filled; this is whether a new one goes up.",
        answers: &[
            Answer {
                says: "offer again straight away",
                value: "republish",
            },
            Answer {
                says: "offer again, but wait a little longer each time",
                value: "republish_with_backoff",
            },
            Answer {
                says: "stop offering",
                value: "retire",
            },
        ],
        suggested: 0,
    },
    Question {
        path: "seller.on.buyer_no_show",
        situation: "A buyer paid, matched you, and then never connected.",
        because: "Their money is escrowed and your gateway is holding a slot for them.",
        answers: &[
            Answer {
                says: "tidy the deal up and offer again",
                value: "cleanup_and_republish",
            },
            Answer {
                says: "tidy the deal up and stop offering",
                value: "cleanup_and_retire",
            },
            Answer {
                says: "shut the gateway down and leave the deal alone",
                value: "retire_gateway",
            },
        ],
        suggested: 0,
    },
    Question {
        path: "seller.on.dispute_against_me",
        situation: "A buyer disputes what you delivered.",
        because: "Until this is answered, the money for that deal sits frozen.",
        answers: &[
            Answer {
                says: "release the money if your own records show the work was delivered",
                value: "release_if_clean",
            },
            Answer {
                says: "hold everything until a human looks at it",
                value: "hold",
            },
        ],
        suggested: 0,
    },
];

/// The numbers a seller is asked for.
pub(crate) const SELLER_COUNTS: &[Count] = &[Count {
    path: "seller.max_open_deals",
    situation: "How many deals should run at once?",
    because: "Each one holds a slot on your gateway and its own escrow.",
    unit: "deals at once",
    suggested: 1,
    least: 1,
}];

/// What a buyer is asked, in the order it is asked.
pub(crate) const BUYER_QUESTIONS: &[Question] = &[
    Question {
        path: "buyer.on.no_handover_after_match",
        situation: "A seller matched your order and never sent the connection details.",
        because: "You have paid; nothing is streaming.",
        answers: &[
            Answer {
                says: "wait a while, then take your money back",
                value: "wait_then_reclaim",
            },
            Answer {
                says: "go to the next seller",
                value: "next_seller",
            },
            Answer {
                says: "stop and let me look at it",
                value: "fail_closed",
            },
        ],
        suggested: 0,
    },
    Question {
        path: "buyer.on.malformed_handover",
        situation: "The seller sent connection details that cannot be read.",
        because: "There is nothing to connect to, and your money is already committed.",
        answers: &[
            Answer {
                says: "take your money back",
                value: "reclaim",
            },
            Answer {
                says: "dispute it",
                value: "dispute",
            },
            Answer {
                says: "stop and let me look at it",
                value: "fail_closed",
            },
        ],
        suggested: 0,
    },
    Question {
        path: "buyer.on.dead_gateway",
        situation: "The seller's gateway does not answer.",
        because: "It may be a blip, or the seller may be gone.",
        answers: &[
            Answer {
                says: "try again for a while, then take your money back",
                value: "retry_then_reclaim",
            },
            Answer {
                says: "go to the next seller",
                value: "next_seller",
            },
            Answer {
                says: "stop and let me look at it",
                value: "fail_closed",
            },
        ],
        suggested: 0,
    },
    Question {
        path: "buyer.on.empty_stream",
        situation: "The connection works, but nothing comes out of it.",
        because: "You are paying per tick for output that is not arriving.",
        answers: &[
            Answer {
                says: "take your money back",
                value: "reclaim",
            },
            Answer {
                says: "go to the next seller",
                value: "next_seller",
            },
            Answer {
                says: "stop and let me look at it",
                value: "fail_closed",
            },
        ],
        suggested: 0,
    },
    Question {
        path: "buyer.on.seller_stalls_mid_stream",
        situation: "Output was arriving and then stopped, part way through.",
        because: "You have had some of what you paid for, and not the rest.",
        answers: &[
            Answer {
                says: "keep what arrived and take back the money for the rest",
                value: "accept_delivered_then_reclaim",
            },
            Answer {
                says: "dispute it",
                value: "dispute",
            },
        ],
        suggested: 0,
    },
    Question {
        path: "buyer.on.bad_output_scam",
        situation: "Output arrives, but it is not what the model should have produced.",
        because: "This is the one case the client cannot judge for you.",
        answers: &[
            Answer {
                says: "dispute it",
                value: "dispute",
            },
            Answer {
                says: "stop and let me look at it",
                value: "stop",
            },
            // `stop_and_blacklist` is a valid value of this field and is deliberately NOT offered:
            // the consumer surface has no seller identity or blacklist store, refuses to act on it,
            // and refuses to quietly degrade to `stop` -- so an operator who chose it here would be
            // promised something no code does. It stays settable by hand for a surface that grows
            // one.
        ],
        suggested: 0,
    },
];

/// The numbers a buyer is asked for.
pub(crate) const BUYER_COUNTS: &[Count] = &[
    Count {
        path: "buyer.failover.max_sellers_to_try",
        situation: "If a seller lets you down, how many others should be tried?",
        because: "Each attempt is a fresh order, and each one costs.",
        unit: "sellers to try",
        suggested: 3,
        least: 1,
    },
    Count {
        path: "buyer.failover.total_spend_cap_shells",
        situation: "Across all those attempts, how much may be spent in total, in SHELL?",
        because: "This is the ceiling on one purchase going wrong repeatedly.",
        unit: "SHELL in total",
        suggested: 20,
        least: 1,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of this module: every question the operator answers has to produce a value
    /// the existing validator accepts, and cover every field it requires. A question that asked
    /// something the file does not have, or a field with no question, would leave the operator with
    /// a policy that still fails to load.
    #[test]
    fn the_questions_cover_exactly_the_fields_the_policy_requires() {
        for (role, paths) in [
            (
                crate::cli::policy::RuntimeRole::Seller,
                SELLER_QUESTIONS
                    .iter()
                    .map(|question| question.path)
                    .chain(SELLER_COUNTS.iter().map(|count| count.path))
                    .collect::<Vec<_>>(),
            ),
            (
                crate::cli::policy::RuntimeRole::Buyer,
                BUYER_QUESTIONS
                    .iter()
                    .map(|question| question.path)
                    .chain(BUYER_COUNTS.iter().map(|count| count.path))
                    .collect::<Vec<_>>(),
            ),
        ] {
            let required = crate::cli::policy::required_paths(role);
            let mut asked = paths.clone();
            asked.sort_unstable();
            let mut required = required.clone();
            required.sort_unstable();
            assert_eq!(
                asked, required,
                "{role:?}: asked fields differ from required"
            );
        }
    }

    /// Every answer must be a value the validator accepts for that field, or the file the operator
    /// just filled in refuses to load.
    #[test]
    fn every_answer_is_a_value_the_validator_accepts() {
        for question in SELLER_QUESTIONS.iter().chain(BUYER_QUESTIONS) {
            for answer in question.answers {
                assert!(
                    crate::cli::policy::accepts(question.path, answer.value),
                    "{}: {} is not accepted",
                    question.path,
                    answer.value
                );
            }
        }
    }

    /// An answer the runtime cannot carry out must not be offered: the file would load and the
    /// command would then refuse to start, which is the walk into a refusal the interview exists to
    /// prevent. Pinned against the client's own list, so a runtime that grows the ability shows it
    /// here without this being edited.
    #[test]
    fn only_answers_the_runtime_can_carry_out_are_offered() {
        for question in SELLER_QUESTIONS.iter().chain(BUYER_QUESTIONS) {
            let executable = crate::cli::policy::runtime_supported(question.path);
            let offered = question.offering(executable);
            assert!(
                !offered.is_empty(),
                "{}: nothing left to offer",
                question.path
            );
            assert_eq!(
                offered.iter().filter(|(_, suggested)| *suggested).count(),
                1,
                "{}: exactly one suggestion survives the narrowing",
                question.path
            );
            if let Some(values) = executable {
                for (answer, _) in &offered {
                    assert!(
                        values.contains(&answer.value),
                        "{}: {}",
                        question.path,
                        answer.value
                    );
                }
            }
        }
    }

    /// A number prompt has to name what it counts. An operator who has just typed `--ticks 2`
    /// reads a bare "how many [3]:" as being asked for the ticks again.
    #[test]
    fn every_number_names_what_it_counts() {
        for count in SELLER_COUNTS.iter().chain(BUYER_COUNTS) {
            assert!(!count.unit.is_empty(), "{}", count.path);
            assert!(!count.unit.contains('_'), "{}: {}", count.path, count.unit);
        }
    }

    /// A suggestion exists for every question, and is one of its own answers.
    #[test]
    fn each_question_suggests_one_of_its_own_answers() {
        for question in SELLER_QUESTIONS.iter().chain(BUYER_QUESTIONS) {
            let offered = question.offering(None);
            let suggested: Vec<Answer> = offered
                .into_iter()
                .filter_map(|(answer, suggested)| suggested.then_some(answer))
                .collect();
            assert_eq!(suggested, vec![question.suggestion()], "{}", question.path);
        }
    }

    /// The situations are what the operator reads instead of the field names, so they must not be
    /// the field names: no dotted paths, no enum values, and a full sentence each.
    #[test]
    fn the_situations_are_sentences_and_not_field_names() {
        for question in SELLER_QUESTIONS.iter().chain(BUYER_QUESTIONS) {
            let text = question.situation;
            assert!(text.ends_with('.') || text.ends_with('?'), "{text}");
            assert!(
                !text.contains('.'.to_string().as_str()) || !text.contains("on."),
                "{text}"
            );
            assert!(!text.contains('_'), "{text}");
            for answer in question.answers {
                assert!(!answer.says.contains('_'), "{}", answer.says);
            }
        }
    }
}
