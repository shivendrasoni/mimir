use crate::{model::ThinkingLevel, tools::AgentMode};

use super::app::ThemeName;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpCommand {
    List,
    Login { server: String },
    Logout { server: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Login {
        provider: Option<String>,
    },
    Logout {
        provider: Option<String>,
    },
    Model {
        model: Option<String>,
    },
    Mode {
        mode: Option<AgentMode>,
    },
    Session {
        session: Option<String>,
    },
    Sessions {
        session: Option<String>,
    },
    Effort {
        level: Option<ThinkingLevel>,
    },
    Fast,
    ScopedModels,
    Resume {
        session: Option<String>,
    },
    New {
        name: Option<String>,
        prompt: Option<String>,
    },
    Name {
        name: Option<String>,
    },
    Tree,
    Fork,
    Clone,
    Compact {
        instructions: Option<String>,
    },
    Refine {
        arguments: Option<String>,
    },
    Learn {
        arguments: Option<String>,
    },
    Copy,
    SideQuestion {
        question: String,
    },
    Export {
        path: Option<String>,
    },
    Import {
        path: String,
    },
    Share,
    Hotkeys,
    Changelog,
    SystemPrompt,
    Logs,
    Update {
        arguments: Option<String>,
    },
    RlmMaxDepth {
        arguments: Option<String>,
    },
    Fullscreen {
        enabled: Option<bool>,
    },
    Traces {
        arguments: Option<String>,
    },
    Heartbeat {
        arguments: Option<String>,
    },
    Heartbeats,
    Goal {
        arguments: Option<String>,
    },
    Autonomous {
        arguments: Option<String>,
    },
    Reload,
    Context,
    Mcp {
        command: McpCommand,
    },
    Settings,
    Theme {
        theme: Option<ThemeName>,
    },
    Help,
    Quit,
    Invalid {
        message: String,
    },
}

#[must_use]
pub fn parse_slash_command(input: &str) -> Option<SlashCommand> {
    let trimmed = input.trim();
    let rest = trimmed.strip_prefix('/')?;
    let (command, arguments) = split_command(rest);
    let argument = (!arguments.is_empty()).then(|| arguments.to_owned());
    match command {
        "login" => Some(SlashCommand::Login { provider: argument }),
        "logout" => Some(SlashCommand::Logout { provider: argument }),
        "model" => Some(SlashCommand::Model { model: argument }),
        "mode" => Some(parse_agent_mode(arguments)),
        "session" => Some(if arguments.is_empty() {
            SlashCommand::Session { session: None }
        } else {
            SlashCommand::Invalid {
                message: "Usage: /session (use /sessions [id] to switch)".into(),
            }
        }),
        "sessions" => Some(SlashCommand::Sessions { session: argument }),
        "effort" | "thinking" => Some(parse_effort(arguments)),
        "fast" => Some(no_argument(command, arguments, SlashCommand::Fast)),
        "scoped-models" => Some(no_argument(command, arguments, SlashCommand::ScopedModels)),
        "resume" => Some(SlashCommand::Resume { session: argument }),
        "new" | "clear" => Some(parse_new(arguments)),
        "name" | "rename" => Some(SlashCommand::Name { name: argument }),
        "tree" => Some(no_argument(command, arguments, SlashCommand::Tree)),
        "fork" => Some(no_argument(command, arguments, SlashCommand::Fork)),
        "clone" => Some(no_argument(command, arguments, SlashCommand::Clone)),
        "compact" => Some(SlashCommand::Compact {
            instructions: argument,
        }),
        "refine" => Some(SlashCommand::Refine {
            arguments: argument,
        }),
        "learn" | "learning" => Some(SlashCommand::Learn {
            arguments: argument,
        }),
        "copy" => Some(no_argument(command, arguments, SlashCommand::Copy)),
        "btw" | "side" => Some(if arguments.is_empty() {
            SlashCommand::Invalid {
                message: "Usage: /btw <question>".into(),
            }
        } else {
            SlashCommand::SideQuestion {
                question: arguments.into(),
            }
        }),
        "export" => Some(SlashCommand::Export { path: argument }),
        "import" => Some(parse_import(arguments)),
        "share" => Some(no_argument(command, arguments, SlashCommand::Share)),
        "hotkeys" => Some(no_argument(command, arguments, SlashCommand::Hotkeys)),
        "changelog" => Some(no_argument(command, arguments, SlashCommand::Changelog)),
        "system-prompt" => Some(no_argument(command, arguments, SlashCommand::SystemPrompt)),
        "logs" => Some(no_argument(command, arguments, SlashCommand::Logs)),
        "update" => Some(SlashCommand::Update {
            arguments: argument,
        }),
        "rlm-max-depth" => Some(SlashCommand::RlmMaxDepth {
            arguments: argument,
        }),
        "fullscreen" => Some(parse_fullscreen(arguments)),
        "trace" | "traces" => Some(SlashCommand::Traces {
            arguments: argument,
        }),
        "heartbeat" => Some(SlashCommand::Heartbeat {
            arguments: argument,
        }),
        "heartbeats" => Some(no_argument(command, arguments, SlashCommand::Heartbeats)),
        "goal" => Some(SlashCommand::Goal {
            arguments: argument,
        }),
        "autonomous" => Some(SlashCommand::Autonomous {
            arguments: argument,
        }),
        "reload" => Some(no_argument(command, arguments, SlashCommand::Reload)),
        "usage" | "context" => Some(no_argument(command, arguments, SlashCommand::Context)),
        "mcp" => Some(parse_mcp(arguments)),
        "settings" => Some(no_argument(command, arguments, SlashCommand::Settings)),
        "theme" => Some(SlashCommand::Theme {
            theme: argument.as_deref().and_then(ThemeName::parse),
        }),
        "help" => Some(no_argument(command, arguments, SlashCommand::Help)),
        "quit" => Some(no_argument(command, arguments, SlashCommand::Quit)),
        _ => None,
    }
}

#[must_use]
pub(super) fn builtin_command_usage(command: &str) -> Option<&'static str> {
    match command.to_ascii_lowercase().as_str() {
        "autonomous" => Some("/autonomous [on|off|status|cancel]"),
        "btw" | "side" => Some("/btw <question>"),
        "clear" | "new" => Some("/new [--name <name>] [-- <prompt>]"),
        "compact" => Some("/compact [instructions]"),
        "effort" | "thinking" => Some("/effort [off|minimal|low|medium|high|xhigh|max]"),
        "export" => Some("/export [path]"),
        "fullscreen" => Some("/fullscreen [on|off]"),
        "goal" => Some("/goal [--budget <tokens>] <objective>"),
        "heartbeat" => Some("/heartbeat [--every <interval>] [--steer|--follow-up] <instruction>"),
        "import" => Some("/import <path.jsonl>"),
        "login" => Some("/login [provider]"),
        "learn" | "learning" => Some(
            "/learn [status|candidates|propose|feedback yes|no|rollback <id>|mode off|observe|auto|contribution enable|disable|check|update|submit <id>|pin [version]]",
        ),
        "logout" => Some("/logout [provider]"),
        "mcp" => Some("/mcp [list|login <name>|logout <name>]"),
        "model" => Some("/model [provider/model]"),
        "mode" => Some("/mode [default|auto]"),
        "name" | "rename" => Some("/name [name]"),
        "refine" => {
            Some("/refine [--scope session|project|user] [instructions|rollback <refinement-id>]")
        }
        "resume" => Some("/resume [session]"),
        "rlm-max-depth" => Some("/rlm-max-depth [<non-negative integer> [--global]]"),
        "sessions" => Some("/sessions [id]"),
        "theme" => Some("/theme [system|dark|light|name]"),
        "trace" | "traces" => Some("/traces [preview|status|upload]"),
        "update" => Some("/update [status|check]"),
        _ => None,
    }
}

