//! Narrow bee session boundary for `dexdo wallet onboard`.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ackinacki_kit::contracts::authservice::profile::{AuthProfile, ParamsOfQueryProfileEvents};
use ackinacki_kit::tvm_client::abi::Signer;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use ackinacki_kit::tvm_client::processing::ResultOfSendMessage;
use ackinacki_kit::tvm_client::{ClientConfig, ClientContext};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use bee_connect::dh::{rekey_inbound, rekey_outbound, ConnectSessionState};
use bee_connect::errors::AppError;
use bee_connect::message::{
    connect_message_aad, decrypt_connect_body, encrypt_connect_body, normalize_owner_public_hex,
    CONNECT_MESSAGE_ENC_XCHACHA20POLY1305_HKDF_SHA256, CONNECT_MESSAGE_TYPE_WALLET_HELLO,
    CONNECT_MESSAGE_VERSION,
};
use bee_connect::{
    ConnectClient, ParamsOfCreateSharedKeySession, ParamsOfWaitWalletHello,
    ResultOfCreateSharedKeySession, ResultOfWaitWalletHello,
};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

pub const SESSION_FILE_VERSION: u8 = 1;
pub const AGENT_ONBOARD_REQUEST_TYPE: &str = "agent_onboard_request";
pub const AGENT_ONBOARD_BODY_TYPE: &str = "agent_multisig_onboard";
pub const AGENT_WALLETS_RESPONSE_TYPE: &str = "agent_wallets_response";
pub const AGENT_WALLETS_BODY_VERSION: u8 = 1;
const DEXDO_CLI_BEE_APP_ID: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000078";
/// The onboarding intent this CLI declares on its connect deeplink, verbatim as the wallet reads it.

/// An EXTERNAL query parameter on the final link, deliberately not a `ConnectPayload` field: the
/// payload is the bee-authenticated part of the invitation and its shape belongs to bee, while this
/// is a routing hint the wallet reads straight off the URL it was opened with. On seeing it the

/// arrives. It authorises nothing and starts no deployment.

/// `DEXDO_CLI_BEE_APP_ID` stays exactly as it was: it still identifies DEXDO CLI and names it in the
/// wallet, it just no longer decides which onboarding flow the wallet opens.
pub const AGENT_ONBOARD_INTENT_QUERY: &str = "intent=agent_onboard";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionLimits {
    pub session_ttl: Duration,
    pub hello_poll_attempts: u32,
    pub hello_poll_interval: Duration,
    pub response_poll_attempts: u32,
    pub response_poll_interval: Duration,
    pub context_event_limit: u32,
    pub timestamp_future_skew: Duration,
    pub agent_name_max_chars: usize,
}

impl SessionLimits {
    pub fn validate(self) -> Result<Self> {
        if self.session_ttl.is_zero() {
            bail!("wallet onboarding session TTL must be positive");
        }
        if self.hello_poll_attempts == 0 || self.response_poll_attempts == 0 {
            bail!("wallet onboarding poll attempts must be positive");
        }
        if self.hello_poll_interval.is_zero() || self.response_poll_interval.is_zero() {
            bail!("wallet onboarding poll intervals must be positive");
        }
        if self.context_event_limit == 0 {
            bail!("wallet onboarding context event limit must be positive");
        }
        if self.agent_name_max_chars == 0 {
            bail!("wallet onboarding agent-name limit must be positive");
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedAddress {
    pub canonical: String,
    pub dapp_id: String,
    pub account_address: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentWalletsResponse {
    pub version: u8,
    pub network: String,
    pub vault: ScopedAddress,
    pub hot: ScopedAddress,
}

#[derive(Serialize, Deserialize)]
pub struct OnboardingSession {
    pub file_version: u8,
    pub agent_name: String,
    pub network: String,
    pub endpoint: String,
    pub hot_pubkey: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vault_pubkey: Option<String>,
    pub phase: SessionPhase,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "name", rename_all = "snake_case")]
pub enum SessionPhase {
    AwaitingWalletHello {
        nonce: String,
        invitation: ResultOfCreateSharedKeySession,
    },
    RequestPrepared {
        request: PreparedRequest,
    },
    AwaitingWalletsResponse {
        request: PreparedRequest,
    },
    ResponseReceived {
        profile_address: String,
        /// Carried on from the request. See `PreparedRequest::wallet_address`.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        wallet_address: String,
        response_event_id: String,
        session_state: ConnectSessionState,
        response: AgentWalletsResponse,
    },
    Complete {
        /// The `AuthProfile` address of the authenticated `wallet_hello`, kept rather than dropped.

        /// It is the one non-secret identity the completed onboarding proved, and completion used
        /// to discard it: `ResponseReceived` carried it and `Complete` did not, so a finished
        /// session no longer knew which profile it had been onboarded through. Defaulted for
        /// state files written before it was retained -- a session that completed under the old
        /// shape must still load rather than force a fresh QR scan.
        #[serde(default)]
        profile_address: String,
        /// The multifactor wallet address, kept for the same reason and on the same terms: the
        /// binding records it as reserved metadata, and completion is the last place it exists.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        wallet_address: String,
        response: AgentWalletsResponse,
    },
}

#[derive(Serialize, Deserialize)]
pub struct PreparedRequest {
    pub profile_address: String,
    /// The multifactor wallet address from the authenticated `wallet_hello`, carried but never
    /// sent.

    /// It has to travel with the request because that is the only durable state between the hello
    /// that proves it and the completion that records it: the invitation is spent and cannot be
    /// scanned twice, so an onboarding resumed after a restart would otherwise finish without it.
    /// Optional by specification -- its absence must not fail an onboarding -- so it is defaulted
    /// on read and left out on write when empty, which keeps a state file written before this
    /// field existed byte-identical.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub wallet_address: String,
    pub session_id: String,
    pub hello_event_id: String,
    pub context_created_at_from: u64,
    pub envelope_json: String,
    pub session_state: ConnectSessionState,
}

impl std::fmt::Debug for OnboardingSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OnboardingSession")
            .field("file_version", &self.file_version)
            .field("agent_name", &self.agent_name)
            .field("network", &self.network)
            .field("endpoint", &self.endpoint)
            .field("hot_pubkey", &self.hot_pubkey)
            .field("vault_pubkey", &self.vault_pubkey)
            .field("phase", &self.phase_name())
            .finish()
    }
}

impl OnboardingSession {
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        agent_name: &str,
        network: &str,
        endpoint: &str,
        hot_pubkey: &str,
        vault_pubkey: Option<&str>,
        nonce: &str,
        limits: SessionLimits,
    ) -> Result<Self> {
        let limits = limits.validate()?;
        let agent_name = agent_name.trim();
        let agent_name_chars = agent_name.chars().count();
        if agent_name_chars == 0 || agent_name_chars > limits.agent_name_max_chars {
            bail!(
                "wallet onboarding agent name must contain 1..={} characters",
                limits.agent_name_max_chars
            );
        }
        // this used to admit exactly two labels by name, which made a binding for any
        // chain deployed later impossible to express -- and the two names were written here, in a
        // crate that never reads a manifest and so has no way to know what is deployed. What a
        // label must be is non-empty: it keys the binding file, and an empty key cannot be told
        // apart from any other.
        if network.trim().is_empty() {
            bail!(
                "wallet onboarding network must name the chain this binding is for; the manifest's \
                 `network` field is what says which"
            );
        }
        let endpoint = endpoint.trim();
        if endpoint.is_empty() {
            bail!("wallet onboarding endpoint must not be empty");
        }
        let hot_pubkey = normalize_owner_public_hex(hot_pubkey)
            .map_err(|error| anyhow!("wallet onboarding Hot public key: {error}"))?;
        let vault_pubkey = vault_pubkey
            .map(normalize_owner_public_hex)
            .transpose()
            .map_err(|error| anyhow!("wallet onboarding Vault public key: {error}"))?;
        validate_nonce_hex(nonce)?;
        let ttl_secs = limits.session_ttl.as_secs();
        let mut invitation = ConnectClient::new()
            .create_shared_key_session(ParamsOfCreateSharedKeySession {
                app_id: DEXDO_CLI_BEE_APP_ID.to_string(),
                ttl_secs: Some(ttl_secs),
                nonce: Some(nonce.to_string()),
            })
            .map_err(|error| {
                anyhow!(
                    "{}",
                    describe_bee_failure("create bee wallet onboarding session", &error, &[])
                )
            })?;
        // Declare the intent on the FINAL link, so the one string that is printed, stored, and
        // handed to the QR renderer is the one the wallet is opened with.
        invitation.deep_link = with_agent_onboard_intent(&invitation.deep_link);
        if invitation.deep_link.contains(&hot_pubkey)
            || vault_pubkey
                .as_ref()
                .is_some_and(|public| invitation.deep_link.contains(public))
        {
            bail!("bee connection invitation unexpectedly contains an agent public key");
        }

        Ok(Self {
            file_version: SESSION_FILE_VERSION,
            agent_name: agent_name.to_string(),
            network: network.to_string(),
            endpoint: endpoint.to_string(),
            hot_pubkey,
            vault_pubkey,
            phase: SessionPhase::AwaitingWalletHello {
                nonce: nonce.to_string(),
                invitation,
            },
        })
    }

