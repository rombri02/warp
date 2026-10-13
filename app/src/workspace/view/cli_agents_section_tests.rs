use super::*;
use crate::terminal::cli_agent_sessions::BlockedSource;

fn display(status: CLIAgentSessionStatus, has_unread: bool) -> CliAgentDisplayStatus {
    CliAgentDisplayStatus::new(&status, true, has_unread)
}

fn blocked() -> CLIAgentSessionStatus {
    CLIAgentSessionStatus::Blocked {
        message: None,
        source: BlockedSource::PermissionRequest,
    }
}

#[test]
fn attention_priority_orders_blocked_then_unseen_done_then_working_then_seen() {
    let ordered = [
        display(blocked(), false),
        display(CLIAgentSessionStatus::Success, true),
        display(CLIAgentSessionStatus::InProgress, false),
        display(CLIAgentSessionStatus::Success, false),
    ]
    .map(CliAgentDisplayStatus::attention_priority);
    assert!(ordered.windows(2).all(|pair| pair[0] > pair[1]));
}

#[test]
fn stopped_session_is_done_until_seen_then_idle() {
    assert_eq!(
        display(CLIAgentSessionStatus::Success, true),
        CliAgentDisplayStatus::Done
    );
    assert_eq!(
        display(CLIAgentSessionStatus::Success, false),
        CliAgentDisplayStatus::Idle
    );
}

#[test]
fn session_with_unknown_status_is_running_not_working() {
    let status = CliAgentDisplayStatus::new(&CLIAgentSessionStatus::InProgress, false, false);
    assert_eq!(status, CliAgentDisplayStatus::Running);
}
