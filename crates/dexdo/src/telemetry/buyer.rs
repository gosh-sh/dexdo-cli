//! The buyer's instruments: its local HTTP server and its calls to the seller gateway. Installed
//! only by the session `dexdo buyer` starts; everywhere else every recording is a no-op. No label
//! carries an address, a key, a prompt or a deal identifier.
use axum::{
    body::{Body, Bytes},
    extract::{MatchedPath, Request},
    middleware::Next,
    response::Response,
};
use dexdo_core::params::{
    BUYER_METRICS_HTTP_SERVER_DURATION_BOUNDARIES, BUYER_METRICS_RPC_CLIENT_DURATION_BOUNDARIES,
};
use futures::Stream;
use opentelemetry::{
    metrics::{Counter, Histogram, Meter},
    KeyValue,
};
use std::{
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll},
    time::Instant,
};
use tokio_rustls::rustls::{AlertDescription, Error as RustlsError};

pub(super) static METRICS: OnceLock<Arc<Metrics>> = OnceLock::new();
/// The one model this buyer process buys; every series carries it as `model`.
static MODEL: OnceLock<String> = OnceLock::new();

/// Name the model this process buys, once it is fixed and before any request or gateway call. A
/// model a client names in its request never reaches a label.
pub fn set_model(model: &str) {
    let _ = MODEL.set(model.to_owned());
}

fn labels() -> Vec<KeyValue> {
    MODEL
        .get()
        .map(|model| KeyValue::new("model", model.clone()))
        .into_iter()
        .collect()
}

pub(super) struct Metrics {
    http_errors: Counter<u64>,
    http_duration: Histogram<f64>,
    connect_errors: Counter<u64>,
    connect_duration: Histogram<f64>,
    call_duration: Histogram<f64>,
    call_errors: Counter<u64>,
}

pub(super) fn install(meter: &Meter) {
    let seconds = |name: &'static str, boundaries: &[f64]| {
        meter
            .f64_histogram(name)
            .with_unit("s")
            .with_boundaries(boundaries.to_vec())
            .build()
    };
    let _ = METRICS.set(Arc::new(Metrics {
        http_errors: meter.u64_counter("dexdo.buyer.http.server.errors").build(),
        http_duration: seconds(
            "dexdo.buyer.http.server.request.duration",
            &BUYER_METRICS_HTTP_SERVER_DURATION_BOUNDARIES,
        ),
        connect_errors: meter
            .u64_counter("dexdo.buyer.rpc.client.connect.errors")
            .build(),
        connect_duration: seconds(
            "dexdo.buyer.rpc.client.connect.duration",
            &BUYER_METRICS_RPC_CLIENT_DURATION_BOUNDARIES,
        ),
        call_duration: seconds(
            "dexdo.buyer.rpc.client.call.duration",
            &BUYER_METRICS_RPC_CLIENT_DURATION_BOUNDARIES,
        ),
        call_errors: meter.u64_counter("dexdo.buyer.rpc.client.errors").build(),
    }));
}

/// Local HTTP server middleware: every answer with the OpenTelemetry `http.server.request.duration`
/// attributes, timed to the last byte of its body; answers of 400 and above are also counted.
pub async fn http_server(request: Request, next: Next) -> Response {
    let Some(metrics) = METRICS.get().cloned() else {
        return next.run(request).await;
    };
    let mut attributes = labels();
    attributes.extend([
        KeyValue::new("http.request.method", method(request.method())),
        KeyValue::new("url.scheme", "http"),
    ]);
    if let Some(version) = protocol_version(request.version()) {
        attributes.push(KeyValue::new("network.protocol.version", version));
    }
    // The matched route template only; a raw request path never becomes a label.
    if let Some(route) = request.extensions().get::<MatchedPath>() {
        attributes.push(KeyValue::new("http.route", route.as_str().to_owned()));
    }
    let mut exchange = HttpExchange {
        metrics,
        started: Instant::now(),
        attributes,
        status: None,
        recorded: false,
    };
    // Dropped here, before an answer exists, the request ends as `client_disconnected`.
    let response = next.run(request).await;
    exchange.status = Some(response.status());
    response.map(|body| Body::new(TimedBody { body, exchange }))
}

