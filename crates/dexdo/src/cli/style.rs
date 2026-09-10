//! How every line the operator reads is drawn: colour roles, glyphs, the grid and the numbers.

//! One module, because the alternative is what this replaces -- each command picking its own
//! padding, its own shade of green and its own way of writing an amount, so that two results printed
//! by one program looked like two programs.

//! The rules are `spec.md`: 120 columns, glyph in column 0, text from column 2, values aligned to
//! column 12, human numbers, truecolor with a 256-colour fallback.

//! Every glyph is written as an escape rather than as itself. The publishing gate reads the
//! generated tree as strict ASCII, and a character pasted into source is how it went red -- twice.

use std::io::IsTerminal as _;

/// What a fragment MEANS, which is the only thing colour is allowed to say.

/// A role, not a colour: the palette is one table below, and a caller never names a shade. That is
/// what keeps "success" the same green in the buyer, the seller and the note commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    /// Amounts and block headings.
    Bold,
    /// Ordinary text: no escape at all.
    Text,
    /// Labels, units, explanations.
    Label,
    /// Raw figures and technical asides -- quieter than a label.
    Meta,
    /// Success, and the buy side.
    Ok,
    /// A spinner, a warning, a wait.
    Wait,
    /// A model, a note, a transaction hash.
    Id,
    /// A refusal, and the sell side.
    Err,
}

impl Role {
    /// `(truecolor, 256-colour)` for this role, or `None` where the role adds no colour at all.
    fn sgr(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Bold => Some(("\u{1b}[1m", "\u{1b}[1m")),
            Self::Text => None,
            Self::Label => Some(("\u{1b}[38;2;112;122;140m", "\u{1b}[38;5;245m")),
            Self::Meta => Some(("\u{1b}[38;2;75;84;100m", "\u{1b}[38;5;240m")),
            Self::Ok => Some(("\u{1b}[38;2;79;201;140m", "\u{1b}[38;5;78m")),
            Self::Wait => Some(("\u{1b}[38;2;226;177;105m", "\u{1b}[38;5;179m")),
            Self::Id => Some(("\u{1b}[38;2;98;184;221m", "\u{1b}[38;5;74m")),
            Self::Err => Some(("\u{1b}[38;2;217;123;123m", "\u{1b}[38;5;174m")),
        }
    }
}

const RESET: &str = "\u{1b}[0m";

/// The glyphs, by what they mean. Column 0 of a line, never inside one.
pub(crate) const OK: &str = "\u{2714}";
pub(crate) const ERR: &str = "\u{2716}";
pub(crate) const WARN: &str = "\u{26a0}";
pub(crate) const INFO: &str = "\u{25c6}";
pub(crate) const BUY: &str = "\u{25b2}";

/// Where a value starts: labels are indented two, values line up in column twelve.
const VALUE_COLUMN: usize = 12;
const INDENT: usize = 2;

/// The gap between a label and its value, and it exists whatever the label's length.

/// It is subtracted from the label's own field rather than added after it, so that the gap is part
/// of the grid instead of whatever padding happens to be left over. That distinction is the whole
/// defect in: the label used to be padded to `VALUE_COLUMN - INDENT` and nothing separated it
/// from the value, so a name that filled the field exactly consumed the separator with it and
/// `dexdo doctor` printed `generationmanifest 4.0.36, chain 4.0.36`. A name LONGER than the field
/// collided the same way, which is why the fix cannot be "make the field wide enough for today's
/// longest name": the contract names in that block come from the manifest and are not this code's
/// to bound.

/// Values whose label fits the narrowed field keep the column they have always started in, so
/// every row that reads correctly today is unchanged; only the rows that collided move, and they
/// move right by exactly the overflow.
const LABEL_GAP: usize = 1;

/// How much colour this destination can take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Palette {
    /// No escapes at all: `NO_COLOR`, `--no-color`, or a destination that is not a terminal. The
    /// glyphs stay -- they carry meaning, and a log with them reads the same as a screen.
    None,
    /// `COLORTERM` says the terminal draws 24-bit colour.
    TrueColor,
    /// Anything else that is a terminal.
    Ansi256,
}

impl Palette {
    /// What the stream in front of us can show.
    pub(crate) fn of(stream_is_terminal: bool) -> Self {
        Self::resolved(stream_is_terminal, crate::cli::no_color_requested())
    }

