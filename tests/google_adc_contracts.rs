use std::{path::PathBuf, sync::Arc};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use mimir::provider::{GoogleAdcEnvironment, GoogleAdcError, GoogleAdcResolver};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex,
};

const TEST_PRIVATE_KEY: &str = r"-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDOqAR1MkwdyDR4
J4GNpYusBp00GcYUpuoaxqO1ENzXLMUWlUkF6f75CNReEwBq3yfUZ6fYPBEdGl3x
XvKTdxrvuSADKlu5ifjk9eGwUJPT8TD5hMB6dyo2g/Tcb7KKloFbMhI8zdE5VPGd
W4skoQPncaNMModVxl7WThkk/9BNMLIULMJi9uAEj+9tvGUvIUeeocSx1m0o9NRu
9xheOQy/ouAjgFgvMYi7lOu/BdMUh6Ca/wH8u0zUv9HwqDOzjBGBPIQ2C7U8fUSB
AxxAuthxO7a4NcsICY7RW6Me5LpyioyOEPcpThtBY7eu5XJ3v9iBpvnUPgLGklqj
oEP7f7eNAgMmBEUCggEACZdjTEd8GgOdRMgQsqqjx4YrC4jTcIJy4eFfZvyTiXCf
ihHz6mpa9+/6LQHBicUPDVvL4/onLlbPxKTh04ip3NZd+GKJAlVsi2u2pkFwm1o1
3SYixz1GQL+hNZ0OduHyGfjjPrm9C1nsGY6y/MquA0a9i0AeEuPGfLYpVpwYWi4P
NaL3QWRMHMpeuxOdd6iaKVP8XjAEjA5hnwfI0h0eVrivBxk9D98/Sc1J8u8xOLUT
iGLxRwvS0W40ornnd3psjI1MuUtOi11hbNU5N1PnsPMcx9pVm6jpjYiJFOM0dzuv
SYrYjZYx61/jLlOkFFvwulSL8rClDGP2exTjmKNLDQKBgQDzZOFu8SAX7JWxkjCM
AeCHzuYZq1Cl1TfMAYQdySswvO8NnyxrvV3BC99bn2w1Sk+fIQUgyqcrT1w2uNM/
EDJMVZVhQYkeRbhLSfWepb+sb/o4kArKUJmvzQL2qCV8Qc5bsuQVODK1wi+R6zif
ogNcPPmzN0UxxIPLMM6wByWbHQKBgQDZXAmV+r38vM/mPg8yYvgGhpq1ceF5Ppcj
TLub1LaHKnaVOJlVz2znJFNFpd+AIffZUZVjr7wmZ/UxH60JPeLALNBpEJtjDWjj
v3Z/iniKat16sZny2PakC3wZENnatSrIDQ0zOqyrMhqTCdyITd1NtjegwRFa48CO
+mg/metzMQKBgQDU9++fm+lHup0bO85Z5WCIOaHkZFU+G20RPQ9jZ1i7tHOon5lJ
g26tQLbzFO7jrCJE17bzeeg/MOF3g61o1Qhol4icBRwm4VWKSiIL/CQplYYGRLXX
o+9ROsYSucbAogIbtrnN59vSH+WFh0bHlWPpurfQa0OqtDoKXK+rRRmmYQKBgD2y
OG8XTy6j70tr0WAXSc4tavqL418FEXhiHxaiOtNuugPAcxNjiSQZaeW4ftsPy88a
C9bhrul7rh8tl6q+GbF9vn2Uks22igiX9XI1DoRsZpZg3JeMUGjaWYUk/KihNjWN
Pl+PatXPeNkInJP0cxiRYs4PjEkCoZkCtjOz5pJtAoGBAN3JcRg9zzRQxD4Guirz
I3nu3rLWeFE/twa3WgBgmBVAQfSwnmvjhdSyXYDsJByJnkqYahewoSHaq5Gh5Pxa
GAKnB03z+z92YfDhyXeZ420h3pN8xCCVhdswpWrwoijgAQNu15JiXlNozhAVdKLp
dn8HM4lJ8K/fNYzs0Yxtbz9V
-----END PRIVATE KEY-----";

#[derive(Clone, Debug)]
struct RecordedRequest {
    head: String,
    body: String,
}