struct HttpExchange {
    metrics: Arc<Metrics>,
    started: Instant,
    attributes: Vec<KeyValue>,
    status: Option<http::StatusCode>,
    recorded: bool,
}
impl HttpExchange {
    fn record(&mut self) {
        if std::mem::replace(&mut self.recorded, true) {
            return;
        }
        let mut attributes = std::mem::take(&mut self.attributes);
        match self.status {
            Some(status) => {
                attributes.push(KeyValue::new(
                    "http.response.status_code",
                    i64::from(status.as_u16()),
                ));
                // By the convention a server error is 5xx only.
                if status.is_server_error() {
                    attributes.push(KeyValue::new("error.type", status.as_str().to_owned()));
                }
                if status.as_u16() >= 400 {
                    self.metrics.http_errors.add(1, &attributes);
                }
            }
            None => attributes.push(KeyValue::new("error.type", "client_disconnected")),
        }
        self.metrics
            .http_duration
            .record(self.started.elapsed().as_secs_f64(), &attributes);
    }
}
impl Drop for HttpExchange {
    fn drop(&mut self) {
        self.record()
    }
}

/// The answer body, unchanged. Its end, its error or its drop by a departing client ends the
/// exchange.
struct TimedBody {
    body: Body,
    exchange: HttpExchange,
}
impl http_body::Body for TimedBody {
    type Data = Bytes;
    type Error = axum::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, axum::Error>>> {
        let polled = Pin::new(&mut self.body).poll_frame(cx);
        if matches!(polled, Poll::Ready(None | Some(Err(_)))) {
            self.exchange.record();
        }
        polled
    }
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

/// A method RFC 9110 defines, or PATCH; any other is `_OTHER`.
fn method(method: &http::Method) -> &'static str {
    [
        "CONNECT", "DELETE", "GET", "HEAD", "OPTIONS", "PATCH", "POST", "PUT", "TRACE",
    ]
    .into_iter()
    .find(|known| *known == method.as_str())
    .unwrap_or("_OTHER")
}

fn protocol_version(version: http::Version) -> Option<&'static str> {
    [
        (http::Version::HTTP_09, "0.9"),
        (http::Version::HTTP_10, "1.0"),
        (http::Version::HTTP_11, "1.1"),
        (http::Version::HTTP_2, "2"),
        (http::Version::HTTP_3, "3"),
    ]
    .into_iter()
    .find(|(known, _)| *known == version)
    .map(|(_, name)| name)
}

/// One pinned dial of the seller gateway, from the TCP connect to a ready h2 channel or to the
/// failure, with the stage it failed at and the TLS alert the gateway sent, if any.
pub(crate) fn connect(started: Instant, failure: Option<&anyhow::Error>) {
    let Some(metrics) = METRICS.get() else {
        return;
    };
    let mut attributes = labels();
    if let Some(error) = failure {
        // A handover address that does not parse is no connection: nothing is recorded.
        let Some(stage) = failed_stage(error) else {
            return;
        };
        attributes.push(KeyValue::new("error.type", stage));
        if let Some(alert) = received_alert(error) {
            attributes.push(KeyValue::new("tls.alert", alert_name(alert)));
        }
        metrics.connect_errors.add(1, &attributes);
    }
    metrics
        .connect_duration
        .record(started.elapsed().as_secs_f64(), &attributes);
}

