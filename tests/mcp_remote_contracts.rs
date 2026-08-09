use std::{collections::BTreeMap, fmt::Write as _, sync::Arc, time::Duration};

use async_trait::async_trait;
use mimir::{
    auth::AuthStore,
    mcp::{
        McpAuthCoordinator, McpCatalogHttp, McpCatalogServer, McpClient, McpHttpConfig,
        McpOAuthAuthorization, McpOAuthClient, McpOAuthClientMetadataStore, McpOAuthCodeReceiver,
        McpServerCatalog, McpServerConfig, McpToolCallOutput, builtin_mcp_catalog,
        connect_catalog_client,
    },
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Mutex,
};

#[derive(Clone)]
struct TestResponse {
    status: u16,
    content_type: &'static str,
    headers: Vec<(String, String)>,
    body: String,
}

#[derive(Debug, Clone)]
struct RecordedRequest {
    head: String,
    body: String,
}

struct CallbackReceiver {
    redirect_uri: &'static str,
    authorization_url: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl McpOAuthCodeReceiver for CallbackReceiver {
    async fn receive_code(
        &self,
        authorization: &McpOAuthAuthorization,
    ) -> mimir::error::Result<String> {
        let authorization_url =
            reqwest::Url::parse(authorization.authorization_url()).expect("authorization URL");
        let state = authorization_url
            .query_pairs()
            .find_map(|(name, value)| (name == "state").then(|| value.into_owned()))
            .expect("state");
        *self.authorization_url.lock().await = Some(authorization_url.into());
        Ok(format!(
            "{}?code=challenge-code&state={state}",
            self.redirect_uri
        ))
    }
}

async fn loopback_server<F>(build: F) -> (String, Arc<Mutex<Vec<RecordedRequest>>>)
where
    F: FnOnce(&str) -> Vec<TestResponse>,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let responses = build(&base);
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let sink = recorded.clone();
    tokio::spawn(async move {
        for response in responses {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let request = read_request(&mut stream).await;
            sink.lock().await.push(request);
            let reason = match response.status {
                200 => "OK",
                202 => "Accepted",
                404 => "Not Found",
                _ => "Error",
            };
            let mut head = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                response.status,
                reason,
                response.content_type,
                response.body.len()
            );
            for (name, value) in response.headers {
                let _ = write!(head, "{name}: {value}\r\n");
            }
            head.push_str("\r\n");
            stream.write_all(head.as_bytes()).await.expect("head");
            stream
                .write_all(response.body.as_bytes())
                .await
                .expect("body");
        }
    });
    (base, recorded)
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> RecordedRequest {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut buffer = [0_u8; 1024];
        let read = stream.read(&mut buffer).await.expect("read");
        assert!(read > 0, "unexpected EOF");
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let head = String::from_utf8(bytes[..header_end].to_vec()).expect("head utf8");
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("content length"))
        })
        .unwrap_or_default();
    while bytes.len() - header_end < content_length {
        let mut buffer = [0_u8; 1024];
        let read = stream.read(&mut buffer).await.expect("body read");
        assert!(read > 0, "unexpected body EOF");
        bytes.extend_from_slice(&buffer[..read]);
    }
    RecordedRequest {
        head,
        body: String::from_utf8(bytes[header_end..header_end + content_length].to_vec())
            .expect("body utf8"),
    }
}

fn json_response(body: &Value) -> TestResponse {
    TestResponse {
        status: 200,
        content_type: "application/json",
        headers: Vec::new(),
        body: body.to_string(),
    }
}

#[test]
fn remote_config_and_builtin_catalog_are_safe_and_reference_compatible() {
    let remote = McpHttpConfig::new("https://mcp.example.com/mcp").expect("remote");
    let config = McpServerConfig::remote("demo", "Demo", remote).expect("config");
    assert_eq!(
        config.remote.as_ref().unwrap().url,
        "https://mcp.example.com/mcp"
    );
    assert!(McpHttpConfig::new("http://mcp.example.com/mcp").is_err());
    assert!(McpHttpConfig::new("https://user:secret@mcp.example.com/mcp").is_err());

    let builtins = builtin_mcp_catalog();
    assert_eq!(builtins.len(), 2);
    assert_eq!(builtins[0].server, "linear");
    assert_eq!(
        builtins[0].remote.as_ref().unwrap().url,
        "https://mcp.linear.app/mcp"
    );
    assert_eq!(builtins[1].server, "notion");
    assert!(builtins.iter().all(|entry| entry.oauth));
}

