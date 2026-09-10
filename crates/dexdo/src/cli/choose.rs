//! Pick one row with the arrow keys.

//! The client asks an operator to type an address it already knows: a note is chosen by pasting 128
//! hex characters of `--note-addr`, out of a pool the client wrote itself. The address is neither a
//! decision nor an input -- it is a lookup key the operator performs by hand.

//! So a command that needs one row of its own state offers the rows instead. Up/Down moves, Enter
//! chooses, `q` or Ctrl-C leaves without choosing anything.

//! Split the way the QR display is split: the movement and the key decoding are pure and tested;
//! the raw-mode terminal I/O sits behind `unix` next to its only callers. A session with no terminal
//! never reaches either half -- the caller refuses with the flag the operator should have passed,
//! because a prompt nobody can answer is a hang, not a question.

/// Weight and shape, which are not colour and have no role: an interview uses them to tell a title
/// from a question, and they read the same in every palette.
const BOLD: &str = "\u{1b}[1m";
const UNDERLINE: &str = "\u{1b}[4m";
const ITALIC: &str = "\u{1b}[3m";
const RESET: &str = "\u{1b}[0m";

/// The colours come from the one table, like everywhere else.

/// This module used to keep its own: cyan `36`, green `32`, amber `38;5;214`. They were near the
/// spec's shades without being them, so the menu an operator answers and the result it produces were
/// visibly two programs. The menu draws on the terminal it opened, so the only question left is
/// whether colour was refused.
fn palette() -> crate::cli::style::Palette {
    crate::cli::style::Palette::resolved(true, crate::cli::no_color_requested())
}

/// The heading of a section of questions -- not a question itself, and it must not look like one.

/// The interview opened with the same `?` and the same weight as the questions under it, so the
/// operator read the introduction as the first question and lost their place. A title is underlined
/// and carries no mark; a question carries `?`. Two different things, two different shapes.
pub(crate) fn title(text: &str) -> String {
    if crate::cli::no_color_requested() {
        return format!("{text}\n{}", "-".repeat(text.chars().count().min(78)));
    }
    format!("{BOLD}{UNDERLINE}{text}{RESET}")
}

/// A framed note: something the operator should read once and then forget about.

/// A box, because the thing it says is exactly "this is not going to keep happening" -- an
/// interview that looks like it will be asked again at every command is one an operator dreads,
/// and the shape says otherwise before the words do.
pub(crate) fn note(lines: &[&str]) -> String {
    let width = lines
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    use crate::cli::style::{self, Role};
    let palette = palette();
    let rule = |left: &str, right: &str| {
        style::paint(
            palette,
            Role::Id,
            &format!("{left}{}{right}", "\u{2500}".repeat(width + 2)),
        )
    };
    let bar = style::paint(palette, Role::Id, "\u{2502}");
    let mut out = format!("{}\n", rule("\u{256d}", "\u{256e}"));
    for line in lines {
        let pad = width - line.chars().count();
        out.push_str(&format!("{bar} {line}{} {bar}\n", " ".repeat(pad)));
    }
    out.push_str(&rule("\u{2570}", "\u{256f}"));
    out
}

/// A question, set off from the terminal's own noise: bold, with a mark in front of it.

/// An interview is a wall of prose otherwise, and an operator reading a wall does not read it. The
/// mark carries the meaning where colour is off, exactly as the cursor's own pointer does.
pub(crate) fn heading(text: &str) -> String {
    if crate::cli::no_color_requested() {
        return format!("? {text}");
    }
    format!(
        "{} {BOLD}{text}{RESET}",
        crate::cli::style::paint(palette(), crate::cli::style::Role::Id, "?")
    )
}

/// The line under a question that says why it is being asked. Quieter than the question itself, so
/// the eye lands on what it has to answer.
pub(crate) fn aside(text: &str) -> String {
    if crate::cli::no_color_requested() {
        return format!("  {text}");
    }
    format!(
        "  {}",
        crate::cli::style::paint(palette(), crate::cli::style::Role::Meta, text)
    )
}

/// One field of a result: two spaces, the label, then the value at a fixed column.

/// Every result block in the client uses this, so `address:`, `endpoint:` and `next:` line up
/// wherever they appear. They did not: each block chose its own padding by hand, and the widest
/// label in one block set a column no other block shared.