    /// The same answer from facts a caller already holds.

    /// A site that is handed "this stream is a terminal" and "colour was refused" as arguments --
    /// which is how the funding notice is testable at all -- must not have to set the environment
    /// to ask this question.
    pub(crate) fn resolved(stream_is_terminal: bool, no_color: bool) -> Self {
        if no_color || !stream_is_terminal {
            return Self::None;
        }
        match std::env::var("COLORTERM").ok().as_deref() {
            Some("truecolor") | Some("24bit") => Self::TrueColor,
            _ => Self::Ansi256,
        }
    }

    /// The palette for ordinary output -- the stream a result goes to.
    pub(crate) fn stdout() -> Self {
        Self::of(std::io::stdout().is_terminal())
    }

    /// The palette for the error stream, where steps, the live line and refusals go.
    pub(crate) fn stderr() -> Self {
        Self::of(std::io::stderr().is_terminal())
    }
}

/// One fragment in its role, reset immediately after it.
pub(crate) fn paint(palette: Palette, role: Role, text: &str) -> String {
    let Some((truecolor, ansi256)) = role.sgr() else {
        return text.to_string();
    };
    match palette {
        Palette::None => text.to_string(),
        Palette::TrueColor => format!("{truecolor}{text}{RESET}"),
        Palette::Ansi256 => format!("{ansi256}{text}{RESET}"),
    }
}

/// A line that opens with a glyph: the glyph in column 0, the text from column 2.
pub(crate) fn glyph_line(palette: Palette, glyph: &str, role: Role, text: &str) -> String {
    format!("{} {text}", paint(palette, role, glyph))
}

/// One `label value` row of a block: label indented two, value in column twelve.

/// The label is quiet and the value is not, because the value is what the operator came for.
pub(crate) fn field(palette: Palette, label: &str, value: &str, value_role: Role) -> String {
    // An empty label is a row that continues the one above it: the value keeps its column and the
    // label column stays blank, which is what a list under one label looks like. The role still
    // applies -- dropping it here painted the first wrapped line of a paragraph differently from
    // every continuation line, because those go through `field_continued(&paint(...))`.
    if label.is_empty() {
        return field_continued(&paint(palette, value_role, value));
    }
    let pad = VALUE_COLUMN - INDENT - LABEL_GAP;
    format!(
        "{:INDENT$}{:<pad$}{:LABEL_GAP$}{}",
        "",
        paint(palette, Role::Label, label),
        "",
        paint(palette, value_role, value),
        INDENT = INDENT,
        pad = pad + painted_overhead(palette, Role::Label),
        LABEL_GAP = LABEL_GAP,
    )
}

/// Something for the operator to do: amber, and bold.

/// Every "next", every `try:` command, every "run this now" in the client goes through here. Two
/// roles at once rather than a ninth role, because it is not a ninth meaning -- it is `Wait`, "this
/// one is yours", with the weight that makes it findable in a screen of results. Uniform on purpose:
/// an operator scanning for what to type should be able to find it by colour alone, in any command.
pub(crate) fn action(palette: Palette, text: &str) -> String {
    paint(palette, Role::Bold, &paint(palette, Role::Wait, text))
}

/// A continuation under a value rather than under its label.
pub(crate) fn field_continued(text: &str) -> String {
    format!("{:VALUE_COLUMN$}{text}", "", VALUE_COLUMN = VALUE_COLUMN)
}

/// How many invisible bytes a painted label adds, so `{:<width}` still lines the values up.

/// Padding counts bytes, and an escape sequence is bytes nobody sees. Without this every coloured
/// label pulls its own value left by the length of its escape, which is exactly the ragged column
/// this module exists to remove.
fn painted_overhead(palette: Palette, role: Role) -> usize {
    match (palette, role.sgr()) {
        (Palette::None, _) | (_, None) => 0,
        (Palette::TrueColor, Some((truecolor, _))) => truecolor.len() + RESET.len(),
        (Palette::Ansi256, Some((_, ansi256))) => ansi256.len() + RESET.len(),
    }
}

/// The window this output is laid out for: what the terminal reports, capped at the spec's 120, and
/// 120 where nothing reports anything (a pipe, a log file).