async fn read_request(socket: &mut TcpStream) -> RecordedRequest {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = socket.read(&mut chunk).await.expect("read request");
        assert_ne!(read, 0, "request closed before headers");
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap()
        + 4;
    let head = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let content_length = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    while bytes.len() - header_end < content_length {
        let read = socket.read(&mut chunk).await.expect("read request body");
        assert_ne!(read, 0);
        bytes.extend_from_slice(&chunk[..read]);
    }
    RecordedRequest {
        head,
        body: String::from_utf8_lossy(&bytes[header_end..header_end + content_length]).into_owned(),
    }
}

async fn token_server(
    expected_requests: usize,
    expires_in: u64,
) -> (
    String,
    Arc<Mutex<Vec<RecordedRequest>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let task = tokio::spawn(async move {
        for _ in 0..expected_requests {
            let (mut socket, _) = listener.accept().await.expect("accept");
            captured.lock().await.push(read_request(&mut socket).await);
            let body = json!({
                "access_token": "fresh-access-token",
                "expires_in": expires_in,
                "token_type": "Bearer"
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("respond");
        }
    });
    (format!("http://{address}"), requests, task)
}

async fn write_adc(temp: &TempDir, value: Value) -> PathBuf {
    let path = temp.path().join("adc.json");
    tokio::fs::write(&path, serde_json::to_vec(&value).unwrap())
        .await
        .unwrap();
    path
}

#[tokio::test]
async fn explicit_access_token_resolves_routing_without_network() {
    let resolver = GoogleAdcResolver::new(GoogleAdcEnvironment {
        oauth_access_token: Some("explicit-token".into()),
        project_id: Some("vertex-project".into()),
        quota_project_id: Some("billing-project".into()),
        location: Some("europe-west4".into()),
        ..GoogleAdcEnvironment::default()
    })
    .expect("resolver");

    let credential = resolver.resolve().await.expect("credential");
    assert_eq!(credential.exposed_access_token(), "explicit-token");
    assert_eq!(credential.project_id, "vertex-project");
    assert_eq!(
        credential.quota_project_id.as_deref(),
        Some("billing-project")
    );
    assert_eq!(credential.location, "europe-west4");
    assert!(credential.expires_at.is_none());
    assert!(!format!("{credential:?}").contains("explicit-token"));
}

#[tokio::test]
async fn authorized_user_refresh_is_form_encoded_and_cached() {
    let (base, requests, server) = token_server(1, 3_600).await;
    let temp = TempDir::new().unwrap();
    let path = write_adc(
        &temp,
        json!({
            "type": "authorized_user",
            "client_id": "client-id",
            "client_secret": "client-secret",
            "refresh_token": "refresh-token",
            "token_uri": format!("{base}/token"),
            "quota_project_id": "file-quota"
        }),
    )
    .await;
    let resolver = GoogleAdcResolver::new(GoogleAdcEnvironment {
        application_credentials: Some(path),
        project_id: Some("env-project".into()),
        ..GoogleAdcEnvironment::default()
    })
    .unwrap();

    let first = resolver.resolve().await.unwrap();
    let second = resolver.resolve().await.unwrap();
    server.await.unwrap();
    assert_eq!(first.exposed_access_token(), "fresh-access-token");
    assert_eq!(second.exposed_access_token(), "fresh-access-token");
    assert_eq!(first.quota_project_id.as_deref(), Some("file-quota"));
    let captured = requests.lock().await;
    assert_eq!(captured.len(), 1, "fresh token must come from cache");
    assert!(captured[0].head.starts_with("POST /token HTTP/1.1"));
    assert!(captured[0].body.contains("grant_type=refresh_token"));
    assert!(captured[0].body.contains("client_secret=client-secret"));
    assert!(captured[0].body.contains("refresh_token=refresh-token"));
}

#[tokio::test]
async fn token_inside_expiry_skew_is_refreshed_instead_of_reused() {
    let (base, requests, server) = token_server(2, 30).await;
    let temp = TempDir::new().unwrap();
    let path = write_adc(
        &temp,
        json!({
            "type": "authorized_user",
            "client_id": "client-id",
            "client_secret": "client-secret",
            "refresh_token": "refresh-token",
            "token_uri": format!("{base}/token")
        }),
    )
    .await;
    let resolver = GoogleAdcResolver::new(GoogleAdcEnvironment {
        application_credentials: Some(path),
        project_id: Some("env-project".into()),
        ..GoogleAdcEnvironment::default()
    })
    .unwrap();

    resolver.resolve().await.unwrap();
    resolver.resolve().await.unwrap();
    server.await.unwrap();
    assert_eq!(requests.lock().await.len(), 2);
}

#[tokio::test]
async fn service_account_uses_signed_jwt_bearer_exchange() {
    let (base, requests, server) = token_server(1, 3_600).await;
    let temp = TempDir::new().unwrap();
    let token_uri = format!("{base}/service-token");
    let path = write_adc(
        &temp,
        json!({
            "type": "service_account",
            "client_email": "agent@example.iam.gserviceaccount.com",
            "private_key": TEST_PRIVATE_KEY,
            "token_uri": token_uri,
            "project_id": "service-project"
        }),
    )
    .await;
    let resolver = GoogleAdcResolver::new(GoogleAdcEnvironment {
        application_credentials: Some(path),
        location: Some("asia-south1".into()),
        ..GoogleAdcEnvironment::default()
    })
    .unwrap();

    let credential = resolver.resolve().await.unwrap();
    server.await.unwrap();
    assert_eq!(credential.project_id, "service-project");
    assert_eq!(credential.location, "asia-south1");
    let body = &requests.lock().await[0].body;
    assert!(body.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer"));
    let assertion = body
        .split('&')
        .find_map(|field| field.strip_prefix("assertion="))
        .expect("assertion");
    // JWT-safe base64 does not require percent decoding for this generated value.
    let segments: Vec<_> = assertion.split('.').collect();
    assert_eq!(segments.len(), 3);
    let claims: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(segments[1]).unwrap()).unwrap();
    assert_eq!(claims["iss"], "agent@example.iam.gserviceaccount.com");
    assert_eq!(
        claims["scope"],
        "https://www.googleapis.com/auth/cloud-platform"
    );
    assert_eq!(claims["aud"], format!("{base}/service-token"));
    assert!(claims["exp"].as_u64().unwrap() > claims["iat"].as_u64().unwrap());
    assert!(!segments[2].is_empty());
}

#[tokio::test]
async fn metadata_requires_flavor_header_and_caches_attached_identity() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        for (path, body, content_type) in [
            (
                "/computeMetadata/v1/project/project-id",
                "metadata-project",
                "text/plain",
            ),
            (
                "/computeMetadata/v1/instance/service-accounts/default/token",
                r#"{"access_token":"metadata-token","expires_in":1200}"#,
                "application/json",
            ),
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            assert!(request.head.starts_with(&format!("GET {path} HTTP/1.1")));
            assert!(
                request
                    .head
                    .to_ascii_lowercase()
                    .contains("metadata-flavor: google")
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\nMetadata-Flavor: Google\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let resolver = GoogleAdcResolver::new(GoogleAdcEnvironment::default())
        .unwrap()
        .with_metadata_base_url(format!("http://{address}/computeMetadata/v1"))
        .unwrap();
    let first = resolver.resolve().await.unwrap();
    let second = resolver.resolve().await.unwrap();
    task.await.unwrap();
    assert_eq!(first.project_id, "metadata-project");
    assert_eq!(first.location, "us-central1");
    assert_eq!(second.exposed_access_token(), "metadata-token");
}

#[test]
fn metadata_and_oauth_ssrf_inputs_fail_closed() {
    let resolver = GoogleAdcResolver::new(GoogleAdcEnvironment::default()).unwrap();
    assert!(matches!(
        resolver.with_metadata_base_url("http://169.254.169.254/latest/meta-data"),
        Err(GoogleAdcError::InvalidConfiguration(_))
    ));
}

#[tokio::test]
async fn credential_file_cannot_exfiltrate_refresh_secret_to_arbitrary_https_host() {
    let temp = TempDir::new().unwrap();
    let path = write_adc(
        &temp,
        json!({
            "type": "authorized_user",
            "client_id": "client-id",
            "client_secret": "client-secret",
            "refresh_token": "refresh-token",
            "token_uri": "https://attacker.example/token"
        }),
    )
    .await;
    let resolver = GoogleAdcResolver::new(GoogleAdcEnvironment {
        application_credentials: Some(path),
        project_id: Some("env-project".into()),
        ..GoogleAdcEnvironment::default()
    })
    .unwrap();
    assert!(matches!(
        resolver.resolve().await,
        Err(GoogleAdcError::InvalidConfiguration(_))
    ));
}