/// Unused in THIS branch and kept anyway: `wallet_manual.rs` on the open manual-deploy branch
/// calls it, and removing it here made the two branches merge cleanly and then fail to
/// compile. Dead by the compiler's reckoning is not dead across the branches that are in flight.
#[allow(dead_code)]
pub(crate) fn field(label: &str, value: &str) -> String {
    format!(
        "  {:<FIELD_WIDTH$}{value}",
        format!("{label}:"),
        FIELD_WIDTH = FIELD_WIDTH
    )
}

/// A continuation of the field above, aligned under its value rather than under its label.
#[allow(dead_code)]
pub(crate) fn field_continued(value: &str) -> String {
    format!("  {:<FIELD_WIDTH$}{value}", "", FIELD_WIDTH = FIELD_WIDTH)
}

/// Wide enough for the longest label any result uses (`endpoint:`), plus the space after it.
#[allow(dead_code)]
const FIELD_WIDTH: usize = 10;

/// Anything the operator is being asked to DO: an instruction, or the command that carries it out.

/// Amber and bold, the same amber a refusal uses for "this one is yours" and the live line uses for
/// a wait that only a person can end. One colour for one meaning across the whole client: if it is
/// this colour, it is waiting on you.

/// A result is mostly facts -- addresses, paths, figures -- and exactly one or two lines that are
/// not. Those are the lines an operator looks for, and they were set in the same ink as everything
/// around them.

/// Colour is never the only signal: with `NO_COLOR` the words are unchanged and still read as an
/// instruction.
#[allow(dead_code)]
pub(crate) fn action(text: &str) -> String {
    crate::cli::style::action(palette(), text)
}

/// What was chosen, left behind as the record of an answer.
pub(crate) fn answered(text: &str) -> String {
    if crate::cli::no_color_requested() {
        return format!("\u{2714} {text}");
    }
    format!(
        "{} {text}",
        crate::cli::style::paint(palette(), crate::cli::style::Role::Ok, "\u{2714}")
    )
}

/// What one keypress means to a menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Key {
    Up,
    Down,
    Choose,
    Cancel,
    /// Anything else, which a menu ignores rather than guesses at.
    Other,
}

/// Decode one keypress from the bytes a terminal delivers for it.

/// Arrow keys arrive as three bytes (`ESC [ A`), and an application-cursor terminal sends `ESC O A`
/// for the same key; both are accepted. A bare `ESC` is how a menu is left, along with `q` and the
/// Ctrl-C byte -- a raw-mode terminal delivers `0x03` as data rather than as a signal, so the menu
/// has to honour it itself or the operator cannot get out.
pub(crate) fn decode(bytes: &[u8]) -> Key {
    match bytes {
        [0x1b, b'[', b'A'] | [0x1b, b'O', b'A'] | [b'k'] => Key::Up,
        [0x1b, b'[', b'B'] | [0x1b, b'O', b'B'] | [b'j'] => Key::Down,
        [b'\r'] | [b'\n'] => Key::Choose,
        [0x03] | [b'q'] | [0x1b] => Key::Cancel,
        _ => Key::Other,
    }
}

/// Split what one read returned into the keypresses it actually carries.

/// A terminal delivers whatever has arrived, not one key: type ahead, hold a key down, or paste, and
/// several land in one buffer. Decoding the buffer as a single key made every one of those an
/// unrecognised press and threw the lot away -- the menu simply stopped responding.

/// An escape sequence is taken whole (three bytes, or two for a bare `ESC` at the end); everything
/// else is one byte, one key.
pub(crate) fn decode_all(bytes: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let taken = match rest {
            [0x1b, b'[' | b'O', _, ..] => 3,
            _ => 1,
        };
        let taken = taken.min(rest.len());
        keys.push(decode(&rest[..taken]));
        rest = &rest[taken..];
    }
    keys
}

/// The rows and which one is under the cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Menu {
    rows: Vec<String>,
    at: usize,
}

impl Menu {
    pub(crate) fn new(rows: Vec<String>) -> Menu {
        Menu { rows, at: 0 }
    }

