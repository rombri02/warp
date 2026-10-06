use warp_cli::agent::Harness;

use crate::ai::agent_sdk::driver::AgentDriverError;

/// The process to spawn for a harness driven over the Agent Client Protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AcpLaunchSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl AcpLaunchSpec {
    /// The ACP adapter shipped alongside each harness CLI, or `None` for harnesses that have no
    /// ACP transport.
    pub(crate) fn for_harness(harness: Harness) -> Option<Self> {
        let (program, args): (&str, &[&str]) = match harness {
            Harness::Claude => ("claude-code-acp", &[]),
            Harness::Codex => ("codex-acp", &[]),
            Harness::Gemini => ("gemini", &["--experimental-acp"]),
            Harness::Oz | Harness::OpenCode | Harness::Unknown => return None,
        };
        Some(Self {
            program: program.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        })
    }

    /// Parses a shell-style command line such as `npx -y @zed-industries/codex-acp`.
    pub(crate) fn from_command_line(command_line: &str) -> Result<Self, AgentDriverError> {
        let setup_failed = |reason: String| AgentDriverError::HarnessSetupFailed {
            harness: "acp".to_owned(),
            reason,
        };
        let mut parts = shell_words::split(command_line).map_err(|error| {
            setup_failed(format!(
                "Could not parse ACP agent command `{command_line}`: {error}"
            ))
        })?;
        if parts.is_empty() {
            return Err(setup_failed("ACP agent command is empty".to_owned()));
        }
        let program = parts.remove(0);
        Ok(Self {
            program,
            args: parts,
        })
    }
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod tests;
