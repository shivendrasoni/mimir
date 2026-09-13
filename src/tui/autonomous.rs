use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use crate::{
    error::{MimirError, Result},
    model::Usage,
};

pub const DEFAULT_AUTONOMOUS_CONTINUATION_PROMPT: &str = "No human input is available in autonomous mode. Continue working until the task is genuinely complete. When the work is complete and validated, call finish_task with a concise summary and any workspace-relative artifacts. If you were asking the user a question, make a reasonable assumption and verify it. If blocked, preserve host-observable evidence and keep looking for safe progress while budget remains.";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AutonomousLimits {
    pub max_continuations: Option<u32>,
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u64>,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutonomousGateCommand {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessExecutionPolicy {
    pub allow_autonomous_quality_gates: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutonomousLimitReason {
    MaxContinuations,
    MaxTurns,
    MaxTokens,
    Timeout,
}

impl AutonomousLimitReason {
    const fn label(self) -> &'static str {
        match self {
            Self::MaxContinuations => "continuation limit",
            Self::MaxTurns => "turn limit",
            Self::MaxTokens => "token limit",
            Self::Timeout => "time limit",
        }
    }
}

#[derive(Debug)]
pub struct AutonomousState {
    enabled: bool,
    generation: u64,
    continuations_used: u32,
    turns_used: u32,
    tokens_used: u64,
    started_at: Option<Instant>,
    limits: AutonomousLimits,
    continuation_prompt: String,
    quality_gates: Vec<AutonomousGateCommand>,
}

impl Default for AutonomousState {
    fn default() -> Self {
        Self {
            enabled: false,
            generation: 0,
            continuations_used: 0,
            turns_used: 0,
            tokens_used: 0,
            started_at: None,
            limits: AutonomousLimits::default(),
            continuation_prompt: DEFAULT_AUTONOMOUS_CONTINUATION_PROMPT.into(),
            quality_gates: Vec::new(),
        }
    }
}

impl AutonomousState {
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub const fn limits(&self) -> AutonomousLimits {
        self.limits
    }

    #[must_use]
    pub const fn continuations_used(&self) -> u32 {
        self.continuations_used
    }

    #[must_use]
    pub const fn turns_used(&self) -> u32 {
        self.turns_used
    }

    #[must_use]
    pub const fn tokens_used(&self) -> u64 {
        self.tokens_used
    }

    #[must_use]
    pub fn quality_gate_count(&self) -> usize {
        self.quality_gates.len()
    }

    pub fn set_limits(&mut self, limits: AutonomousLimits) {
        self.limits = limits;
    }

    pub fn enable(&mut self, now: Instant) {
        self.enabled = true;
        self.generation = self.generation.wrapping_add(1);
        self.continuations_used = 0;
        self.turns_used = 0;
        self.tokens_used = 0;
        self.started_at = Some(now);
    }

    pub fn disable(&mut self) {
        self.enabled = false;
        self.cancel_current();
        self.started_at = None;
    }

    /// Invalidates the active loop without changing whether the next user turn
    /// should start in autonomous mode.
    pub fn cancel_current(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    #[must_use]
    pub fn begin_run(&mut self, now: Instant) -> Option<u64> {
        if !self.enabled {
            return None;
        }
        self.generation = self.generation.wrapping_add(1);
        self.continuations_used = 0;
        self.turns_used = 0;
        self.tokens_used = 0;
        self.started_at = Some(now);
        Some(self.generation)
    }

    pub fn record_turn(&mut self, generation: u64, usage: Usage) {
        if !self.enabled || self.generation != generation {
            return;
        }
        self.turns_used = self.turns_used.saturating_add(1);
        self.tokens_used = self.tokens_used.saturating_add(usage.budget_tokens());
    }

    #[must_use]
    pub fn next_continuation(&mut self, generation: u64, now: Instant) -> Option<String> {
        if !self.enabled || self.generation != generation || self.limit_reason(now).is_some() {
            return None;
        }
        self.continuations_used = self.continuations_used.saturating_add(1);
        Some(self.continuation_prompt.clone())
    }

    #[must_use]
    pub fn limit_reason(&self, now: Instant) -> Option<AutonomousLimitReason> {
        if self
            .limits
            .max_continuations
            .is_some_and(|limit| self.continuations_used >= limit)
        {
            return Some(AutonomousLimitReason::MaxContinuations);
        }
        if self
            .limits
            .max_turns
            .is_some_and(|limit| self.turns_used >= limit)
        {
            return Some(AutonomousLimitReason::MaxTurns);
        }
        if self
            .limits
            .max_tokens
            .is_some_and(|limit| self.tokens_used >= limit)
        {
            return Some(AutonomousLimitReason::MaxTokens);
        }
        if self.started_at.is_some_and(|started| {
            self.limits
                .timeout
                .is_some_and(|limit| now.saturating_duration_since(started) >= limit)
        }) {
            return Some(AutonomousLimitReason::Timeout);
        }
        None
    }

    #[must_use]
    pub fn status(&self, now: Instant) -> String {
        let state = if self.enabled { "on" } else { "off" };
        let limit = self
            .limit_reason(now)
            .map_or("within limits", AutonomousLimitReason::label);
        format!(
            "Autonomous {state}: {}/{} continuations, {}/{} turns, {}/{} tokens ({limit}); quality gates: {}",
            self.continuations_used,
            display_limit(self.limits.max_continuations),
            self.turns_used,
            display_limit(self.limits.max_turns),
            self.tokens_used,
            display_limit(self.limits.max_tokens),
            if self.quality_gates.is_empty() {
                "disabled"
            } else {
                "configured"
            }
        )
    }

    /// Installs optional process gates only when the outer tool policy grants
    /// that authority. Paths must be absolute and commands remain shell-free.
    ///
    /// # Errors
    ///
    /// Rejects implicit process authority, relative executables, or unbounded
    /// gate catalogs.
    pub fn configure_quality_gates(
        &mut self,
        commands: Vec<AutonomousGateCommand>,
        policy: ProcessExecutionPolicy,
    ) -> Result<()> {
        if !commands.is_empty() && !policy.allow_autonomous_quality_gates {
            return Err(MimirError::Configuration(
                "autonomous quality gates require explicit process policy permission".into(),
            ));
        }
        if commands.len() > 8 {
            return Err(MimirError::Configuration(
                "autonomous quality gates are limited to 8 commands".into(),
            ));
        }
        if commands.iter().any(|command| {
            !command.executable.is_absolute()
                || command.arguments.len() > 64
                || command
                    .arguments
                    .iter()
                    .any(|argument| argument.len() > 4096)
        }) {
            return Err(MimirError::Configuration(
                "autonomous quality gates require absolute executables and bounded arguments"
                    .into(),
            ));
        }
        self.quality_gates = commands;
        Ok(())
    }
}

fn display_limit<T: std::fmt::Display>(limit: Option<T>) -> String {
    limit.map_or_else(|| "unlimited".into(), |value| value.to_string())
}

pub(super) fn apply_autonomous_command(
    state: &mut AutonomousState,
    arguments: Option<&str>,
    now: Instant,
) -> Result<(String, bool)> {
    match arguments.map(str::trim).filter(|value| !value.is_empty()) {
        None | Some("status") => Ok((state.status(now), false)),
        Some("on") => {
            state.enable(now);
            Ok((state.status(now), false))
        }
        Some("off") => {
            state.disable();
            Ok((state.status(now), true))
        }
        Some("cancel") => {
            state.cancel_current();
            Ok((
                "Cancelled the active autonomous continuation loop".into(),
                true,
            ))
        }
        Some(_) => Err(MimirError::Configuration(
            "Usage: /autonomous [on|off|status|cancel]".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_limits_stop_continuations_and_count_only_new_tokens() {
        let now = Instant::now();
        let mut state = AutonomousState::default();
        state.set_limits(AutonomousLimits {
            max_continuations: Some(3),
            max_tokens: Some(80_000),
            ..AutonomousLimits::default()
        });
        state.enable(now);
        let generation = state.begin_run(now).expect("enabled");
        state.record_turn(
            generation,
            Usage {
                input_tokens: 50_010,
                output_tokens: 5,
                cached_tokens: 50_000,
            },
        );
        for _ in 0..state.limits.max_continuations.expect("bounded") {
            assert!(state.next_continuation(generation, now).is_some());
        }
        assert_eq!(
            state.limit_reason(now),
            Some(AutonomousLimitReason::MaxContinuations)
        );
        assert!(state.status(now).contains("15/80000 tokens"));
    }

    #[test]
    fn default_limits_are_unrestricted() {
        let now = Instant::now();
        let mut state = AutonomousState::default();
        state.enable(now);
        let generation = state.begin_run(now).expect("enabled");
        for _ in 0..100 {
            state.record_turn(
                generation,
                Usage {
                    input_tokens: 10_000,
                    output_tokens: 1_000,
                    cached_tokens: 0,
                },
            );
            assert!(state.next_continuation(generation, now).is_some());
        }
        assert_eq!(state.limit_reason(now), None);
        assert!(state.status(now).contains("/unlimited"));
    }

    #[test]
    fn cancellation_generation_fences_an_in_flight_loop() {
        let now = Instant::now();
        let mut state = AutonomousState::default();
        state.enable(now);
        let generation = state.begin_run(now).expect("enabled");
        state.cancel_current();
        assert!(state.next_continuation(generation, now).is_none());
        assert!(state.begin_run(now).is_some());
    }

    #[test]
    fn each_independent_user_task_starts_fresh_aggregate_accounting() {
        let now = Instant::now();
        let mut state = AutonomousState::default();
        state.enable(now);
        let first = state.begin_run(now).expect("enabled");
        state.record_turn(
            first,
            Usage {
                input_tokens: 100,
                output_tokens: 10,
                cached_tokens: 0,
            },
        );
        assert!(state.next_continuation(first, now).is_some());
        assert_eq!(state.turns_used(), 1);
        assert_eq!(state.continuations_used(), 1);

        let second = state.begin_run(now).expect("next task");
        assert_ne!(first, second);
        assert_eq!(state.turns_used(), 0);
        assert_eq!(state.continuations_used(), 0);
        assert_eq!(state.tokens_used(), 0);
    }

    #[test]
    fn quality_gates_fail_closed_without_explicit_process_policy() {
        let mut state = AutonomousState::default();
        let command = AutonomousGateCommand {
            executable: PathBuf::from("/usr/bin/true"),
            arguments: Vec::new(),
        };
        let error = state
            .configure_quality_gates(vec![command.clone()], ProcessExecutionPolicy::default())
            .expect_err("implicit process authority");
        assert!(error.to_string().contains("explicit process policy"));
        state
            .configure_quality_gates(
                vec![command],
                ProcessExecutionPolicy {
                    allow_autonomous_quality_gates: true,
                },
            )
            .expect("explicit policy");
    }
}
