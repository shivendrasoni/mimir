use std::{
    fmt,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::Usage;

pub const DEFAULT_METERED_TASK_TOKENS: u64 = 1_000_000;

#[must_use]
pub fn provider_default_token_limit(provider: &str) -> Option<u64> {
    (!matches!(provider, "anthropic" | "openai-codex")).then_some(DEFAULT_METERED_TASK_TOKENS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub max_turns: Option<u32>,
    pub max_tool_calls: Option<u32>,
    /// Cumulative fresh-input plus output token ceiling for one run.
    pub max_tokens: Option<u64>,
    pub max_elapsed: Option<Duration>,
    pub max_context_messages: usize,
    /// Provider context-window ceiling used by automatic compaction.
    pub max_context_tokens: u64,
    /// Percentage of the context window at which compaction starts.
    pub auto_compaction_threshold_percent: u8,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_turns: None,
            max_tool_calls: None,
            max_tokens: Some(DEFAULT_METERED_TASK_TOKENS),
            max_elapsed: None,
            max_context_messages: 200,
            max_context_tokens: 128_000,
            auto_compaction_threshold_percent: 80,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BudgetError {
    #[error("turn budget exhausted at {limit}")]
    Turns { limit: u32 },
    #[error("tool-call budget exhausted at {limit}")]
    ToolCalls { limit: u32 },
    #[error("token budget exhausted at {limit}")]
    Tokens { limit: u64 },
    #[error("elapsed-time budget exhausted after {limit_ms} ms")]
    Elapsed { limit_ms: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetKind {
    Turns,
    ToolCalls,
    Tokens,
    Elapsed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetPause {
    pub kind: BudgetKind,
    pub limit: u64,
    pub usage: BudgetSnapshot,
}

impl fmt::Display for BudgetPause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let exhausted = match self.kind {
            BudgetKind::Turns => "turn budget exhausted at",
            BudgetKind::ToolCalls => "tool-call budget exhausted at",
            BudgetKind::Tokens => "token budget exhausted at",
            BudgetKind::Elapsed => "elapsed-time budget exhausted after",
        };
        write!(
            formatter,
            "{exhausted} {} (turns={}, tool_calls={}, budget_tokens={}, input_tokens={}, cache_read_tokens={}, cache_write_tokens={}, fresh_input_tokens={}, output_tokens={}, current_context_tokens={}, elapsed_ms={})",
            self.limit,
            self.usage.turns,
            self.usage.tool_calls,
            self.usage.tokens,
            self.usage.input_tokens,
            self.usage.cached_tokens,
            self.usage.cache_write_tokens,
            self.usage.fresh_input_tokens,
            self.usage.output_tokens,
            self.usage.current_context_tokens,
            self.usage.elapsed_ms
        )
    }
}

impl BudgetError {
    #[must_use]
    pub fn pause(&self, usage: BudgetSnapshot) -> BudgetPause {
        match *self {
            Self::Turns { limit } => BudgetPause {
                kind: BudgetKind::Turns,
                limit: u64::from(limit),
                usage,
            },
            Self::ToolCalls { limit } => BudgetPause {
                kind: BudgetKind::ToolCalls,
                limit: u64::from(limit),
                usage,
            },
            Self::Tokens { limit } => BudgetPause {
                kind: BudgetKind::Tokens,
                limit,
                usage,
            },
            Self::Elapsed { limit_ms } => BudgetPause {
                kind: BudgetKind::Elapsed,
                limit: limit_ms,
                usage,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetSnapshot {
    pub turns: u32,
    pub tool_calls: u32,
    /// Cumulative operational tokens (fresh input plus output).
    pub tokens: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub fresh_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// Provider-reported input size for the most recently completed request.
    #[serde(default)]
    pub current_context_tokens: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug)]
pub struct BudgetUsage {
    turns: u32,
    tool_calls: u32,
    tokens: u64,
    input_tokens: u64,
    cached_tokens: u64,
    cache_write_tokens: u64,
    fresh_input_tokens: u64,
    output_tokens: u64,
    current_context_tokens: u64,
    started_at: Instant,
}

impl Default for BudgetUsage {
    fn default() -> Self {
        Self {
            turns: 0,
            tool_calls: 0,
            tokens: 0,
            input_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            fresh_input_tokens: 0,
            output_tokens: 0,
            current_context_tokens: 0,
            started_at: Instant::now(),
        }
    }
}

impl BudgetUsage {
    /// Records a completed provider turn and its token usage.
    ///
    /// # Errors
    ///
    /// Reserved for usage-accounting failures. Current counters saturate safely.
    pub fn record_turn(&mut self, usage: Usage) -> Result<(), BudgetError> {
        self.turns = self.turns.saturating_add(1);
        let fresh_input_tokens = usage.uncached_input_tokens();
        self.tokens = self.tokens.saturating_add(usage.budget_tokens());
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.cached_tokens = self.cached_tokens.saturating_add(usage.cached_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(usage.cache_write_tokens);
        self.fresh_input_tokens = self.fresh_input_tokens.saturating_add(fresh_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        self.current_context_tokens = usage.input_tokens;
        Ok(())
    }

    pub fn record_tool_call(&mut self) {
        self.tool_calls = self.tool_calls.saturating_add(1);
    }

    /// Checks whether another runtime action may start.
    ///
    /// # Errors
    ///
    /// Returns the first exhausted budget in deterministic priority order.
    pub fn check(&self, budget: &Budget) -> Result<(), BudgetError> {
        if let Some(limit) = budget.max_turns
            && self.turns >= limit
        {
            return Err(BudgetError::Turns { limit });
        }
        if let Some(limit) = budget.max_tool_calls
            && self.tool_calls >= limit
        {
            return Err(BudgetError::ToolCalls { limit });
        }
        if let Some(limit) = budget.max_tokens
            && self.tokens >= limit
        {
            return Err(BudgetError::Tokens { limit });
        }
        if let Some(limit) = budget.max_elapsed
            && self.started_at.elapsed() >= limit
        {
            return Err(BudgetError::Elapsed {
                limit_ms: duration_millis(limit),
            });
        }
        Ok(())
    }

    pub fn snapshot(&self) -> BudgetSnapshot {
        BudgetSnapshot {
            turns: self.turns,
            tool_calls: self.tool_calls,
            tokens: self.tokens,
            input_tokens: self.input_tokens,
            cached_tokens: self.cached_tokens,
            cache_write_tokens: self.cache_write_tokens,
            fresh_input_tokens: self.fresh_input_tokens,
            output_tokens: self.output_tokens,
            current_context_tokens: self.current_context_tokens,
            elapsed_ms: duration_millis(self.started_at.elapsed()),
        }
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
