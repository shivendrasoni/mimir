use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    model::ToolDefinition,
    skills::{SkillSummary, rank_skill_summaries},
};

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
        let results = rank_skill_summaries(&query, &self.skills, input.limit)
            .into_iter()
            .map(|skill| {
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