    pub fn validate_file(&self) -> Result<()> {
        if self.file_version != SESSION_FILE_VERSION {
            bail!(
                "wallet onboarding state version {} is unsupported; expected {}",
                self.file_version,
                SESSION_FILE_VERSION
            );
        }
        if self.agent_name.trim().is_empty() {
            bail!("wallet onboarding state has an empty agent name");
        }
        // reading a saved session used to admit exactly two labels, so a session recorded
        // against any other chain was refused on LOAD -- after it had been written, with the money
        // already committed. The label is a key, not a permission: it must be there, and that is all
        // this crate can honestly check about it.
        if self.network.trim().is_empty() {
            bail!(
                "wallet onboarding state names no network, so nothing says which chain it is for"
            );
        }
        if self.endpoint.trim().is_empty() {
            bail!("wallet onboarding state has an empty endpoint");
        }
        normalize_owner_public_hex(&self.hot_pubkey)
            .map_err(|error| anyhow!("wallet onboarding state Hot public key: {error}"))?;
        self.vault_pubkey
            .as_deref()
            .map(normalize_owner_public_hex)
            .transpose()
            .map_err(|error| anyhow!("wallet onboarding state Vault public key: {error}"))?;
        match &self.phase {
            SessionPhase::AwaitingWalletHello { nonce, invitation } => {
                validate_nonce_hex(nonce)?;
                if invitation.session_id.is_empty()
                    || invitation.description.is_empty()
                    || invitation.client_dh_secret.is_empty()
                    || invitation.deep_link.is_empty()
                {
                    bail!("wallet onboarding invitation state is incomplete");
                }
            }
            SessionPhase::RequestPrepared { request }
            | SessionPhase::AwaitingWalletsResponse { request } => {
                request.validate()?;
            }
            SessionPhase::ResponseReceived {
                profile_address,
                response_event_id,
                response,
                ..
            } => {
                if profile_address.is_empty() || response_event_id.is_empty() {
                    bail!("wallet onboarding received-response state is incomplete");
                }
                validate_response_value(response, &self.network)?;
            }
            SessionPhase::Complete { response, .. } => {
                validate_response_value(response, &self.network)?;
            }
        }
        Ok(())
    }

    pub fn phase_name(&self) -> &'static str {
        match self.phase {
            SessionPhase::AwaitingWalletHello { .. } => "awaiting_wallet_hello",
            SessionPhase::RequestPrepared { .. } => "request_prepared",
            SessionPhase::AwaitingWalletsResponse { .. } => "awaiting_wallets_response",
            SessionPhase::ResponseReceived { .. } => "response_received",
            SessionPhase::Complete { .. } => "complete",
        }
    }

    pub fn deep_link(&self) -> Option<&str> {
        match &self.phase {
            SessionPhase::AwaitingWalletHello { invitation, .. } => {
                Some(invitation.deep_link.as_str())
            }
            _ => None,
        }
    }

    pub fn response(&self) -> Option<&AgentWalletsResponse> {
        match &self.phase {
            SessionPhase::ResponseReceived { response, .. }
            | SessionPhase::Complete { response, .. } => Some(response),
            _ => None,
        }
    }

    /// The `AuthProfile` address this session was onboarded through, once one has been proved.

    /// Available in both phases that hold it, so a caller that resumes straight into `complete`
    /// reads the same value as one that has just consumed the response. Empty is reported as
    /// absent: a state file written before the address was retained carries no address at all,
    /// and an empty string is not an address.
    pub fn profile_address(&self) -> Option<&str> {
        let address = match &self.phase {
            SessionPhase::ResponseReceived {
                profile_address, ..
            }
            | SessionPhase::Complete {
                profile_address, ..
            } => profile_address.as_str(),
            _ => return None,
        };
        (!address.is_empty()).then_some(address)
    }

    /// The multifactor wallet address the `wallet_hello` carried, on the same terms.

    /// A DIFFERENT value from [`Self::profile_address`] -- one is the Connect `AuthProfile`, this
    /// one is the wallet -- and optional: a wallet that sends no address is not a failed
    /// onboarding, so this answers `None` rather than refusing.
    pub fn wallet_address(&self) -> Option<&str> {
        let address = match &self.phase {
            SessionPhase::ResponseReceived { wallet_address, .. }
            | SessionPhase::Complete { wallet_address, .. } => wallet_address.as_str(),
            _ => return None,
        };
        (!address.is_empty()).then_some(address)
    }

    pub fn mark_complete(mut self) -> Result<Self> {
        self.validate_file()?;
        let phase = std::mem::replace(
            &mut self.phase,
            SessionPhase::Complete {
                profile_address: String::new(),
                wallet_address: String::new(),
                response: placeholder_response(),
            },
        );
        let SessionPhase::ResponseReceived {
            profile_address,
            wallet_address,
            response,
            ..
        } = phase
        else {
            bail!("wallet onboarding can complete only after a validated response");
        };
        self.phase = SessionPhase::Complete {
            profile_address,
            wallet_address,
            response,
        };
        Ok(self)
    }

    pub async fn advance(self, io: &dyn BeeSessionIo, limits: SessionLimits) -> Result<Self> {
        self.advance_inner(io, limits, false).await
    }

    pub async fn advance_after_restart(
        self,
        io: &dyn BeeSessionIo,
        limits: SessionLimits,
    ) -> Result<Self> {
        self.advance_inner(io, limits, true).await
    }

    async fn advance_inner(
        mut self,
        io: &dyn BeeSessionIo,
        limits: SessionLimits,
        reconcile_prepared_request: bool,
    ) -> Result<Self> {
        self.validate_file()?;
        let limits = limits.validate()?;
        let phase = std::mem::replace(
            &mut self.phase,
            SessionPhase::Complete {
                profile_address: String::new(),
                wallet_address: String::new(),
                response: placeholder_response(),
            },
        );
        self.phase = match phase {
            SessionPhase::AwaitingWalletHello { nonce, invitation } => {
                let hello = io.wait_wallet_hello(&invitation, limits).await?;
                let now = now_unix_secs()?;
                SessionPhase::RequestPrepared {
                    request: prepare_request(
                        &self.agent_name,
                        &self.hot_pubkey,
                        self.vault_pubkey.as_deref(),
                        &nonce,
                        &invitation,
                        hello,
                        limits,
                        now,
                    )?,
                }
            }
            SessionPhase::RequestPrepared { request } => {
                let request_exists = if reconcile_prepared_request {
                    request_exists_after_restart(io, &request, limits).await?
                } else {
                    io.request_exists(&request, limits).await?
                };
                if !request_exists {
                    io.publish_request(&request).await?;
                }
                SessionPhase::AwaitingWalletsResponse { request }
            }
            SessionPhase::AwaitingWalletsResponse { request } => {
                let event = io.wait_wallets_response(&request, limits).await?;
                let now = now_unix_secs()?;
                let wallet_address = request.wallet_address.clone();
                let (profile_address, response_event_id, session_state, response) =
                    consume_wallets_response(&self.network, &request, event, limits, now)?;
                SessionPhase::ResponseReceived {
                    profile_address,
                    wallet_address,
                    response_event_id,
                    session_state,
                    response,
                }
            }
            other @ SessionPhase::ResponseReceived { .. }
            | other @ SessionPhase::Complete { .. } => other,
        };
        Ok(self)
    }
}