#[tokio::test]
async fn streamable_http_initializes_and_propagates_session_auth_and_protocol_headers() {
    let responses = vec![
        TestResponse {
            headers: vec![("Mcp-Session-Id".into(), "session-123".into())],
            ..json_response(&json!({"jsonrpc":"2.0","id":1,"result":{
                "protocolVersion":"2025-06-18","capabilities":{"tools":{}},
                "serverInfo":{"name":"remote","version":"1.0.0"}
            }}))
        },
        TestResponse {
            status: 202,
            content_type: "application/json",
            headers: Vec::new(),
            body: String::new(),
        },
        TestResponse {
            status: 200,
            content_type: "text/event-stream",
            headers: Vec::new(),
            body: format!(
                "data: {}\r\n\r\ndata: {}\r\n\r\n",
                json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}),
                json!({"jsonrpc":"2.0","id":2,"result":{"tools":[{
                    "name":"list_issues","description":"List","inputSchema":{"type":"object"}
                }]}})
            ),
        },
        json_response(&json!({"jsonrpc":"2.0","id":3,"result":{
            "structuredContent":{"ok":true},"content":[],"isError":false
        }})),
        TestResponse {
            status: 403,
            content_type: "application/json",
            headers: vec![(
                "WWW-Authenticate".into(),
                "Bearer error=\"insufficient_scope\", scope=\"issues:write\"".into(),
            )],
            body: "{}".into(),
        },
    ];
    let (base, requests) = loopback_server(|_| responses).await;
    let mut http = McpHttpConfig::new(format!("{base}/mcp")).expect("http");
    http.headers.insert("X-Client".into(), "mimir".into());
    let config = McpServerConfig::remote("remote", "Remote", http).expect("config");
    let mut client = McpClient::connect_with_bearer(&config, Some("top-secret"))
        .await
        .expect("connect");
    assert_eq!(client.list_tools().await.expect("tools").len(), 1);
    assert_eq!(
        client
            .call_tool("list_issues", json!({}))
            .await
            .expect("call"),
        McpToolCallOutput::Structured(json!({"ok":true}))
    );
    let error = client
        .list_tools()
        .await
        .expect_err("insufficient scope must remain actionable");
    assert!(error.to_string().contains("required scopes: issues:write"));
    let challenge = client
        .take_authorization_challenge()
        .expect("parsed authorization challenge");
    assert_eq!(
        challenge.scopes().expect("challenge scopes"),
        ["issues:write"]
    );

    let requests = requests.lock().await;
    assert_eq!(requests.len(), 5);
    assert!(!requests[0].head.contains("Mcp-Session-Id"));
    for request in &requests[1..] {
        assert!(request.head.contains("mcp-session-id: session-123"));
        assert!(request.head.contains("mcp-protocol-version: 2025-06-18"));
        assert!(request.head.contains("authorization: Bearer top-secret"));
        assert!(request.head.contains("x-client: mimir"));
    }
    assert!(requests[1].body.contains("notifications/initialized"));
}

#[tokio::test]
async fn catalog_connector_applies_stored_credentials_without_exposing_them() {
    let responses = vec![
        json_response(&json!({"jsonrpc":"2.0","id":1,"result":{
            "protocolVersion":"2025-06-18","capabilities":{},
            "serverInfo":{"name":"remote","version":"1.0.0"}
        }})),
        TestResponse {
            status: 202,
            content_type: "application/json",
            headers: Vec::new(),
            body: String::new(),
        },
    ];
    let (base, requests) = loopback_server(|_| responses).await;
    let state = TempDir::new().expect("state");
    let catalog = McpServerCatalog::new(state.path()).expect("catalog");
    catalog
        .upsert(
            McpCatalogServer::remote(
                "remote",
                "Remote",
                McpCatalogHttp::new(format!("{base}/mcp")).expect("remote"),
            )
            .expect("entry"),
        )
        .await
        .expect("persist");
    McpAuthCoordinator::new(state.path())
        .expect("auth")
        .store_api_key("remote", "stored-secret")
        .await
        .expect("store key");

    let client = connect_catalog_client(&catalog, state.path(), "remote")
        .await
        .expect("connect");
    assert_eq!(client.server_info().name, "remote");
    let requests = requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.head.contains("authorization: Bearer stored-secret"))
    );
}

