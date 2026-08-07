use serde_json::Value;

use super::{App, Overlay, ThemeName, term::TerminalCapabilities};

#[derive(Debug, Clone, Copy)]
struct Rgb(u8, u8, u8);

#[derive(Debug, Clone, Copy)]
struct Palette {
    accent: Rgb,
    muted: Rgb,
    user: Rgb,
    assistant: Rgb,
    system: Rgb,
    prompt: Rgb,
}

impl Palette {
    const fn dark() -> Self {
        Self {
            accent: Rgb(96, 165, 250),
            muted: Rgb(148, 163, 184),
            user: Rgb(125, 211, 252),
            assistant: Rgb(134, 239, 172),
            system: Rgb(251, 191, 36),
            prompt: Rgb(196, 181, 253),
        }
    }

    const fn light() -> Self {
        Self {
            accent: Rgb(0, 95, 204),
            muted: Rgb(71, 85, 105),
            user: Rgb(3, 105, 161),
            assistant: Rgb(21, 128, 61),
            system: Rgb(180, 83, 9),
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
pub fn render(app: &App, size: TerminalSize, options: RenderOptions) -> String {
    if size.width == 0 || size.height == 0 {
        return String::new();
    }
    let mut lines = vec![
        format!("Mimir Rust TUI [{}]", app.theme_label()),
        format!(
            "Model: {} | Effort: {} | Session: {}",
            app.selected_model().unwrap_or("unset"),
            app.selected_effort().as_str(),
            app.selected_session().unwrap_or("default")
        ),
        String::new(),
    ];

    let transcript_budget = size.height.saturating_sub(3);
    let transcript_start = app.transcript().len().saturating_sub(transcript_budget);
    for entry in &app.transcript()[transcript_start..] {
        lines.push(format!("{}> {}", entry.role.label(), entry.text));
    }

    if let Some(active) = app.active_assistant_text() {
        lines.push(format!("assistant…> {active}"));
    }

    append_overlay(app, &mut lines);

    lines.push(String::new());
    let prompt = format!(
        "{}Prompt: {}",
        " ".repeat(usize::from(app.editor_padding_x())),
        app.prompt()
    );

    let wrapped = wrap_lines(&lines, size.width);
    let prompt_lines = wrap_lines(&[prompt], size.width);
    let prompt_height = prompt_lines.len().min(size.height);
    let content_height = size.height.saturating_sub(prompt_height);
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
    let prompt_start = prompt_lines.len().saturating_sub(prompt_height);
    visible.extend(prompt_lines.into_iter().skip(prompt_start));
    let body = if options.capabilities.ansi && options.capabilities.color {
        let palette = resolved_palette(app);
        visible
            .iter()
            .enumerate()
            .map(|(index, line)| style_line(line, index, palette))
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
    apply_color_aliases(definition, &["system", "warning"], &mut palette.system);
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

fn style_line(line: &str, index: usize, palette: Palette) -> String {
    if line.is_empty() {
        return String::new();
    }
    let color = if index == 0 {
        palette.accent
    } else if line.trim_start().starts_with("user>") {
        palette.user
    } else if line.trim_start().starts_with("assistant") {
        palette.assistant
    } else if line.trim_start().starts_with("system>") {
        palette.system
    } else if line.trim_start().starts_with("Prompt:") {
        palette.prompt
    } else {
        palette.muted
    };
    format!(
        "\u{1b}[38;2;{};{};{}m{line}\u{1b}[0m",
        color.0, color.1, color.2
    )
}

fn append_overlay(app: &App, lines: &mut Vec<String>) {
    match app.overlay() {
        Overlay::None => {}
        Overlay::Help => {
            lines.push(String::new());
            lines.push("Help".into());
            lines.push(
                "/login /logout /model /effort /session /sessions /resume /new /clear /name".into(),
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
            lines.push("Backspace/Delete edit · Ctrl+C cancel run · Ctrl+D quit".into());
        }
        Overlay::Confirm { title, message, .. } => {
            lines.push(String::new());
            lines.push(title.clone());
            lines.push(message.clone());
            lines.push("Enter confirm · Esc cancel".into());
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
