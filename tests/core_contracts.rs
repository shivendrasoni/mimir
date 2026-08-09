use std::time::Duration;

use mimir::budget::{Budget, BudgetError, BudgetUsage};
use mimir::config::{Config, ProviderConfig};
use mimir::model::{Content, Message, Role, StopReason, ToolCall};
use serde_json::json;

#[test]
fn configuration_debug_output_redacts_provider_secrets() {
    let config = Config::for_test(
        ProviderConfig::openai(
            "https://api.openai.com/v1",
            "gpt-test",
            "sk-test-do-not-leak",
        )
        .expect("valid provider configuration"),
    );

    let rendered = format!("{config:?}");
    assert!(rendered.contains("[REDACTED]"));
    assert!(!rendered.contains("sk-test-do-not-leak"));
}

#[test]
fn message_contract_round_trips_tool_calls_without_losing_arguments() {
    let message = Message::assistant(
        vec![Content::ToolCall(ToolCall {
            id: "call-1".into(),
            name: "read_file".into(),
            arguments: json!({"path": "README.md"}),
        })],
        StopReason::ToolUse,
    );

    let encoded = serde_json::to_string(&message).expect("message should serialize");
    let decoded: Message = serde_json::from_str(&encoded).expect("message should deserialize");

    assert_eq!(decoded.role, Role::Assistant);
    assert_eq!(decoded, message);
}

#[test]
fn budget_names_the_first_exhausted_limit() {
    let budget = Budget {
        max_turns: 2,
        max_tool_calls: 3,
        max_tokens: 100,
        max_elapsed: Duration::from_secs(10),
        max_context_messages: 20,
    };
    let mut usage = BudgetUsage::default();

    usage.record_turn(40).expect("first turn should fit");
    usage.record_turn(40).expect("second turn should fit");
    assert_eq!(usage.check(&budget), Err(BudgetError::Turns { limit: 2 }));
}

#[test]
fn user_and_tool_result_constructors_preserve_observable_roles() {
    let user = Message::user("inspect this repository");
    let result = Message::tool_result("call-1", "read_file", "contents", false);

    assert_eq!(user.role, Role::User);
    assert_eq!(result.role, Role::Tool);
}

#[test]
fn provider_configuration_rejects_blank_security_inputs() {
    for (base_url, model, key) in [
        ("", "model", "key"),
        ("https://example.test/v1", " ", "key"),
        ("https://example.test/v1", "model", "\t"),
    ] {
        let error = ProviderConfig::openai(base_url, model, key)
            .expect_err("blank values must fail closed");
        assert!(error.to_string().contains("must not be blank"));
    }
}

#[test]
fn text_projection_preserves_boundaries_between_content_blocks() {
    let message = Message::assistant(
        vec![
            Content::Text {
                text: "first".into(),
            },
            Content::Text {
                text: "second".into(),
            },
        ],
        StopReason::Stop,
    );

    assert_eq!(message.text(), "first\nsecond");
}
