//! `dexdo wallet onboard`: one explicit, resumable bee handoff to canonical agent wallets.

use crate::cli::args::WalletOnboardArgs;
use anyhow::Result;

pub(crate) use live::ONBOARD_STEPS;

mod live {
    use std::path::{Path, PathBuf};

    use anyhow::{anyhow, bail, Context, Result};
    use async_trait::async_trait;
    use dexdo_core::params::WalletOnboardingParams;
    use dexdo_core::{Address, ChainClient, KeyPair};
    use dexdo_wallet_onboarding::{
        AgentWalletsResponse, CanonicalBeeSessionIo, OnboardingSession, SessionLimits,
    };
    use qrcode::render::svg;
    use qrcode::QrCode;
    use serde_json::Value;
    use zeroize::Zeroizing;

    use crate::cli::args::WalletOnboardArgs;
    use crate::cli::note::{resolve_private_file_path, write_private_atomic};
    use crate::cli::support::read_secret_hex;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct WalletAccountFact {
        status: String,
        code_hash: Option<String>,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ValidatedWalletPair {
        network: String,
        vault_scoped_address: String,
        hot_scoped_address: String,
    }

    fn wallet_validation_endpoint(endpoint: &str) -> Result<String> {
        dexdo_core::normalize_endpoint(endpoint)
            .context("normalize selected network endpoint for wallet validation")
    }

    /// The command boundary: the one place this command decides what its endpoint is.

    /// Lifted out of `run` unchanged so it can be asserted on. `run` is unreachable from a test --
    /// the next thing it does is talk to a chain -- so with the normalisation inline there was
    /// nowhere for a regression to bind, and the seam that matters is exactly this one: what the
    /// bee `ClientContext`, the `AuthProfile` write, the `ConnectClient` poll, the chain read and
    /// the durable state all receive.

    /// Normalise ONCE, here, and let every later use take this value.

    /// The defaults are bare hosts (one chain's host). Reads survive that, but the WRITE
    /// that publishes `agent_onboard_request` through `AuthProfile.add_context_text` goes out on
    /// the wrong scheme and fails as a bare `Send message` -- observed live on mainnet, with the
    /// request already prepared and durably stored. `wallet_validation_endpoint` normalised too
    /// late: only for validating an already-received Vault/Hot pair, never for the publish.
    fn onboarding_endpoint() -> Result<String> {
        // The manifest names it, and nothing else does: `--endpoint` used to rank above it and the
        // network's own default below it, and removed both.

        // NO AGREEMENT CHECK HERE, AND THAT IS THE POINT: the network being onboarded is DERIVED
        // from this same file. `network_from_manifest` loads `manifest_path()` and hands its
        // `network` field to `WalletNetwork::from_manifest_label`, so a disagreement between the
        // two is not representable -- the manifest cannot describe a chain other than the one it
        // names. `run_remove_archived` is the case that still needs the check, because there the
        // network comes from the ARCHIVED BINDING and the manifest is a second, independent source.

        // A check stood here anyway, comparing `deployed.network` against the derived label as raw
        // strings. It could fire on exactly one input -- a label padded with whitespace, which the
        // derivation trims and the raw comparison does not -- and the refusal it then printed
        // contradicted itself: "names net-a, and this onboarding is for net-a". What it

        // chain and answered from another, back when the network came from a flag rather than from
        // the file. The flag is gone and so is the divergence it created.
        let manifest = crate::cli::commands::manifest_path()?;
        let network = crate::cli::wallet::network_from_manifest()?;
        let endpoint = crate::cli::wallet::wallet_read_endpoint(Some(manifest.as_path()), network)?;
        let endpoint = endpoint.trim().to_string();
        if endpoint.is_empty() {
            bail!("the network names no endpoint to publish the onboarding request to");
        }
        dexdo_core::normalize_endpoint(&endpoint)
            .context("normalize the wallet onboarding endpoint")
    }

    /// The `note deploy` follow-up, as a line that survives the shell an operator pastes it into.

    /// this used to append `<existing note-deploy arguments>` to the command. That is not
    /// argv -- a POSIX shell reads `<existing` as an input redirection, opens a file named
    /// `existing`, and hands the binary two stray tokens, so the line an operator pastes is not the
    /// line that was printed. It is the defect exactly, and it survived here because the span
    /// carries no backticks while the lint reads backticked spans only.

    /// Repaired the way this tree repairs it elsewhere (`release_dispute_guidance`): the command
    /// renders the arguments it knows, and the ones it cannot render are left to the prose above,
    /// which the shell never sees.
    fn note_deploy_handoff_command(wallets: &ValidatedWalletPair, hot_key: &Path) -> String {
        // Through `command_here`, so the line carries this run's `--data-dir`. Without it the
        // follow-up deploys into the platform default and the binding it was just told to use is in
        // another directory entirely.
        crate::cli::support::command_here(&format!(
            "note deploy --nominal N100 --multisig-address {} --multisig-private-key {}",
            wallets.hot_scoped_address,
            hot_key.display()
        ))
    }

    #[async_trait(?Send)]
    trait WalletChainReader {
        async fn account(&self, address: &Address) -> Result<Option<WalletAccountFact>>;

        async fn getter(&self, address: &Address, method: &'static str) -> Result<Option<Value>>;
    }

    #[async_trait(?Send)]
    impl WalletChainReader for ChainClient {
        async fn account(&self, address: &Address) -> Result<Option<WalletAccountFact>> {
            Ok(self
                .get_account(address)
                .await?
                .map(|account| WalletAccountFact {
                    status: account.status,
                    code_hash: account.code_hash,
                }))
        }

        async fn getter(&self, address: &Address, method: &'static str) -> Result<Option<Value>> {
            self.run_getter(
                address,
                dexdo_core::canonical_multisig::MULTISIG_ABI_JSON,
                method,
                serde_json::json!({}),
            )
            .await
        }
    }

    struct LocalAgentKeys {
        hot: KeyPair,
        vault: Option<KeyPair>,
    }

    /// What onboarding does, in the order it does it, in the two tenses the display reads in.

    /// Every one of these is a wait on somebody else -- the operator's phone, then the wallet, then
    /// the chain, then a download. A wait nobody named is what made this command look like a wall of
    /// sentences with no shape.
    pub(crate) const ONBOARD_STEPS: [(&str, &str); 4] = [
        (
            // Also the operator's move: nothing happens until a phone reads the code on their
            // screen. Written as the instruction it is, and drawn amber for the same reason.
            "scan the code above with Acki Nacki Wallet",
            "wallet app connected",
        ),
        (
            // The wallet does not start until the operator approves it, so this is an instruction,
            // not an observation. Said the other way round -- "waiting for the wallet app to
            // deploy" -- it reads as the client watching something already in motion, and an
            // operator with the phone in their hand waits for a deploy nobody asked them to
            // confirm.
            "approve the Vault/Hot deploy in Acki Nacki Wallet",
            "Vault and Hot deployed by the wallet",
        ),
        (
            "checking the pair on chain",
            "pair checked on chain and bound",
        ),
        (
            "preparing the proving reference string",
            "proving material ready",
        ),
    ];

    pub(super) async fn run(
        args: WalletOnboardArgs,
        binding_id: &str,
    ) -> Result<crate::cli::wallet::WalletBinding> {
        let params = WalletOnboardingParams::canonical();
        let limits = session_limits(params);
        let endpoint = onboarding_endpoint()?;

        let (state_arg, hot_key_arg) = owner_only_paths(&args)?;
        let state_path = resolve_private_file_path(&state_arg, "wallet onboarding state")?;
        let (mut session, keys, created) =
            load_or_create_session(&args, &endpoint, &state_path, limits)?;
        let mut reconcile_prepared_request = !created && session.phase_name() == "request_prepared";
        // Both of these report bookkeeping, not a result: which files were written before the
        // command reached the network, and which durable phase a rerun picked up from. They matter
        // when something has to be reconstructed after the fact, and they are noise while the
        // operator is looking for the thing they have to scan -- so they go out at `info`, where
        // `RUST_LOG` reaches them and the quiet default does not.
        if created {
            tracing::info!(
                "wallet onboarding state and local Hot{} key persisted owner-only before connecting",
                if keys.vault.is_some() {
                    "/Vault"
                } else {
                    ""
                }
            );
        } else {
            tracing::info!(
                "resuming wallet onboarding from durable phase `{}`",
                session.phase_name()
            );
        }

        if let Some(deep_link) = session.deep_link() {
            print_invitation(
                deep_link,
                args.qr_file.as_deref(),
                args.terminal_qr,
                std::io::IsTerminal::is_terminal(&std::io::stdout()),
                &mut std::io::stdout(),
            )?;
        }

        // The display starts AFTER the invitation is drawn: the code goes to stdout, the live line
        // to stderr, and a spinner running while a QR is being printed would rewrite a line of it.
        // From here on nothing else prints, so the two never meet.
        let _status = crate::cli::progress::Status::with_plan(
            ONBOARD_STEPS[0].0,
            ONBOARD_STEPS.iter().copied(),
        );
        let io = CanonicalBeeSessionIo::new(&endpoint)?;
        let mut invitation_consumed_announced = false;
        loop {
            // Once a signed `wallet_hello` has been accepted, the invitation is spent. Rescanning
            // it builds fresh session keys against an `AuthProfile` that already has an owner, and
            // the wallet answers `AuthProfile owner mismatch`. Announced once, on entering the
            // phase -- whether by advancing into it or by resuming into it.
            if session.phase_name() == "request_prepared" && !invitation_consumed_announced {
                print_invitation_consumed();
                invitation_consumed_announced = true;
            }
            match session.phase_name() {
                "awaiting_wallet_hello" => {
                    crate::cli::progress::step_needs_you(ONBOARD_STEPS[0].0);
                    session = session.advance(&io, limits).await?;
                    save_session(&state_path, &session)?;
                    tracing::info!(
                        "signed wallet_hello verified; request prepared durably before publication"
                    );
                }
                "request_prepared" => {
                    session = if reconcile_prepared_request {
                        let session = session.advance_after_restart(&io, limits).await?;
                        reconcile_prepared_request = false;
                        session
                    } else {
                        session.advance(&io, limits).await?
                    };
                    save_session(&state_path, &session)?;
                    // The wallet waits for the operator's approval, so the line is theirs to act on.
                    crate::cli::progress::step_needs_you(ONBOARD_STEPS[1].0);
                    tracing::info!(
                        "one agent_onboard_request reconciled; post-send bee ratchet persisted"
                    );
                }
                "awaiting_wallets_response" => {
                    session = session.advance(&io, limits).await?;
                    save_session(&state_path, &session)?;
                    crate::cli::progress::step(ONBOARD_STEPS[2].0);
                    tracing::info!(
                        "durable agent_wallets_response authenticated and consumed; validating chain facts"
                    );
                }
                "response_received" | "complete" => {
                    let response = session
                        .response()
                        .cloned()
                        .ok_or_else(|| anyhow!("wallet onboarding response state is missing"))?;
                    let chain_endpoint = wallet_validation_endpoint(&endpoint)?;
                    let chain = ChainClient::connect(&chain_endpoint)
                        .context("connect selected network for wallet validation")?;
                    let vault_public = keys
                        .vault
                        .as_ref()
                        .map(KeyPair::public_hex)
                        .unwrap_or_else(|| keys.hot.public_hex());
                    let validated = validate_wallet_pair(
                        &chain,
                        &response,
                        keys.hot.public_hex(),
                        vault_public,
                    )
                    .await?;
                    if session.phase_name() == "response_received" {
                        session = session.mark_complete()?;
                        save_session(&state_path, &session)?;
                    }
                    print_handoff(&validated, &hot_key_arg);
                    // The RESOLVED Hot key path, not `args.hot_key`: when the operator passed
                    // nothing the canonical default has been rebased under the effective data
                    // directory, and that is the file this flow actually wrote. Recording the
                    // unresolved default would name a path that holds no key.

                    // `wallet_address` is read AFTER `mark_complete`, deliberately: this is the
                    // value completion used to discard, and reading it here is what proves it
                    // survived.
                    return Ok(binding_of(
                        crate::cli::wallet::WalletNetwork::from_manifest_label(&validated.network)?,
                        binding_id,
                        &validated,
                        &hot_key_arg,
                        args.vault_key.as_deref(),
                        session.wallet_address(),
                    ));
                }
                phase => bail!("unsupported wallet onboarding phase `{phase}`"),
            }
        }
    }

    /// The binding this flow proved. It is BUILT, never written: `run_selected` commits it once
    /// through `WalletStore::commit_active`, which archives whatever it replaces.

    /// This is only about `wallet/binding.json`. The bee SESSION state (`--state`, written by
    /// `save_session`) stays exactly as it is and is not a binding: the specification keeps it as
    /// the temporary, resumable state of an Acki Nacki onboarding that may take an hour, and
    /// deleting it would break resumption.

    /// `hot_address` is the Hot and only the Hot. dexdo spends from the Hot; the Vault is the
    /// human's custody instrument. `vault_address` is recorded ALONGSIDE it -- not as a second
    /// candidate for spending, but because the `ackinacki-wallet` funding flow addresses its
    /// Vault -> Hot top-up request to it (`cli::wallet_funding`), and a binding without it cannot
    /// ask the Vault for anything. Nothing reads it as a wallet to spend from.

    /// `vault_key_file` is the key that signs the future Vault -> Hot request, so it is recorded
    /// whether or not it is a separate key. The specification's own binding example fixes both
    /// cases -- "owner-only path OR the same Hot key file" -- and the code already treats them as
    /// one: when `--vault-key` is absent, `run` validates the Vault against the HOT public key,
    /// because the Hot key is then the Vault custodian too. Leaving the field `None` in that case
    /// would record "this binding has no Vault key" about a binding that does; leaving it `None`
    /// when `--vault-key` WAS given loses a separately generated key that only this flow knows the
    /// path of.

    /// `push_profile_address` is `hello.wallet_address` -- the multifactor wallet address, which
    /// the recorded answer makes explicitly a DIFFERENT value from
    /// `hello.profile_address`. It is reserved non-secret metadata: nothing reads it yet, and it
    /// is recorded so that it is not lost when onboarding completes, which is exactly what used to
    /// happen. It is also optional by that same answer, so a wallet that sends no address yields
    /// `None` here rather than failing an onboarding that is otherwise proved. The Connect
    /// `AuthProfile` address is NOT written here: it is the other value, and it is kept in the
    /// completed onboarding state instead.
    fn binding_of(
        network: crate::cli::wallet::WalletNetwork,
        binding_id: &str,
        validated: &ValidatedWalletPair,
        hot_key: &Path,
        vault_key: Option<&Path>,
        push_profile_address: Option<&str>,
    ) -> crate::cli::wallet::WalletBinding {
        crate::cli::wallet::WalletBinding {
            version: crate::cli::wallet::BINDING_VERSION,
            id: binding_id.to_string(),
            provider: crate::cli::wallet::WalletProvider::AckinackiWallet,
            // The label the WALLET reported, taken as it came. Mapping it onto a closed pair used
            // to silently file anything that was not `mainnet` under the test network -- including
            // a chain this binary had never heard of.
            network,
            hot_address: validated.hot_scoped_address.clone(),
            vault_address: Some(validated.vault_scoped_address.clone()),
            hot_key_file: Some(hot_key.to_path_buf()),
            vault_key_file: Some(vault_key.unwrap_or(hot_key).to_path_buf()),
            hot_seed_file: None,
            push_profile_address: push_profile_address
                .map(str::trim)
                .filter(|address| !address.is_empty())
                .map(str::to_string),
        }
    }

    fn session_limits(params: WalletOnboardingParams) -> SessionLimits {
        SessionLimits {
            session_ttl: params.session_ttl,
            hello_poll_attempts: params.hello_poll_attempts,
            hello_poll_interval: params.hello_poll_interval,
            response_poll_attempts: params.response_poll_attempts,
            response_poll_interval: params.response_poll_interval,
            context_event_limit: params.context_event_limit,
            timestamp_future_skew: params.timestamp_future_skew,
            agent_name_max_chars: params.agent_name_max_chars,
        }
    }

    /// The dispatcher has already resolved clap defaults inside this attempt's binding draft.
    /// Explicit paths remain exact and are deliberately not rebased here.
    fn owner_only_paths(args: &WalletOnboardArgs) -> Result<(PathBuf, PathBuf)> {
        Ok((
            args.state
                .clone()
                .context("wallet dispatcher did not resolve --state")?,
            args.hot_key
                .clone()
                .context("wallet dispatcher did not resolve --hot-key")?,
        ))
    }

    fn load_or_create_session(
        args: &WalletOnboardArgs,
        endpoint: &str,
        state_path: &Path,
        limits: SessionLimits,
    ) -> Result<(OnboardingSession, LocalAgentKeys, bool)> {
        let hot_path = resolve_private_file_path(
            args.hot_key
                .as_deref()
                .context("wallet dispatcher did not resolve --hot-key")?,
            "wallet onboarding Hot key",
        )?;
        let vault_path = args
            .vault_key
            .as_deref()
            .map(|path| resolve_private_file_path(path, "wallet onboarding Vault key"))
            .transpose()?;
        ensure_distinct_paths(state_path, &hot_path, vault_path.as_deref())?;

        if let Some(session) = load_session(state_path)? {
            validate_resume_arguments(&session, args, endpoint)?;
            let hot = load_key(&hot_path, "--hot-key")?;
            if session.hot_pubkey != hot.public_hex() {
                bail!("--hot-key does not match the Hot public key in durable onboarding state");
            }
            let vault = match (session.vault_pubkey.as_deref(), vault_path.as_deref()) {
                (Some(expected), Some(path)) => {
                    let key = load_key(path, "--vault-key")?;
                    if expected != key.public_hex() {
                        bail!(
                            "--vault-key does not match the Vault public key in durable onboarding state"
                        );
                    }
                    Some(key)
                }
                (None, None) => None,
                (Some(_), None) => {
                    bail!("durable onboarding state requires the original --vault-key path")
                }
                (None, Some(_)) => {
                    bail!("durable onboarding state was created without a distinct Vault key")
                }
            };
            return Ok((session, LocalAgentKeys { hot, vault }, false));
        }

        ensure_new_file(&hot_path, "--hot-key")?;
        if let Some(path) = vault_path.as_deref() {
            ensure_new_file(path, "--vault-key")?;
        }

        let hot = KeyPair::generate();
        let vault = vault_path.as_ref().map(|_| KeyPair::generate());
        if vault
            .as_ref()
            .is_some_and(|vault| vault.public_hex() == hot.public_hex())
        {
            bail!("generated Vault key unexpectedly equals the Hot key");
        }
        write_private_atomic(&hot_path, hot.secret_hex().as_bytes())
            .context("persist generated Hot key before wallet onboarding")?;
        if let (Some(path), Some(key)) = (vault_path.as_deref(), vault.as_ref()) {
            write_private_atomic(path, key.secret_hex().as_bytes())
                .context("persist generated Vault key before wallet onboarding")?;
        }

        let nonce = KeyPair::generate().public_hex().to_string();
        let session = OnboardingSession::create(
            &args.agent_name,
            crate::cli::wallet::network_from_manifest()?.as_str(),
            endpoint,
            hot.public_hex(),
            vault.as_ref().map(KeyPair::public_hex),
            &nonce,
            limits,
        )?;
        save_session(state_path, &session)
            .context("persist fresh bee session after local keys were safely stored")?;
        Ok((session, LocalAgentKeys { hot, vault }, true))
    }

    fn validate_resume_arguments(
        session: &OnboardingSession,
        args: &WalletOnboardArgs,
        endpoint: &str,
    ) -> Result<()> {
        session.validate_file()?;
        if session.agent_name != args.agent_name.trim() {
            bail!("--agent-name does not match durable onboarding state");
        }
        if session.network != crate::cli::wallet::network_from_manifest()?.as_str() {
            bail!(
                "the durable onboarding state was written for network `{}`, and this run is on a \
                 different one. Point DEXDO_MANIFEST at the manifest this session was started \
                 under, or start a new session.",
                session.network
            );
        }
        // Sessions written before the endpoint was normalised hold a bare host, so compare the
        // normalised forms rather than the stored text: an operator mid-onboarding must not be
        // forced to delete durable state -- it carries a verified `wallet_hello` and a prepared
        // request that cannot be reproduced without scanning a new QR.
        let stored = dexdo_core::normalize_endpoint(&session.endpoint)
            .unwrap_or_else(|_| session.endpoint.clone());
        let given =
            dexdo_core::normalize_endpoint(endpoint).unwrap_or_else(|_| endpoint.to_string());
        if stored != given {
            bail!("--endpoint does not match durable onboarding state");
        }
        Ok(())
    }

    fn load_key(path: &Path, argument: &str) -> Result<KeyPair> {
        let secret = Zeroizing::new(read_secret_hex(path, argument)?);
        KeyPair::from_secret_hex(secret.as_str())
            .with_context(|| format!("parse {argument} {}", path.display()))
    }

    fn ensure_distinct_paths(state: &Path, hot: &Path, vault: Option<&Path>) -> Result<()> {
        if state == hot {
            bail!("--state and --hot-key must be different files");
        }
        if let Some(vault) = vault {
            if state == vault || hot == vault {
                bail!("--state, --hot-key, and --vault-key must be different files");
            }
        }
        Ok(())
    }

    fn ensure_new_file(path: &Path, argument: &str) -> Result<()> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => bail!(
                "{argument} {} already exists while --state does not; refusing to overwrite a key",
                path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => bail!("inspect {argument} {}: {error}", path.display()),
        }
    }

    fn load_session(path: &Path) -> Result<Option<OnboardingSession>> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => bail!("read wallet onboarding state {}: {error}", path.display()),
        };
        let session: OnboardingSession = serde_json::from_slice(&bytes).map_err(|error| {
            anyhow!(
                "wallet onboarding state {} is not valid JSON: {error}",
                path.display()
            )
        })?;
        session.validate_file()?;
        Ok(Some(session))
    }

    fn save_session(path: &Path, session: &OnboardingSession) -> Result<()> {
        session.validate_file()?;
        let bytes = serde_json::to_vec_pretty(session)
            .context("serialize durable wallet onboarding state")?;
        write_private_atomic(path, &bytes)
            .with_context(|| format!("persist wallet onboarding state {}", path.display()))
    }

    /// Show the invitation where the operator can act on it.

    /// Two readers want opposite things from this output, and `interactive` is which one is on the
    /// other end.

    /// A PARENT PROCESS reads the first line of stdout and gets the deep link to hand on. That
    /// position is a machine contract, older than, and it holds wherever a program is reading:
    /// a pipe, a redirection, a capture.

    /// A HUMAN gets nothing from it. The wallet is a phone application, the scheme is handled on
    /// the phone, and this is a workstation console -- there is nothing to tap, and a line of 670
    /// opaque characters is the largest thing on their screen. So on a terminal the link is not
    /// printed at all, and the scannable code is: it is the only part of this output an operator
    /// can use. Anyone who does want the string still has the machine channel -- piping the command
    /// puts the link back on line one, because that is exactly the case the contract is about.

    /// The code itself is printed unconditionally either way: no flag to know about, no file to go
    /// looking for. Where the terminal answers the capability probe it is an inline image -- an
    /// escape sequence in this stream, so the module size stops being the font size and the code
    /// gets small; where it does not, it is the compact text rendering. Neither opens a window, and
    /// neither writes to disk: a file is produced only by an explicit `--qr-file`.
    fn print_invitation(
        deep_link: &str,
        qr_file: Option<&Path>,
        _terminal_qr: bool,
        interactive: bool,
        output: &mut dyn std::io::Write,
    ) -> Result<()> {
        if !interactive {
            writeln!(output, "{deep_link}")?;
        }
        if let Some(path) = qr_file {
            // The saved file keeps the ordinary error correction. It is an image with no size
            // constraint, so the trade the drawn code makes below -- correction for a smaller
            // symbol -- buys nothing here, and its bytes are pinned by the intent regression.
            let svg = QrCode::new(deep_link.as_bytes())
                .context("ordinary bee connection deep link does not fit a QR code")?
                .render::<svg::Color>()
                .quiet_zone(true)
                .build();
            write_private_atomic(path, svg.as_bytes())
                .with_context(|| format!("save wallet onboarding QR code {}", path.display()))?;
            writeln!(output, "QR code saved to {}", path.display())?;
        }
        writeln!(output, "Scan this in the Acki Nacki Wallet app:")?;
        let scannable = crate::cli::qr_compact::smallest_code(deep_link.as_bytes())?;
        crate::cli::qr_display::write_qr(output, &scannable)
            .context("render the bee connection QR code")?;
        writeln!(output, "waiting for the wallet's signed hello...")?;
        output
            .flush()
            .context("flush wallet onboarding invitation")?;
        Ok(())
    }

    fn print_invitation_consumed() {
        crate::cli::progress::tick("code scanned; do not scan it again");
        tracing::info!(
            "the invitation is spent; rerunning this command with the same state and key files \
             resumes rather than re-invites"
        );
    }

    /// The binding as the result of this command: what was created, and where what signs with it
    /// lives.

    /// The key file is named because counts a binding as an artifact -- something that
    /// exists after the command and cannot be found again without the client -- and an artifact is
    /// named with the file that holds its secret. It said the addresses and stopped, so the one
    /// thing an operator has to keep, and keep private, was the one thing it did not mention.
    /// The hand-off block: what the operator must do in the wallet application, and where.


    /// result on stdout and everything else on stderr, and the result for all three providers is
    /// printed by one function after this returns. On stdout it also broke `--json`: this block
    /// went out first, so the "one JSON document" a runtime parses arrived with a styled human
    /// heading in front of it and `jq` failed on the first line.
    fn print_handoff(wallets: &ValidatedWalletPair, hot_key: &Path) {
        // The live line comes down before the result goes up, or the two write over each other.
        crate::cli::progress::complete();
        use crate::cli::style::{self, Palette, Role};
        let palette = Palette::stderr();
        eprintln!(
            "{}",
            style::glyph_line(
                palette,
                style::OK,
                Role::Ok,
                &format!(
                    "wallet bound {} {}",
                    style::paint(palette, Role::Label, "\u{b7}"),
                    style::paint(palette, Role::Bold, &wallets.network.to_string())
                )
            )
        );
        eprintln!(
            "{}",
            style::field(palette, "vault", &wallets.vault_scoped_address, Role::Id)
        );
        eprintln!(
            "{}",
            style::field(palette, "hot", &wallets.hot_scoped_address, Role::Id)
        );
        eprintln!(
            "{}",
            style::field(
                palette,
                "secret",
                &format!(
                    "{} holds the Hot key this binding signs with - keep it private",
                    hot_key.display()
                ),
                Role::Text
            )
        );
        // What the binding is FOR. A wallet on its own trades nothing here: trading happens from a
        // note, so the next flow is deploying one, and the operator is told what a note is before
        // being told to make one. The command is the one line meant to be copied, so it carries the
        // colour that means "yours to act on".
        eprintln!(
            "{}",
            style::field(
                palette,
                "next",
                &style::paint(
                    palette,
                    Role::Wait,
                    "fund Hot in the wallet app, then deploy a note - the dex's own anonymous"
                ),
                Role::Text
            )
        );
        eprintln!(
            "{}",
            style::field_continued(&style::paint(
                palette,
                Role::Wait,
                "wallet, the account trading is done from:"
            ))
        );
        eprintln!(
            "{}",
            style::field_continued(&style::paint(
                palette,
                Role::Wait,
                &note_deploy_handoff_command(wallets, hot_key)
            ))
        );
    }

    /// The exact on-chain shape one half of the agreed Acki Nacki pair must have.

    /// Recorded on (2026-08-12): "owners are `[K0, K1, matching_agent_key]`; Vault/Hot
    /// transaction confirms `2`/`1`, data confirms `2` on both", restated for the Vault as "the
    /// exact form: exactly three pubkey custodians including the local Vault key,
    /// `requiredTxnConfirms=2` and `requiredDataConfirms=2`".

    /// Every field is a money-safety invariant, so each one REFUSES rather than warns. A Hot that
    /// confirms with fewer signatures than agreed, or that carries a fourth custodian nobody
    /// intended, is a wallet a third party can spend from -- and this is the last point before the
    /// binding hands its address and key to `note deploy`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct AgreedWalletShape {
        custodians: usize,
        required_txn_confirms: u8,
        required_data_confirms: u8,
    }

    impl AgreedWalletShape {
        fn vault(params: WalletOnboardingParams) -> Self {
            Self {
                custodians: params.agent_wallet_custodians,
                required_txn_confirms: params.vault_required_txn_confirms,
                required_data_confirms: params.agent_wallet_required_data_confirms,
            }
        }

        fn hot(params: WalletOnboardingParams) -> Self {
            Self {
                custodians: params.agent_wallet_custodians,
                required_txn_confirms: params.hot_required_txn_confirms,
                required_data_confirms: params.agent_wallet_required_data_confirms,
            }
        }
    }

    async fn validate_wallet_pair(
        chain: &dyn WalletChainReader,
        response: &AgentWalletsResponse,
        hot_public: &str,
        vault_public: &str,
    ) -> Result<ValidatedWalletPair> {
        if response.hot.account_address == response.vault.account_address {
            bail!("wallet onboarding response must contain distinct Hot and Vault accounts");
        }
        let params = WalletOnboardingParams::canonical();
        validate_wallet(
            chain,
            "Vault",
            &response.vault.account_address,
            vault_public,
            AgreedWalletShape::vault(params),
        )
        .await?;
        validate_wallet(
            chain,
            "Hot",
            &response.hot.account_address,
            hot_public,
            AgreedWalletShape::hot(params),
        )
        .await?;
        Ok(ValidatedWalletPair {
            network: response.network.clone(),
            vault_scoped_address: response.vault.canonical.clone(),
            hot_scoped_address: response.hot.canonical.clone(),
        })
    }

    async fn validate_wallet(
        chain: &dyn WalletChainReader,
        role: &str,
        address: &str,
        expected_custodian: &str,
        shape: AgreedWalletShape,
    ) -> Result<()> {
        let address = Address::parse(address)
            .with_context(|| format!("{role} wallet returned an invalid account address"))?;
        let display = address.with_workchain();
        let account = chain
            .account(&address)
            .await
            .with_context(|| format!("read {role} wallet {display}"))?
            .ok_or_else(|| anyhow!("{role} wallet {display} was not found"))?;
        if account.status != "Active" {
            bail!(
                "{role} wallet {display} is not Active (status={})",
                account.status
            );
        }
        let code_hash = normalize_code_hash(
            account
                .code_hash
                .as_deref()
                .ok_or_else(|| anyhow!("{role} wallet {display} has no code hash"))?,
        )
        .with_context(|| format!("{role} wallet {display} code hash"))?;
        if code_hash != dexdo_core::canonical_multisig::CODE_HASH {
            bail!(
                "{role} wallet {display} has unsupported code hash {code_hash}; expected canonical {}",
                dexdo_core::canonical_multisig::CODE_HASH
            );
        }

        let version = chain
            .getter(&address, "getVersion")
            .await
            .with_context(|| format!("read {role} wallet {display} getVersion"))?
            .ok_or_else(|| anyhow!("{role} wallet {display} getVersion returned no output"))?;
        let actual_version = required_string(&version, "value0", role, &display, "getVersion")?;
        let actual_name = required_string(&version, "value1", role, &display, "getVersion")?;
        if actual_version != dexdo_core::canonical_multisig::VERSION
            || actual_name != dexdo_core::canonical_multisig::CONTRACT_NAME
        {
            bail!(
                "{role} wallet {display} is {actual_name} {actual_version}; expected {} {}",
                dexdo_core::canonical_multisig::CONTRACT_NAME,
                dexdo_core::canonical_multisig::VERSION
            );
        }

        let custodians = chain
            .getter(&address, "getCustodians")
            .await
            .with_context(|| format!("read {role} wallet {display} getCustodians"))?
            .ok_or_else(|| anyhow!("{role} wallet {display} getCustodians returned no output"))?;
        ensure_custodian_shape(
            role,
            &display,
            &custodians,
            expected_custodian,
            shape.custodians,
        )?;

        let parameters = chain
            .getter(&address, "getParameters")
            .await
            .with_context(|| format!("read {role} wallet {display} getParameters"))?
            .ok_or_else(|| anyhow!("{role} wallet {display} getParameters returned no output"))?;
        let txn_confirms = required_u8(&parameters, "requiredTxnConfirms").ok_or_else(|| {
            anyhow!(
                "{role} wallet {display} getParameters has invalid or missing requiredTxnConfirms"
            )
        })?;
        if txn_confirms != shape.required_txn_confirms {
            bail!(
                "{role} wallet {display} requires {txn_confirms} transaction confirmations; the agreed {role} shape is exactly {}",
                shape.required_txn_confirms
            );
        }
        let data_confirms = required_u8(&parameters, "requiredDataConfirms").ok_or_else(|| {
            anyhow!(
                "{role} wallet {display} getParameters has invalid or missing requiredDataConfirms"
            )
        })?;
        if data_confirms != shape.required_data_confirms {
            bail!(
                "{role} wallet {display} requires {data_confirms} data confirmations; the agreed {role} shape is exactly {}",
                shape.required_data_confirms
            );
        }
        Ok(())
    }

    fn required_string<'a>(
        value: &'a Value,
        field: &str,
        role: &str,
        address: &str,
        getter: &str,
    ) -> Result<&'a str> {
        value
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                anyhow!("{role} wallet {address} {getter} has invalid or missing {field}")
            })
    }

    fn normalize_code_hash(value: &str) -> Result<String> {
        let value = value
            .trim()
            .strip_prefix("0x")
            .or_else(|| value.trim().strip_prefix("0X"))
            .unwrap_or_else(|| value.trim());
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("expected exactly 32-byte hex");
        }
        Ok(value.to_ascii_lowercase())
    }

    fn normalize_custodian(value: &str) -> Option<String> {
        let value = value
            .trim()
            .strip_prefix("0x")
            .or_else(|| value.trim().strip_prefix("0X"))
            .unwrap_or_else(|| value.trim());
        if value.is_empty()
            || value.len() > 64
            || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        Some(format!("{value:0>64}").to_ascii_lowercase())
    }

    /// Every custodian the getter returned, or an error naming the one that could not be read.

    /// Deliberately not a `filter_map`: silently DROPPING an entry whose `owner_pubkey` is missing
    /// or malformed would let a four-custodian wallet count as three, which is exactly the shape
    /// this check exists to refuse. An authoritative set that cannot be read in full is not an
    /// authoritative set.
    fn custodian_pubkeys(role: &str, address: &str, output: &Value) -> Result<Vec<String>> {
        let custodians = output
            .get("custodians")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                anyhow!(
                    "{role} wallet {address} getCustodians has no authoritative custodians array"
                )
            })?;
        custodians
            .iter()
            .map(|custodian| {
                custodian
                    .get("owner_pubkey")
                    .and_then(Value::as_str)
                    .and_then(normalize_custodian)
                    .ok_or_else(|| {
                        anyhow!(
                            "{role} wallet {address} getCustodians has a custodian with an invalid or missing owner_pubkey"
                        )
                    })
            })
            .collect()
    }

    /// The agreed custodian set, enforced exactly: the count, no repeated key, and the local key.

    /// The count is the half that was missing. Membership alone accepts a wallet that also carries
    /// a custodian the operator never agreed to -- on the Hot, where one signature executes, such a
    /// custodian can drain it alone.
    fn ensure_custodian_shape(
        role: &str,
        address: &str,
        output: &Value,
        expected: &str,
        expected_custodians: usize,
    ) -> Result<()> {
        let custodians = custodian_pubkeys(role, address, output)?;
        if custodians.len() != expected_custodians {
            bail!(
                "{role} wallet {address} has {} pubkey custodians; the agreed {role} shape is exactly {expected_custodians}",
                custodians.len()
            );
        }
        let mut distinct = custodians.clone();
        distinct.sort();
        distinct.dedup();
        if distinct.len() != custodians.len() {
            bail!(
                "{role} wallet {address} lists the same custodian public key more than once; the agreed {role} shape is {expected_custodians} distinct custodians"
            );
        }
        let expected = normalize_custodian(expected)
            .ok_or_else(|| anyhow!("local {role} public key is invalid"))?;
        if !custodians.contains(&expected) {
            bail!(
                "local {role} public key is not in wallet {address}'s authoritative custodian set"
            );
        }
        Ok(())
    }

    fn required_u8(value: &Value, field: &str) -> Option<u8> {
        value
            .get(field)
            .and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
            })
            .and_then(|value| u8::try_from(value).ok())
    }

    #[cfg(test)]
    mod tests {
        use std::cell::Cell;
        use std::collections::HashMap;

        use dexdo_wallet_onboarding::{parse_scoped_address, SessionPhase};

        use super::*;

        #[derive(Clone)]
        struct FixtureWallet {
            account: Option<WalletAccountFact>,
            version: Option<Value>,
            custodians: Option<Value>,
            parameters: Option<Value>,
        }

        struct FakeChain {
            wallets: HashMap<String, FixtureWallet>,
            writes: Cell<usize>,
        }

        #[async_trait(?Send)]
        impl WalletChainReader for FakeChain {
            async fn account(&self, address: &Address) -> Result<Option<WalletAccountFact>> {
                Ok(self
                    .wallets
                    .get(&address.with_workchain())
                    .and_then(|wallet| wallet.account.clone()))
            }

            async fn getter(
                &self,
                address: &Address,
                method: &'static str,
            ) -> Result<Option<Value>> {
                let wallet = self
                    .wallets
                    .get(&address.with_workchain())
                    .ok_or_else(|| anyhow!("missing fixture wallet"))?;
                Ok(match method {
                    "getVersion" => wallet.version.clone(),
                    "getCustodians" => wallet.custodians.clone(),
                    "getParameters" => wallet.parameters.clone(),
                    other => bail!("unexpected getter {other}"),
                })
            }
        }

        fn public(byte: char) -> String {
            byte.to_string().repeat(64)
        }

        fn response() -> AgentWalletsResponse {
            AgentWalletsResponse {
                version: 1,
                network: "net-a".to_string(),
                vault: parse_scoped_address(&format!("{0}::{0}", public('c'))).unwrap(),
                hot: parse_scoped_address(&format!("{0}::{0}", public('d'))).unwrap(),
            }
        }

        fn wallet(custodian: &str, threshold: u8) -> FixtureWallet {
            FixtureWallet {
                account: Some(WalletAccountFact {
                    status: "Active".to_string(),
                    code_hash: Some(dexdo_core::canonical_multisig::CODE_HASH.to_string()),
                }),
                version: Some(serde_json::json!({
                    "value0": dexdo_core::canonical_multisig::VERSION,
                    "value1": dexdo_core::canonical_multisig::CONTRACT_NAME,
                })),
                custodians: Some(serde_json::json!({
                    "custodians": [
                        {"index": 0, "owner_pubkey": format!("0x{}", public('a'))},
                        {"index": 1, "owner_pubkey": format!("0x{}", public('b'))},
                        {"index": 2, "owner_pubkey": format!("0x{custodian}")},
                    ],
                })),
                parameters: Some(serde_json::json!({
                    "requiredTxnConfirms": threshold,
                    "requiredDataConfirms": 2,
                })),
            }
        }

        fn valid_chain() -> FakeChain {
            let response = response();
            FakeChain {
                wallets: HashMap::from([
                    (
                        response.vault.account_address.clone(),
                        wallet(&public('2'), 2),
                    ),
                    (
                        response.hot.account_address.clone(),
                        wallet(&public('1'), 1),
                    ),
                ]),
                writes: Cell::new(0),
            }
        }

        #[tokio::test]
        async fn valid_pair_produces_only_the_existing_note_deploy_handoff() {
            let response = response();
            let chain = valid_chain();
            let validated = validate_wallet_pair(&chain, &response, &public('1'), &public('2'))
                .await
                .unwrap();
            assert_eq!(validated.network, "net-a");
            assert_eq!(validated.hot_scoped_address, response.hot.canonical);
            assert_eq!(validated.vault_scoped_address, response.vault.canonical);
            let command = note_deploy_handoff_command(&validated, Path::new("hot.key"));
            assert!(
                command.contains(&format!("--multisig-address {}", response.hot.canonical)),
                "{command}"
            );
            assert!(
                !command.contains(&format!(
                    "--multisig-address {}",
                    response.hot.account_address
                )),
                "{command}"
            );
            assert_eq!(chain.writes.get(), 0);
        }

        /// The endpoint handed to the write path is absolute, whatever shape it arrived in.

        /// This used to pass a network LABEL and expect that network's host back, because the
        /// function mapped one to the other from a compiled-in table. The table is gone --
        /// the endpoint comes from the manifest -- so what is left to hold, and the only thing this
        /// seam ever really did, is that a host without a scheme is not handed on as a relative URL.
        #[test]
        fn the_chain_validation_endpoint_is_absolute() {
            assert_eq!(
                wallet_validation_endpoint("net-a.example").unwrap(),
                "https://net-a.example"
            );
            assert_eq!(
                wallet_validation_endpoint("  https://net-a.example  ").unwrap(),
                "https://net-a.example"
            );
        }

        #[tokio::test]
        async fn every_invalid_chain_fact_fails_before_any_mutating_handoff() {
            #[derive(Clone, Copy)]
            enum Mutation {
                Missing,
                Inactive,
                WrongHash,
                WrongVersion,
                WrongName,
                MissingCustodian,
            }

            let response = response();
            for (role, address) in [
                ("Hot", &response.hot.account_address),
                ("Vault", &response.vault.account_address),
            ] {
                for mutation in [
                    Mutation::Missing,
                    Mutation::Inactive,
                    Mutation::WrongHash,
                    Mutation::WrongVersion,
                    Mutation::WrongName,
                    Mutation::MissingCustodian,
                ] {
                    let mut chain = valid_chain();
                    let wallet = chain.wallets.get_mut(address).unwrap();
                    match mutation {
                        Mutation::Missing => wallet.account = None,
                        Mutation::Inactive => {
                            wallet.account.as_mut().unwrap().status = "Frozen".to_string();
                        }
                        Mutation::WrongHash => {
                            wallet.account.as_mut().unwrap().code_hash = Some(
                                dexdo_core::canonical_multisig::LEGACY_SPENDING_CODE_HASH
                                    .to_string(),
                            );
                        }
                        Mutation::WrongVersion => {
                            wallet.version.as_mut().unwrap()["value0"] = serde_json::json!("2.2.0");
                        }
                        Mutation::WrongName => {
                            wallet.version.as_mut().unwrap()["value1"] =
                                serde_json::json!("Multisig");
                        }
                        Mutation::MissingCustodian => {
                            wallet.custodians = Some(serde_json::json!({
                                "custodians": [{"owner_pubkey": format!("0x{}", public('9'))}],
                            }));
                        }
                    }
                    assert!(
                        validate_wallet_pair(&chain, &response, &public('1'), &public('2'))
                            .await
                            .is_err(),
                        "{role}"
                    );
                    assert_eq!(chain.writes.get(), 0, "{role}");
                }
            }

            let mut chain = valid_chain();
            chain
                .wallets
                .get_mut(&response.hot.account_address)
                .unwrap()
                .parameters = Some(serde_json::json!({"requiredTxnConfirms": 2}));
            assert!(
                validate_wallet_pair(&chain, &response, &public('1'), &public('2'))
                    .await
                    .is_err()
            );
            assert_eq!(chain.writes.get(), 0);
        }

        #[tokio::test]
        async fn invalid_vault_facts_and_same_address_fail_before_hot_handoff() {
            let response = response();
            let mut chain = valid_chain();
            chain
                .wallets
                .get_mut(&response.vault.account_address)
                .unwrap()
                .custodians = Some(serde_json::json!({
                "custodians": [{"owner_pubkey": format!("0x{}", public('9'))}],
            }));
            assert!(
                validate_wallet_pair(&chain, &response, &public('1'), &public('2'))
                    .await
                    .is_err()
            );
            assert_eq!(chain.writes.get(), 0);

            let mut same = response;
            same.vault = same.hot.clone();
            assert!(
                validate_wallet_pair(&valid_chain(), &same, &public('1'), &public('1'))
                    .await
                    .is_err()
            );
        }

        /// What a PROGRAM reads: the link on the first line, and the code after it.

        /// A caller takes the invitation from there, so the position holds for every destination
        /// that is not a human -- a pipe, a redirection, this capture. is not a licence to
        /// move it.
        #[test]
        fn a_captured_invitation_leads_with_the_link_and_still_draws_the_code() {
            let dir = tempfile::tempdir().unwrap();
            let absent = dir.path().join("invite.svg");
            let deep_link = "bee-connect://invite/fixture";
            let mut output = Vec::new();

            print_invitation(deep_link, None, false, false, &mut output).unwrap();

            let output = String::from_utf8(output).unwrap();
            assert_eq!(
                output.lines().next(),
                Some(deep_link),
                "the link must be the first line: {output}"
            );
            let qr_line = output.find('\u{2588}').or_else(|| output.find('\u{2584}'));
            assert!(qr_line.is_some(), "no QR block glyphs in output: {output}");
            assert!(!absent.exists());
        }

        /// What a HUMAN reads: the code, and not one character of the link.

        /// The link opens a phone wallet, so on a workstation console there is nothing to do with
        /// it, and at some 670 characters it is the largest thing on the screen. Printing it there
        /// buys the operator nothing and costs them the code's legibility.
        #[test]
        fn an_interactive_invitation_shows_the_code_and_never_the_link() {
            let deep_link = "bee-connect://invite/fixture";
            let mut output = Vec::new();

            print_invitation(deep_link, None, false, true, &mut output).unwrap();

            let output = String::from_utf8(output).unwrap();
            assert!(
                !output.contains(deep_link),
                "the link must not reach a human: {output}"
            );
            assert!(
                output.contains('\u{2588}') || output.contains('\u{2584}'),
                "no QR block glyphs in output: {output}"
            );
        }

        /// The invitation must not depend on a terminal that can show an image: the same code
        /// goes out whether or not the caller asked for the old opt-in flag.
        #[test]
        fn invitation_qr_does_not_depend_on_the_terminal_qr_flag() {
            let deep_link = "bee-connect://invite/fixture";
            let mut with_flag = Vec::new();
            let mut without_flag = Vec::new();

            print_invitation(deep_link, None, true, false, &mut with_flag).unwrap();
            print_invitation(deep_link, None, false, false, &mut without_flag).unwrap();

            assert_eq!(with_flag, without_flag);
        }

        /// The drawn code keeps the shape `qr_compact` promises: one module per column, two per
        /// row, every line the same width.
        #[test]
        fn invitation_qr_keeps_the_module_square() {
            let deep_link = "bee-connect://invite/fixture";
            let mut output = Vec::new();

            print_invitation(deep_link, None, false, true, &mut output).unwrap();

            let code = crate::cli::qr_compact::smallest_code(deep_link.as_bytes()).unwrap();
            let quiet_zone = 2;
            let columns = code.width() + 2 * quiet_zone;
            let rows = columns.div_ceil(2);
            let output = String::from_utf8(output).unwrap();
            let drawn: Vec<&str> = output
                .lines()
                .filter(|line| {
                    line.chars()
                        .all(|c| c == ' ' || ('\u{2580}'..='\u{259F}').contains(&c))
                })
                .collect();
            assert_eq!(drawn.len(), rows, "{output}");
            for line in drawn {
                assert_eq!(line.chars().count(), columns, "{line:?}");
            }
        }

        #[test]
        fn invitation_qr_file_is_opt_in_svg_and_its_path_is_printed() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("invite.svg");
            let mut output = Vec::new();

            print_invitation(
                "bee-connect://invite/fixture",
                Some(&path),
                false,
                false,
                &mut output,
            )
            .unwrap();

            let svg = std::fs::read_to_string(&path).unwrap();
            assert!(svg.starts_with("<?xml"), "{svg}");
            let output = String::from_utf8(output).unwrap();
            assert!(
                output.contains(&format!("QR code saved to {}", path.display())),
                "{output}"
            );
        }

        #[test]
        #[cfg(unix)]
        fn fresh_local_state_is_owner_only_and_contains_no_agent_secrets() {
            use std::os::unix::fs::PermissionsExt;

            let dir = tempfile::tempdir().unwrap();
            let args = WalletOnboardArgs {
                // The manifest a run finds for itself; these fixtures never dial.
                agent_name: "fixture-agent".to_string(),
                state: Some(dir.path().join("session.json")),
                hot_key: Some(dir.path().join("hot.key")),
                vault_key: Some(dir.path().join("vault.key")),
                qr_file: None,
                terminal_qr: false,
            };
            let state = resolve_private_file_path(args.state.as_deref().unwrap(), "state").unwrap();
            let (session, keys, created) = load_or_create_session(
                &args,
                crate::cli::wallet::test_network_a().as_str(),
                &state,
                session_limits(WalletOnboardingParams::canonical()),
            )
            .unwrap();
            assert!(created);
            assert!(matches!(
                session.phase,
                SessionPhase::AwaitingWalletHello { .. }
            ));
            let state_bytes = std::fs::read(args.state.as_deref().unwrap()).unwrap();
            let hot_secret = std::fs::read_to_string(args.hot_key.as_deref().unwrap()).unwrap();
            let vault_secret = std::fs::read_to_string(args.vault_key.as_ref().unwrap()).unwrap();
            assert!(!String::from_utf8_lossy(&state_bytes).contains(&hot_secret));
            assert!(!String::from_utf8_lossy(&state_bytes).contains(&vault_secret));
            assert_ne!(hot_secret, vault_secret);
            assert_eq!(keys.hot.public_hex(), session.hot_pubkey);
            assert_eq!(
                keys.vault.as_ref().unwrap().public_hex(),
                session.vault_pubkey.as_deref().unwrap()
            );
            for path in [
                args.state.as_ref().unwrap(),
                args.hot_key.as_ref().unwrap(),
                args.vault_key.as_ref().unwrap(),
            ] {
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            let link = session.deep_link().unwrap();
            assert!(!link.contains(keys.hot.public_hex()));
            assert!(!link.contains(keys.vault.as_ref().unwrap().public_hex()));
            // The onboarding intent is a public routing hint, not a secret: the wallet needs it to

            assert!(link.ends_with("&intent=agent_onboard"), "{link}");
            assert_eq!(link.matches("intent=agent_onboard").count(), 1, "{link}");
        }

        #[test]
        fn resume_rejects_changed_inputs_or_local_key() {
            let dir = tempfile::tempdir().unwrap();
            let args = WalletOnboardArgs {
                agent_name: "fixture-agent".to_string(),
                // The manifest a run finds for itself; these fixtures never dial.
                state: Some(dir.path().join("session.json")),
                hot_key: Some(dir.path().join("hot.key")),
                vault_key: None,
                qr_file: None,
                terminal_qr: false,
            };
            let state = resolve_private_file_path(args.state.as_deref().unwrap(), "state").unwrap();
            load_or_create_session(
                &args,
                crate::cli::wallet::test_network_a().as_str(),
                &state,
                session_limits(WalletOnboardingParams::canonical()),
            )
            .unwrap();

            let changed = WalletOnboardArgs {
                agent_name: "different".to_string(),
                ..args
            };
            assert!(load_or_create_session(
                &changed,
                crate::cli::wallet::test_network_b().as_str(),
                &state,
                session_limits(WalletOnboardingParams::canonical()),
            )
            .is_err());
        }
    }

    include!("wallet_onboarding_endpoint_tests.rs");
    include!("wallet_onboarding_shape_tests.rs");
}

pub(crate) async fn run_wallet_onboard(
    args: WalletOnboardArgs,
    binding_id: &str,
) -> Result<crate::cli::wallet::WalletBinding> {
    live::run(args, binding_id).await
}
