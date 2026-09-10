//! QR output that upgrades itself to a real image when the terminal can show one.

//! The two invitation flows (`wallet onboard` and the Gosh.ai provider) print a QR the user must
//! scan with a phone camera. Unicode pseudo-graphics survive everywhere but scan poorly on small
//! cells and dark themes; kitty-, iTerm2- and sixel-capable terminals can show a crisp bitmap.

//! Detection is probe-first, environment-second (agreed design):

//! 1. An active capability probe writes a kitty graphics query (`ESC _G... a=q... ESC \`)
//! followed by DA1 (`CSI c`) to the controlling terminal and reads the answer with a short
//! timeout. Every terminal answers DA1, so it doubles as the stop marker; a kitty `OK` reply
//! proves the kitty protocol, DA1 attribute `4` proves sixel.
//! 2. Environment variables answer only when the probe could not (`KITTY_WINDOW_ID`,
//! `TERM_PROGRAM`,...).

//! Inside a multiplexer or an embedded terminal neither source is trusted -- see
//! [`TerminalEnv::mux`] for the measurement behind that rule.

//! Anything short of a confirmed protocol falls back to the unicode rendering this module
//! replaced, and `DEXDO_QR` (`auto|text|kitty|iterm2|sixel`) forces either direction.

//! The parsing, the protocol decision and the three encoders are pure and compile under every
//! test build (the `wallet_manual` pattern); the probe compiles there too so every CI OS
//! type-checks its platform half, and only the `qrcode` entry point sits behind the chain build.

//! NOTE: `cli` is mounted from `main.rs`, so this file's tests run under
//! `cargo test -p dexdo --bin dexdo` -- a `--lib` filter matches nothing and reports "ok".

use base64::Engine as _;

/// What the capability probe's answer stream contained.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProbeReply {
    /// The terminal answered the kitty graphics query with `OK`.
    pub(crate) kitty: bool,
    /// DA1 attributes, present once the terminal answered `CSI c`.
    pub(crate) da1: Option<Vec<u16>>,
    /// `(width, height)` of one character cell in pixels, from the terminal's `CSI 16 t` answer.
    /// The OS often does not know it (`TIOCGWINSZ` reports zeroes), and it is what decides how
    /// large an image may be; a terminal that ignores the request simply leaves this `None`.
    pub(crate) cell_px: Option<(usize, usize)>,
}

impl ProbeReply {
    /// DA1 attribute `4` is the sixel advertisement (DEC STD-070).
    pub(crate) fn sixel(&self) -> bool {
        self.da1.as_ref().is_some_and(|attrs| attrs.contains(&4))
    }

    /// DA1 is sent after the kitty query and answered by every terminal, so its arrival means the
    /// probe has everything it will ever get.
    pub(crate) fn complete(&self) -> bool {
        self.da1.is_some()
    }
}

/// Scan the probe answer for the kitty reply and DA1, at ANY position.

/// The read races the operator's fingers: type-ahead pressed while the probe is listening lands
/// in the same buffer as the answers. Prefix-scanning instead of strict matching survives that
/// garbage -- bytes before, between and after the recognised sequences are ignored.
pub(crate) fn parse_probe_reply(bytes: &[u8]) -> ProbeReply {
    ProbeReply {
        kitty: kitty_confirmed(bytes),
        da1: find_da1(bytes),
        cell_px: find_cell_size(bytes),
    }
}

/// The `CSI 16 t` answer is `CSI 6; height; width t` (XTWINOPS), pixels of one cell.

