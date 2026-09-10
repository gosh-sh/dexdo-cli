//! The smallest terminal QR the wallet onboarding invitation can be shown as.

//! The onboarding operator sits at a workstation while the wallet is a phone application, so the
//! scannable code is the only artefact of the invitation that is usable where it is printed. This
//! module is the rendering for every terminal that cannot show an image; where one can,
//! [`crate::cli::qr_display`] sends an inline image instead and the module size stops being the
//! font size. Both go to the same stream -- no window, no file.

//! What makes it small is the symbol, not how many modules are crammed into a character.

//! A terminal cell is about twice as tall as it is wide, so the only packing that leaves a module
//! SQUARE is one module per column and two per row: half blocks. Denser cells exist -- quadrants
//! carry 2x2 modules and braille 2x4 -- and they buy their narrowness by stretching the symbol
//! vertically by the cell's own aspect. That distortion is visible at a glance and is not
//! something a decoder should be asked to undo; a code that is narrow and does not scan is not
//! smaller, it is broken.

//! So the size comes from the symbol:

//! 1. **The lowest error correction the format has.** A code that lives on screen for one scan
//! does not need redundancy against print damage, and `L` costs several versions less than the
//! crate's default `M` -- each version removed is four modules off both axes.
//! 2. **A two-module quiet zone**, rather than the four a printed symbol asks for.

//! Past that, the symbol is as large as the invitation makes it, and the invitation is built by
//! the bee node rather than here.

//! Polarity is not left to the terminal's colour scheme. A QR decoder expects dark modules on a
//! light field, and a light-on-dark theme would invert exactly that, so the rendering paints its
//! own black-on-white and restores the terminal at the end of every line.

use anyhow::{Context as _, Result};
use qrcode::{EcLevel, QrCode};
use std::io::Write;

/// Modules of light field around the symbol. The format asks for four; on a screen, where there
/// is no print bleed and the operator can move the camera, two scan reliably and take four
/// columns and two rows off the rendering.
const QUIET_ZONE: usize = 2;

/// Black on white, set per line so a wrapped or interrupted line cannot leak the colours into the
/// rest of the session.
const PAINT: &str = "\x1b[107;30m";
const RESET: &str = "\x1b[0m";

/// The four vertical halves of a cell, indexed by `top | bottom<<1`.
const HALVES: [char; 4] = [' ', '\u{2580}', '\u{2584}', '\u{2588}'];

/// Encode `payload` into the smallest symbol that holds it.
pub(crate) fn smallest_code(payload: &[u8]) -> Result<QrCode> {
    QrCode::with_error_correction_level(payload, EcLevel::L)
        .context("the bee connection deep link does not fit a QR code")
}

/// Write `code` to `output`. `colour` paints the light field white and the modules black; pass
/// `false` when the destination is not a terminal, so captured or redirected output stays plain.
pub(crate) fn write(output: &mut dyn Write, code: &QrCode, colour: bool) -> Result<()> {
    let dark = dark_grid(code);
    let columns = code.width() + 2 * QUIET_ZONE;
    for top in (0..dark.len()).step_by(2) {
        let mut line = String::with_capacity(columns);
        for (&upper, &lower) in dark[top].iter().zip(&dark[top + 1]).take(columns) {
            let index = usize::from(upper) | usize::from(lower) << 1;
            line.push(HALVES[index]);
        }
        if colour {
            writeln!(output, "{PAINT}{line}{RESET}")?;
        } else {
            writeln!(output, "{line}")?;
        }
    }
    Ok(())
}

/// The symbol as a grid of "is this module dark", padded with the quiet zone and, when the side is
/// odd, one further light ROW so the last cell has both of its halves. Columns are never padded:
/// one module is one column, and a spare one would widen every line past the symbol.
fn dark_grid(code: &QrCode) -> Vec<Vec<bool>> {
    let width = code.width();
    let colors = code.to_colors();
    let side = width + 2 * QUIET_ZONE;
    let mut grid = vec![vec![false; side]; side + side % 2];
    for row in 0..width {
        for column in 0..width {
            if colors[row * width + column] == qrcode::Color::Dark {
                grid[row + QUIET_ZONE][column + QUIET_ZONE] = true;
            }
        }
    }
    grid
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rendering must stay solid: a decoder reads contiguous dark area, so every glyph the
    /// table can emit is either blank or a full block element.
    #[test]
    fn every_glyph_is_a_block_element_or_a_space() {
        for glyph in HALVES {
            assert!(
                glyph == ' ' || ('\u{2580}'..='\u{259F}').contains(&glyph),
                "{glyph:?} is not a block element"
            );
        }
    }

    /// The index arithmetic and the table must agree, or the symbol is drawn upside down and
    /// nothing scans.
    #[test]
    fn halves_table_matches_its_index_arithmetic() {
        assert_eq!(HALVES[0], ' ');
        assert_eq!(HALVES[1], '\u{2580}');
        assert_eq!(HALVES[2], '\u{2584}');
        assert_eq!(HALVES[3], '\u{2588}');
    }

    /// The module has to stay square. A terminal cell is about twice as tall as it is wide, so one
    /// module per column and two per row is the packing that does not stretch the symbol; anything
    /// denser trades a scannable code for a narrow one.
    #[test]
    fn one_module_per_column_and_two_per_row_keeps_the_module_square() {
        let code = smallest_code(b"dexdo").expect("payload fits");
        let side = code.width() + 2 * QUIET_ZONE;
        let columns = side;
        let rows = side.div_ceil(2);
        assert_eq!(columns, side, "one module per column");
        assert_eq!(rows, side.div_ceil(2), "two modules per row");

        let mut output = Vec::new();
        write(&mut output, &code, false).expect("write");
        let text = String::from_utf8(output).expect("UTF-8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), rows);
        for line in lines {
            assert_eq!(line.chars().count(), columns, "{line:?}");
        }
    }

    /// The lowest error correction is what keeps the symbol small; a change of default in the
    /// encoder must not silently grow it back.
    #[test]
    fn lowest_error_correction_keeps_the_symbol_smaller_than_the_crate_default() {
        let payload = b"https://links.gosh.sh/deeplinks/wallet/v1/connect?payload=".repeat(6);
        let ours = smallest_code(&payload).expect("payload fits");
        let default = QrCode::new(&payload).expect("payload fits");
        assert!(
            ours.width() < default.width(),
            "ours={} default={}",
            ours.width(),
            default.width()
        );
    }

    /// Colour is per line and always closed, so an interrupted print cannot leave the terminal
    /// painted.
    #[test]
    fn colour_is_opened_and_closed_on_every_line() {
        let code = smallest_code(b"dexdo").expect("payload fits");
        let mut output = Vec::new();
        write(&mut output, &code, true).expect("write");
        let text = String::from_utf8(output).expect("UTF-8");
        for line in text.lines() {
            assert!(line.starts_with(PAINT), "{line:?}");
            assert!(line.ends_with(RESET), "{line:?}");
        }
        assert_eq!(text.matches(PAINT).count(), text.matches(RESET).count());
    }

    /// Plain output carries no escapes at all, which is what captured and redirected output gets.
    #[test]
    fn plain_output_has_no_escapes() {
        let code = smallest_code(b"dexdo").expect("payload fits");
        let mut output = Vec::new();
        write(&mut output, &code, false).expect("write");
        let text = String::from_utf8(output).expect("UTF-8");
        assert!(!text.contains('\x1b'), "{text:?}");
    }
}
