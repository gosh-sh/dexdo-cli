//! Reading a function's own text in a test, without an anchor that can be deleted.

//! A dozen tests in this crate guard ORDER rather than values: the owner key is checked before the
//! withdraw is submitted, the registry is asked before anything is paid for, the recovery
//! checkpoint is written before the spend. Order is not observable from a return value, so these
//! tests read the function's source text with `include_str!` and compare the offsets of two calls.

//! That much is sound. What was not sound is how they decided where the function ENDS. They looked
//! for the next `#[cfg(not(feature = "net-a"))]` -- the stub that used to sit after each of
//! these functions -- and sliced up to it.

//! Removing the cargo features deleted every one of those stubs, and ten tests started failing with
//! `run_note_withdraw cfg end present`. Not one of them was about the feature: the guard they
//! protect was untouched. They failed because their end marker was a neighbour, and a neighbour can
//! be deleted by work that has nothing to do with the test.

//! The end of a function is its closing brace, and nothing else. This scans for it. The cost of the
//! alternative is measured: ten tests that assert nothing for as long as it takes someone to notice
//! that `expect("... end present")` is a missing anchor, not a failed guard.

/// The body of the item whose declaration begins at `signature`, from its opening brace to the
/// matching close.

/// Panics with a message naming what was not found, because every caller is a test whose next line
/// would otherwise search an empty string and report a guard as missing.
#[cfg(test)]
pub(crate) fn body_of<'a>(source: &'a str, signature: &str) -> &'a str {
    // THE FIRST OCCURRENCE IS NOT ALWAYS THE ITEM. A test that names its target verbatim is an
    // earlier occurrence of the same text whenever the test module sits above the code, and
    // `commands.rs` is such a file. Measured there, twice: anchored on `fn tell(self)` this read
    // its own string literal and returned the next line; anchored on `impl RegistryAnswer {` it
    // opened a brace inside a literal and died on "the braces do not balance". Both were the
    // scanner pointing at the test instead of at the code, and neither said so.

    // A declaration STARTS a line, after indentation. An occurrence inside a line is inside a
    // string, a comment or a longer path, and is not what the caller meant.
    let start = source
        .match_indices(signature)
        .map(|(at, _)| at)
        .find(|at| {
            let line_start = source[..*at].rfind('\n').map_or(0, |n| n + 1);
            source[line_start..*at].trim().is_empty()
        })
        .unwrap_or_else(|| {
            panic!(
                "no item declared as `{signature}` in this source (it appears {} time(s), never \
                 at the start of a line, so every one of them is inside a string, a comment or a \
                 longer path)",
                source.matches(signature).count()
            )
        });
    let rest = &source[start..];
    let open = rest
        .find('{')
        .unwrap_or_else(|| panic!("`{signature}` has no body: no opening brace after it"));

    let bytes = rest.as_bytes();
    let mut depth = 0usize;
    let mut index = open;
    while index < bytes.len() {
        // Skip anything that can hold a brace without being one: a comment, a string, a char.
        // Without this, one `//` line saying "closes the { above" ends the body early, and the test
        // silently narrows to a fragment where its second call no longer appears.
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += rest[index..]
                    .find('\n')
                    .map_or(bytes.len() - index, |n| n + 1);
                continue;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                // Nested, because Rust nests them. Stopping at the first `*/` resumes INSIDE the
                // outer comment, where a `{` is prose rather than a brace: the depth count then
                // either never returns to zero -- "the braces do not balance", a red test naming
                // nothing about the guard it holds -- or closes the body early and silently.
                let mut depth = 1usize;
                let mut scan = index + 2;
                while scan + 1 < bytes.len() && depth > 0 {
                    match (bytes[scan], bytes[scan + 1]) {
                        (b'/', b'*') => {
                            depth += 1;
                            scan += 2;
                        }
                        (b'*', b'/') => {
                            depth -= 1;
                            scan += 2;
                        }
                        _ => scan += 1,
                    }
                }
                index = if depth == 0 { scan } else { bytes.len() };
                continue;
            }
            b'r' => {
                // A raw string: r"..", r#".."#, r##".."##. Its hashes decide where it ends, and its
                // contents are exempt from escape rules, so it needs its own skip.
                let hashes = rest[index + 1..]
                    .bytes()
                    .take_while(|byte| *byte == b'#')
                    .count();
                if bytes.get(index + 1 + hashes) == Some(&b'"') {
                    let terminator = format!("\"{}", "#".repeat(hashes));
                    let from = index + 2 + hashes;
                    index = rest[from..]
                        .find(&terminator)
                        .map_or(bytes.len(), |n| from + n + terminator.len());
                    continue;
                }
                index += 1;
                continue;
            }
            // A LIFETIME IS NOT A CHAR LITERAL, and `'` opens both.

            // `'static` here opened a literal that closed on the next apostrophe in the file --
            // which was an ordinary `operator's` inside a comment forty lines down. Everything
            // between was swallowed, braces included, so the depth count went wrong and this
            // returned a body that stopped in the middle of the function. Two order guards then
            // reported that calls were missing from a function that still made them.

            // The rule: `'` followed by an identifier start is a lifetime UNLESS the byte after
            // that identifier start closes it -- `'a'` is a char, `'a,` and `'static` are not. A
            // multi-byte or escaped body (an accented letter, `'\n'`) falls through to the literal
            // branch, which is where it belongs.
            b'\''
                if bytes
                    .get(index + 1)
                    .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
                    && bytes.get(index + 2) != Some(&b'\'') =>
            {
                index += 1;
                continue;
            }
            b'"' | b'\'' => {
                let quote = bytes[index];
                let mut scan = index + 1;
                while scan < bytes.len() {
                    match bytes[scan] {
                        b'\\' => scan += 2,
                        byte if byte == quote => {
                            scan += 1;
                            break;
                        }
                        _ => scan += 1,
                    }
                }
                index = scan;
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return &rest[open..=index];
                }
            }
            _ => {}
        }
        index += 1;
    }

    panic!("`{signature}` is never closed: the braces in this source do not balance")
}