/// Both streams are asked, stderr first. Refusals, steps and the live line -- everything this
/// wrapping is FOR -- go to stderr, and measuring only stdout got the width wrong in the ordinary
/// case of `dexdo buyer --model X > run.log`: the ioctl on the redirected stdout fails, the width
/// falls back to 120, and the refusal is then folded at 80 by the terminal stderr still points at.
/// That fold is the exact failure the wrapping was added to prevent.
pub(crate) fn window_columns() -> usize {
    const SPEC_WIDTH: usize = 120;
    #[cfg(unix)]
    {
        for stream in [libc::STDERR_FILENO, libc::STDOUT_FILENO] {
            // SAFETY: winsize is plain data and the ioctl fills the local struct.
            let mut size: libc::winsize = unsafe { std::mem::zeroed() };
            if unsafe { libc::ioctl(stream, libc::TIOCGWINSZ, &mut size) } == 0 && size.ws_col > 0 {
                return (size.ws_col as usize).min(SPEC_WIDTH);
            }
        }
    }
    SPEC_WIDTH
}

/// One field whose value is prose: wrapped at the window, continuing under the value column.

/// A sentence longer than the window is folded by the terminal at column zero, which puts half of it
/// under the label and breaks the grid this module exists to keep. Words are never split.
pub(crate) fn field_wrapped(
    palette: Palette,
    label: &str,
    value: &str,
    value_role: Role,
) -> String {
    let room = window_columns().saturating_sub(VALUE_COLUMN).max(20);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in value.split_whitespace() {
        if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > room {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    let mut out = String::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            out.push('\n');
            out.push_str(&field_continued(&paint(palette, value_role, line)));
        } else {
            out.push_str(&field(palette, label, line, value_role));
        }
    }
    out
}

/// Did the operator ask for the raw chain figures as well?
static RAW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record `--raw`, once, from `main`.
pub(crate) fn configure_raw(raw: bool) {
    RAW.store(raw, std::sync::atomic::Ordering::Relaxed);
}

/// Whether a block should add its raw line.
pub(crate) fn raw_requested() -> bool {
    RAW.load(std::sync::atomic::Ordering::Relaxed)
}

/// The raw line a block adds under itself when `--raw` is on: quiet, and never in anybody's way
/// otherwise.
pub(crate) fn raw_line(palette: Palette, text: &str) -> String {
    field(
        palette,
        "raw",
        &paint(palette, Role::Meta, text),
        Role::Meta,
    )
}

/// An amount as a person says it: SHELL to two decimals.

/// Raw units are never printed beside it -- `--raw` is where a reconstruction gets them. Two
/// decimals rather than "as many as it takes", so a column of amounts is a column.

/// Truncated, not rounded: money shown to the person who owns it may be short of what is there,
/// never over it. Rounding up would print `1.00` for an amount that cannot buy what `1.00` buys.

/// The one thing truncation must not do is print money as no money. Every amount this draws today
/// is a whole number of SHELL by protocol -- `PRICE_STEP == SHELL_UNIT`, so a price is a whole
/// number of SHELL a tick -- but this helper is general, and the first fee or remainder that is not
/// would arrive as `0.00` with `--raw` the only way to learn otherwise. Below a hundredth the
/// answer is therefore `<0.01`: still smaller than the truth, and no longer a claim that the amount
/// is zero.
pub(crate) fn shell(raw: u128) -> String {
    let unit = dexdo_core::params::SHELL_UNIT;
    let whole = raw / unit;
    let hundredths = (raw % unit) * 100 / unit;
    if raw > 0 && whole == 0 && hundredths == 0 {
        return "<0.01".to_string();
    }
    format!("{whole}.{hundredths:02}")
}

/// The last six characters of an address, for a line that identifies rather than addresses.