fn parse_agent_mode(arguments: &str) -> SlashCommand {
    if arguments.is_empty() {
        return SlashCommand::Mode { mode: None };
    }
    AgentMode::parse(arguments).map_or_else(
        || SlashCommand::Invalid {
            message: "Usage: /mode [default|auto]".into(),
        },
        |mode| SlashCommand::Mode { mode: Some(mode) },
    )
}

fn parse_import(value: &str) -> SlashCommand {
    let value = value.trim();
    let path = if let Some(quote) = value.chars().next().filter(|ch| matches!(ch, '\'' | '"')) {
        if value.len() < 2 || !value.ends_with(quote) {
            return SlashCommand::Invalid {
                message: "Usage: /import <path.jsonl>".into(),
            };
        }
        &value[quote.len_utf8()..value.len() - quote.len_utf8()]
    } else if value.split_whitespace().count() == 1 {
        value
    } else {
        return SlashCommand::Invalid {
            message: "Usage: /import <path.jsonl> (quote paths containing spaces)".into(),
        };
    };
    if path.is_empty() {
        SlashCommand::Invalid {
            message: "Usage: /import <path.jsonl>".into(),
        }
    } else {
        SlashCommand::Import { path: path.into() }
    }
}

fn parse_fullscreen(arguments: &str) -> SlashCommand {
    match arguments.to_ascii_lowercase().as_str() {
        "" => SlashCommand::Fullscreen { enabled: None },
        "on" => SlashCommand::Fullscreen {
            enabled: Some(true),
        },
        "off" => SlashCommand::Fullscreen {
            enabled: Some(false),
        },
        _ => SlashCommand::Invalid {
            message: "Usage: /fullscreen [on|off]".into(),
        },
    }
}

