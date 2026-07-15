use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const WORKSPACE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const FRAME_MODEL: &str = "dexdo-mock";
const TOKEN_CONTRACT: &str = "0:0000000000000000000000000000000000000000000000000000000000000203";
const DEAL_HANDLE: &str = "deal-0-0000000000000000000000000000000000000000000000000000000000000203";
const SELLER_SECRET: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const BUYER_SECRET: &str = "2222222222222222222222222222222222222222222222222222222222222222";

struct TempDirCleanup(PathBuf);

impl Drop for TempDirCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct ChildGuard {
    child: Child,
    log: PathBuf,
    label: &'static str,
}

impl ChildGuard {
    fn assert_running(&mut self, context: &str) {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                let log = std::fs::read_to_string(&self.log).unwrap_or_default();
                panic!(
                    "{} exited during {context}: status={status}\n{}",
                    self.label, log
                );
            }
            Ok(None) => {}
            Err(e) => panic!("{} try_wait failed during {context}: {e}", self.label),
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn create_private_temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&dir).expect("create private temp dir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("chmod private temp dir");
    }
    dir
}

fn write_key(dir: &Path, name: &str, hex: &str) -> PathBuf {
    let p = dir.join(name);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&p).expect("create key file");
    f.write_all(hex.as_bytes()).expect("write key");
    p
}

fn free_loopback_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind loopback probe")
        .local_addr()
        .expect("probe local addr")
        .port()
}

fn dexdo() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dexdo"));
    cmd.current_dir(WORKSPACE);
    cmd
}

fn successful_stdout(out: std::process::Output, label: &str) -> String {
    assert!(
        out.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("stdout utf8")
}

fn failed_stdout(out: std::process::Output, label: &str) -> String {
    assert!(
        !out.status.success(),
        "{label} unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("stdout utf8")
}

fn parse_json_line(stdout: &str, label: &str) -> Value {
    let mut lines = stdout.lines().filter(|line| !line.trim().is_empty());
    let first = lines
        .next()
        .unwrap_or_else(|| panic!("{label}: empty stdout"));
    assert!(
        lines.next().is_none(),
        "{label}: expected one JSON line, got\n{stdout}"
    );
    serde_json::from_str(first).unwrap_or_else(|e| panic!("{label}: invalid JSON: {e}\n{stdout}"))
}

fn parse_json_lines(stdout: &str, label: &str) -> Vec<Value> {
    let lines = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    assert!(!lines.is_empty(), "{label}: empty stdout");
    lines
        .into_iter()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("{label}: invalid JSONL line: {e}\n{line}"))
        })
        .collect()
}

fn required_buyer_event_fields(event: &str) -> &'static [&'static str] {
    match event {
        "starting" => &[
            "network",
            "frame_model",
            "mode",
            "requested_bind_addr",
            "anthropic_compat",
            "continuity_mode",
        ],
        "quote_selected" => &[
            "frame_model",
            "model_hash",
            "order_book",
            "ticks",
            "max_price_per_tick",
            "escrow",
            "quote_complete",
            "fills",
        ],
        "resume_selected" => &[
            "token_contract",
            "role",
            "source",
            "deal_handle",
            "frame_model",
        ],
        "buy_submitted" => &[
            "frame_model",
            "order_book",
            "ticks",
            "max_price_per_tick",
            "escrow",
        ],
        "matched" => &["frame_model", "order_book", "token_contract"],
        "handover_waiting" => &["token_contract", "deadline_unix", "poll_interval_ms"],
        "handover_received" => &["token_contract", "deal_handle", "handover_anchor"],
        "endpoint_binding" => &[
            "token_contract",
            "deal_handle",
            "requested_bind_addr",
            "allow_port_zero",
        ],
        "endpoint_ready" => &[
            "token_contract",
            "deal_handle",
            "bind_addr",
            "base_url",
            "models_url",
            "served_models",
            "anthropic_compat",
        ],
        "stopping" => &["token_contract", "deal_handle", "reason"],
        "settlement_submitted" => &[
            "token_contract",
            "deal_handle",
            "role",
            "action",
            "submitted",
        ],
        "settled" => &[
            "token_contract",
            "deal_handle",
            "role",
            "action",
            "state",
            "terminal",
        ],
        "exiting" => &["token_contract", "deal_handle", "outcome", "exit_code"],
        "error" => &["code", "message", "retryable"],
        _ => &[],
    }
}

fn validate_required_buyer_event_fields(v: &Value) -> Result<(), String> {
    for field in [
        "schema",
        "seq",
        "ts_unix",
        "session_id",
        "operation",
        "event",
    ] {
        let has_field = v.as_object().is_some_and(|obj| obj.contains_key(field));
        if !has_field || v[field].is_null() {
            return Err(format!(
                "buyer envelope missing required field {field}: {v}"
            ));
        }
    }
    let event = v["event"]
        .as_str()
        .ok_or_else(|| format!("event name missing: {v}"))?;
    for field in required_buyer_event_fields(event) {
        let has_field = v.as_object().is_some_and(|obj| obj.contains_key(*field));
        if !has_field || v[*field].is_null() {
            return Err(format!("{event} missing required field {field}: {v}"));
        }
    }
    Ok(())
}

fn assert_buyer_event_fields(v: &Value) {
    validate_required_buyer_event_fields(v)
        .unwrap_or_else(|e| panic!("buyer event required-field validation failed: {e}"));
}

