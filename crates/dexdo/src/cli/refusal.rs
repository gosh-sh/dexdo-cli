//! What a refusal says to the operator, above what it says to a machine.

//! A refusal has two readers and they want opposite things. The operator wants one sentence -- what
//! did not happen -- and one instruction: what to do about it. Whoever reconstructs the run
//! afterwards wants the class, the addresses, the raw amounts and the source chain.

//! Today's money-command refusals are written for the second reader and shown to the first. Measured
//! cost, on the run that produced: a buy on an empty book named the preflight, its failure
//! class twice, the whole order-book address, the internal matcher, the book's counters and advice
//! about cleaning other people's rows -- and never the one thing that settles it, that
//! `--wait-for-seller` queues the buy until a seller appears and that without it a refusal on an
//! empty book is correct behaviour. Half an evening went into reading that as a client defect.

//! So a refusal is composed here: the operator's two lines first, the machine's line after them,
//! unchanged. Nothing about behaviour, exit codes or the machine surface moves -- only what is put
//! in front of a person.



/// The two lines an operator reads, and the detail that follows them.

/// `did_not_happen` is one sentence in the past tense; `do_next` is an instruction naming a flag or
/// a command. The pair is required, not optional: a refusal an operator cannot act on is a log line
/// that reached the wrong stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    did_not_happen: String,
    do_next: String,
    detail: String,
    kind: Kind,
    /// The ways out, where there is more than one and each is a command rather than a sentence.

    /// `spec.md` draws these as a `try:` block: what the choice is on the left, the command that
    /// makes it on the right. A refusal that offers three options inside one paragraph makes the
    /// operator parse prose to find a flag; three rows make it a choice.
    alternatives: Vec<(String, String)>,
}

/// Whose problem this is, which is the only thing colour is allowed to say.

/// The market said no, and the client broke, are different news. An operator who sees the same red
/// for both learns to read every refusal as a defect and stops reading the sentence -- which is how
/// "no seller is offering this model right now", correct behaviour, cost half an evening of
/// debugging. Amber is "your situation"; red is "ours".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// The client worked and the answer is no: an empty book, a ceiling under every ask, a wait
    /// that ran out. Nothing to fix in the client.
    Business,
    /// The client failed: a transport that would not talk, a state it could not read, a case it
    /// cannot classify. This is the one that deserves alarm.
    Breakage,
}

impl Refusal {
    /// The market's answer, or the operator's own arguments: amber.
    pub(crate) fn new(
        did_not_happen: impl Into<String>,
        do_next: impl Into<String>,
        detail: impl Into<String>,
    ) -> Refusal {
        Refusal {
            did_not_happen: did_not_happen.into(),
            do_next: do_next.into(),
            alternatives: Vec::new(),
            detail: detail.into(),
            kind: Kind::Business,
        }
    }

    /// The client's own failure: red.
    pub(crate) fn breakage(
        did_not_happen: impl Into<String>,
        do_next: impl Into<String>,
        detail: impl Into<String>,
    ) -> Refusal {
        Refusal {
            kind: Kind::Breakage,
            ..Refusal::new(did_not_happen, do_next, detail)
        }
    }

    #[cfg(test)]
    pub(crate) fn kind(&self) -> Kind {
        self.kind
    }

