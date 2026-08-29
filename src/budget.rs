use std::{
    fmt,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub max_turns: u32,
    pub max_tool_calls: u32,
    pub max_tokens: u64,
    pub max_elapsed: Duration,
    pub max_context_messages: usize,
    /// Provider context-window ceiling used by automatic compaction.
    pub max_context_tokens: u64,
    /// Percentage of the context window at which compaction starts.
    pub auto_compaction_threshold_percent: u8,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_turns: 12,
            max_tool_calls: 48,
            max_tokens: 1_000_000,
            max_elapsed: Duration::from_secs(30 * 60),
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
            "{exhausted} {} (turns={}, tool_calls={}, tokens={}, elapsed_ms={})",
            self.limit,
            self.usage.turns,
            self.usage.tool_calls,
            self.usage.tokens,
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
    pub tokens: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug)]
pub struct BudgetUsage {
    turns: u32,
    tool_calls: u32,
    tokens: u64,
    started_at: Instant,
}

impl Default for BudgetUsage {
    fn default() -> Self {
        Self {
            turns: 0,
            tool_calls: 0,
            tokens: 0,
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
    pub fn record_turn(&mut self, tokens: u64) -> Result<(), BudgetError> {
        self.turns = self.turns.saturating_add(1);
        self.tokens = self.tokens.saturating_add(tokens);
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
        if self.turns >= budget.max_turns {
            return Err(BudgetError::Turns {
                limit: budget.max_turns,
            });
        }
        if self.tool_calls >= budget.max_tool_calls {
            return Err(BudgetError::ToolCalls {
                limit: budget.max_tool_calls,
            });
        }
        if self.tokens >= budget.max_tokens {
            return Err(BudgetError::Tokens {
                limit: budget.max_tokens,
            });
        }
        if self.started_at.elapsed() >= budget.max_elapsed {
            return Err(BudgetError::Elapsed {
                limit_ms: duration_millis(budget.max_elapsed),
            });
        }
        Ok(())
    }

    pub fn snapshot(&self) -> BudgetSnapshot {
        BudgetSnapshot {
            turns: self.turns,
            tool_calls: self.tool_calls,
            tokens: self.tokens,
            elapsed_ms: duration_millis(self.started_at.elapsed()),
        }
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