fn assert_buyer_error_jsonl(v: &Value, code: &str) {
    assert_eq!(v["schema"], "dexdo.error.v1");
    assert_eq!(v["event"], "error");
    assert_eq!(v["operation"], "buyer_start");
    assert_eq!(v["code"], code);
    assert!(v["seq"].as_u64().is_some(), "missing seq: {v}");
    assert!(v["ts_unix"].as_u64().is_some(), "missing ts_unix: {v}");
    assert!(
        v["session_id"]
            .as_str()
            .is_some_and(|s| s.starts_with("buyer-")),
        "missing buyer session_id: {v}"
    );
    assert!(v["message"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(v["retryable"].as_bool().is_some());
    assert_buyer_event_fields(v);
}

struct BuyerReady {
    ready: Value,
    events: Vec<Value>,
}

fn assert_buyer_startup_contract(events: &[Value]) {
    let mut last_seq = 0u64;
    for event in events {
        assert_buyer_event_fields(event);
        let seq = event["seq"]
            .as_u64()
            .unwrap_or_else(|| panic!("event seq missing: {event}"));
        assert!(seq > last_seq, "event seq not increasing: {events:?}");
        last_seq = seq;
    }
    let names = events
        .iter()
        .map(|v| v["event"].as_str().unwrap_or(""))
        .collect::<Vec<_>>();
    let expected = [
        "starting",
        "quote_selected",
        "buy_submitted",
        "matched",
        "handover_waiting",
        "handover_received",
        "endpoint_binding",
        "endpoint_ready",
    ];
    let mut cursor = 0usize;
    for want in expected {
        let offset = names[cursor..]
            .iter()
            .position(|name| *name == want)
            .unwrap_or_else(|| panic!("missing startup event {want}; saw {names:?}"));
        cursor += offset + 1;
    }
    let quote = events
        .iter()
        .find(|v| v["event"] == "quote_selected")
        .expect("quote_selected event");
    assert_eq!(quote["quote_complete"], true);
    assert!(
        quote["fills"]
            .as_array()
            .is_some_and(|fills| !fills.is_empty()),
        "quote_selected must carry selected fills: {quote}"
    );
}

fn assert_no_machine_leak(text: &str, forbidden: &[String]) {
    for raw in forbidden {
        assert!(
            !raw.is_empty() && !text.contains(raw),
            "machine output leaked forbidden fragment `{raw}`:\n{text}"
        );
    }
    for human in ["seller_ready", "matched deal TokenContract", "placing buy:"] {
        assert!(
            !text.contains(human),
            "machine output contains human log marker `{human}`:\n{text}"
        );
    }
}

fn spawn_seller(
    endpoints: &Path,
    deals_dir: &Path,
    note_key: &Path,
    gateway_addr: &str,
    log: &Path,
) -> ChildGuard {
    let log_file = std::fs::File::create(log).expect("seller log");
    let child = dexdo()
        .args([
            "seller",
            "--mock-chain",
            "--mock-model",
            "--endpoints-file",
            endpoints.to_str().unwrap(),
            "--deals-dir",
            deals_dir.to_str().unwrap(),
            "--note-key",
            note_key.to_str().unwrap(),
            "--token-contract",
            TOKEN_CONTRACT,
            "--gateway-listen",
            gateway_addr,
            "--model",
            FRAME_MODEL,
            "--price-per-tick",
            "1000",
            "--mock-token-count",
            "3",
        ])
        .env("RUST_LOG", "info")
        .stdout(Stdio::from(log_file.try_clone().unwrap()))
        .stderr(Stdio::from(log_file))
        .spawn()
        .expect("spawn seller");
    ChildGuard {
        child,
        log: log.to_path_buf(),
        label: "seller",
    }
}

fn poll_markets_json(seller: &mut ChildGuard, endpoints: &Path, forbidden: &[String]) -> Value {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        seller.assert_running("market discovery");
        let stdout = successful_stdout(
            dexdo()
                .args([
                    "markets",
                    "--json",
                    "--mock-chain",
                    "--endpoints-file",
                    endpoints.to_str().unwrap(),
                    "--frame-model",
                    FRAME_MODEL,
                ])
                .output()
                .expect("run markets --json"),
            "markets --json",
        );
        assert_no_machine_leak(&stdout, forbidden);
        let v = parse_json_line(&stdout, "markets --json");
        assert_eq!(v["schema"], "dexdo.markets.v1");
        if v["markets"]
            .as_array()
            .and_then(|markets| markets.first())
            .and_then(|market| market["ask_count"].as_u64())
            .unwrap_or(0)
            >= 1
        {
            return v;
        }
        assert!(
            Instant::now() < deadline,
            "seller offer did not appear in markets JSON"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn run_quote_json(endpoints: &Path, forbidden: &[String]) -> Value {
    let stdout = successful_stdout(
        dexdo()
            .args([
                "quote",
                "--json",
                "--mock-chain",
                "--endpoints-file",
                endpoints.to_str().unwrap(),
                "--model",
                FRAME_MODEL,
                "--ticks",
                "1",
            ])
            .output()
            .expect("run quote --json"),
        "quote --json",
    );
    assert_no_machine_leak(&stdout, forbidden);
    let v = parse_json_line(&stdout, "quote --json");
    assert_eq!(v["schema"], "dexdo.quote.v1");
    assert_eq!(v["frame_model"], FRAME_MODEL);
    assert_eq!(v["complete"], true);
    v
}

fn spawn_buyer(
    endpoints: &Path,
    deals_dir: &Path,
    note_key: &Path,
    log: &Path,
) -> (ChildGuard, mpsc::Receiver<String>) {
    let log_file = std::fs::File::create(log).expect("buyer log");
    let mut child = dexdo()
        .args([
            "buyer",
            "--json",
            "--mock-chain",
            "--mock-model",
            "--endpoints-file",
            endpoints.to_str().unwrap(),
            "--deals-dir",
            deals_dir.to_str().unwrap(),
            "--note-key",
            note_key.to_str().unwrap(),
            "--token-contract",
            TOKEN_CONTRACT,
            "--frame-model",
            FRAME_MODEL,
            "--local-listen",
            "127.0.0.1:0",
            "--ticks",
            "2",
            "--max-price-per-tick",
            "1000",
            "--max-tokens",
            "2",
        ])
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log_file))
        .spawn()
        .expect("spawn buyer");
    let stdout = child.stdout.take().expect("buyer stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else {
                break;
            };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    (
        ChildGuard {
            child,
            log: log.to_path_buf(),
            label: "buyer",
        },
        rx,
    )
}

fn spawn_buyer_on_demand(
    endpoints: &Path,
    deals_dir: &Path,
    note_key: &Path,
    log: &Path,
) -> (ChildGuard, mpsc::Receiver<String>) {
    spawn_buyer_on_demand_with_ticks(endpoints, deals_dir, note_key, log, "2")
}

fn spawn_buyer_on_demand_with_ticks(
    endpoints: &Path,
    deals_dir: &Path,
    note_key: &Path,
    log: &Path,
    ticks: &str,
) -> (ChildGuard, mpsc::Receiver<String>) {
    let log_file = std::fs::File::create(log).expect("buyer log");
    let mut child = dexdo()
        .args([
            "buyer",
            "--json",
            "--mock-chain",
            "--mock-model",
            "--endpoints-file",
            endpoints.to_str().unwrap(),
            "--deals-dir",
            deals_dir.to_str().unwrap(),
            "--note-key",
            note_key.to_str().unwrap(),
            "--token-contract",
            TOKEN_CONTRACT,
            "--frame-model",
            FRAME_MODEL,
            "--local-listen",
            "127.0.0.1:0",
            "--continuity-mode",
            "on-demand",
            "--ticks",
            ticks,
            "--max-price-per-tick",
            "1000",
            "--max-tokens",
            "2",
        ])
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log_file))
        .spawn()
        .expect("spawn on-demand buyer");
    let stdout = child.stdout.take().expect("buyer stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else {
                break;
            };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    (
        ChildGuard {
            child,
            log: log.to_path_buf(),
            label: "buyer-on-demand",
        },
        rx,
    )
}

