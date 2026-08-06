use regex::Regex;
use std::collections::BTreeMap;

const RESERVED: &[&str] = &["call_tool", "list_tools"];

/// Returns the direct command identifier for an MCP tool name when it is safe.
///
/// # Panics
///
/// Panics only if the built-in validation regex is invalid.
pub fn tool_identifier(name: &str) -> Option<String> {
    let pattern =
        Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").expect("MCP tool identifier regex must be valid");
    if pattern.is_match(name) && !RESERVED.contains(&name) {
        Some(name.to_owned())
    } else {
        None
    }
}

pub fn identifier_map<'a, I>(names: I) -> BTreeMap<&'a str, Option<String>>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut counts = BTreeMap::<String, usize>::new();
    let collected = names
        .into_iter()
        .map(|name| {
            let identifier = tool_identifier(name);
            if let Some(identifier) = &identifier {
                *counts.entry(identifier.clone()).or_default() += 1;
            }
            (name, identifier)
        })
        .collect::<Vec<_>>();
    collected
        .into_iter()
        .map(|(name, identifier)| {
            let identifier = identifier.filter(|identifier| counts.get(identifier) == Some(&1));
            (name, identifier)
        })
        .collect()
}
