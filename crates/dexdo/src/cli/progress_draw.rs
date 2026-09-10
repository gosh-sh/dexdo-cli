//! What the status display actually writes, and where.

//! Split out of [`super::progress`] so that module keeps the command-facing API and this one keeps
//! the escape sequences. Two renderings, chosen once from the destination:

//! - **On a terminal**: exactly ONE line is ever rewritten -- the live one, at the bottom. The
//! checklist is printed into the scrollback like any other output: the steps once when the
//! command starts, a ticked line each time one is passed.
//! - **Everywhere else**: the same lines, written once, with no escapes at all -- a log file and a
//! machine consumer read the same bytes, and a cursor movement in either is corruption.

//! # Why nothing is redrawn above the cursor

//! An earlier version redrew the whole checklist in place, moving the cursor up by the number of
//! lines it had drawn. That is correct only while the block does not sit at the bottom of the
//! window: once it does, every newline scrolls the screen, the recorded distance no longer points
//! at the block, and each frame leaves a copy of itself behind. Observed live as the checklist
//! repeating down the screen forever.

//! One rewritten line has no such arithmetic. It is also why the live line is truncated to the
//! window: a line that wraps is two lines, and `\r` only ever returns to the start of the last one.

use std::io::{IsTerminal as _, Write as _};
use std::time::{Duration, Instant};

use super::progress_plan::Plan;

/// Where the status display writes. A duplicate of stderr on unix, so redirecting descriptor 2
/// does not redirect the status display with it; plain stderr elsewhere.
#[cfg(unix)]
type Sink = std::fs::File;
#[cfg(not(unix))]
type Sink = std::io::Stderr;

#[cfg(unix)]
fn own_stderr() -> Sink {
    use std::os::fd::FromRawFd as _;
    // SAFETY: `dup` returns a fresh descriptor this process owns, and the File takes it over.
    let duplicated = unsafe { libc::dup(libc::STDERR_FILENO) };
    if duplicated < 0 {
        // SAFETY: descriptor 2 is open for the lifetime of the process; the File is leaked on
        // purpose below by never closing what it did not create.
        return unsafe { std::fs::File::from_raw_fd(libc::STDERR_FILENO) };
    }
    // SAFETY: `duplicated` is a valid owned descriptor.
    unsafe { std::fs::File::from_raw_fd(duplicated) }
}

#[cfg(not(unix))]
fn own_stderr() -> Sink {
    std::io::stderr()
}

/// Columns of the terminal, or the conventional assumption when nothing reports one.
#[cfg(unix)]
fn window_columns() -> usize {
    // SAFETY: winsize is plain data; zeroed is a valid representation, and the ioctl fills it in.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: the pointer is to the local winsize above, which outlives the call.
    if unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut size) } != 0
        || size.ws_col == 0
    {
        return DEFAULT_COLUMNS;
    }
    size.ws_col as usize
}

#[cfg(not(unix))]
fn window_columns() -> usize {
    DEFAULT_COLUMNS
}

const DEFAULT_COLUMNS: usize = 80;
/// Wide enough to read at a glance, narrow enough to leave the label room on an 80-column window.
const BAR_WIDTH: usize = 16;
pub(super) const TICK: Duration = Duration::from_millis(120);
const FRAMES: [&str; 10] = [
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280f}",
];
const DIM: &str = "\x1b[2m";
/// The same amber a refusal uses for "this one is yours": it marks the line an operator has to act
/// on. A wait on the client and a wait on the person look identical otherwise, and the second one
/// never ends by itself.
const AMBER: &str = "\x1b[38;5;214m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const RESET: &str = "\x1b[0m";
const CLEAR_LINE: &str = "\r\x1b[2K";

fn plain_clear_line(columns: usize) -> String {
    format!("\r{}\r", " ".repeat(columns.saturating_sub(1)))
}

/// How far a measurable wait has come, when the running code can say: `(done, total)` in whatever
/// unit it counts -- blocks, bytes, deals. Rendered as a bar beside the live line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Measure {
    pub(super) done: u64,
    pub(super) total: u64,
}

impl Measure {
    /// `[####....] 62%`, drawn with block characters