/// A status line names which note it is talking about; a result prints the whole address, because
/// that one is copied. `spec.md` uses this form in its own samples.
pub(crate) fn short_id(address: &str) -> String {
    let scoped = dexdo_core::address::display_self_dapp(address);
    let account = scoped.split("::").last().unwrap_or(&scoped);
    let chars: Vec<char> = account.chars().collect();
    format!(
        "\u{2026}{}",
        chars[chars.len().saturating_sub(6)..]
            .iter()
            .collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The palette degrades to nothing where colour cannot be shown, and the glyphs stay: they carry
    /// the meaning, and a captured log has to read like the screen it came from.
    #[test]
    fn a_destination_that_is_not_a_terminal_takes_no_escapes() {
        assert_eq!(Palette::of(false), Palette::None);
        let line = glyph_line(Palette::None, OK, Role::Ok, "note deployed");
        assert_eq!(line, "\u{2714} note deployed");
        assert!(!line.contains('\u{1b}'), "{line}");
    }

    /// Values line up in one column whether or not the labels are painted -- the escapes are bytes
    /// nobody sees, and padding counts bytes.
    #[test]
    fn values_line_up_in_the_same_column_painted_or_not() {
        let plain = field(Palette::None, "ceiling", "3.00 SHELL / tick", Role::Bold);
        let painted = field(Palette::Ansi256, "ceiling", "3.00 SHELL / tick", Role::Bold);

        let visible = |line: &str| -> String {
            let mut out = String::new();
            let mut chars = line.chars();
            while let Some(c) = chars.next() {
                if c == '\u{1b}' {
                    for skip in chars.by_ref() {
                        if skip == 'm' {
                            break;
                        }
                    }
                    continue;
                }
                out.push(c);
            }
            out
        };
        assert_eq!(visible(&plain), visible(&painted));
        assert!(
            visible(&plain).starts_with("  ceiling   3.00"),
            "{}",
            visible(&plain)
        );
    }

    /// Amounts are what a person says, to two decimals, and never the raw integer.
    #[test]
    fn an_amount_reads_as_a_person_says_it() {
        let unit = dexdo_core::params::SHELL_UNIT;
        assert_eq!(shell(0), "0.00");
        assert_eq!(shell(unit), "1.00");
        assert_eq!(shell(3 * unit), "3.00");
        assert_eq!(shell(6 * unit + unit * 15 / 100), "6.15");
        assert_eq!(shell(100 * unit), "100.00");
    }

    /// Money is never drawn short of the truth in the one direction that matters, and money is
    /// never drawn as no money at all.

    /// Two decimals mean a third one has to go somewhere. It is truncated rather than rounded, so
    /// what is shown is never more than what is held -- and the case truncation must not reach is
    /// an amount that exists arriving as `0.00`, which is the one reading that would send an
    /// operator looking for a payment that is right there. Every caller today hands this whole
    /// SHELL, so nothing below is reachable from the client as it stands; the helper is general and
    /// the first fee that is not whole would find it.
    #[test]
    fn an_amount_that_exists_is_never_drawn_as_nothing() {
        let unit = dexdo_core::params::SHELL_UNIT;
        assert_eq!(shell(1), "<0.01", "one raw unit is money, and says so");
        assert_eq!(shell(unit / 100 - 1), "<0.01", "just under a hundredth");
        assert_eq!(shell(unit / 100), "0.01", "a hundredth is drawn as itself");
        assert_eq!(shell(0), "0.00", "nothing is still nothing");
        // Truncated, never rounded up: 1.999 SHELL may not read as 2.00, because 2.00 buys what
        // 1.999 cannot.
        assert_eq!(shell(unit * 2 - 1), "1.99");
        assert_eq!(shell(unit + unit * 999 / 1000), "1.99");
    }

    /// `--raw` is off by default and adds a quiet line rather than changing the human one.

    /// The two readings are for two different people: an amount as it is said, and the integer the
    /// chain stores. Mixing them is what made a book heading read `prices are raw ECC[2]
    /// (PRICE_STEP 1000000000 = 1 SHELL)` -- two names for one number in one sentence.
    #[test]
    fn the_raw_figures_are_one_flag_away_and_quiet() {
        configure_raw(false);
        assert!(!raw_requested());
        configure_raw(true);
        assert!(raw_requested());

        let line = raw_line(Palette::None, "ceiling 3000000000 ECC");
        assert!(line.starts_with("  raw"), "{line}");
        assert!(line.contains("ceiling 3000000000 ECC"), "{line}");
        configure_raw(false);
    }

    /// A status line identifies a note by its tail; the whole address belongs to a result.
    #[test]
    fn a_status_line_names_a_note_by_its_tail() {
        let address = format!("0:{}", "ab".repeat(32));
        assert_eq!(short_id(&address), "\u{2026}ababab");
    }
}

/// the gap after a label exists whatever the label's length.
#[cfg(test)]
mod issue_1905_label_gap;
