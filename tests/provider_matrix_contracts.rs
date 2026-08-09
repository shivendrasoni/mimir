use std::collections::BTreeMap;

use mimir::{
    auth::{AuthStore, CredentialType, resolve_credential_typed},
    config::ProviderConfig,
    model::{Message, ModelRequest, ThinkingLevel},
    provider::{
        AnthropicCredentialKind, AnthropicProvider, OpenAiProvider, Provider, ResponsesProvider,
        cloudflare::{CloudflareConfig, CloudflareProvider},
        registry::{AuthKind, ProviderRegistry, RuntimeSupport, model_catalog},
    },
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn typed_environment_credentials_preserve_oauth_provenance_without_leaking() {
    let root = tempfile::TempDir::new().expect("state root");
    let store = AuthStore::new(root.path()).expect("auth store");
    let secret = "oauth-environment-secret";
    let resolved = resolve_credential_typed(
        &store,
        "anthropic",
        Some((secret, CredentialType::OAuthToken)),
    )
    .await
    .expect("credential resolution")
    .expect("environment credential");
    assert_eq!(resolved.auth_type, "oauth");
    assert_eq!(resolved.expose_for_provider(), secret);
    assert!(!format!("{resolved:?}").contains(secret));
}

#[test]
fn wire_compatible_reference_providers_are_runnable_with_exact_defaults() {
    let registry = ProviderRegistry::builtin();
    let expected = [
        (
            "minimax",
            RuntimeSupport::AnthropicMessages,
            "https://api.minimax.io/anthropic",
            "MiniMax-M2.7",
        ),
        (
            "minimax-cn",
            RuntimeSupport::AnthropicMessages,
            "https://api.minimaxi.com/anthropic",
            "MiniMax-M2.7",
        ),
        (
            "huggingface",
            RuntimeSupport::OpenAiCompatible,
            "https://router.huggingface.co/v1",
            "moonshotai/Kimi-K2.6",
        ),
        (
            "kimi-coding",
            RuntimeSupport::AnthropicMessages,
            "https://api.kimi.com/coding",
            "kimi-for-coding",
        ),
        (
            "cloudflare-workers-ai",
            RuntimeSupport::OpenAiCompatible,
            "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1",
            "@cf/moonshotai/kimi-k2.6",
        ),
        (
            "xiaomi",
            RuntimeSupport::AnthropicMessages,
            "https://api.xiaomimimo.com/anthropic",
            "mimo-v2.5-pro",
        ),
        (
            "xiaomi-token-plan-cn",
            RuntimeSupport::AnthropicMessages,
            "https://token-plan-cn.xiaomimimo.com/anthropic",
            "mimo-v2.5-pro",
        ),
        (
            "xiaomi-token-plan-ams",
            RuntimeSupport::AnthropicMessages,
            "https://token-plan-ams.xiaomimimo.com/anthropic",
            "mimo-v2.5-pro",
        ),
        (
            "xiaomi-token-plan-sgp",
            RuntimeSupport::AnthropicMessages,
            "https://token-plan-sgp.xiaomimimo.com/anthropic",
            "mimo-v2.5-pro",
        ),
    ];
    for (id, runtime, base_url, default_model) in expected {
        let provider = registry.get(id).unwrap_or_else(|| panic!("missing {id}"));
        assert_eq!(
            provider.runtime_support, runtime,
            "runtime mismatch for {id}"
        );
        assert_eq!(
            provider.base_url,
            Some(base_url),
            "base URL mismatch for {id}"
        );
        assert_eq!(
            provider.default_model,
            Some(default_model),
            "default model mismatch for {id}"
        );
    }

    let mistral = registry.get("mistral").expect("mistral catalog entry");
    assert_eq!(
        mistral.runtime_support,
        RuntimeSupport::MistralConversations
    );
    assert_eq!(mistral.base_url, Some("https://api.mistral.ai"));
    assert_eq!(mistral.default_model, Some("devstral-medium-latest"));

    let anthropic = registry.get("anthropic").expect("anthropic");
    assert_eq!(
        anthropic.env_vars,
        &["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"]
    );
    assert!(anthropic.auth.contains(&AuthKind::OAuthPkce));
    assert!(anthropic.auth.contains(&AuthKind::ApiKey));
}

#[test]
fn mixed_catalog_providers_route_each_model_to_an_existing_transport() {
    let registry = ProviderRegistry::builtin();
    for provider_id in ["opencode", "opencode-go", "cloudflare-ai-gateway"] {
        let provider = registry.get(provider_id).expect("mixed provider");
        assert_eq!(provider.runtime_support, RuntimeSupport::CatalogRouted);
        for model in model_catalog()
            .iter()
            .filter(|model| model.provider == provider_id)
        {
            let resolved = provider
                .runtime_for_model(model)
                .unwrap_or_else(|| panic!("unroutable {provider_id}/{} ({})", model.id, model.api));
            assert_ne!(resolved, RuntimeSupport::CatalogRouted);
            assert_ne!(resolved, RuntimeSupport::Unsupported);
        }
    }

    let opencode = registry.get("opencode").expect("opencode");
    let routed: BTreeMap<_, _> = model_catalog()
        .iter()
        .filter(|model| model.provider == "opencode")
        .map(|model| {
            (
                model.api.as_str(),
                opencode.runtime_for_model(model).expect("supported API"),
            )
        })
        .collect();
    assert_eq!(
        routed["anthropic-messages"],
        RuntimeSupport::AnthropicMessages
    );
    assert_eq!(
        routed["google-generative-ai"],
        RuntimeSupport::GoogleGenerativeAi
    );
    assert_eq!(
        routed["openai-completions"],
        RuntimeSupport::OpenAiCompatible
    );
    assert_eq!(routed["openai-responses"], RuntimeSupport::OpenAiCompatible);
}

#[test]
fn cloudflare_placeholders_are_expanded_only_from_valid_explicit_configuration() {
    let workers = CloudflareConfig::new(
        CloudflareProvider::WorkersAi,
        "0123456789abcdef0123456789abcdef",
        None,
    )
    .expect("workers config");
    assert_eq!(
        workers
            .resolve_base_url(
                "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1"
            )
            .expect("resolved workers URL"),
        "https://api.cloudflare.com/client/v4/accounts/0123456789abcdef0123456789abcdef/ai/v1"
    );

    let gateway = CloudflareConfig::new(
        CloudflareProvider::AiGateway,
        "0123456789abcdef0123456789abcdef",
        Some("prime-gateway_1"),
    )
    .expect("gateway config");
    assert_eq!(
        gateway
            .resolve_base_url(
                "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/compat"
            )
            .expect("resolved gateway URL"),
        "https://gateway.ai.cloudflare.com/v1/0123456789abcdef0123456789abcdef/prime-gateway_1/compat"
    );

    let missing_gateway = CloudflareConfig::new(
        CloudflareProvider::AiGateway,
        "0123456789abcdef0123456789abcdef",
        None,
    )
    .expect_err("gateway id is required");
    assert!(
        missing_gateway
            .to_string()
            .contains("CLOUDFLARE_GATEWAY_ID")
    );
    assert!(!missing_gateway.to_string().contains("0123456789abcdef"));

    assert!(
        CloudflareConfig::new(CloudflareProvider::WorkersAi, "", None)
            .expect_err("blank account id must fail")
            .to_string()
            .contains("CLOUDFLARE_ACCOUNT_ID")
    );
    for invalid in ["../escape", "value/with/slash", "{ANOTHER_SECRET}"] {
        let error = CloudflareConfig::new(CloudflareProvider::WorkersAi, invalid, None)
            .expect_err("invalid account id must fail");
        assert!(!error.to_string().contains(invalid));
    }

    let unknown = workers
        .resolve_base_url("https://example.test/{HOME}/v1")
        .expect_err("unknown placeholder must fail closed");
    assert!(unknown.to_string().contains("unsupported placeholder"));
    assert!(!unknown.to_string().contains("/Users/"));

    for untrusted in [
        "https://attacker.invalid/{CLOUDFLARE_ACCOUNT_ID}/ai/v1",
        "http://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1",
        "https://api.cloudflare.com.evil.invalid/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1",
    ] {
        assert!(
            workers.resolve_base_url(untrusted).is_err(),
            "untrusted endpoint must fail: {untrusted}"
        );
    }
}

#[tokio::test]
async fn catalog_static_headers_are_applied_but_cannot_override_authentication() {
    let (base_url, captured) = spawn_json_server(json!({
        "id": "msg_kimi",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
    .await;
    let headers = BTreeMap::from([("User-Agent".into(), "KimiCLI/1.5".into())]);
    let provider = AnthropicProvider::with_credential_kind_and_headers(
        Some(&base_url),
        "kimi-secret",
        AnthropicCredentialKind::ApiKey,
        &headers,
    )
    .expect("Kimi-compatible provider");
    provider
        .complete(model_request("kimi-for-coding"))
        .await
        .expect("Kimi-compatible request");
    let request = captured.await.expect("captured request");
    assert!(
        request
            .to_ascii_lowercase()
            .contains("user-agent: kimicli/1.5")
    );

    let protected = BTreeMap::from([("Authorization".into(), "Bearer attacker".into())]);
    let error = AnthropicProvider::with_credential_kind_and_headers(
        Some(&base_url),
        "kimi-secret",
        AnthropicCredentialKind::ApiKey,
        &protected,
    )
    .expect_err("catalog cannot override authentication");
    assert!(!error.to_string().contains("attacker"));
}

#[tokio::test]
async fn cloudflare_gateway_tokens_use_only_the_gateway_header_on_all_reused_transports() {
    let (chat_base_url, chat_captured) = spawn_json_server(json!({
        "id": "chat-cf",
        "choices": [{"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
    }))
    .await;
    let chat_config = ProviderConfig::openai(&chat_base_url, "workers-model", "cf-secret")
        .expect("Cloudflare chat config");
    OpenAiProvider::new_cloudflare_gateway(chat_config)
        .expect("Cloudflare chat provider")
        .complete(model_request("workers-model"))
        .await
        .expect("Cloudflare chat request");
    assert_cloudflare_gateway_auth(&chat_captured.await.expect("chat request"));

    let responses_body = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-cf\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
    );
    let (responses_base_url, responses_captured) =
        spawn_http_server(responses_body.into(), "text/event-stream").await;
    ResponsesProvider::new_cloudflare_gateway(Some(&responses_base_url), "cf-secret")
        .expect("Cloudflare Responses provider")
        .complete(model_request("gpt-5.4"))
        .await
        .expect("Cloudflare Responses request");
    assert_cloudflare_gateway_auth(&responses_captured.await.expect("Responses request"));

    let (anthropic_base_url, anthropic_captured) = spawn_json_server(json!({
        "id": "msg_cf",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
    .await;
    AnthropicProvider::with_credential_kind(
        Some(&anthropic_base_url),
        "cf-secret",
        AnthropicCredentialKind::CloudflareGateway,
    )
    .expect("Cloudflare Anthropic provider")
    .complete(model_request("claude-sonnet-4-6"))
    .await
    .expect("Cloudflare Anthropic request");
    assert_cloudflare_gateway_auth(&anthropic_captured.await.expect("Anthropic request"));
}

#[tokio::test]
async fn provider_errors_redact_gateway_and_oauth_credentials_echoed_by_upstreams() {
    let (chat_base_url, _) = spawn_http_server_with_status(
        json!({"error":{"message":"rejected cf-secret"}}).to_string(),
        "application/json",
        "400 Bad Request",
    )
    .await;
    let chat = OpenAiProvider::new_cloudflare_gateway(
        ProviderConfig::openai(&chat_base_url, "workers-model", "cf-secret")
            .expect("Cloudflare chat config"),
    )
    .expect("Cloudflare chat provider");
    let error = chat
        .complete(model_request("workers-model"))
        .await
        .expect_err("upstream error");
    assert!(!error.to_string().contains("cf-secret"));

    let (anthropic_base_url, _) = spawn_http_server_with_status(
        json!({"error":{"message":"rejected oauth-secret"}}).to_string(),
        "application/json",
        "400 Bad Request",
    )
    .await;
    let anthropic = AnthropicProvider::with_credential_kind(
        Some(&anthropic_base_url),
        "oauth-secret",
        AnthropicCredentialKind::OAuthToken,
    )
    .expect("OAuth provider");
    let error = anthropic
        .complete(model_request("claude-sonnet-4-6"))
        .await
        .expect_err("upstream error");
    assert!(!error.to_string().contains("oauth-secret"));
}

fn assert_cloudflare_gateway_auth(request: &str) {
    let headers = request
        .split("\r\n\r\n")
        .next()
        .expect("request headers")
        .to_ascii_lowercase();
    assert!(headers.contains("cf-aig-authorization: bearer cf-secret"));
    assert!(
        !headers
            .lines()
            .any(|line| line.starts_with("authorization:"))
    );
    assert!(!headers.contains("x-api-key:"));
}

#[tokio::test]
async fn anthropic_oauth_token_uses_bearer_identity_and_never_api_key_auth() {
    let (base_url, captured) = spawn_json_server(json!({
        "id": "msg_oauth",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
    .await;
    let token = "sk-ant-oat-test-secret";
    let provider = AnthropicProvider::with_credential_kind(
        Some(&base_url),
        token,
        AnthropicCredentialKind::OAuthToken,
    )
    .expect("oauth provider");
    provider
        .complete(model_request("claude-sonnet-4-6"))
        .await
        .expect("oauth request");

    let request = captured.await.expect("captured request");
    let lower = request.to_ascii_lowercase();
    assert!(lower.contains("authorization: bearer sk-ant-oat-test-secret"));
    assert!(!lower.contains("x-api-key:"));
    assert!(lower.contains("anthropic-beta: claude-code-20250219,oauth-2025-04-20"));
    assert!(lower.contains("x-app: cli"));
    assert!(lower.contains("user-agent: claude-cli/2.1.75"));
    let body = request.split("\r\n\r\n").nth(1).expect("request body");
    let value: serde_json::Value = serde_json::from_str(body).expect("JSON request");
    assert_eq!(
        value["system"].as_str().expect("system").lines().next(),
        Some("You are Claude Code, Anthropic's official CLI for Claude.")
    );
    let debug = format!("{provider:?}");
    assert!(!debug.contains(token));
    assert!(debug.contains("[REDACTED]"));
}

#[tokio::test]
async fn anthropic_api_key_stays_on_x_api_key_and_omits_oauth_identity() {
    let (base_url, captured) = spawn_json_server(json!({
        "id": "msg_key",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
    .await;
    let provider = AnthropicProvider::with_credential_kind(
        Some(&base_url),
        "anthropic-api-key-secret",
        AnthropicCredentialKind::ApiKey,
    )
    .expect("api-key provider");
    provider
        .complete(model_request("claude-sonnet-4-6"))
        .await
        .expect("api-key request");

    let request = captured.await.expect("captured request");
    let lower = request.to_ascii_lowercase();
    assert!(lower.contains("x-api-key: anthropic-api-key-secret"));
    assert!(!lower.contains("authorization: bearer"));
    assert!(!lower.contains("oauth-2025-04-20"));
    assert!(!request.contains("You are Claude Code"));
}

fn model_request(model: &str) -> ModelRequest {
    ModelRequest {
        model: model.into(),
        system_prompt: "Be concise".into(),
        messages: vec![Message::user("hello")],
        tools: Vec::new(),
        max_output_tokens: 2_048,
        thinking_level: ThinkingLevel::Off,
        thinking_effort: None,
    }
}

async fn spawn_json_server(
    response: serde_json::Value,
) -> (String, tokio::sync::oneshot::Receiver<String>) {
    spawn_http_server(response.to_string(), "application/json").await
}

async fn spawn_http_server(
    body: String,
    content_type: &'static str,
) -> (String, tokio::sync::oneshot::Receiver<String>) {
    spawn_http_server_with_status(body, content_type, "200 OK").await
}

async fn spawn_http_server_with_status(
    body: String,
    content_type: &'static str,
    status: &'static str,
) -> (String, tokio::sync::oneshot::Receiver<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("local address");
    let (sender, receiver) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4_096];
        let header_end = loop {
            let read = stream.read(&mut buffer).await.expect("read request");
            assert!(read > 0, "request ended before headers");
            request.extend_from_slice(&buffer[..read]);
            if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
            .unwrap_or(0);
        while request.len() < header_end + content_length {
            let read = stream.read(&mut buffer).await.expect("read request body");
            assert!(read > 0, "request body ended early");
            request.extend_from_slice(&buffer[..read]);
        }
        let captured = String::from_utf8_lossy(&request).into_owned();
        let _ = sender.send(captured);
        let reply = format!(
            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(reply.as_bytes())
            .await
            .expect("write response");
    });
    (format!("http://{address}"), receiver)
}