fn wait_endpoint_ready(
    buyer: &mut ChildGuard,
    lines: &mpsc::Receiver<String>,
    forbidden: &[String],
) -> BuyerReady {
    let deadline = Instant::now() + Duration::from_secs(70);
    let mut seen = Vec::new();
    loop {
        match lines.recv_timeout(Duration::from_millis(250)) {
            Ok(line) => {
                assert_no_machine_leak(&line, forbidden);
                let v: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("buyer JSONL line is invalid: {e}\n{line}"));
                assert!(
                    matches!(
                        v["schema"].as_str(),
                        Some("dexdo.buyer.event.v1") | Some("dexdo.error.v1")
                    ),
                    "unexpected buyer schema: {v}"
                );
                if v["schema"] == "dexdo.error.v1" {
                    panic!("buyer emitted structured error before endpoint_ready: {v}");
                }
                assert_buyer_event_fields(&v);
                if v["event"] == "endpoint_ready" {
                    assert_eq!(v["operation"], "buyer_runtime");
                    assert_ne!(v["bind_addr"].as_str().unwrap_or(""), "127.0.0.1:0");
                    assert_eq!(v["served_models"][0], FRAME_MODEL);
                    seen.push(v.clone());
                    assert_buyer_startup_contract(&seen);
                    return BuyerReady {
                        ready: v,
                        events: seen,
                    };
                }
                seen.push(v);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(Some(status)) = buyer.child.try_wait() {
                    let log = std::fs::read_to_string(&buyer.log).unwrap_or_default();
                    panic!(
                        "buyer exited before endpoint_ready: status={status}\nseen={seen:?}\nlog={log}"
                    );
                }
                if Instant::now() >= deadline {
                    let log = std::fs::read_to_string(&buyer.log).unwrap_or_default();
                    panic!("timed out waiting endpoint_ready\nseen={seen:?}\nlog={log}");
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let log = std::fs::read_to_string(&buyer.log).unwrap_or_default();
                panic!("buyer stdout closed before endpoint_ready\nseen={seen:?}\nlog={log}");
            }
        }
    }
}

fn wait_on_demand_endpoint_ready(
    buyer: &mut ChildGuard,
    lines: &mpsc::Receiver<String>,
    forbidden: &[String],
) -> BuyerReady {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = Vec::new();
    loop {
        match lines.recv_timeout(Duration::from_millis(250)) {
            Ok(line) => {
                assert_no_machine_leak(&line, forbidden);
                let v: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("buyer JSONL line is invalid: {e}\n{line}"));
                assert!(
                    matches!(
                        v["schema"].as_str(),
                        Some("dexdo.buyer.event.v1") | Some("dexdo.error.v1")
                    ),
                    "unexpected buyer schema: {v}"
                );
                if v["schema"] == "dexdo.error.v1" {
                    panic!("buyer emitted structured error before endpoint_ready: {v}");
                }
                assert_buyer_event_fields(&v);
                if v["event"] == "endpoint_ready" {
                    assert_eq!(v["operation"], "buyer_runtime");
                    assert_ne!(v["bind_addr"].as_str().unwrap_or(""), "127.0.0.1:0");
                    assert_eq!(v["served_models"][0], FRAME_MODEL);
                    assert_eq!(v["token_contract"], "pending:on-demand");
                    seen.push(v.clone());
                    let names = seen
                        .iter()
                        .map(|event| event["event"].as_str().unwrap_or(""))
                        .collect::<Vec<_>>();
                    assert!(
                        names.starts_with(&["starting", "endpoint_binding", "endpoint_ready"]),
                        "on-demand buyer must bind before purchase events; saw {names:?}"
                    );
                    for forbidden_event in ["quote_selected", "buy_submitted", "matched"] {
                        assert!(
                            !names.contains(&forbidden_event),
                            "on-demand startup ran purchase event {forbidden_event} before endpoint_ready: {names:?}"
                        );
                    }
                    return BuyerReady {
                        ready: v,
                        events: seen,
                    };
                }
                seen.push(v);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(Some(status)) = buyer.child.try_wait() {
                    let log = std::fs::read_to_string(&buyer.log).unwrap_or_default();
                    panic!(
                        "buyer exited before endpoint_ready: status={status}\nseen={seen:?}\nlog={log}"
                    );
                }
                if Instant::now() >= deadline {
                    let log = std::fs::read_to_string(&buyer.log).unwrap_or_default();
                    panic!("timed out waiting endpoint_ready\nseen={seen:?}\nlog={log}");
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let log = std::fs::read_to_string(&buyer.log).unwrap_or_default();
                panic!("buyer stdout closed before endpoint_ready\nseen={seen:?}\nlog={log}");
            }
        }
    }
}

