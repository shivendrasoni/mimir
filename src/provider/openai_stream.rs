use std::collections::BTreeMap;

use futures::StreamExt;
use secrecy::ExposeSecret;
use serde_json::Value;

use crate::model::{Content, Message, ModelRequest, ModelResponse, StopReason, ToolCall, Usage};

use super::{OpenAiProvider, ProviderError, ProviderEvent, ProviderEventSink};

const MAX_STREAM_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
struct ToolCallBuilder {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct StreamAccumulator {
    id: Option<String>,
    text: String,
    tools: BTreeMap<u64, ToolCallBuilder>,
    usage: Usage,
    stop_reason: StopReason,
}

impl StreamAccumulator {
    async fn apply(
        &mut self,
        value: &Value,
        sink: &dyn ProviderEventSink,
    ) -> Result<(), ProviderError> {
        if self.id.is_none() {
            self.id = value.get("id").and_then(Value::as_str).map(str::to_owned);
        }
        if let Some(usage) = value.get("usage") {
            self.usage = Usage {
                input_tokens: usage
                    .get("prompt_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.usage.input_tokens),
                output_tokens: usage
                    .get("completion_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.usage.output_tokens),
                cached_tokens: usage
                    .pointer("/prompt_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.usage.cached_tokens),
                cache_write_tokens: 0,
            };
        }
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(());
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop_reason = match reason {
                "tool_calls" | "function_call" => StopReason::ToolUse,
                "length" => StopReason::Length,
                "stop" => StopReason::Stop,
                _ => StopReason::Error,
            };
        }
        let delta = &choice["delta"];
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            self.text.push_str(text);
            sink.emit(ProviderEvent::TextDelta(text.into())).await;
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                let builder = self.tools.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    builder.id.push_str(id);
                }
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    builder.name.push_str(name);
                }
                if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str)
                {
                    builder.arguments.push_str(arguments);
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<ModelResponse, ProviderError> {
        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(Content::Text { text: self.text });
        }
        for (_, tool) in self.tools {
            if tool.id.is_empty() || tool.name.is_empty() {
                return Err(ProviderError::Protocol {
                    message: "stream ended with an incomplete tool call".into(),
                });
            }
            let arguments =
                serde_json::from_str(&tool.arguments).map_err(|error| ProviderError::Protocol {
                    message: format!("streamed tool arguments are invalid JSON: {error}"),
                })?;
            content.push(Content::ToolCall(ToolCall {
                id: tool.id,
                name: tool.name,
                arguments,
            }));
        }
        let mut message = Message::assistant(content, self.stop_reason);
        message.usage = self.usage;
        Ok(ModelResponse {
            message,
            response_id: self.id,
        })
    }
}

pub async fn stream_openai(
    provider: &OpenAiProvider,
    request: ModelRequest,
    sink: &dyn ProviderEventSink,
) -> Result<ModelResponse, ProviderError> {
    let endpoint = format!(
        "{}/chat/completions",
        provider.config.base_url.trim_end_matches('/')
    );
    let mut body = provider.request_body(&request);
    body["stream"] = Value::Bool(true);
    body["stream_options"] = serde_json::json!({"include_usage": true});
    let response = provider
        .authentication
        .apply(
            provider.client.post(endpoint),
            provider.config.api_key_secret().expose_secret(),
        )
        .json(&body)
        .send()
        .await
        .map_err(|error| transport_error(&error))?;
    let status = response.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(ProviderError::Authentication);
    }
    if status.as_u16() == 429 {
        return Err(ProviderError::RateLimited {
            message: "provider rate limit".into(),
        });
    }
    if status.is_server_error() {
        return Err(ProviderError::Unavailable {
            message: format!("HTTP {status}"),
        });
    }
    if !status.is_success() {
        return Err(ProviderError::Protocol {
            message: format!("HTTP {status}"),
        });
    }

    let mut stream = response.bytes_stream();
    let mut pending = Vec::<u8>::new();
    let mut received = 0_usize;
    let mut accumulator = StreamAccumulator::default();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| transport_error(&error))?;
        received = received.saturating_add(chunk.len());
        if received > MAX_STREAM_BYTES {
            return Err(ProviderError::Protocol {
                message: "provider stream exceeds the 8 MiB limit".into(),
            });
        }
        pending.extend_from_slice(&chunk);
        while let Some(position) = pending.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = pending.drain(..=position).collect();
            while line
                .last()
                .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
            {
                line.pop();
            }
            apply_sse_line(&mut accumulator, &line, sink).await?;
        }
    }
    if !pending.is_empty() {
        while pending
            .last()
            .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
        {
            pending.pop();
        }
        apply_sse_line(&mut accumulator, &pending, sink).await?;
    }
    accumulator.finish()
}

async fn apply_sse_line(
    accumulator: &mut StreamAccumulator,
    line: &[u8],
    sink: &dyn ProviderEventSink,
) -> Result<(), ProviderError> {
    let Some(payload) = line.strip_prefix(b"data: ") else {
        return Ok(());
    };
    if payload == b"[DONE]" {
        return Ok(());
    }
    let value: Value =
        serde_json::from_slice(payload).map_err(|error| ProviderError::Protocol {
            message: format!("invalid provider stream event: {error}"),
        })?;
    accumulator.apply(&value, sink).await
}

fn transport_error(error: &reqwest::Error) -> ProviderError {
    if error.is_timeout() || error.is_connect() {
        ProviderError::Unavailable {
            message: error.to_string(),
        }
    } else {
        ProviderError::Protocol {
            message: error.to_string(),
        }
    }
}
