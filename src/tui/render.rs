use serde_json::Value;

use super::{App, Overlay, ThemeName, app::TranscriptRole, term::TerminalCapabilities};

#[derive(Debug, Clone, Copy)]
struct Rgb(u8, u8, u8);

#[derive(Debug, Clone, Copy)]
struct Palette {
    accent: Rgb,
    muted: Rgb,
    user: Rgb,
    assistant: Rgb,
    thinking: Rgb,
    tool: Rgb,
    system: Rgb,
    error: Rgb,
    prompt: Rgb,
}

impl Palette {
    const fn dark() -> Self {
        Self {
            accent: Rgb(96, 165, 250),
            muted: Rgb(148, 163, 184),
            user: Rgb(125, 211, 252),
            assistant: Rgb(134, 239, 172),
            thinking: Rgb(167, 139, 250),
            tool: Rgb(148, 163, 184),
            system: Rgb(251, 191, 36),
            error: Rgb(248, 113, 113),
            prompt: Rgb(196, 181, 253),
        }
    }

    const fn light() -> Self {
        Self {
            accent: Rgb(0, 95, 204),
            muted: Rgb(71, 85, 105),
            user: Rgb(3, 105, 161),
            assistant: Rgb(21, 128, 61),
            thinking: Rgb(109, 40, 217),
            tool: Rgb(71, 85, 105),
            system: Rgb(180, 83, 9),
            error: Rgb(185, 28, 28),
            prompt: Rgb(109, 40, 217),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub width: usize,
    pub height: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOptions {
    pub capabilities: TerminalCapabilities,
}

#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "the renderer keeps one explicit top-to-bottom terminal layout pipeline"
)]
pub fn render(app: &App, size: TerminalSize, options: RenderOptions) -> String {
    if size.width == 0 || size.height == 0 {
        return String::new();
    }
    let rule = "─".repeat(size.width.min(120));
    let mut lines = vec![
        "◆ Mimir".into(),
        format!(
            "  {}  ·  {} effort  ·  {} session",
            app.selected_model().unwrap_or("select a model"),
            app.selected_effort().as_str(),
            app.selected_session().unwrap_or("default")
        ),
        rule.clone(),
        String::new(),
    ];

    let transcript_start = app.transcript().len().saturating_sub(size.height);
    for entry in &app.transcript()[transcript_start..] {
        let prefix = match entry.role {
            TranscriptRole::User => "❯",
            TranscriptRole::Assistant => "●",
            TranscriptRole::Thinking => "✦ Thinking ·",
            TranscriptRole::Tool => "  └",
            TranscriptRole::System => "•",
            TranscriptRole::Warning => "▲",
            TranscriptRole::Error => "✕",
        };
        lines.push(format!("{prefix} {}", entry.text));
        lines.push(String::new());
    }

    if let Some(active) = app.active_assistant_text() {
        lines.push(format!("● {active}"));
    }

    append_overlay(app, &mut lines);

    let mut composer = Vec::new();
    if let Some(activity) = app.current_activity() {
        composer.push(format!("✦ {activity}"));
    } else if app.run_active() {
        composer.push("✦ Working…".into());
    }
    if !app.pending_images().is_empty() {
        let bytes = app
            .pending_images()
            .iter()
            .map(|image| image.byte_size)
            .sum::<usize>();
        composer.push(format!(
            "▣ {} image{} attached · {}",
            app.pending_images().len(),
            if app.pending_images().len() == 1 {
                ""
            } else {
                "s"
            },
            human_bytes(bytes)
        ));
    }
    composer.push(rule);
    composer.push(format!(
        "{} · {} · mode:{} · /help  ·  ctrl+v attach image  ·  ctrl+c cancel · twice exit",
        app.selected_model().unwrap_or("model unset"),
        app.selected_effort().as_str(),
        app.agent_mode().as_str()
    ));
    if let Some(hint) = app.command_parameter_hint() {
        composer.push(format!("↳ {hint}"));
    }
    composer.push(format!(
        "{}❯ {}",
        " ".repeat(usize::from(app.editor_padding_x())),
        app.prompt()
    ));

    let wrapped = wrap_lines(&lines, size.width);
    let composer_lines = wrap_lines(&composer, size.width);
    let composer_height = composer_lines.len().min(size.height);
    let content_height = size.height.saturating_sub(composer_height);
    let visible_content = if wrapped.len() > content_height {
        wrapped[wrapped.len() - content_height..].to_vec()
    } else {
        wrapped
    };
    let mut visible = Vec::with_capacity(size.height);
    visible.extend(visible_content);
    visible.extend(std::iter::repeat_n(
        String::new(),
        content_height.saturating_sub(visible.len()),
    ));
    let composer_start = composer_lines.len().saturating_sub(composer_height);
    visible.extend(composer_lines.into_iter().skip(composer_start));
    let body = if options.capabilities.ansi && options.capabilities.color {
        let palette = resolved_palette(app);
        let last_line = visible.len().saturating_sub(1);
        visible
            .iter()
            .enumerate()
            .map(|(index, line)| style_line(line, index, last_line, palette))
            .collect::<Vec<_>>()
            .join("\r\n")
    } else {
        visible.join("\n")
    };
    if options.capabilities.ansi {
        format!("{}{}", options.capabilities.screen_prefix(), body)
    } else {
        body
    }
}

fn resolved_palette(app: &App) -> Palette {
    let mut palette = match app.theme() {
        ThemeName::Light => Palette::light(),
        ThemeName::Dark | ThemeName::System => Palette::dark(),
    };
    let Some(theme) = app.selected_custom_theme() else {
        return palette;
    };
    let definition = &theme.definition;
    apply_color(definition, "accent", &mut palette.accent);
    apply_color(definition, "muted", &mut palette.muted);
    apply_color_aliases(
        definition,
        &["user", "userMessage", "userMessageText"],
        &mut palette.user,
    );
    apply_color_aliases(
        definition,
        &["assistant", "text", "assistantText"],
        &mut palette.assistant,
    );
    apply_color_aliases(
        definition,
        &["thinking", "reasoning"],
        &mut palette.thinking,
    );
    apply_color_aliases(definition, &["tool", "muted"], &mut palette.tool);
    apply_color_aliases(definition, &["system", "warning"], &mut palette.system);
    apply_color_aliases(definition, &["error", "danger"], &mut palette.error);
    apply_color_aliases(definition, &["prompt", "accent"], &mut palette.prompt);
    palette
}

fn apply_color_aliases(definition: &Value, names: &[&str], target: &mut Rgb) {
    if let Some(color) = names.iter().find_map(|name| theme_color(definition, name)) {
        *target = color;
    }
}

fn apply_color(definition: &Value, name: &str, target: &mut Rgb) {
    if let Some(color) = theme_color(definition, name) {
        *target = color;
    }
}

fn theme_color(definition: &Value, name: &str) -> Option<Rgb> {
    let object = definition.as_object()?;
    let colors = object.get("colors").and_then(Value::as_object);
    let vars = object.get("vars").and_then(Value::as_object);
    let value = colors
        .and_then(|colors| colors.get(name))
        .or_else(|| vars.and_then(|vars| vars.get(name)))?
        .as_str()?;
    let resolved = value
        .strip_prefix('$')
        .and_then(|variable| vars.and_then(|vars| vars.get(variable)))
        .and_then(Value::as_str)
        .unwrap_or(value);
    parse_hex_color(resolved)
}

fn parse_hex_color(value: &str) -> Option<Rgb> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(Rgb(
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ))
}