fn http_get_json(url: &str) -> Value {
    let rest = url
        .strip_prefix("http://")
        .unwrap_or_else(|| panic!("only http URLs are expected in fixture: {url}"));
    let (host, path) = match rest.split_once('/') {
        Some((host, path)) => (host, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    let addr: SocketAddr = host.parse().expect("loopback socket addr");
    let mut stream =
        TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect loopback API");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .expect("write request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    assert!(
        response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200"),
        "unexpected HTTP response:\n{response}"
    );
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .expect("HTTP response body");
    serde_json::from_str(body).unwrap_or_else(|e| panic!("invalid HTTP JSON: {e}\n{body}"))
}

fn http_post_json(url: &str, request: Value) -> (u16, Value) {
    let rest = url
        .strip_prefix("http://")
        .unwrap_or_else(|| panic!("only http URLs are expected in fixture: {url}"));
    let (host, path) = match rest.split_once('/') {
        Some((host, path)) => (host, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    let addr: SocketAddr = host.parse().expect("loopback socket addr");
    let mut stream =
        TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect loopback API");
    stream
        .set_read_timeout(Some(Duration::from_secs(70)))
        .expect("read timeout");
    let body = request.to_string();
    stream
        .write_all(
            format!(
                "POST {path} HTTP/1.1\r\n\
                 Host: {host}\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n\
                 {body}",
                body.len()
            )
            .as_bytes(),
        )
        .expect("write request");
    let mut bytes = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(70);
    loop {
        let mut chunk = [0u8; 4096];
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                bytes.extend_from_slice(&chunk[..n]);
                if http_response_body_complete(&bytes) {
                    break;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if http_response_body_complete(&bytes) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out reading HTTP response:\n{}",
                    String::from_utf8_lossy(&bytes)
                );
            }
            Err(e) => panic!("read response: {e}"),
        }
    }
    let response = String::from_utf8(bytes).expect("HTTP response utf8");
    let status_line = response
        .lines()
        .next()
        .unwrap_or_else(|| panic!("missing HTTP status line:\n{response}"));
    let status = status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_else(|| panic!("missing HTTP status code:\n{response}"))
        .parse::<u16>()
        .unwrap_or_else(|e| panic!("invalid HTTP status code: {e}\n{response}"));
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .expect("HTTP response body");
    let json = serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("invalid HTTP JSON: {e}\n{body}\nfull response:\n{response}"));
    (status, json)
}

fn http_response_body_complete(bytes: &[u8]) -> bool {
    let Some(header_end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            value.trim().parse::<usize>().ok()
        } else {
            None
        }
    });
    match content_length {
        Some(len) => bytes.len().saturating_sub(header_end + 4) >= len,
        None => false,
    }
}