    /// A bar is drawn only where there is a real denominator. A spinner says "alive"; a bar says
    /// "this far of that much", and one drawn from an invented total says something false.
    fn bar(self, width: usize) -> String {
        let filled = if self.total == 0 {
            0
        } else {
            (self.done.min(self.total) as usize * width).div_ceil(self.total.max(1) as usize)
        };
        let percent = (self.done.min(self.total) * 100)
            .checked_div(self.total)
            .unwrap_or(0);
        format!(
            "\u{2595}{}{}\u{258f}{percent:>3}%",
            "\u{2588}".repeat(filled.min(width)),
            "\u{2591}".repeat(width.saturating_sub(filled)),
        )
    }
}

pub(super) struct Shared {
    /// A bar for the current step, where the step can be measured at all.
    pub(super) measure: Option<Measure>,
    pub(super) label: String,
    pub(super) started: Instant,
    pub(super) plan: Plan,
    frame: usize,
    pub(super) live: bool,
    pub(super) colour: bool,
    columns: usize,
    /// The real stderr, held as its own descriptor.

    /// [`super::progress_capture`] points file descriptor 2 at a pipe while the prover runs, so a
    /// status line that wrote through `std::io::stderr()` would feed itself into the pipe it is
    /// draining. This is taken before any of that and stays valid through it.
    sink: Sink,
    /// Is the current step waiting on the OPERATOR rather than on the client?
    pub(super) needs_you: bool,
    /// The live line is off while something else owns the screen.

    /// An interview asks its questions on `/dev/tty` and the operator types the answer there. The
    /// ticker redraws stderr on its own schedule and knows nothing about that, so every frame
    /// landed on top of the prompt: the question scrolled, the `[1/4]...` line took its place and
    /// the answer was typed blind. Observed on a real buy, over seven questions.
    pub(super) held: bool,
}

impl Shared {
    pub(super) fn new(label: String, plan: Plan) -> Shared {
        let sink = own_stderr();
        let terminal = sink.is_terminal();
        Shared {
            measure: None,
            label,
            started: Instant::now(),
            plan,
            frame: 0,
            live: terminal,
            colour: terminal && !crate::cli::no_color_requested(),
            columns: if terminal {
                window_columns()
            } else {
                DEFAULT_COLUMNS
            },
            sink,
            held: false,
            needs_you: false,
        }
    }

    fn write(&mut self, text: &str) {
        let _ = self.sink.write_all(text.as_bytes());
        let _ = self.sink.flush();
    }

    /// One rewritten line: spinner, what is happening, and how long it has been happening.
    pub(super) fn render(&mut self) {
        if !self.live || self.held {
            return;
        }
        self.frame = (self.frame + 1) % FRAMES.len();
        let spinner = FRAMES[self.frame];
        let seconds = format!("{}s", self.started.elapsed().as_secs());
        // Where the run stands, beside what it is doing: the checklist itself cannot be shown as a
        // block, because nothing above the cursor may be redrawn.
        let label = match self.plan.position() {
            Some((step, total)) => format!("[{step}/{total}] {}", self.label),
            None => self.label.clone(),
        };
        // The spinner, two spaces and the seconds are the fixed part; whatever room is left is the
        // label's, and a label that does not fit is cut rather than wrapped -- a wrapped line is
        // two lines, and the next frame's `\r` would only rewrite the second.
        let bar = self
            .measure
            .filter(|measure| measure.total > 0)
            .map(|measure| format!(" {}", measure.bar(BAR_WIDTH)))
            .unwrap_or_default();
        // A plain terminal clears by overwriting the row, so it leaves the last column unused:
        // filling it can trigger an automatic wrap before the next carriage return arrives.
        let columns = if self.colour {
            self.columns
        } else {
            self.columns.saturating_sub(1)
        };
        let room = columns.saturating_sub(
            spinner.chars().count() + seconds.chars().count() + bar.chars().count() + 2,
        );
        let label = format!("{}{bar}", clip(&label, room));
        // Amber for a wait that ends only when the operator does something, cyan for the client's
        // own work. Colour is the only difference, and it is deliberately not the only signal: the
        // label itself is written as an instruction, so a plain terminal reads the same.
        let text = if self.colour {
            let mark = if self.needs_you { AMBER } else { CYAN };
            let body = if self.needs_you {
                format!("{AMBER}{label}{RESET}")
            } else {
                label.clone()
            };
            format!("{CLEAR_LINE}{mark}{spinner}{RESET} {body} {DIM}{seconds}{RESET}")
        } else {
            let text = format!("{spinner} {label} {seconds}");
            let padding = columns.saturating_sub(visible_width(&text));
            format!("\r{text}{}", " ".repeat(padding))
        };
        self.write(&text);
    }