/// Scanned at any offset like the other answers, and required to end in `t`: `CSI 6;... R` is
/// a cursor report, which carries rows and columns and would be read as a comically small cell.
fn find_cell_size(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut rest = bytes;
    while let Some(start) = find(rest, b"\x1b[6;") {
        let body = &rest[start + 4..];
        match body
            .iter()
            .position(|&byte| !(byte.is_ascii_digit() || byte == b';'))
        {
            Some(end) if body[end] == b't' => {
                let mut fields = body[..end]
                    .split(|&byte| byte == b';')
                    .filter_map(|field| std::str::from_utf8(field).ok()?.parse::<usize>().ok());
                let height = fields.next()?;
                let width = fields.next()?;
                // A zero either way is "unknown", and dividing a budget by it is worse than the
                // assumption it would replace.
                return (height > 0 && width > 0).then_some((width, height));
            }
            Some(end) => rest = &body[end..],
            // The parameters ran off the end of the buffer: the answer is still incomplete.
            None => return None,
        }
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A kitty reply is `ESC _ G <id-keys>; <answer> ESC \`, and only `OK` is support: a terminal
/// that answers but rejects the probe's format (`;EINVAL...`) cannot show the QR either.
fn kitty_confirmed(bytes: &[u8]) -> bool {
    let mut rest = bytes;
    while let Some(start) = find(rest, b"\x1b_G") {
        let body = &rest[start + 3..];
        // Unterminated: the reply is still in flight, and counts for nothing yet.
        let Some(end) = find(body, b"\x1b\\") else {
            return false;
        };
        if body[..end]
            .split(|&byte| byte == b';')
            .nth(1)
            .is_some_and(|answer| answer.starts_with(b"OK"))
        {
            return true;
        }
        rest = &body[end + 2..];
    }
    false
}

/// DA1 is `ESC [ ? <attr>; <attr>... c`. Other `CSI ?` replies (DECRPM and friends) end in a
/// different final byte, so a candidate that runs into one is skipped, not misread.
fn find_da1(bytes: &[u8]) -> Option<Vec<u16>> {
    let mut rest = bytes;
    while let Some(start) = find(rest, b"\x1b[?") {
        let body = &rest[start + 3..];
        match body
            .iter()
            .position(|&byte| !(byte.is_ascii_digit() || byte == b';'))
        {
            Some(end) if body[end] == b'c' => {
                return Some(
                    body[..end]
                        .split(|&byte| byte == b';')
                        .filter_map(|attr| std::str::from_utf8(attr).ok()?.parse::<u16>().ok())
                        .collect(),
                );
            }
            Some(end) => rest = &body[end..],
            // Parameters ran off the end of the buffer: the answer is still incomplete.
            None => return None,
        }
    }
    None
}

/// The chosen way to put this QR on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QrDisplay {
    Kitty,
    Iterm2,
    Sixel,
    /// The unicode pseudo-graphics fallback.
    Text,
}

/// The environment half of the decision, captured once so the choice itself stays pure.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TerminalEnv {
    /// `DEXDO_QR` parsed: `Some` forces one rendering -- the escape hatch when detection guesses
    /// wrong, and the debug forcing. `None` (unset, `auto`, or an unrecognised value, which must
    /// not silently disable the QR) lets detection decide.
    pub(crate) force: Option<QrDisplay>,
    /// Both stdout and stdin are terminals; without this, auto mode stays with text (a forced
    /// protocol still wins -- it exists to debug exactly these situations).
    pub(crate) tty: bool,
    /// Inside a terminal multiplexer or an embedded terminal (tmux, screen, zellij, nvim's
    /// `:terminal`, emacs vterm), where nothing heard through the intermediary predicts what the
    /// operator sees. Measured on tmux 3.4 with a byte-logging client tmux itself had marked
    /// `sixel`-capable in `#{client_termfeatures}`: tmux advertises DA1 attribute 4 and ACCEPTS
    /// sixel, yet re-emits none of it -- panes draw an unscannable `+`-placeholder grid. The
    /// environment is no better, because the outer terminal's identity variables leak into panes
    /// whose escape protocols are not passed through. Auto mode therefore stays with text here;
    /// `DEXDO_QR` remains the override for an intermediary known to pass images on.
    pub(crate) mux: bool,
    /// The kitty protocol advertised by identity: kitty, Ghostty, or Konsole >= 22.04.
    pub(crate) kitty_env: bool,
    /// The iTerm2 protocol advertised by identity: iTerm2 (also over ssh) or WezTerm.
    pub(crate) iterm2_env: bool,
}

impl TerminalEnv {
    /// Build from an environment lookup, kept injectable so the rules are testable.
    pub(crate) fn from_lookup(lookup: impl Fn(&str) -> Option<String>, tty: bool) -> TerminalEnv {
        let var = |key: &str| lookup(key).filter(|value| !value.is_empty());
        let term = var("TERM").unwrap_or_default();
        let term_program = var("TERM_PROGRAM").unwrap_or_default();
        TerminalEnv {
            force: match var("DEXDO_QR").as_deref() {
                Some("text") => Some(QrDisplay::Text),
                Some("kitty") => Some(QrDisplay::Kitty),
                Some("iterm2") => Some(QrDisplay::Iterm2),
                Some("sixel") => Some(QrDisplay::Sixel),
                _ => None,
            },
            tty,
            mux: var("TMUX").is_some()
                || var("ZELLIJ").is_some()
                || var("NVIM").is_some()
                || var("INSIDE_EMACS").is_some()
                || term.starts_with("screen")
                || term.starts_with("tmux"),
            kitty_env: var("KITTY_WINDOW_ID").is_some()
                || term == "xterm-kitty"
                || term == "xterm-ghostty"
                || var("GHOSTTY_RESOURCES_DIR").is_some()
                // Konsole gained the kitty graphics protocol in 22.04.
                || var("KONSOLE_VERSION")
                    .and_then(|version| version.parse::<u32>().ok())
                    .is_some_and(|version| version >= 220400),
            iterm2_env: term_program == "iTerm.app"
                || term_program == "WezTerm"
                // Over ssh iTerm2 propagates LC_TERMINAL, not TERM_PROGRAM.
                || var("LC_TERMINAL").as_deref() == Some("iTerm2"),
        }
    }
}

/// The one decision: probe first, environment as fallback, text when nothing is proven.

/// A forced protocol wins even over the tty check -- it exists to debug exactly the situations
/// detection refuses.
pub(crate) fn choose_display(env: &TerminalEnv, probe: &ProbeReply) -> QrDisplay {
    if let Some(forced) = env.force {
        return forced;
    }
    // A mux outranks even a positive probe answer: display, not parsing, is what fails there
    // (see [`TerminalEnv::mux`]); in production the probe is not even run in that case
    // ([`probe_can_matter`]), this keeps the rule true regardless of the caller.
    if !env.tty || env.mux {
        return QrDisplay::Text;
    }
    if probe.kitty {
        return QrDisplay::Kitty;
    }
    if probe.sixel() {
        return QrDisplay::Sixel;
    }
    // Every kitty-graphics terminal answers the kitty query, so a probe that COMPLETED (DA1 came
    // back) without the OK is direct negative evidence that outranks an identity variable. iTerm2
    // has no query to decline, so its identity keeps counting either way.
    if env.kitty_env && !probe.complete() {
        return QrDisplay::Kitty;
    }
    if env.iterm2_env {
        return QrDisplay::Iterm2;
    }
    QrDisplay::Text
}

/// What the window can spare for the image.

/// `rows`/`cols` come from the OS (`TIOCGWINSZ` and its Windows counterpart), which every
/// terminal fills in; the pixel size of one character cell is the datum that is often missing --
/// WezTerm under WSL, for one, reports `(45, 190, 0, 0)`. It is taken from the OS when non-zero,
/// from the terminal's own `CSI 16 t` answer when it gives one, and assumed otherwise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TerminalGeometry {
    pub(crate) rows: usize,
    pub(crate) cols: usize,
    /// `(width, height)` of one character cell in pixels.
    pub(crate) cell: Option<(usize, usize)>,
}

/// The largest whole number of pixels per module that keeps a `modules`-wide image (quiet zone
/// included) inside the window's share, clamped to the scale range.

/// Whole numbers only: fractional scaling, or handing the resize to the terminal through kitty's
/// `c=`/`r=` or iTerm2's `width=`, interpolates the module edges, and a blurred QR is one a phone
/// gives up on. Smaller with crisp edges beats bigger and smeared.
pub(crate) fn fit_module_scale(modules: usize, geometry: &TerminalGeometry) -> usize {
    use dexdo_core::params::{
        QR_IMAGE_ASSUMED_CELL_HEIGHT, QR_IMAGE_ASSUMED_CELL_WIDTH, QR_IMAGE_MODULE_SCALE_MAX,
        QR_IMAGE_MODULE_SCALE_MIN, QR_IMAGE_WINDOW_HEIGHT_PERCENT,
    };

    // Nothing usable was reported: invent no constraint rather than a wrong one.
    if modules == 0 || geometry.rows == 0 || geometry.cols == 0 {
        return QR_IMAGE_MODULE_SCALE_MAX;
    }
    let (cell_width, cell_height) = geometry
        .cell
        .unwrap_or((QR_IMAGE_ASSUMED_CELL_WIDTH, QR_IMAGE_ASSUMED_CELL_HEIGHT));
    let height_budget = geometry.rows * QR_IMAGE_WINDOW_HEIGHT_PERCENT / 100 * cell_height;
    let width_budget = geometry.cols * cell_width;
    (height_budget.min(width_budget) / modules)
        .clamp(QR_IMAGE_MODULE_SCALE_MIN, QR_IMAGE_MODULE_SCALE_MAX)
}

/// Whether running the capability probe can change [`choose_display`]'s answer at all: with a
/// forced rendering, a non-tty, or an intermediary in between, it cannot -- and the probe's cost
/// (a raw-mode timeout window) should then not be paid. The io half consults this instead of
/// re-deriving the rule.
pub(crate) fn probe_can_matter(env: &TerminalEnv) -> bool {
    env.force.is_none() && env.tty && !env.mux
}

/// The QR as pixels: modules scaled up and wrapped in the light quiet zone the standard requires
/// for scanning (4 modules; the caller passes it explicitly so tests stay small).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QrBitmap {
    width: usize,
    height: usize,
    /// Row-major; `true` is a dark pixel.
    dark: Vec<bool>,
}

impl QrBitmap {
    pub(crate) fn from_modules(
        modules: &[bool],
        modules_wide: usize,
        scale: usize,
        quiet_zone: usize,
    ) -> QrBitmap {
        // Zero-size inputs cannot come from a real QR; answering with an empty bitmap instead of
        // dividing by zero keeps the function total.
        if modules_wide == 0 || scale == 0 {
            return QrBitmap {
                width: 0,
                height: 0,
                dark: vec![],
            };
        }
        let modules_high = modules.len() / modules_wide;
        let width = (modules_wide + 2 * quiet_zone) * scale;
        let height = (modules_high + 2 * quiet_zone) * scale;
        let mut dark = vec![false; width * height];
        for module_y in 0..modules_high {
            for module_x in 0..modules_wide {
                if !modules[module_y * modules_wide + module_x] {
                    continue;
                }
                for pixel_y in 0..scale {
                    let y = (module_y + quiet_zone) * scale + pixel_y;
                    let row = y * width + (module_x + quiet_zone) * scale;
                    dark[row..row + scale].fill(true);
                }
            }
        }
        QrBitmap {
            width,
            height,
            dark,
        }
    }