#[tokio::test]
async fn oauth_authorize_uses_www_authenticate_metadata_and_authoritative_scopes() {
    let (base, requests) = loopback_server(|base| {
        vec![
            TestResponse {
                status: 401,
                content_type: "application/json",
                headers: vec![(
                    "WWW-Authenticate".into(),
                    format!(
                        "Bearer resource_metadata=\"{base}/protected-metadata\", scope=\"challenge:write\""
                    ),
                )],
                body: "{}".into(),
            },
            json_response(&json!({
                "resource":format!("{base}/mcp"),
                "authorization_servers":[base],
                "scopes_supported":["metadata:read"]
            })),
            json_response(&json!({
                "issuer":base,
                "authorization_endpoint":format!("{base}/authorize"),
                "token_endpoint":format!("{base}/token"),
                "registration_endpoint":format!("{base}/register"),
                "code_challenge_methods_supported":["S256"]
            })),
            json_response(&json!({"client_id":"challenge-client"})),
            json_response(&json!({
                "access_token":"challenge-access", "refresh_token":"challenge-refresh",
                "token_type":"Bearer", "expires_in":3600
            })),
        ]
    })
    .await;
    let mut remote = McpHttpConfig::new(format!("{base}/mcp")).expect("remote");
    remote.scopes = vec!["configured:scope".into()];
    let observed_url = Arc::new(Mutex::new(None));
    let receiver = CallbackReceiver {
        redirect_uri: "http://127.0.0.1:53700/callback",
        authorization_url: observed_url.clone(),
    };
    let routing_headers = BTreeMap::from([("X-Tenant".into(), "workspace-a".into())]);
    let bundle = McpOAuthClient::new(Duration::from_secs(2), 64 * 1024)
        .expect("oauth")
        .authorize_with_headers(
            &remote,
            &routing_headers,
            None,
            "http://127.0.0.1:53700/callback",
            &receiver,
        )
        .await
        .expect("authorize");
    assert_eq!(bundle.credential.access, "challenge-access");
    let authorization_url = observed_url.lock().await.clone().expect("observed URL");
    let authorization_url = reqwest::Url::parse(&authorization_url).expect("authorization URL");
    let parameters = authorization_url.query_pairs().collect::<BTreeMap<_, _>>();
    assert_eq!(
        parameters.get("scope").map(std::convert::AsRef::as_ref),
        Some("challenge:write")
    );
    let requests = requests.lock().await;
    assert!(requests[0].head.starts_with("POST /mcp "));
    assert!(requests[0].head.contains("x-tenant: workspace-a"));
    assert!(requests[1].head.starts_with("GET /protected-metadata "));
}