async fn request_exists_after_restart(
    io: &dyn BeeSessionIo,
    request: &PreparedRequest,
    limits: SessionLimits,
) -> Result<bool> {
    // Both request and response use the same eventually consistent context index.
    for attempt in 0..limits.response_poll_attempts {
        if io.request_exists(request, limits).await? {
            return Ok(true);
        }
        if attempt + 1 < limits.response_poll_attempts {
            tokio::time::sleep(limits.response_poll_interval).await;
        }
    }
    Ok(false)
}

impl PreparedRequest {
    fn validate(&self) -> Result<()> {
        if self.profile_address.is_empty()
            || self.session_id.is_empty()
            || self.hello_event_id.is_empty()
            || self.envelope_json.is_empty()
        {
            bail!("wallet onboarding prepared request state is incomplete");
        }
        self.session_state
            .ensure_not_expired()
            .map_err(|error| anyhow!("wallet onboarding bee session: {error}"))?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedContextEvent {
    pub id: String,
    pub created_at: u64,
    pub text: String,
}

#[async_trait(?Send)]
pub trait BeeSessionIo {
    async fn wait_wallet_hello(
        &self,
        invitation: &ResultOfCreateSharedKeySession,
        limits: SessionLimits,
    ) -> Result<ResultOfWaitWalletHello>;

    async fn request_exists(
        &self,
        request: &PreparedRequest,
        limits: SessionLimits,
    ) -> Result<bool>;

    async fn publish_request(&self, request: &PreparedRequest) -> Result<()>;

    async fn wait_wallets_response(
        &self,
        request: &PreparedRequest,
        limits: SessionLimits,
    ) -> Result<ObservedContextEvent>;
}

/// Render a bee onboarding failure from its structured cause instead of its one-line message.

/// Both failing layers carry far more than `message`. `bee_connect` returns an `AppError` with
/// `kind`, `module`, `error_code`, `details` and the raw `tvm_error`; `ackinacki_kit` returns a
/// `KitError` that the same `AppError` already knows how to absorb, lifting the TVM code and the
/// computed-phase exit code into `details`. Reporting only `message` collapses "TVM code 12, HTTP
/// 405 on /v2/messages" into the words "Send message", which an operator cannot tell apart from a
/// service outage.

/// `secrets` are scrubbed from the finished string unconditionally, and this is the only exit from
/// this function: a transport error is free to quote the request it could not send, and neither the
/// session's signing secret, nor its DH secrets, nor the encrypted envelope may reach a log.
fn describe_bee_failure(operation: &str, error: &AppError, secrets: &[&str]) -> String {
    let mut described = format!("{operation}: {}", error.message);
    for (name, value) in [
        ("kind", error.kind.as_deref()),
        ("module", error.module.as_deref()),
        ("error_code", error.error_code.as_deref()),
        ("details", error.details.as_deref()),
    ] {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            described.push_str(&format!("; {name}={value}"));
        }
    }
    if let Some(tvm_error) = error.tvm_error.as_ref() {
        described.push_str(&format!("; tvm_error={tvm_error}"));
    }
    redact(described, secrets)
}

fn redact(mut text: String, secrets: &[&str]) -> String {
    for secret in secrets {
        if !secret.is_empty() {
            text = text.replace(secret, "[redacted]");
        }
    }
    text
}

/// The values that decide whether a delivered message actually executed, quoted as they came back.

/// `aborted` and `exit_code` are `Option`, and the distinction between "the node said false" and
/// "the node said nothing" is exactly what tells a rejected message from an unread receipt, so the
/// absent case is reported as `unknown` rather than folded into a default.
fn send_message_failure(result: &ResultOfSendMessage) -> Option<String> {
    if !result.aborted.unwrap_or(false) && result.exit_code.unwrap_or(0) == 0 {
        return None;
    }
    fn reported<T: std::fmt::Display>(value: Option<T>) -> String {
        value.map_or_else(|| "unknown".to_string(), |value| value.to_string())
    }
    Some(format!(
        "aborted={}, exit_code={}, message_hash={}, tx_hash={}, block_hash={}",
        reported(result.aborted),
        reported(result.exit_code),
        reported(result.message_hash.as_deref()),
        reported(result.tx_hash.as_deref()),
        reported(result.block_hash.as_deref()),
    ))
}

pub struct CanonicalBeeSessionIo {
    endpoint: String,
    context: Arc<ClientContext>,
}

impl CanonicalBeeSessionIo {
    /// The endpoint must already be absolute. This constructor refuses a bare host rather than
    /// repairing one, because the repair belongs at the command boundary and must happen exactly
    /// once -- a second normaliser here could disagree with the one that wrote the durable state.

    /// A bare host is not merely untidy, it is the difference between reading and writing.
    /// `ServerLink::new` picks the REST base -- the `/v2/` prefix that carries `/v2/messages`, the
    /// only way an `AuthProfile` write leaves this process -- with
    /// `endpoints.first().starts_with("https://")`, so a scheme-less endpoint posts over plain
    /// http. GraphQL takes a different route: `Endpoint::expand_address` upgrades any non-loopback
    /// bare host to https by itself. Reads therefore succeed while the publish is answered 405 by
    /// the edge and surfaces from the SDK as the single word "Send message".
    pub fn new(endpoint: &str) -> Result<Self> {
        let endpoint = endpoint.trim();
        if endpoint.is_empty() {
            bail!("wallet onboarding endpoint must not be empty");
        }
        if !endpoint.starts_with("https://") && !endpoint.starts_with("http://") {
            bail!(
                "wallet onboarding endpoint `{endpoint}` has no scheme; an absolute https:// endpoint is required because the AuthProfile write goes to `/v2/messages` over REST, which a scheme-less endpoint sends over plain http"
            );
        }
        let mut config = ClientConfig::default();
        config.network.endpoints = Some(vec![endpoint.to_string()]);
        let context = Arc::new(
            ClientContext::new(config)
                .map_err(|error| anyhow!("create wallet onboarding client context: {error}"))?,
        );
        Ok(Self {
            endpoint: endpoint.to_string(),
            context,
        })
    }

    fn profile(&self, address: &str) -> AuthProfile {
        AuthProfile::new_default(self.context.clone(), address)
    }

    async fn query_context(
        &self,
        profile_address: &str,
        created_at_from: u64,
        limit: u32,
    ) -> Result<Vec<ObservedContextEvent>> {
        let profile = self.profile(profile_address);
        let mut before = None;
        let mut seen_cursors = std::collections::HashSet::new();
        let mut events = Vec::new();
        loop {
            let result = profile
                .query_context_added_events(ParamsOfQueryProfileEvents {
                    created_at_from: Some(created_at_from),
                    limit: Some(limit),
                    before,
                })
                .await
                .map_err(|error| {
                    anyhow!(
                        "{}",
                        describe_bee_failure(
                            &format!(
                                "query bee wallet onboarding context of AuthProfile {profile_address}"
                            ),
                            &AppError::from(error),
                            &[],
                        )
                    )
                })?;
            let page_is_empty = result.events.is_empty();
            events.extend(
                result
                    .events
                    .into_iter()
                    .map(|record| ObservedContextEvent {
                        id: record.event.id,
                        created_at: record.event.created_at,
                        text: record.data.text,
                    }),
            );
            if !result.page_info.has_previous_page || page_is_empty {
                break;
            }
            let cursor = result.page_info.cursor.ok_or_else(|| {
                anyhow!("bee context pagination reported an older page without a cursor")
            })?;
            if !seen_cursors.insert(cursor.clone()) {
                bail!("bee context pagination repeated a cursor");
            }
            before = Some(cursor);
        }
        Ok(events)
    }
}

#[async_trait(?Send)]
impl BeeSessionIo for CanonicalBeeSessionIo {
    async fn wait_wallet_hello(
        &self,
        invitation: &ResultOfCreateSharedKeySession,
        limits: SessionLimits,
    ) -> Result<ResultOfWaitWalletHello> {
        ConnectClient::new()
            .wait_wallet_hello(ParamsOfWaitWalletHello {
                endpoints: vec![self.endpoint.clone()],
                session_id: invitation.session_id.clone(),
                description: invitation.description.clone(),
                client_dh_secret: invitation.client_dh_secret.clone(),
                created_at_from: Some(invitation.created_at),
                max_attempts: Some(limits.hello_poll_attempts),
                interval_ms: Some(duration_millis(
                    limits.hello_poll_interval,
                    "wallet hello poll interval",
                )?),
            })
            .await
            .map_err(|error| {
                anyhow!(
                    "{}",
                    describe_bee_failure(
                        "wait for signed wallet_hello",
                        &error,
                        &[invitation.client_dh_secret.as_str()],
                    )
                )
            })
    }

