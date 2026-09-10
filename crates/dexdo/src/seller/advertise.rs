//! Classification of the seller's advertised gateway address and the error codes used by
//! the `advertised_gateway` readiness probe.

//! The advertised address is what the seller encrypts into the buyer handover. The default
//! assumption is a REAL deployment -- seller and buyer on different machines -- so a non-routable
//! advertise is a footgun: the offer rests in the book and no remote buyer can reach it. It is
//! rejected fail-closed unless the operator opts into local/LAN testing with
//! `--allow-private-advertise`.

//! The classifier screens a FINITE, enumerated set of classes -- it is a footgun screen, not a proof
//! of routability, and an address it accepts is one that matched none of these:

//! * scope-limited -- bind-all wildcard, loopback, RFC1918/ULA, link-local, CGNAT;
//! * reserved-but-unroutable -- documentation (RFC 5737 / RFC 3849 / RFC 9637), benchmarking
//! (RFC 2544), `240.0.0.0/4` reserved-for-future-use, `0.0.0.0/8` "this network", and multicast.

//! The second group matters as much as the first because this classifier is the input to the
//! probe policy: classifying a documentation range as public let a probe failure no buyer could
//! ever recover from take the same path as a NAT/VPN hairpin, and rested an ask carrying an
//! endpoint nobody can dial. rejects the unreachable and forgives the unobservable -- a
//! gap in the former makes the latter fail open.

//! DNS names are NOT resolved here: resolution at startup would depend on the seller host's own
//! resolver (split-horizon DNS, VPN resolvers) and would make the check both slow and wrong. A name
//! is presumed publicly resolvable, except for the reserved special-use local names.

//! Error codes emitted from this area, rendered in the shape
//! (`error[CODE] (kind): message` + `cause:` lines + a `hint:` line):

//! | code | kind | meaning |
//! |------|------|---------|
//! | `E_ADVERTISE_NOT_PUBLIC` | config | the advertised host cannot be reached by a remote buyer |
//! | `E_ADVERTISE_UNREACHABLE` | network | the advertised address did not answer the pinned-TLS (h2) self-probe |
//! | `E_ADVERTISE_WRONG_GATEWAY` | tls | the advertised address answered, but it is not this gateway |

use dexdo_core::{error_codes, DexdoError};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Why an advertised host cannot be dialled by a remote buyer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NonPublicReason {
    /// `0.0.0.0` / `::` -- a bind-all wildcard, not a connect target.
    BindAll,
    /// `127.0.0.0/8`, `::1`, `localhost` -- the seller's own host only.
    Loopback,
    /// RFC1918 (`10/8`, `172.16/12`, `192.168/16`) or IPv6 ULA (`fc00::/7`) -- LAN only.
    Private,
    /// `169.254/16` / `fe80::/10` -- link-local only.
    LinkLocal,
    /// `100.64/10` -- carrier-grade NAT / overlay range, not internet-routable.
    Cgnat,
    /// `0.0.0.0/8` past the wildcard itself -- "this network", valid only as a source address.
    ThisNetwork,
    /// `192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24` (RFC 5737), `2001:db8::/32` (RFC 3849)
    /// and `3fff::/20` (RFC 9637) -- reserved for documentation, never routed to a real host.
    Documentation,
    /// `198.18.0.0/15` -- inter-network benchmarking (RFC 2544), never routed between networks.
    Benchmarking,
    /// `240.0.0.0/4` -- reserved for future use, including the `255.255.255.255` limited broadcast.
    Reserved,
    /// `224.0.0.0/4` / `ff00::/8` -- a multicast group, never a unicast connect target.
    Multicast,
    /// A reserved special-use local name (`localhost`, `*.local`, `*.internal`, `*.home.arpa`).
    LocalName,
}

impl NonPublicReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BindAll => "bind-all wildcard",
            Self::Loopback => "loopback",
            Self::Private => "private (RFC1918/ULA)",
            Self::LinkLocal => "link-local",
            Self::Cgnat => "CGNAT (100.64/10)",
            Self::ThisNetwork => "this-network (0.0.0.0/8)",
            Self::Documentation => "documentation-only (RFC 5737/3849/9637)",
            Self::Benchmarking => "benchmarking (198.18.0.0/15)",
            Self::Reserved => "reserved for future use (240.0.0.0/4)",
            Self::Multicast => "multicast group",
            Self::LocalName => "reserved local name",
        }
    }
}

/// Whether the advertised address can plausibly be dialled by a remote buyer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdvertiseReachability {
    /// An IP literal that matched none of the screened non-routable classes, or a DNS name
    /// (presumed publicly resolvable -- not resolved here). This is the absence of a known footgun,
    /// NOT a positive finding that the address is globally routable or currently dialable.
    Public,
    NonPublic(NonPublicReason),
}