    /// What belongs in a record rather than on the screen: the machine line and everything under it.
    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }

    /// The instruction alone, for a test that asks whether this refusal can be acted on.
    #[cfg(test)]
    pub(crate) fn do_next(&self) -> &str {
        &self.do_next
    }

    /// The two lines an operator reads, and nothing else.

    /// The mark matches the ticks a command's steps leave behind, so a run that ends badly reads as

    /// that is not a step, the live line or the result is a record, and a record goes to the log --
    /// "not printed shorter, not printed". Whoever takes the run apart afterwards reads it with
    /// `RUST_LOG=info`, and a machine reads it from the machine surface, which nothing here moves.

    /// Colour says whose problem it is and nothing else: amber for the market's answer, red for the
    /// client's own failure. Where colour is off the sentence carries the whole meaning by itself.
    /// The refusal as the error a command returns: the operator's two lines on top, the record
    /// underneath, and nothing lost from either.
    pub(crate) fn into_error(self) -> anyhow::Error {
        let shown = self.render();
        // Recorded HERE, not in a helper a caller may forget: `main` prints the two lines and exits,
        // so nothing downstream ever unwraps this chain on the human path. Without this line the
        // detail reached neither the screen nor the log, and no `RUST_LOG` could bring it back --
        // measured on the funding timeout, a refusal on the money path whose detail carries the
        // whole address, the raw figures and what was left untouched.
        tracing::info!("{}", self.detail);
        anyhow::Error::new(OperatorRefusal {
            shown,
            detail: anyhow::Error::msg(self.detail),
        })
    }

    /// The ways out, as `(what this choice is, the command that makes it)`.

    /// They replace nothing: `do_next` still says what to do in words, and these say it in commands
    /// for the cases where there is a real choice between them.
    pub(crate) fn with_alternatives(
        mut self,
        alternatives: impl IntoIterator<Item = (String, String)>,
    ) -> Refusal {
        self.alternatives = alternatives.into_iter().collect();
        self
    }

    /// The two lines as the operator sees them, in whatever colour their terminal gets.
    pub(crate) fn render(&self) -> String {
        self.render_with(crate::cli::style::Palette::stderr())
    }

    /// The same render against a named palette, so a test can assert on CONTENT.

    /// `render` used to read `Palette::stderr()` itself, which made every assertion about what a
    /// refusal says depend on whether the run had a terminal: `contains("\u{26a0} ")` held under a
    /// pipe and failed on a developer's screen, because colour puts a reset between the glyph and
    /// its space. Two tests passed in CI and failed the moment anyone ran them by hand. The palette
    /// is an input, not an ambient fact, and tests pass `Palette::None`.
    pub(crate) fn render_with(&self, palette: crate::cli::style::Palette) -> String {
        use crate::cli::style::{self, Role};

        // `spec.md`: a refusal opens with its own glyph in column 0, the sentence follows from
        // column 2, and what to do about it stands under it. Colour still says whose problem it is
        // and nothing else -- the market's answer is a wait, the client's own failure is an error --
        // and where colour is off the two lines say the same thing by themselves.
        let (glyph, role) = match self.kind {
            Kind::Business => (style::WARN, Role::Wait),
            Kind::Breakage => (style::ERR, Role::Err),
        };
        let head = style::glyph_line(
            palette,
            glyph,
            role,
            &style::paint(palette, Role::Bold, &self.did_not_happen),
        );
        // The instruction is prose and prose is long: folded by the terminal it lands under the
        // glyph and the block stops being a block. Wrapped here, it keeps its column.

        // It is printed whether or not there are alternatives, because `with_alternatives` says of
        // itself that "they replace nothing". It used to be returned early and therefore dropped
        // the moment a refusal offered choices -- so a buy whose price ceiling sat under every ask
        // showed two commands to run and never the one sentence naming the remedy, which was to
        // raise that ceiling.
        let mut out = format!(
            "{head}\n{}",
            style::field_wrapped(palette, "", &self.do_next, Role::Text)
        );
        if self.alternatives.is_empty() {
            return out;
        }
        // The `try:` block of `spec.md`: the first row carries the label, the rest line up under it,
        // and every command is in the colour that means "yours to act on".
        let widest = self
            .alternatives
            .iter()
            .map(|(choice, _)| choice.chars().count())
            .max()
            .unwrap_or(0);
        for (index, (choice, command)) in self.alternatives.iter().enumerate() {
            let label = if index == 0 { "try" } else { "" };
            out.push('\n');
            out.push_str(&style::field(
                palette,
                label,
                &format!(
                    "{:widest$}  {}",
                    choice,
                    style::paint(palette, Role::Wait, command),
                    widest = widest
                ),
                Role::Text,
            ));
        }
        out
    }
}

/// The two lines, carried as an error so the top level can tell them from a stack of causes.

/// The detail stays in the chain underneath -- `{error:#}` still reads it, the machine surface still
/// carries it, a reconstruction still finds it -- and `main` prints ONLY this. That split is the
/// whole point: the operator gets the sentence, the record keeps everything.

/// # Why this is the error and not a context layer on one