/// The same body with its comments removed, for guards that assert on TEXT rather than on braces.

/// `body_of` skips comments while counting depth, but it returns the slice whole -- comments and
/// all. A guard that then asks `body.contains("ensure_model_resolves(")` is satisfied by a line
/// that says `// ensure_model_resolves(...)`, so commenting the call out leaves the guard green.
/// Measured in `admin.rs`: commenting out the whole `ensure_model_resolves(...)` block left its
/// guard passing.

/// `admin.rs` grew its own filter for this, over `//` lines only. `/*... */` walked straight
/// through it, and it kept the deletable `#[cfg(` anchor. One seam instead: bound by brace depth,
/// drop both forms of comment, leave string and char literals alone -- a refusal message that says
/// `// not a call` is still part of the code.
#[cfg(test)]
pub(crate) fn code_of(source: &str, signature: &str) -> String {
    let body = body_of(source, signature);
    let bytes = body.as_bytes();
    let mut code = String::with_capacity(body.len());
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                // Keep the newline: two statements must not fuse into one line, or an order guard
                // reading line numbers would see them at the same place.
                index += body[index..].find('\n').unwrap_or(bytes.len() - index);
                continue;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                // Rust nests block comments, and `find("*/")` does not. Stopping at the FIRST `*/`
                // would leave the outer comment's tail in the output as if it were code, so a
                // commented-out call inside it would satisfy the very guards this seam exists for.
                let mut depth = 1usize;
                let mut scan = index + 2;
                while scan + 1 < bytes.len() && depth > 0 {
                    match (bytes[scan], bytes[scan + 1]) {
                        (b'/', b'*') => {
                            depth += 1;
                            scan += 2;
                        }
                        (b'*', b'/') => {
                            depth -= 1;
                            scan += 2;
                        }
                        _ => scan += 1,
                    }
                }
                index = if depth == 0 { scan } else { bytes.len() };
                continue;
            }
            b'r' => {
                let hashes = body[index + 1..]
                    .bytes()
                    .take_while(|byte| *byte == b'#')
                    .count();
                if bytes.get(index + 1 + hashes) == Some(&b'"') {
                    let terminator = format!("\"{}", "#".repeat(hashes));
                    let from = index + 2 + hashes;
                    let end = body[from..]
                        .find(&terminator)
                        .map_or(bytes.len(), |n| from + n + terminator.len());
                    code.push_str(&body[index..end]);
                    index = end;
                    continue;
                }
                code.push('r');
                index += 1;
                continue;
            }
            b'\''
                if bytes
                    .get(index + 1)
                    .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
                    && bytes.get(index + 2) != Some(&b'\'') =>
            {
                code.push('\'');
                index += 1;
                continue;
            }
            b'"' | b'\'' => {
                let quote = bytes[index];
                let mut scan = index + 1;
                while scan < bytes.len() {
                    match bytes[scan] {
                        b'\\' => scan += 2,
                        byte if byte == quote => {
                            scan += 1;
                            break;
                        }
                        _ => scan += 1,
                    }
                }
                let end = scan.min(bytes.len());
                code.push_str(&body[index..end]);
                index = end;
                continue;
            }
            _ => {}
        }
        // A whole CHARACTER, not a byte. `&body[index..index + 1]` panics the moment a multi-byte
        // one appears outside a literal -- Rust identifiers may hold them -- and the panic surfaces
        // inside a test helper, where it reads as the guard being broken rather than the scanner.
        let character = body[index..]
            .chars()
            .next()
            .expect("the scanner only ever stops on a character boundary");
        code.push(character);
        index += character.len_utf8();
    }
    code
}