    fn is_dark(&self, x: usize, y: usize) -> bool {
        self.dark[y * self.width + x]
    }
}

fn grayscale_of(bitmap: &QrBitmap) -> Vec<u8> {
    let mut gray = Vec::with_capacity(bitmap.width * bitmap.height);
    for y in 0..bitmap.height {
        for x in 0..bitmap.width {
            gray.push(if bitmap.is_dark(x, y) { 0u8 } else { 255u8 });
        }
    }
    gray
}

/// The kitty graphics escape: raw RGB pixels, base64, in the protocol's 4096-byte chunks.
pub(crate) fn encode_kitty(bitmap: &QrBitmap) -> String {
    let rgb: Vec<u8> = grayscale_of(bitmap)
        .into_iter()
        .flat_map(|value| [value, value, value])
        .collect();
    // Chunked on the INPUT at 3072 bytes rather than on the base64 text: 3072 is both a multiple
    // of 3 -- so every chunk but the last encodes without padding and the concatenation is the
    // base64 of the whole image -- and exactly the 4096 output characters the kitty protocol
    // fixes as its chunk size (not a tunable). Encoding each chunk to its own `String` also
    // leaves no byte slice to convert back into text.
    let mut chunks: Vec<String> = rgb
        .chunks(3072)
        .map(|chunk| base64::engine::general_purpose::STANDARD.encode(chunk))
        .collect();
    // An empty image (empty bitmap) still emits one chunk, keeping the function total.
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    let last = chunks.len() - 1;
    let mut out = String::new();
    for (index, chunk) in chunks.iter().enumerate() {
        if index == 0 {
            // `q=2` suppresses the terminal's transmission responses, which would otherwise land
            // in the same tty queue later readers (the hidden wallet prompt) consume.
            out.push_str(&format!(
                "\x1b_Ga=T,q=2,f=24,s={},v={}",
                bitmap.width, bitmap.height
            ));
            if last > 0 {
                out.push_str(",m=1");
            }
        } else {
            out.push_str(&format!("\x1b_Gm={}", u8::from(index != last)));
        }
        out.push(';');
        out.push_str(chunk);
        out.push_str("\x1b\\");
    }
    out
}

/// The iTerm2 inline-image escape: a grayscale PNG in the `OSC 1337 File` wrapper.
pub(crate) fn encode_iterm2(bitmap: &QrBitmap) -> anyhow::Result<String> {
    use anyhow::Context as _;

    let mut png_bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut png_bytes, bitmap.width as u32, bitmap.height as u32);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("write the QR png header")?;
    writer
        .write_image_data(&grayscale_of(bitmap))
        .context("write the QR png pixels")?;
    writer.finish().context("finish the QR png")?;
    let payload = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    Ok(format!(
        "\x1b]1337;File=inline=1;size={}:{payload}\x07",
        png_bytes.len()
    ))
}

/// A monochrome sixel: white pass, carriage return, black pass, per 6-row band.

/// The white pass is not optional decoration: sixel leaves unpainted pixels at the terminal's
/// background colour, and a QR on a dark theme without its painted-white quiet zone does not scan.
pub(crate) fn encode_sixel(bitmap: &QrBitmap) -> String {
    let mut out = format!(
        "\x1bPq\"1;1;{};{}\x230;2;100;100;100\x231;2;0;0;0",
        bitmap.width, bitmap.height
    );
    for band_top in (0..bitmap.height).step_by(6) {
        if band_top > 0 {
            out.push('-');
        }
        out.push_str("\x230");
        sixel_band_pass(&mut out, bitmap, band_top, false);
        out.push_str("$\x231");
        sixel_band_pass(&mut out, bitmap, band_top, true);
    }
    out.push_str("\x1b\\");
    out
}

/// One colour's pixels of one 6-row band, run-length encoded with the `!` repeat introducer.
fn sixel_band_pass(out: &mut String, bitmap: &QrBitmap, band_top: usize, wants_dark: bool) {
    let mut run: Option<(char, usize)> = None;
    let flush = |run: Option<(char, usize)>, out: &mut String| {
        let Some((sixel, count)) = run else { return };
        if count >= 4 {
            out.push('!');
            out.push_str(&count.to_string());
            out.push(sixel);
        } else {
            out.extend(std::iter::repeat_n(sixel, count));
        }
    };
    for x in 0..bitmap.width {
        let mut bits = 0u8;
        for (bit, y) in (band_top..bitmap.height.min(band_top + 6)).enumerate() {
            if bitmap.is_dark(x, y) == wants_dark {
                bits |= 1 << bit;
            }
        }
        let sixel = char::from(63 + bits);
        match &mut run {
            Some((current, count)) if *current == sixel => *count += 1,
            _ => {
                flush(run.take(), out);
                run = Some((sixel, 1));
            }
        }
    }
    flush(run, out);
}

/// The active capability probe: ask the terminal itself, before believing any variable.

/// Compiled wherever the tests are, so the cross-OS CI matrix type-checks both platform halves
/// (Windows included, which nothing on a Linux host can cross-check -- the chain graph's C
/// build scripts need MSVC tooling); it RUNS only from the chain entry point below.
mod probe {
    use dexdo_core::params::QR_PROBE_TIMEOUT;

    use super::ProbeReply;

    /// Three questions in one round trip, answered in order:

