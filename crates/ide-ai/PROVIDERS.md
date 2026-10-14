# Supported AI Providers in IDE Levity

`ide-ai` supports three HTTP wire protocols and twelve provider presets out of the box.
Every cloud and local provider can be configured through environment variables or `~/.config/ide-levity/ai.json`.

## Wire Protocols

1. **OpenAI Chat Completions (`WireProtocol::OpenAiChat`)**
   - Endpoint: `POST {base_url}/chat/completions` and `GET {base_url}/models`.
   - Authentication header: `Authorization: Bearer <API_KEY>` (optional for local servers).
   - Streaming: `stream: true` with Server-Sent Events (`text/event-stream`) ending in `data: [DONE]`.

2. **Anthropic Messages (`WireProtocol::AnthropicMessages`)**
   - Endpoint: `POST {base_url}/messages` and `GET {base_url}/models?limit=1000`.
   - Authentication headers: `x-api-key: <API_KEY>` and `anthropic-version: 2023-06-01`.
   - Streaming: `stream: true` with typed SSE events (`message_start`, `content_block_delta`, `message_delta`, `message_stop`, `error`).

3. **Google Gemini GenerateContent (`WireProtocol::GeminiGenerateContent`)**
   - Endpoint: `POST {base_url}/models/{model}:generateContent` and `POST {base_url}/models/{model}:streamGenerateContent?alt=sse`.
   - Authentication header: `x-goog-api-key: <API_KEY>`.
   - Streaming: SSE stream of `GenerateContentResponse` JSON objects where `candidates[0].content.parts[].text` carries visible output and `thought: true` parts are filtered out.

## Provider Matrix

| Provider | ID | Wire Protocol | Default Base URL | Default Model | Environment Variable(s) |
| :--- | :--- | :--- | :--- | :--- | :--- |
| Anthropic | `anthropic` | Anthropic Messages | `https://api.anthropic.com/v1` | `claude-sonnet-5-5` | `ANTHROPIC_API_KEY` |
| OpenAI | `openai` | OpenAI Chat | `https://api.openai.com/v1` | `gpt-6.1-sol` | `OPENAI_API_KEY` |
| Google Gemini | `gemini` | Gemini GenerateContent | `https://generativelanguage.googleapis.com/v1beta` | `gemini-3.8-flash` | `GEMINI_API_KEY`, `GOOGLE_API_KEY` |
| OpenRouter | `openrouter` | OpenAI Chat | `https://openrouter.ai/api/v1` | `openrouter/auto` | `OPENROUTER_API_KEY` |
| Groq | `groq` | OpenAI Chat | `https://api.groq.com/openai/v1` | `openai/gpt-oss-120b` | `GROQ_API_KEY` |
| DeepSeek | `deepseek` | OpenAI Chat | `https://api.deepseek.com` | `deepseek-flash` | `DEEPSEEK_API_KEY` |
| Mistral | `mistral` | OpenAI Chat | `https://api.mistral.ai/v1` | `mistral-medium-latest` | `MISTRAL_API_KEY` |
| xAI | `xai` | OpenAI Chat | `https://api.x.ai/v1` | `grok-4.7` | `XAI_API_KEY` |
| Together AI | `together` | OpenAI Chat | `https://api.together.xyz/v1` | `openai/gpt-oss-120b` | `TOGETHER_API_KEY` |
| Ollama (Local) | `ollama` | OpenAI Chat | `http://127.0.0.1:11434/v1` | Auto-selected from `/v1/models` | `OLLAMA_HOST` (optional) |
| LM Studio (Local) | `lmstudio` | OpenAI Chat | `http://127.0.0.1:1234/v1` | Auto-selected from `/v1/models` | None required |
| OpenAI-Compatible | `openai-compatible` | OpenAI Chat | User-configured | User-configured or `/v1/models` | `LEVITY_AI_BASE_URL`, `LEVITY_AI_API_KEY` |

## Resolution Precedence

`resolve_config` evaluates sources in the following order:

1. Explicit `LEVITY_AI_PROVIDER` environment variable.
2. Configuration file (`$LEVITY_AI_CONFIG`, `$XDG_CONFIG_HOME/ide-levity/ai.json`, or `~/.config/ide-levity/ai.json`).
3. Explicit `LEVITY_AI_BASE_URL` or key-prefix detection from `LEVITY_AI_API_KEY`.
4. Provider-specific environment variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `GOOGLE_API_KEY`, `OPENROUTER_API_KEY`, `GROQ_API_KEY`, `DEEPSEEK_API_KEY`, `MISTRAL_API_KEY`, `XAI_API_KEY`, `TOGETHER_API_KEY`).
5. Fast local probe (`300ms` timeout) against Ollama (`http://127.0.0.1:11434/v1/models`) and LM Studio (`http://127.0.0.1:1234/v1/models`).

Once a provider is selected, `LEVITY_AI_MODEL`, `LEVITY_AI_BASE_URL`, and `LEVITY_AI_API_KEY` override individual fields.

## Local Ollama Quickstart

To run the AI Agent locally without cloud API keys:

```bash
ollama serve &
ollama pull qwen2.5-coder:3b
```

IDE Levity automatically detects the running Ollama daemon and prefers code-specialized models when multiple models are installed.