impl AdvertiseReachability {
    pub fn is_public(self) -> bool {
        matches!(self, Self::Public)
    }
}

/// Split `host:port` (also `[v6]:port`) into its host part.
fn advertise_host(addr: &str) -> &str {
    let addr = addr.trim();
    if let Some(rest) = addr.strip_prefix('[') {
        if let Some((host, _)) = rest.split_once(']') {
            return host;
        }
        return rest;
    }
    // An unbracketed IPv6 literal has more than one colon and is not a `host:port` pair.
    if addr.matches(':').count() > 1 {
        return addr;
    }
    match addr.rsplit_once(':') {
        Some((host, _)) => host,
        None => addr,
    }
}

fn classify_v4(ip: Ipv4Addr) -> AdvertiseReachability {
    let octets = ip.octets();
    if ip.is_unspecified() {
        return AdvertiseReachability::NonPublic(NonPublicReason::BindAll);
    }
    // 0.0.0.0/8 past the wildcard: "this network", a source-only range (RFC 1122 3.2.1.3). No
    // stable or unstable std helper covers it, so it is spelled out.
    if octets[0] == 0 {
        return AdvertiseReachability::NonPublic(NonPublicReason::ThisNetwork);
    }
    if ip.is_loopback() {
        return AdvertiseReachability::NonPublic(NonPublicReason::Loopback);
    }
    if ip.is_private() {
        return AdvertiseReachability::NonPublic(NonPublicReason::Private);
    }
    if ip.is_link_local() {
        return AdvertiseReachability::NonPublic(NonPublicReason::LinkLocal);
    }
    // 100.64.0.0/10 (`Ipv4Addr::is_shared` is still unstable).
    if octets[0] == 100 && (64..=127).contains(&octets[1]) {
        return AdvertiseReachability::NonPublic(NonPublicReason::Cgnat);
    }
    // RFC 5737: 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24. `Ipv4Addr::is_documentation` is
    // stable and is exactly those three blocks.
    if ip.is_documentation() {
        return AdvertiseReachability::NonPublic(NonPublicReason::Documentation);
    }
    // 198.18.0.0/15 (`Ipv4Addr::is_benchmarking` is still unstable).
    if octets[0] == 198 && (octets[1] == 18 || octets[1] == 19) {
        return AdvertiseReachability::NonPublic(NonPublicReason::Benchmarking);
    }
    // 224.0.0.0/4.
    if ip.is_multicast() {
        return AdvertiseReachability::NonPublic(NonPublicReason::Multicast);
    }
    // 240.0.0.0/4, which also covers the 255.255.255.255 limited broadcast -- neither is a unicast
    // connect target (`Ipv4Addr::is_reserved` is still unstable).
    if octets[0] >= 240 {
        return AdvertiseReachability::NonPublic(NonPublicReason::Reserved);
    }
    AdvertiseReachability::Public
}

fn classify_v6(ip: Ipv6Addr) -> AdvertiseReachability {
    if ip.is_unspecified() {
        return AdvertiseReachability::NonPublic(NonPublicReason::BindAll);
    }
    if ip.is_loopback() {
        return AdvertiseReachability::NonPublic(NonPublicReason::Loopback);
    }
    let segments = ip.segments();
    let first = segments[0];
    // fe80::/10 and fc00::/7, kept as explicit masks so every range in this module reads the same
    // way (`is_unicast_link_local`/`is_unique_local` became stable in 1.84.0; the documentation
    // helper below is still unstable, so a mixed style would say nothing).
    if first & 0xffc0 == 0xfe80 {
        return AdvertiseReachability::NonPublic(NonPublicReason::LinkLocal);
    }
    if first & 0xfe00 == 0xfc00 {
        return AdvertiseReachability::NonPublic(NonPublicReason::Private);
    }
    // 2001:db8::/32 (RFC 3849) and 3fff::/20 (RFC 9637) -- the two documentation prefixes
    // (`Ipv6Addr::is_documentation` is still unstable). A /20 fixes the first segment plus the top
    // four bits of the second.
    if (first == 0x2001 && segments[1] == 0x0db8) || (first == 0x3fff && segments[1] & 0xf000 == 0)
    {
        return AdvertiseReachability::NonPublic(NonPublicReason::Documentation);
    }
    // ff00::/8.
    if ip.is_multicast() {
        return AdvertiseReachability::NonPublic(NonPublicReason::Multicast);
    }
    AdvertiseReachability::Public
}

fn classify_name(host: &str) -> AdvertiseReachability {
    let name = host.trim_end_matches('.').to_ascii_lowercase();
    let local = name == "localhost"
        || name.ends_with(".localhost")
        || name.ends_with(".local")
        || name.ends_with(".internal")
        || name.ends_with(".home.arpa");
    if local {
        AdvertiseReachability::NonPublic(NonPublicReason::LocalName)
    } else {
        AdvertiseReachability::Public
    }
}