fn collect_buyer_events_until(
    buyer: &mut ChildGuard,
    lines: &mpsc::Receiver<String>,
    forbidden: &[String],
    wanted: &[&str],
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut seen = Vec::new();
    loop {
        match lines.recv_timeout(Duration::from_millis(250)) {
            Ok(line) => {
                assert_no_machine_leak(&line, forbidden);
                let v: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("buyer JSONL line is invalid: {e}\n{line}"));
                if v["schema"] == "dexdo.error.v1" {
                    panic!("buyer emitted structured error during on-demand chat: {v}");
                }
                assert_buyer_event_fields(&v);
                seen.push(v);
                let names = seen
                    .iter()
                    .map(|event| event["event"].as_str().unwrap_or(""))
                    .collect::<Vec<_>>();
                if wanted.iter().all(|want| names.contains(want)) {
                    return seen;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(Some(status)) = buyer.child.try_wait() {
                    let log = std::fs::read_to_string(&buyer.log).unwrap_or_default();
                    panic!(
                        "buyer exited while waiting for events {wanted:?}: status={status}\nseen={seen:?}\nlog={log}"
                    );
                }
                if Instant::now() >= deadline {
                    let log = std::fs::read_to_string(&buyer.log).unwrap_or_default();
                    panic!("timed out waiting for events {wanted:?}\nseen={seen:?}\nlog={log}");
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let log = std::fs::read_to_string(&buyer.log).unwrap_or_default();
                panic!("buyer stdout closed while waiting for events {wanted:?}\nseen={seen:?}\nlog={log}");
            }
        }
    }
}

fn run_close_status_json(
    endpoints: &Path,
    deals_dir: &Path,
    deal_handle: &str,
    forbidden: &[String],
) {
    let close_stdout = successful_stdout(
        dexdo()
            .args([
                "close",
                "--json",
                "--mock-chain",
                "--endpoints-file",
                endpoints.to_str().unwrap(),
                "--deals-dir",
                deals_dir.to_str().unwrap(),
                deal_handle,
            ])
            .output()
            .expect("run close --json"),
        "close --json",
    );
    assert_no_machine_leak(&close_stdout, forbidden);
    let close = parse_json_line(&close_stdout, "close --json");
    assert_eq!(close["schema"], "dexdo.close.v1");
    assert_eq!(close["token_contract"], TOKEN_CONTRACT);
    assert_eq!(close["handle"], deal_handle);
    assert_eq!(close["role"], "buyer");
    assert!(close["submitted"].as_bool().is_some());
    assert!(close["terminal"].as_bool().is_some());

    let status_stdout = successful_stdout(
        dexdo()
            .args([
                "status",
                "--json",
                "--mock-chain",
                "--endpoints-file",
                endpoints.to_str().unwrap(),
                "--deals-dir",
                deals_dir.to_str().unwrap(),
                deal_handle,
            ])
            .output()
            .expect("run status --json"),
        "status --json",
    );
    assert_no_machine_leak(&status_stdout, forbidden);
    let status = parse_json_line(&status_stdout, "status --json");
    assert_eq!(status["schema"], "dexdo.status.v1");
    assert_eq!(status["token_contract"], TOKEN_CONTRACT);
    assert_eq!(status["handle"], deal_handle);
    assert!(matches!(
        status["state"].as_str(),
        Some("stopped") | Some("closed")
    ));
}

#[test]
fn parent_process_discovers_quotes_serves_models_and_closes_without_human_log_parsing() {
    let dir = create_private_temp_dir("dexdo-runtime-json-203");
    let _cleanup = TempDirCleanup(dir.clone());
    let endpoints = dir.join("endpoints.json");
    let deals_dir = dir.join("deals");
    std::fs::create_dir(&deals_dir).expect("create deals dir");
    let seller_key = write_key(&dir, "seller.key", SELLER_SECRET);
    let buyer_key = write_key(&dir, "buyer.key", BUYER_SECRET);
    let forbidden = vec![
        SELLER_SECRET.to_string(),
        BUYER_SECRET.to_string(),
        endpoints.display().to_string(),
        seller_key.display().to_string(),
        buyer_key.display().to_string(),
    ];

    let gateway_addr = format!("127.0.0.1:{}", free_loopback_port());
    let seller_log = dir.join("seller.log");
    let mut seller = spawn_seller(
        &endpoints,
        &deals_dir,
        &seller_key,
        &gateway_addr,
        &seller_log,
    );

    let markets = poll_markets_json(&mut seller, &endpoints, &forbidden);
    assert_eq!(markets["markets"][0]["frame_model"], FRAME_MODEL);
    assert_eq!(markets["markets"][0]["best_ask"], "1000");

    let quote = run_quote_json(&endpoints, &forbidden);
    assert_eq!(quote["filled_ticks"], "1");
    assert_eq!(quote["no_liquidity"], false);

    let buyer_log = dir.join("buyer.log");
    let (mut buyer, buyer_lines) = spawn_buyer(&endpoints, &deals_dir, &buyer_key, &buyer_log);
    let observed = wait_endpoint_ready(&mut buyer, &buyer_lines, &forbidden);
    let ready = observed.ready;
    assert!(observed.events.len() >= 8);
    let deal_handle = ready["deal_handle"]
        .as_str()
        .expect("endpoint_ready deal_handle");
    assert!(deals_dir.join(format!("{deal_handle}.json")).exists());
    let models = http_get_json(ready["models_url"].as_str().expect("models_url"));
    assert_eq!(models["data"][0]["id"], FRAME_MODEL);

    run_close_status_json(&endpoints, &deals_dir, deal_handle, &forbidden);
}

#[test]
fn buyer_on_demand_binds_models_before_purchase_work() {
    let dir = create_private_temp_dir("dexdo-runtime-json-309");
    let _cleanup = TempDirCleanup(dir.clone());
    let endpoints = dir.join("endpoints.json");
    let deals_dir = dir.join("deals");
    std::fs::create_dir(&deals_dir).expect("create deals dir");
    let buyer_key = write_key(&dir, "buyer.key", BUYER_SECRET);
    let forbidden = vec![
        BUYER_SECRET.to_string(),
        endpoints.display().to_string(),
        buyer_key.display().to_string(),
    ];

    let buyer_log = dir.join("buyer.log");
    let (mut buyer, buyer_lines) =
        spawn_buyer_on_demand(&endpoints, &deals_dir, &buyer_key, &buyer_log);
    let observed = wait_on_demand_endpoint_ready(&mut buyer, &buyer_lines, &forbidden);
    let ready = observed.ready;
    assert_eq!(observed.events.len(), 3, "{:?}", observed.events);
    assert!(
        !deals_dir.join(format!("{DEAL_HANDLE}.json")).exists(),
        "on-demand startup must not create a deal handle before the first chat request"
    );
    let models = http_get_json(ready["models_url"].as_str().expect("models_url"));
    assert_eq!(models["data"][0]["id"], FRAME_MODEL);
    buyer.assert_running("on-demand endpoint ready before purchase");
}

#[test]
fn buyer_on_demand_first_chat_reaches_stream_after_handover_without_invalid_argument() {
    let dir = create_private_temp_dir("dexdo-runtime-json-323");
    let _cleanup = TempDirCleanup(dir.clone());
    let endpoints = dir.join("endpoints.json");
    let deals_dir = dir.join("deals");
    std::fs::create_dir(&deals_dir).expect("create deals dir");
    let seller_key = write_key(&dir, "seller.key", SELLER_SECRET);
    let buyer_key = write_key(&dir, "buyer.key", BUYER_SECRET);
    let forbidden = vec![
        SELLER_SECRET.to_string(),
        BUYER_SECRET.to_string(),
        endpoints.display().to_string(),
        seller_key.display().to_string(),
        buyer_key.display().to_string(),
    ];

    let gateway_addr = format!("127.0.0.1:{}", free_loopback_port());
    let seller_log = dir.join("seller.log");
    let mut seller = spawn_seller(
        &endpoints,
        &deals_dir,
        &seller_key,
        &gateway_addr,
        &seller_log,
    );
    let markets = poll_markets_json(&mut seller, &endpoints, &forbidden);
    assert_eq!(markets["markets"][0]["frame_model"], FRAME_MODEL);

    let buyer_log = dir.join("buyer.log");
    let (mut buyer, buyer_lines) =
        spawn_buyer_on_demand(&endpoints, &deals_dir, &buyer_key, &buyer_log);
    let observed = wait_on_demand_endpoint_ready(&mut buyer, &buyer_lines, &forbidden);
    let base_url = observed.ready["base_url"]
        .as_str()
        .expect("endpoint_ready base_url");

    let (status, body) = http_post_json(
        &format!("{base_url}/chat/completions"),
        serde_json::json!({
            "model": FRAME_MODEL,
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 1,
            "stream": false
        }),
    );
    assert_eq!(status, 200, "on-demand chat failed: {body}");
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(body["model"], FRAME_MODEL);
    assert!(
        body["choices"][0]["message"]["content"]
            .as_str()
            .is_some_and(|content| !content.is_empty()),
        "on-demand chat response did not include content: {body}"
    );

    let events = collect_buyer_events_until(
        &mut buyer,
        &buyer_lines,
        &forbidden,
        &[
            "quote_selected",
            "buy_submitted",
            "matched",
            "handover_received",
        ],
    );
    let names = events
        .iter()
        .map(|event| event["event"].as_str().unwrap_or(""))
        .collect::<Vec<_>>();
    assert!(
        names
            .iter()
            .position(|name| *name == "quote_selected")
            .zip(names.iter().position(|name| *name == "handover_received"))
            .is_some_and(|(quote, handover)| quote < handover),
        "unexpected on-demand event order after first chat: {names:?}"
    );
    assert!(deals_dir.join(format!("{DEAL_HANDLE}.json")).exists());
}

#[test]
fn buyer_on_demand_first_chat_rejects_one_tick_buy_without_internal_error() {
    let dir = create_private_temp_dir("dexdo-runtime-json-328");
    let _cleanup = TempDirCleanup(dir.clone());
    let endpoints = dir.join("endpoints.json");
    let deals_dir = dir.join("deals");
    std::fs::create_dir(&deals_dir).expect("create deals dir");
    let seller_key = write_key(&dir, "seller.key", SELLER_SECRET);
    let buyer_key = write_key(&dir, "buyer.key", BUYER_SECRET);
    let forbidden = vec![
        SELLER_SECRET.to_string(),
        BUYER_SECRET.to_string(),
        endpoints.display().to_string(),
        seller_key.display().to_string(),
        buyer_key.display().to_string(),
    ];

    let gateway_addr = format!("127.0.0.1:{}", free_loopback_port());
    let seller_log = dir.join("seller.log");
    let mut seller = spawn_seller(
        &endpoints,
        &deals_dir,
        &seller_key,
        &gateway_addr,
        &seller_log,
    );
    let markets = poll_markets_json(&mut seller, &endpoints, &forbidden);
    assert_eq!(markets["markets"][0]["frame_model"], FRAME_MODEL);

    let buyer_log = dir.join("buyer.log");
    let (mut buyer, buyer_lines) =
        spawn_buyer_on_demand_with_ticks(&endpoints, &deals_dir, &buyer_key, &buyer_log, "1");
    let observed = wait_on_demand_endpoint_ready(&mut buyer, &buyer_lines, &forbidden);
    let base_url = observed.ready["base_url"]
        .as_str()
        .expect("endpoint_ready base_url");

    let (status, body) = http_post_json(
        &format!("{base_url}/chat/completions"),
        serde_json::json!({
            "model": FRAME_MODEL,
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 1,
            "stream": false
        }),
    );
    assert_eq!(
        status, 400,
        "one-tick on-demand chat failed wrongly: {body}"
    );
    let message = body["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("missing error message: {body}"));
    assert!(message.contains("invalid buy ticks"), "{message}");
    assert!(message.contains("--ticks 1"), "{message}");
    assert!(message.contains("2-tick stream minimum"), "{message}");
    assert!(
        !message.contains("internal invariant failed"),
        "generic INTERNAL leaked into actionable error: {message}"
    );

    let events =
        collect_buyer_events_until(&mut buyer, &buyer_lines, &forbidden, &["purchase_progress"]);
    let names = events
        .iter()
        .map(|event| event["event"].as_str().unwrap_or(""))
        .collect::<Vec<_>>();
    assert!(
        !names.contains(&"quote_selected"),
        "one-tick buy must fail before quote/preflight: {names:?}"
    );
    assert!(
        !names.contains(&"buy_submitted"),
        "one-tick buy must fail before escrow submission: {names:?}"
    );
    assert!(
        !names.contains(&"matched"),
        "one-tick buy must fail before match: {names:?}"
    );
    assert!(
        !deals_dir.join(format!("{DEAL_HANDLE}.json")).exists(),
        "one-tick on-demand rejection must not create a deal handle"
    );
}

#[test]
fn buyer_event_required_field_validator_rejects_missing_fields() {
    let events = [
        "starting",
        "quote_selected",
        "buy_submitted",
        "matched",
        "handover_waiting",
        "handover_received",
        "endpoint_binding",
        "endpoint_ready",
        "stopping",
        "settlement_submitted",
        "settled",
        "exiting",
        "error",
    ];
    for event in events {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "schema".to_string(),
            Value::String("dexdo.buyer.event.v1".to_string()),
        );
        obj.insert("seq".to_string(), Value::from(1));
        obj.insert("ts_unix".to_string(), Value::from(1782910310u64));
        obj.insert(
            "session_id".to_string(),
            Value::String("buyer-test".to_string()),
        );
        obj.insert(
            "operation".to_string(),
            Value::String("buyer_start".to_string()),
        );
        obj.insert("event".to_string(), Value::String(event.to_string()));
        for field in required_buyer_event_fields(event) {
            obj.insert((*field).to_string(), Value::String("present".to_string()));
        }
        let valid = Value::Object(obj.clone());
        validate_required_buyer_event_fields(&valid)
            .unwrap_or_else(|e| panic!("{event} should validate before field removal: {e}"));
        for field in [
            "schema",
            "seq",
            "ts_unix",
            "session_id",
            "operation",
            "event",
        ]
        .into_iter()
        .chain(required_buyer_event_fields(event).iter().copied())
        {
            let mut missing = obj.clone();
            missing.remove(field);
            let missing = Value::Object(missing);
            assert!(
                validate_required_buyer_event_fields(&missing).is_err(),
                "{event} without {field} unexpectedly validated: {missing}"
            );
        }
    }
}

#[test]
fn buyer_json_no_liquidity_fails_before_buy_submission() {
    let dir = create_private_temp_dir("dexdo-runtime-json-203-no-liquidity");
    let _cleanup = TempDirCleanup(dir.clone());
    let endpoints = dir.join("endpoints.json");
    let deals_dir = dir.join("deals");
    std::fs::create_dir(&deals_dir).expect("create deals dir");
    let buyer_key = write_key(&dir, "buyer.key", BUYER_SECRET);
    let forbidden = vec![
        BUYER_SECRET.to_string(),
        endpoints.display().to_string(),
        buyer_key.display().to_string(),
    ];

    let out = dexdo()
        .args([
            "buyer",
            "--json",
            "--mock-chain",
            "--mock-model",
            "--endpoints-file",
            endpoints.to_str().unwrap(),
            "--deals-dir",
            deals_dir.to_str().unwrap(),
            "--note-key",
            buyer_key.to_str().unwrap(),
            "--token-contract",
            TOKEN_CONTRACT,
            "--frame-model",
            FRAME_MODEL,
            "--local-listen",
            "127.0.0.1:0",
            "--ticks",
            "2",
            "--max-price-per-tick",
            "1000",
        ])
        .output()
        .expect("run buyer --json no liquidity");
    assert!(
        !out.status.success(),
        "buyer --json unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    assert_no_machine_leak(&stdout, &forbidden);
    let events = parse_json_lines(&stdout, "buyer --json no liquidity");
    assert_eq!(events[0]["schema"], "dexdo.buyer.event.v1");
    assert_eq!(events[0]["event"], "starting");
    let error = events.last().unwrap();
    assert_buyer_error_jsonl(error, "NO_LIQUIDITY");
    assert_eq!(error["token_contract"], TOKEN_CONTRACT);
    assert_eq!(error["deal_handle"], DEAL_HANDLE);
    assert!(
        !events.iter().any(|v| matches!(
            v["event"].as_str(),
            Some("buy_submitted") | Some("matched") | Some("handover_waiting")
        )),
        "buyer emitted post-money events after NO_LIQUIDITY: {events:?}"
    );
    assert!(
        !endpoints.with_extension("chainstate.json").exists(),
        "no-liquidity buyer should not create mock chain state"
    );
}

#[test]
fn buyer_json_incomplete_quote_fails_before_buy_submission() {
    let dir = create_private_temp_dir("dexdo-runtime-json-203-incomplete");
    let _cleanup = TempDirCleanup(dir.clone());
    let endpoints = dir.join("endpoints.json");
    let deals_dir = dir.join("deals");
    std::fs::create_dir(&deals_dir).expect("create deals dir");
    let seller_key = write_key(&dir, "seller.key", SELLER_SECRET);
    let buyer_key = write_key(&dir, "buyer.key", BUYER_SECRET);
    let forbidden = vec![
        SELLER_SECRET.to_string(),
        BUYER_SECRET.to_string(),
        endpoints.display().to_string(),
        seller_key.display().to_string(),
        buyer_key.display().to_string(),
    ];

    let gateway_addr = format!("127.0.0.1:{}", free_loopback_port());
    let seller_log = dir.join("seller.log");
    let mut seller = spawn_seller(
        &endpoints,
        &deals_dir,
        &seller_key,
        &gateway_addr,
        &seller_log,
    );
    let markets = poll_markets_json(&mut seller, &endpoints, &forbidden);
    assert_eq!(markets["markets"][0]["depth_ticks"], "1024");

    let out = dexdo()
        .args([
            "buyer",
            "--json",
            "--mock-chain",
            "--mock-model",
            "--endpoints-file",
            endpoints.to_str().unwrap(),
            "--deals-dir",
            deals_dir.to_str().unwrap(),
            "--note-key",
            buyer_key.to_str().unwrap(),
            "--token-contract",
            TOKEN_CONTRACT,
            "--frame-model",
            FRAME_MODEL,
            "--local-listen",
            "127.0.0.1:0",
            "--ticks",
            "2048",
            "--max-price-per-tick",
            "1000",
        ])
        .output()
        .expect("run buyer --json incomplete quote");
    assert!(
        !out.status.success(),
        "buyer --json unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    assert_no_machine_leak(&stdout, &forbidden);
    let events = parse_json_lines(&stdout, "buyer --json incomplete quote");
    assert_eq!(events[0]["schema"], "dexdo.buyer.event.v1");
    assert_eq!(events[0]["event"], "starting");
    let error = events.last().unwrap();
    assert_buyer_error_jsonl(error, "INCOMPLETE_QUOTE");
    assert_eq!(error["token_contract"], TOKEN_CONTRACT);
    assert_eq!(error["deal_handle"], DEAL_HANDLE);
    assert_eq!(error["quote_complete"], false);
    assert!(
        !events.iter().any(|v| matches!(
            v["event"].as_str(),
            Some("buy_submitted") | Some("matched") | Some("handover_waiting")
        )),
        "buyer emitted post-money events after INCOMPLETE_QUOTE: {events:?}"
    );
    let state = std::fs::read_to_string(endpoints.with_extension("chainstate.json"))
        .expect("seller created mock chain state");
    let state: Value = serde_json::from_str(&state).expect("mock chain state json");
    assert_eq!(state["matches"].as_object().map(|m| m.len()), Some(0));
    assert_eq!(state["streams"].as_object().map(|m| m.len()), Some(0));
}

#[test]
fn quote_json_conflicting_size_is_structured_error_and_redacted() {
    let dir = create_private_temp_dir("dexdo-runtime-json-203-error");
    let _cleanup = TempDirCleanup(dir.clone());
    let endpoints = dir.join("endpoints.json");
    let secret_path = write_key(&dir, "unused.key", BUYER_SECRET);
    let forbidden = vec![
        BUYER_SECRET.to_string(),
        endpoints.display().to_string(),
        secret_path.display().to_string(),
    ];

    let stdout = failed_stdout(
        dexdo()
            .args([
                "quote",
                "--json",
                "--mock-chain",
                "--endpoints-file",
                endpoints.to_str().unwrap(),
                "--model",
                FRAME_MODEL,
                "--ticks",
                "1",
                "--budget",
                "1000",
            ])
            .output()
            .expect("run failing quote --json"),
        "quote --json conflicting size",
    );
    assert_no_machine_leak(&stdout, &forbidden);
    let error = parse_json_line(&stdout, "quote --json conflicting size");
    assert_eq!(error["schema"], "dexdo.error.v1");
    assert_eq!(error["operation"], "quote");
    assert_eq!(error["code"], "INVALID_ARGUMENT");
    assert_eq!(error["retryable"], false);
    assert!(!error["message"].as_str().unwrap_or("").is_empty());
}

#[test]
fn buyer_json_required_error_codes_are_command_surface_jsonl() {
    let dir = create_private_temp_dir("dexdo-runtime-json-203-error-codes");
    let _cleanup = TempDirCleanup(dir.clone());
    let endpoints = dir.join("endpoints.json");
    let deals_dir = dir.join("deals");
    std::fs::create_dir(&deals_dir).expect("create deals dir");
    let forbidden = vec![endpoints.display().to_string()];
    let codes = [
        "NO_LIQUIDITY",
        "INSUFFICIENT_BALANCE",
        "HANDOVER_TIMEOUT",
        "CHAIN_TRANSPORT",
        "SETTLEMENT_FAILED",
        "NOT_RECOVERABLE_YET",
        "DISPUTED_DEAL",
    ];

    for code in codes {
        let out = dexdo()
            .args([
                "buyer",
                "--json",
                "--mock-chain",
                "--mock-model",
                "--endpoints-file",
                endpoints.to_str().unwrap(),
                "--deals-dir",
                deals_dir.to_str().unwrap(),
                "--token-contract",
                TOKEN_CONTRACT,
                "--frame-model",
                FRAME_MODEL,
                "--local-listen",
                "127.0.0.1:0",
                "--ticks",
                "2",
                "--max-price-per-tick",
                "1000",
            ])
            .env("DEXDO_BUYER_JSON_ERROR_FIXTURE", code)
            .output()
            .unwrap_or_else(|e| panic!("run buyer --json fixture {code}: {e}"));
        assert!(
            !out.status.success(),
            "buyer --json fixture {code} unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
        assert_no_machine_leak(&stdout, &forbidden);
        let events = parse_json_lines(&stdout, &format!("buyer --json fixture {code}"));
        assert_eq!(
            events.len(),
            2,
            "expected starting + error for {code}: {events:?}"
        );
        assert_eq!(events[0]["schema"], "dexdo.buyer.event.v1");
        assert_eq!(events[0]["event"], "starting");
        let error = events.last().unwrap();
        assert_buyer_error_jsonl(error, code);
        assert_eq!(error["token_contract"], TOKEN_CONTRACT);
        assert_eq!(error["deal_handle"], DEAL_HANDLE);
    }
}

#[test]
fn parse_time_machine_failures_are_structured_error_and_redacted() {
    let dir = create_private_temp_dir("dexdo-runtime-json-203-parse");
    let _cleanup = TempDirCleanup(dir.clone());
    let endpoints = dir.join("endpoints.json");
    let forbidden = vec![endpoints.display().to_string()];

    let out = dexdo()
        .args([
            "quote",
            "--json",
            "--mock-chain",
            "--endpoints-file",
            endpoints.to_str().unwrap(),
            "--model",
            FRAME_MODEL,
            "--ticks",
        ])
        .output()
        .expect("run parse-failing quote --json");
    assert!(
        !out.status.success(),
        "parse-failing quote unexpectedly succeeded"
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let stderr = String::from_utf8(out.stderr).expect("stderr utf8");
    assert_no_machine_leak(&stdout, &forbidden);
    assert!(
        stderr.trim().is_empty(),
        "machine parse failure must not print raw clap text to stderr:\n{stderr}"
    );
    let error = parse_json_line(&stdout, "parse-failing quote --json");
    assert_eq!(error["schema"], "dexdo.error.v1");
    assert_eq!(error["operation"], "quote");
    assert_eq!(error["code"], "INVALID_ARGUMENT");

    let out = dexdo()
        .args(["buyer", "--json", "--definitely-not-a-flag"])
        .output()
        .expect("run parse-failing buyer --json");
    assert!(
        !out.status.success(),
        "parse-failing buyer unexpectedly succeeded"
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let stderr = String::from_utf8(out.stderr).expect("stderr utf8");
    assert_no_machine_leak(&stdout, &forbidden);
    assert!(
        stderr.trim().is_empty(),
        "buyer machine parse failure must not print raw clap text to stderr:\n{stderr}"
    );
    let error = parse_json_line(&stdout, "parse-failing buyer --json");
    assert_buyer_error_jsonl(&error, "INVALID_ARGUMENT");
}
