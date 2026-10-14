//! Standalone paths must stay complete when the optional company runner is absent.
use assert_cmd::Command;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use hmac::{Hmac, Mac};
use predicates::prelude::*;
use ring::{
    rand::SystemRandom,
    signature::{Ed25519KeyPair, KeyPair},
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn enrolled_release(root: &std::path::Path, native: serde_json::Value, expired: bool) {
    let key = Ed25519KeyPair::from_pkcs8(
        Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .expect("key document")
            .as_ref(),
    )
    .expect("key");
    let organization = uuid::Uuid::new_v4();
    let team = uuid::Uuid::new_v4();
    let now = chrono::Utc::now();
    let expires = if expired {
        now - chrono::Duration::hours(1)
    } else {
        now + chrono::Duration::hours(1)
    };
    let payload = serde_json::to_vec(&serde_json::json!({"schema":4,"release_id":uuid::Uuid::new_v4(),"organization_id":organization,"team_id":team,"project_id":null,"profile_name":"Native test rules","created_at":now-chrono::Duration::hours(2),"expires_at":expires,"minimum_setup_version":"1.0.0","minimum_runtime_versions":{"mimir":"0.0.0"},"policies":[],"instances":[],"native_constraints":native,"skills":[],"telemetry":[]})).expect("release bytes");
    let envelope = serde_json::json!({"schema":2,"payload_base64":BASE64.encode(&payload),"sha256":format!("{:x}",Sha256::digest(&payload)),"key_id":uuid::Uuid::new_v4(),"signature":BASE64.encode(key.sign(&payload).as_ref())});
    let enrollment = serde_json::json!({"schema":1,"endpoint":"http://127.0.0.1:1","installation_id":uuid::Uuid::new_v4(),"installation_token":format!("bli_{}",uuid::Uuid::new_v4().simple()),"organization_id":organization,"team_id":team,"profile_id":uuid::Uuid::new_v4(),"channel":"stable","signing_public_key_base64":BASE64.encode(key.public_key().as_ref()),"etag":null});
    std::fs::write(
        root.join("enrollment.json"),
        serde_json::to_vec(&enrollment).expect("connection"),
    )
    .expect("connection file");
    std::fs::write(
        root.join("profile.json"),
        serde_json::to_vec(&envelope).expect("envelope"),
    )
    .expect("release file");
}

struct StandaloneFixture {
    workspace: TempDir,
    state: TempDir,
    home: TempDir,
    enterprise: TempDir,
    empty_path: TempDir,
}
impl StandaloneFixture {
    fn new() -> Self {
        Self {
            workspace: TempDir::new().expect("workspace"),
            state: TempDir::new().expect("state"),
            home: TempDir::new().expect("isolated user configuration"),
            enterprise: TempDir::new().expect("unenrolled enterprise state"),
            empty_path: TempDir::new().expect("PATH without Node or Betterloop"),
        }
    }
    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("mimir").expect("native Mimir binary");
        command
            .env_clear()
            .env("HOME", self.home.path())
            .env("PATH", self.empty_path.path())
            .env("MIMIR_ENTERPRISE_STATE_DIR", self.enterprise.path())
            .args([
                "--workspace",
                self.workspace.path().to_str().expect("workspace"),
                "--state-dir",
                self.state.path().to_str().expect("state"),
            ]);
        command
    }
    fn assert_no_enrollment(&self) {
        assert!(!self.enterprise.path().join("enrollment.json").exists());
        assert!(!self.enterprise.path().join("device-broker.json").exists());
        assert!(!self.enterprise.path().join("profile.json").exists());
    }
}

#[test]
fn standalone_help_and_provider_discovery_require_no_node_or_company_setup() {
    let fixture = StandaloneFixture::new();
    fixture
        .command()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("--provider"));
    fixture
        .command()
        .arg("providers")
        .assert()
        .success()
        .stdout(predicate::str::contains("anthropic"));
    fixture.assert_no_enrollment();
}

#[test]
fn standalone_fake_provider_runs_and_persists_sessions_without_node_or_company_credentials() {
    let fixture = StandaloneFixture::new();
    fixture
        .command()
        .args([
            "--provider",
            "fake",
            "--session",
            "standalone-policy-regression",
            "--fake-response",
            "independent runtime works",
            "--print",
            "hello",
        ])
        .assert()
        .success()
        .stdout("independent runtime works\n");
    let root =
        mimir::learning::project_session_root(fixture.state.path(), fixture.workspace.path())
            .expect("project sessions");
    assert!(
        root.join("sessions/standalone-policy-regression.jsonl")
            .is_file()
    );
    fixture.assert_no_enrollment();
}

