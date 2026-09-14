use std::fmt::Write as _;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    Accent,
    Muted,
    User,
    Assistant,
    Thinking,
    Tool,
    System,
    Error,
    Prompt,
    Link,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextStyle {
    tone: Tone,
    decorations: u8,
}

impl TextStyle {
    const BOLD: u8 = 1 << 0;
    const ITALIC: u8 = 1 << 1;
    const DIM: u8 = 1 << 2;
    const UNDERLINE: u8 = 1 << 3;

    const fn new(tone: Tone) -> Self {
        Self {
            tone,
            decorations: 0,
        }
    }

    const fn bold(mut self) -> Self {
        self.decorations |= Self::BOLD;
        self
    }

    const fn italic(mut self) -> Self {
        self.decorations |= Self::ITALIC;
        self
    }

    const fn dim(mut self) -> Self {
        self.decorations |= Self::DIM;
        self
    }

    const fn underline(mut self) -> Self {
        self.decorations |= Self::UNDERLINE;
        self
    }

    const fn has(self, decoration: u8) -> bool {
        self.decorations & decoration != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StyledSpan {
    text: String,
    style: TextStyle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StyledLine {
    spans: Vec<StyledSpan>,
    continuation: Vec<StyledSpan>,
}

impl StyledLine {
    const fn empty() -> Self {
        Self {
            spans: Vec::new(),
            continuation: Vec::new(),
        }
    }

    fn plain(text: impl Into<String>, style: TextStyle) -> Self {
        Self {
            spans: vec![StyledSpan {
                text: text.into(),
                style,
            }],
            continuation: Vec::new(),
        }
    }

    fn push(&mut self, text: impl Into<String>, style: TextStyle) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.spans.last_mut()
            && last.style == style
        {
            last.text.push_str(&text);
        } else {
            self.spans.push(StyledSpan { text, style });
        }
    }
}

impl Palette {
    const fn dark() -> Self {
        Self {
            accent: Rgb(96, 165, 250),
            muted: Rgb(148, 163, 184),
            user: Rgb(125, 211, 252),
            assistant: Rgb(203, 213, 225),
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
            assistant: Rgb(30, 41, 59),
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
    let accent = TextStyle::new(Tone::Accent);
    let muted = TextStyle::new(Tone::Muted);
    let mut lines = vec![
        StyledLine::plain(
            format!("   ▄████▄      Mimir v{}", env!("CARGO_PKG_VERSION")),
            accent,
        ),
        StyledLine::plain(
            format!(
                "  █ ◈  ◈ █     {}",
                app.selected_model().unwrap_or("select a model")
            ),
            muted,
        ),
        StyledLine::plain(
            format!(
                "  █  ▄▄  █     {} effort · {} session",
                app.selected_effort().as_str(),
                app.selected_session().unwrap_or("default")
            ),
            muted,
        ),
        StyledLine::plain("   █▄██▄█", muted),
        StyledLine::plain("    ▀  ▀", muted),
        StyledLine::plain(rule.clone(), muted),
        StyledLine::empty(),
    ];

    let transcript_start = app.transcript().len().saturating_sub(size.height);
    for entry in &app.transcript()[transcript_start..] {
        lines.extend(transcript_lines(entry.role, &entry.text));
        lines.push(StyledLine::empty());
    }

    if let Some(active) = app.active_assistant_text() {
        lines.extend(assistant_markdown_lines(active));
    }

    let mut overlay = Vec::new();
    append_overlay(app, &mut overlay, size.width);
    lines.extend(
        overlay
            .into_iter()
            .map(|line| StyledLine::plain(line, muted)),
    );

    let mut composer = Vec::<StyledLine>::new();
    if let Some(activity) = app.current_activity() {
        composer.push(StyledLine::plain(
            format!("✦ {activity}"),
            TextStyle::new(Tone::Thinking),
        ));
    } else if app.run_active() {
        composer.push(StyledLine::plain(
            "✦ Working…",
            TextStyle::new(Tone::Thinking),
        ));
    }
    if !app.pending_images().is_empty() {
        let bytes = app
            .pending_images()
            .iter()
            .map(|image| image.byte_size)
            .sum::<usize>();
        composer.push(StyledLine::plain(
            format!(
                "▣ {} image{} attached · {}",
                app.pending_images().len(),
                if app.pending_images().len() == 1 {
                    ""
                } else {
                    "s"
                },
                human_bytes(bytes)
            ),
            muted,
        ));
    }
    composer.push(StyledLine::plain(rule, muted));
    composer.push(StyledLine::plain(
        format!(
            "{} · {} · mode:{} · /help · @ paths · ctrl+v image · ctrl+c clear/cancel · twice exit",
            app.selected_model().unwrap_or("model unset"),
            app.selected_effort().as_str(),
            app.agent_mode().as_str()
        ),
        muted,
    ));
    if let Some(hint) = app.command_parameter_hint() {
        composer.push(StyledLine::plain(format!("↳ {hint}"), muted));
    }
    composer.push(StyledLine::plain(
        format!(
            "{}❯ {}",
            " ".repeat(usize::from(app.editor_padding_x())),
            app.prompt()
        ),
        TextStyle::new(Tone::Prompt),
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
        StyledLine::empty(),
        content_height.saturating_sub(visible.len()),
    ));
    let composer_start = composer_lines.len().saturating_sub(composer_height);
    visible.extend(composer_lines.into_iter().skip(composer_start));
    let body = if options.capabilities.ansi && options.capabilities.color {
        let palette = resolved_palette(app);
        visible
            .iter()
            .map(|line| style_line(line, palette))
            .collect::<Vec<_>>()
            .join("\r\n")
    } else {
        visible
            .iter()
            .map(plain_line)
            .collect::<Vec<_>>()
            .join("\n")
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

fn transcript_lines(role: TranscriptRole, text: &str) -> Vec<StyledLine> {
    if role == TranscriptRole::Assistant {
        return assistant_markdown_lines(text);
    }
    let (prefix, tone) = match role {
        TranscriptRole::User => ("❯", Tone::User),
        TranscriptRole::Assistant => unreachable!("assistant handled above"),
        TranscriptRole::Thinking => ("✦ Thinking ·", Tone::Thinking),
        TranscriptRole::Tool => ("  └", Tone::Tool),
        TranscriptRole::System => ("•", Tone::System),
        TranscriptRole::Warning => ("▲", Tone::System),
        TranscriptRole::Error => ("✕", Tone::Error),
    };
    let style = TextStyle::new(tone);
    let continuation = " ".repeat(prefix.chars().count().saturating_add(1));
    let mut output = Vec::new();
    for (index, source) in text.split('\n').enumerate() {
        let line_prefix = if index == 0 { prefix } else { &continuation };
        let mut line = StyledLine::empty();
        line.push(format!("{line_prefix} "), style);
        line.push(source.trim_end_matches('\r'), style);
        line.continuation = vec![StyledSpan {
            text: continuation.clone(),
            style,
        }];
        output.push(line);
    }
    output
}

fn assistant_markdown_lines(text: &str) -> Vec<StyledLine> {
    let assistant = TextStyle::new(Tone::Assistant);
    let muted = TextStyle::new(Tone::Muted).dim();
    let link = TextStyle::new(Tone::Link);
    let mut output = Vec::new();
    let mut first_content = true;
    let mut in_code_block = false;
    let mut fence_character = None;

    for source in text.split('\n') {
        let source = source.trim_end_matches(['\r', ' ', '\t']);
        let trimmed = source.trim_start();
        if let Some((character, language)) = markdown_fence(trimmed) {
            if in_code_block && fence_character == Some(character) {
                in_code_block = false;
                fence_character = None;
                continue;
            }
            if !in_code_block {
                ensure_block_spacing(&mut output);
                in_code_block = true;
                fence_character = Some(character);
                if !language.is_empty() {
                    push_assistant_line(
                        &mut output,
                        &mut first_content,
                        "┌─ ",
                        language,
                        muted,
                        false,
                    );
                }
                continue;
            }
        }

        if in_code_block {
            push_assistant_line(&mut output, &mut first_content, "│ ", source, link, false);
            continue;
        }

        if trimmed.is_empty() {
            if !first_content && output.last().is_some_and(|line| !line.spans.is_empty()) {
                output.push(StyledLine::empty());
            }
            continue;
        }

        if let Some(heading) = markdown_heading(trimmed) {
            ensure_block_spacing(&mut output);
            push_assistant_line(
                &mut output,
                &mut first_content,
                "",
                heading,
                assistant.bold(),
                true,
            );
            continue;
        }

        if is_markdown_rule(trimmed) {
            ensure_block_spacing(&mut output);
            push_assistant_line(
                &mut output,
                &mut first_content,
                "",
                "────────",
                muted,
                false,
            );
            continue;
        }

        if let Some((prefix, content)) = markdown_list_item(trimmed) {
            push_assistant_line(
                &mut output,
                &mut first_content,
                &prefix,
                content,
                assistant,
                true,
            );
            continue;
        }

        if let Some(quote) = trimmed.strip_prefix("> ") {
            ensure_block_spacing(&mut output);
            push_assistant_line(
                &mut output,
                &mut first_content,
                "│ ",
                quote,
                assistant.italic(),
                true,
            );
            continue;
        }

        push_assistant_line(&mut output, &mut first_content, "", source, assistant, true);
    }

    while output.last().is_some_and(|line| line.spans.is_empty()) {
        output.pop();
    }
    output
}

fn ensure_block_spacing(output: &mut Vec<StyledLine>) {
    if output.last().is_some_and(|line| !line.spans.is_empty()) {
        output.push(StyledLine::empty());
    }
}

fn push_assistant_line(
    output: &mut Vec<StyledLine>,
    first_content: &mut bool,
    block_prefix: &str,
    content: &str,
    style: TextStyle,
    parse_inline: bool,
) {
    let entry_prefix = if *first_content { "● " } else { "  " };
    *first_content = false;
    let mut line = StyledLine::empty();
    line.push(entry_prefix, TextStyle::new(Tone::Assistant));
    if !block_prefix.is_empty() {
        line.push(block_prefix, style);
    }
    if parse_inline {
        push_inline_markdown(&mut line, content, style);
    } else {
        line.push(content, style);
    }
    line.continuation = vec![StyledSpan {
        text: " ".repeat(entry_prefix.chars().count() + block_prefix.chars().count()),
        style,
    }];
    output.push(line);
}

fn markdown_fence(text: &str) -> Option<(char, &str)> {
    for marker in ["```", "~~~"] {
        if let Some(rest) = text.strip_prefix(marker) {
            return Some((marker.chars().next()?, rest.trim()));
        }
    }
    None
}

fn markdown_heading(text: &str) -> Option<&str> {
    let hashes = text.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&hashes) || text.as_bytes().get(hashes) != Some(&b' ') {
        return None;
    }
    Some(text[hashes + 1..].trim_end_matches('#').trim_end())
}

fn is_markdown_rule(text: &str) -> bool {
    let compact = text
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    compact.len() >= 3
        && compact
            .chars()
            .all(|character| character == '-' || character == '*' || character == '_')
        && compact
            .chars()
            .all(|character| character == compact.chars().next().unwrap_or('-'))
}

fn markdown_list_item(text: &str) -> Option<(String, &str)> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(content) = text.strip_prefix(marker) {
            if let Some(content) = content.strip_prefix("[ ] ") {
                return Some(("☐ ".into(), content));
            }
            if let Some(content) = content
                .strip_prefix("[x] ")
                .or_else(|| content.strip_prefix("[X] "))
            {
                return Some(("☑ ".into(), content));
            }
            return Some(("• ".into(), content));
        }
    }
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let suffix = &text[digits..];
    let content = suffix
        .strip_prefix(". ")
        .or_else(|| suffix.strip_prefix(") "))?;
    Some((format!("{}. ", &text[..digits]), content))
}

fn push_inline_markdown(line: &mut StyledLine, mut text: &str, style: TextStyle) {
    while !text.is_empty() {
        if let Some(rest) = text.strip_prefix('\\')
            && let Some(character) = rest.chars().next()
        {
            line.push(character.to_string(), style);
            text = &rest[character.len_utf8()..];
            continue;
        }
        if let Some(rest) = text.strip_prefix("**")
            && let Some(end) = rest.find("**")
        {
            push_inline_markdown(line, &rest[..end], style.bold());
            text = &rest[end + 2..];
            continue;
        }
        if let Some(rest) = text.strip_prefix("__")
            && let Some(end) = rest.find("__")
        {
            push_inline_markdown(line, &rest[..end], style.bold());
            text = &rest[end + 2..];
            continue;
        }
        if let Some(rest) = text.strip_prefix('`')
            && let Some(end) = rest.find('`')
        {
            line.push(&rest[..end], TextStyle::new(Tone::Link));
            text = &rest[end + 1..];
            continue;
        }
        if let Some(rest) = text.strip_prefix("~~")
            && let Some(end) = rest.find("~~")
        {
            push_inline_markdown(line, &rest[..end], style.dim());
            text = &rest[end + 2..];
            continue;
        }
        if let Some(rest) = text.strip_prefix('*')
            && let Some(end) = rest.find('*')
        {
            push_inline_markdown(line, &rest[..end], style.italic());
            text = &rest[end + 1..];
            continue;
        }
        if let Some(rest) = text.strip_prefix('[')
            && let Some(label_end) = rest.find("](")
        {
            let after_label = &rest[label_end + 2..];
            if let Some(url_end) = after_label.find(')') {
                let linked = TextStyle::new(Tone::Link).underline();
                push_inline_markdown(line, &rest[..label_end], linked);
                let url = &after_label[..url_end];
                if rest[..label_end] != *url {
                    line.push(format!(" ({url})"), linked.dim());
                }
                text = &after_label[url_end + 1..];
                continue;
            }
        }
        if text.starts_with("https://") || text.starts_with("http://") {
            let end = text.find(char::is_whitespace).unwrap_or(text.len());
            let mut visible_end = end;
            while visible_end > 0 && matches!(text.as_bytes()[visible_end - 1], b'.' | b',' | b';')
            {
                visible_end -= 1;
            }
            line.push(&text[..visible_end], TextStyle::new(Tone::Link).underline());
            text = &text[visible_end..];
            continue;
        }

        let next = text
            .char_indices()
            .skip(1)
            .find_map(|(index, _)| is_inline_marker(&text[index..]).then_some(index))
            .unwrap_or(text.len());
        line.push(&text[..next], style);
        text = &text[next..];
    }
}

fn is_inline_marker(text: &str) -> bool {
    text.starts_with('\\')
        || text.starts_with("**")
        || text.starts_with("__")
        || text.starts_with('`')
        || text.starts_with("~~")
        || text.starts_with('*')
        || text.starts_with('[')
        || text.starts_with("https://")
        || text.starts_with("http://")
}

fn plain_line(line: &StyledLine) -> String {
    line.spans
        .iter()
        .map(|span| span.text.as_str())
        .collect::<String>()
}

fn style_line(line: &StyledLine, palette: Palette) -> String {
    if line.spans.is_empty() {
        return String::new();
    }
    let mut rendered = String::new();
    for span in &line.spans {
        let color = match span.style.tone {
            Tone::Accent | Tone::Link => palette.accent,
            Tone::Muted => palette.muted,
            Tone::User => palette.user,
            Tone::Assistant => palette.assistant,
            Tone::Thinking => palette.thinking,
            Tone::Tool => palette.tool,
            Tone::System => palette.system,
            Tone::Error => palette.error,
            Tone::Prompt => palette.prompt,
        };
        rendered.push_str("\u{1b}[");
        if span.style.has(TextStyle::BOLD) {
            rendered.push_str("1;");
        }
        if span.style.has(TextStyle::DIM) {
            rendered.push_str("2;");
        }
        if span.style.has(TextStyle::ITALIC) {
            rendered.push_str("3;");
        }
        if span.style.has(TextStyle::UNDERLINE) {
            rendered.push_str("4;");
        }
        write!(
            &mut rendered,
            "38;2;{};{};{}m{}\u{1b}[0m",
            color.0, color.1, color.2, span.text
        )
        .expect("writing terminal styling to a String cannot fail");
    }
    rendered
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

#[allow(
    clippy::too_many_lines,
    reason = "overlay rendering exhaustively maps each mutually exclusive TUI surface"
)]
fn append_overlay(app: &App, lines: &mut Vec<String>, width: usize) {
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
            lines.push(
                "/implement /compact /refine /goal /autonomous /heartbeat /heartbeats".into(),
            );
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
                "Backspace/Delete edit · @ workspace paths · Ctrl+V attach image · Ctrl+C clear/cancel · twice exit · Ctrl+D quit"
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
        Overlay::Clarification {
            request,
            selected,
            input,
        } => {
            lines.push(String::new());
            lines.push(request.header.clone());
            lines.push(request.question.clone());
            for (index, option) in request.options.iter().enumerate() {
                let recommended = if index == 0 { " (Recommended)" } else { "" };
                lines.push(format!(
                    "{} {}{} — {}",
                    if *selected == index { ">" } else { " " },
                    option.label,
                    recommended,
                    option.description
                ));
            }
            lines.push(format!(
                "{} Other: {}",
                if *selected == request.options.len() {
                    ">"
                } else {
                    " "
                },
                input
            ));
            lines.push("Up/Down choose · Type for Other · Enter answer · Esc close".into());
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
            let command_width = selector
                .options
                .iter()
                .map(|option| option.chars().count())
                .max()
                .unwrap_or_default()
                .min(32);
            for (index, option) in selector.options.iter().enumerate() {
                let marker = if index == selector.selected { '>' } else { ' ' };
                if selector.kind == super::OverlayKind::ScopedModelsSelector {
                    let checked = if app.scoped_models().contains(option) {
                        "[x]"
                    } else {
                        "[ ]"
                    };
                    lines.push(format!("{marker} {checked} {option}"));
                } else if selector.kind == super::OverlayKind::Autocomplete {
                    let description = app.autocomplete_description(option);
                    let description = truncate_with_ellipsis(
                        &description,
                        width.saturating_sub(command_width.saturating_add(5)),
                    );
                    lines.push(format!("{marker} {option:command_width$}  {description}"));
                } else if selector.kind == super::OverlayKind::PathAutocomplete {
                    let description = App::path_autocomplete_description(option);
                    lines.push(format!("{marker} {option:command_width$}  {description}"));
                } else {
                    lines.push(format!("{marker} {option}"));
                }
            }
        }
    }
}

fn truncate_with_ellipsis(text: &str, max_chars: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }
    if max_chars == 0 {
        return String::new();
    }
    if max_chars == 1 {
        return "…".into();
    }
    let mut truncated = normalized.chars().take(max_chars - 1).collect::<String>();
    truncated.push('…');
    truncated
}

fn wrap_lines(lines: &[StyledLine], width: usize) -> Vec<StyledLine> {
    let mut wrapped = Vec::new();
    for line in lines {
        if line.spans.is_empty() {
            wrapped.push(StyledLine::empty());
            continue;
        }
        // Provider, tool, and extension text is untrusted terminal input. Strip
        // C0/C1 controls before adding the renderer's own bounded ANSI styles.
        let width = width.max(1);
        let mut current = StyledLine::empty();
        let mut current_width = 0;
        for span in &line.spans {
            for character in span.text.chars() {
                if current_width == width {
                    wrapped.push(current);
                    current = StyledLine::empty();
                    let continuation_width = line
                        .continuation
                        .iter()
                        .map(|span| span.text.chars().count())
                        .sum::<usize>();
                    if continuation_width < width {
                        current.spans.clone_from(&line.continuation);
                        current_width = continuation_width;
                    } else {
                        current_width = 0;
                    }
                }
                current.push(
                    if character.is_control() {
                        " ".to_owned()
                    } else {
                        character.to_string()
                    },
                    span.style,
                );
                current_width = current_width.saturating_add(1);
            }
        }
        wrapped.push(current);
    }
    wrapped
}