fn split_command(rest: &str) -> (&str, &str) {
    let split = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let command = &rest[..split];
    let arguments = rest[split..].trim();
    (command, arguments)
}

fn no_argument(command: &str, arguments: &str, valid: SlashCommand) -> SlashCommand {
    if arguments.is_empty() {
        valid
    } else {
        SlashCommand::Invalid {
            message: format!("Usage: /{command}"),
        }
    }
}

fn parse_effort(arguments: &str) -> SlashCommand {
    if arguments.is_empty() {
        return SlashCommand::Effort { level: None };
    }
    let level = match arguments.to_ascii_lowercase().as_str() {
        "off" => ThinkingLevel::Off,
        "minimal" => ThinkingLevel::Minimal,
        "low" => ThinkingLevel::Low,
        "medium" => ThinkingLevel::Medium,
        "high" => ThinkingLevel::High,
        "xhigh" => ThinkingLevel::Xhigh,
        "max" => ThinkingLevel::Max,
        _ => {
            return SlashCommand::Invalid {
                message: "Usage: /effort [off|minimal|low|medium|high|xhigh|max]".into(),
            };
        }
    };
    SlashCommand::Effort { level: Some(level) }
}

fn parse_mcp(arguments: &str) -> SlashCommand {
    let mut parts = arguments.split_whitespace();
    match (parts.next(), parts.next(), parts.next()) {
        (None | Some("list"), None, None) => SlashCommand::Mcp {
            command: McpCommand::List,
        },
        (Some("login"), Some(server), None) => SlashCommand::Mcp {
            command: McpCommand::Login {
                server: server.into(),
            },
        },
        (Some("logout"), Some(server), None) => SlashCommand::Mcp {
            command: McpCommand::Logout {
                server: server.into(),
            },
        },
        _ => SlashCommand::Invalid {
            message: "Usage: /mcp [list|login <name>|logout <name>]".into(),
        },
    }
}

fn parse_new(arguments: &str) -> SlashCommand {
    match parse_new_options(arguments) {
        Ok((name, prompt)) => SlashCommand::New { name, prompt },
        Err(message) => SlashCommand::Invalid { message },
    }
}

fn parse_new_options(arguments: &str) -> Result<(Option<String>, Option<String>), String> {
    let arguments = arguments.trim();
    if arguments.is_empty() {
        return Ok((None, None));
    }
    if !arguments.starts_with('-') {
        return Ok((None, Some(arguments.into())));
    }
    if arguments == "--" {
        return required_new_prompt("").map(|prompt| (None, Some(prompt)));
    }
    if let Some(prompt) = arguments
        .strip_prefix("--")
        .filter(|prompt| prompt.starts_with(char::is_whitespace))
    {
        return required_new_prompt(prompt).map(|prompt| (None, Some(prompt)));
    }
    let Some(rest) = arguments.strip_prefix("--name") else {
        let option = arguments.split_whitespace().next().unwrap_or(arguments);
        return Err(format!("Unknown /new option: {option}"));
    };
    if !rest.starts_with(char::is_whitespace) {
        return Err(format!(
            "Unknown /new option: {}",
            arguments.split_whitespace().next().unwrap_or(arguments)
        ));
    }
    let (name, rest) = parse_new_name(rest.trim_start())?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return Err("Expected \"--\" before the /new prompt".into());
    }
    let rest = rest.trim_start();
    if rest.is_empty() {
        return Ok((Some(name), None));
    }
    if rest == "--" {
        return required_new_prompt("").map(|prompt| (Some(name), Some(prompt)));
    }
    let Some(prompt) = rest
        .strip_prefix("--")
        .filter(|prompt| prompt.starts_with(char::is_whitespace))
    else {
        return Err("Expected \"--\" before the /new prompt".into());
    };
    required_new_prompt(prompt).map(|prompt| (Some(name), Some(prompt)))
}

fn parse_new_name(value: &str) -> Result<(String, &str), String> {
    if value.is_empty() || value.starts_with('-') {
        return Err("Missing value for /new option \"--name\"".into());
    }
    let (name, rest) = if let Some(quote @ ('\'' | '"')) = value.chars().next() {
        let quoted = &value[quote.len_utf8()..];
        let Some(end) = quoted.find(quote) else {
            return Err("Unterminated quote in /new --name".into());
        };
        (quoted[..end].to_owned(), &quoted[end + quote.len_utf8()..])
    } else {
        let end = value.find(char::is_whitespace).unwrap_or(value.len());
        (value[..end].to_owned(), &value[end..])
    };
    if name.trim().is_empty() {
        return Err("/new option \"--name\" cannot be empty".into());
    }
    Ok((name, rest))
}

fn required_new_prompt(value: &str) -> Result<String, String> {
    let prompt = value.trim_start();
    if prompt.is_empty() {
        Err("Missing prompt after /new \"--\"".into())
    } else {
        Ok(prompt.into())
    }
}
