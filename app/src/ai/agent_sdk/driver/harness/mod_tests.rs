use warp_cli::agent::{Harness, HarnessTransport};

use super::{
    AcpLaunchSpec, ClaudeHarness, HARNESS_FAILURE_OUTPUT_TRUNCATION_MARKER, HarnessKind,
    HarnessTransportMode, ThirdPartyHarness, auth_check_command_for, harness_kind,
    prepare_harness_failure_output, renders_natively, validate_cli_installed,
};
use crate::ai::agent_sdk::driver::AgentDriverError;

fn assert_harness_setup_failed(err: &AgentDriverError) -> (&str, &str) {
    match err {
        AgentDriverError::HarnessSetupFailed { harness, reason } => (harness, reason),
        other => panic!("expected HarnessSetupFailed, got: {other}"),
    }
}

#[test]
fn harness_failure_output_uses_harness_truncation_marker() {
    let output = format!("START{}END", "x".repeat(4_096));

    let prepared = prepare_harness_failure_output(&output);
    assert!(prepared.contains(HARNESS_FAILURE_OUTPUT_TRUNCATION_MARKER));
}

#[cfg(not(windows))]
#[test]
fn validate_cli_installed_succeeds_for_known_binary() {
    assert!(validate_cli_installed("ls", None).is_ok());
}

#[test]
fn validate_cli_installed_fails_for_missing_binary() {
    let err = validate_cli_installed("__nonexistent_cli_abc123__", None).unwrap_err();
    let (harness, reason) = assert_harness_setup_failed(&err);
    assert_eq!(harness, "__nonexistent_cli_abc123__");
    assert!(reason.contains("not found"));
    assert!(!reason.contains("Install it first"));
}

#[test]
fn validate_cli_installed_includes_docs_url_in_error() {
    let url = "https://example.com/install";
    let err = validate_cli_installed("__nonexistent_cli_abc123__", Some(url)).unwrap_err();
    let (_, reason) = assert_harness_setup_failed(&err);
    assert!(reason.contains(url));
    assert!(reason.contains("Install it first"));
}

// --- Runtime error pattern tests ---

#[test]
fn claude_runtime_error_patterns_returns_slice() {
    use super::ThirdPartyHarness;
    use super::claude_code::ClaudeHarness;
    // Patterns are initially empty until validated needles are filled in.
    // The trait method must still be callable.
    let _: &[&str] = ClaudeHarness.runtime_error_patterns();
}

#[test]
fn codex_runtime_error_patterns_returns_slice() {
    use super::ThirdPartyHarness;
    use super::codex::CodexHarness;
    let _: &[&str] = CodexHarness.runtime_error_patterns();
}

#[test]
fn gemini_runtime_error_patterns_is_empty_by_default() {
    use super::ThirdPartyHarness;
    use super::gemini::GeminiHarness;
    assert!(GeminiHarness.runtime_error_patterns().is_empty());
}

#[test]
fn auth_check_command_for_gemini_is_none() {
    assert!(auth_check_command_for(Harness::Gemini).is_none());
}

#[test]
fn auth_check_command_for_oz_is_none() {
    assert!(auth_check_command_for(Harness::Oz).is_none());
}

#[test]
fn auth_check_command_for_unsupported_is_none() {
    // OpenCode is mapped to HarnessKind::Unsupported and therefore has no
    // auth check command of its own.
    assert!(auth_check_command_for(Harness::OpenCode).is_none());
}

#[test]
fn auth_check_command_for_unknown_is_none() {
    // Harness::Unknown causes harness_kind to return Err; the helper still
    // returns None instead of panicking.
    assert!(auth_check_command_for(Harness::Unknown).is_none());
}

#[test]
fn pty_transport_keeps_existing_harness_dispatch() {
    for (harness, expected) in [
        (Harness::Oz, Harness::Oz),
        (Harness::Claude, Harness::Claude),
        (Harness::Codex, Harness::Codex),
        (Harness::Gemini, Harness::Gemini),
    ] {
        let kind = harness_kind(harness, &HarnessTransportMode::Pty).unwrap();
        assert_eq!(kind.harness(), expected);
    }
    assert!(matches!(
        harness_kind(Harness::OpenCode, &HarnessTransportMode::Pty).unwrap(),
        HarnessKind::Unsupported(Harness::OpenCode)
    ));
}

#[test]
fn acp_transport_rejects_harnesses_without_a_launch() {
    let err = harness_kind(Harness::Oz, &HarnessTransportMode::Acp { launch: None }).unwrap_err();
    let (harness, reason) = assert_harness_setup_failed(&err);
    assert_eq!(harness, "oz");
    assert!(reason.contains("cannot be driven over ACP"), "{reason}");
}

#[test]
fn acp_transport_keeps_the_harness_identity_and_skips_cli_session_tracking() {
    let kind = harness_kind(Harness::Claude, &HarnessTransportMode::Acp { launch: None }).unwrap();
    assert_eq!(kind.harness(), Harness::Claude);
    let HarnessKind::ThirdParty(harness) = kind else {
        panic!("ACP transport should produce a third-party harness");
    };
    assert!(!harness.drives_cli_agent_session());
    assert_eq!(
        harness.auth_check_command(),
        ClaudeHarness.auth_check_command(),
        "auth preflight is a property of the harness, not the transport"
    );

    let launch = AcpLaunchSpec::from_command_line("npx -y some-acp-agent").unwrap();
    let kind = harness_kind(
        Harness::Oz,
        &HarnessTransportMode::Acp {
            launch: Some(launch),
        },
    )
    .unwrap();
    assert_eq!(kind.harness(), Harness::Oz);
}

#[test]
fn only_oz_and_acp_runs_render_natively() {
    assert!(renders_natively(Harness::Oz, HarnessTransport::Pty));
    assert!(renders_natively(Harness::Oz, HarnessTransport::Acp));
    assert!(renders_natively(Harness::Claude, HarnessTransport::Acp));
    assert!(!renders_natively(Harness::Claude, HarnessTransport::Pty));
    assert!(!renders_natively(Harness::Codex, HarnessTransport::Pty));
}