    pub(crate) fn at(&self) -> usize {
        self.at
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// Move, and say whether anything changed -- a menu that redraws on a keypress that moved
    /// nothing flickers for no reason.

    /// Deliberately does NOT wrap around. A list of notes is money: an operator holding Down to
    /// reach the last row would sail past it back to the first, and the row under the cursor when
    /// they press Enter is the note that gets spent from.
    pub(crate) fn moved(&mut self, key: Key) -> bool {
        let was = self.at;
        match key {
            Key::Up => self.at = self.at.saturating_sub(1),
            Key::Down if self.at + 1 < self.rows.len() => self.at += 1,
            _ => {}
        }
        self.at != was
    }

    /// The rows as they are drawn: the cursor's row marked, the rest indented to line up under it.

    /// The mark is what carries the meaning -- a screenshot, a log and a reader who cannot see
    /// colour all still show which row is under the cursor. Colour is added on top of it, never
    /// instead of it.
    pub(crate) fn lines(&self) -> Vec<String> {
        self.rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                if index == self.at {
                    format!("\u{276f} {row}")
                } else {
                    format!("  {row}")
                }
            })
            .collect()
    }

    /// The same rows, painted: the chosen one bright, the rest dim.
    pub(crate) fn painted(&self, colour: bool) -> Vec<String> {
        self.lines()
            .into_iter()
            .enumerate()
            .map(|(index, line)| {
                use crate::cli::style::{self, Palette, Role};
                // The caller has already decided whether this draw is painted at all, so the
                // palette is asked with that answer rather than with the environment's. The
                // unpainted rows return untouched -- weight is styling too, and a row that keeps
                // its bold under `NO_COLOR` is still an escape nobody asked for.
                let palette = Palette::resolved(true, !colour);
                if !colour {
                    line
                } else if index == self.at {
                    style::paint(palette, Role::Id, &format!("{BOLD}{line}"))
                } else {
                    style::paint(palette, Role::Meta, &line)
                }
            })
            .collect()
    }
}

/// Ask the operator to pick one row, on the terminal.

/// `None` means they left without choosing -- Esc, `q` or Ctrl-C -- which every caller must treat as
/// "do nothing", never as row zero. Money is on the other side of this function.

/// Reads and writes `/dev/tty` rather than stdin/stdout: stdout is the command's result and may be
/// redirected, and a menu drawn into a pipe is corruption. It is also why this is not offered at all
/// where there is no terminal -- the caller refuses first, naming the flag that carries the answer.
#[cfg(unix)]
pub(crate) fn ask(prompt: &str, rows: Vec<String>) -> anyhow::Result<Option<usize>> {
    use std::io::{Read as _, Write as _};

    if rows.is_empty() {
        return Ok(None);
    }
    // The menu owns the terminal until an answer comes back. Without this the status line's ticker
    // keeps redrawing stderr underneath the rows and the operator picks blind.
    let _hold = crate::cli::progress::hold();
    let mut tty = std::fs::File::options()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|error| anyhow::anyhow!("open the terminal to ask which row: {error}"))?;
    let mut menu = Menu::new(rows);
    let _raw = RawMode::enter(&tty)
        .ok_or_else(|| anyhow::anyhow!("put the terminal in raw mode to read the arrow keys"))?;

    let colour = !crate::cli::no_color_requested();
    if colour {
        writeln!(tty, "{ITALIC}{prompt}{RESET}")?;
    } else {
        writeln!(tty, "{prompt}")?;
    }
    draw(&mut tty, &menu, false, colour)?;
    let mut buffer = [0u8; 8];
    loop {
        let read = tty.read(&mut buffer)?;
        // End of input with nothing chosen is the same answer as Esc: the caller does nothing.
        if read == 0 {
            erase(&mut tty, &menu)?;
            return Ok(None);
        }
        for key in decode_all(&buffer[..read]) {
            match key {
                Key::Choose => {
                    erase(&mut tty, &menu)?;
                    return Ok(Some(menu.at()));
                }
                Key::Cancel => {
                    erase(&mut tty, &menu)?;
                    return Ok(None);
                }
                key => {
                    if menu.moved(key) {
                        draw(&mut tty, &menu, true, colour)?;
                    }
                }
            }
        }
    }
}

/// Ask for a number, with a suggestion the operator takes by pressing Enter.