    async fn request_exists(
        &self,
        request: &PreparedRequest,
        limits: SessionLimits,
    ) -> Result<bool> {
        Ok(self
            .query_context(
                &request.profile_address,
                request.context_created_at_from,
                limits.context_event_limit,
            )
            .await?
            .iter()
            .any(|event| event.text == request.envelope_json))
    }

    async fn publish_request(&self, request: &PreparedRequest) -> Result<()> {
        let secrets = [
            request.session_state.signing_secret.as_str(),
            request.session_state.my_dh_secret.as_str(),
            request.session_state.encryption_root.as_str(),
            request.envelope_json.as_str(),
        ];
        let keys = KeyPair {
            public: request.session_state.signing_public.clone(),
            secret: request.session_state.signing_secret.as_str().to_string(),
        };
        let result = self
            .profile(&request.profile_address)
            .add_context_text(&request.envelope_json, Signer::Keys { keys })
            .await
            .map_err(|error| {
                anyhow!(
                    "{}",
                    describe_bee_failure(
                        &format!(
                            "publish agent_onboard_request to AuthProfile {} via {}",
                            request.profile_address, self.endpoint
                        ),
                        &AppError::from(error),
                        &secrets,
                    )
                )
            })?;
        if let Some(failure) = send_message_failure(&result) {
            bail!("publish agent_onboard_request transaction failed: {failure}");
        }
        Ok(())
    }

    async fn wait_wallets_response(
        &self,
        request: &PreparedRequest,
        limits: SessionLimits,
    ) -> Result<ObservedContextEvent> {
        for _ in 0..limits.response_poll_attempts {
            let mut candidates = self
                .query_context(
                    &request.profile_address,
                    request.context_created_at_from,
                    limits.context_event_limit,
                )
                .await?
                .into_iter()
                .filter(|event| is_candidate_response(&event.text, &request.session_id))
                .collect::<Vec<_>>();
            candidates.sort_by(|left, right| {
                left.created_at
                    .cmp(&right.created_at)
                    .then_with(|| left.id.cmp(&right.id))
            });
            if let Some(event) = candidates.into_iter().next() {
                return Ok(event);
            }
            tokio::time::sleep(limits.response_poll_interval).await;
        }
        bail!(
            "timed out waiting for durable agent_wallets_response in AuthProfile {} after {} polls every {:?} (session {}, context from {})",
            request.profile_address,
            limits.response_poll_attempts,
            limits.response_poll_interval,
            request.session_id,
            request.context_created_at_from
        )
    }
}

#[derive(Serialize)]
struct AgentOnboardRequestBody<'a> {
    v: u8,
    #[serde(rename = "type")]
    body_type: &'static str,
    agent_name: &'a str,
    hot_pubkey: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    vault_pubkey: Option<&'a str>,
}

#[derive(Deserialize)]
struct ConnectEnvelope {
    v: String,
    session_id: String,
    dir: String,
    seq: u64,
    #[serde(rename = "type")]
    message_type: String,
    ts: u64,
    dh_public: String,
    enc: ConnectEncryption,
    body: String,
}

#[derive(Deserialize)]
struct ConnectEncryption {
    alg: String,
    nonce: String,
    salt: String,
}

#[derive(Deserialize)]
struct AgentWalletsResponseBody {
    v: u8,
    network: String,
    vault_address: String,
    hot_address: String,
}

#[allow(clippy::too_many_arguments)]
fn prepare_request(
    agent_name: &str,
    hot_pubkey: &str,
    vault_pubkey: Option<&str>,
    expected_nonce: &str,
    invitation: &ResultOfCreateSharedKeySession,
    mut hello: ResultOfWaitWalletHello,
    limits: SessionLimits,
    now: u64,
) -> Result<PreparedRequest> {
    verify_wallet_hello(expected_nonce, invitation, &hello, limits, now)?;
    hello.session_state.created_at = invitation.created_at;
    hello.session_state.expires_at = invitation.expires_at;
    hello
        .session_state
        .ensure_not_expired()
        .map_err(|error| anyhow!("wallet onboarding bee session: {error}"))?;
    let seq = hello
        .session_state
        .next_outbound_seq()
        .map_err(|error| anyhow!("agent_onboard_request sequence: {error}"))?;
    let rekey = rekey_outbound(&hello.session_state, &invitation.session_id, seq)
        .map_err(|error| anyhow!("agent_onboard_request ratchet: {error}"))?;
    let body = serde_json::to_value(AgentOnboardRequestBody {
        v: AGENT_WALLETS_BODY_VERSION,
        body_type: AGENT_ONBOARD_BODY_TYPE,
        agent_name,
        hot_pubkey,
        vault_pubkey,
    })
    .context("serialize agent_onboard_request body")?;
    let aad = connect_message_aad(
        &invitation.session_id,
        "c2w",
        seq,
        AGENT_ONBOARD_REQUEST_TYPE,
        now,
    )
    .map_err(|error| anyhow!("agent_onboard_request AAD: {error}"))?;
    let encrypted = encrypt_connect_body(&body, &rekey.message_encryption_root, &aad)
        .map_err(|error| anyhow!("encrypt agent_onboard_request: {error}"))?;
    let envelope_json = serde_json::to_string(&serde_json::json!({
        "v": CONNECT_MESSAGE_VERSION,
        "session_id": invitation.session_id,
        "dir": "c2w",
        "seq": seq,
        "type": AGENT_ONBOARD_REQUEST_TYPE,
        "ts": now,
        "dh_public": rekey.new_dh_public.as_deref().unwrap_or(""),
        "enc": {
            "alg": CONNECT_MESSAGE_ENC_XCHACHA20POLY1305_HKDF_SHA256,
            "nonce": encrypted.nonce_b64url,
            "salt": encrypted.salt_b64url,
        },
        "body": encrypted.ciphertext_b64url,
    }))
    .context("serialize agent_onboard_request envelope")?;

    Ok(PreparedRequest {
        profile_address: hello.profile_address,
        wallet_address: hello.wallet_address,
        session_id: invitation.session_id.clone(),
        hello_event_id: hello.event_id,
        context_created_at_from: hello.event_created_at,
        envelope_json,
        session_state: rekey.updated_state,
    })
}

fn verify_wallet_hello(
    expected_nonce: &str,
    invitation: &ResultOfCreateSharedKeySession,
    hello: &ResultOfWaitWalletHello,
    limits: SessionLimits,
    now: u64,
) -> Result<()> {
    let envelope: ConnectEnvelope = serde_json::from_str(&hello.raw_message_json)
        .context("wallet_hello envelope is not valid bee JSON")?;
    if envelope.v != CONNECT_MESSAGE_VERSION
        || envelope.session_id != invitation.session_id
        || envelope.dir != "w2c"
        || envelope.seq == 0
        || envelope.message_type != CONNECT_MESSAGE_TYPE_WALLET_HELLO
        || envelope.seq != hello.session_state.last_seen_seq
    {
        bail!("wallet_hello envelope does not match the fresh bee session");
    }
    if hello.profile_address.is_empty() || hello.event_id.is_empty() {
        bail!("wallet_hello is missing its durable profile event identity");
    }
    validate_timestamp(
        "wallet_hello envelope",
        envelope.ts,
        invitation.created_at,
        invitation.expires_at,
        now,
        limits.timestamp_future_skew,
    )?;
    validate_timestamp(
        "wallet_hello event",
        hello.event_created_at,
        invitation.created_at,
        invitation.expires_at,
        now,
        limits.timestamp_future_skew,
    )?;
    let nonce = hello
        .nonce
        .as_deref()
        .ok_or_else(|| anyhow!("wallet_hello is missing the signed nonce"))?;
    if nonce != expected_nonce {
        bail!("wallet_hello nonce does not match the fresh onboarding nonce");
    }
    let signature = hello
        .signature
        .as_deref()
        .ok_or_else(|| anyhow!("wallet_hello is missing the nonce signature"))?;
    let public = hello
        .epk_public
        .as_deref()
        .ok_or_else(|| anyhow!("wallet_hello is missing the signing EPK"))?;
    let nonce_bytes = hex::decode(nonce).context("wallet_hello nonce is not valid hex")?;
    let public_bytes: [u8; 32] = hex::decode(public)
        .context("wallet_hello EPK is not valid hex")?
        .try_into()
        .map_err(|_| anyhow!("wallet_hello EPK must be 32 bytes"))?;
    let signature = Signature::from_slice(
        &hex::decode(signature).context("wallet_hello signature is not valid hex")?,
    )
    .map_err(|_| anyhow!("wallet_hello signature must be 64 bytes"))?;
    let verifying_key = VerifyingKey::from_bytes(&public_bytes)
        .map_err(|_| anyhow!("wallet_hello EPK is not a valid Ed25519 key"))?;
    if verifying_key
        .verify_strict(&nonce_bytes, &signature)
        .is_err()
    {
        bail!("wallet_hello nonce signature is invalid");
    }
    Ok(())
}

