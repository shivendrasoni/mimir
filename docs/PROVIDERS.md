# Providers and authentication

Anthropic OAuth is the default login and Claude Sonnet 5 is the default model.

## Login and credential management

```bash
mimir providers
mimir login
printf '%s\n' "$OPENAI_API_KEY" | mimir login openai --api-key-stdin
mimir auth status
mimir logout openai
mimir login openai-codex
printf '%s\n' "$ANTHROPIC_API_KEY" | mimir login anthropic --api-key-stdin
```

Inside the TUI, use `/login`; credentials are masked while entered.

## Provider behavior

ChatGPT Codex OAuth uses the native Codex Responses transport. Anthropic is native and distinguishes API-key authentication (`x-api-key`) from OAuth bearer authentication; the two credential types are not interchangeable.

Bedrock, Vertex, Google, Mistral, OpenAI-compatible, safe custom providers, and extension-provided transports are selected through the same typed runtime factory.

Providers that are discovery-only or unsupported by the selected runtime fail closed before execution.
