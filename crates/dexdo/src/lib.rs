//! `dexdo` -- a binary with `seller`/`buyer` subcommands (clap) and the shared library of their logic.
//! Mock mode (`--mock-model`, `--mock-chain`) is a standard mode in production code.

#[cfg(test)]
mod test_refusing_endpoint;

// Shared with the binary's `cli::source_probe` -- same file, declared in both targets. See the
// comment there.
#[cfg(test)]
pub(crate) mod source_probe;
// the deal's `_balance` must be disposed of on every path to `selfdestruct`. Declared at the
// crate root because it guards a CONTRACT, not any one Rust module, and because `code_of` lives here.
#[cfg(test)]
mod token_contract_die_disposal_1786;
/// Process entry: a closed stdout ends the process on SIGPIPE, a closed SOCKET does
/// not.

/// Two writers can meet a reader that has gone away, and they need opposite answers.

/// - **stdout.** `dexdo... | less` that was quit. `print!`/`println!` panic on a write error by
/// contract, which reports a command that did its work as exit 101 with `panicked at` on stderr
/// . The answer every Unix tool gives is to die on signal 13.
/// - **a socket.** This binary's socket writes go out through a bare `writev(2)` -- there is no
/// `MSG_NOSIGNAL` anywhere in the mio/tokio/hyper stack it uses, and no `SO_NOSIGPIPE` is set.
/// Under the default disposition, a peer that closed its end does not hand the writer an `EPIPE`
/// to handle: it kills the process. That is not only a serving concern. `wallet onboard`,
/// `note deploy`, `provision` and the close paths talk to the chain and the wallet relay for
/// minutes, and is `wallet onboard ackinacki-wallet` dying with -13 between "wallet app
/// connected" and the Vault/Hot deploy -- no line saying why, on a path that may already have
/// spent.

/// answered the first by restoring `SIG_DFL` for the whole process at entry, which handed the
/// second to the kernel for every command but the two that serve. So the disposition stays
/// IGNORED, as Rust's runtime installs it before `main`, and the first case is answered where it
/// arises: the standard library's print panic is recognised in a panic hook, and only that panic is
/// turned into the SIGPIPE death. Every other panic goes to the hook that was there before.
#[cfg(unix)]
pub fn closed_stdout_ends_on_sigpipe() {
    // SAFETY: SIG_IGN on SIGPIPE is the disposition Rust's runtime already installed before `main`;
    // stated here so that process entry owns it rather than inheriting it by accident.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if is_closed_standard_stream(info.payload()) {
            // SAFETY: restores the kernel's default disposition and raises the signal on this
            // thread; SIGPIPE is not blocked anywhere in this process, so it is delivered here and
            // the process ends on signal 13 -- the way asks a closed reader to end it.
            unsafe {
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                libc::raise(libc::SIGPIPE);
            }
        }
        previous(info);
    }));
}

/// Non-unix builds have no SIGPIPE; the print panic stays what it is there.
#[cfg(not(unix))]
pub fn closed_stdout_ends_on_sigpipe() {}

/// The standard library's panic for a print to a stream whose reader has gone away:
/// `failed printing to stdout: Broken pipe (os error 32)` (and the same for stderr). Recognised by
/// the library's own prefix and by `EPIPE`'s number, which is 32 on every unix this ships for, so
/// that no other panic -- and no other write error -- is ever turned into a signal.
#[cfg(unix)]
fn is_closed_standard_stream(payload: &(dyn std::any::Any + Send)) -> bool {
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or_default();
    message.starts_with("failed printing to std") && message.contains("(os error 32)")
}

/// Put SIGPIPE back to "ignored" for a process that is about to SERVE.

/// Since process entry already leaves SIGPIPE ignored ([`closed_stdout_ends_on_sigpipe`]), so
/// this is a no-op at the serving point today. It stays, called where serving begins, so that the
/// serving modes do not depend on the entry policy staying what it is: a gateway killed mid-stream
/// by a consumer that hung up is a death on the money path, and first shipped exactly that
/// shape for every command.

/// Closing the risk by construction rather than by a lucky run: a probe that fails to make a gateway
/// die proves nothing, because the absence of a signal in one run is not the absence of the signal.
#[cfg(unix)]
pub fn serving_process_ignores_sigpipe() {
    // SAFETY: sets a process-wide signal disposition to SIG_IGN, which is the disposition Rust's own
    // runtime installs before `main`. Idempotent, and safe to call from any thread.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

/// Non-unix builds never changed the disposition, so there is nothing to put back.
#[cfg(not(unix))]
pub fn serving_process_ignores_sigpipe() {}

pub mod api_format;
pub mod buyer;
pub mod registry;
pub mod runtime_events;
pub mod seller;
pub mod wallet_seed;

pub mod telemetry;
