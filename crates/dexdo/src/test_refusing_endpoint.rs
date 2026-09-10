//! The one reservation primitive the tests use for an address that must stay DEAD.

//! `crates/dexdo/src/cli/` is compiled into the `dexdo` BINARY and `crates/dexdo/src/seller/` into
//! the `dexdo` LIBRARY, so a `#[cfg(test)]` item in one is invisible to the other. This file is
//! declared by BOTH crate roots, which keeps a single definition in the tree without putting a
//! test-only helper on the library's production surface.

/// An address that refuses every connection for as long as the returned socket is held.

/// The port is held by a **UDP** socket, which leaves TCP on that port genuinely closed: a SYN finds
/// no TCP control block at all and is answered with RST (`ECONNREFUSED`), exactly like a closed
/// port. Hold the socket for as long as the address must stay dead.

/// Binding a listener, reading `local_addr()` and dropping it hands the port straight back to the
/// kernel, and any concurrent `bind("127.0.0.1:0")` -- a neighbouring test, the second CI pipeline on
/// the same builder -- can be handed that exact port before the probe runs. The "dead" endpoint then
/// answers and the test fails, hangs on the handshake, or silently proves the wrong path.

/// Holding a TCP listener open instead is not equivalent: `listen(2)` completes the handshake out of
/// the backlog, so the connect would SUCCEED and the connection-refused path under test would never
/// be reached. A *connected* socket is not equivalent either -- Linux can reuse its local port as
/// the source of a connect to that same port, and the resulting TCP self-connection is accepted;
/// that shape measured 6 successful connections in 120,000.

/// # Why UDP and not a TCP socket bound without `listen`

/// That was this primitive's shape until 2026-08-17, and its refusal is **Linux-only**. On BSD the
/// bound socket still owns a TCP control block for the port, and a SYN that finds one in a
/// non-listening state is dropped in silence rather than reset -- so the connect does not refuse, it
/// hangs until the caller's deadline. Measured on both runners, connecting to a held port:

/// ```text
/// TCP bound, never listened Linux ECONNREFUSED 0.001s macos-latest TIMED OUT after 3.001s
/// UDP-held, TCP truly closed Linux ECONNREFUSED 0.000s macos-latest ECONNREFUSED 0.000s
/// ```

/// That is why four `seller::liveness` rows failed on `macos-latest` and nowhere else: every one of
/// them collapsed into `handshake_timeout`, so the rows asserting `stage: tcp_connect` saw the wrong
/// stage, and the two arms the matrix deliberately keeps apart -- a TRANSPORT fault and a probe
/// TIMEOUT -- became indistinguishable.

/// TCP and UDP are separate port spaces, so the hold does not stop a TCP `bind(0)` from being handed
/// the same number. Measured over 3,000 rounds: 0 collisions on Linux, 1 on `macos-latest`.

/// # The residue is not accepted any more: the address is proved dead before it is returned

/// That collision stopped being theoretical. On a builder running eight pipelines at once a
/// neighbouring test's gateway held TCP on the number this hold had taken by UDP, and
/// `unreachable_advertised_gateway_fails_before_any_sell_post` read
/// `stage: tls_handshake... received corrupt message of type InvalidContentType` instead of the
/// `tcp_connect` it asserts. Nothing in the failure named a port collision, so the row read as a
/// defect in the code under test.

/// So the reservation now checks what it claims: one TCP connect to the held address, which must be
/// refused. A connect that SUCCEEDS means another process owns TCP there and the address is not
/// dead, so that port is abandoned and the next one tried. The check costs one refused connect --
/// under a millisecond on both runners, the same measurement the table above rests on -- and it
/// removes the case where a live listener elsewhere on the machine decides what a probe stage says.

/// What remains is the window between this check and the caller's own probe. To do harm, something
/// would have to start listening on this exact port inside it; a listener that was already there,
/// which is what happened, is now caught.
pub(crate) fn refusing_endpoint() -> (tokio::net::UdpSocket, String) {
    // Bounded: a machine where 64 ports in a row all have a live TCP listener is not a flake to
    // retry through, and a caller waiting on a silent loop is worse than a named failure.
    for attempt in 1..=64 {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("hold a port with UDP");
        socket
            .set_nonblocking(true)
            .expect("the held socket is never read from");
        let addr = socket.local_addr().expect("bound address");
        if tcp_refuses(addr) {
            let socket = tokio::net::UdpSocket::from_std(socket).expect("adopt the held socket");
            return (socket, addr.to_string());
        }
        // The port is held by UDP but something answers TCP on it. Drop the hold and take another:
        // this address cannot be the dead one the caller asked for.
        drop(socket);
        assert!(
            attempt < 64,
            "no port on 127.0.0.1 refused TCP after 64 attempts: something on this machine is \
             listening on every port this process was handed, so no address can be held dead"
        );
    }
    unreachable!("the loop returns or asserts")
}

/// Does TCP on this address refuse a connection right now?

/// The one thing the caller is promised. A refusal is the answer wanted; a connection that
/// completes means the address is alive and belongs to someone else.
fn tcp_refuses(addr: std::net::SocketAddr) -> bool {
    // Long enough for a refusal on both runners (measured under a millisecond) and short enough that
    // a silently dropped SYN -- the BSD shape described above -- ends the check rather than the test.
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(200)).is_err()
}

#[cfg(test)]
mod tests {
    use super::{refusing_endpoint, tcp_refuses};

    /// The promise this primitive makes: the address it hands back refuses TCP. Before the check was
    /// added the promise was inferred from the UDP hold and was wrong whenever another process owned
    /// TCP on the same number.
    // Adopting the held socket into tokio needs a reactor, the same as every caller of this
    // primitive has.
    #[tokio::test]
    async fn the_reserved_address_refuses_tcp() {
        let (_hold, addr) = refusing_endpoint();
        let addr: std::net::SocketAddr = addr.parse().expect("the reservation returns an address");
        assert!(
            tcp_refuses(addr),
            "the reservation returned {addr}, which does not refuse TCP"
        );
    }

    /// The case that turned a CI round red: a live listener. An address someone answers on is not a
    /// dead address, and the check has to say so -- otherwise the reservation hands it out and the
    /// probe under test reports a TLS stage instead of the refusal it asserts.
    #[test]
    fn a_live_listener_is_not_a_refusing_address() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("live listener");
        let addr = listener.local_addr().expect("bound address");
        assert!(
            !tcp_refuses(addr),
            "a live listener at {addr} was reported as refusing"
        );
    }
}