    /// - the kitty graphics query (`a=q`, one dummy RGB pixel; `i=31` because the protocol
    /// addresses replies by id -- the parser itself accepts any `ESC _G` reply, ours being the
    /// only query in flight);
    /// - `CSI 16 t`, the cell size in pixels, which decides how large an image may be and which
    /// the OS frequently does not know (`TIOCGWINSZ` reports zeroes under WSL);
    /// - DA1, the stop marker every terminal answers, and therefore last.
    const QUERY: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[16t\x1b[c";

    /// Ask the terminal what it can show. Every failure -- no controlling terminal, no raw mode,
    /// no answer, a write error -- is the same empty reply, and the caller falls back to the
    /// environment and to unicode.
    pub(super) fn run() -> ProbeReply {
        imp::probe(QUERY, QR_PROBE_TIMEOUT).unwrap_or_default()
    }

    #[cfg(unix)]
    mod imp {
        use std::fs::File;
        use std::io::{Read as _, Write as _};
        use std::os::unix::io::AsRawFd as _;
        use std::time::{Duration, Instant};

        use super::super::{parse_probe_reply, ProbeReply};

        /// Canonical mode and echo off, so the answer's bytes arrive unbuffered and unseen;
        /// everything is restored on drop, early returns and panics included.
        struct RawMode {
            fd: libc::c_int,
            saved: libc::termios,
        }

        impl RawMode {
            fn enter(fd: libc::c_int) -> Option<RawMode> {
                let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
                // SAFETY: tcgetattr fills the pointed-to termios on success.
                if unsafe { libc::tcgetattr(fd, saved.as_mut_ptr()) } != 0 {
                    return None;
                }
                // SAFETY: initialised by the successful tcgetattr above.
                let saved = unsafe { saved.assume_init() };
                let mut raw = saved;
                raw.c_lflag &= !(libc::ICANON | libc::ECHO);
                // Reads return after 0.1s with whatever arrived, keeping the wait loop bounded
                // without extra polling machinery.
                raw.c_cc[libc::VMIN] = 0;
                raw.c_cc[libc::VTIME] = 1;
                // SAFETY: raw is a valid termios derived from the current settings.
                if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
                    return None;
                }
                Some(RawMode { fd, saved })
            }
        }

        impl Drop for RawMode {
            fn drop(&mut self) {
                // SAFETY: restores the termios captured in enter; a failure here leaves the
                // terminal as it is, and there is nothing better to do about it.
                unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
            }
        }

        pub(super) fn probe(query: &[u8], timeout: Duration) -> Option<ProbeReply> {
            // The controlling terminal, NOT stdin: the hidden wallet prompts (rpassword) read
            // `/dev/tty` as their own file description, and `StdinLock` is an 8 KiB BufReader
            // whose over-read would strand bytes -- the operator's type-ahead -- where no later
            // reader could ever see them. One unbuffered fd carries both the query and the
            // answer and leaves the stdin stream untouched.
            let mut tty = File::options()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .ok()?;
            let _raw = RawMode::enter(tty.as_raw_fd())?;
            tty.write_all(query).ok()?;
            let deadline = Instant::now() + timeout;
            // The drain budget mirrors the answer budget: after the stop marker, reading goes on
            // until one whole VTIME window passes silently, so a trailing burst (a slow link's
            // late kitty reply) is consumed here instead of surfacing inside the wallet's hidden
            // prompt. Type-ahead spent inside these windows is lost -- the price every active
            // prober pays.
            let hard_stop = deadline + timeout;
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 256];
            loop {
                // VTIME above bounds this read to 0.1s of silence.
                let read = tty.read(&mut chunk).ok()?;
                buffer.extend_from_slice(&chunk[..read]);
                let reply = parse_probe_reply(&buffer);
                let now = Instant::now();
                if reply.complete() {
                    if read == 0 || now >= hard_stop {
                        return Some(reply);
                    }
                } else if now >= deadline {
                    return Some(reply);
                }
            }
        }
    }

    #[cfg(windows)]
    mod imp {
        use std::io::Write as _;
        use std::time::{Duration, Instant};

        use dexdo_core::params::QR_PROBE_POLL_INTERVAL;
        use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetNumberOfConsoleInputEvents, GetStdHandle, ReadConsoleInputW,
            SetConsoleMode, CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
            ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
            ENABLE_VIRTUAL_TERMINAL_PROCESSING, INPUT_RECORD, KEY_EVENT, STD_INPUT_HANDLE,
            STD_OUTPUT_HANDLE,
        };

        use super::super::{parse_probe_reply, ProbeReply};

        /// One console handle's mode, restored on drop.
        struct ModeGuard {
            handle: HANDLE,
            saved: CONSOLE_MODE,
        }

        impl ModeGuard {
            fn set(
                handle: HANDLE,
                change: impl FnOnce(CONSOLE_MODE) -> CONSOLE_MODE,
            ) -> Option<ModeGuard> {
                let mut saved: CONSOLE_MODE = 0;
                // SAFETY: GetConsoleMode writes the mode on success; SetConsoleMode takes a
                // plain value. Both are no-ops on failure.
                if unsafe { GetConsoleMode(handle, &mut saved) } == 0
                    || unsafe { SetConsoleMode(handle, change(saved)) } == 0
                {
                    return None;
                }
                Some(ModeGuard { handle, saved })
            }
        }

        impl Drop for ModeGuard {
            fn drop(&mut self) {
                // SAFETY: restores the mode captured in set; nothing to do about failure.
                unsafe { SetConsoleMode(self.handle, self.saved) };
            }
        }

        pub(super) fn probe(query: &[u8], timeout: Duration) -> Option<ProbeReply> {
            // SAFETY: GetStdHandle takes a constant and returns a handle or a sentinel.
            let stdin = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
            // SAFETY: same contract for the output handle.
            let stdout_handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
            if stdin.is_null()
                || stdin == INVALID_HANDLE_VALUE
                || stdout_handle.is_null()
                || stdout_handle == INVALID_HANDLE_VALUE
            {
                return None;
            }
            // VT input turns the terminal's answer into readable key events; line buffering and
            // echo off so it is neither held until Enter nor shown.
            let _stdin_mode = ModeGuard::set(stdin, |mode| {
                (mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
                    | ENABLE_VIRTUAL_TERMINAL_INPUT
            })?;
            // VT processing on stdout is switched on and deliberately NOT restored: the image
            // escape is written after this function returns, and turning the flag back off would
            // print it as literal glyphs. Leaving it enabled is the conventional one-way switch
            // for a CLI that emits escapes.
            let mut stdout_mode: CONSOLE_MODE = 0;
            // SAFETY: GetConsoleMode writes the mode on success; SetConsoleMode takes a value.
            unsafe {
                if GetConsoleMode(stdout_handle, &mut stdout_mode) != 0 {
                    SetConsoleMode(
                        stdout_handle,
                        stdout_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                    );
                }
            }
            {
                let mut stdout = std::io::stdout().lock();
                stdout.write_all(query).ok()?;
                stdout.flush().ok()?;
            }
            let deadline = Instant::now() + timeout;
            // Same drain rule as the unix half: after the stop marker, keep consuming until the
            // queue is empty, bounded by a mirror of the answer budget.
            let hard_stop = deadline + timeout;
            let mut buffer = Vec::new();
            loop {
                let mut pending: u32 = 0;
                // SAFETY: writes the queue length on success.
                if unsafe { GetNumberOfConsoleInputEvents(stdin, &mut pending) } == 0 {
                    return None;
                }
                let reply = parse_probe_reply(&buffer);
                let now = Instant::now();
                if pending == 0 {
                    if reply.complete() || now >= deadline {
                        return Some(reply);
                    }
                    std::thread::sleep(QR_PROBE_POLL_INTERVAL);
                    continue;
                }
                if reply.complete() && now >= hard_stop {
                    return Some(reply);
                }
                // SAFETY: INPUT_RECORD is plain data; zeroed is a valid representation.
                let mut records: [INPUT_RECORD; 16] = unsafe { std::mem::zeroed() };
                let mut read: u32 = 0;
                // SAFETY: the buffer really holds 16 records; read receives the count.
                if unsafe { ReadConsoleInputW(stdin, records.as_mut_ptr(), 16, &mut read) } == 0 {
                    return None;
                }
                for record in &records[..read as usize] {
                    if u32::from(record.EventType) != KEY_EVENT as u32 {
                        continue;
                    }
                    // SAFETY: EventType selects the KeyEvent member of the union.
                    let key = unsafe { record.Event.KeyEvent };
                    if key.bKeyDown == 0 {
                        continue;
                    }
                    // SAFETY: uChar is a union of one u16 viewed as UTF-16 or bytes.
                    let unit = unsafe { key.uChar.UnicodeChar };
                    // The probe's answers are pure ASCII; anything wider is the operator typing,
                    // which the parser skips anyway, but it must not be folded into a fake byte.
                    if unit != 0 && unit <= 0x7F {
                        buffer.push(unit as u8);
                    }
                }
            }
        }
    }
}

pub(crate) use io::write_qr;

/// The the chain build half: the probe-driven choice and the actual writing. Everything above is
/// pure or probe-only; this half talks to `qrcode` and the caller's writer, and exists only next
/// to its callers.
mod io {
    use std::io::IsTerminal as _;

