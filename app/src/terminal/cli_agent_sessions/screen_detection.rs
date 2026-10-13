//! Infers the status of CLI agents that have no plugin reporting it, by matching the text at the
//! bottom of their terminal screen. Rules are adapted from herdr's agent manifests
//! (https://github.com/herdrdev/herdr, Apache-2.0).

use std::time::Duration;

use super::{BlockedSource, CLIAgentSessionStatus};
use crate::terminal::CLIAgent;
use crate::terminal::model::grid::Dimensions;
use crate::terminal::model::grid::RespectDisplayedOutput;
use crate::terminal::model::index::Point;
use crate::terminal::model::secrets::RespectObfuscatedSecrets;
use crate::terminal::model::terminal_model::TerminalModel;

pub const SCREEN_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Enough rows to cover every rule's region (the largest is the recent-screen scan).
const SCREEN_TAIL_ROWS: usize = 40;

pub fn has_screen_rules(agent: CLIAgent) -> bool {
    matches!(agent, CLIAgent::Antigravity)
}

/// Infers `agent`'s status from the bottom of its screen. Returns `None` when there are no rules
/// for `agent`, or when the screen matches none of them (e.g. a dialog that the rules don't know
/// about yet): callers should then keep the previous status rather than guess.
pub fn detect_status(agent: CLIAgent, screen_tail: &str) -> Option<CLIAgentSessionStatus> {
    match agent {
        CLIAgent::Antigravity => detect_antigravity(&screen_tail.to_lowercase()),
        _ => None,
    }
}

/// How many consecutive polls must agree before a detected status is applied. Screens flicker
/// between states (e.g. the spinner disappears for a moment between tool calls), and every applied
/// change can notify the user.
const STABLE_POLLS: u8 = 2;

/// Filters out statuses that don't hold for [`STABLE_POLLS`] consecutive polls.
#[derive(Default)]
pub struct StatusDebouncer {
    candidate: Option<(CLIAgentSessionStatus, u8)>,
}

impl StatusDebouncer {
    /// Records the status detected by one poll, returning it once it has been stable long enough.
    pub fn observe(&mut self, status: CLIAgentSessionStatus) -> Option<CLIAgentSessionStatus> {
        let count = match &mut self.candidate {
            Some((candidate, count)) if *candidate == status => {
                *count = count.saturating_add(1);
                *count
            }
            _ => {
                self.candidate = Some((status.clone(), 1));
                1
            }
        };
        (count >= STABLE_POLLS).then_some(status)
    }
}

/// The last `SCREEN_TAIL_ROWS` rows of whatever the agent is drawing into: the alt screen for
/// fullscreen TUIs, otherwise the running block's output.
pub fn screen_tail(model: &TerminalModel) -> String {
    let grid = if model.is_alt_screen_active() {
        model.alt_screen().grid_handler()
    } else {
        model
            .block_list()
            .active_block()
            .output_grid()
            .grid_handler()
    };
    let (rows, columns) = (grid.total_rows(), grid.columns());
    if rows == 0 || columns == 0 {
        return String::new();
    }
    grid.bounds_to_string(
        Point::new(rows.saturating_sub(SCREEN_TAIL_ROWS), 0),
        Point::new(rows - 1, columns - 1),
        false, /* include_esc_sequences */
        RespectObfuscatedSecrets::No,
        false, /* force_obfuscated_secrets */
        RespectDisplayedOutput::No,
    )
}

fn bottom_non_empty_lines(screen: &str, count: usize) -> Vec<&str> {
    let mut lines: Vec<_> = screen
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(count)
        .collect();
    lines.reverse();
    lines
}

fn contains_all(lines: &[&str], needles: &[&str]) -> bool {
    let text = lines.join("\n");
    needles.iter().all(|needle| text.contains(needle))
}

fn blocked(source: BlockedSource) -> CLIAgentSessionStatus {
    CLIAgentSessionStatus::Blocked {
        message: None,
        source,
    }
}

/// Expects lowercased text.
fn detect_antigravity(screen: &str) -> Option<CLIAgentSessionStatus> {
    // Dialogs replace the prompt box and end with a key-hint row above the footer.
    let footer = bottom_non_empty_lines(screen, 2);
    if contains_all(&footer, &["navigate", "tab amend"]) {
        return Some(blocked(BlockedSource::PermissionRequest));
    }
    if contains_all(&footer, &["navigate", "esc skip"]) {
        return Some(blocked(BlockedSource::QuestionAsked));
    }
    if contains_all(
        &bottom_non_empty_lines(screen, 8),
        &[
            "do you trust the contents of this project?",
            "yes, i trust this folder",
            "enter confirm",
        ],
    ) {
        return Some(blocked(BlockedSource::PermissionRequest));
    }
    // Permission dialog of versions before 1.1.28. Only the bottom of the screen counts, so the
    // same words in the conversation above don't keep the agent "blocked".
    let dialog = bottom_non_empty_lines(screen, 12).join("\n");
    if dialog.contains("requesting permission for:")
        && (dialog.contains("do you want to proceed?")
            || (dialog.contains("tab amend") && dialog.contains("edit command")))
    {
        return Some(blocked(BlockedSource::PermissionRequest));
    }

    // While working, the footer under the prompt box reads "esc to cancel" instead of
    // "? for shortcuts".
    let esc_to_cancel_footer = footer.iter().any(|line| {
        let line = line.trim_start();
        let after_rule = line.trim_start_matches('─');
        line.len() - after_rule.len() >= '─'.len_utf8() * 3
            && after_rule.trim_start().starts_with("esc to cancel")
    });
    let spinner = bottom_non_empty_lines(screen, 12)
        .iter()
        .any(|line| starts_with_braille_spinner(line));
    if esc_to_cancel_footer || spinner {
        return Some(CLIAgentSessionStatus::InProgress);
    }

    // Idle only when the prompt box is visibly waiting for input, so that an unknown screen isn't
    // mistaken for a finished task.
    let prompt_box_visible = bottom_non_empty_lines(screen, 6)
        .iter()
        .any(|line| line.trim_start().starts_with("───"));
    let shortcuts_footer = footer.iter().any(|line| line.contains("? for shortcuts"));
    (prompt_box_visible || shortcuts_footer).then_some(CLIAgentSessionStatus::Success)
}

/// A braille spinner frame followed by a word, e.g. "⠋ Thinking".
fn starts_with_braille_spinner(line: &str) -> bool {
    let line = line.trim_start();
    let after_spinner = line.trim_start_matches(|c| ('\u{2801}'..='\u{28FF}').contains(&c));
    after_spinner.len() < line.len()
        && after_spinner.starts_with(char::is_whitespace)
        && !after_spinner.trim().is_empty()
}

#[cfg(test)]
#[path = "screen_detection_tests.rs"]
mod tests;
