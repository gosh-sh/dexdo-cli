/// and: a policy line is a record, and the field that looked like a reading was one.

/// Two contracts, one line. The stream contract is that nothing prose-shaped reaches stdout, which
/// is this command's machine stream: the proof that reads it parses every line, so one prose line
/// does not degrade the report, it replaces it. The field contract is that `state=` is present only
/// where a state was actually read.
mod policy_stream_1497 {
    use std::sync::{Arc, Mutex};

    /// The log, captured the way an operator sees it with `RUST_LOG=info`.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("capture buffer").extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Captured {
            self.clone()
        }
    }

    /// Drive the real cleanup path against the recording chain and return what it recorded.
    fn recorded_cleanup_path() -> String {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            runtime.block_on(async {
                let chain = super::RecordingRecoveryChain::with_deal_state(
                    super::ready_funded_never_opened_state(),
                );
                let token_contract = format!("0:{}", "4".repeat(64));
                let outcome = super::super::policy_cleanup_unopened_after_match_timeout(
                    &chain,
                    &token_contract,
                    crate::cli::policy::NoHandoverAfterMatchAction::WaitThenReclaim,
                )
                .await
                .expect("the cleanup-ready path submits cleanup_unopened");
                assert!(
                    matches!(outcome, super::super::PolicyCleanupOutcome::Cleaned(_)),
                    "the fixture is the cleanup-ready state, so this path must clean up"
                );
            });
        });
        let bytes = captured.0.lock().expect("buffer").clone();
        String::from_utf8(bytes).expect("utf-8")
    }

    /// The one recorded `policy_action` line of that run, as the client wrote it -- without the
    /// subscriber's timestamp and level, which are the log's frame and not part of the line.
    fn recorded_policy_line() -> String {
        let log = recorded_cleanup_path();
        let line = log
            .lines()
            .find(|line| line.contains("policy_action failure_class="))
            .unwrap_or_else(|| panic!("no policy_action line was recorded at RUST_LOG=info:\n{log}"))
            .to_string();
        let start = line
            .find("policy_action failure_class=")
            .expect("the line was found by that marker");
        line[start..].to_string()
    }

    /// The proof's reader, in the shape `live_cli.rs`'s `live10_push_buyer_event` has: every line of
    /// the buyer's machine stream is parsed as JSON, and the first that is not aborts the run with
    /// serde's message standing in for the diagnosis.
    fn report(machine_stream: &[&str], log: &str) -> String {
        for line in machine_stream {
            if let Err(error) = serde_json::from_str::<serde_json::Value>(line) {
                return format!("buyer JSONL: {error}; line={line}");
            }
        }
        log.to_string()
    }

    /// The subject of: the run whose real cause was "matched, funded, the seller never connected"
    /// was reported as a parse error, because the line naming that cause was sitting on the stream the
    /// reader parses.
    #[test]
    fn a_policy_refusal_reaches_the_report_as_its_class_not_as_a_parse_error() {
        let log = recorded_cleanup_path();
        // What production leaves on the machine stream for this path: its events, and nothing else.
        let machine_stream = [r#"{"schema":"dexdo.buyer.event.v3","seq":1,"event":"endpoint_ready"}"#];
        let report = report(&machine_stream, &log);

        assert!(
            report.contains("failure_class=no_handover_after_match"),
            "the report must name the failure class:\n{report}"
        );
        assert!(
            report.contains("result=cleanup_unopened_submitted"),
            "and what the client did about it:\n{report}"
        );
        assert!(
            !report.contains("expected value at line 1 column 1"),
            "the reader's own breakage may never stand in for the diagnosis:\n{report}"
        );
    }

    /// And the reason that substitution used to happen, stated against the real line: it is not JSON,
    /// so on the machine stream it does not add noise -- it ends the read.
    #[test]
    fn the_policy_line_would_end_the_read_if_it_were_on_the_machine_stream() {
        let line = recorded_policy_line();
        assert!(
            serde_json::from_str::<serde_json::Value>(&line).is_err(),
            "the line is prose, so nothing may put it where JSON is parsed: {line}"
        );
        let poisoned = report(&[line.as_str()], "");
        assert!(
            poisoned.contains("expected value at line 1 column 1"),
            "this is exactly the report the proof produced instead of the cause: {poisoned}"
        );
    }

    /// The whole policy surface, not just the line that was caught: nothing between the cleanup helper
    /// and the end of the failover policy writes to stdout.
    #[test]
    fn no_policy_action_line_is_written_to_the_machine_stream() {
        let body = policy_surface_source();
        assert!(
            !body.contains("println!"),
            "the policy surface writes to stdout, which is where the JSON is parsed:\n{body}"
        );
        assert_eq!(
            body.matches("record_policy_action(&format!").count(),
            6,
            "every policy line goes through the one seam that chooses the stream"
        );
    }

    /// the field that was never read is gone. It said `funded/opened` about a deal that was
    /// funded and NEVER opened, while the three fields beside it were substituted, so it read as a
    /// measurement and was a constant.
    #[test]
    fn the_policy_record_carries_no_state_field_it_never_read() {
        let line = recorded_policy_line();
        assert!(
            !line.contains("state="),
            "a field that was never read may not be printed as though it were: {line}"
        );
        for recorded in recorded_line_sources() {
            assert!(
                !recorded.contains("state="),
                "a recorded policy line grew a state field back:\n{recorded}"
            );
        }
    }

    /// And the other half of: where a state IS read, it is still reported, from the reading.
    #[test]
    fn the_state_that_was_read_is_still_reported_from_the_reading() {
        let token_contract = format!("0:{}", "4".repeat(64));
        let summary = super::super::matched_state_summary(
            &token_contract,
            &super::super::MatchedTokenContractStatus::FundedNeverOpened {
                funded_time: Some(1),
                cleanup_after_unix: Some(2),
                cleanup_ready: true,
                remaining_secs: Some(0),
            },
        );
        assert!(
            summary.contains("funded=true") && summary.contains("opened=false"),
            "the reading says what actually happened, and it is the opposite of the old literal: {summary}"
        );

        let body = policy_surface_source();
        assert!(
            body.contains("state={}") && body.contains("matched_state_summary(token_contract, &status)"),
            "the refusals in this surface still carry a state, and it comes from the reading:\n{body}"
        );
    }

    /// The survivor: what the line says about itself is unchanged by either fix, so this test has to
    /// stay green when the old behaviour is put back. A mutant that reds everything proves nothing
    /// about which contract it broke.
    #[test]
    fn the_policy_line_still_names_its_class_action_and_result() {
        let body = policy_surface_source();
        for named in [
            "failure_class=no_handover_after_match",
            "failure_class=dead_gateway",
            "action=next_seller",
            "result=cleanup_unopened_submitted",
            "result=waiting_cleanup_ready",
            "result=handover_opened_after_wait",
            "result=retrying_gateway",
            "result=placing_next_seller",
            "result=next_seller_matched",
        ] {
            assert!(
                body.contains(named),
                "the policy surface stopped naming {named}, which neither fix touches"
            );
        }
    }

    /// The six lines that go through the seam, sliced out of the source one by one.

    /// Only these are's subject. The refusals beside them still carry `state=`, and two of
    /// them fill it from `matched_state_summary` -- a reading. The literals that remain in the
    /// other refusals of this file, and in the seller's policy surface, are measured and reported
    /// rather than changed here: they are a different set of lines than the ones these two issues
    /// name.
    fn recorded_line_sources() -> Vec<String> {
        let body = policy_surface_source();
        let mut out = Vec::new();
        for chunk in body.split("record_policy_action(&format!(").skip(1) {
            let end = chunk.find("));").expect("each seam call is closed");
            out.push(chunk[..end].to_string());
        }
        assert_eq!(out.len(), 6, "the policy surface has six recorded lines");
        out
    }

    /// The policy surface: the cleanup helper through the end of the failover policy, which is where
    /// all six lines live.
    fn policy_surface_source() -> &'static str {
        let source = include_str!("../buyer.rs");
        let start = source
            .find("async fn policy_cleanup_unopened_after_match_timeout")
            .expect("cleanup helper present");
        let end = source[start..]
            .find("fn buyer_monitor_current_facts")
            .map(|offset| start + offset)
            .expect("end marker present");
        &source[start..end]
    }

}