/// The stage a pinned dial failed at, or `None` when it failed before any connection was tried.
/// Only the dial itself returns a tonic transport error; untagged inside one, it is the h2
/// handshake.
fn failed_stage(error: &anyhow::Error) -> Option<&'static str> {
    if crate::buyer::tls::dial_reached_wrong_endpoint(error) {
        return Some("tls_certificate_pin");
    }
    error
        .chain()
        .any(|source| source.is::<tonic::transport::Error>())
        .then(|| crate::buyer::tls::dial_stage(error))
}

fn received_alert(error: &anyhow::Error) -> Option<AlertDescription> {
    error.chain().find_map(|source| {
        match source
            .downcast_ref::<std::io::Error>()?
            .get_ref()?
            .downcast_ref::<RustlsError>()?
        {
            RustlsError::AlertReceived(alert) => Some(*alert),
            _ => None,
        }
    })
}

/// The alert's name in the TLS alert registry; `_OTHER` for a code the registry does not assign.
fn alert_name(alert: AlertDescription) -> &'static str {
    match alert {
        AlertDescription::CloseNotify => "close_notify",
        AlertDescription::UnexpectedMessage => "unexpected_message",
        AlertDescription::BadRecordMac => "bad_record_mac",
        AlertDescription::DecryptionFailed => "decryption_failed",
        AlertDescription::RecordOverflow => "record_overflow",
        AlertDescription::DecompressionFailure => "decompression_failure",
        AlertDescription::HandshakeFailure => "handshake_failure",
        AlertDescription::NoCertificate => "no_certificate",
        AlertDescription::BadCertificate => "bad_certificate",
        AlertDescription::UnsupportedCertificate => "unsupported_certificate",
        AlertDescription::CertificateRevoked => "certificate_revoked",
        AlertDescription::CertificateExpired => "certificate_expired",
        AlertDescription::CertificateUnknown => "certificate_unknown",
        AlertDescription::IllegalParameter => "illegal_parameter",
        AlertDescription::UnknownCA => "unknown_ca",
        AlertDescription::AccessDenied => "access_denied",
        AlertDescription::DecodeError => "decode_error",
        AlertDescription::DecryptError => "decrypt_error",
        AlertDescription::ExportRestriction => "export_restriction",
        AlertDescription::ProtocolVersion => "protocol_version",
        AlertDescription::InsufficientSecurity => "insufficient_security",
        AlertDescription::InternalError => "internal_error",
        AlertDescription::InappropriateFallback => "inappropriate_fallback",
        AlertDescription::UserCanceled => "user_canceled",
        AlertDescription::NoRenegotiation => "no_renegotiation",
        AlertDescription::MissingExtension => "missing_extension",
        AlertDescription::UnsupportedExtension => "unsupported_extension",
        AlertDescription::CertificateUnobtainable => "certificate_unobtainable",
        AlertDescription::UnrecognisedName => "unrecognized_name",
        AlertDescription::BadCertificateStatusResponse => "bad_certificate_status_response",
        AlertDescription::BadCertificateHashValue => "bad_certificate_hash_value",
        AlertDescription::UnknownPSKIdentity => "unknown_psk_identity",
        AlertDescription::CertificateRequired => "certificate_required",
        AlertDescription::NoApplicationProtocol => "no_application_protocol",
        AlertDescription::EncryptedClientHelloRequired => "ech_required",
        _ => "_OTHER",
    }
}

