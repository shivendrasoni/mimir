use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use chrono::{DateTime, Datelike, Duration as ChronoDuration, Timelike, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    atomic,
    error::{MimirError, Result},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleKind {
    #[default]
    Once,
    Cron,
    Interval,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleSource {
    #[default]
    Cron,
    Heartbeat,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeartbeatDeliveryMode {
    #[default]
    Steer,
    FollowUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatManagementAction {
    Pause,
    Resume,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub schema_version: u16,
    pub id: Uuid,
    pub name: String,
    pub session_id: String,
    pub prompt: String,
    pub next_run: DateTime<Utc>,
    pub every_seconds: Option<u64>,
    pub enabled: bool,
    pub last_run: Option<DateTime<Utc>>,
    #[serde(default)]
    pub schedule_kind: ScheduleKind,
    #[serde(default)]
    pub schedule_expression: String,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub run_count: u64,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub source: ScheduleSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_mode: Option<HeartbeatDeliveryMode>,
    #[serde(default)]
    pub paused: bool,
}

pub struct ScheduleStore {
    root: PathBuf,
    path: PathBuf,
    mutation_lock: Arc<tokio::sync::Mutex<()>>,
}

impl ScheduleStore {
    pub fn new(state_root: &Path) -> Self {
        let root = atomic::canonical_state_root(state_root);
        let path = root.join("schedules/schedules.json");
        Self {
            root,
            mutation_lock: atomic::path_lock(&path),
            path,
        }
    }

    /// Validates a reference-compatible textual schedule without mutating state.
    ///
    /// # Errors
    ///
    /// Returns the same parsing, recurrence, or overflow errors as [`Self::add_text`].
    pub fn validate_text(schedule: &str, now: DateTime<Utc>) -> Result<()> {
        parse_schedule_text(schedule, now).map(|_| ())
    }

    /// Validates and normalizes a heartbeat schedule without mutating state.
    ///
    /// # Errors
    ///
    /// Returns an error when the schedule is invalid or one-shot.
    pub fn validate_heartbeat_text(schedule: &str, now: DateTime<Utc>) -> Result<()> {
        let parsed = parse_schedule_text(&normalize_heartbeat_schedule(schedule), now)?;
        if parsed.kind == ScheduleKind::Once {
            return Err(MimirError::Configuration(
                "heartbeat schedule must be recurring".into(),
            ));
        }
        Ok(())
    }

    /// Adds a durable scheduled prompt.
    ///
    /// # Errors
    ///
    /// Returns an error for blank fields, zero recurrence, overflow, or persistence failure.
    pub async fn add(
        &self,
        name: &str,
        session_id: &str,
        prompt: &str,
        next_run: DateTime<Utc>,
        every: Option<Duration>,
    ) -> Result<Schedule> {
        let every_seconds = every.map(|duration| duration.as_secs());
        if every_seconds == Some(0) {
            return Err(MimirError::Configuration(
                "schedule interval must be positive".into(),
            ));
        }
        let kind = if every_seconds.is_some() {
            ScheduleKind::Interval
        } else {
            ScheduleKind::Once
        };
        let expression = every_seconds.map_or_else(
            || format!("at {}", next_run.to_rfc3339()),
            |seconds| format!("every {seconds}s"),
        );
        self.add_record(
            name,
            session_id,
            prompt,
            next_run,
            every_seconds,
            kind,
            expression,
            Utc::now(),
        )
        .await
    }

    /// Parses and adds a reference-compatible textual schedule.
    ///
    /// Supported forms are `in 10m`, `at <RFC3339>`, `every 15m`, aliases such
    /// as `@hourly`, and five-field minute-based cron expressions.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid schedules, unsafe recurrence frequency, blank
    /// fields, overflow, or persistence failure.
    pub async fn add_text(
        &self,
        name: &str,
        session_id: &str,
        prompt: &str,
        schedule: &str,
        now: DateTime<Utc>,
    ) -> Result<Schedule> {
        let parsed = parse_schedule_text(schedule, now)?;
        self.add_record(
            name,
            session_id,
            prompt,
            parsed.next_run,
            parsed.every_seconds,
            parsed.kind,
            parsed.expression,
            now,
        )
        .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the persisted schedule record makes every parsed contract field explicit"
    )]
    async fn add_record(
        &self,
        name: &str,
        session_id: &str,
        prompt: &str,
        next_run: DateTime<Utc>,
        every_seconds: Option<u64>,
        schedule_kind: ScheduleKind,
        schedule_expression: String,
        now: DateTime<Utc>,
    ) -> Result<Schedule> {
        if name.trim().is_empty() || session_id.trim().is_empty() || prompt.trim().is_empty() {
            return Err(MimirError::Configuration(
                "schedule name, session_id, and prompt must not be blank".into(),
            ));
        }
        if !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(MimirError::Configuration(
                "session id must contain only letters, digits, '-' or '_'".into(),
            ));
        }
        let schedule = Schedule {
            schema_version: 1,
            id: Uuid::new_v4(),
            name: name.trim().into(),
            session_id: session_id.trim().into(),
            prompt: prompt.trim().into(),
            next_run,
            every_seconds,
            enabled: true,
            last_run: None,
            schedule_kind,
            schedule_expression,
            created_at: Some(now),
            updated_at: Some(now),
            run_count: 0,
            cancelled: false,
            source: ScheduleSource::Cron,
            delivery_mode: None,
            paused: false,
        };
        let _guard = self.mutation_lock.lock().await;
        let mut schedules = self.list_unlocked().await?;
        schedules.push(schedule.clone());
        self.write_unlocked(&schedules).await?;
        Ok(schedule)
    }

    /// Loads all schedules.
    ///
    /// # Errors
    ///
    /// Returns a persistence or deserialization error.
    pub async fn list(&self) -> Result<Vec<Schedule>> {
        let mut schedules = self.list_unlocked().await?;
        schedules.sort_by(|left, right| {
            left.next_run
                .cmp(&right.next_run)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(schedules)
    }

    /// Creates or replaces the current heartbeat for a session.
    ///
    /// Heartbeats are recurring schedules. Replacing one preserves its delivery
    /// mode when the caller omits a new mode and cancels the previous active or
    /// paused record without deleting its audit history.
    ///
    /// # Errors
    ///
    /// Returns an error for a one-shot or invalid schedule, blank fields, or a
    /// persistence failure.
    #[allow(
        clippy::too_many_arguments,
        reason = "the heartbeat persistence boundary keeps every reference field explicit"
    )]
    pub async fn set_heartbeat(
        &self,
        name: &str,
        session_id: &str,
        prompt: &str,
        schedule: &str,
        delivery_mode: Option<HeartbeatDeliveryMode>,
        now: DateTime<Utc>,
    ) -> Result<Schedule> {
        let normalized = normalize_heartbeat_schedule(schedule);
        let parsed = parse_schedule_text(&normalized, now)?;
        if parsed.kind == ScheduleKind::Once {
            return Err(MimirError::Configuration(
                "heartbeat schedule must be recurring".into(),
            ));
        }
        validate_record_fields(name, session_id, prompt)?;

        let _guard = self.mutation_lock.lock().await;
        let mut schedules = self.list_unlocked().await?;
        let inherited_mode = schedules
            .iter()
            .filter(|candidate| is_current_heartbeat(candidate, session_id))
            .max_by_key(|candidate| heartbeat_updated_at(candidate))
            .and_then(|candidate| candidate.delivery_mode);
        for candidate in schedules
            .iter_mut()
            .filter(|candidate| is_current_heartbeat(candidate, session_id))
        {
            candidate.enabled = false;
            candidate.paused = false;
            candidate.cancelled = true;
            candidate.updated_at = Some(now);
        }
        let heartbeat = Schedule {
            schema_version: 1,
            id: Uuid::new_v4(),
            name: name.trim().into(),
            session_id: session_id.trim().into(),
            prompt: prompt.trim().into(),
            next_run: parsed.next_run,
            every_seconds: parsed.every_seconds,
            enabled: true,
            last_run: None,
            schedule_kind: parsed.kind,
            schedule_expression: parsed.expression,
            created_at: Some(now),
            updated_at: Some(now),
            run_count: 0,
            cancelled: false,
            source: ScheduleSource::Heartbeat,
            delivery_mode: delivery_mode
                .or(inherited_mode)
                .or(Some(HeartbeatDeliveryMode::Steer)),
            paused: false,
        };
        schedules.push(heartbeat.clone());
        self.write_unlocked(&schedules).await?;
        Ok(heartbeat)
    }

    /// Returns the newest active or paused heartbeat for a session.
    ///
    /// # Errors
    ///
    /// Returns a persistence or deserialization error.
    pub async fn get_heartbeat(&self, session_id: &str) -> Result<Option<Schedule>> {
        Ok(self
            .list_unlocked()
            .await?
            .into_iter()
            .filter(|candidate| is_current_heartbeat(candidate, session_id))
            .max_by_key(heartbeat_updated_at))
    }

    /// Lists all active and paused heartbeats.
    ///
    /// # Errors
    ///
    /// Returns a persistence or deserialization error.
    pub async fn list_heartbeats(&self) -> Result<Vec<Schedule>> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .filter(|candidate| {
                candidate.source == ScheduleSource::Heartbeat
                    && (candidate.enabled || candidate.paused)
            })
            .collect())
    }

    /// Pauses the current heartbeat for `session_id`.
    ///
    /// # Errors
    ///
    /// Returns a recurrence or persistence error.
    pub async fn pause_heartbeat(
        &self,
        session_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Schedule>> {
        self.update_current_heartbeat(session_id, HeartbeatManagementAction::Pause, now)
            .await
    }

    /// Resumes the current heartbeat for `session_id` from a fresh next-run time.
    ///
    /// # Errors
    ///
    /// Returns a recurrence or persistence error.
    pub async fn resume_heartbeat(
        &self,
        session_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Schedule>> {
        self.update_current_heartbeat(session_id, HeartbeatManagementAction::Resume, now)
            .await
    }

    /// Cancels the current heartbeat for `session_id`.
    ///
    /// # Errors
    ///
    /// Returns a persistence error.
    pub async fn clear_heartbeat(
        &self,
        session_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<Schedule>> {
        self.update_current_heartbeat(session_id, HeartbeatManagementAction::Stop, now)
            .await
    }

    /// Applies a lifecycle action to a specifically addressed heartbeat.
    ///
    /// Returns `None` when the id/session pair is unknown or the heartbeat is no
    /// longer active or paused.
    ///
    /// # Errors
    ///
    /// Returns a recurrence or persistence error.
    pub async fn manage_heartbeat(
        &self,
        session_id: &str,
        id: Uuid,
        action: HeartbeatManagementAction,
        now: DateTime<Utc>,
    ) -> Result<Option<Schedule>> {
        let _guard = self.mutation_lock.lock().await;
        let mut schedules = self.list_unlocked().await?;
        let Some(index) = schedules.iter().position(|candidate| {
            candidate.id == id && is_current_heartbeat(candidate, session_id)
        }) else {
            return Ok(None);
        };
        apply_heartbeat_action(&mut schedules[index], action, now)?;
        let updated = schedules[index].clone();
        self.write_unlocked(&schedules).await?;
        Ok(Some(updated))
    }

    async fn update_current_heartbeat(
        &self,
        session_id: &str,
        action: HeartbeatManagementAction,
        now: DateTime<Utc>,
    ) -> Result<Option<Schedule>> {
        let _guard = self.mutation_lock.lock().await;
        let mut schedules = self.list_unlocked().await?;
        let Some((index, _)) = schedules
            .iter()
            .enumerate()
            .filter(|(_, candidate)| is_current_heartbeat(candidate, session_id))
            .max_by_key(|(_, candidate)| heartbeat_updated_at(candidate))
        else {
            return Ok(None);
        };
        apply_heartbeat_action(&mut schedules[index], action, now)?;
        let updated = schedules[index].clone();
        self.write_unlocked(&schedules).await?;
        Ok(Some(updated))
    }

    /// Returns enabled schedules due at or before `now`.
    ///
    /// # Errors
    ///
    /// Returns a persistence or deserialization error.
    pub async fn due(&self, now: DateTime<Utc>) -> Result<Vec<Schedule>> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .filter(|schedule| schedule.enabled && schedule.next_run <= now)
            .collect())
    }

    /// Records a schedule run and advances or disables it.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown id, interval overflow, or persistence failure.
    pub async fn mark_run(&self, id: Uuid) -> Result<Schedule> {
        let _guard = self.mutation_lock.lock().await;
        let mut schedules = self.list_unlocked().await?;
        let schedule = schedules
            .iter_mut()
            .find(|schedule| schedule.id == id)
            .ok_or_else(|| MimirError::Configuration(format!("unknown schedule {id}")))?;
        let now = Utc::now();
        schedule.last_run = Some(now);
        schedule.updated_at = Some(now);
        schedule.run_count = schedule.run_count.saturating_add(1);
        if let Some(seconds) = schedule.every_seconds {
            let seconds = i64::try_from(seconds)
                .map_err(|_| MimirError::Configuration("schedule interval is too large".into()))?;
            schedule.next_run = now + ChronoDuration::seconds(seconds);
        } else if schedule.schedule_kind == ScheduleKind::Cron {
            schedule.next_run = next_cron_run_after(&schedule.schedule_expression, now)?;
        } else {
            schedule.enabled = false;
        }
        let updated = schedule.clone();
        self.write_unlocked(&schedules).await?;
        Ok(updated)
    }

    /// Disables a schedule without deleting its audit state.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown id or persistence failure.
    pub async fn cancel(&self, id: Uuid) -> Result<Schedule> {
        let _guard = self.mutation_lock.lock().await;
        let mut schedules = self.list_unlocked().await?;
        let schedule = schedules
            .iter_mut()
            .find(|schedule| schedule.id == id)
            .ok_or_else(|| MimirError::Configuration(format!("unknown schedule {id}")))?;
        schedule.enabled = false;
        schedule.cancelled = true;
        schedule.paused = false;
        schedule.updated_at = Some(Utc::now());
        let updated = schedule.clone();
        self.write_unlocked(&schedules).await?;
        Ok(updated)
    }

    async fn list_unlocked(&self) -> Result<Vec<Schedule>> {
        tokio::fs::create_dir_all(&self.root).await?;
        atomic::prepare_state_path(&self.root, &self.path).await?;
        Ok(atomic::read_json(&self.path).await?.unwrap_or_default())
    }

    async fn write_unlocked(&self, schedules: &[Schedule]) -> Result<()> {
        tokio::fs::create_dir_all(&self.root).await?;
        atomic::prepare_state_path(&self.root, &self.path).await?;
        atomic::write_json(&self.path, schedules).await
    }
}

fn validate_record_fields(name: &str, session_id: &str, prompt: &str) -> Result<()> {
    if name.trim().is_empty() || session_id.trim().is_empty() || prompt.trim().is_empty() {
        return Err(MimirError::Configuration(
            "schedule name, session_id, and prompt must not be blank".into(),
        ));
    }
    if !session_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(MimirError::Configuration(
            "session id must contain only letters, digits, '-' or '_'".into(),
        ));
    }
    Ok(())
}