    use anyhow::Result;
    use qrcode::QrCode;

    use super::{
        choose_display, encode_iterm2, encode_kitty, encode_sixel, fit_module_scale, probe,
        probe_can_matter, ProbeReply, QrBitmap, QrDisplay, TerminalEnv, TerminalGeometry,
    };

    /// The QR standard's quiet zone (ISO/IEC 18004): 4 light modules on every side. A protocol
    /// fact like kitty's 4096 chunk, not a tunable -- hence not in `params.rs`.
    const QUIET_ZONE: usize = 4;

    /// Write `code` to `output`: an image when the terminal proved it can show one, PR1362's
    /// unicode pseudo-graphics otherwise. Every rendering misfortune degrades to that text.

    /// Everything goes through the caller's writer, never `println!`: the invitation output is
    /// pinned by tests that read it back from a `Vec<u8>`, and a renderer that wrote to stdout
    /// behind their backs would make those tests stop seeing what the operator sees. Detection
    /// still asks the real terminal -- a captured writer means a captured stdout, which is not a
    /// tty, which selects the text rendering anyway.

    /// Blocks the calling thread for up to two probe timeouts. Both callers are interactive
    /// commands that go on to wait for the operator, so a briefly blocked tokio worker is
    /// accepted over threading `spawn_blocking` through their synchronous call chains.
    pub(crate) fn write_qr(output: &mut dyn std::io::Write, code: &QrCode) -> Result<()> {
        let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        let env = TerminalEnv::from_lookup(|key| std::env::var(key).ok(), tty);
        let probe = if probe_can_matter(&env) {
            probe::run()
        } else {
            ProbeReply::default()
        };
        let chosen = choose_display(&env, &probe);
        // The terminal's own answer outranks the OS here: `TIOCGWINSZ` reports no pixels at all
        // on WezTerm under WSL, where `CSI 16 t` does answer.
        let mut geometry = window_geometry();
        if probe.cell_px.is_some() {
            geometry.cell = probe.cell_px;
        }
        let scale = fit_module_scale(code.width() + 2 * QUIET_ZONE, &geometry);
        // The one failure mode this feature has is "a picture is there but does not scan"; keep
        // what was decided, and from which evidence, reachable after the fact.
        tracing::debug!(display = ?chosen, scale, ?geometry, ?env, ?probe, "QR display decision");
        match chosen {
            QrDisplay::Text => write_text(output, code)?,
            display => match render_image(code, display, scale) {
                Ok(escape) => {
                    writeln!(output, "{escape}")?;
                    writeln!(
                        output,
                        "(QR shown as an image; if it does not scan, rerun with DEXDO_QR=text)"
                    )?;
                }
                // A cosmetic failure must not abort an onboarding: fall back, but say so.
                Err(error) => {
                    tracing::warn!("QR image rendering failed, falling back to text: {error:#}");
                    write_text(output, code)?;
                }
            },
        }
        Ok(())
    }

    fn render_image(code: &QrCode, display: QrDisplay, scale: usize) -> Result<String> {
        let modules: Vec<bool> = code
            .to_colors()
            .iter()
            .map(|color| *color == qrcode::Color::Dark)
            .collect();
        let bitmap = QrBitmap::from_modules(&modules, code.width(), scale, QUIET_ZONE);
        match display {
            QrDisplay::Kitty => Ok(encode_kitty(&bitmap)),
            QrDisplay::Iterm2 => encode_iterm2(&bitmap),
            QrDisplay::Sixel => Ok(encode_sixel(&bitmap)),
            // The caller routes text before it gets here, so this is our own mistake -- and the
            // module's contract is that only an unencodable payload is fatal. Refusing sends it
            // back through the caller's error arm, which prints the text QR.
            QrDisplay::Text => Err(anyhow::anyhow!(
                "the text rendering has no image encoder; it is written directly"
            )),
        }
    }