/// Cooked mode, deliberately: this is typed rather than picked, so the terminal's own line editing --
/// backspace, kill-word -- has to keep working. Refuses below `least` and asks again rather than
/// clamping: a silently corrected number is one the operator believes they chose.
#[cfg(unix)]
pub(crate) fn ask_number(prompt: &str, suggested: u64, least: u64) -> anyhow::Result<u64> {
    use std::io::{BufRead as _, BufReader, Write as _};

    // Same as the menu: the prompt and the typed answer belong to the terminal until Enter, and a
    // spinner redrawing stderr on a timer writes straight over both.
    let _hold = crate::cli::progress::hold();

    let tty = std::fs::File::options()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|error| anyhow::anyhow!("open the terminal to ask for a number: {error}"))?;
    let mut out = tty.try_clone()?;
    let mut lines = BufReader::new(tty).lines();
    loop {
        write!(out, "{prompt} [{suggested}]: ")?;
        out.flush()?;
        let Some(line) = lines.next().transpose()? else {
            // End of input: the suggestion is what was on offer, and taking it is what Enter would
            // have done.
            writeln!(out)?;
            return Ok(suggested);
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            return Ok(suggested);
        }
        match line.parse::<u64>() {
            Ok(value) if value >= least => return Ok(value),
            Ok(_) => writeln!(out, "  it has to be {least} or more.")?,
            Err(_) => writeln!(out, "  a whole number, please.")?,
        }
    }
}

/// Draw the rows with the cursor on its own, leaving the terminal cursor on the LAST row.

/// `redraw` moves back up over the rows already on screen instead of adding new ones: after the
/// first draw this function never emits a newline past the last row, so the screen cannot scroll
/// under it and the upward move stays exact.
#[cfg(unix)]
fn draw(tty: &mut std::fs::File, menu: &Menu, redraw: bool, colour: bool) -> anyhow::Result<()> {
    use std::io::Write as _;

    let lines = menu.painted(colour);
    let mut frame = String::new();
    if redraw && lines.len() > 1 {
        frame.push_str(&format!("\x1b[{}A", lines.len() - 1));
    }
    for (index, line) in lines.iter().enumerate() {
        frame.push_str("\r\x1b[2K");
        frame.push_str(line);
        if index + 1 < lines.len() {
            frame.push('\n');
        }
    }
    tty.write_all(frame.as_bytes())?;
    tty.flush()?;
    Ok(())
}

/// Take the menu off the screen, leaving the cursor where the prompt was.
#[cfg(unix)]
fn erase(tty: &mut std::fs::File, menu: &Menu) -> anyhow::Result<()> {
    use std::io::Write as _;

    let mut frame = String::new();
    if menu.len() > 1 {
        frame.push_str(&format!("\x1b[{}A", menu.len() - 1));
    }
    // Up one more for the prompt, then everything below goes.
    frame.push_str("\x1b[1A\r\x1b[J");
    tty.write_all(frame.as_bytes())?;
    tty.flush()?;
    Ok(())
}

/// Canonical mode and echo off for as long as this is held, so arrow keys arrive as bytes and are
/// not printed. Restored on every exit, panics included.
#[cfg(unix)]
struct RawMode {
    fd: libc::c_int,
    saved: libc::termios,
}