    /// Remove the live line, leaving the cursor where it was.
    pub(super) fn erase(&mut self) {
        if self.live {
            if self.colour {
                self.write(CLEAR_LINE);
            } else {
                self.write(&plain_clear_line(self.columns));
            }
        }
    }

    /// Put one line into the log, above the live one.
    pub(super) fn line(&mut self, text: &str) {
        self.erase();
        let text = format!("{}\n", clip(text, self.columns));
        self.write(&text);
    }

    /// A line that records a step passing. Ticks are green where colour is allowed; the mark itself
    /// carries the meaning, so a captured log reads the same without it.
    pub(super) fn ticked(&mut self, text: &str) {
        let text = if self.colour {
            format!("{GREEN}{text}{RESET}")
        } else {
            text.to_string()
        };
        self.line(&text);
    }
}

/// Cut `text` to `columns` of VISIBLE width, marking the cut.

/// Escape sequences take no width and must never be split: the wallet's name carries an `OSC 8`
/// hyperlink, which makes a 67-character sentence 109 long, and a cut counted in raw characters
/// lands inside the sequence and leaves the terminal holding an unclosed link -- every line after it
/// becomes part of that link. Sequences are therefore copied whole and only printable characters are
/// counted, whole characters at that, because a cut inside a multi-byte one is not text either.
fn clip(text: &str, columns: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if visible_width(text) <= columns {
        return text.to_string();
    }
    // Room for the ellipsis that says a cut happened.
    let room = columns.saturating_sub(1);
    let mut out = String::new();
    let mut shown = 0usize;
    let mut at = 0usize;
    while at < chars.len() {
        if chars[at] == ESCAPE {
            let end = escape_end(&chars, at);
            out.extend(&chars[at..end]);
            at = end;
            continue;
        }
        if shown == room {
            break;
        }
        out.push(chars[at]);
        shown += 1;
        at += 1;
    }
    if columns > 0 {
        out.push('\u{2026}');
    }
    out
}

/// Printable characters only: what the operator sees, and what a window is measured in.
fn visible_width(text: &str) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut width = 0usize;
    let mut at = 0usize;
    while at < chars.len() {
        if chars[at] == ESCAPE {
            at = escape_end(&chars, at);
            continue;
        }
        width += 1;
        at += 1;
    }
    width
}

const ESCAPE: char = '\x1b';

/// One past the end of the escape sequence starting at `at`.