/// It used to be `anyhow::Error::msg(detail).context(OperatorRefusal(shown))`, and a context layer
/// is not a node of the error chain: `anyhow` stores it inside its own `ContextError<C, E>`, which
/// only `anyhow::Error::downcast_ref` knows how to look into -- and only for as long as every layer
/// above it is also an `anyhow` layer. The funding wait wraps its refusal once more, in a wrapper
/// that holds the inner error as a struct FIELD, and the downcast in `main` stopped dead at that
/// wrapper: the branch that prints these two lines was unreachable from the money path they were
/// written for, and the operator got the guess `for_operator` makes from the flattened text --
/// "the client could not reach the chain" -- for a wait that ended on their own clock.

/// As an error in its own right the refusal is an ordinary node of the source chain, and every
/// wrapper that keeps its `source()` passes it on: an `anyhow` context, a struct like
/// `FundingContext`, a `thiserror` enum. That is the difference between fixing the one wrapper that
/// bit us and fixing the shape that let it.
#[derive(Debug)]
pub(crate) struct OperatorRefusal {
    shown: String,
    /// The record, kept as this error's own source so `{error:#}` reads exactly what it read before.
    detail: anyhow::Error,
}

impl std::fmt::Display for OperatorRefusal {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(&self.shown)
    }
}

impl std::error::Error for OperatorRefusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.detail.as_ref())
    }
}

/// The operator's two lines, wherever in the chain a wrapper left them.

/// `anyhow::Error::downcast_ref` on its own is not enough, and that is the whole defect: it walks
/// `anyhow`'s own context layers and stops at the first wrapper that carries its inner error as a
/// field. The ordinary source chain crosses that boundary, so this is what `main` asks.
pub(crate) fn shown_to_operator(error: &anyhow::Error) -> Option<&OperatorRefusal> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<OperatorRefusal>())
}

/// What to show an operator for a refusal nobody wrote two lines for yet.

/// Every command in this client can fail in ways this module has not been through one at a time, and
/// an operator meeting one of those got the raw sentence a developer wrote for themselves: file
/// paths, lock names, `BOC`, "by-fact reconciliation", the whole address. This is the sweep: the
/// situations an operator actually reaches on the buy / deploy / bind paths, each with the action
/// that settles it, and everything else as a plain red line rather than a transcript.