#[test]
fn standalone_provider_login_status_and_logout_do_not_require_company_identity() {
    let fixture = StandaloneFixture::new();
    let synthetic_key = "standalone-provider-fixture-value";
    fixture
        .command()
        .args(["login", "openai", "--api-key-stdin"])
        .write_stdin(format!("{synthetic_key}\n"))
        .assert()
        .success()
        .stdout(predicate::str::contains(synthetic_key).not());
    fixture
        .command()
        .args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("openai"))
        .stdout(predicate::str::contains(synthetic_key).not());
    fixture
        .command()
        .args(["logout", "openai"])
        .assert()
        .success()
        .stdout(predicate::str::contains("logged_out"));
    fixture.assert_no_enrollment();
}

#[test]
fn standalone_ignores_orphaned_unavailable_policy_runtime_assets_without_enrollment() {
    let fixture = StandaloneFixture::new();
    std::fs::write(fixture.enterprise.path().join("device-broker.json"), r#"{"policy_runtime":{"executable":"/unavailable/node","runner":"/unavailable/runner.mjs"}}"#).expect("orphaned runtime metadata");
    fixture
        .command()
        .args([
            "--provider",
            "fake",
            "--no-session",
            "--fake-response",
            "standalone still works",
            "--print",
            "hello",
        ])
        .assert()
        .success()
        .stdout("standalone still works\n");
    assert!(!fixture.enterprise.path().join("enrollment.json").exists());
}

#[test]
fn standalone_native_tools_remain_available_without_enrollment_or_policy_runtime() {
    let fixture = StandaloneFixture::new();
    fixture
        .command()
        .args(["--provider", "fake", "--no-session", "doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("offline_fake"));
    fixture.assert_no_enrollment();
}

#[test]
fn standalone_native_read_write_tools_execute_without_node_or_company_setup() {
    if std::env::var_os("MIMIR_STANDALONE_POLICY_CHILD").is_some() {
        let workspace = TempDir::new().expect("workspace");
        let registry = mimir::tools::ToolRegistry::with_default_tools(
            workspace.path(),
            mimir::tools::ToolPolicy::default(),
        )
        .expect("native tools");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("async runtime");
        runtime.block_on(async {
            registry
                .execute(
                    "write_file",
                    serde_json::json!({"path":"native-marker.txt","content":"native tools work"}),
                )
                .await
                .expect("native write");
            registry
                .execute("read_file", serde_json::json!({"path":"native-marker.txt"}))
                .await
                .expect("native read");
        });
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("native-marker.txt")).expect("marker"),
            "native tools work"
        );
        return;
    }
    let fixture = StandaloneFixture::new();
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .env_clear()
        .env("HOME", fixture.home.path())
        .env("PATH", fixture.empty_path.path())
        .env("MIMIR_ENTERPRISE_STATE_DIR", fixture.enterprise.path())
        .env("MIMIR_STANDALONE_POLICY_CHILD", "1")
        .args([
            "--exact",
            "standalone_native_read_write_tools_execute_without_node_or_company_setup",
            "--nocapture",
        ])
        .output()
        .expect("isolated native tool test");
    assert!(
        output.status.success(),
        "isolated native tool path failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    fixture.assert_no_enrollment();
}

#[test]
fn enrolled_native_constraints_are_scoped_and_preserved_without_a_node_runner() {
    if std::env::var_os("MIMIR_NATIVE_POLICY_CHILD").is_some() {
        let root = std::path::PathBuf::from(
            std::env::var_os("MIMIR_ENTERPRISE_STATE_DIR").expect("isolated state"),
        );
        enrolled_release(
            &root,
            serde_json::json!({"allowed_tools":{"mimir":["read_file"],"claude-code":["write_file"]},"allowed_shell_programs":{"mimir":["git"]},"approved_models":{"mimir":["provider/approved-model"],"claude-code":["different-model"]},"maximum_reasoning_level":{"mimir":"medium","claude-code":"max"}}),
            false,
        );
        assert!(mimir::enterprise::tool_allowed("read_file").expect("read allowed"));
        assert!(!mimir::enterprise::tool_allowed("write_file").expect("write denied"));
        assert!(mimir::enterprise::shell_program_allowed("git").expect("git allowed"));
        assert!(!mimir::enterprise::shell_program_allowed("printf").expect("printf denied"));
        assert!(
            mimir::enterprise::model_allowed("provider", "approved-model").expect("model allowed")
        );
        assert!(
            !mimir::enterprise::model_allowed("provider", "different-model").expect("model denied")
        );
        assert_eq!(
            mimir::enterprise::maximum_thinking_level().expect("reasoning constraint"),
            Some(mimir::model::ThinkingLevel::Medium)
        );
        return;
    }
    let fixture = StandaloneFixture::new();
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .env_clear()
        .env("HOME", fixture.home.path())
        .env("PATH", fixture.empty_path.path())
        .env("MIMIR_ENTERPRISE_STATE_DIR", fixture.enterprise.path())
        .env("MIMIR_NATIVE_POLICY_CHILD", "1")
        .args([
            "--exact",
            "enrolled_native_constraints_are_scoped_and_preserved_without_a_node_runner",
            "--nocapture",
        ])
        .output()
        .expect("isolated native policy test");
    assert!(
        output.status.success(),
        "native gate test failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn expired_native_rules_block_only_actions_with_relevant_mimir_constraints() {
    if std::env::var_os("MIMIR_EXPIRED_POLICY_CHILD").is_some() {
        let root = std::path::PathBuf::from(
            std::env::var_os("MIMIR_ENTERPRISE_STATE_DIR").expect("isolated state"),
        );
        enrolled_release(
            &root,
            serde_json::json!({"allowed_tools":{"claude-code":[]}}),
            true,
        );
        assert!(
            mimir::enterprise::tool_allowed("read_file")
                .expect("non-target native restriction must not block")
        );
        assert!(
            mimir::enterprise::model_allowed("provider", "any-model")
                .expect("uncovered model action continues")
        );
        assert!(
            mimir::enterprise::maximum_thinking_level()
                .expect("uncovered reasoning action continues")
                .is_none()
        );
        enrolled_release(
            &root,
            serde_json::json!({"allowed_tools":{"mimir":["read_file"]}}),
            true,
        );
        assert!(mimir::enterprise::tool_allowed("read_file").is_err());
        assert!(
            mimir::enterprise::model_allowed("provider", "any-model")
                .expect("other uncovered action continues")
        );
        return;
    }
    let fixture = StandaloneFixture::new();
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .env_clear()
        .env("HOME", fixture.home.path())
        .env("PATH", fixture.empty_path.path())
        .env("MIMIR_ENTERPRISE_STATE_DIR", fixture.enterprise.path())
        .env("MIMIR_EXPIRED_POLICY_CHILD", "1")
        .args([
            "--exact",
            "expired_native_rules_block_only_actions_with_relevant_mimir_constraints",
            "--nocapture",
        ])
        .output()
        .expect("isolated native expiry test");
    assert!(
        output.status.success(),
        "matching-only native expiry failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

fn authenticated_recovery_fixture(
    fixture: &StandaloneFixture,
    endpoint: &str,
    mutation: Option<&str>,
) {
    let key = Ed25519KeyPair::from_pkcs8(
        Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .expect("key document")
            .as_ref(),
    )
    .expect("key");
    let organization = uuid::Uuid::new_v4();
    let team = uuid::Uuid::new_v4();
    let installation = uuid::Uuid::new_v4();
    let release = uuid::Uuid::new_v4();
    let token = format!("bli_{}", uuid::Uuid::new_v4().simple());
    let workspace = fixture.workspace.path().canonicalize().expect("workspace");
    let enterprise = fixture.enterprise.path().canonicalize().expect("state");
    let alias = format!(
        "{:x}",
        Sha256::digest(workspace.to_string_lossy().as_bytes())
    );
    let marker = workspace.join("cached-program-executed.txt");
    // If cached code were evaluated by a Node host during recovery, this would
    // produce an observable side effect before any policy decision.
    let program = format!(
        "require('node:fs').writeFileSync({}, 'cached code executed'); throw new Error('cached code must never execute');",
        serde_json::to_string(&marker.to_string_lossy()).expect("marker literal")
    );
    let definition = "d".repeat(64);
    let now = chrono::Utc::now();
    let payload = serde_json::to_vec(&serde_json::json!({
        "schema":4,"release_id":release,"organization_id":organization,"team_id":team,
        "project_id":null,"profile_name":"Recovered match descriptors",
        "created_at":now,"expires_at":now+chrono::Duration::hours(1),
        "minimum_setup_version":"1.0.0","minimum_runtime_versions":{"mimir":"0.28.0"},
        "policies":[{
            "manifest":{"schema":1,"id":"protected-bash","version":"1.0.0",
                "description":"Protect matching Bash commands","sdk_version":"1.0.0",
                "match":{"events":["tool_call"],"tools":["bash"]},
                "parameters":{"type":"object","properties":{},"additionalProperties":false},
                "requires":[],"effects":["allow","deny"],"reason_codes":["blocked"]},
            "program":program,"sha256":format!("{:x}",Sha256::digest(program.as_bytes())),
            "definition_sha256":definition}],
        "instances":[{"id":"protected-bash-instance",
            "definition":{"id":"protected-bash","version":"1.0.0","sha256":definition},
            "parameters":{},"target_agents":["mimir"],"requirement":"required",
            "mode":"enforce","on_error":"deny","order":0}],
        "native_constraints":{},"skills":[],"telemetry":[]
    }))
    .expect("release bytes");
    let mut envelope = serde_json::json!({"schema":2,"payload_base64":BASE64.encode(&payload),
        "sha256":format!("{:x}",Sha256::digest(&payload)),"key_id":uuid::Uuid::new_v4(),
        "signature":BASE64.encode(key.sign(&payload).as_ref())});
    if mutation == Some("signature") {
        envelope["signature"] = serde_json::json!(BASE64.encode([0u8; 64]));
    }
    let enrollment = serde_json::json!({"schema":1,"endpoint":endpoint,
        "installation_id":installation,"installation_token":token,"organization_id":organization,
        "team_id":team,"profile_id":uuid::Uuid::new_v4(),"channel":"stable",
        "signing_public_key_base64":BASE64.encode(key.public_key().as_ref()),"etag":null});
    let mut snapshot = serde_json::json!({"schema":1,"installation_id":installation,
        "organization_id":organization,"team_id":team,"project_alias":alias,
        "release_digest":envelope["sha256"],"envelope":envelope,
        "activation":{"schema":1,"release_id":release,"release_digest":envelope["sha256"],
            "runtime":{"harness":"mimir","harness_version":env!("CARGO_PKG_VERSION"),"adapter_version":"1.0.0"},
            "policies":[{"instance_id":"protected-bash-instance","state":"active","reason_code":"activated"}]}});
    if mutation == Some("scope") {
        snapshot["team_id"] = serde_json::json!(uuid::Uuid::new_v4());
    }
    if mutation == Some("project") {
        snapshot["project_alias"] = serde_json::json!("0".repeat(64));
    }
    let bytes = serde_json::to_vec(&snapshot).expect("snapshot bytes");
    let mut mac = Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("installation HMAC");
    mac.update(&bytes);
    snapshot["authentication_input"] = serde_json::json!(BASE64.encode(&bytes));
    snapshot["authentication"] = serde_json::json!(if mutation == Some("authentication") {
        "0".repeat(64)
    } else {
        format!("{:x}", mac.finalize().into_bytes())
    });
    let projects = enterprise.join("projects");
    let activation = projects.join(alias).join("mimir/policy-activation.json");
    std::fs::create_dir_all(activation.parent().expect("activation parent"))
        .expect("activation directory");
    std::fs::write(
        &activation,
        serde_json::to_vec(&snapshot).expect("snapshot"),
    )
    .expect("authenticated snapshot");
    std::fs::write(
        enterprise.join("enrollment.json"),
        serde_json::to_vec(&enrollment).expect("enrollment"),
    )
    .expect("enrollment file");
    std::fs::write(enterprise.join("profile.json"), b"damaged primary cache")
        .expect("damaged cache");
    std::fs::write(enterprise.join("device-broker.json"), serde_json::to_vec(&serde_json::json!({
        "executable":"/unavailable/node","runner":"/unavailable/setup.mjs",
        "policy_activation_root":projects,"policy_activation":activation,
        "policy_runtime":{"executable":"/unavailable/node","runner":"/unavailable/policy-child.mjs"}
    })).expect("broker"))
        .expect("unavailable runner descriptor");
    std::fs::write(
        workspace.join("legitimate.txt"),
        "unrelated work remains available",
    )
    .expect("legitimate file");
}

fn recovery_child(fixture: &StandaloneFixture, test: &str, mode: &str) {
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .env_clear()
        .env("HOME", fixture.home.path())
        .env("PATH", fixture.empty_path.path())
        .env(
            "MIMIR_ENTERPRISE_STATE_DIR",
            fixture.enterprise.path().canonicalize().expect("state"),
        )
        .env(
            "MIMIR_RECOVERY_WORKSPACE",
            fixture.workspace.path().canonicalize().expect("workspace"),
        )
        .env("MIMIR_RECOVERY_CHILD", mode)
        .args(["--exact", test, "--nocapture"])
        .output()
        .expect("isolated native recovery test");
    assert!(
        output.status.success(),
        "native recovery failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn damaged_primary_cache_recovers_matching_denial_without_executing_cached_code() {
    if std::env::var_os("MIMIR_RECOVERY_CHILD").is_some() {
        let workspace = std::path::PathBuf::from(
            std::env::var_os("MIMIR_RECOVERY_WORKSPACE").expect("workspace"),
        );
        mimir::enterprise::bind_repository(&workspace).expect("captured workspace");
        let manager = mimir::enterprise::EnterpriseManager::global().expect("native manager");
        assert!(
            manager.load_profile().is_err(),
            "strict cache verification must remain strict"
        );
        let recovered = manager
            .load_runtime_profile()
            .expect("authenticated signed match descriptors");
        assert_eq!(recovered.instances.len(), 1);
        let registry = mimir::tools::ToolRegistry::with_default_tools(
            &workspace,
            mimir::tools::ToolPolicy {
                allow_shell: true,
                ..mimir::tools::ToolPolicy::default()
            },
        )
        .expect("native tool registry");
        assert!(
            registry
                .definitions()
                .iter()
                .any(|tool| tool.name == "bash"),
            "test must reach a registered Bash tool"
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("async runtime");
        runtime.block_on(async {
            assert!(mimir::enterprise::prepare_runtime(true).await.expect("offline recovered startup").is_some());
            let read = registry.execute("read_file", serde_json::json!({"path":"legitimate.txt"})).await.expect("unrelated native read");
            assert!(read.content.contains("unrelated work remains available"));
            let denied = registry.execute("bash", serde_json::json!({"command":"printf executed > protected-command-executed.txt"})).await.expect_err("matching command must fail closed");
            assert!(denied.to_string().contains("enabled policy could not evaluate"), "must deny through recovered match metadata: {denied}");
        });
        assert!(!workspace.join("protected-command-executed.txt").exists());
        assert!(!workspace.join("cached-program-executed.txt").exists());
        assert!(
            manager.load_profile().is_err(),
            "recovery must not replace or bless the damaged primary cache"
        );
        return;
    }
    let fixture = StandaloneFixture::new();
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("disposable unenrollment service");
    let endpoint = format!(
        "http://{}",
        listener.local_addr().expect("local service address")
    );
    authenticated_recovery_fixture(&fixture, &endpoint, None);
    recovery_child(
        &fixture,
        "damaged_primary_cache_recovers_matching_denial_without_executing_cached_code",
        "active",
    );
    fixture
        .command()
        .env(
            "MIMIR_ENTERPRISE_STATE_DIR",
            fixture.enterprise.path().canonicalize().expect("state"),
        )
        .args([
            "--offline",
            "--provider",
            "fake",
            "--no-session",
            "--fake-response",
            "recovered startup works",
            "--print",
            "hello",
        ])
        .assert()
        .success()
        .stdout("recovered startup works\n");
    let server = std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        listener.set_nonblocking(true).expect("bounded server");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                Err(error) => panic!("unenrollment request did not arrive: {error}"),
            }
        };
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("bounded request");
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 1024];
        while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
            let size = socket.read(&mut buffer).expect("unenrollment request");
            assert!(size > 0 && bytes.len() < 16 * 1024, "bounded HTTP request");
            bytes.extend_from_slice(&buffer[..size]);
        }
        assert!(String::from_utf8_lossy(&bytes).starts_with("DELETE /v1/harness/enrollment "));
        socket
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .expect("unenrollment response");
    });
    fixture
        .command()
        .args(["enterprise", "unenroll"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"unenrolled\": true"));
    server.join().expect("observed explicit unenrollment");
    fixture.assert_no_enrollment();
    fixture
        .command()
        .args([
            "--offline",
            "--provider",
            "fake",
            "--no-session",
            "--fake-response",
            "standalone restored",
            "--print",
            "hello",
        ])
        .assert()
        .success()
        .stdout("standalone restored\n");
    assert!(
        !fixture
            .workspace
            .path()
            .join("cached-program-executed.txt")
            .exists()
    );
    assert!(
        !fixture
            .workspace
            .path()
            .join("protected-command-executed.txt")
            .exists()
    );
}

#[test]
fn damaged_primary_cache_rejects_untrusted_or_wrong_scope_recovery_snapshots() {
    if std::env::var_os("MIMIR_RECOVERY_CHILD").is_some() {
        let workspace = std::path::PathBuf::from(
            std::env::var_os("MIMIR_RECOVERY_WORKSPACE").expect("workspace"),
        );
        mimir::enterprise::bind_repository(&workspace).expect("workspace");
        let manager = mimir::enterprise::EnterpriseManager::global().expect("manager");
        assert!(manager.load_profile().is_err());
        assert!(
            manager.load_runtime_profile().is_err(),
            "unauthenticated or mismatched metadata cannot recover a damaged cache"
        );
        assert!(!workspace.join("cached-program-executed.txt").exists());
        return;
    }
    for mutation in ["authentication", "scope", "project", "signature"] {
        let fixture = StandaloneFixture::new();
        authenticated_recovery_fixture(&fixture, "http://127.0.0.1:1", Some(mutation));
        recovery_child(
            &fixture,
            "damaged_primary_cache_rejects_untrusted_or_wrong_scope_recovery_snapshots",
            mutation,
        );
    }
}

#[cfg(unix)]
fn boundary_fixture(fixture: &StandaloneFixture, event: &str) {
    let key = Ed25519KeyPair::from_pkcs8(
        Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .expect("key")
            .as_ref(),
    )
    .expect("key");
    let organization = uuid::Uuid::new_v4();
    let team = uuid::Uuid::new_v4();
    let installation = uuid::Uuid::new_v4();
    let release = uuid::Uuid::new_v4();
    let enrollment_fixture = "bli_local_boundary_fixture";
    let workspace = fixture.workspace.path().canonicalize().expect("workspace");
    let state = fixture.enterprise.path().canonicalize().expect("state");
    let alias = format!(
        "{:x}",
        Sha256::digest(workspace.to_string_lossy().as_bytes())
    );
    let now = chrono::Utc::now();
    let program = "var __betterloopPolicy={evaluate:()=>({outcome:'deny',reason_code:'blocked'})}";
    let payload = serde_json::to_vec(&serde_json::json!({
        "schema":4,"release_id":release,"organization_id":organization,"team_id":team,"project_id":null,
        "profile_name":"Boundary fixture","created_at":now,"expires_at":now+chrono::Duration::hours(1),
        "minimum_setup_version":"1.0.0","minimum_runtime_versions":{"mimir":"0.29.0"},
        "policies":[{"manifest":{"schema":1,"id":"boundary","version":"1.0.0","sdk_version":"1.0.0",
            "description":"Boundary fixture","match":{"events":[event]},"parameters":{"type":"object"},
            "requires":[],"effects":["allow","deny"],"reason_codes":["allowed","blocked"]},
            "program":program,"sha256":format!("{:x}",Sha256::digest(program)),"definition_sha256":"d".repeat(64)}],
        "instances":[{"id":"boundary-instance","definition":{"id":"boundary","version":"1.0.0","sha256":"d".repeat(64)},
            "parameters":{},"target_agents":["mimir"],"requirement":"required","mode":"enforce","on_error":"deny","order":0}],
        "native_constraints":{},"skills":[],"telemetry":[]
    })).expect("release");
    let envelope = serde_json::json!({"schema":2,"payload_base64":BASE64.encode(&payload),"sha256":format!("{:x}",Sha256::digest(&payload)),
        "key_id":uuid::Uuid::new_v4(),"signature":BASE64.encode(key.sign(&payload).as_ref())});
    let enrollment = serde_json::json!({"schema":1,"endpoint":"http://127.0.0.1:1","installation_id":installation,
        "installation_token":enrollment_fixture,"organization_id":organization,"team_id":team,"profile_id":uuid::Uuid::new_v4(),
        "channel":"stable","signing_public_key_base64":BASE64.encode(key.public_key().as_ref()),"etag":null});
    let runner = state.join("policy-child.mjs");
    // Deterministic broker fixture, not a replacement for actual-host QuickJS
    // conformance: inspect the exact result/completion boundary and bind replies.
    let script = r#"IFS= read -r input || :
identity=$(printf '%s' "$input" | /usr/bin/sed -n 's/^{"identity":\({[^}]*}\),.*/\1/p')
request=$(printf '%s' "$identity" | /usr/bin/sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
release=$(printf '%s' "$identity" | /usr/bin/sed -n 's/.*"release_id":"\([^"]*\)".*/\1/p')
digest=$(printf '%s' "$identity" | /usr/bin/sed -n 's/.*"release_digest":"\([^"]*\)".*/\1/p')
outcome=deny
case "$input" in *'"assistant_message":"verified"'*) outcome=allow ;; esac
printf '{"identity":%s,"result":{"schema":1,"request_id":"%s","release_id":"%s","release_digest":"%s","outcome":"%s","results":[{"instance_id":"boundary-instance","outcome":"%s","message":"Complete the reviewed checks","reason_code":"blocked","status":"evaluated"}],"guidance":[]}}' "$identity" "$request" "$release" "$digest" "$outcome" "$outcome"
"#;
    std::fs::write(&runner, script).expect("runner");
    let manifest=serde_json::to_vec(&serde_json::json!({"files":[{"path":"policy-child.mjs","sha256":format!("{:x}",Sha256::digest(script))}]})).expect("manifest");
    std::fs::write(state.join("runtime-manifest.json"), &manifest).expect("manifest");
    let executable = std::fs::canonicalize("/bin/sh").expect("shell fixture");
    let descriptor = serde_json::json!({"schema":1,"executable":executable,"runner":runner,"node_version":"22.0.0",
        "executable_sha256":format!("{:x}",Sha256::digest(std::fs::read(&executable).expect("executable"))),
        "manifest_sha256":format!("{:x}",Sha256::digest(&manifest))});
    let mut snapshot = serde_json::json!({"schema":1,"installation_id":installation,"organization_id":organization,"team_id":team,
        "project_alias":alias,"release_digest":envelope["sha256"],"envelope":envelope,"runtime_manifest_sha256":descriptor["manifest_sha256"],
        "activation":{"schema":1,"release_id":release,"release_digest":envelope["sha256"],
            "runtime":{"harness":"mimir","harness_version":env!("CARGO_PKG_VERSION"),"adapter_version":"1.0.0"},
            "policies":[{"instance_id":"boundary-instance","state":"active","reason_code":"ready"}]}});
    let bytes = serde_json::to_vec(&snapshot).expect("snapshot");
    let mut mac = Hmac::<Sha256>::new_from_slice(enrollment_fixture.as_bytes()).expect("HMAC");
    mac.update(&bytes);
    snapshot["authentication_input"] = serde_json::json!(BASE64.encode(&bytes));
    snapshot["authentication"] = serde_json::json!(format!("{:x}", mac.finalize().into_bytes()));
    let projects = state.join("projects");
    let activation = projects.join(alias).join("mimir/policy-activation.json");
    std::fs::create_dir_all(activation.parent().expect("parent")).expect("activation dir");
    for (file, value) in [
        (&activation, snapshot),
        (&state.join("enrollment.json"), enrollment),
        (&state.join("profile.json"), envelope),
        (
            &state.join("device-broker.json"),
            serde_json::json!({"policy_activation_root":projects,"policy_activation":activation,"policy_runtime":descriptor}),
        ),
    ] {
        std::fs::write(file, serde_json::to_vec(&value).expect("JSON")).expect("fixture file");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn enrolled_result_denial_withholds_original_from_provider_and_history() {
    if std::env::var_os("MIMIR_BOUNDARY_CHILD").is_some() {
        use mimir::{
            model::{Content, Message, ModelResponse, StopReason, ToolCall},
            provider::FakeProvider,
            runtime::{AgentRuntime, RuntimeConfig, VecEventSink},
            session::InMemorySessionStore,
            tools::{ToolPolicy, ToolRegistry},
        };
        use std::sync::Arc;
        let workspace = std::path::PathBuf::from(
            std::env::var_os("MIMIR_BOUNDARY_WORKSPACE").expect("workspace"),
        );
        let sensitive_fixture = "synthetic-secret-must-never-enter-model";
        std::fs::write(workspace.join("sensitive.txt"), sensitive_fixture).expect("file");
        let provider = Arc::new(FakeProvider::new(vec![
            ModelResponse {
                message: Message::assistant(
                    vec![Content::ToolCall(ToolCall {
                        id: "read-1".into(),
                        name: "read_file".into(),
                        arguments: serde_json::json!({"path":"sensitive.txt"}),
                    })],
                    StopReason::ToolUse,
                ),
                response_id: None,
            },
            ModelResponse {
                message: Message::assistant(
                    vec![Content::Text {
                        text: "verified".into(),
                    }],
                    StopReason::Stop,
                ),
                response_id: None,
            },
        ]));
        let runtime = AgentRuntime::resume(
            provider.clone(),
            Arc::new(
                ToolRegistry::with_default_tools(&workspace, ToolPolicy::default()).expect("tools"),
            ),
            Arc::new(InMemorySessionStore::default()),
            RuntimeConfig::default_for_model("fake-model"),
        )
        .await
        .expect("runtime");
        assert_eq!(
            runtime
                .run("read fixture", &VecEventSink::default())
                .await
                .expect("run"),
            "verified"
        );
        assert_eq!(provider.requests().await.len(), 2);
        for request in provider.requests().await {
            assert!(!format!("{request:?}").contains(sensitive_fixture));
        }
        assert!(!format!("{:?}", runtime.messages_snapshot().await).contains(sensitive_fixture));
        assert!(
            format!("{:?}", runtime.messages_snapshot().await)
                .contains("Complete the reviewed checks")
        );
        return;
    }
    let fixture = StandaloneFixture::new();
    boundary_fixture(&fixture, "tool_result");
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .env_clear()
        .env("HOME", fixture.home.path())
        .env("PATH", fixture.empty_path.path())
        .env(
            "MIMIR_ENTERPRISE_STATE_DIR",
            fixture
                .enterprise
                .path()
                .canonicalize()
                .expect("canonical state"),
        )
        .env("MIMIR_BOUNDARY_CHILD", "1")
        .env("MIMIR_BOUNDARY_WORKSPACE", fixture.workspace.path())
        .args([
            "--exact",
            "enrolled_result_denial_withholds_original_from_provider_and_history",
            "--nocapture",
        ])
        .output()
        .expect("child");
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn enrolled_completion_denial_remediates_and_bounds_retries() {
    if std::env::var_os("MIMIR_COMPLETION_BOUNDARY_CHILD").is_some() {
        use mimir::{
            model::{Content, Message, ModelResponse, StopReason},
            provider::FakeProvider,
            runtime::{AgentRuntime, RuntimeConfig, VecEventSink},
            session::InMemorySessionStore,
            tools::{ToolPolicy, ToolRegistry},
        };
        use std::sync::Arc;
        let workspace = std::path::PathBuf::from(
            std::env::var_os("MIMIR_BOUNDARY_WORKSPACE").expect("workspace"),
        );
        for (texts, success, requests) in [
            (vec!["unchecked", "verified"], true, 2),
            (vec!["unchecked"; 4], false, 4),
        ] {
            let provider = Arc::new(FakeProvider::new(
                texts
                    .into_iter()
                    .map(|text| ModelResponse {
                        message: Message::assistant(
                            vec![Content::Text { text: text.into() }],
                            StopReason::Stop,
                        ),
                        response_id: None,
                    })
                    .collect(),
            ));
            let runtime = AgentRuntime::resume(
                provider.clone(),
                Arc::new(
                    ToolRegistry::with_default_tools(&workspace, ToolPolicy::default())
                        .expect("tools"),
                ),
                Arc::new(InMemorySessionStore::default()),
                RuntimeConfig::default_for_model("fake-model"),
            )
            .await
            .expect("runtime");
            let sink = VecEventSink::default();
            let result = runtime.run("finish fixture", &sink).await;
            assert_eq!(result.is_ok(), success, "{result:?}");
            assert_eq!(provider.requests().await.len(), requests);
            if success {
                assert_eq!(result.expect("completed"), "verified");
            } else {
                assert!(
                    !sink.events().await.iter().any(|event| matches!(
                        event,
                        mimir::runtime::RuntimeEvent::Completed { .. }
                    ))
                );
            }
        }
        return;
    }
    let fixture = StandaloneFixture::new();
    boundary_fixture(&fixture, "before_turn_end");
    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .env_clear()
        .env("HOME", fixture.home.path())
        .env("PATH", fixture.empty_path.path())
        .env(
            "MIMIR_ENTERPRISE_STATE_DIR",
            fixture
                .enterprise
                .path()
                .canonicalize()
                .expect("canonical state"),
        )
        .env("MIMIR_COMPLETION_BOUNDARY_CHILD", "1")
        .env("MIMIR_BOUNDARY_WORKSPACE", fixture.workspace.path())
        .args([
            "--exact",
            "enrolled_completion_denial_remediates_and_bounds_retries",
            "--nocapture",
        ])
        .output()
        .expect("child");
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