/// One gRPC call to the seller gateway. It ends with the status the call ended with; dropped
/// before that, the buyer abandoned it, which gRPC states as `CANCELLED`.
pub struct Call {
    method: &'static str,
    purpose: Option<&'static str>,
    started: Instant,
    ended: bool,
}
impl Call {
    /// `purpose`: `authorize` for the nonce that signs a request, `capabilities` for the deal's
    /// path query.
    pub(crate) fn get_challenge(purpose: &'static str) -> Self {
        Self::start("dexdo.v1.Gateway/GetChallenge", Some(purpose))
    }
    pub(crate) fn open_stream() -> Self {
        Self::start("dexdo.v1.Gateway/OpenStream", None)
    }
    fn start(method: &'static str, purpose: Option<&'static str>) -> Self {
        Self {
            method,
            purpose,
            started: Instant::now(),
            ended: false,
        }
    }
    pub(crate) fn answered<T>(self, answer: &Result<T, tonic::Status>) {
        match answer {
            Ok(_) => self.finish(tonic::Code::Ok),
            Err(status) => self.refused(status),
        }
    }
    pub(crate) fn refused(self, status: &tonic::Status) {
        self.finish(status.code())
    }
    fn finish(mut self, code: tonic::Code) {
        self.end(code)
    }
    /// The call goes on until the end of its response stream.
    pub(crate) fn watch<S>(self, stream: S) -> CallStream<S> {
        CallStream { stream, call: self }
    }
    fn end(&mut self, code: tonic::Code) {
        if std::mem::replace(&mut self.ended, true) {
            return;
        }
        let Some(metrics) = METRICS.get() else {
            return;
        };
        let name = code_name(code);
        let mut attributes = labels();
        attributes.extend([
            KeyValue::new("rpc.system.name", "grpc"),
            KeyValue::new("rpc.method", self.method),
            KeyValue::new("rpc.response.status_code", name),
        ]);
        if let Some(purpose) = self.purpose {
            attributes.push(KeyValue::new("purpose", purpose));
        }
        if code != tonic::Code::Ok {
            attributes.push(KeyValue::new("error.type", name));
            metrics.call_errors.add(1, &attributes);
        }
        metrics
            .call_duration
            .record(self.started.elapsed().as_secs_f64(), &attributes);
    }
}
impl Drop for Call {
    fn drop(&mut self) {
        self.end(tonic::Code::Cancelled)
    }
}

/// A gateway response stream, unchanged; its end or its error status ends the call.
pub struct CallStream<S> {
    stream: S,
    call: Call,
}
impl<S, T> CallStream<S>
where
    S: Stream<Item = Result<T, tonic::Status>> + Unpin,
{
    /// The next message, as [`tonic::Streaming::message`] returns it.
    // The signature is tonic's, so callers of the unmetered stream keep compiling.
    #[allow(clippy::result_large_err)]
    pub async fn message(&mut self) -> Result<Option<T>, tonic::Status> {
        futures::StreamExt::next(self).await.transpose()
    }
}
impl<S, T> Stream for CallStream<S>
where
    S: Stream<Item = Result<T, tonic::Status>> + Unpin,
{
    type Item = S::Item;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<S::Item>> {
        let polled = Pin::new(&mut self.stream).poll_next(cx);
        match &polled {
            Poll::Ready(None) => self.call.end(tonic::Code::Ok),
            Poll::Ready(Some(Err(status))) => self.call.end(status.code()),
            _ => (),
        }
        polled
    }
}