fn is_current_heartbeat(schedule: &Schedule, session_id: &str) -> bool {
    schedule.session_id == session_id
        && schedule.source == ScheduleSource::Heartbeat
        && (schedule.enabled || schedule.paused)
}

fn heartbeat_updated_at(schedule: &Schedule) -> DateTime<Utc> {
    schedule
        .updated_at
        .or(schedule.created_at)
        .unwrap_or(schedule.next_run)
}

fn apply_heartbeat_action(
    schedule: &mut Schedule,
    action: HeartbeatManagementAction,
    now: DateTime<Utc>,
) -> Result<()> {
    match action {
        HeartbeatManagementAction::Pause => {
            schedule.enabled = false;
            schedule.paused = true;
            schedule.cancelled = false;
        }
        HeartbeatManagementAction::Resume => {
            schedule.next_run = next_run_for_recurring_schedule(schedule, now)?;
            schedule.enabled = true;
            schedule.paused = false;
            schedule.cancelled = false;
        }
        HeartbeatManagementAction::Stop => {
            schedule.enabled = false;
            schedule.paused = false;
            schedule.cancelled = true;
        }
    }
    schedule.updated_at = Some(now);
    Ok(())
}

fn next_run_for_recurring_schedule(
    schedule: &Schedule,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    if let Some(seconds) = schedule.every_seconds {
        return add_seconds(now, seconds);
    }
    if schedule.schedule_kind == ScheduleKind::Cron {
        return next_cron_run_after(&schedule.schedule_expression, now);
    }
    Err(MimirError::Configuration(
        "heartbeat schedule must be recurring".into(),
    ))
}

