use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::model::ToolDefinition;

use super::{ObservationStatus, Tool, ToolError, ToolObservation, object_schema, parse_input};

const DEFAULT_RESULT_LIMIT: usize = 5;
const MAX_RESULT_LIMIT: usize = 10;
const MAX_QUERY_BYTES: usize = 512;
const MAX_DESCRIPTION_BYTES: usize = 2_048;

#[derive(Debug, Clone)]
struct ToolSummary {
    name: String,
    description: String,
}

pub(super) struct SearchToolsTool {
    tools: Vec<ToolSummary>,
}

impl SearchToolsTool {
    pub(super) fn new(definitions: Vec<ToolDefinition>) -> Self {
        let tools = definitions
            .into_iter()
            .filter(|definition| definition.name != "search_tools")
            .map(|definition| ToolSummary {
                name: definition.name,
                description: truncate_utf8(&definition.description, MAX_DESCRIPTION_BYTES),
            })
            .collect();
        Self { tools }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchToolsInput {
    query: String,
    #[serde(default = "default_result_limit")]
    limit: usize,
}

const fn default_result_limit() -> usize {
    DEFAULT_RESULT_LIMIT
}

#[async_trait]
impl Tool for SearchToolsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "search_tools".into(),
            description: "Find and activate configured tools omitted from the current TypeSafe shortlist. Use only when visible tools cannot complete the task; activated tools appear on the next model step."
                .into(),
            parameters: object_schema(
                &json!({
                    "query": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_QUERY_BYTES,
                        "description": "Concrete capability, operation, service, or data source needed next"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_RESULT_LIMIT,
                        "default": DEFAULT_RESULT_LIMIT
                    }
                }),
                &["query"],
            ),
        }
    }

    async fn execute(&self, input: Value) -> Result<ToolObservation, ToolError> {
        let input: SearchToolsInput = parse_input("search_tools", input)?;
        let query = input.query.trim();
        if query.is_empty() || input.query.len() > MAX_QUERY_BYTES {
            return Err(ToolError::InvalidArguments {
                tool: "search_tools".into(),
                message: format!("query must contain 1 to {MAX_QUERY_BYTES} bytes"),
            });
        }
        if !(1..=MAX_RESULT_LIMIT).contains(&input.limit) {
            return Err(ToolError::InvalidArguments {
                tool: "search_tools".into(),
                message: format!("limit must be from 1 to {MAX_RESULT_LIMIT}"),
            });
        }

        let activated = rank(query, &self.tools, input.limit)
            .into_iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                })
            })
            .collect::<Vec<_>>();
        let count = activated.len();
        Ok(ToolObservation {
            status: ObservationStatus::Success,
            summary: if count == 0 {
                "no matching tools found".into()
            } else {
                format!("activated {count} matching tool(s) for this run")
            },
            next_actions: if count == 0 {
                vec!["Continue with the available tools or retry with more specific capability terms".into()]
            } else {
                vec!["Use the activated tool on the next model step".into()]
            },
            artifacts: Vec::new(),
            content: json!({"activated": activated}).to_string(),
        })
    }
}

fn rank<'a>(query: &str, tools: &'a [ToolSummary], limit: usize) -> Vec<&'a ToolSummary> {
    let query = query.to_ascii_lowercase();
    let query_terms = terms(&query);
    let mut matches = tools
        .iter()
        .filter_map(|tool| {
            let name = tool.name.to_ascii_lowercase();
            let description = tool.description.to_ascii_lowercase();
            let name_terms = terms(&name);
            let description_terms = terms(&description);
            let mut score = u32::from(query == name) * 10_000;
            score += u32::from(description.contains(&query)) * 80;
            let mut matched = 0_u32;
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
                score += term_score;
                matched += u32::from(term_score > 0);
            }
            if !query_terms.is_empty()
                && matched == u32::try_from(query_terms.len()).unwrap_or(u32::MAX)
            {
                score += 30;
            }
            (score > 0).then_some((score, tool))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.name.cmp(&right.name))
    });
    matches.truncate(limit);
    matches.into_iter().map(|(_, tool)| tool).collect()
}

fn terms(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|term| term.len() >= 2)
        .map(str::to_owned)
        .collect()
}

fn truncate_utf8(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}