/// Classify the host part of an advertised `host:port`.
pub fn classify_advertise(addr: &str) -> AdvertiseReachability {
    let host = advertise_host(addr);
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => classify_v4(v4),
        Ok(IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => classify_v4(v4),
            None => classify_v6(v6),
        },
        Err(_) => classify_name(host),
    }
}

/// `true` when a remote buyer can plausibly dial the advertised address.
pub fn advertise_is_public(addr: &str) -> bool {
    classify_advertise(addr).is_public()
}

/// Fail-closed validation of the advertised gateway address before any order is posted.

/// `defaulted_from_listen` only shapes the message: it says the operator never chose the address.
/// `allow_private` is the explicit `--allow-private-advertise` opt-in for same-host/LAN testing.
pub fn validate_advertise(
    advertised: &str,
    defaulted_from_listen: bool,
    allow_private: bool,
) -> Result<(), DexdoError> {
    if allow_private {
        return Ok(());
    }
    match classify_advertise(advertised) {
        AdvertiseReachability::Public => Ok(()),
        AdvertiseReachability::NonPublic(reason) => {
            let message = if defaulted_from_listen {
                format!(
                    "--gateway-advertise defaulted to --gateway-listen {advertised}, which is not \
                     reachable by remote buyers ({})",
                    reason.as_str()
                )
            } else {
                format!(
                    "--gateway-advertise {advertised} is not reachable by remote buyers ({})",
                    reason.as_str()
                )
            };
            Err(
                DexdoError::new(error_codes::E_ADVERTISE_NOT_PUBLIC, message)
                    .with_hint(error_codes::E_ADVERTISE_NOT_PUBLIC.fix()),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason(addr: &str) -> NonPublicReason {
        match classify_advertise(addr) {
            AdvertiseReachability::NonPublic(reason) => reason,
            AdvertiseReachability::Public => panic!("{addr} must not classify as public"),
        }
    }

    /// E2E-ROW: E2E-ADV-07/LP
    #[test]
    fn every_non_routable_class_is_rejected() {
        assert_eq!(reason("0.0.0.0:8443"), NonPublicReason::BindAll);
        assert_eq!(reason("[::]:8443"), NonPublicReason::BindAll);
        assert_eq!(reason("127.0.0.1:8443"), NonPublicReason::Loopback);
        assert_eq!(reason("127.10.20.30:8443"), NonPublicReason::Loopback);
        assert_eq!(reason("[::1]:8443"), NonPublicReason::Loopback);
        assert_eq!(reason("10.1.2.3:8443"), NonPublicReason::Private);
        assert_eq!(reason("172.16.0.1:8443"), NonPublicReason::Private);
        assert_eq!(reason("172.31.255.254:8443"), NonPublicReason::Private);
        assert_eq!(reason("192.168.1.10:8443"), NonPublicReason::Private);
        assert_eq!(reason("[fd00::1]:8443"), NonPublicReason::Private);
        assert_eq!(reason("169.254.10.1:8443"), NonPublicReason::LinkLocal);
        assert_eq!(reason("[fe80::1]:8443"), NonPublicReason::LinkLocal);
        assert_eq!(reason("100.64.0.1:8443"), NonPublicReason::Cgnat);
        assert_eq!(reason("100.127.255.254:8443"), NonPublicReason::Cgnat);
        assert_eq!(reason("localhost:8443"), NonPublicReason::LocalName);
        assert_eq!(reason("LocalHost.:8443"), NonPublicReason::LocalName);
        assert_eq!(reason("seller.local:8443"), NonPublicReason::LocalName);
        assert_eq!(reason("box.internal:8443"), NonPublicReason::LocalName);
        assert_eq!(reason("box.home.arpa:8443"), NonPublicReason::LocalName);
        // IPv4-mapped IPv6 must not smuggle a private address past the classifier.
        assert_eq!(
            reason("[::ffff:192.168.1.10]:8443"),
            NonPublicReason::Private
        );
        assert_eq!(reason("[::ffff:127.0.0.1]:8443"), NonPublicReason::Loopback);
    }

    /// E2E-ROW: E2E-ADV-08/L0
    #[test]
    fn public_addresses_and_names_are_allowed() {
        for addr in [
            "94.156.178.14:8443",
            "8.8.8.8:443",
            // 172.15/16 and 172.32/16 sit outside RFC1918; 100.128/9 sits outside CGNAT.
            "172.15.0.1:8443",
            "172.32.0.1:8443",
            "100.128.0.1:8443",
            "99.64.0.1:8443",
            "[2606:4700::1111]:443",
            "seller.example.net:443",
            "gw.internal.example.net:443",
            "laptop:8443",
        ] {
            assert!(
                advertise_is_public(addr),
                "{addr} must classify as publicly dialable"
            );
        }
    }

    /// E2E-ROW: E2E-ADV-09/L0
    #[test]
    fn allow_private_opt_in_accepts_every_rejected_class() {
        for addr in [
            "0.0.0.0:8443",
            "127.0.0.1:8443",
            "192.168.1.10:8443",
            "169.254.10.1:8443",
            "100.64.0.1:8443",
            "localhost:8443",
        ] {
            assert!(
                validate_advertise(addr, false, false).is_err(),
                "{addr} must fail closed by default"
            );
            assert!(
                validate_advertise(addr, false, true).is_ok(),
                "{addr} must be accepted with --allow-private-advertise"
            );
        }
        assert!(validate_advertise("seller.example.net:443", false, false).is_ok());
    }

    /// E2E-ADV-07 -- a seller is refused an address no buyer on the internet could dial, across
    /// the whole of every such range and not merely at a few sampled addresses.

    /// Setup: seven ranges of addresses that only work on one machine or one local network, each
    /// with the addresses immediately outside it. Do: ask what the seller makes of each. Observe:
    /// every address inside a range is refused with that range's own reason, every neighbour just
    /// outside is accepted, and the same holds through the alternative spelling that writes an
    /// old-style address inside a new-style one.

    /// The ranges implemented on this head are proven here. E2E-ADV-07's remaining special-use
    /// classes are pinned separately by
    /// `every_non_public_special_use_class_is_rejected_before_sell`, which is red-by-design while
    /// the classifier still accepts them.

    /// E2E-ADV-07, `tests/e2e/test-specification.md`.
    #[test]
    fn every_decided_non_public_range_holds_across_the_whole_range_and_its_complement() {
        // (label, verdict, addresses inside the range, adjacent addresses outside it)
        let ranges: [(&str, NonPublicReason, Vec<String>, Vec<String>); 7] = [
            (
                "loopback 127/8 (Ipv4Addr::is_loopback)",
                NonPublicReason::Loopback,
                (0..=255)
                    .map(|second| format!("127.{second}.0.1:8443"))
                    .collect(),
                vec!["126.255.255.255:8443".into(), "128.0.0.1:8443".into()],
            ),
            (
                "RFC1918 10/8 (Ipv4Addr::is_private)",
                NonPublicReason::Private,
                (0..=255).map(|b| format!("10.{b}.0.1:8443")).collect(),
                vec!["9.255.255.255:8443".into(), "11.0.0.0:8443".into()],
            ),
            (
                "RFC1918 172.16/12 (Ipv4Addr::is_private)",
                NonPublicReason::Private,
                (16..=31).map(|b| format!("172.{b}.0.1:8443")).collect(),
                vec!["172.15.255.255:8443".into(), "172.32.0.0:8443".into()],
            ),
            (
                "RFC1918 192.168/16 (Ipv4Addr::is_private)",
                NonPublicReason::Private,
                (0..=255).map(|c| format!("192.168.{c}.1:8443")).collect(),
                vec!["192.167.255.255:8443".into(), "192.169.0.0:8443".into()],
            ),
            (
                "link-local 169.254/16 (Ipv4Addr::is_link_local)",
                NonPublicReason::LinkLocal,
                (0..=255).map(|c| format!("169.254.{c}.1:8443")).collect(),
                vec!["169.253.255.255:8443".into(), "169.255.0.0:8443".into()],
            ),
            (
                "CGNAT 100.64/10 (advertise.rs:104, inclusive 64..=127)",
                NonPublicReason::Cgnat,
                (64..=127).map(|b| format!("100.{b}.0.1:8443")).collect(),
                vec!["100.63.255.255:8443".into(), "100.128.0.0:8443".into()],
            ),
            (
                "IPv6 ULA fc00::/7 (advertise.rs:122)",
                NonPublicReason::Private,
                (0xfc00..=0xfdff_u16)
                    .step_by(7)
                    .map(|seg| format!("[{seg:x}::1]:8443"))
                    .collect(),
                vec!["[fbff::1]:8443".into(), "[fe00::1]:8443".into()],
            ),
        ];

        for (label, verdict, inside, outside) in ranges {
            assert!(!inside.is_empty(), "{label}: empty sweep proves nothing");
            for addr in &inside {
                assert_eq!(
                    classify_advertise(addr),
                    AdvertiseReachability::NonPublic(verdict),
                    "{label}: {addr} must be rejected as {}",
                    verdict.as_str()
                );
                // The complement of the range check: what the classifier rejects, the degradation
                // gate must not call public. `advertise_is_public` is the predicate the probe
                // policy reads (`seller/liveness.rs:318-326`), so the two must never disagree.
                assert!(!advertise_is_public(addr), "{label}: {addr}");
            }
            for addr in &outside {
                assert!(
                    advertise_is_public(addr),
                    "{label}: {addr} is adjacent to the range and must stay public -- a classifier \
                     that swallows its neighbours refuses reachable sellers"
                );
            }
        }

        // The IPv4-mapped IPv6 form re-dispatches (`advertise.rs:147-150`), so the whole sweep has
        // to hold through it as well or the mapped form is a smuggling route around every range
        // above.
        for (mapped, verdict) in [
            ("[::ffff:10.0.0.1]:8443", NonPublicReason::Private),
            ("[::ffff:172.16.0.1]:8443", NonPublicReason::Private),
            ("[::ffff:169.254.0.1]:8443", NonPublicReason::LinkLocal),
            ("[::ffff:100.64.0.1]:8443", NonPublicReason::Cgnat),
            ("[::ffff:127.0.0.1]:8443", NonPublicReason::Loopback),
            ("[::ffff:0.0.0.0]:8443", NonPublicReason::BindAll),
        ] {
            assert_eq!(
                classify_advertise(mapped),
                AdvertiseReachability::NonPublic(verdict),
                "{mapped} smuggled a non-routable address past the classifier"
            );
        }
        assert!(advertise_is_public("[::ffff:8.8.8.8]:8443"));
    }

    /// E2E-ADV-07/LP -- every special-use class which is not globally reachable is rejected across
    /// generated members of the whole prefix, including this-network, documentation,
    /// benchmarking, multicast, future-reserved, IPv4-mapped and both IPv6
    /// documentation prefixes. The fixed seed keeps the red counterexample deterministic.

    /// E2E-ADV-07, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-ADV-07/LP
    #[test]
    fn e2e_adv_07_rejects_every_non_public_special_use_class() {
        use proptest::prelude::*;
        use proptest::test_runner::{Config, RngSeed, TestRunner};

        let octets = any::<(u8, u8, u8)>();
        let suffix = any::<u8>();
        let strategies: Vec<(&str, bool, BoxedStrategy<String>)> = vec![
            (
                "0/8 this-network",
                true,
                octets
                    .prop_map(|(b, c, d)| format!("0.{b}.{c}.{d}:8443"))
                    .boxed(),
            ),
            (
                "RFC5737 documentation",
                true,
                (
                    prop_oneof![
                        Just((192_u8, 0_u8, 2_u8)),
                        Just((198, 51, 100)),
                        Just((203, 0, 113))
                    ],
                    suffix,
                )
                    .prop_map(|((a, b, c), d)| format!("{a}.{b}.{c}.{d}:8443"))
                    .boxed(),
            ),
            (
                "198.18/15 benchmark",
                true,
                (18_u8..=19, any::<(u8, u8)>())
                    .prop_map(|(b, (c, d))| format!("198.{b}.{c}.{d}:8443"))
                    .boxed(),
            ),
            (
                "IPv4 multicast",
                true,
                (224_u8..=239, octets)
                    .prop_map(|(a, (b, c, d))| format!("{a}.{b}.{c}.{d}:8443"))
                    .boxed(),
            ),
            (
                "240/4 reserved",
                true,
                (240_u8..=255, octets)
                    .prop_map(|(a, (b, c, d))| format!("{a}.{b}.{c}.{d}:8443"))
                    .boxed(),
            ),
            (
                "RFC3849 IPv6 documentation",
                false,
                any::<(u16, u16, u16)>()
                    .prop_map(|(a, b, c)| format!("[2001:db8:{a:x}:{b:x}::{c:x}]:8443"))
                    .boxed(),
            ),
            (
                "RFC9637 3fff/20 documentation",
                false,
                (0_u16..=0x0fff, any::<(u16, u16)>())
                    .prop_map(|(second, (a, b))| format!("[3fff:{second:x}:{a:x}::{b:x}]:8443"))
                    .boxed(),
            ),
            (
                "IPv6 multicast",
                false,
                (0xff00_u16..=0xffff, any::<(u16, u16)>())
                    .prop_map(|(first, (a, b))| format!("[{first:x}:{a:x}::{b:x}]:8443"))
                    .boxed(),
            ),
        ];
        let config = Config {
            cases: 64,
            rng_seed: RngSeed::Fixed(0x0ad7_0007),
            failure_persistence: None,
            ..Config::default()
        };
        let failures = strategies
            .into_iter()
            .filter_map(|(label, map_ipv4, strategy)| {
                TestRunner::new(config.clone())
                    .run(&strategy, |address| {
                        prop_assert!(!advertise_is_public(&address));
                        if map_ipv4 {
                            let ipv4 = address
                                .strip_suffix(":8443")
                                .expect("IPv4 strategy carries the fixed port");
                            let mapped = format!("[::ffff:{ipv4}]:8443");
                            prop_assert!(!advertise_is_public(&mapped));
                        }
                        Ok(())
                    })
                    .err()
                    .map(|error| format!("{label}: {error}"))
            })
            .collect::<Vec<_>>();
        for anycast in ["[::ffff:192.0.0.9]:8443", "[::ffff:192.0.0.10]:8443"] {
            assert!(
                advertise_is_public(anycast),
                "{anycast} is a globally reachable anycast exception"
            );
        }
        assert!(
            failures.is_empty(),
            "E2E-ADV-07 classifier accepted a non-public special-use address class\n{}",
            failures.join("\n")
        );
    }

    /// E2E-ADV-08 -- each accepted boundary value is paired with the adjacent rejected value. The
    /// pair makes over-broad range classification observable: proving only the rejected side
    /// would not catch a range which swallowed its public neighbour.

    /// E2E-ADV-08, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-ADV-08/L0
    #[test]
    fn e2e_adv_08_near_miss_boundaries_stay_public() {
        let boundaries = [
            ("9.255.255.255:8443", "10.0.0.0:8443"),
            ("11.0.0.0:8443", "10.255.255.255:8443"),
            ("172.15.255.255:8443", "172.16.0.0:8443"),
            ("172.32.0.0:8443", "172.31.255.255:8443"),
            ("192.167.255.255:8443", "192.168.0.0:8443"),
            ("192.169.0.0:8443", "192.168.255.255:8443"),
            ("100.63.255.255:8443", "100.64.0.0:8443"),
            ("100.128.0.0:8443", "100.127.255.255:8443"),
            (
                "[fbff:ffff:ffff:ffff:ffff:ffff:ffff:ffff]:8443",
                "[fc00::]:8443",
            ),
            (
                "[fe00::]:8443",
                "[fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff]:8443",
            ),
        ];

        for (public, rejected) in boundaries {
            assert_eq!(
                (advertise_is_public(public), advertise_is_public(rejected)),
                (true, false),
                "E2E-ADV-08 boundary pair {public} / {rejected} changed verdict"
            );
        }
    }

    /// The prefixes added for must not swallow their neighbours.

    /// `e2e_adv_08_near_miss_boundaries_stay_public` holds this seam for the ranges that predate
    /// and `e2e_adv_07_rejects_every_non_public_special_use_class` generates addresses only
    /// from INSIDE each added prefix -- so the address immediately outside one of them is the edge
    /// nothing else holds. Over-rejection is the mirror of the defect and it has a victim: a seller
    /// on a genuinely public address, refused into a market no buyer can reach, by a classifier
    /// that is one octet too wide.

    /// Each row is a prefix's own first and last address, then its neighbours just below and just
    /// above. A neighbour is asserted not to carry THAT prefix's class rather than to be public,
    /// because two added ranges are adjacent: `239.255.255.255` is multicast and `240.0.0.0` is
    /// reserved, so "public" would be the wrong claim at that seam. Every neighbour which is
    /// genuinely unscreened is asserted public as well, in the second list.

    /// Boundaries that do not exist are absent rather than faked: `0.0.0.0/8` has no neighbour
    /// below it (`0.0.0.0` is the bind-all wildcard, a class of its own), and `240.0.0.0/4` and
    /// `ff00::/8` each end their address space. Every IPv4 row is held at the same four points in
    /// the IPv4-mapped IPv6 form, so the mapped path cannot over-reject either edge.

    /// E2E-ADV-08, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-ADV-08/L0
    #[test]
    fn the_added_prefixes_do_not_swallow_their_neighbours() {
        fn advertised(ip: &str) -> String {
            if ip.contains(':') {
                format!("[{ip}]:8443")
            } else {
                format!("{ip}:8443")
            }
        }
        // `classify_advertise` re-dispatches the mapped form through `classify_v4`, so every IPv4
        // assertion below is made twice: once bare, once mapped.
        fn mapped(ip: &str) -> Option<String> {
            (!ip.contains(':')).then(|| format!("[::ffff:{ip}]:8443"))
        }

        // (label, the class, the prefix's first and last address, the neighbours outside it)
        let rows: [(&str, NonPublicReason, [&str; 2], &[&str]); 10] = [
            (
                "0.0.0.0/8 this-network",
                NonPublicReason::ThisNetwork,
                ["0.0.0.1", "0.255.255.255"],
                &["1.0.0.0"],
            ),
            (
                "192.0.2.0/24 RFC5737",
                NonPublicReason::Documentation,
                ["192.0.2.0", "192.0.2.255"],
                &["192.0.1.255", "192.0.3.0"],
            ),
            (
                "198.51.100.0/24 RFC5737",
                NonPublicReason::Documentation,
                ["198.51.100.0", "198.51.100.255"],
                &["198.51.99.255", "198.51.101.0"],
            ),
            (
                "203.0.113.0/24 RFC5737",
                NonPublicReason::Documentation,
                ["203.0.113.0", "203.0.113.255"],
                &["203.0.112.255", "203.0.114.0"],
            ),
            (
                "198.18.0.0/15 RFC2544",
                NonPublicReason::Benchmarking,
                ["198.18.0.0", "198.19.255.255"],
                &["198.17.255.255", "198.20.0.0"],
            ),
            (
                "224.0.0.0/4 multicast",
                NonPublicReason::Multicast,
                ["224.0.0.0", "239.255.255.255"],
                &["223.255.255.255", "240.0.0.0"],
            ),
            (
                "240.0.0.0/4 reserved",
                NonPublicReason::Reserved,
                ["240.0.0.0", "255.255.255.255"],
                &["239.255.255.255"],
            ),
            (
                "2001:db8::/32 RFC3849",
                NonPublicReason::Documentation,
                ["2001:db8::", "2001:db8:ffff:ffff:ffff:ffff:ffff:ffff"],
                &["2001:db7:ffff:ffff:ffff:ffff:ffff:ffff", "2001:db9::"],
            ),
            (
                "3fff::/20 RFC9637",
                NonPublicReason::Documentation,
                ["3fff::", "3fff:0fff:ffff:ffff:ffff:ffff:ffff:ffff"],
                &["3ffe:ffff:ffff:ffff:ffff:ffff:ffff:ffff", "3fff:1000::"],
            ),
            (
                "ff00::/8 multicast",
                NonPublicReason::Multicast,
                ["ff00::", "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"],
                &["feff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"],
            ),
        ];

        for (label, class, inside, outside) in rows {
            for ip in inside {
                for addr in [Some(advertised(ip)), mapped(ip)].into_iter().flatten() {
                    assert_eq!(
                        classify_advertise(&addr),
                        AdvertiseReachability::NonPublic(class),
                        "{label}: {addr} is an edge of the prefix and must be rejected as {}",
                        class.as_str()
                    );
                }
            }
            for ip in outside {
                for addr in [Some(advertised(ip)), mapped(ip)].into_iter().flatten() {
                    assert_ne!(
                        classify_advertise(&addr),
                        AdvertiseReachability::NonPublic(class),
                        "{label}: {addr} is OUTSIDE the prefix and must not be caught by it -- a \
                         range one address too wide refuses a reachable seller"
                    );
                }
            }
        }

        // The neighbours which no screened class covers at all. Listed separately because the
        // assertion above deliberately says only "not this class", which the two adjacent ranges
        // above would otherwise let pass as public when they are not.
        for ip in [
            "1.0.0.0",
            "192.0.1.255",
            "192.0.3.0",
            "198.51.99.255",
            "198.51.101.0",
            "203.0.112.255",
            "203.0.114.0",
            "198.17.255.255",
            "198.20.0.0",
            "223.255.255.255",
            "2001:db7:ffff:ffff:ffff:ffff:ffff:ffff",
            "2001:db9::",
            "3ffe:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "3fff:1000::",
            "feff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        ] {
            for addr in [Some(advertised(ip)), mapped(ip)].into_iter().flatten() {
                assert!(
                    advertise_is_public(&addr),
                    "{addr} sits just outside an added prefix and matches no screened class, so \
                     the classifier must keep accepting it"
                );
            }
        }
    }

    /// The refusal names the class it matched, for each class added.

    /// An operator who mistyped into a documentation range must not be told to check their
    /// firewall. Driven through `validate_advertise`, the operator-visible surface, so both the
    /// `NonPublicReason` the classifier chose and the string it renders are pinned -- the enum
    /// alone would let the two drift apart.

    /// E2E-ADV-08, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-ADV-08/L0
    #[test]
    fn the_refusal_names_the_added_class_it_matched() {
        for (addr, class, text) in [
            (
                "0.1.2.3:8443",
                NonPublicReason::ThisNetwork,
                "this-network (0.0.0.0/8)",
            ),
            (
                "192.0.2.1:8443",
                NonPublicReason::Documentation,
                "documentation-only (RFC 5737/3849/9637)",
            ),
            (
                "[2001:db8::1]:8443",
                NonPublicReason::Documentation,
                "documentation-only (RFC 5737/3849/9637)",
            ),
            (
                "198.18.0.1:8443",
                NonPublicReason::Benchmarking,
                "benchmarking (198.18.0.0/15)",
            ),
            (
                "240.0.0.1:8443",
                NonPublicReason::Reserved,
                "reserved for future use (240.0.0.0/4)",
            ),
            (
                "224.0.0.1:8443",
                NonPublicReason::Multicast,
                "multicast group",
            ),
            (
                "[ff02::1]:8443",
                NonPublicReason::Multicast,
                "multicast group",
            ),
        ] {
            assert_eq!(reason(addr), class, "{addr}");
            assert_eq!(class.as_str(), text, "{addr}");
            let refusal = validate_advertise(addr, false, false)
                .expect_err("an added class must fail closed")
                .to_string();
            assert!(
                refusal.contains(text) && refusal.contains(addr),
                "the refusal for {addr} must name the class it matched: {refusal}"
            );
        }
    }

    /// E2E-ADV-09 -- the switch that lets a seller advertise a local address changes the answer
    /// for local addresses and for nothing else.

    /// Setup: a list of addresses covering every local kind, ordinary internet addresses and
    /// names, and two strings that are neither. Do: ask for each, once with the switch off and
    /// once on. Observe: with the switch on every address is accepted, and with it off an address
    /// is accepted exactly when a buyer elsewhere could dial it.

    /// The equality is the assertion. Turning the switch on skips the address check entirely
    /// rather than loosening it, so if a second, unrelated reason to refuse is ever added it would
    /// be skipped too -- and that shows up here as the equality breaking.

    /// Not proven: that a malformed address is refused when the switch is on. Nothing here calls
    /// a malformed address bad in the first place -- with the switch off it is accepted as well --
    /// so the refusal that is owed does not exist at this layer to be skipped. The companion test
    /// in the command-line arguments shows where malformed addresses are actually stopped.

    /// E2E-ADV-09, `tests/e2e/test-specification.md`.
    #[test]
    fn the_private_advertise_opt_in_admits_exactly_the_non_public_set() {
        let corpus = [
            // Non-public: the six classes the flag exists for.
            "0.0.0.0:8443",
            "[::]:8443",
            "127.0.0.1:8443",
            "[::1]:8443",
            "10.1.2.3:8443",
            "172.16.0.1:8443",
            "192.168.1.10:8443",
            "[fd00::1]:8443",
            "169.254.10.1:8443",
            "[fe80::1]:8443",
            "100.64.0.1:8443",
            "localhost:8443",
            "seller.local:8443",
            "box.internal:8443",
            "box.home.arpa:8443",
            "[::ffff:192.168.1.10]:8443",
            // Public: admitted with or without the flag.
            "94.156.178.14:8443",
            "8.8.8.8:443",
            "172.15.0.1:8443",
            "100.128.0.1:8443",
            "[2606:4700::1111]:443",
            "seller.example.net:443",
            // Public by fall-through rather than by intent: `999.999.999.999` is not a parseable
            // IP and reaches `classify_name`. The documentation/test networks are deliberately
            // absent: E2E-ADV-07's single canonical oracle classifies them as non-public.
            "999.999.999.999:8443",
        ];

        for addr in corpus {
            let public = advertise_is_public(addr);
            let default = validate_advertise(addr, false, false);
            let opted_in = validate_advertise(addr, false, true);

            assert!(
                opted_in.is_ok(),
                "{addr}: the opt-in admits every address class"
            );
            // The flag's whole effect, stated as a set difference: default admission is EXACTLY
            // publicness, and the opt-in adds exactly the complement. If `validate_advertise` ever
            // refuses an address the classifier calls public -- a syntactic check being the
            // obvious candidate -- this equality breaks, and it breaks here rather than silently
            // inside the `allow_private` early return.
            assert_eq!(
                default.is_ok(),
                public,
                "{addr}: default admission diverged from publicness, so the opt-in's early return \
                 at advertise.rs:169 now skips a refusal reason that is not about address class"
            );
        }
    }

    #[test]
    fn rejection_message_matches_the_750_shape() {
        let explicit = validate_advertise("192.168.1.10:8443", false, false)
            .expect_err("private advertise must fail");
        assert_eq!(
            explicit.to_string(),
            "error[E_ADVERTISE_NOT_PUBLIC] (config): --gateway-advertise 192.168.1.10:8443 is not \
             reachable by remote buyers (private (RFC1918/ULA))\n  \
             hint: pass a public host:port reachable from the internet, or run on a public host; \
             for local/LAN testing only, use --allow-private-advertise"
        );

        let defaulted = validate_advertise("0.0.0.0:8443", true, false)
            .expect_err("bind-all advertise must fail");
        assert_eq!(
            defaulted.to_string(),
            "error[E_ADVERTISE_NOT_PUBLIC] (config): --gateway-advertise defaulted to \
             --gateway-listen 0.0.0.0:8443, which is not reachable by remote buyers \
             (bind-all wildcard)\n  \
             hint: pass a public host:port reachable from the internet, or run on a public host; \
             for local/LAN testing only, use --allow-private-advertise"
        );
    }
}