fn normalize_heartbeat_schedule(input: &str) -> String {
    let text = input.trim();
    if text.is_empty() {
        return "every 5m".into();
    }
    let digit_count = text.bytes().take_while(u8::is_ascii_digit).count();
    if digit_count > 0 {
        let unit = text[digit_count..].trim().to_ascii_lowercase();
        if matches!(
            unit.as_str(),
            "s" | "sec"
                | "secs"
                | "second"
                | "seconds"
                | "m"
                | "min"
                | "mins"
                | "minute"
                | "minutes"
                | "h"
                | "hr"
                | "hrs"
                | "hour"
                | "hours"
        ) {
            return format!("every {text}");
        }
    }
    text.into()
}

struct ParsedSchedule {
    kind: ScheduleKind,
    expression: String,
    next_run: DateTime<Utc>,
    every_seconds: Option<u64>,
}

fn parse_schedule_text(input: &str, now: DateTime<Utc>) -> Result<ParsedSchedule> {
    let text = strip_matching_quotes(input.trim());
    if text.is_empty() {
        return Err(MimirError::Configuration(
            "cron schedule cannot be empty".into(),
        ));
    }
    if let Some((amount, unit)) = parse_relative(&text, "in")? {
        let seconds = duration_seconds(amount, unit)?;
        return Ok(ParsedSchedule {
            kind: ScheduleKind::Once,
            expression: text,
            next_run: add_seconds(now, seconds)?,
            every_seconds: None,
        });
    }
    if let Some((amount, unit)) = parse_relative(&text, "every")?.or(parse_relative(&text, "each")?)
    {
        let seconds = duration_seconds(amount, unit)?;
        if seconds < 10 {
            return Err(MimirError::Configuration(
                "recurring interval must be at least 10 seconds".into(),
            ));
        }
        return Ok(ParsedSchedule {
            kind: ScheduleKind::Interval,
            expression: text,
            next_run: add_seconds(now, seconds)?,
            every_seconds: Some(seconds),
        });
    }
    if text
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("at "))
    {
        let next_run = DateTime::parse_from_rfc3339(text[3..].trim())
            .map(|value| value.with_timezone(&Utc))
            .map_err(|_| {
                MimirError::Configuration("invalid one-shot schedule; use: at <ISO date>".into())
            })?;
        if next_run <= now {
            return Err(MimirError::Configuration(
                "one-shot schedule must be in the future".into(),
            ));
        }
        return Ok(ParsedSchedule {
            kind: ScheduleKind::Once,
            expression: text,
            next_run,
            every_seconds: None,
        });
    }
    let expression = normalize_cron_alias(&text).to_owned();
    Ok(ParsedSchedule {
        kind: ScheduleKind::Cron,
        next_run: next_cron_run_after(&expression, now)?,
        expression,
        every_seconds: None,
    })
}

