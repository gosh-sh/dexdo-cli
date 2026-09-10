//! Names in the client's output that are worth clicking.

//! An operator told to confirm a transfer "in Acki Nacki Wallet" has to know what that is and where
//! to get it. Terminals have carried hyperlinks since 2017 (OSC 8): the text stays the text, and
//! clicking it opens the address. Where the terminal is not the destination -- a pipe, a log file, a
//! terminal that does not support it -- the same call renders the plain words, so nothing is lost
//! and no escape reaches a file.

//! Kept in one place so the name and its address cannot drift apart across the modules that print
//! it, and so a message can be written as one sentence rather than assembled around an escape.

/// Where the wallet application lives.
const WALLET_APP_URL: &str = "https://ackinacki.com/wallet";

/// The wallet's name, clickable where that means anything.
pub(crate) fn wallet_app() -> String {
    linked(
        "Acki Nacki Wallet",
        WALLET_APP_URL,
        destination_is_a_terminal(),
    )
}

/// `text` as a link to `url`, or as itself.

/// OSC 8 is `ESC ] 8; params; url ST text ESC ] 8;; ST`. A terminal that does not know the
/// sequence shows the text and swallows the rest; one that does makes the text clickable. The
/// closing pair with an empty url is required -- without it every later line is part of the link.
pub(crate) fn linked(text: &str, url: &str, clickable: bool) -> String {
    if !clickable {
        return text.to_string();
    }
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// Both streams the client writes prose on. A link is emitted only when whatever reads this run is
/// a terminal; `NO_COLOR` is honoured too, because an operator who asked for plain output means it.
fn destination_is_a_terminal() -> bool {
    use std::io::IsTerminal as _;

    // stderr as the operator had it, because descriptor 2 is borrowed mid-run by the prover's fold
    // and a link decided against that pipe would be dropped on a real terminal.
    (crate::cli::interaction::screen_is_terminal() || std::io::stdout().is_terminal())
        && !crate::cli::no_color_requested()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The escape has to be complete: an unterminated OSC 8 turns the whole rest of the session
    /// into one link, which is worse than no link at all.
    #[test]
    fn a_link_opens_and_closes_around_its_text() {
        let rendered = linked("Acki Nacki Wallet", WALLET_APP_URL, true);
        assert_eq!(
            rendered,
            "\x1b]8;;https://ackinacki.com/wallet\x1b\\Acki Nacki Wallet\x1b]8;;\x1b\\"
        );
        assert!(rendered.ends_with("\x1b]8;;\x1b\\"), "the link must close");
    }

    /// Anywhere that is not a terminal gets the words and not one escape byte: a log file, a pipe
    /// and a machine consumer all read the same bytes.
    #[test]
    fn without_a_terminal_it_is_just_the_words() {
        let rendered = linked("Acki Nacki Wallet", WALLET_APP_URL, false);
        assert_eq!(rendered, "Acki Nacki Wallet");
        assert!(!rendered.contains('\x1b'));
    }

    /// Under `cargo test` neither stream is a terminal, so the production call renders plain -- the
    /// same thing a redirected run gets.
    #[test]
    fn the_wallet_name_is_plain_where_nothing_can_be_clicked() {
        assert_eq!(wallet_app(), "Acki Nacki Wallet");
    }
}