#[cfg(unix)]
impl RawMode {
    fn enter(tty: &std::fs::File) -> Option<RawMode> {
        use std::os::unix::io::AsRawFd as _;

        let fd = tty.as_raw_fd();
        let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr fills the pointed-to termios on success.
        if unsafe { libc::tcgetattr(fd, saved.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: initialised by the successful tcgetattr above.
        let saved = unsafe { saved.assume_init() };
        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        // One byte is enough to wake the read; an escape sequence arrives in one burst and is read
        // whole by the 8-byte buffer above.
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: raw is a valid termios derived from the current settings.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        Some(RawMode { fd, saved })
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restores the termios captured in enter; a failure leaves the terminal as it is,
        // and there is nothing better to do about it.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
    }
}

/// Where there is no `/dev/tty`, asking is not offered -- and saying so is the answer, not a build
/// failure.

/// Both askers read and write the terminal device directly (see `ask`), which is a unix file. The
/// callers are gated on the chain build and not on the platform, so without these arms the Windows
/// release -- `cargo build --release -p dexdo --target x86_64-pc-windows-msvc`,
/// the asset we ship -- does not compile at all. Woodpecker is linux-only and the Windows leg of the
/// GitHub build is default-features, so nothing in CI says a word about it.

/// A refusal is also what the non-interactive path wants: it names the flag that carries the answer,
/// which is exactly what a run with nobody to ask has to be told.
#[cfg(not(unix))]
pub(crate) fn ask(_prompt: &str, _rows: Vec<String>) -> anyhow::Result<Option<usize>> {
    anyhow::bail!(
        "this platform has no terminal device to ask on: pass the answer as a flag instead"
    )
}

/// The typed counterpart of [`ask`] on a platform with no terminal device. See its note.
#[cfg(not(unix))]
pub(crate) fn ask_number(_prompt: &str, _suggested: u64, _least: u64) -> anyhow::Result<u64> {
    anyhow::bail!(
        "this platform has no terminal device to ask on: pass the number as a flag instead"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrows_are_decoded_in_both_cursor_modes() {
        assert_eq!(decode(b"\x1b[A"), Key::Up);
        assert_eq!(decode(b"\x1bOA"), Key::Up);
        assert_eq!(decode(b"\x1b[B"), Key::Down);
        assert_eq!(decode(b"\x1bOB"), Key::Down);
    }

    /// Raw mode delivers Ctrl-C as a byte rather than as a signal, so the menu is what has to let
    /// the operator out. A menu nobody can leave is worse than no menu.
    #[test]
    fn the_ways_out_are_all_decoded() {
        for bytes in [&b"\x03"[..], b"q", b"\x1b"] {
            assert_eq!(decode(bytes), Key::Cancel, "{bytes:?}");
        }
    }

    /// One read is not one key. Holding a key, typing ahead or pasting delivers several at once,
    /// and decoding the buffer as a single press threw all of them away -- the menu stopped
    /// responding until the operator stopped and pressed again.
    #[test]
    fn a_burst_of_keys_is_split_into_the_presses_it_carries() {
        assert_eq!(
            decode_all(b"\x1b[B\x1b[B\r"),
            vec![Key::Down, Key::Down, Key::Choose]
        );
        assert_eq!(decode_all(b"\n\n"), vec![Key::Choose, Key::Choose]);
        assert_eq!(decode_all(b"jjk"), vec![Key::Down, Key::Down, Key::Up]);
        assert_eq!(decode_all(b""), Vec::new());
        // A bare Esc at the end of a burst is still the way out.
        assert_eq!(decode_all(b"j\x1b"), vec![Key::Down, Key::Cancel]);
    }

    #[test]
    fn enter_chooses_and_anything_else_is_ignored() {
        assert_eq!(decode(b"\r"), Key::Choose);
        assert_eq!(decode(b"\n"), Key::Choose);
        assert_eq!(decode(b"x"), Key::Other);
        assert_eq!(decode(b""), Key::Other);
    }

    /// The rule that matters for money: holding a key must not carry the cursor past the end and
    /// round to the other side, where Enter would spend from a note the operator never looked at.
    #[test]
    fn the_cursor_stops_at_both_ends_instead_of_wrapping() {
        let mut menu = Menu::new(vec!["first".into(), "second".into()]);
        assert!(!menu.moved(Key::Up), "already at the top");
        assert_eq!(menu.at(), 0);
        assert!(menu.moved(Key::Down));
        assert_eq!(menu.at(), 1);
        assert!(!menu.moved(Key::Down), "already at the bottom");
        assert_eq!(menu.at(), 1);
    }

    #[test]
    fn a_key_that_moves_nothing_reports_no_change() {
        let mut menu = Menu::new(vec!["only".into()]);
        for key in [Key::Up, Key::Down, Key::Choose, Key::Other, Key::Cancel] {
            assert!(!menu.moved(key), "{key:?}");
        }
    }

    /// Colour is decoration on top of the mark, never a replacement for it: without colour the rows
    /// must still say which one is chosen, and with it nothing else may change.
    /// The box has to close on every side and hold every line: a frame that does not line up reads
    /// as breakage, which is the opposite of what a "you only do this once" note is for.
    /// The text a line carries, with every SGR escape taken out of it.
    fn without_escapes(line: &str) -> String {
        let mut out = String::new();
        let mut rest = line;
        while let Some(start) = rest.find('\u{1b}') {
            out.push_str(&rest[..start]);
            rest = match rest[start..].find('m') {
                Some(end) => &rest[start + end + 1..],
                None => "",
            };
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn a_framed_note_lines_up_on_every_row() {
        let rendered = note(&["Asked once.", "A longer line here."]);
        // Stripped of every escape rather than of one named colour: the frame is drawn in a role
        // now, and which escape a role resolves to depends on what the terminal reports.
        let plain: Vec<String> = rendered.lines().map(without_escapes).collect();
        assert_eq!(plain.len(), 4, "top, two rows, bottom: {plain:?}");
        let width = plain[0].chars().count();
        for line in &plain {
            assert_eq!(line.chars().count(), width, "{line:?}");
        }
        assert!(plain[0].starts_with('\u{256d}') && plain[0].ends_with('\u{256e}'));
        assert!(plain[3].starts_with('\u{2570}') && plain[3].ends_with('\u{256f}'));
        assert!(plain[1].contains("Asked once."));
    }

    /// A section heading and a question must not look alike: reading the introduction as the first
    /// question is what sent an operator answering the wrong thing.
    #[test]
    fn a_title_does_not_look_like_a_question() {
        let title = title("Setting up the buyer rules");
        let question = heading("A seller never sent the connection details.");
        assert!(!title.contains('?'), "{title:?}");
        assert!(question.contains('?'), "{question:?}");
        if crate::cli::no_color_requested() {
            assert_eq!(
                title.lines().count(),
                2,
                "a plain title has its own rule: {title:?}"
            );
            assert_eq!(
                question.lines().count(),
                1,
                "a plain question has no rule: {question:?}"
            );
        } else {
            assert!(
                title.contains(UNDERLINE),
                "a title is underlined: {title:?}"
            );
            assert!(
                !question.contains(UNDERLINE),
                "a question is not: {question:?}"
            );
        }
    }

    /// Every piece of the interview carries its meaning in a mark, so `NO_COLOR` loses styling and
    /// nothing else.
    #[test]
    fn the_interview_reads_the_same_without_colour() {
        assert!(heading("Which note?").contains("Which note?"));
        assert!(heading("Which note?").contains('?'));
        assert!(aside("because").contains("because"));
        assert!(answered("chosen").contains('\u{2714}'));
        // Every colour that is opened is closed again -- an unclosed one paints the rest of the
        // session.
        for rendered in [heading("q"), aside("a"), answered("c")] {
            // Every escape that opens a colour is matched by a reset that closes it; an unclosed
            // one paints the rest of the session.
            let opened = rendered.matches("\x1b[").count();
            let closed = rendered.matches(RESET).count();
            if crate::cli::no_color_requested() {
                assert_eq!(opened, 0, "{rendered:?} paints despite NO_COLOR");
                assert_eq!(closed, 0, "{rendered:?} resets despite NO_COLOR");
            } else {
                assert!(closed > 0, "{rendered:?} has no reset at all");
                assert_eq!(opened, closed * 2, "{rendered:?} leaves colour open");
            }
        }
    }

    #[test]
    fn colour_paints_the_rows_without_changing_which_one_is_marked() {
        let menu = Menu::new(vec!["a".into(), "b".into()]);
        assert_eq!(menu.painted(false), menu.lines());

        let painted = menu.painted(true);
        assert!(painted[0].contains("\u{276f} a"), "{:?}", painted[0]);
        let opener = |role| {
            crate::cli::style::paint(crate::cli::style::Palette::resolved(true, false), role, "")
                .replace(RESET, "")
        };
        assert!(
            painted[0].starts_with(&opener(crate::cli::style::Role::Id)),
            "the chosen row is bright: {:?}",
            painted[0]
        );
        assert!(
            painted[1].starts_with(&opener(crate::cli::style::Role::Meta)),
            "the rest are quiet: {:?}",
            painted[1]
        );
        for line in painted {
            assert!(
                line.ends_with(RESET),
                "every painted line closes its colour"
            );
        }
    }

    #[test]
    fn the_cursor_row_is_the_marked_one() {
        let mut menu = Menu::new(vec!["a".into(), "b".into()]);
        assert_eq!(menu.lines(), vec!["\u{276f} a", "  b"]);
        menu.moved(Key::Down);
        assert_eq!(menu.lines(), vec!["  a", "\u{276f} b"]);
    }

    /// A menu with nothing in it must not report a selection: the caller has to say "no notes"
    /// rather than hand back row zero of an empty list.
    #[test]
    fn an_empty_menu_draws_nothing_and_moves_nowhere() {
        let mut menu = Menu::new(Vec::new());
        assert_eq!(menu.len(), 0);
        assert!(menu.lines().is_empty());
        assert!(!menu.moved(Key::Down));
    }
}