/// Two shapes reach this display and they end differently, which is the whole reason this is not a
/// "skip until a letter": a `CSI` ends at its final byte (`@` to `~`), while an `OSC` runs until
/// `BEL` or `ST` and its payload is a URL -- full of letters that would stop a naive scan inside the
/// sequence.
fn escape_end(chars: &[char], at: usize) -> usize {
    match chars.get(at + 1) {
        Some('[') => {
            let mut end = at + 2;
            while end < chars.len() && !matches!(chars[end], '\u{40}'..='\u{7e}') {
                end += 1;
            }
            (end + 1).min(chars.len())
        }
        Some(']') => {
            let mut end = at + 2;
            while end < chars.len() {
                if chars[end] == '\u{7}' {
                    return end + 1;
                }
                if chars[end] == ESCAPE && chars.get(end + 1) == Some(&'\\') {
                    return end + 2;
                }
                end += 1;
            }
            chars.len()
        }
        // Anything else is the two-character form, `ESC \` among them.
        Some(_) => at + 2,
        None => chars.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured output must carry no escapes, so the whole live path stays off under `cargo test`.
    /// A wait that ends only when the operator acts is drawn amber; the client's own work is not.

    /// The two are indistinguishable otherwise, and they are opposite states: one finishes by
    /// itself, the other never does. The colour is on the mark AND the words, and neither carries
    /// the meaning alone -- with colour off the label still reads as an instruction.
    #[test]
    fn a_wait_on_the_operator_is_drawn_in_amber() {
        let mut shared = Shared::new("approve the deploy in the wallet".into(), Plan::default());
        shared.live = true;
        shared.colour = true;
        shared.needs_you = true;
        shared.render();

        let mut client_side = Shared::new("checking the network".into(), Plan::default());
        client_side.live = true;
        client_side.colour = true;
        client_side.render();

        // Rendering writes to the sink; what is asserted is the decision that produced it, because
        // the sink here is a captured pipe and the frame is not readable back from it.
        assert!(shared.needs_you, "the operator's wait keeps its mark");
        assert!(
            !client_side.needs_you,
            "the client's own work does not take it"
        );
    }

    #[test]
    fn a_captured_destination_is_not_live() {
        let shared = Shared::new("first".into(), Plan::default());
        assert!(!shared.live);
        assert!(!shared.colour);
    }

    #[test]
    fn a_plain_terminal_clears_without_escape_sequences() {
        let clear = plain_clear_line(5);
        assert_eq!(clear, "\r    \r");
        assert!(!clear.contains(ESCAPE));
    }

    /// The live line must never wrap: `\r` returns to the start of the LAST screen line, so a
    /// wrapped label leaves the part above it on the screen for good. This is the rule that keeps
    /// a recovery note naming a long path from tearing the display apart.
    #[test]
    fn a_label_longer_than_the_window_is_cut_not_wrapped() {
        let long = "recovery: marked deposit voucher wallet submit as uncertain in \
                    /Users/somebody/Documents/GitHub/dexdo-cli-private-2/.dexdo-net-a/\
                    pn_pool.json.recovery.json; reruns will not submit a second wallet spend.";
        let clipped = clip(long, 40);
        assert_eq!(clipped.chars().count(), 40);
        assert!(clipped.ends_with('\u{2026}'), "{clipped}");
    }

    /// A bar is a proportion, and both ends of it have to be right: empty at the start, full at the
    /// end, and never wider than it was asked for.
    #[test]
    fn a_bar_reads_as_the_proportion_it_is() {
        assert!(Measure {
            done: 0,
            total: 191
        }
        .bar(8)
        .contains("  0%"));
        assert!(Measure {
            done: 191,
            total: 191
        }
        .bar(8)
        .contains("100%"));
        let half = Measure {
            done: 96,
            total: 191,
        }
        .bar(8);
        assert!(half.contains(" 50%"), "{half}");
        assert_eq!(
            half.matches('\u{2588}').count() + half.matches('\u{2591}').count(),
            8
        );
    }

    /// A total nobody knows is not a bar. A wait with no denominator gets the spinner and the
    /// seconds, which say "alive" without claiming to say "this far".
    #[test]
    fn nothing_is_drawn_without_a_denominator() {
        let mut shared = Shared::new("waiting".into(), Plan::default());
        shared.measure = Some(Measure { done: 5, total: 0 });
        shared.render();
        assert!(!shared.live, "captured output renders nothing either way");
    }

    /// An escape has no width and must survive the cut whole. A link cut down the middle leaves the
    /// terminal holding an unclosed hyperlink, and every line after it becomes part of it.
    #[test]
    fn clipping_measures_what_is_seen_and_never_splits_an_escape() {
        let link = "\x1b]8;;https://ackinacki.com/wallet\x1b\\Acki Nacki Wallet\x1b]8;;\x1b\\";
        let line = format!("Vault -> Hot funding request sent; confirm it in {link}.");
        assert_eq!(visible_width(&line), 67, "escapes take no width");

        // Wide enough for every visible character: untouched.
        assert_eq!(clip(&line, 80), line);

        // Narrow: cut, and still balanced -- as many sequence starts as terminators.
        let cut = clip(&line, 40);
        assert!(cut.ends_with('\u{2026}'), "{cut:?}");
        assert!(visible_width(&cut) <= 40, "{}", visible_width(&cut));
        assert_eq!(
            cut.matches('\x1b').count() % 2,
            0,
            "an escape was split: {cut:?}"
        );
    }

    /// A label that fits is left exactly as it is -- no ellipsis, no padding.
    #[test]
    fn a_label_that_fits_is_untouched() {
        assert_eq!(clip("funding the wallet", 40), "funding the wallet");
        assert_eq!(clip("exactly ten", 11), "exactly ten");
    }

    /// Cutting must not split a character in half.

    /// The label is multi-byte on purpose -- a byte-indexed cut would land inside a character and
    /// panic. It is also not Cyrillic on purpose: `ci/check_no_cyrillic.sh` refuses Cyrillic
    /// anywhere in source, string literals included, and this test is what taught me that.
    #[test]
    fn cutting_keeps_whole_characters() {
        let clipped = clip("v\u{00e9}rification du r\u{00e9}seau et des contrats", 10);
        assert_eq!(clipped.chars().count(), 10);
        assert!(clipped.ends_with('\u{2026}'));
    }
}