fn consume_wallets_response(
    selected_network: &str,
    request: &PreparedRequest,
    event: ObservedContextEvent,
    limits: SessionLimits,
    now: u64,
) -> Result<(String, String, ConnectSessionState, AgentWalletsResponse)> {
    if event.id.is_empty() {
        bail!("agent_wallets_response is missing its durable event identity");
    }
    validate_timestamp(
        "agent_wallets_response event",
        event.created_at,
        request.context_created_at_from,
        request.session_state.expires_at,
        now,
        limits.timestamp_future_skew,
    )?;
    let envelope: ConnectEnvelope = serde_json::from_str(&event.text)
        .context("agent_wallets_response envelope is not valid bee JSON")?;
    if envelope.v != CONNECT_MESSAGE_VERSION {
        bail!("agent_wallets_response has the wrong bee message version");
    }
    if envelope.session_id != request.session_id {
        bail!("agent_wallets_response belongs to a different bee session");
    }
    if envelope.dir != "w2c" {
        bail!("agent_wallets_response has the wrong bee direction");
    }
    if envelope.message_type != AGENT_WALLETS_RESPONSE_TYPE {
        bail!("agent_wallets_response has the wrong bee message type");
    }
    if envelope.seq == 0 {
        bail!("agent_wallets_response has an invalid sequence");
    }
    if envelope.enc.alg != CONNECT_MESSAGE_ENC_XCHACHA20POLY1305_HKDF_SHA256 {
        bail!("agent_wallets_response has an unsupported bee encryption algorithm");
    }
    validate_timestamp(
        "agent_wallets_response envelope",
        envelope.ts,
        request.session_state.created_at,
        request.session_state.expires_at,
        now,
        limits.timestamp_future_skew,
    )?;
    let rekey = rekey_inbound(
        &request.session_state,
        &envelope.dh_public,
        &request.session_id,
        envelope.seq,
    )
    .map_err(|error| anyhow!("agent_wallets_response ratchet rejected: {error}"))?;
    let aad = connect_message_aad(
        &request.session_id,
        "w2c",
        envelope.seq,
        AGENT_WALLETS_RESPONSE_TYPE,
        envelope.ts,
    )
    .map_err(|error| anyhow!("agent_wallets_response AAD: {error}"))?;
    let plaintext = decrypt_connect_body(
        &envelope.body,
        &envelope.enc.nonce,
        &envelope.enc.salt,
        &rekey.message_encryption_root,
        &aad,
    )
    .map_err(|_| anyhow!("agent_wallets_response authentication failed"))?;
    let wire: AgentWalletsResponseBody = serde_json::from_slice(&plaintext)
        .context("agent_wallets_response body does not match v1 schema")?;
    if wire.v != AGENT_WALLETS_BODY_VERSION {
        bail!(
            "agent_wallets_response version {} is unsupported; expected {}",
            wire.v,
            AGENT_WALLETS_BODY_VERSION
        );
    }
    if wire.network != selected_network {
        bail!("agent_wallets_response network does not match the selected onboarding network");
    }
    let response = AgentWalletsResponse {
        version: wire.v,
        network: wire.network,
        vault: parse_scoped_address(&wire.vault_address)
            .context("agent_wallets_response vault_address")?,
        hot: parse_scoped_address(&wire.hot_address)
            .context("agent_wallets_response hot_address")?,
    };
    validate_response_value(&response, selected_network)?;
    Ok((
        request.profile_address.clone(),
        event.id,
        rekey.updated_state,
        response,
    ))
}

pub fn parse_scoped_address(value: &str) -> Result<ScopedAddress> {
    let value = value.trim();
    let mut parts = value.split("::");
    let dapp = parts.next().unwrap_or_default();
    let account = parts.next().unwrap_or_default();
    if parts.next().is_some() || !is_hex64(dapp) || !is_hex64(account) {
        bail!("expected exactly `<64-hex-dapp>::<64-hex-account>`");
    }
    if !dapp.eq_ignore_ascii_case(account) {
        bail!("a wallet-root dApp address must use the same dApp and account id");
    }
    let dapp_id = dapp.to_ascii_lowercase();
    let account_id = account.to_ascii_lowercase();
    Ok(ScopedAddress {
        canonical: format!("{dapp_id}::{account_id}"),
        dapp_id,
        account_address: format!("0:{account_id}"),
    })
}

fn validate_response_value(response: &AgentWalletsResponse, selected_network: &str) -> Result<()> {
    if response.version != AGENT_WALLETS_BODY_VERSION {
        bail!("wallet onboarding response state has an unsupported version");
    }
    if response.network != selected_network {
        bail!("wallet onboarding response state has the wrong network");
    }
    if parse_scoped_address(&response.vault.canonical)? != response.vault
        || parse_scoped_address(&response.hot.canonical)? != response.hot
    {
        bail!("wallet onboarding response state has a non-canonical address");
    }
    Ok(())
}

fn is_candidate_response(text: &str, session_id: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    value.get("session_id").and_then(serde_json::Value::as_str) == Some(session_id)
        && value.get("dir").and_then(serde_json::Value::as_str) == Some("w2c")
        && value.get("type").and_then(serde_json::Value::as_str)
            == Some(AGENT_WALLETS_RESPONSE_TYPE)
}

fn validate_nonce_hex(nonce: &str) -> Result<()> {
    if nonce.len() != 64 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("wallet onboarding nonce must be exactly 32-byte hex");
    }
    Ok(())
}

fn validate_timestamp(
    label: &str,
    timestamp: u64,
    created_at: u64,
    expires_at: u64,
    now: u64,
    future_skew: Duration,
) -> Result<()> {
    if timestamp < created_at || timestamp > expires_at {
        bail!("{label} timestamp is outside the bee session lifetime");
    }
    if timestamp > now.saturating_add(future_skew.as_secs()) {
        bail!("{label} timestamp is too far in the future");
    }
    Ok(())
}

fn duration_millis(duration: Duration, label: &str) -> Result<u64> {
    duration
        .as_millis()
        .try_into()
        .map_err(|_| anyhow!("{label} does not fit u64 milliseconds"))
}

fn is_hex64(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Append [`AGENT_ONBOARD_INTENT_QUERY`] to a connect deeplink, exactly once.

/// Idempotent on purpose: the final link is what gets persisted in the durable session state, so a
/// resumed onboarding reads back a link that already declares the intent and must not grow a second
/// copy of it. Compared parameter-wise rather than by substring, so a payload that happens to spell
/// the same bytes cannot be mistaken for the query parameter.
fn with_agent_onboard_intent(deep_link: &str) -> String {
    if deep_link
        .split(['?', '&'])
        .skip(1)
        .any(|parameter| parameter == AGENT_ONBOARD_INTENT_QUERY)
    {
        return deep_link.to_string();
    }
    let separator = if deep_link.contains('?') { '&' } else { '?' };
    format!("{deep_link}{separator}{AGENT_ONBOARD_INTENT_QUERY}")
}

pub fn now_unix_secs() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| anyhow!("system clock before UNIX epoch: {error}"))
}

fn placeholder_response() -> AgentWalletsResponse {
    let zero = "0".repeat(64);
    let address = ScopedAddress {
        canonical: format!("{zero}::{zero}"),
        dapp_id: zero.clone(),
        account_address: format!("0:{zero}"),
    };
    AgentWalletsResponse {
        version: AGENT_WALLETS_BODY_VERSION,
        network: "net-a".to_string(),
        vault: address.clone(),
        hot: address,
    }
}

#[cfg(test)]
mod agent_onboard_intent_tests;

#[cfg(test)]
mod endpoint_and_diagnostics_tests;

#[cfg(test)]
mod complete_identity_tests;

