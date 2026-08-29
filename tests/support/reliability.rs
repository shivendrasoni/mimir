use std::{
    collections::BTreeSet,
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
};

use mimir::{
    model::{Content, Message, ModelRequest},
    session::{SessionPayload, SessionRecord},
    tools::{ObservationStatus, ToolRegistry},
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
    task::JoinHandle,
};
use walkdir::WalkDir;

pub struct OneShotSse {
    pub base_url: String,
    task: JoinHandle<()>,
}

impl OneShotSse {
    pub async fn finish(self) {
        self.task.await.expect("loopback SSE fixture");
    }
}

pub async fn serve_sse(body: impl Into<Vec<u8>>) -> OneShotSse {
    let body = body.into();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback fixture");
    let address = listener.local_addr().expect("fixture address");
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept fixture request");
        let mut request = vec![0_u8; 16 * 1024];
        let _ = socket
            .read(&mut request)
            .await
            .expect("read fixture request");
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket
            .write_all(headers.as_bytes())
            .await
            .expect("write fixture headers");
        socket.write_all(&body).await.expect("write fixture body");
    });
    OneShotSse {
        base_url: format!("http://{address}"),
        task,
    }
}

pub fn minimal_request() -> ModelRequest {
    ModelRequest {
        model: "reliability-fixture".into(),
        thinking_level: mimir::model::ThinkingLevel::Off,
        thinking_effort: None,
        system_prompt: String::new(),
        messages: vec![Message::user("fixture")],
        tools: Vec::new(),
        max_output_tokens: 64,
    }
}

pub async fn repeat_process(
    tools: Arc<ToolRegistry>,
    program: &str,
    count: usize,
) -> Result<(), String> {
    for iteration in 0..count {
        let observation = tools
            .execute(
                "run_process",
                serde_json::json!({"program": program, "args": []}),
            )
            .await
            .map_err(|error| format!("iteration {iteration}: {error}"))?;
        if observation.status != ObservationStatus::Success {
            return Err(format!(
                "iteration {iteration}: {}: {}",
                observation.summary, observation.content
            ));
        }
    }
    Ok(())
}

pub fn append_torn_json_tail(path: &Path) {
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open session fixture");
    file.write_all(b"{\"schema_version\":1,\"record_id\":")
        .expect("write torn tail");
}

pub fn orphan_tool_call_ids(records: &[SessionRecord]) -> Vec<String> {
    let mut pending = BTreeSet::new();
    for record in records {
        let SessionPayload::Message(message) = &record.payload else {
            continue;
        };
        for content in &message.content {
            match content {
                Content::ToolCall(call) => {
                    pending.insert(call.id.clone());
                }
                Content::ToolResult(result) => {
                    pending.remove(&result.tool_call_id);
                }
                Content::Text { .. } | Content::Image { .. } | Content::Thinking { .. } => {}
            }
        }
    }
    pending.into_iter().collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeakFinding {
    pub relative_path: PathBuf,
    pub marker: String,
}

pub fn scan_for_literal_leaks(
    root: &Path,
    markers: &[(&str, &str)],
) -> std::io::Result<Vec<LeakFinding>> {
    let mut findings = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(std::io::Error::other)?;
        if !entry.file_type().is_file() || entry.metadata()?.len() > 2 * 1024 * 1024 {
            continue;
        }
        let bytes = std::fs::read(entry.path())?;
        let text = String::from_utf8_lossy(&bytes);
        for (label, literal) in markers {
            if !literal.is_empty() && text.contains(literal) {
                findings.push(LeakFinding {
                    relative_path: entry
                        .path()
                        .strip_prefix(root)
                        .unwrap_or(entry.path())
                        .to_owned(),
                    marker: (*label).to_owned(),
                });
            }
        }
    }
    findings.sort_by(|left, right| {
        left.relative_path
            .cmp(&right.relative_path)
            .then_with(|| left.marker.cmp(&right.marker))
    });
    Ok(findings)
}