    /// What the OS knows about the window. Rows and columns are always filled in; the pixel
    /// fields are optional in the interface and commonly zero, which is reported as "unknown"
    /// rather than as a cell of no size.
    #[cfg(unix)]
    fn window_geometry() -> TerminalGeometry {
        // SAFETY: winsize is plain data; zeroed is a valid representation, and the ioctl fills
        // it in on success.
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: the pointer is to the local winsize above, which outlives the call.
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } != 0 {
            return TerminalGeometry::default();
        }
        TerminalGeometry {
            rows: size.ws_row as usize,
            cols: size.ws_col as usize,
            cell: (size.ws_xpixel > 0 && size.ws_ypixel > 0 && size.ws_col > 0 && size.ws_row > 0)
                .then(|| {
                    (
                        (size.ws_xpixel / size.ws_col) as usize,
                        (size.ws_ypixel / size.ws_row) as usize,
                    )
                }),
        }
    }

    /// The Windows console reports the window in cells only; the cell's pixel size comes from
    /// the terminal's `CSI 16 t` answer or the assumption.
    #[cfg(windows)]
    fn window_geometry() -> TerminalGeometry {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Console::{
            GetConsoleScreenBufferInfo, GetStdHandle, CONSOLE_SCREEN_BUFFER_INFO, STD_OUTPUT_HANDLE,
        };

        // SAFETY: GetStdHandle takes a constant and returns a handle or a sentinel.
        let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return TerminalGeometry::default();
        }
        // SAFETY: CONSOLE_SCREEN_BUFFER_INFO is plain data; zeroed is a valid representation.
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: the pointer is to the local info above, which outlives the call.
        if unsafe { GetConsoleScreenBufferInfo(handle, &mut info) } == 0 {
            return TerminalGeometry::default();
        }
        // The visible window, not the scrollback buffer: srWindow is inclusive on both ends.
        TerminalGeometry {
            rows: (info.srWindow.Bottom - info.srWindow.Top + 1).max(0) as usize,
            cols: (info.srWindow.Right - info.srWindow.Left + 1).max(0) as usize,
            cell: None,
        }
    }

    /// The text fallback, for every terminal that did not prove it can show an image.

    /// `qr_compact` rather than this module's own `Dense1x2` line: the same one-module-per-column
    /// shape, two fewer modules of quiet zone on each side, and a polarity the rendering paints
    /// rather than inherits from the colour scheme. Colour is emitted only to a real terminal, so
    /// captured output stays plain.
    fn write_text(output: &mut dyn std::io::Write, code: &QrCode) -> Result<()> {
        let colour = std::io::stdout().is_terminal();
        crate::cli::qr_compact::write(output, code, colour)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn write_qr_without_a_tty_keeps_the_unicode_rendering() {
            // What every captured-output caller gets, and what PR1362's invitation tests read
            // back: the same unicode block rendering, into the writer, with no escapes.
            let code = QrCode::new(b"dexdo").expect("payload fits");
            let mut output = Vec::new();
            write_qr(&mut output, &code).expect("write");
            let text = String::from_utf8(output).expect("UTF-8");
            assert!(text.contains('\u{2588}'), "{text}");
            assert!(
                !text.contains('\x1b'),
                "no escape may reach a captured writer"
            );
        }

        #[test]
        fn render_image_refuses_the_text_rendering_instead_of_panicking() {
            // `Text` reaching here is our own mistake, and the module's contract is that only an
            // unencodable payload is fatal: a future path that calls this directly must get the
            // text QR through the caller's error arm, not a panic in the middle of an onboarding.
            let code = QrCode::new(b"dexdo").expect("payload fits");
            assert!(render_image(&code, QrDisplay::Text, 8).is_err());
        }

        #[test]
        fn render_image_frames_a_real_qr_with_scale_and_quiet_zone() {
            // The only place `to_colors()` meets `from_modules`: verify the wiring against a
            // real QrCode -- the frame is (modules + 2 quiet zones) scaled, and the first pixel
            // row is quiet-zone white (base64 of 0xFF RGB rows starts with `/`).
            let code = QrCode::new(b"dexdo").expect("payload fits");
            let expected = (code.width() + 2 * QUIET_ZONE) * 8;
            let escape = render_image(&code, QrDisplay::Kitty, 8).expect("encode");
            assert!(escape.starts_with(&format!("\x1b_Ga=T,q=2,f=24,s={expected},v={expected}")));
            let payload = escape.split_once(';').expect("payload").1;
            assert!(
                payload.starts_with("////"),
                "quiet zone must open with white pixels"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- parse_probe_reply -----------------------------------------------------------------

    #[test]
    fn parse_reply_kitty_ok_and_da1_with_sixel() {
        let reply = parse_probe_reply(b"\x1b_Gi=31;OK\x1b\\\x1b[?62;4;22c");
        assert!(reply.kitty);
        assert_eq!(reply.da1, Some(vec![62, 4, 22]));
        assert!(reply.sixel());
        assert!(reply.complete());
    }

    #[test]
    fn parse_reply_da1_only_without_sixel() {
        let reply = parse_probe_reply(b"\x1b[?1;2c");
        assert!(!reply.kitty);
        assert_eq!(reply.da1, Some(vec![1, 2]));
        assert!(!reply.sixel());
        assert!(reply.complete());
    }

    #[test]
    fn parse_reply_survives_keypress_garbage() {
        // Keys pressed during the probe: bytes before the kitty reply and between it and DA1.
        let reply = parse_probe_reply(b"qw\x1b_Gi=31;OK\x1b\\ab\x1b[?64;4c");
        assert!(reply.kitty);
        assert!(reply.sixel());
        assert!(reply.complete());
    }

    #[test]
    fn parse_reply_kitty_error_is_not_support() {
        // A terminal that answers the query but rejects the format must not be taken for kitty.
        let reply = parse_probe_reply(b"\x1b_Gi=31;EINVAL:unsupported\x1b\\\x1b[?6c");
        assert!(!reply.kitty);
        assert!(reply.complete());
    }

    #[test]
    fn parse_reply_incomplete_until_da1() {
        // No DA1 yet: the reader must keep waiting, and an unterminated kitty reply counts for
        // nothing.
        let reply = parse_probe_reply(b"\x1b_Gi=31;OK");
        assert!(!reply.kitty);
        assert!(!reply.complete());
    }

    #[test]
    fn parse_reply_empty_is_incomplete() {
        assert_eq!(parse_probe_reply(b""), ProbeReply::default());
    }

    #[test]
    fn parse_reply_reads_the_cell_size_as_width_by_height() {
        // XTWINOPS answers `CSI 6; height; width t` -- height first, which is the easy half to
        // get backwards and would then shrink the image against the wrong axis.
        let reply = parse_probe_reply(b"\x1b[6;17;8t\x1b[?62;4c");
        assert_eq!(reply.cell_px, Some((8, 17)));
    }

    #[test]
    fn parse_reply_cell_size_survives_garbage_and_other_csi_answers() {
        let reply = parse_probe_reply(b"x\x1b[6;1;1R\x1b_Gi=31;OK\x1b\\\x1b[6;32;16tz\x1b[?1;2c");
        assert_eq!(reply.cell_px, Some((16, 32)));
        assert!(reply.kitty);
        assert!(reply.complete());
    }

    #[test]
    fn parse_reply_without_a_cell_answer_reports_none() {
        // The common case: the terminal ignores `CSI 16 t` and only answers DA1.
        assert_eq!(parse_probe_reply(b"\x1b[?1;2c").cell_px, None);
    }

    // -- choose_display --------------------------------------------------------------------

    fn plain_tty() -> TerminalEnv {
        TerminalEnv {
            tty: true,
            ..TerminalEnv::default()
        }
    }

    fn probe_with(kitty: bool, da1: &[u16]) -> ProbeReply {
        ProbeReply {
            kitty,
            da1: Some(da1.to_vec()),
            cell_px: None,
        }
    }

    #[test]
    fn force_text_wins_over_everything() {
        let env = TerminalEnv {
            force: Some(QrDisplay::Text),
            kitty_env: true,
            ..plain_tty()
        };
        assert_eq!(
            choose_display(&env, &probe_with(true, &[4])),
            QrDisplay::Text
        );
    }

    #[test]
    fn force_protocol_overrides_probe_and_tty() {
        // The forced protocol exists to debug exactly the situations detection refuses, so it
        // wins even without a tty.
        let env = TerminalEnv {
            force: Some(QrDisplay::Sixel),
            ..TerminalEnv::default()
        };
        assert_eq!(
            choose_display(&env, &probe_with(true, &[])),
            QrDisplay::Sixel
        );
    }

    #[test]
    fn non_tty_is_text() {
        let env = TerminalEnv {
            kitty_env: true,
            ..TerminalEnv::default()
        };
        assert_eq!(
            choose_display(&env, &probe_with(true, &[4])),
            QrDisplay::Text
        );
    }

    #[test]
    fn probe_kitty_beats_sixel_and_env() {
        let env = TerminalEnv {
            iterm2_env: true,
            ..plain_tty()
        };
        assert_eq!(
            choose_display(&env, &probe_with(true, &[4])),
            QrDisplay::Kitty
        );
    }

    #[test]
    fn probe_sixel_without_kitty() {
        assert_eq!(
            choose_display(&plain_tty(), &probe_with(false, &[62, 4])),
            QrDisplay::Sixel
        );
    }

    #[test]
    fn env_kitty_when_probe_silent() {
        let env = TerminalEnv {
            kitty_env: true,
            ..plain_tty()
        };
        assert_eq!(
            choose_display(&env, &ProbeReply::default()),
            QrDisplay::Kitty
        );
    }

    #[test]
    fn env_kitty_is_refuted_by_a_completed_probe() {
        // Every kitty-graphics terminal answers the kitty query, so a probe that came back (DA1
        // present) without the OK is direct negative evidence: the identity variable is then a
        // leak from some outer terminal, not a capability of the one listening.
        let env = TerminalEnv {
            kitty_env: true,
            ..plain_tty()
        };
        assert_eq!(
            choose_display(&env, &probe_with(false, &[1, 2])),
            QrDisplay::Text
        );
    }

    #[test]
    fn env_iterm2_when_probe_names_no_protocol() {
        // The probe completed (DA1 came back) but proved nothing; iTerm2 has no query protocol,
        // so its environment identity must still count.
        let env = TerminalEnv {
            iterm2_env: true,
            ..plain_tty()
        };
        assert_eq!(
            choose_display(&env, &probe_with(false, &[1, 2])),
            QrDisplay::Iterm2
        );
    }

    #[test]
    fn mux_suppresses_env_fallback() {
        let env = TerminalEnv {
            mux: true,
            kitty_env: true,
            iterm2_env: true,
            ..plain_tty()
        };
        assert_eq!(
            choose_display(&env, &probe_with(false, &[1])),
            QrDisplay::Text
        );
    }

    #[test]
    fn mux_probe_answers_are_still_text() {
        // See [`TerminalEnv::mux`] for the measurement: nothing heard through an intermediary
        // predicts what the operator sees, so neither a sixel-advertising DA1 nor even a kitty
        // OK may upgrade the output there.
        let env = TerminalEnv {
            mux: true,
            ..plain_tty()
        };
        assert_eq!(
            choose_display(&env, &probe_with(false, &[1, 2, 4])),
            QrDisplay::Text
        );
        assert_eq!(
            choose_display(&env, &probe_with(true, &[4])),
            QrDisplay::Text
        );
    }

    #[test]
    fn nothing_proven_is_text() {
        assert_eq!(
            choose_display(&plain_tty(), &ProbeReply::default()),
            QrDisplay::Text
        );
    }

    #[test]
    fn probe_matters_only_for_auto_tty_outside_mux() {
        assert!(probe_can_matter(&plain_tty()));
        assert!(!probe_can_matter(&TerminalEnv {
            force: Some(QrDisplay::Kitty),
            ..plain_tty()
        }));
        assert!(!probe_can_matter(&TerminalEnv::default()));
        assert!(!probe_can_matter(&TerminalEnv {
            mux: true,
            ..plain_tty()
        }));
    }

    // -- TerminalEnv::from_lookup ----------------------------------------------------------

    fn lookup_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| owned.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    #[test]
    fn lookup_detects_multiplexers_and_embedded_terminals() {
        let mux = |pairs: &[(&str, &str)]| TerminalEnv::from_lookup(lookup_of(pairs), true).mux;
        assert!(mux(&[("TMUX", "/tmp/tmux-1000/default,42,0")]));
        assert!(mux(&[("TERM", "screen-256color")]));
        assert!(mux(&[("TERM", "tmux-256color")]));
        // The same leak exists outside tmux: none of these pass graphics through, and all leave
        // the outer terminal's identity variables intact.
        assert!(mux(&[("ZELLIJ", "0")]));
        assert!(mux(&[("NVIM", "/run/user/1000/nvim.sock")]));
        assert!(mux(&[("INSIDE_EMACS", "30.1,vterm")]));
        assert!(!mux(&[("TERM", "xterm-256color")]));
    }

    #[test]
    fn lookup_detects_kitty_identities() {
        assert!(TerminalEnv::from_lookup(lookup_of(&[("KITTY_WINDOW_ID", "1")]), true).kitty_env);
        assert!(TerminalEnv::from_lookup(lookup_of(&[("TERM", "xterm-kitty")]), true).kitty_env);
        assert!(TerminalEnv::from_lookup(lookup_of(&[("TERM", "xterm-ghostty")]), true).kitty_env);
        assert!(
            TerminalEnv::from_lookup(
                lookup_of(&[("GHOSTTY_RESOURCES_DIR", "/usr/share/ghostty")]),
                true
            )
            .kitty_env
        );
        // Konsole gained the kitty protocol in 22.04.
        assert!(
            TerminalEnv::from_lookup(lookup_of(&[("KONSOLE_VERSION", "230801")]), true).kitty_env
        );
        assert!(
            !TerminalEnv::from_lookup(lookup_of(&[("KONSOLE_VERSION", "211203")]), true).kitty_env
        );
    }

    #[test]
    fn lookup_detects_iterm2_identities() {
        assert!(
            TerminalEnv::from_lookup(lookup_of(&[("TERM_PROGRAM", "iTerm.app")]), true).iterm2_env
        );
        assert!(
            TerminalEnv::from_lookup(lookup_of(&[("TERM_PROGRAM", "WezTerm")]), true).iterm2_env
        );
        // Over ssh iTerm2 propagates LC_TERMINAL, not TERM_PROGRAM.
        assert!(TerminalEnv::from_lookup(lookup_of(&[("LC_TERMINAL", "iTerm2")]), true).iterm2_env);
        assert!(
            !TerminalEnv::from_lookup(lookup_of(&[("TERM_PROGRAM", "Apple_Terminal")]), true)
                .iterm2_env
        );
    }

    #[test]
    fn lookup_parses_dexdo_qr_force() {
        let force =
            |value: &str| TerminalEnv::from_lookup(lookup_of(&[("DEXDO_QR", value)]), true).force;
        assert_eq!(force("text"), Some(QrDisplay::Text));
        assert_eq!(force("kitty"), Some(QrDisplay::Kitty));
        assert_eq!(force("iterm2"), Some(QrDisplay::Iterm2));
        assert_eq!(force("sixel"), Some(QrDisplay::Sixel));
        assert_eq!(force("auto"), None);
        // Unrecognised values must not disable the QR: detection decides.
        assert_eq!(force("mystery"), None);
        assert_eq!(TerminalEnv::from_lookup(lookup_of(&[]), true).force, None);
    }

    #[test]
    fn lookup_records_ttyness() {
        assert!(TerminalEnv::from_lookup(lookup_of(&[]), true).tty);
        assert!(!TerminalEnv::from_lookup(lookup_of(&[]), false).tty);
    }

    // -- fit_module_scale ------------------------------------------------------------------

    /// A window that reports its cell, so the arithmetic is exact.
    fn window(rows: usize, cols: usize) -> TerminalGeometry {
        TerminalGeometry {
            rows,
            cols,
            cell: Some((8, 16)),
        }
    }

    #[test]
    fn a_small_code_keeps_the_maximum_scale() {
        // The Gosh.ai placeholder: 25 modules + 2x4 quiet zone. Nothing to shrink.
        assert_eq!(fit_module_scale(33, &window(45, 190)), 8);
    }

    #[test]
    fn a_large_code_shrinks_to_the_window_share() {
        // The bee deep link: 97 modules + 2x4 quiet zone = 105. 45 rows * 66% * 16px = 464px of
        // budget, so 4px per module (420px) fits and 5 (525px) does not.
        assert_eq!(fit_module_scale(105, &window(45, 190)), 4);
    }

    #[test]
    fn a_short_window_clamps_at_the_minimum() {
        // 10 rows * 66% * 16px = 96px cannot hold 105 modules at any usable scale; the floor is
        // an image that overflows a little, never one too fine to scan.
        assert_eq!(fit_module_scale(105, &window(10, 190)), 2);
    }

    #[test]
    fn a_narrow_window_binds_before_the_height() {
        // 40 columns * 8px = 320px of width against 45 rows * 66% * 16px = 464px of height.
        assert_eq!(fit_module_scale(105, &window(45, 40)), 3);
    }

    #[test]
    fn an_unreported_cell_falls_back_to_the_assumed_one() {
        // WezTerm under WSL reports (45, 190, 0, 0): rows and columns, no pixels. The assumed
        // 8x16 must produce the same answer as if the terminal had said so.
        let assumed = TerminalGeometry {
            rows: 45,
            cols: 190,
            cell: None,
        };
        assert_eq!(
            fit_module_scale(105, &assumed),
            fit_module_scale(105, &window(45, 190))
        );
    }

    #[test]
    fn a_reported_cell_is_used_over_the_assumption() {
        // A 32px-tall cell (HiDPI) doubles the pixel budget of the same 45 rows, and the scale
        // follows it up to the maximum.
        let hidpi = TerminalGeometry {
            rows: 45,
            cols: 190,
            cell: Some((16, 32)),
        };
        assert_eq!(fit_module_scale(105, &hidpi), 8);
    }

    #[test]
    fn an_unknown_window_keeps_the_maximum() {
        // No window report at all (not a tty, or an OS that answered nothing): invent no limit.
        assert_eq!(fit_module_scale(105, &TerminalGeometry::default()), 8);
    }

    // -- QrBitmap --------------------------------------------------------------------------

    #[test]
    fn bitmap_scales_modules_and_adds_quiet_zone() {
        // A 3x1 strip [dark, light, dark], scale 2, quiet zone 1 module. Deliberately
        // asymmetric: a transposed implementation would produce 6x10 instead of 10x6 and move
        // every dark run off these coordinates.
        let bitmap = QrBitmap::from_modules(&[true, false, true], 3, 2, 1);
        assert_eq!((bitmap.width, bitmap.height), (10, 6));
        for x in 0..10 {
            assert!(
                !bitmap.is_dark(x, 0) && !bitmap.is_dark(x, 5),
                "quiet rows must stay light"
            );
        }
        for y in 0..6 {
            assert!(
                !bitmap.is_dark(0, y) && !bitmap.is_dark(9, y),
                "quiet columns must stay light"
            );
        }
        // Module 0 covers x{2,3}, module 1 x{4,5}, module 2 x{6,7} -- all in rows {2,3}.
        assert!(bitmap.is_dark(2, 2) && bitmap.is_dark(3, 3));
        assert!(!bitmap.is_dark(4, 2) && !bitmap.is_dark(5, 3));
        assert!(bitmap.is_dark(6, 2) && bitmap.is_dark(7, 3));
    }

    #[test]
    fn bitmap_of_no_modules_is_empty_not_a_panic() {
        let empty = QrBitmap::from_modules(&[], 0, 8, 4);
        assert_eq!((empty.width, empty.height), (0, 0));
        assert!(empty.dark.is_empty());
    }

    // -- encode_kitty ----------------------------------------------------------------------

    /// The payloads of every `ESC _G...;<payload> ESC \` chunk, in order, with their key parts.
    fn kitty_chunks(encoded: &str) -> Vec<(String, String)> {
        encoded
            .split("\x1b\\")
            .filter(|part| !part.is_empty())
            .map(|part| {
                let body = part
                    .strip_prefix("\x1b_G")
                    .expect("every chunk starts ESC _G");
                let (keys, payload) = body.split_once(';').expect("every chunk has a payload");
                (keys.to_string(), payload.to_string())
            })
            .collect()
    }

    #[test]
    fn kitty_single_chunk_is_exact() {
        // 2x1 [dark, light], hand-encoded: pixels 000000 ffffff -> base64 "AAAA////". The
        // expectation is a literal, so an x/y swap anywhere in the pipeline fails on `s=2,v=1`
        // alone; `q=2` keeps the terminal from answering the transmission (see finding on the
        // response queue).
        let bitmap = QrBitmap {
            width: 2,
            height: 1,
            dark: vec![true, false],
        };
        assert_eq!(
            encode_kitty(&bitmap),
            "\x1b_Ga=T,q=2,f=24,s=2,v=1;AAAA////\x1b\\"
        );
    }

    #[test]
    fn kitty_three_chunks_mark_middle_and_last() {
        // 64x40 px all dark -> 7680 raw bytes -> 10240 base64 chars -> 4096+4096+2048. The
        // middle chunk is the dominant kind for a real QR (53+ chunks at production scale) and
        // must carry m=1 exactly like the first.
        let bitmap = QrBitmap::from_modules(&[true; 640], 32, 2, 0);
        let chunks = kitty_chunks(&encode_kitty(&bitmap));
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].0, "a=T,q=2,f=24,s=64,v=40,m=1");
        assert_eq!(chunks[1].0, "m=1");
        assert_eq!(chunks[2].0, "m=0");
        assert!(chunks.iter().all(|(_, payload)| payload.len() <= 4096));
        let whole: String = chunks.iter().map(|(_, payload)| payload.as_str()).collect();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(whole)
            .expect("concatenated payloads are base64");
        assert_eq!(decoded, vec![0u8; 64 * 40 * 3]);
    }

    #[test]
    fn kitty_empty_bitmap_is_a_single_empty_chunk_not_a_panic() {
        let empty = QrBitmap {
            width: 0,
            height: 0,
            dark: vec![],
        };
        assert_eq!(encode_kitty(&empty), "\x1b_Ga=T,q=2,f=24,s=0,v=0;\x1b\\");
    }

    // -- encode_iterm2 ---------------------------------------------------------------------

    #[test]
    fn iterm2_wraps_a_png_that_round_trips() {
        // 3x2 with dark corners on opposite ends; the expectation is a hand-written literal, so
        // a transposed implementation fails on the 3x2 header before the pixels.
        let bitmap = QrBitmap {
            width: 3,
            height: 2,
            dark: vec![true, false, false, false, false, true],
        };
        let encoded = encode_iterm2(&bitmap).expect("encode");
        let body = encoded
            .strip_prefix("\x1b]1337;File=inline=1;size=")
            .expect("iTerm2 OSC prefix");
        let body = body.strip_suffix('\x07').expect("BEL terminator");
        let (size, payload) = body.split_once(':').expect("size:payload");
        let png_bytes = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .expect("payload is base64");
        assert_eq!(
            size.parse::<usize>().expect("size is a number"),
            png_bytes.len()
        );

        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        let mut reader = decoder.read_info().expect("valid png");
        let mut pixels = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut pixels).expect("decode frame");
        assert_eq!((info.width, info.height), (3, 2));
        assert_eq!(info.color_type, png::ColorType::Grayscale);
        assert_eq!(&pixels[..info.buffer_size()], &[0, 255, 255, 255, 255, 0]);
    }

    // -- encode_sixel ----------------------------------------------------------------------

    #[test]
    fn sixel_paints_white_then_black_per_band() {
        // 4x2: dark corners on the top row, dark middle on the bottom row.
        // col: 0 1 2 3
        // row 0: D L L D
        // row 1: L D D L
        // Column bits (bit0 = top row): white pass A@@A, black pass @AA@.
        let bitmap = QrBitmap {
            width: 4,
            height: 2,
            dark: vec![true, false, false, true, false, true, true, false],
        };
        assert_eq!(
            encode_sixel(&bitmap),
            "\x1bPq\"1;1;4;2\x230;2;100;100;100\x231;2;0;0;0\x230A@@A$\x231@AA@\x1b\\"
        );
    }

    #[test]
    fn sixel_run_length_encodes_from_four() {
        // 6x1 all dark: six identical columns compress to !6, on both passes.
        let bitmap = QrBitmap {
            width: 6,
            height: 1,
            dark: vec![true; 6],
        };
        assert_eq!(
            encode_sixel(&bitmap),
            "\x1bPq\"1;1;6;1\x230;2;100;100;100\x231;2;0;0;0\x230!6?$\x231!6@\x1b\\"
        );
    }

    #[test]
    fn sixel_bands_are_separated_by_line_feeds() {
        // 1x12 all dark: two full bands of one column each, joined by `-`.
        let bitmap = QrBitmap {
            width: 1,
            height: 12,
            dark: vec![true; 12],
        };
        assert_eq!(
            encode_sixel(&bitmap),
            "\x1bPq\"1;1;1;12\x230;2;100;100;100\x231;2;0;0;0\x230?$\x231~-\x230?$\x231~\x1b\\"
        );
    }
}