fn style_line(line: &str, index: usize, last_line: usize, palette: Palette) -> String {
    if line.is_empty() {
        return String::new();
    }
    let color = if index == last_line {
        palette.prompt
    } else if index == 0 || line.starts_with('◆') {
        palette.accent
    } else if line.trim_start().starts_with('❯') {
        palette.user
    } else if line.trim_start().starts_with('●') {
        palette.assistant
    } else if line.trim_start().starts_with('✦') {
        palette.thinking
    } else if line.trim_start().starts_with('└') {
        palette.tool
    } else if line.trim_start().starts_with('✕') {
        palette.error
    } else if line.trim_start().starts_with('▲') || line.trim_start().starts_with('•') {
        palette.system
    } else {
        palette.muted
    };
    format!(
        "\u{1b}[38;2;{};{};{}m{line}\u{1b}[0m",
        color.0, color.1, color.2
    )
}

fn human_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    if bytes < 1024 * 1024 {
        let tenths = bytes.saturating_mul(10) / 1024;
        return format!("{}.{:01} KiB", tenths / 10, tenths % 10);
    }
    let tenths = bytes.saturating_mul(10) / (1024 * 1024);
    format!("{}.{:01} MiB", tenths / 10, tenths % 10)
}

fn append_overlay(app: &App, lines: &mut Vec<String>) {
    match app.overlay() {
        Overlay::None => {}
        Overlay::Help => {
            lines.push(String::new());
            lines.push("Help".into());
            lines.push(
                "/login /logout /model /mode /effort /session /sessions /resume /new /clear /name"
                    .into(),
            );
            lines.push("/tree /fork /clone".into());
            lines.push("/compact /refine /goal /autonomous /heartbeat /heartbeats".into());
            lines.push("/fast /scoped-models /copy /btw /export /import /share /traces".into());
            lines.push("/system-prompt /logs /changelog /update /rlm-max-depth /fullscreen".into());
            lines.push("/skill:<name> [request] (names are exposed by get_commands)".into());
            lines.push("/reload /usage /context /mcp /settings /theme /help /quit".into());
            lines.push("Help".into());
        }
        Overlay::Hotkeys => {
            lines.push(String::new());
            lines.push("Keyboard shortcuts".into());
            lines.push("Enter submit/confirm · Esc close overlay or quit".into());
            lines.push("Up/Down history or selector · Left/Right move cursor".into());
            lines.push(
                "Backspace/Delete edit · Ctrl+V attach image · Ctrl+C cancel · twice exit · Ctrl+D quit"
                    .into(),
            );
        }
        Overlay::Confirm { title, message, .. } => {
            lines.push(String::new());
            lines.push(title.clone());
            lines.push(message.clone());
            lines.push("Enter confirm · Esc cancel".into());
        }
        Overlay::WorkspacePermission { request, selected } => {
            lines.push(String::new());
            lines.push("Workspace permission required".into());
            lines.push(request.message());
            for (index, option) in ["Allow once", "Always allow for this workspace", "Deny"]
                .iter()
                .enumerate()
            {
                lines.push(format!(
                    "{} {option}",
                    if *selected == index { ">" } else { " " }
                ));
            }
            lines.push("Up/Down choose · Enter decide · Esc deny".into());
        }
        Overlay::Login { provider, input } => {
            lines.push(String::new());
            lines.push("Login".into());
            lines.push(format!(
                "Provider: {}",
                provider
                    .as_deref()
                    .unwrap_or("type provider and press Enter")
            ));
            if provider.is_some() {
                lines.push(format!(
                    "Credential (leave blank for OAuth): {}",
                    "•".repeat(input.chars().count())
                ));
            }
        }
        Overlay::Logout { provider, input } => {
            lines.push(String::new());
            lines.push("Logout".into());
            lines.push(format!(
                "Provider: {}",
                provider.as_deref().unwrap_or(input.as_str())
            ));
        }
        Overlay::McpLogin { server, input } => {
            lines.push(String::new());
            lines.push("MCP login".into());
            lines.push(format!("Server: {server}"));
            lines.push(format!("API key: {}", "•".repeat(input.chars().count())));
        }
        Overlay::Selector(selector) => {
            lines.push(String::new());
            lines.push(selector.title.clone());
            for (index, option) in selector.options.iter().enumerate() {
                let marker = if index == selector.selected { '>' } else { ' ' };
                if selector.kind == super::OverlayKind::ScopedModelsSelector {
                    let checked = if app.scoped_models().contains(option) {
                        "[x]"
                    } else {
                        "[ ]"
                    };
                    lines.push(format!("{marker} {checked} {option}"));
                } else {
                    lines.push(format!("{marker} {option}"));
                }
            }
        }
    }
}

fn wrap_lines(lines: &[String], width: usize) -> Vec<String> {
    let mut wrapped = Vec::new();
    for line in lines {
        if line.is_empty() {
            wrapped.push(String::new());
            continue;
        }
        // Provider, tool, and extension text is untrusted terminal input. Strip
        // C0/C1 controls before adding the renderer's own bounded ANSI styles.
        let chars = line
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect::<Vec<_>>();
        for chunk in chars.chunks(width.max(1)) {
            wrapped.push(chunk.iter().collect());
        }
    }
    wrapped
}
