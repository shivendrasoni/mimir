use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType", alias = "mime_type")]
        mime_type: String,
    },
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redacted: bool,
    },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    #[default]
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
    BudgetExhausted,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub const ALL: [Self; 7] = [
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
}

impl Usage {
    /// Returns the provider's raw total token count for telemetry.
    ///
    /// `cached_tokens` is provider metadata describing a subset of input tokens
    /// after provider adapters normalize usage. Adding it again would double
    /// count OpenAI-compatible cache hits.
    pub fn total(self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Returns input tokens that were not served from the provider cache.
    ///
    /// Provider adapters normalize `cached_tokens` as a subset of
    /// `input_tokens`. Saturating subtraction keeps malformed third-party
    /// telemetry from underflowing the budget counter.
    #[must_use]
    pub fn uncached_input_tokens(self) -> u64 {
        self.input_tokens.saturating_sub(self.cached_tokens)
    }

    /// Returns the tokens charged to Mimir's cumulative operational budget.
    ///
    /// Cached prompt replay remains visible in raw telemetry and current-context
    /// measurements, but does not consume the same run budget again.
    #[must_use]
    pub fn budget_tokens(self) -> u64 {
        self.uncached_input_tokens()
            .saturating_add(self.output_tokens)
    }

    /// Normalizes providers that report cached input separately from uncached
    /// input (for example Anthropic) into the shared inclusive-input contract.
    #[must_use]
    pub fn from_separate_cached_input(
        uncached_input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
    ) -> Self {
        Self {
            input_tokens: uncached_input_tokens.saturating_add(cached_tokens),
            output_tokens,
            cached_tokens,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
    pub stop_reason: Option<StopReason>,
    pub usage: Usage,
    pub timestamp_ms: i64,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self::new(Role::User, vec![Content::Text { text: text.into() }])
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self::new(Role::System, vec![Content::Text { text: text.into() }])
    }

    /// Creates a user message from already validated content blocks.
    pub fn user_content(content: Vec<Content>) -> Self {
        Self::new(Role::User, content)
    }

    pub fn assistant(content: Vec<Content>, stop_reason: StopReason) -> Self {
        let mut message = Self::new(Role::Assistant, content);
        message.stop_reason = Some(stop_reason);
        message
    }

    /// Creates the empty assistant envelope used before the first stream delta.
    pub fn assistant_pending() -> Self {
        Self::new(Role::Assistant, Vec::new())
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self::new(
            Role::Tool,
            vec![Content::ToolResult(ToolResult {
                tool_call_id: tool_call_id.into(),
                tool_name: tool_name.into(),
                content: content.into(),
                is_error,
            })],
        )
    }

    /// Creates a tool-role message from one or more validated result blocks.
    pub fn tool_results(content: Vec<Content>) -> Self {
        debug_assert!(
            content
                .iter()
                .all(|block| matches!(block, Content::ToolResult(_)))
        );
        Self::new(Role::Tool, content)
    }

    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|content| match content {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn new(role: Role, content: Vec<Content>) -> Self {
        Self {
            role,
            content,
            stop_reason: None,
            usage: Usage::default(),
            timestamp_ms: Utc::now().timestamp_millis(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model: String,
    #[serde(default)]
    pub thinking_level: ThinkingLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<String>,
    pub system_prompt: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    pub message: Message,
    pub response_id: Option<String>,
}