fn strip_matching_quotes(value: &str) -> String {
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_owned()
    } else {
        value.to_owned()
    }
}

fn parse_relative<'a>(text: &'a str, prefix: &str) -> Result<Option<(u64, &'a str)>> {
    let fields = text.split_whitespace().collect::<Vec<_>>();
    if fields
        .first()
        .is_none_or(|field| !field.eq_ignore_ascii_case(prefix))
    {
        return Ok(None);
    }
    let (amount, unit) = if fields.len() == 2 {
        let split = fields[1]
            .find(|character: char| !character.is_ascii_digit())
            .ok_or_else(|| MimirError::Configuration(format!("invalid {prefix} schedule")))?;
        (&fields[1][..split], &fields[1][split..])
    } else if fields.len() == 3 {
        (fields[1], fields[2])
    } else {
        return Err(MimirError::Configuration(format!(
            "invalid {prefix} schedule"
        )));
    };
    let amount = amount
        .parse::<u64>()
        .map_err(|_| MimirError::Configuration(format!("invalid schedule amount: {amount}")))?;
    if unit.is_empty() {
        return Err(MimirError::Configuration(format!(
            "invalid {prefix} schedule"
        )));
    }
    Ok(Some((amount, unit)))
}

fn duration_seconds(amount: u64, unit: &str) -> Result<u64> {
    let unit = unit.to_ascii_lowercase();
    let multiplier = if matches!(unit.as_str(), "s" | "sec" | "secs" | "second" | "seconds") {
        1
    } else if matches!(unit.as_str(), "m" | "min" | "mins" | "minute" | "minutes") {
        60
    } else if matches!(unit.as_str(), "h" | "hr" | "hrs" | "hour" | "hours") {
        60 * 60
    } else if matches!(unit.as_str(), "d" | "day" | "days") {
        24 * 60 * 60
    } else {
        return Err(MimirError::Configuration(format!(
            "unsupported schedule unit: {unit}"
        )));
    };
    amount
        .checked_mul(multiplier)
        .ok_or_else(|| MimirError::Configuration("schedule duration is too large".into()))
}

