use warp_cli::agent::Harness;

use super::AcpLaunchSpec;
use crate::ai::agent_sdk::driver::AgentDriverError;

#[test]
fn launch_table_covers_every_acp_capable_harness() {
    for (harness, program) in [
        (Harness::Claude, "claude-code-acp"),
        (Harness::Codex, "codex-acp"),
        (Harness::Gemini, "gemini"),
    ] {
        let spec = AcpLaunchSpec::for_harness(harness)
            .unwrap_or_else(|| panic!("{harness} should have an ACP launch spec"));
        assert_eq!(spec.program, program);
    }
    assert_eq!(
        AcpLaunchSpec::for_harness(Harness::Gemini).unwrap().args,
        vec!["--experimental-acp"]
    );
}

#[test]
fn launch_table_has_no_entry_for_harnesses_without_acp() {
    for harness in [Harness::Oz, Harness::OpenCode, Harness::Unknown] {
        assert!(AcpLaunchSpec::for_harness(harness).is_none(), "{harness}");
    }
}

#[test]
fn command_line_override_splits_like_a_shell() {
    let spec = AcpLaunchSpec::from_command_line("npx -y '@zed-industries/codex-acp' --flag 'a b'")
        .unwrap();
    assert_eq!(spec.program, "npx");
    assert_eq!(
        spec.args,
        vec!["-y", "@zed-industries/codex-acp", "--flag", "a b"]
    );
}

#[test]
fn command_line_override_rejects_empty_and_malformed_input() {
    for command_line in ["", "   ", "npx 'unterminated"] {
        let error = AcpLaunchSpec::from_command_line(command_line)
            .expect_err("malformed ACP command lines must be rejected");
        match error {
            AgentDriverError::HarnessSetupFailed { harness, .. } => assert_eq!(harness, "acp"),
            other => panic!("expected HarnessSetupFailed for {command_line:?}, got {other}"),
        }
    }
}
