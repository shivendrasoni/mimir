use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mimir::{
    auth::{AuthCredential, AuthStore, OAuthCredential, PendingOAuth, resolve_credential},
    model::ThinkingLevel,
    provider::registry::{AuthKind, ProviderRegistry, RuntimeSupport, model_catalog},
};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn auth_store_round_trips_redacted_credentials_and_logout() {
    let root = TempDir::new().expect("state root");
    let store = AuthStore::new(root.path()).expect("auth store");

    store
        .set_api_key("openai", "sk-secret-value")
        .await
        .expect("store key");

    let status = store.statuses().await.expect("statuses");
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].provider, "openai");
    assert_eq!(status[0].auth_type, "api_key");
    assert!(!format!("{status:?}").contains("sk-secret-value"));
    assert!(matches!(
        store.get("openai").await.expect("get"),
        Some(AuthCredential::ApiKey { .. })
    ));

    assert!(store.logout("openai").await.expect("logout"));
    assert!(
        store
            .get("openai")
            .await
            .expect("get after logout")
            .is_none()
    );
}

#[test]
fn generated_model_catalog_preserves_reference_metadata_and_thinking_maps() {
    let catalog = model_catalog();
    assert_eq!(catalog.len(), 1_162);
    let mini = catalog
        .iter()
        .find(|model| model.provider == "openai" && model.id == "gpt-5-mini")
        .expect("gpt-5-mini");
    assert_eq!(mini.name, "GPT-5 Mini");
    assert_eq!(mini.api, "openai-responses");
    assert_eq!(mini.input, ["text", "image"]);
    assert_eq!(mini.context_window, 400_000);
    assert_eq!(mini.max_tokens, 128_000);
    assert_eq!(
        mini.thinking_levels(),
        vec![
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ]
    );

    let frontier = catalog
        .iter()
        .find(|model| model.provider == "openai" && model.id == "gpt-5.6")
        .expect("gpt-5.6");
    assert_eq!(
        frontier.thinking_levels(),
        vec![
            ThinkingLevel::Off,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Xhigh,
            ThinkingLevel::Max,
        ]
    );
    assert!(
        catalog
            .iter()
            .any(|model| { model.provider == "openai-codex" && model.id == "gpt-5.6-sol" })
    );
    assert!(
        !catalog
            .iter()
            .any(|model| { model.provider == "openai-codex" && model.id == "gpt-5.1-codex" })
    );
}

#[tokio::test]
async fn environment_credentials_take_precedence_without_being_persisted() {
    let root = TempDir::new().expect("state root");
    let store = AuthStore::new(root.path()).expect("auth store");
    store
        .set_api_key("openai", "stored-key")
        .await
        .expect("store key");

    let resolved = resolve_credential(&store, "openai", Some("environment-key"))
        .await
        .expect("resolve")
        .expect("credential");
    assert_eq!(resolved.source.as_str(), "environment");
    assert_eq!(resolved.expose_for_provider(), "environment-key");
}

#[tokio::test]
async fn expired_oauth_credentials_are_reported_without_exposing_tokens() {
    let root = TempDir::new().expect("state root");
    let store = AuthStore::new(root.path()).expect("auth store");
    store
        .set_oauth(
            "openai-codex",
            OAuthCredential {
                access: "access-secret".into(),
                refresh: "refresh-secret".into(),
                expires_at_ms: 1,
                account_id: Some("acct-test".into()),
                enterprise_url: None,
            },
        )
        .await
        .expect("store oauth");

    let statuses = store.statuses().await.expect("statuses");
    assert!(statuses[0].expired);
    let rendered = format!("{statuses:?}");
    assert!(!rendered.contains("access-secret"));
    assert!(!rendered.contains("refresh-secret"));
}

