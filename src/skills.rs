use std::collections::BTreeMap;

use thiserror::Error;

use crate::{
    model::{Message, Role},
    resources::{MAX_SKILL_NAME_BYTES, Skill},
};

const MAX_ACTIVE_SKILL_CONTEXT_BYTES: usize = 96 * 1_024;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SkillInvocationError {
    #[error("malformed skill invocation: {0}")]
    Malformed(String),
    #[error("unknown skill `{0}`; use the `get_commands` RPC command to list available skills")]
    Unknown(String),
    #[error("active skill instructions exceed {MAX_ACTIVE_SKILL_CONTEXT_BYTES} bytes")]
    Oversized,
}

#[derive(Debug, Clone, Default)]
pub struct SkillRuntime {
    skills: BTreeMap<String, Skill>,
}

impl SkillRuntime {
    #[must_use]
    pub fn new(skills: Vec<Skill>) -> Self {
        Self {
            skills: skills
                .into_iter()
                .map(|skill| (skill.name.clone(), skill))
                .collect(),
        }
    }

    /// Returns a bounded, ephemeral system-prompt fragment for skill invocations
    /// in the messages submitted for the current run.
    ///
    /// The skill body is not written to session history. Only the user's compact
    /// invocation remains there, so later unrelated turns do not inherit it.
    ///
    /// # Errors
    ///
    /// Returns a clear error for malformed or unknown skill invocations, or when
    /// the combined active instructions exceed the runtime bound.
    pub fn context_for_messages(
        &self,
        messages: &[Message],
    ) -> Result<Option<String>, SkillInvocationError> {
        let mut contexts = Vec::new();
        let mut total = 0_usize;
        for message in messages {
            if message.role != Role::User {
                continue;
            }
            let text = message.text();
            let Some((name, _arguments)) = parse_invocation(&text)? else {
                continue;
            };
            let skill = self
                .skills
                .get(name)
                .ok_or_else(|| SkillInvocationError::Unknown(name.to_owned()))?;
            let base_dir = skill.path.parent().unwrap_or(&skill.path).display();
            let context = format!(
                "<active_skill name=\"{}\" location=\"{}\">\nReferences are relative to {}. The activating user message contains any request arguments after the skill name. Do not install packages or execute referenced scripts automatically; inspect them and request the authority required by the active task.\n\n{}\n</active_skill>",
                skill.name,
                escape_xml(&skill.path.display().to_string()),
                escape_xml(&base_dir.to_string()),
                skill.body,
            );
            total = total.saturating_add(context.len());
            if total > MAX_ACTIVE_SKILL_CONTEXT_BYTES {
                return Err(SkillInvocationError::Oversized);
            }
            contexts.push(context);
        }
        Ok((!contexts.is_empty()).then(|| contexts.join("\n\n")))
    }

    #[must_use]
    pub fn command_names(&self) -> Vec<String> {
        self.skills
            .keys()
            .map(|name| format!("skill:{name}"))
            .collect()
    }
}

fn parse_invocation(input: &str) -> Result<Option<(&str, &str)>, SkillInvocationError> {
    let trimmed = input.trim();
    let rest = if let Some(rest) = trimmed.strip_prefix("/skill:") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("skill:") {
        rest
    } else {
        return Ok(None);
    };
    let split = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..split];
    validate_invocation_name(name)?;
    Ok(Some((name, rest[split..].trim())))
}

fn validate_invocation_name(name: &str) -> Result<(), SkillInvocationError> {
    if name.is_empty() {
        return Err(SkillInvocationError::Malformed(
            "expected /skill:<name> [arguments]".into(),
        ));
    }
    if name.len() > MAX_SKILL_NAME_BYTES
        || name.starts_with('-')
        || name.ends_with('-')
        || name.contains("--")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(SkillInvocationError::Malformed(format!(
            "invalid skill name `{name}`"
        )));
    }
    Ok(())
}

fn escape_xml(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }
    escaped
}