#[cfg(test)]
mod tests {
    use super::{body_of, code_of};

    /// The body stops at its own closing brace, not at the next item's text.

    /// This is the property the deleted `#[cfg(not(feature = "net-a"))]` anchors were standing
    /// in for, and getting it wrong in the lenient direction is the dangerous one: a body that runs
    /// on into the next function makes an ORDER assertion read calls that are not in the function
    /// under test at all.
    #[test]
    fn a_body_ends_at_its_own_brace_and_does_not_run_into_the_next_item() {
        let source = "\
fn first() {
    guard();
    submit();
}

fn second() {
    submit();
    guard();
}
";
        let body = body_of(source, "fn first");
        assert!(body.contains("guard();"), "the body lost its own contents");
        assert_eq!(
            body.matches("submit();").count(),
            1,
            "the body ran into `second`, so an order assertion would compare calls from two \
             different functions: {body}"
        );
        assert!(
            body.trim_end().ends_with('}'),
            "the body does not end at a closing brace: {body}"
        );
    }

    /// Braces inside comments, strings and chars do not end the body.

    /// Each of these appears in the functions this helper is used on: refusal messages carry `{}`
    /// for formatting, and several carry a literal brace in prose.
    #[test]
    fn braces_that_are_not_code_do_not_close_the_body() {
        let source = "\
fn only() {
    // a closing brace } in a comment
    /* and } in a block comment */
    let text = \"a brace } in a string\";
    let raw = r#\"a brace } in a raw string\"#;
    let brace = '}';
    let escaped = \"\\\" } still inside\";
    done();
}
fn after() {}
";
        let body = body_of(source, "fn only");
        assert!(
            body.contains("done();"),
            "the body was cut short by a brace that is not code: {body}"
        );
        assert!(
            !body.contains("fn after"),
            "the body ran past its own closing brace: {body}"
        );
    }

    /// A commented-out call is not a call, in either form of comment.

    /// This is the whole point of `code_of`. `body_of` returns the comments too, so a guard reading
    /// `body.contains("guard(")` is satisfied by a line that says `// guard(...)`. Measured in
    /// `admin.rs`: commenting out the whole `ensure_model_resolves(...)` block left its guard green.
    /// The `//` filter that was written there in response did not touch `/*... */`.
    #[test]
    fn a_commented_out_call_is_not_a_call_in_either_comment_form() {
        let source = "\
fn only() {
    // guard();
    /* guard(); */
    let message = \"call guard() first\";
    submit();
}
";
        let code = code_of(source, "fn only");
        assert!(
            !code.contains("// guard();"),
            "a line comment survived, so commenting a call out still reads as making it: {code}"
        );
        assert!(
            !code.contains("/* guard(); */"),
            "a block comment survived: {code}"
        );
        assert!(
            code.contains("submit();"),
            "the code itself was dropped: {code}"
        );
        assert!(
            code.contains("call guard() first"),
            "a string literal is code, not a comment, and a refusal message must survive: {code}"
        );
    }

    /// A NESTED block comment does not end at the inner `*/`.

    /// Rust nests them; `find("*/")` does not. Stopping at the first one leaves the outer comment's
    /// tail in the output as code, so `guard()` written inside a commented-out block satisfies a
    /// guard that asks whether the function calls it -- the exact false pass `code_of` exists to
    /// prevent, reintroduced by the seam meant to prevent it.
    #[test]
    fn a_nested_block_comment_does_not_end_at_the_inner_close() {
        let source = "\
fn only() {
    /* outer /* inner */ guard(); */
    submit();
}
";
        let code = code_of(source, "fn only");
        assert!(
            !code.contains("guard()"),
            "the outer comment ended at the inner `*/`, so its tail reads as code: {code}"
        );
        assert!(
            code.contains("submit();"),
            "the scanner ran past the outer comment and ate the code: {code}"
        );
    }

    /// A multi-byte character outside a literal is copied, not sliced through.

    /// `&body[index..index + 1]` panics on one, and the panic surfaces inside a test helper where
    /// it reads as the guard being broken rather than the scanner. Rust identifiers may hold them.
    #[test]
    fn a_multi_byte_character_outside_a_literal_does_not_panic() {
        let source = "\
fn only() {
    let \u{e9}t\u{e9} = 1;
    submit();
}
";
        let code = code_of(source, "fn only");
        assert!(
            code.contains("submit();"),
            "the scanner did not reach the end of the body: {code}"
        );
        assert!(
            code.contains("\u{e9}t\u{e9}"),
            "the identifier was mangled on the way through: {code}"
        );
    }

    /// Statements do not fuse when the comment between them is removed.

    /// Dropping the trailing newline with the comment would put two calls on one line, and every
    /// caller of this seam is an ORDER guard that compares where calls appear.
    #[test]
    fn removing_a_comment_does_not_join_two_statements() {
        let code = code_of(
            "fn only() {\n    first(); // why\n second();\n}\n",
            "fn only",
        );
        let first = code
            .lines()
            .position(|line| line.contains("first();"))
            .expect("the first call survived");
        let second = code
            .lines()
            .position(|line| line.contains("second();"))
            .expect("the second call survived");
        assert!(
            first < second,
            "the two calls landed on the same line, so an order guard reading lines would see \
             them at the same place: {code:?}"
        );
    }

    /// The same nesting rule in `body_of`, which is the half that decides the BOUNDS.

    /// `code_of` had the fix first and `body_of` did not, which fixed nothing: `body_of` runs
    /// first and hands `code_of` a slice. The fixture holds a `{` inside the outer comment, so
    /// without the rule the depth count never returns to zero and the panic is "the braces in this
    /// source do not balance" -- a red test that names nothing about the guard it holds.
    #[test]
    fn body_of_also_counts_nested_block_comments() {
        let source = "\
fn only() {
    /* outer /* inner */ if x { */
    done();
}
fn after() {}
";
        let body = body_of(source, "fn only");
        assert!(
            body.contains("done();"),
            "the body lost its own contents: {body}"
        );
        assert!(
            !body.contains("fn after"),
            "the body ran past its own closing brace: {body}"
        );
    }

    /// The item, not a test's own mention of it.

    /// `find` takes the FIRST occurrence, and a guard that spells its target out loud is an
    /// earlier occurrence whenever the test module sits above the code -- `commands.rs` is such a
    /// file. Measured there twice: anchored on `fn tell(self)` the scanner read its own string
    /// literal and returned the next line; anchored on a signature carrying `{` it opened a brace
    /// inside a literal and died on "the braces do not balance". Silently wrong and loudly wrong
    /// about the wrong thing. A declaration starts a line; a mention inside one does not.
    #[test]
    fn a_mention_inside_a_line_is_not_the_declaration() {
        // THE MENTION MUST BE FOLLOWED BY A BRACE OF ITS OWN, or the fixture proves nothing: with
        // no brace between the literal and the real declaration, `find` lands in the literal and
        // then walks forward to the REAL body anyway, and the test is green either way. Measured:
        // the first version of this fixture passed with the rule and without it.
        let source = "\
fn caller() {
    if check(\"fn subject()\") { decoy(); }
}

fn subject() {
    the_real_body();
}
";
        let body = body_of(source, "fn subject()");
        assert!(
            body.contains("the_real_body();"),
            "the scanner anchored on the string literal in `caller`, not on the item: {body}"
        );
    }

    /// A signature that is not there names itself, rather than returning an empty slice that the
    /// caller then searches in vain.
    #[test]
    fn a_missing_signature_is_named() {
        let panic = std::panic::catch_unwind(|| body_of("fn present() {}", "fn absent"))
            .expect_err("a missing signature must panic");
        let message = panic
            .downcast_ref::<String>()
            .expect("the panic carries a message");
        assert!(
            message.contains("fn absent"),
            "the panic does not name what was missing: {message}"
        );
    }

    /// A lifetime is not a char literal, and treating it as one truncates the body.

    /// Measured: `'static` in a signature opened a literal that closed on the next apostrophe in
    /// the file -- an ordinary `operator's` in a comment forty lines down. Everything between was
    /// swallowed, braces included, so the depth count ended the body early and two order guards
    /// reported calls missing from a function that still made them.

    /// THE FIXTURE MUST BE UNBALANCED INSIDE THE SWALLOWED CHUNK, or it proves nothing. The first
    /// version of this test put `needle()` inside the `if` and the swallowed span held one `{` and
    /// one `}` -- balanced, so the depth count survived and the body came back whole. It was green
    /// with the rule, green without it, and green with the opposite rule: it did not distinguish
    /// the three states of the scanner at all. Measured, not reasoned: a probe printed byte-equal
    /// bodies for the flag on and off.

    /// Here the span between `'static` and the apostrophe in `operator's` holds TWO opening braces
    /// and one closing one. Without the rule the scanner misses a net `+1`, so the `}` that closes
    /// `if` looks like the one that closes the function and the body ends before `needle()` -- the
    /// same shape as the real defect, where the body stopped 511 lines early.
    #[test]
    fn a_lifetime_does_not_open_a_char_literal() {
        const SOURCE: &str = "\
fn subject() {
    let handle: Arc<Thing<'static>> = Arc::new(move |x| {
        inner(x)
    });
    if true {
        // a comment with the operator's apostrophe in it
        inner(1);
    }
    needle();
}
";
        let body = super::body_of(SOURCE, "fn subject()");
        assert!(
            body.contains("needle()"),
            "the body stopped early, so a guard would report a call the function still makes: \
             {body}"
        );
        assert!(
            body.trim_end().ends_with('}'),
            "the body must be balanced: {body}"
        );
    }

    /// The OTHER half of the rule: `'` followed by a letter is still a char literal when the byte
    /// after that letter closes it.

    /// Without this, deleting `&& bytes.get(index + 2) != Some(&b'\'')` leaves every test in this
    /// module green while `let c = 'x';` has its closing quote read as the opening of a literal
    /// that runs to the next apostrophe in the file -- reproducing the very truncation the sibling
    /// test guards against. The fixture therefore carries an alphabetic char literal AND the
    /// unbalanced brace that makes the truncation visible.
    #[test]
    fn an_alphabetic_char_literal_is_not_read_as_a_lifetime() {
        const SOURCE: &str = "\
fn subject() {
    let c = 'x';
    if true {
        // a comment with the operator's apostrophe in it
        inner(c);
    }
    needle();
}
";
        let body = super::body_of(SOURCE, "fn subject()");
        assert!(
            body.contains("needle()"),
            "the closing quote of `'x'` opened a literal, so the body stopped early: {body}"
        );
    }
}