#[test]
fn pkce_authorization_rejects_state_mismatch_and_accepts_matching_redirect() {
    let pending = PendingOAuth::new_for_test(
        "openai-codex",
        "verifier",
        "expected-state",
        "http://localhost:1455/auth/callback",
    );
    assert!(
        pending
            .parse_authorization_input("http://localhost:1455/auth/callback?code=abc&state=wrong")
            .is_err()
    );
    assert_eq!(
        pending
            .parse_authorization_input(
                "http://localhost:1455/auth/callback?code=abc&state=expected-state"
            )
            .expect("valid callback"),
        "abc"
    );
}

#[tokio::test]
async fn browser_oauth_callback_is_received_and_gets_a_completion_page() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve callback port");
    let port = probe.local_addr().expect("callback address").port();
    drop(probe);
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let pending =
        PendingOAuth::new_for_test("anthropic", "verifier", "expected-state", &redirect_uri);
    let receiver = {
        let pending = pending.clone();
        tokio::spawn(async move {
            pending
                .receive_browser_callback(Duration::from_secs(2))
                .await
        })
    };

    let mut browser = loop {
        match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            Ok(stream) => break stream,
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    };
    browser
        .write_all(
            b"GET /callback?code=claude-code&state=expected-state HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("send browser callback");
    let mut response = String::new();
    browser
        .read_to_string(&mut response)
        .await
        .expect("read completion page");
    let callback = receiver
        .await
        .expect("callback task")
        .expect("accepted callback");

    assert_eq!(
        pending
            .parse_authorization_input(&callback)
            .expect("validated callback"),
        "claude-code"
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(response.contains("Authentication received"));
    assert!(!response.contains("claude-code"));
}

#[tokio::test]
async fn browser_oauth_callback_rejects_non_loopback_redirects_before_binding() {
    let pending = PendingOAuth::new_for_test(
        "anthropic",
        "verifier",
        "expected-state",
        "http://example.com:53692/callback",
    );
    let error = pending
        .receive_browser_callback(Duration::from_millis(1))
        .await
        .expect_err("non-loopback redirect must fail closed");

    assert!(error.to_string().contains("must be loopback"));
}

#[test]
fn provider_registry_contains_subscription_and_api_key_catalog() {
    let registry = ProviderRegistry::builtin();
    for provider in [
        "openai-codex",
        "github-copilot",
        "openai",
        "google",
        "amazon-bedrock",
        "openrouter",
        "prime-inference",
    ] {
        assert!(registry.get(provider).is_some(), "missing {provider}");
    }
    assert_eq!(
        registry
            .get("anthropic")
            .expect("anthropic")
            .runtime_support,
        RuntimeSupport::AnthropicMessages
    );
    assert!(
        registry
            .get("openai-codex")
            .expect("codex")
            .auth
            .contains(&AuthKind::OAuthPkce)
    );
    assert!(
        registry
            .get("github-copilot")
            .expect("copilot")
            .auth
            .contains(&AuthKind::OAuthDevice)
    );
    assert_eq!(
        registry.get("openai").expect("openai").runtime_support,
        RuntimeSupport::OpenAiCompatible
    );
    assert_eq!(
        registry
            .get("openai-codex")
            .expect("openai-codex")
            .runtime_support,
        RuntimeSupport::CodexResponses
    );
    assert!(registry.len() >= 25);
}

#[cfg(unix)]
#[tokio::test]
async fn auth_file_is_owner_read_write_only() {
    use std::os::unix::fs::PermissionsExt;

    let root = TempDir::new().expect("state root");
    let store = AuthStore::new(root.path()).expect("auth store");
    store
        .set_api_key("openai", "secret")
        .await
        .expect("store key");
    let mode = std::fs::metadata(store.path())
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn oauth_expiry_uses_epoch_milliseconds() {
    let now = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis(),
    )
    .expect("epoch milliseconds fit in u64");
    let credential = OAuthCredential {
        access: "access".into(),
        refresh: "refresh".into(),
        expires_at_ms: now + 60_000,
        account_id: None,
        enterprise_url: None,
    };
    assert!(!credential.is_expired(now));
    assert!(credential.is_expired(now + 60_001));
}