fn add_seconds(now: DateTime<Utc>, seconds: u64) -> Result<DateTime<Utc>> {
    let seconds = i64::try_from(seconds)
        .map_err(|_| MimirError::Configuration("schedule duration is too large".into()))?;
    now.checked_add_signed(ChronoDuration::seconds(seconds))
        .ok_or_else(|| MimirError::Configuration("schedule timestamp overflowed".into()))
}

fn normalize_cron_alias(text: &str) -> &str {
    match text {
        "@hourly" => "0 * * * *",
        "@daily" => "0 0 * * *",
        "@weekly" => "0 0 * * 0",
        "@monthly" => "0 0 1 * *",
        _ => text,
    }
}

fn next_cron_run_after(expression: &str, after: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let fields = parse_cron_expression(expression)?;
    let mut candidate = after
        .checked_add_signed(ChronoDuration::minutes(1))
        .and_then(|value| value.with_second(0))
        .and_then(|value| value.with_nanosecond(0))
        .ok_or_else(|| MimirError::Configuration("cron timestamp overflowed".into()))?;
    let deadline = candidate
        .checked_add_signed(ChronoDuration::days(366))
        .ok_or_else(|| MimirError::Configuration("cron timestamp overflowed".into()))?;
    while candidate <= deadline {
        if cron_matches(candidate, &fields) {
            return Ok(candidate);
        }
        candidate = candidate
            .checked_add_signed(ChronoDuration::minutes(1))
            .ok_or_else(|| MimirError::Configuration("cron timestamp overflowed".into()))?;
    }
    Err(MimirError::Configuration(format!(
        "cron schedule did not match within one year: {expression}"
    )))
}