/// Matching is on the whole chain (`{error:#}`), because the sentence that identifies a situation is
/// often the cause rather than the top. Per-site refusals are better and take precedence -- this is
/// the floor, not the ceiling.
pub(crate) fn for_operator(error: &anyhow::Error) -> Option<Refusal> {
    let text = format!("{error:#}");
    let lower = text.to_ascii_lowercase();
    let detail = text.clone();

    // A buy this note already sent and nobody has confirmed yet. The remedy is a command, and the
    // operator has no way to guess it from a lock file name.
    if lower.contains("money submission awaiting by-fact reconciliation")
        || lower.contains("money lock is already acquired")
    {
        return Some(Refusal::new(
            "This note already has a buy waiting to be confirmed on chain, and nothing new was sent.",
            // The command is NAMED, not handed over as a line to copy, and the position of the
            // note is said in words. Printed with the note in it, the line either carries a
            // placeholder -- which reads as runnable and is not, and which the source guard in
            // `main.rs` rejects because `<this note>` splits on its space and eats the subcommand
            // -- or it carries a real address this code does not have. And printed as a bare
            // `dexdo orders journal`, as it shipped, it leaves the operator to guess where the
            // note goes; they put it after the subcommand, where `--note-addr` belongs to the
            // `orders` group and not to `journal`, and meet a second refusal that names the flag
            // rather than its position.
            "Run `dexdo orders journal` for this note -- `--note-addr` belongs to `orders`, so it \
             goes before the subcommand. It shows that submission and closes it as soon as the \
             chain proves how it ended. Then run this command again.",
            detail,
        ));
    }
    // The rules file is deliberately NOT here. Its refusal already has the shape 679 asks for --
    // what did not happen, then what to do -- and between those it names every field that is
    // unanswered and what each one is allowed to be:

    // policy (/path/policy.json) is incomplete - dexdo seller will not place an order.
    // Unanswered/invalid (no defaults allowed):
    // seller.on.after_deal_done -> retire (...)
    // Run `dexdo policy init` to scaffold, fill every field, then retry.

    // That list is the answer, not a detail behind one: it is the whole reason `dexdo policy
    // validate` exists, and the seller's refusal is the same text so an operator never has to run a
    // second command to learn which field is missing. Two lines in its place ("the rules are not
    // filled in") would leave the record holding the only copy -- and at the shipped level the
    // record is not on screen, so the answer would reach nobody.
    // A note the operator has to choose, on a run that cannot ask.
    if lower.contains("--note-addr") && lower.contains("required") {
        return Some(Refusal::new(
            "This run has no note to spend from, and nothing was sent.",
            "Pass --note-addr, or run the same command on a terminal -- it offers the notes the \
             pool records, with what each one holds.",
            detail,
        ));
    }
    // The form the client prints is not the form its own inputs accept yet.
    if lower.contains("unsupported address workchain") {
        return Some(Refusal::new(
            "The address given is not in a form this command accepts, and nothing was sent.",
            "Pass the note as `0:<account>` -- the printed `<dapp>::<account>` form is not accepted \
             on input yet.",
            detail,
        ));
    }
    // Money that is not there yet. The wait itself has its own two lines; this is the plain refusal.
    if lower.contains("insufficient") || lower.contains("is waiting for") {
        return Some(Refusal::new(
            "The wallet does not hold enough for this yet, and nothing was sent.",
            "Top the wallet up in Acki Nacki Wallet, then run the same command again -- it re-reads \
             the balance and carries on.",
            detail,
        ));
    }
    // The chain could not be reached, and WHERE is the whole content of that refusal. An
    // operator holds three candidate addresses at once -- `--endpoint`, the manifest's own, the
    // network default -- and a sentence naming none of them cannot tell them which one was dialled,
    // which is the single fact the run was there to establish. The address is not invented here: it
    // is lifted out of the error the transport already wrote.
    if lower.contains("connect")
        || lower.contains("timed out")
        || lower.contains("connection")
        || lower.contains("dns")
    {
        let did_not_happen = match endpoint_named_in(&text) {
            Some(endpoint) => {
                format!("The client could not reach the chain at {endpoint}, and nothing was sent.")
            }
            // A transport failure whose text names no address at all. The sentence it always had is
            // still true, and inventing an endpoint to fill the gap would be worse than the gap.
            None => "The client could not reach the chain, and nothing was sent.".to_string(),
        };
        return Some(Refusal::breakage(
            did_not_happen,
            "Check the network, then run the same command again. `dexdo doctor` says what it can \
             reach and what it cannot.",
            detail,
        ));
    }
    // Another instance holds this data directory. The remedy is in the operator's hands and the
    // message already carries it; what it lacked was the shape.
    if lower.contains("instance is already using data directory") || lower.contains("instance.lock")
    {
        return Some(Refusal::new(
            "Another instance of this command is already using that data directory, and nothing \
             was sent.",
            "Give this run its own --data-dir, or stop the instance that holds it.",
            detail,
        ));
    }
    // Not recognised: left exactly as it was. A refusal nobody has written two lines for is still
    // better than two lines invented for it -- and rewriting every unknown error would also rewrite
    // the ones other contracts pin word for word.
    None
}

/// The address a transport failure names, taken out of the error rather than guessed at.

/// Every layer that dials the chain already writes the address into its own context -- the SDK's
/// liveness probe leaves `POST http://127.0.0.1:1/graphql:... Connection refused`, the balance read
/// leaves `connect read-only balance endpoint <url>` -- so the endpoint is in `{error:#}` by the
/// time a refusal is composed. What was missing was carrying it the last step, onto the screen.

/// Whole rather than folded the way [`address`] folds an account: an endpoint is a handful of
/// characters the operator typed or a manifest supplied, and the point of printing it is that they
/// can compare it against what they meant. A folded one would name nothing.

/// Total by construction -- every step is a `split` or a `trim` on the borrowed text, so there is no
/// input that panics and none that allocates.
fn endpoint_named_in(text: &str) -> Option<&str> {
    text.split_ascii_whitespace()
        .filter(|word| word.contains("://"))
        // The address arrives wearing the punctuation of a sentence: `POST <url>:` from the SDK's
        // context, `for url (<url>)` from `reqwest`. Alphanumerics and the path separator are what a
        // URL may legitimately end on; anything else at either edge belongs to the prose.
        .map(|word| word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '/'))
        // Re-checked after trimming, so a bare `://` left in some other error is not reported as an
        // address: both halves have to survive.
        .find(|word| {
            word.split_once("://")
                .is_some_and(|(scheme, rest)| !scheme.is_empty() && !rest.is_empty())
        })
}

