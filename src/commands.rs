pub struct CommandSpec {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
    pub argument: bool,
    pub idle_only: bool,
}

pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/status",
        usage: "/status",
        description: "Show session and quota details",
        argument: false,
        idle_only: false,
    },
    CommandSpec {
        name: "/model",
        usage: "/model",
        description: "Select model and reasoning effort",
        argument: false,
        idle_only: true,
    },
    CommandSpec {
        name: "/steer",
        usage: "/steer <text>",
        description: "Guide the active turn",
        argument: true,
        idle_only: false,
    },
    CommandSpec {
        name: "/clear",
        usage: "/clear",
        description: "Start a new conversation",
        argument: false,
        idle_only: false,
    },
    CommandSpec {
        name: "/budget",
        usage: "/budget <credits>",
        description: "View or change session budget",
        argument: true,
        idle_only: false,
    },
    CommandSpec {
        name: "/logs",
        usage: "/logs",
        description: "Show recent session log",
        argument: false,
        idle_only: false,
    },
    CommandSpec {
        name: "/profile",
        usage: "/profile",
        description: "Show active profile",
        argument: false,
        idle_only: false,
    },
    CommandSpec {
        name: "/interrupt",
        usage: "/interrupt",
        description: "Interrupt active turn",
        argument: false,
        idle_only: false,
    },
    CommandSpec {
        name: "/kill",
        usage: "/kill",
        description: "Kill Codex process tree",
        argument: false,
        idle_only: false,
    },
    CommandSpec {
        name: "/quit",
        usage: "/quit",
        description: "Exit Codex Guard",
        argument: false,
        idle_only: false,
    },
    CommandSpec {
        name: "/help",
        usage: "/help",
        description: "Show command help",
        argument: false,
        idle_only: false,
    },
];

pub fn find(text: &str) -> Option<&'static CommandSpec> {
    let name = text.split_whitespace().next()?;
    COMMANDS.iter().find(|spec| spec.name == name)
}
pub fn help() -> String {
    let rows = COMMANDS
        .iter()
        .map(|spec| format!("  {:18} {}", spec.usage, spec.description))
        .collect::<Vec<_>>();
    format!(
        "\nCommands\n{}\n\nEnter newline · Ctrl+D/F2 send\n",
        rows.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn help_and_parsing_share_the_command_catalog() {
        for spec in COMMANDS {
            assert!(help().contains(spec.usage));
            assert!(find(spec.usage).is_some());
        }
        assert!(find("/unknown").is_none());
    }
}