#[cfg(test)]
mod reconciliation_tests;

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::HashSet;

    use bee_connect::dh::{
        compute_shared_secret, create_initial_state, derive_session_keys, generate_dh_keypair,
    };
    use ed25519_dalek::{Signer as _, SigningKey};

    use super::*;

    fn limits() -> SessionLimits {
        SessionLimits {
            session_ttl: Duration::from_secs(3_600),
            hello_poll_attempts: 2,
            hello_poll_interval: Duration::from_millis(1),
            response_poll_attempts: 2,
            response_poll_interval: Duration::from_millis(1),
            context_event_limit: 50,
            timestamp_future_skew: Duration::from_secs(30),
            agent_name_max_chars: 64,
        }
    }

    fn hot_secret() -> &'static str {
        "1111111111111111111111111111111111111111111111111111111111111111"
    }

    fn vault_secret() -> &'static str {
        "2222222222222222222222222222222222222222222222222222222222222222"
    }

    fn signing_public(secret: &str) -> String {
        let bytes: [u8; 32] = hex::decode(secret).unwrap().try_into().unwrap();
        hex::encode(SigningKey::from_bytes(&bytes).verifying_key().to_bytes())
    }

    #[test]
    fn new_session_deeplink_identifies_dexdo_cli_in_payload_and_description() {
        let expected_app_id = "0x0000000000000000000000000000000000000000000000000000000000000078";
        let hot_public = signing_public(hot_secret());
        let nonce = signing_public(vault_secret());
        let session = OnboardingSession::create(
            "test-agent",
            "net-a",
            "net-a.example",
            &hot_public,
            None,
            &nonce,
            limits(),
        )
        .unwrap();
        let SessionPhase::AwaitingWalletHello { invitation, .. } = session.phase else {
            panic!("a new onboarding must await wallet_hello")
        };
        let payload_b64url = invitation
            .deep_link
            .split_once("?payload=")
            .and_then(|(_, query)| query.split_once("&client_dh_public="))
            .map(|(payload, _)| payload)
            .expect("the connect deeplink must carry its payload");
        let payload = bee_connect::decode_connect_payload_b64url(payload_b64url).unwrap();

        assert_eq!(invitation.app_id, expected_app_id);
        assert_eq!(payload.app_id, expected_app_id);
        assert_eq!(payload.description, invitation.description);
        assert_eq!(
            payload.description.split(':').nth(2),
            Some(payload.app_id.as_str()),
            "the description and ConnectPayload must carry the same app_id"
        );
    }

    struct HelloFixture {
        session: OnboardingSession,
        hello: ResultOfWaitWalletHello,
        wallet_state: ConnectSessionState,
        client_state_before_send: ConnectSessionState,
        hot_public: String,
        vault_public: String,
    }

    fn hello_fixture(distinct_vault: bool) -> HelloFixture {
        let hot_public = signing_public(hot_secret());
        let vault_public = signing_public(vault_secret());
        let nonce_key = SigningKey::from_bytes(&[9u8; 32]);
        let nonce = hex::encode(nonce_key.verifying_key().to_bytes());
        let session = OnboardingSession::create(
            "test-agent",
            "net-a",
            "net-a.example",
            &hot_public,
            distinct_vault.then_some(vault_public.as_str()),
            &nonce,
            limits(),
        )
        .unwrap();
        let SessionPhase::AwaitingWalletHello { invitation, .. } = &session.phase else {
            unreachable!()
        };
        let now = invitation.created_at;

        let wallet_dh = generate_dh_keypair().unwrap();
        let shared =
            compute_shared_secret(&invitation.client_dh_secret, &wallet_dh.public_hex).unwrap();
        let session_keys = derive_session_keys(&shared, &invitation.session_id).unwrap();
        let mut client_state = create_initial_state(
            &session_keys,
            &invitation.client_dh_secret,
            &wallet_dh.public_hex,
        );
        let mut wallet_state = create_initial_state(
            &session_keys,
            &wallet_dh.secret_hex,
            &invitation.client_dh_public,
        );
        client_state.created_at = invitation.created_at;
        client_state.expires_at = invitation.expires_at;
        wallet_state.created_at = invitation.created_at;
        wallet_state.expires_at = invitation.expires_at;
        client_state.last_seen_seq = 1;
        wallet_state.last_sent_seq = 1;

        let body = serde_json::json!({
            "wallet_name": "fixture-wallet",
            "wallet_address": format!("0:{}", "a".repeat(64)),
            "nonce": nonce,
            "signature": hex::encode(nonce_key.sign(&hex::decode(&nonce).unwrap()).to_bytes()),
            "epk_public": hex::encode(nonce_key.verifying_key().to_bytes()),
        });
        let aad = connect_message_aad(
            &invitation.session_id,
            "w2c",
            1,
            CONNECT_MESSAGE_TYPE_WALLET_HELLO,
            now,
        )
        .unwrap();
        let encrypted =
            encrypt_connect_body(&body, &session_keys.encryption_root_hex, &aad).unwrap();
        let raw_message_json = serde_json::json!({
            "v": CONNECT_MESSAGE_VERSION,
            "session_id": invitation.session_id,
            "dir": "w2c",
            "seq": 1,
            "type": CONNECT_MESSAGE_TYPE_WALLET_HELLO,
            "ts": now,
            "dh_public": wallet_dh.public_hex,
            "enc": {
                "alg": CONNECT_MESSAGE_ENC_XCHACHA20POLY1305_HKDF_SHA256,
                "nonce": encrypted.nonce_b64url,
                "salt": encrypted.salt_b64url,
            },
            "body": encrypted.ciphertext_b64url,
        })
        .to_string();
        let hello = ResultOfWaitWalletHello {
            profile_address: format!("0:{}", "b".repeat(64)),
            event_id: "hello-event".to_string(),
            event_created_at: now,
            wallet_name: "fixture-wallet".to_string(),
            wallet_address: format!("0:{}", "a".repeat(64)),
            raw_message_json,
            session_state: client_state.clone(),
            nonce: Some(nonce),
            signature: Some(hex::encode(
                nonce_key
                    .sign(&nonce_key.verifying_key().to_bytes())
                    .to_bytes(),
            )),
            epk_public: Some(hex::encode(nonce_key.verifying_key().to_bytes())),
        };

        HelloFixture {
            session,
            hello,
            wallet_state,
            client_state_before_send: client_state,
            hot_public,
            vault_public,
        }
    }

    #[derive(Default)]
    struct FakeIo {
        hello: RefCell<Option<ResultOfWaitWalletHello>>,
        response: RefCell<Option<ObservedContextEvent>>,
        published: RefCell<HashSet<String>>,
        stale_request_observations: Cell<usize>,
        hello_calls: Cell<usize>,
        publish_calls: Cell<usize>,
        response_calls: Cell<usize>,
    }

    impl FakeIo {
        fn with_hello(hello: ResultOfWaitWalletHello) -> Self {
            Self {
                hello: RefCell::new(Some(hello)),
                ..Self::default()
            }
        }
    }

    #[async_trait(?Send)]
    impl BeeSessionIo for FakeIo {
        async fn wait_wallet_hello(
            &self,
            _invitation: &ResultOfCreateSharedKeySession,
            _limits: SessionLimits,
        ) -> Result<ResultOfWaitWalletHello> {
            self.hello_calls.set(self.hello_calls.get() + 1);
            self.hello
                .borrow()
                .as_ref()
                .cloned()
                .ok_or_else(|| anyhow!("no fixture hello"))
        }

        async fn request_exists(
            &self,
            request: &PreparedRequest,
            _limits: SessionLimits,
        ) -> Result<bool> {
            let exists = self.published.borrow().contains(&request.envelope_json);
            if exists && self.stale_request_observations.get() > 0 {
                self.stale_request_observations
                    .set(self.stale_request_observations.get() - 1);
                return Ok(false);
            }
            Ok(exists)
        }

        async fn publish_request(&self, request: &PreparedRequest) -> Result<()> {
            self.publish_calls.set(self.publish_calls.get() + 1);
            self.published
                .borrow_mut()
                .insert(request.envelope_json.clone());
            Ok(())
        }

        async fn wait_wallets_response(
            &self,
            _request: &PreparedRequest,
            _limits: SessionLimits,
        ) -> Result<ObservedContextEvent> {
            self.response_calls.set(self.response_calls.get() + 1);
            self.response
                .borrow()
                .as_ref()
                .cloned()
                .ok_or_else(|| anyhow!("no fixture response"))
        }
    }

    fn prepared_fixture(
        distinct_vault: bool,
    ) -> (
        OnboardingSession,
        PreparedRequest,
        ConnectSessionState,
        String,
        String,
    ) {
        let fixture = hello_fixture(distinct_vault);
        let SessionPhase::AwaitingWalletHello { nonce, invitation } = &fixture.session.phase else {
            unreachable!()
        };
        let request = prepare_request(
            &fixture.session.agent_name,
            &fixture.hot_public,
            distinct_vault.then_some(fixture.vault_public.as_str()),
            nonce,
            invitation,
            fixture.hello,
            limits(),
            now_unix_secs().unwrap(),
        )
        .unwrap();
        let session = OnboardingSession {
            file_version: SESSION_FILE_VERSION,
            agent_name: "test-agent".to_string(),
            network: "net-a".to_string(),
            endpoint: "net-a.example".to_string(),
            hot_pubkey: fixture.hot_public.clone(),
            vault_pubkey: distinct_vault.then_some(fixture.vault_public.clone()),
            phase: SessionPhase::RequestPrepared {
                request: clone_request(&request),
            },
        };
        (
            session,
            request,
            fixture.wallet_state,
            fixture.hot_public,
            fixture.vault_public,
        )
    }

    fn clone_request(request: &PreparedRequest) -> PreparedRequest {
        PreparedRequest {
            profile_address: request.profile_address.clone(),
            wallet_address: request.wallet_address.clone(),
            session_id: request.session_id.clone(),
            hello_event_id: request.hello_event_id.clone(),
            context_created_at_from: request.context_created_at_from,
            envelope_json: request.envelope_json.clone(),
            session_state: request.session_state.clone(),
        }
    }

    fn wallet_state_after_request(
        wallet_state: &ConnectSessionState,
        request: &PreparedRequest,
    ) -> ConnectSessionState {
        let envelope: ConnectEnvelope = serde_json::from_str(&request.envelope_json).unwrap();
        rekey_inbound(
            wallet_state,
            &envelope.dh_public,
            &request.session_id,
            envelope.seq,
        )
        .unwrap()
        .updated_state
    }

    fn response_event(
        request: &PreparedRequest,
        wallet_state: &ConnectSessionState,
        body: serde_json::Value,
        ts: u64,
    ) -> ObservedContextEvent {
        let wallet_state = wallet_state_after_request(wallet_state, request);
        let seq = wallet_state.next_outbound_seq().unwrap();
        let rekey = rekey_outbound(&wallet_state, &request.session_id, seq).unwrap();
        let aad = connect_message_aad(
            &request.session_id,
            "w2c",
            seq,
            AGENT_WALLETS_RESPONSE_TYPE,
            ts,
        )
        .unwrap();
        let encrypted = encrypt_connect_body(&body, &rekey.message_encryption_root, &aad).unwrap();
        ObservedContextEvent {
            id: "response-event".to_string(),
            created_at: ts,
            text: serde_json::json!({
                "v": CONNECT_MESSAGE_VERSION,
                "session_id": request.session_id,
                "dir": "w2c",
                "seq": seq,
                "type": AGENT_WALLETS_RESPONSE_TYPE,
                "ts": ts,
                "dh_public": rekey.new_dh_public.as_deref().unwrap(),
                "enc": {
                    "alg": CONNECT_MESSAGE_ENC_XCHACHA20POLY1305_HKDF_SHA256,
                    "nonce": encrypted.nonce_b64url,
                    "salt": encrypted.salt_b64url,
                },
                "body": encrypted.ciphertext_b64url,
            })
            .to_string(),
        }
    }

    fn valid_response_body() -> serde_json::Value {
        serde_json::json!({
            "v": 1,
            "network": "net-a",
            "vault_address": format!("{0}::{0}", "c".repeat(64)),
            "hot_address": format!("{0}::{0}", "d".repeat(64)),
        })
    }

    #[test]
    fn invitation_is_plain_bee_link_and_request_has_only_allowed_public_fields() {
        let fixture = hello_fixture(true);
        let SessionPhase::AwaitingWalletHello { invitation, .. } = &fixture.session.phase else {
            unreachable!()
        };
        assert_eq!(invitation.deep_link.matches('?').count(), 1);
        for forbidden in [
            fixture.hot_public.as_str(),
            fixture.vault_public.as_str(),
            hot_secret(),
            vault_secret(),
            AGENT_ONBOARD_REQUEST_TYPE,
            AGENT_ONBOARD_BODY_TYPE,
            "hot_pubkey",
            "vault_pubkey",
            "hot_address",
            "vault_address",
        ] {
            assert!(!invitation.deep_link.contains(forbidden), "{forbidden}");
            assert!(!invitation.payload_json.contains(forbidden), "{forbidden}");
        }

        let (_, request, wallet_state, _, _) = prepared_fixture(true);
        let envelope: ConnectEnvelope = serde_json::from_str(&request.envelope_json).unwrap();
        let rekey = rekey_inbound(
            &wallet_state,
            &envelope.dh_public,
            &request.session_id,
            envelope.seq,
        )
        .unwrap();
        let aad = connect_message_aad(
            &request.session_id,
            "c2w",
            envelope.seq,
            AGENT_ONBOARD_REQUEST_TYPE,
            envelope.ts,
        )
        .unwrap();
        let plaintext = decrypt_connect_body(
            &envelope.body,
            &envelope.enc.nonce,
            &envelope.enc.salt,
            &rekey.message_encryption_root,
            &aad,
        )
        .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&plaintext).unwrap();
        let keys = body
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        assert_eq!(
            keys,
            ["v", "type", "agent_name", "hot_pubkey", "vault_pubkey"]
                .into_iter()
                .map(str::to_string)
                .collect()
        );
        let all_output = format!(
            "{}\n{}\n{:?}",
            invitation.deep_link, request.envelope_json, fixture.session
        );
        assert!(!all_output.contains(hot_secret()));
        assert!(!all_output.contains(vault_secret()));
        assert!(!all_output.contains(&invitation.client_dh_secret));
        assert!(!all_output.contains(request.session_state.signing_secret.as_str()));
    }

    #[tokio::test]
    async fn invalid_missing_replayed_wrong_nonce_and_wrong_session_hello_never_prepare_a_request()
    {
        let mut cases = Vec::new();

        let fixture = hello_fixture(false);
        let mut missing = fixture.hello.clone();
        missing.signature = None;
        cases.push(("missing", fixture.session, missing));

        let fixture = hello_fixture(false);
        let mut invalid = fixture.hello.clone();
        invalid.signature = Some("00".repeat(64));
        cases.push(("invalid", fixture.session, invalid));

        let fixture = hello_fixture(false);
        let mut wrong_nonce = fixture.hello.clone();
        wrong_nonce.nonce = Some("ff".repeat(32));
        cases.push(("wrong_nonce", fixture.session, wrong_nonce));

        let fixture = hello_fixture(false);
        let mut wrong_session = fixture.hello.clone();
        let mut raw: serde_json::Value =
            serde_json::from_str(&wrong_session.raw_message_json).unwrap();
        raw["session_id"] = serde_json::Value::String("another-session".to_string());
        wrong_session.raw_message_json = raw.to_string();
        cases.push(("wrong_session", fixture.session, wrong_session));

        let fixture = hello_fixture(false);
        let mut replayed = fixture.hello.clone();
        let SessionPhase::AwaitingWalletHello { invitation, .. } = &fixture.session.phase else {
            unreachable!()
        };
        let stale = invitation.created_at.saturating_sub(1);
        let mut raw: serde_json::Value = serde_json::from_str(&replayed.raw_message_json).unwrap();
        raw["ts"] = serde_json::json!(stale);
        replayed.raw_message_json = raw.to_string();
        replayed.event_created_at = stale;
        cases.push(("replayed_stale", fixture.session, replayed));

        for (case, session, hello) in cases {
            let io = FakeIo::with_hello(hello);
            let result = session.advance(&io, limits()).await;
            assert!(result.is_err(), "{case} hello unexpectedly accepted");
            assert_eq!(io.publish_calls.get(), 0, "{case}");
            assert!(io.published.borrow().is_empty(), "{case}");
        }
    }

    #[tokio::test]
    async fn restart_reconciles_one_request_and_consumes_one_response_once() {
        let fixture = hello_fixture(false);
        let io = FakeIo::with_hello(fixture.hello);
        let prepared = fixture.session.advance(&io, limits()).await.unwrap();
        assert_eq!(prepared.phase_name(), "request_prepared");
        assert_eq!(io.publish_calls.get(), 0);

        let prepared_bytes = serde_json::to_vec(&prepared).unwrap();
        let first_restart: OnboardingSession = serde_json::from_slice(&prepared_bytes).unwrap();
        let awaiting = first_restart.advance(&io, limits()).await.unwrap();
        assert_eq!(awaiting.phase_name(), "awaiting_wallets_response");
        assert_eq!(io.publish_calls.get(), 1);

        let second_restart: OnboardingSession = serde_json::from_slice(&prepared_bytes).unwrap();
        let reconciled = second_restart
            .advance_after_restart(&io, limits())
            .await
            .unwrap();
        assert_eq!(reconciled.phase_name(), "awaiting_wallets_response");
        assert_eq!(
            io.publish_calls.get(),
            1,
            "restart after an ambiguous send must not publish twice"
        );
        assert_eq!(io.hello_calls.get(), 1, "hello replay must not be consumed");

        let SessionPhase::AwaitingWalletsResponse { request } = &reconciled.phase else {
            unreachable!()
        };
        *io.response.borrow_mut() = Some(response_event(
            request,
            &fixture.wallet_state,
            valid_response_body(),
            now_unix_secs().unwrap(),
        ));
        let received = reconciled.advance(&io, limits()).await.unwrap();
        assert_eq!(received.phase_name(), "response_received");
        assert_eq!(io.response_calls.get(), 1);

        let received_bytes = serde_json::to_vec(&received).unwrap();
        let consumed: OnboardingSession = serde_json::from_slice(&received_bytes).unwrap();
        let consumed = consumed.advance(&io, limits()).await.unwrap();
        assert_eq!(consumed.phase_name(), "response_received");
        assert_eq!(
            io.response_calls.get(),
            1,
            "a durable response must be consumed exactly once"
        );
    }

    #[tokio::test]
    async fn restart_waits_through_stale_index_after_successful_request_publish() {
        let (prepared, _, _, _, _) = prepared_fixture(false);
        let prepared_bytes = serde_json::to_vec(&prepared).unwrap();
        let io = FakeIo::default();

        let first_run: OnboardingSession = serde_json::from_slice(&prepared_bytes).unwrap();
        let awaiting = first_run.advance(&io, limits()).await.unwrap();
        assert_eq!(awaiting.phase_name(), "awaiting_wallets_response");
        assert_eq!(io.publish_calls.get(), 1);

        io.stale_request_observations.set(1);
        let restarted: OnboardingSession = serde_json::from_slice(&prepared_bytes).unwrap();
        let reconciled = restarted
            .advance_after_restart(&io, limits())
            .await
            .unwrap();

        assert_eq!(reconciled.phase_name(), "awaiting_wallets_response");
        assert_eq!(
            io.publish_calls.get(),
            1,
            "stale restart observations must not duplicate a successful request"
        );
        assert_eq!(io.stale_request_observations.get(), 0);
    }

    #[test]
    fn response_requires_post_send_ratchet_and_rejects_tamper_and_aad_mismatch() {
        let fixture = hello_fixture(false);
        let SessionPhase::AwaitingWalletHello { nonce, invitation } = &fixture.session.phase else {
            unreachable!()
        };
        let request = prepare_request(
            &fixture.session.agent_name,
            &fixture.hot_public,
            None,
            nonce,
            invitation,
            fixture.hello,
            limits(),
            now_unix_secs().unwrap(),
        )
        .unwrap();
        let now = now_unix_secs().unwrap();
        let event = response_event(&request, &fixture.wallet_state, valid_response_body(), now);
        consume_wallets_response("net-a", &request, event.clone(), limits(), now).unwrap();

        let pre_send = PreparedRequest {
            session_state: fixture.client_state_before_send,
            ..clone_request(&request)
        };
        assert!(
            consume_wallets_response("net-a", &pre_send, event.clone(), limits(), now).is_err()
        );

        for (case, field, value) in [
            ("session", "session_id", serde_json::json!("wrong-session")),
            ("direction", "dir", serde_json::json!("c2w")),
            ("type", "type", serde_json::json!("wrong-type")),
            ("sequence", "seq", serde_json::json!(9_999_999_999_999u64)),
            ("timestamp", "ts", serde_json::json!(now.saturating_add(1))),
        ] {
            let mut changed = event.clone();
            let mut envelope: serde_json::Value = serde_json::from_str(&changed.text).unwrap();
            envelope[field] = value;
            changed.text = envelope.to_string();
            assert!(
                consume_wallets_response("net-a", &request, changed, limits(), now).is_err(),
                "{case}"
            );
        }

        let mut tampered = event.clone();
        let mut envelope: serde_json::Value = serde_json::from_str(&tampered.text).unwrap();
        let body = envelope["body"].as_str().unwrap();
        let replacement = if body.starts_with('A') { "B" } else { "A" };
        envelope["body"] = serde_json::Value::String(format!("{replacement}{}", &body[1..]));
        tampered.text = envelope.to_string();
        assert!(consume_wallets_response("net-a", &request, tampered, limits(), now).is_err());

        let (_, _, post_receive, _) =
            consume_wallets_response("net-a", &request, event.clone(), limits(), now).unwrap();
        let replay_request = PreparedRequest {
            session_state: post_receive,
            ..clone_request(&request)
        };
        let replay =
            consume_wallets_response("net-a", &replay_request, event, limits(), now).unwrap_err();
        assert!(replay.to_string().contains("replay"), "{replay}");
    }

    #[test]
    fn response_schema_rejects_wrong_or_missing_fields_and_accepts_additions() {
        let (_, request, wallet_state, _, _) = prepared_fixture(false);
        let now = now_unix_secs().unwrap();
        let mut additive = valid_response_body();
        additive["future_field"] = serde_json::json!({"safe": true});
        let event = response_event(&request, &wallet_state, additive, now);
        consume_wallets_response("net-a", &request, event, limits(), now).unwrap();

        let cases = [
            (
                "version",
                serde_json::json!({
                    "v": 2,
                    "network": "net-a",
                    "vault_address": format!("{0}::{0}", "c".repeat(64)),
                    "hot_address": format!("{0}::{0}", "d".repeat(64)),
                }),
            ),
            (
                "network",
                serde_json::json!({
                    "v": 1,
                    "network": "mainnet",
                    "vault_address": format!("{0}::{0}", "c".repeat(64)),
                    "hot_address": format!("{0}::{0}", "d".repeat(64)),
                }),
            ),
            (
                "malformed_address",
                serde_json::json!({
                    "v": 1,
                    "network": "net-a",
                    "vault_address": "0:dead",
                    "hot_address": format!("{0}::{0}", "d".repeat(64)),
                }),
            ),
            (
                "mismatched_dapp",
                serde_json::json!({
                    "v": 1,
                    "network": "net-a",
                    "vault_address": format!("{}::{}", "c".repeat(64), "e".repeat(64)),
                    "hot_address": format!("{0}::{0}", "d".repeat(64)),
                }),
            ),
            (
                "missing",
                serde_json::json!({
                    "v": 1,
                    "network": "net-a",
                    "vault_address": format!("{0}::{0}", "c".repeat(64)),
                }),
            ),
            (
                "retyped",
                serde_json::json!({
                    "v": 1,
                    "network": "net-a",
                    "vault_address": format!("{0}::{0}", "c".repeat(64)),
                    "hot_address": 7,
                }),
            ),
        ];
        for (case, body) in cases {
            let event = response_event(&request, &wallet_state, body, now);
            assert!(
                consume_wallets_response("net-a", &request, event, limits(), now).is_err(),
                "{case}"
            );
        }
    }

    #[test]
    fn stale_and_future_response_timestamps_fail_closed() {
        let (_, request, wallet_state, _, _) = prepared_fixture(false);
        let now = now_unix_secs().unwrap();
        let stale = request.session_state.created_at.saturating_sub(1);
        let event = response_event(&request, &wallet_state, valid_response_body(), stale);
        assert!(consume_wallets_response("net-a", &request, event, limits(), now).is_err());

        let future = now
            .saturating_add(limits().timestamp_future_skew.as_secs())
            .saturating_add(1);
        let event = response_event(&request, &wallet_state, valid_response_body(), future);
        assert!(consume_wallets_response("net-a", &request, event, limits(), now).is_err());
    }
}
