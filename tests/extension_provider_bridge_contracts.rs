use std::{collections::BTreeSet, path::Path, sync::Mutex, time::Duration};

use async_trait::async_trait;
use mimir::{
    auth::{AuthCredential, AuthStore, OAuthCredential},
    extensions::{
        Capability, CatalogEntry, ExtensionEntrypoint, ExtensionManager, ExtensionManifest,
        HostLimits, ManifestSource, RuntimeLimits, UiRequest,
    },
    model::{Message, ModelRequest},
    provider::{ProviderEvent, ProviderEventSink},
};
use serde_json::json;
use tempfile::TempDir;

fn limits() -> RuntimeLimits {
    RuntimeLimits {
        host: HostLimits {
            max_request_bytes: 64 * 1024,
            max_response_bytes: 64 * 1024,
            timeout: Duration::from_secs(2),
        },
        max_concurrency: 2,
        max_registrations: 16,
    }
}

fn manifest(module: &Path) -> ExtensionManifest {
    ExtensionManifest {
        schema_version: 1,
        name: "provider-bridge".into(),
        version: "1.0.0".into(),
        entrypoint: ExtensionEntrypoint::EmbeddedJavaScript {
            module: module.display().to_string(),
        },
        capabilities: BTreeSet::from([Capability::Provider, Capability::Ui]),
    }
}

#[derive(Default)]
struct Events(Mutex<Vec<ProviderEvent>>);

#[async_trait]
impl ProviderEventSink for Events {
    async fn emit(&self, event: ProviderEvent) {
        self.0.lock().expect("event lock").push(event);
    }
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end fixture proves OAuth suspension, durable refresh, credential conversion, and stream replay together"
)]
async fn custom_oauth_callbacks_refresh_and_stream_simple_are_bridged() {
    let workspace = TempDir::new().expect("workspace");
    let state = TempDir::new().expect("state");
    let extension = TempDir::new().expect("extension");
    let module = extension.path().join("provider.ts");
    std::fs::write(
        &module,
        r#"
export default function activate(pi) {
  pi.registerProvider("custom-oauth", {
    baseUrl: "https://example.invalid/v1",
    api: "custom-api",
    models: [{ id: "custom-model", name: "Custom", reasoning: false, input: ["text"], cost: {}, contextWindow: 4096, maxTokens: 256 }],
    oauth: {
      name: "Custom OAuth",
      async login(callbacks) {
        callbacks.onAuth({ url: "https://login.example.invalid/authorize" });
        const code = await callbacks.onPrompt({ message: "Enter authorization code:" });
        return { access: `login-${code}`, refresh: "refresh-login", expires: Date.now() + 60000 };
      },
      async refreshToken(credentials) {
        if (credentials.refresh !== "refresh-old") throw new Error("unexpected refresh credential");
        return { access: "new-access", refresh: "refresh-new", expires: Date.now() + 60000 };
      },
      getApiKey(credentials) { return `derived:${credentials.access}`; },
    },
    async *streamSimple(model, context, options) {
      if (model.id !== "custom-model" || model.provider !== "custom-oauth") throw new Error("invalid model bridge");
      if (context.systemPrompt !== "system" || context.messages[0].role !== "user") throw new Error("invalid context bridge");
      if (options.apiKey !== "derived:new-access") throw new Error("invalid OAuth API-key bridge");
      const message = {
        role: "assistant", content: [{ type: "text", text: "bridge-ok" }],
        api: model.api, provider: model.provider, model: model.id,
        usage: { input: 3, output: 2, cacheRead: 1, cacheWrite: 0, totalTokens: 6, cost: {} },
        stopReason: "stop", responseId: "custom-response", timestamp: Date.now(),
      };
      yield { type: "start", partial: { ...message, content: [] } };
      yield { type: "text_delta", contentIndex: 0, delta: "bridge-ok", partial: message };
      yield { type: "done", reason: "stop", message };
    },
  });
}
"#,
    )
    .expect("module");
    let auth = AuthStore::new(state.path()).expect("auth store");
    let manager = ExtensionManager::load_with_auth_store(
        vec![CatalogEntry {
            manifest: manifest(&module),
            root_dir: extension.path().to_owned(),
            source: ManifestSource::Workspace,
            enabled: true,
        }],
        workspace.path(),
        state.path(),
        limits(),
        auth.clone(),
    )
    .await
    .expect("manager");

    let prompt = manager
        .begin_provider_oauth_login("custom-oauth")
        .await
        .expect("begin OAuth")
        .expect("OAuth prompt");
    let UiRequest::Input { id, prompt, .. } = prompt else {
        panic!("OAuth login must request input")
    };
    assert!(prompt.contains("https://login.example.invalid/authorize"));
    manager
        .respond_ui(&id, json!("code-123"))
        .await
        .expect("resume OAuth");
    let stored = auth
        .get("custom-oauth")
        .await
        .expect("credential")
        .expect("stored OAuth");
    assert!(matches!(stored, AuthCredential::OAuth(_)));

    auth.set_oauth(
        "custom-oauth",
        OAuthCredential {
            access: "old-access".into(),
            refresh: "refresh-old".into(),
            expires_at_ms: 1,
            account_id: None,
            enterprise_url: None,
        },
    )
    .await
    .expect("expired credential");
    let (provider, _) = manager
        .activate_provider("custom-oauth", "custom-model")
        .await
        .expect("activate provider");
    let request = ModelRequest {
        model: "ignored".into(),
        thinking_level: mimir::model::ThinkingLevel::Off,
        thinking_effort: None,
        system_prompt: "system".into(),
        messages: vec![Message::user("hello")],
        tools: Vec::new(),
        max_output_tokens: 128,
    };
    let events = Events::default();
    let response = provider
        .stream(request, &events)
        .await
        .expect("custom stream");
    assert_eq!(response.response_id.as_deref(), Some("custom-response"));
    assert_eq!(response.message.text(), "bridge-ok");
    assert_eq!(
        events.0.lock().expect("events").as_slice(),
        [ProviderEvent::TextDelta("bridge-ok".into())]
    );
    let refreshed = auth
        .get("custom-oauth")
        .await
        .expect("credential")
        .expect("refreshed OAuth");
    let AuthCredential::OAuth(refreshed) = refreshed else {
        panic!("expected OAuth credential")
    };
    assert_eq!(refreshed.refresh, "refresh-new");
}