#[tokio::test]
async fn oauth_refuses_authorization_servers_without_advertised_pkce_s256() {
    let (base, _) = loopback_server(|base| {
        vec![
            json_response(&json!({
                "resource":format!("{base}/mcp"),
                "authorization_servers":[base]
            })),
            json_response(&json!({
                "issuer":base,
                "authorization_endpoint":format!("{base}/authorize"),
                "token_endpoint":format!("{base}/token")
            })),
            TestResponse {
                status: 404,
                content_type: "application/json",
                headers: Vec::new(),
                body: "{}".into(),
            },
        ]
    })
    .await;
    let remote = McpHttpConfig::new(format!("{base}/mcp")).expect("remote");
    let error = McpOAuthClient::new(Duration::from_secs(2), 64 * 1024)
        .expect("oauth")
        .begin_authorization(&remote, None, "http://127.0.0.1:53700/callback")
        .await
        .expect_err("PKCE advertisement is mandatory");
    assert!(error.to_string().contains("no usable PKCE issuer"));
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the end-to-end OAuth contract keeps ordered discovery, PKCE, callback, token, and refresh assertions together"
)]
async fn oauth_discovers_oidc_registers_pkce_exchanges_and_refreshes_without_leaking_tokens() {
    let (base, requests) = loopback_server(|base| {
        vec![
            json_response(&json!({
                "resource":format!("{base}/mcp"),
                "authorization_servers":["http://127.0.0.1:1/unavailable", format!("{base}/tenant")],
                "scopes_supported":["issues:read"]
            })),
            TestResponse {
                status: 404,
                content_type: "application/json",
                headers: Vec::new(),
                body: "{}".into(),
            },
            json_response(&json!({
                "issuer":format!("{base}/tenant"),
                "authorization_endpoint":format!("{base}/authorize"),
                "token_endpoint":format!("{base}/token"),
                "registration_endpoint":format!("{base}/register"),
                "code_challenge_methods_supported":["S256"]
            })),
            json_response(&json!({"client_id":"dynamic-client"})),
            json_response(&json!({
                "access_token":"access-secret","refresh_token":"refresh-secret",
                "token_type":"Bearer","expires_in":3600
            })),
            json_response(&json!({
                "access_token":"next-access","token_type":"Bearer","expires_in":7200
            })),
        ]
    })
    .await;
    let mut remote = McpHttpConfig::new(format!("{base}/mcp")).expect("remote");
    remote.scopes = vec!["issues:read".into()];
    let oauth = McpOAuthClient::new(Duration::from_secs(2), 64 * 1024).expect("oauth");
    let flow = oauth
        .begin_authorization(&remote, None, "http://127.0.0.1:53700/callback")
        .await
        .expect("begin");
    let auth_url = reqwest::Url::parse(flow.authorization_url()).expect("auth url");
    let params = auth_url.query_pairs().collect::<BTreeMap<_, _>>();
    let expected_resource = format!("{base}/mcp");
    assert_eq!(
        params
            .get("code_challenge_method")
            .map(std::convert::AsRef::as_ref),
        Some("S256")
    );
    assert_eq!(
        params.get("resource").map(std::convert::AsRef::as_ref),
        Some(expected_resource.as_str())
    );
    let state = params.get("state").expect("state");
    assert!(
        oauth
            .exchange_code(&flow, "bare-code")
            .await
            .expect_err("bare codes must not bypass state")
            .to_string()
            .contains("code and state")
    );
    let wrong_redirect = format!("http://127.0.0.1:53701/callback?code=auth-code&state={state}");
    assert!(
        oauth
            .exchange_code(&flow, &wrong_redirect)
            .await
            .expect_err("redirect URI must match")
            .to_string()
            .contains("redirect mismatch")
    );
    let callback = format!("http://127.0.0.1:53700/callback?code=auth-code&state={state}");
    let bundle = oauth
        .exchange_code(&flow, &callback)
        .await
        .expect("exchange");
    assert_eq!(bundle.credential.access, "access-secret");
    assert_eq!(bundle.metadata.client_id, "dynamic-client");
    assert!(!format!("{bundle:?}").contains("access-secret"));

    let mut stale_metadata = bundle.metadata.clone();
    stale_metadata.resource = "https://different.example.com/mcp".into();
    assert!(
        oauth
            .refresh(&remote, &stale_metadata, &bundle.credential)
            .await
            .expect_err("refresh metadata must stay bound to the configured resource")
            .to_string()
            .contains("no longer matches")
    );

    let refreshed = oauth
        .refresh(&remote, &bundle.metadata, &bundle.credential)
        .await
        .expect("refresh");
    assert_eq!(refreshed.credential.access, "next-access");
    assert_eq!(refreshed.credential.refresh, "refresh-secret");

    let requests = requests.lock().await;
    assert!(
        requests[0]
            .head
            .starts_with("GET /.well-known/oauth-protected-resource/mcp ")
    );
    assert!(
        requests[1]
            .head
            .starts_with("GET /.well-known/oauth-authorization-server/tenant ")
    );
    assert!(
        requests[2]
            .head
            .starts_with("GET /.well-known/openid-configuration/tenant ")
    );
    assert!(!requests[3].body.contains("dynamic-client"));
    assert!(requests[4].body.contains("code_verifier="));
    assert!(requests[4].body.contains("resource="));
    assert!(requests[5].body.contains("refresh_token=refresh-secret"));
}

#[tokio::test]
async fn oauth_metadata_store_is_private_and_does_not_duplicate_tokens() {
    let state = TempDir::new().expect("state");
    let auth = AuthStore::new(state.path()).expect("auth");
    let metadata = McpOAuthClientMetadataStore::new(state.path()).expect("metadata");
    let value = mimir::mcp::McpOAuthClientMetadata {
        token_endpoint: "https://auth.example.com/token".into(),
        client_id: "public-client".into(),
        resource: "https://mcp.example.com/mcp".into(),
        scopes: vec!["read".into()],
    };
    metadata.set("linear", value.clone()).await.expect("set");
    assert_eq!(metadata.get("linear").await.expect("get"), Some(value));
    let persisted = std::fs::read_to_string(metadata.path()).expect("read");
    assert!(!persisted.contains("access-secret"));
    assert!(!persisted.contains("refresh-secret"));
    assert!(auth.get("mcp:linear").await.expect("auth read").is_none());
    auth.set_api_key("mcp:linear", "access-secret")
        .await
        .expect("store auth");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(metadata.path())
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(auth.path())
                .expect("auth metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