struct CronFields {
    minute: BTreeSet<u32>,
    hour: BTreeSet<u32>,
    day_of_month: BTreeSet<u32>,
    month: BTreeSet<u32>,
    day_of_week: BTreeSet<u32>,
}

fn parse_cron_expression(expression: &str) -> Result<CronFields> {
    let fields = expression.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 5 {
        return Err(MimirError::Configuration(
            "unsupported cron schedule; use 'in 10m', 'at <ISO date>', @hourly, or five fields: minute hour day month weekday".into(),
        ));
    }
    Ok(CronFields {
        minute: parse_cron_field(fields[0], 0, 59)?,
        hour: parse_cron_field(fields[1], 0, 23)?,
        day_of_month: parse_cron_field(fields[2], 1, 31)?,
        month: parse_cron_field(fields[3], 1, 12)?,
        day_of_week: parse_cron_field(fields[4], 0, 7)?,
    })
}

fn parse_cron_field(field: &str, minimum: u32, maximum: u32) -> Result<BTreeSet<u32>> {
    let mut values = BTreeSet::new();
    for part in field.split(',') {
        if part.is_empty() {
            return Err(MimirError::Configuration(format!(
                "invalid cron field: {field}"
            )));
        }
        let (range, step) = part.split_once('/').map_or((part, 1), |(range, step)| {
            (range, parse_cron_number(step, 1, maximum).unwrap_or(0))
        });
        if step == 0 {
            return Err(MimirError::Configuration(format!(
                "invalid cron step in field: {field}"
            )));
        }
        let (start, end) = if range == "*" {
            (minimum, maximum)
        } else if let Some((start, end)) = range.split_once('-') {
            let start = parse_cron_number(start, minimum, maximum)?;
            let end = parse_cron_number(end, minimum, maximum)?;
            if start > end {
                return Err(MimirError::Configuration(format!(
                    "invalid cron range: {range}"
                )));
            }
            (start, end)
        } else {
            let value = parse_cron_number(range, minimum, maximum)?;
            (value, value)
        };
        let mut value = start;
        while value <= end {
            values.insert(value);
            value = value.saturating_add(step);
            if value == u32::MAX {
                break;
            }
        }
    }
    Ok(values)
}

fn parse_cron_number(value: &str, minimum: u32, maximum: u32) -> Result<u32> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_| MimirError::Configuration(format!("invalid cron number: {value}")))?;
    if !(minimum..=maximum).contains(&parsed) {
        return Err(MimirError::Configuration(format!(
            "cron number out of range: {value}"
        )));
    }
    Ok(parsed)
}

fn cron_matches(candidate: DateTime<Utc>, fields: &CronFields) -> bool {
    let weekday = candidate.weekday().num_days_from_sunday();
    let weekday_matches =
        fields.day_of_week.contains(&weekday) || (weekday == 0 && fields.day_of_week.contains(&7));
    fields.minute.contains(&candidate.minute())
        && fields.hour.contains(&candidate.hour())
        && fields.day_of_month.contains(&candidate.day())
        && fields.month.contains(&candidate.month())
        && weekday_matches
}
