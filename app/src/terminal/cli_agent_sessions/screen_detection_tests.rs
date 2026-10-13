use super::*;

fn agy(screen: &str) -> CLIAgentSessionStatus {
    detect_status(CLIAgent::Antigravity, screen).expect("screen should match a rule")
}

#[test]
fn antigravity_idle_prompt() {
    let screen = "\
Ciao! Tutto bene, grazie.

────────────────────────────
>
────────────────────────────
? for shortcuts            Gemini 3.1 Pro · high
";
    assert_eq!(agy(screen), CLIAgentSessionStatus::Success);
}

#[test]
fn antigravity_working_footer() {
    let screen = "\
> ciao come va
────────────────────────────
>
──────────────────────────── esc to cancel
";
    assert_eq!(agy(screen), CLIAgentSessionStatus::InProgress);
}

#[test]
fn antigravity_working_spinner() {
    let screen = "\
⠋ Thinking about your question
────────────────────────────
>
";
    assert_eq!(agy(screen), CLIAgentSessionStatus::InProgress);
}

#[test]
fn antigravity_question_dialog_is_blocked() {
    let screen = "\
? Qual è il tuo linguaggio di programmazione preferito?
  > Rust
    Go
↑/↓ Navigate · enter Select · esc Skip
esc to cancel
";
    assert!(matches!(
        agy(screen),
        CLIAgentSessionStatus::Blocked {
            source: BlockedSource::QuestionAsked,
            ..
        }
    ));
}

#[test]
fn antigravity_permission_dialog_is_blocked() {
    let screen = "\
Run command: cargo test
↑/↓ Navigate · tab Amend · ctrl+g edit/expand command
esc to cancel
";
    assert!(matches!(
        agy(screen),
        CLIAgentSessionStatus::Blocked {
            source: BlockedSource::PermissionRequest,
            ..
        }
    ));
}

#[test]
fn agents_without_rules_are_not_detected() {
    assert!(!has_screen_rules(CLIAgent::Claude));
    assert!(has_screen_rules(CLIAgent::Antigravity));
    assert_eq!(
        detect_status(CLIAgent::Claude, "> \n? for shortcuts\n"),
        None
    );
}

#[test]
fn antigravity_unknown_screen_is_not_guessed() {
    let screen = "\
Some full-screen view the rules don't know about
press any key to continue
";
    assert_eq!(detect_status(CLIAgent::Antigravity, screen), None);
    assert_eq!(detect_status(CLIAgent::Antigravity, ""), None);
}

#[test]
fn antigravity_old_permission_text_in_scrollback_is_not_blocked() {
    let mut screen =
        String::from("Requesting permission for: rm -rf target\nDo you want to proceed?\n");
    for i in 0..20 {
        screen.push_str(&format!("output line {i}\n"));
    }
    screen.push_str(
        "────────────────────────────\n>\n────────────────────────────\n? for shortcuts\n",
    );
    assert_eq!(agy(&screen), CLIAgentSessionStatus::Success);
}

#[test]
fn debouncer_applies_status_only_once_stable() {
    let mut debouncer = StatusDebouncer::default();
    assert_eq!(debouncer.observe(CLIAgentSessionStatus::InProgress), None);
    assert_eq!(
        debouncer.observe(CLIAgentSessionStatus::InProgress),
        Some(CLIAgentSessionStatus::InProgress)
    );
    // A one-poll flicker never gets through.
    assert_eq!(debouncer.observe(CLIAgentSessionStatus::Success), None);
    assert_eq!(debouncer.observe(CLIAgentSessionStatus::InProgress), None);
    assert_eq!(
        debouncer.observe(CLIAgentSessionStatus::InProgress),
        Some(CLIAgentSessionStatus::InProgress)
    );
}