/// Raw ECC[2] as the unit an operator holds, with a fraction only where there is one.
pub(crate) fn shell(raw: u128) -> String {
    let unit = dexdo_core::params::SHELL_UNIT;
    let whole = raw / unit;
    let fraction = raw % unit;
    if fraction == 0 {
        return whole.to_string();
    }
    format!("{whole}.{fraction:0>9}")
        .trim_end_matches('0')
        .to_string()
}

/// An address with its dapp still named and neither half in full.

/// The dapp half says which dapp the account lives in and is not decoration; the tails are what tell
/// two notes apart at a glance. The whole form is one `RUST_LOG=info` line away.
pub(crate) fn address(raw: &str) -> String {
    let scoped = dexdo_core::address::display_self_dapp(raw);
    match scoped.split_once("::") {
        Some((dapp, account)) => format!("\u{2026}{}::\u{2026}{}", tail(dapp), tail(account)),
        None => format!("\u{2026}{}", tail(&scoped)),
    }
}

fn tail(part: &str) -> String {
    let chars: Vec<char> = part.chars().collect();
    chars[chars.len().saturating_sub(6)..].iter().collect()
}

/// A duration in the words a person uses for it.
pub(crate) fn how_long(seconds: u64) -> String {
    let (amount, unit) = match seconds {
        0..=90 => (seconds, "second"),
        91..=5400 => ((seconds + 30) / 60, "minute"),
        _ => ((seconds + 1800) / 3600, "hour"),
    };
    if amount == 1 {
        format!("{amount} {unit}")
    } else {
        format!("{amount} {unit}s")
    }
}

// the transport refusal has to name the address it could not reach. Its own file rather
// than a case in `tests` below, and free of the removed chain feature, because `for_operator` is asked
// about every error on every build -- so this belongs in the gate CI actually runs.
#[cfg(test)]
#[path = "refusal_endpoint_named_1481.rs"]
mod refusal_endpoint_named_1481;

