use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{model::ToolDefinition, skills::SkillSummary};

use super::{ObservationStatus, Tool, ToolError, ToolObservation, object_schema, parse_input};

const DEFAULT_RESULT_LIMIT: usize = 5;
const MAX_RESULT_LIMIT: usize = 10;
const MAX_QUERY_BYTES: usize = 512;

pub(super) struct SearchSkillsTool {
    skills: Vec<SkillSummary>,
}

impl SearchSkillsTool {
    pub(super) fn new(skills: Vec<SkillSummary>) -> Self {
        Self { skills }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchSkillsInput {
    query: Option<String>,
    name: Option<String>,
    #[serde(default = "default_result_limit")]
    limit: usize,
}

fn default_result_limit() -> usize {
    DEFAULT_RESULT_LIMIT
}

#[async_trait]
impl Tool for SearchSkillsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "search_skills".into(),
            description: "Discover an applicable skill without loading the full skill catalog. Use query when the task may benefit from specialized instructions and you do not know the exact skill. Results contain only bounded names and descriptions. If the user names a skill or a result is clearly applicable, call this tool with its exact name to activate that skill for the current run; only then are its full instructions added ephemerally to system context. Do not activate a merely plausible skill without checking that its description fits the task."
                .into(),
            parameters: object_schema(
                &json!({
                    "query": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_QUERY_BYTES,
                        "description": "Natural-language task, intent, or keywords to search for"
                    },
                    "name": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Exact skill name to activate after discovery or when explicitly named by the user"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_RESULT_LIMIT,
                        "default": DEFAULT_RESULT_LIMIT
                    }
                }),
                &[],
            ),
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolObservation, ToolError> {
        let input: SearchSkillsInput = parse_input("search_skills", input)?;
        if input.query.is_some() == input.name.is_some() {
            return Err(ToolError::InvalidArguments {
                tool: "search_skills".into(),
                message: "provide exactly one of query or name".into(),
            });
        }
        if !(1..=MAX_RESULT_LIMIT).contains(&input.limit) {
            return Err(ToolError::InvalidArguments {
                tool: "search_skills".into(),
                message: format!("limit must be from 1 to {MAX_RESULT_LIMIT}"),
            });
        }
        if let Some(name) = input.name {
            let Some(skill) = self.skills.iter().find(|skill| skill.name == name) else {
                return Err(ToolError::InvalidArguments {
                    tool: "search_skills".into(),
                    message: format!("unknown skill `{name}`; search by query before retrying"),
                });
            };
            return Ok(ToolObservation {
                status: ObservationStatus::Success,
                summary: format!("activated skill `{}` for this run", skill.name),
                next_actions: vec![
                    "Follow the activated skill instructions while completing the user's task"
                        .into(),
                ],
                artifacts: Vec::new(),
                content: json!({
                    "activated": skill.name,
                    "description": skill.description,
                    "ephemeral": true
                })
                .to_string(),
            });
        }

        let query = input.query.expect("query/name exclusivity checked");
        if query.trim().is_empty() || query.len() > MAX_QUERY_BYTES {
            return Err(ToolError::InvalidArguments {
                tool: "search_skills".into(),
                message: format!("query must contain 1 to {MAX_QUERY_BYTES} bytes"),
            });
        }
        let mut matches = self
            .skills
            .iter()
            .filter_map(|skill| {
                let score = relevance_score(&query, skill);
                (score > 0).then_some((score, skill))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|(left_score, left), (right_score, right)| {
            right_score
                .cmp(left_score)
                .then_with(|| left.name.cmp(&right.name))
        });
        matches.truncate(input.limit);
        let results = matches
            .into_iter()
            .map(|(_, skill)| {
                json!({
                    "name": skill.name,
                    "description": skill.description
                })
            })
            .collect::<Vec<_>>();
        let count = results.len();
        Ok(ToolObservation {
            status: ObservationStatus::Success,
            summary: if count == 0 {
                "no matching skills found".into()
            } else {
                format!("found {count} matching skill(s)")
            },
            next_actions: if count == 0 {
                vec!["Continue without a skill or retry with more specific task keywords".into()]
            } else {
                vec![
                    "Activate the best applicable result by calling search_skills with its exact name"
                        .into(),
                ]
            },
            artifacts: Vec::new(),
            content: json!({"matches": results}).to_string(),
        })
    }
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
        assert!(
            relevance_score("brainstorming", &brainstorming)
                > relevance_score("brainstorming", &api)
        );
        assert!(relevance_score("product ideas", &brainstorming) > 0);
        assert_eq!(relevance_score("unrelated quantum gardening", &api), 0);
    }
}
