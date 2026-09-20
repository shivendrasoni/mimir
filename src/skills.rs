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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillSummary {
    pub name: String,
    pub description: String,
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
            let context = render_skill_context(skill);
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

    #[must_use]
    pub(crate) fn summaries(&self) -> Vec<SkillSummary> {
        self.skills
            .values()
            .map(|skill| SkillSummary {
                name: skill.name.clone(),
                description: skill.description.clone(),
            })
            .collect()
    }

    /// Adds one exact skill to the ephemeral context for the current run.
    /// Returns `false` when that skill is already active.
    pub(crate) fn activate(
        &self,
        active_context: &mut Option<String>,
        name: &str,
    ) -> Result<bool, SkillInvocationError> {
        let skill = self
            .skills
            .get(name)
            .ok_or_else(|| SkillInvocationError::Unknown(name.to_owned()))?;
        let marker = format!("<active_skill name=\"{}\"", escape_xml(name));
        if active_context
            .as_deref()
            .is_some_and(|context| context.contains(&marker))
        {
            return Ok(false);
        }
        let context = render_skill_context(skill);
        let current_len = active_context.as_ref().map_or(0, String::len);
        let separator_len = usize::from(active_context.is_some()) * 2;
        if current_len
            .saturating_add(separator_len)
            .saturating_add(context.len())
            > MAX_ACTIVE_SKILL_CONTEXT_BYTES
        {
            return Err(SkillInvocationError::Oversized);
        }
        match active_context {
            Some(active) => {
                active.push_str("\n\n");
                active.push_str(&context);
            }
            None => *active_context = Some(context),
        }
        Ok(true)
    }
}

/// Ranks the bounded skill catalog with Mimir's current lexical discovery
/// heuristic. The result is deterministic and contains only matching skills.
#[must_use]
pub(crate) fn rank_skill_summaries<'a>(
    query: &str,
    skills: &'a [SkillSummary],
    limit: usize,
) -> Vec<&'a SkillSummary> {
    let mut matches = skills
        .iter()
        .filter_map(|skill| {
            let score = relevance_score(query, skill);
            (score > 0).then_some((score, skill))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.name.cmp(&right.name))
    });
    matches.truncate(limit);
    matches.into_iter().map(|(_, skill)| skill).collect()
}

fn relevance_score(query: &str, skill: &SkillSummary) -> u32 {
    let query = query.trim().to_ascii_lowercase();
    let name = skill.name.to_ascii_lowercase();
    let description = skill.description.to_ascii_lowercase();
    if query == name {
        return 10_000;
    }
    let query_terms = terms(&query);
    if query_terms.is_empty() {
        return 0;
    }
    let name_terms = terms(&name);
    let description_terms = terms(&description);
    let mut score = u32::from(description.contains(&query)) * 80;
    let mut matched_terms = 0_u32;
    for term in &query_terms {
        let term_score = if name_terms.contains(term) {
            40
        } else if name.contains(term) {
            24
        } else if description_terms.contains(term) {
            12
        } else if term.len() >= 4 && description.contains(term) {
            4
        } else {
            0
        };
        if term_score > 0 {
            matched_terms += 1;
            score += term_score;
        }
    }
    if usize::try_from(matched_terms).ok() == Some(query_terms.len()) {
        score += 20;
    }
    score
}

fn terms(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|term| term.len() > 1)
        .map(str::to_owned)
        .collect()
}

fn render_skill_context(skill: &Skill) -> String {
    let base_dir = skill.path.parent().unwrap_or(&skill.path).display();
    format!(
        "<active_skill name=\"{}\" location=\"{}\">\nReferences are relative to {}. The current user request supplies the task context; for an explicit skill invocation, text after the skill name supplies its arguments. Do not install packages or execute referenced scripts automatically; inspect them and request the authority required by the active task.\n\n{}\n</active_skill>",
        skill.name,
        escape_xml(&skill.path.display().to_string()),
        escape_xml(&base_dir.to_string()),
        skill.body,
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_exact_names_and_description_matches_deterministically() {
        let brainstorming = SkillSummary {
            name: "brainstorming".into(),
            description: "Explore product ideas before implementation".into(),
        };
        let api = SkillSummary {
            name: "api-design".into(),
            description: "Design stable service interfaces".into(),
        };
        assert_eq!(
            rank_skill_summaries("brainstorming", &[brainstorming.clone(), api.clone()], 1)[0].name,
            "brainstorming"
        );
        assert_eq!(
            rank_skill_summaries("product ideas", &[brainstorming, api.clone()], 1)[0].name,
            "brainstorming"
        );
        assert!(rank_skill_summaries("unrelated quantum gardening", &[api], 1).is_empty());
    }
}