// second point: the money-lock advice has to say WHERE the note goes, not only which command.
// Its own file for the same two reasons as above -- `for_operator` answers on every build, so
// the pin belongs in the default gate, and the assertions are token-level because `render()` wraps
// this very sentence.
#[cfg(test)]
#[path = "refusal_advice_says_where_the_note_goes_1474.rs"]
mod refusal_advice_says_where_the_note_goes_1474;

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape the directive asks for, and the only one a test should pin: what happened, what to
    /// do, and the detail still there for whoever needs it.
    #[test]
    fn a_refusal_leads_with_the_fact_and_the_instruction() {
        let refusal = Refusal::new(
            "No seller is offering qwen--qwen3--32b at 3 SHELL a tick right now, and nothing was committed.",
            "Add --wait-for-seller to queue the buy until one appears, or try again later.",
            "BUYER_PREFLIGHT matchable=false reason=empty_model_book detail=...",
        );
        let rendered = refusal.render_with(crate::cli::style::Palette::None);
        let lines: Vec<&str> = rendered.lines().collect();

        // Asserted by content rather than by prefix: the first line is wrapped in the colour that
        // says whose problem this is, and the sentence is what carries the meaning under it.
        // `spec.md` gives the two kinds their own glyphs: a warning for the market's answer, an
        // error mark for the client's own failure. Both stand in column 0 with the sentence from
        // column 2.
        assert!(
            lines[0].contains("\u{26a0} ") && lines[0].contains("No seller is offering"),
            "{rendered}"
        );
        assert!(
            lines[1].trim_start().starts_with("Add --wait-for-seller"),
            "{rendered}"
        );
        assert_eq!(lines.len(), 2, "two lines and nothing else: {rendered}");
        assert!(
            !rendered.contains("BUYER_PREFLIGHT"),
            "the machine line is a record, not a thing an operator reads: {rendered}"
        );
        let carried = format!("{:#}", refusal.into_error());
        assert!(
            carried.contains("BUYER_PREFLIGHT"),
            "and the error still carries it underneath: {carried}"
        );
    }

    /// An operator holds SHELL, not raw ECC[2]: `100000000000` is a number they have to divide
    /// before they can compare it with anything they know.
    #[test]
    fn an_amount_reads_in_the_unit_the_operator_holds() {
        let unit = dexdo_core::params::SHELL_UNIT;
        assert_eq!(shell(0), "0");
        assert_eq!(shell(unit), "1");
        assert_eq!(shell(100 * unit), "100");
        assert_eq!(shell(unit / 2), "0.5");
        assert_eq!(shell(6 * unit + unit / 10), "6.1");
    }

    /// The dapp half stays named, and neither half arrives in full: the live refusal carried 128
    /// hex characters of one address.
    #[test]
    fn an_address_keeps_its_dapp_and_loses_its_middle() {
        let scoped = address(
            "0000000000000000000000000000000000000000000000000000000000000004::c59c7c5867f2addcad6b0bc9ad29eaa4e0cba92a874a0e4d8520104e626cb785",
        );
        assert_eq!(scoped, "\u{2026}000004::\u{2026}6cb785");
        assert!(scoped.chars().count() < 20, "{scoped}");
    }

    /// The outcome, not the mechanism: a refusal turned into an error must LEAVE a record, and at
    /// the shipped default level must leave none.

    /// The chain carrying the detail was asserted before this and proved nothing: on the human path
    /// `main` prints the two lines and exits, so nobody ever unwraps that chain. What has to be true
    /// is that `RUST_LOG=info` prints the detail and the default does not -- both measured here by
    /// capturing what the subscriber actually wrote.
    #[test]
    fn the_detail_is_recorded_at_info_and_silent_by_default() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Captured(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for Captured {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .expect("capture buffer")
                    .extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
            type Writer = Captured;
            fn make_writer(&'a self) -> Captured {
                self.clone()
            }
        }

        let refusal = || {
            Refusal::new(
                "Hot ...7acc2b::...7acc2b is still short 100 SHELL after 10 minutes.",
                "Confirm the transfer, then run the same command again.",
                "note deploy: timed out after 600s waiting for Hot 0:f5d7cf2a...; still missing \
                 100000000000 raw ECC[2] SHELL",
            )
        };

        let at_info = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(at_info.clone())
            .with_ansi(false)
            .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let _ = refusal().into_error();
        });
        let recorded = String::from_utf8(at_info.0.lock().expect("buffer").clone()).expect("utf-8");
        assert!(
            recorded.contains("100000000000 raw ECC[2] SHELL")
                && recorded.contains("timed out after 600s"),
            "the detail has to be recoverable with RUST_LOG=info: {recorded}"
        );

        let at_default = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(at_default.clone())
            .with_ansi(false)
            .with_env_filter(tracing_subscriber::EnvFilter::new("error"))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let _ = refusal().into_error();
        });
        let quiet = String::from_utf8(at_default.0.lock().expect("buffer").clone()).expect("utf-8");
        assert!(
            quiet.is_empty(),
            "at the shipped default the operator's screen carries the two lines and nothing else: {quiet}"
        );
    }

    /// The sweep: every situation an operator reaches on the buy / deploy / bind paths turns into
    /// two lines with an action -- and everything else is left exactly as it was.

    /// The second half matters as much as the first. An earlier version rewrote every unrecognised
    /// error into "this one is on the client", which invented an explanation for refusals that were
    /// already good ("another instance is using that data directory") and broke the contracts that
    /// pin an exact message word for word.
    #[test]
    fn the_translation_covers_what_it_knows_and_leaves_the_rest_alone() {
        let cases = [
            (
                "buyer note 0000...0004::ad6f already has another money submission awaiting by-fact \
                 reconciliation; no BOC was sent (/path/note.money: pool lock is already held)",
                "orders journal",
                Kind::Business,
            ),
            (
                "a real chain provisioning: --note-addr (provisioned note address) is required",
                "--note-addr",
                Kind::Business,
            ),
            (
                "unsupported address workchain \"0000...0004\"; sdk Address supports only workchain 0",
                "0:<account>",
                Kind::Business,
            ),
            (
                "Another seller instance is already using data directory /tmp/shared; choose a \
                 different --data-dir (lock /tmp/shared/.dexdo-seller.instance.lock is held)",
                "--data-dir",
                Kind::Business,
            ),
            (
                "connect read-only balance endpoint https://example: transport refused",
                "doctor",
                Kind::Breakage,
            ),
        ];

        for (raw, expected_action, expected_kind) in cases {
            let refusal = for_operator(&anyhow::anyhow!("{raw}"))
                .unwrap_or_else(|| panic!("this situation has to be recognised: {raw}"));
            assert!(
                refusal.do_next().contains(expected_action),
                "{raw}\n-> {}",
                refusal.do_next()
            );
            assert_eq!(refusal.kind(), expected_kind, "{raw}");
            let shown = refusal.render_with(crate::cli::style::Palette::None);
            assert!(
                !shown.contains("/path/") && !shown.contains("BOC"),
                "the record must not reach the screen: {shown}"
            );
        }

        for left_alone in [
            // Nobody has written two lines for this one, so it is printed as its author wrote it.
            "the buyer mock chain also requires --mock-model (, 11.1)",
            // Written for an operator already, and it names the fields -- which is the answer the
            // command was run for. Two lines here would put that list in the record only, and the
            // shipped level keeps the record off the screen.
            "policy (/path/policy.json) is incomplete - dexdo buyer will not place an order.\n\
             Unanswered/invalid (no defaults allowed):\n  buyer.on.dead_gateway -> UNSET\n\
             Run `dexdo policy init` to scaffold, fill every field, then retry.",
        ] {
            assert!(
                for_operator(&anyhow::anyhow!("{left_alone}")).is_none(),
                "this refusal is printed as it was written, not rewritten: {left_alone}"
            );
        }
    }

    /// A refusal with more than one way out draws them as commands, one per row.

    /// `spec.md`'s `try:` block: the first row carries the label, the rest line up under it, and
    /// every command is in the colour that means "yours to act on". Asserted on the shape an
    /// operator reads, because that is what the block exists for -- three options in one paragraph
    /// make them read prose to find a flag.
    #[test]
    fn a_refusal_with_several_ways_out_lists_them_as_commands() {
        let refusal = Refusal::new(
            "No seller is offering qwen right now, and nothing was committed.",
            "Add --wait-for-seller to queue the buy until one appears.",
            "BUYER_PREFLIGHT matchable=false",
        )
        .with_alternatives([
            (
                "queue the buy".to_string(),
                "dexdo buyer --wait-for-seller".to_string(),
            ),
            (
                "see what is offered".to_string(),
                "dexdo markets".to_string(),
            ),
        ]);
        let rendered = refusal.render_with(crate::cli::style::Palette::None);
        let lines: Vec<&str> = rendered.lines().collect();

        // News, then the instruction in words, then one row per way out. The instruction used to be
        // missing here: `render` returned early on the alternatives branch, so a refusal that
        // offered commands silently dropped the sentence naming the remedy -- and this assertion,
        // written as `lines.len() == 3`, recorded that loss as if it were the design.
        assert_eq!(
            lines.len(),
            4,
            "one line of news, the instruction, and one row per way out: {rendered}"
        );
        assert!(lines[0].contains("No seller is offering"), "{rendered}");
        assert!(
            lines[1].contains("Add --wait-for-seller"),
            "the instruction survives the presence of alternatives -- `with_alternatives` says of \
             itself that they replace nothing: {rendered}"
        );
        assert!(
            lines[2].contains("try") && lines[2].contains("dexdo buyer --wait-for-seller"),
            "{rendered}"
        );
        assert!(
            !lines[3].contains("try") && lines[3].contains("dexdo markets"),
            "the second row continues under the first, without repeating the label: {rendered}"
        );
        assert!(
            !rendered.contains("BUYER_PREFLIGHT"),
            "the record stays off the screen: {rendered}"
        );
    }

    /// "after 600s" is how a timeout is stored, not how a wait is described.
    #[test]
    fn a_duration_reads_as_a_person_would_say_it() {
        assert_eq!(how_long(45), "45 seconds");
        assert_eq!(how_long(600), "10 minutes");
        assert_eq!(how_long(1), "1 second", "no plural on one");
        assert_eq!(how_long(120), "2 minutes");
        assert_eq!(how_long(7200), "2 hours");
    }
}