fn code_name(code: tonic::Code) -> &'static str {
    match code {
        tonic::Code::Ok => "OK",
        tonic::Code::Cancelled => "CANCELLED",
        tonic::Code::Unknown => "UNKNOWN",
        tonic::Code::InvalidArgument => "INVALID_ARGUMENT",
        tonic::Code::DeadlineExceeded => "DEADLINE_EXCEEDED",
        tonic::Code::NotFound => "NOT_FOUND",
        tonic::Code::AlreadyExists => "ALREADY_EXISTS",
        tonic::Code::PermissionDenied => "PERMISSION_DENIED",
        tonic::Code::ResourceExhausted => "RESOURCE_EXHAUSTED",
        tonic::Code::FailedPrecondition => "FAILED_PRECONDITION",
        tonic::Code::Aborted => "ABORTED",
        tonic::Code::OutOfRange => "OUT_OF_RANGE",
        tonic::Code::Unimplemented => "UNIMPLEMENTED",
        tonic::Code::Internal => "INTERNAL",
        tonic::Code::Unavailable => "UNAVAILABLE",
        tonic::Code::DataLoss => "DATA_LOSS",
        tonic::Code::Unauthenticated => "UNAUTHENTICATED",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_outside_rfc_9110_and_patch_are_other() {
        for known in [
            "GET", "HEAD", "POST", "PUT", "DELETE", "CONNECT", "OPTIONS", "TRACE",
        ] {
            assert_eq!(method(&known.parse().unwrap()), known);
        }
        assert_eq!(method(&http::Method::PATCH), "PATCH");
        // Methods are case-sensitive: a lowercase name is an extension method.
        for other in ["get", "PROPFIND", "QUERY"] {
            assert_eq!(method(&other.parse().unwrap()), "_OTHER");
        }
    }

    #[test]
    fn protocol_versions_use_the_convention_spelling() {
        assert_eq!(protocol_version(http::Version::HTTP_11), Some("1.1"));
        assert_eq!(protocol_version(http::Version::HTTP_2), Some("2"));
        assert_eq!(protocol_version(http::Version::HTTP_10), Some("1.0"));
    }

    /// TCP and TLS pass and the h2 handshake fails: the transport error carries no connector
    /// stage. A handover address that does not parse never reaches the dial.
    #[tokio::test]
    async fn an_untagged_dial_failure_is_http2_and_one_before_the_dial_is_no_connection() {
        let (client, server) = tokio::io::duplex(64);
        drop(server);
        let mut client = Some(client);
        let dialled = tonic::transport::Endpoint::from_static("http://127.0.0.1:1")
            .connect_with_connector(tower::service_fn(move |_: http::Uri| {
                let io = client.take().map(hyper_util::rt::TokioIo::new);
                async move { io.ok_or_else(|| std::io::Error::other("dialled twice")) }
            }))
            .await
            .expect_err("the peer is gone before the h2 preface");
        assert_eq!(
            failed_stage(&anyhow::Error::new(dialled)),
            Some("http2_handshake")
        );
        let unparsed = crate::buyer::tls::connect_pinned("https://bad host:1", &"00".repeat(32))
            .await
            .expect_err("an address with a space does not parse");
        assert_eq!(failed_stage(&unparsed), None);
    }

    #[test]
    fn buyer_export_is_off_without_otel_and_names_the_buyer() {
        let parse = |values: &[(&str, &str)]| {
            super::super::Config::parse_as(super::super::Role::Buyer, |key| {
                values
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.to_string())
            })
        };
        // No exporter configured: no provider, so no connection to any collector.
        assert!(parse(&[]).unwrap().is_none());
        assert!(super::super::Session::start(None).unwrap().is_none());
        let config = parse(&[("OTEL_METRICS_EXPORTER", "otlp")])
            .unwrap()
            .unwrap();
        let attribute = |key: &str| {
            config
                .resource
                .iter()
                .find(|attribute| attribute.key.as_str() == key)
                .map(|attribute| attribute.value.to_string())
        };
        assert_eq!(attribute("role").as_deref(), Some("buyer"));
        assert_eq!(
            attribute("service.name").as_deref(),
            Some(dexdo_core::params::BUYER_METRICS_SERVICE_NAME)
        );
        let error = parse(&[("OTEL_METRICS_EXPORTER", "prometheus")])
            .err()
            .unwrap()
            .to_string();
        assert!(error.starts_with("buyer metrics: "), "{error}");
    }

    #[test]
    fn alerts_and_codes_use_their_registry_names() {
        assert_eq!(
            alert_name(AlertDescription::HandshakeFailure),
            "handshake_failure"
        );
        assert_eq!(alert_name(AlertDescription::UnknownCA), "unknown_ca");
        assert_eq!(alert_name(AlertDescription::Unknown(0xff)), "_OTHER");
        assert_eq!(code_name(tonic::Code::DataLoss), "DATA_LOSS");
        assert_eq!(
            code_name(tonic::Code::ResourceExhausted),
            "RESOURCE_EXHAUSTED"
        );
    }
}
