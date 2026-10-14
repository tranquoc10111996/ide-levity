//! Supported AI providers and their wire protocols.
//!
//! Default endpoints and model IDs were taken from each provider's official
//! documentation in October 2026; see `PROVIDERS.md` for sources and details.

use std::fmt;

/// The HTTP protocol family a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WireProtocol {
    /// OpenAI Chat Completions (`POST {base}/chat/completions`), also spoken
    /// by OpenRouter, Groq, DeepSeek, Mistral, xAI, Together, Ollama and
    /// LM Studio.
    OpenAiChat,
    /// Anthropic Messages API (`POST {base}/messages`).
    AnthropicMessages,
    /// Google Gemini `generateContent` / `streamGenerateContent`.
    GeminiGenerateContent,
}

/// A known AI provider (or a generic OpenAI-compatible endpoint).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    /// OpenAI platform.
    OpenAi,
    /// Anthropic Claude platform.
    Anthropic,
    /// Google Gemini API (AI Studio keys).
    Gemini,
    /// OpenRouter aggregator.
    OpenRouter,
    /// Groq.
    Groq,
    /// DeepSeek.
    DeepSeek,
    /// Mistral AI.
    Mistral,
    /// xAI (Grok).
    Xai,
    /// Together AI.
    Together,
    /// Local Ollama server.
    Ollama,
    /// Local LM Studio server.
    LmStudio,
    /// Any other OpenAI-compatible endpoint (vLLM, llama.cpp server, proxies).
    OpenAiCompatible,
}

impl ProviderKind {
    /// Every provider, in auto-detection order for hosted providers followed
    /// by local servers and the generic endpoint.
    pub const ALL: [ProviderKind; 12] = [
        ProviderKind::Anthropic,
        ProviderKind::OpenAi,
        ProviderKind::Gemini,
        ProviderKind::OpenRouter,
        ProviderKind::Groq,
        ProviderKind::DeepSeek,
        ProviderKind::Mistral,
        ProviderKind::Xai,
        ProviderKind::Together,
        ProviderKind::Ollama,
        ProviderKind::LmStudio,
        ProviderKind::OpenAiCompatible,
    ];

    /// Stable lowercase identifier used in config files and env vars.
    pub fn id(self) -> &'static str {
        match self {
            ProviderKind::OpenAi => "openai",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Gemini => "gemini",
            ProviderKind::OpenRouter => "openrouter",
            ProviderKind::Groq => "groq",
            ProviderKind::DeepSeek => "deepseek",
            ProviderKind::Mistral => "mistral",
            ProviderKind::Xai => "xai",
            ProviderKind::Together => "together",
            ProviderKind::Ollama => "ollama",
            ProviderKind::LmStudio => "lmstudio",
            ProviderKind::OpenAiCompatible => "openai-compatible",
        }
    }

    /// Human readable name for status bars.
    pub fn display_name(self) -> &'static str {
        match self {
            ProviderKind::OpenAi => "OpenAI",
            ProviderKind::Anthropic => "Anthropic",
            ProviderKind::Gemini => "Gemini",
            ProviderKind::OpenRouter => "OpenRouter",
            ProviderKind::Groq => "Groq",
            ProviderKind::DeepSeek => "DeepSeek",
            ProviderKind::Mistral => "Mistral",
            ProviderKind::Xai => "xAI",
            ProviderKind::Together => "Together",
            ProviderKind::Ollama => "Ollama",
            ProviderKind::LmStudio => "LM Studio",
            ProviderKind::OpenAiCompatible => "OpenAI-compatible",
        }
    }

    /// Parses a provider identifier (case-insensitive, common aliases
    /// accepted, e.g. `claude`, `google`, `grok`, `lm-studio`, `custom`).
    pub fn parse(value: &str) -> Option<ProviderKind> {
        let normalized: String = value
            .trim()
            .to_ascii_lowercase()
            .chars()
            .filter(|c| !matches!(c, '-' | '_' | ' ' | '.'))
            .collect();
        Some(match normalized.as_str() {
            "openai" | "chatgpt" | "gpt" => ProviderKind::OpenAi,
            "anthropic" | "claude" => ProviderKind::Anthropic,
            "gemini" | "google" | "googleai" | "aistudio" => ProviderKind::Gemini,
            "openrouter" => ProviderKind::OpenRouter,
            "groq" => ProviderKind::Groq,
            "deepseek" => ProviderKind::DeepSeek,
            "mistral" | "mistralai" => ProviderKind::Mistral,
            "xai" | "grok" => ProviderKind::Xai,
            "together" | "togetherai" => ProviderKind::Together,
            "ollama" => ProviderKind::Ollama,
            "lmstudio" => ProviderKind::LmStudio,
            "openaicompatible" | "compatible" | "custom" | "local" | "vllm" | "llamacpp" => {
                ProviderKind::OpenAiCompatible
            }
            _ => return None,
        })
    }

    /// Wire protocol used by this provider.
    pub fn wire(self) -> WireProtocol {
        match self {
            ProviderKind::Anthropic => WireProtocol::AnthropicMessages,
            ProviderKind::Gemini => WireProtocol::GeminiGenerateContent,
            _ => WireProtocol::OpenAiChat,
        }
    }

    /// Default API base URL. Request paths (`/chat/completions`,
    /// `/messages`, `/models/{model}:generateContent`) are appended to it.
    /// Empty for [`ProviderKind::OpenAiCompatible`], which needs an explicit URL.
    pub fn default_base_url(self) -> &'static str {
        match self {
            ProviderKind::OpenAi => "https://api.openai.com/v1",
            ProviderKind::Anthropic => "https://api.anthropic.com/v1",
            ProviderKind::Gemini => "https://generativelanguage.googleapis.com/v1beta",
            ProviderKind::OpenRouter => "https://openrouter.ai/api/v1",
            ProviderKind::Groq => "https://api.groq.com/openai/v1",
            ProviderKind::DeepSeek => "https://api.deepseek.com",
            ProviderKind::Mistral => "https://api.mistral.ai/v1",
            ProviderKind::Xai => "https://api.x.ai/v1",
            ProviderKind::Together => "https://api.together.xyz/v1",
            ProviderKind::Ollama => "http://127.0.0.1:11434/v1",
            ProviderKind::LmStudio => "http://127.0.0.1:1234/v1",
            ProviderKind::OpenAiCompatible => "",
        }
    }

    /// Recommended default model for code editing, or `None` for local and
    /// generic endpoints where the model must be discovered or configured.
    pub fn default_model(self) -> Option<&'static str> {
        Some(match self {
            ProviderKind::OpenAi => "gpt-6.1-sol",
            ProviderKind::Anthropic => "claude-sonnet-5-5",
            ProviderKind::Gemini => "gemini-3.8-flash",
            ProviderKind::OpenRouter => "openrouter/auto",
            ProviderKind::Groq => "openai/gpt-oss-120b",
            ProviderKind::DeepSeek => "deepseek-flash",
            ProviderKind::Mistral => "mistral-medium-latest",
            ProviderKind::Xai => "grok-4.7",
            ProviderKind::Together => "openai/gpt-oss-120b",
            ProviderKind::Ollama | ProviderKind::LmStudio | ProviderKind::OpenAiCompatible => {
                return None
            }
        })
    }

    /// Well-known environment variables holding this provider's API key,
    /// in lookup order.
    pub fn api_key_env_vars(self) -> &'static [&'static str] {
        match self {
            ProviderKind::OpenAi => &["OPENAI_API_KEY"],
            ProviderKind::Anthropic => &["ANTHROPIC_API_KEY"],
            ProviderKind::Gemini => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            ProviderKind::OpenRouter => &["OPENROUTER_API_KEY"],
            ProviderKind::Groq => &["GROQ_API_KEY"],
            ProviderKind::DeepSeek => &["DEEPSEEK_API_KEY"],
            ProviderKind::Mistral => &["MISTRAL_API_KEY"],
            ProviderKind::Xai => &["XAI_API_KEY"],
            ProviderKind::Together => &["TOGETHER_API_KEY"],
            ProviderKind::Ollama | ProviderKind::LmStudio | ProviderKind::OpenAiCompatible => &[],
        }
    }

    /// Whether requests fail without an API key.
    pub fn requires_api_key(self) -> bool {
        !self.is_local() && self != ProviderKind::OpenAiCompatible
    }

    /// Whether this is a local inference server discovered by probing.
    pub fn is_local(self) -> bool {
        matches!(self, ProviderKind::Ollama | ProviderKind::LmStudio)
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trips_ids_and_aliases() {
        for kind in ProviderKind::ALL {
            assert_eq!(ProviderKind::parse(kind.id()), Some(kind));
        }
        assert_eq!(ProviderKind::parse(" Claude "), Some(ProviderKind::Anthropic));
        assert_eq!(ProviderKind::parse("LM-Studio"), Some(ProviderKind::LmStudio));
        assert_eq!(ProviderKind::parse("lm_studio"), Some(ProviderKind::LmStudio));
        assert_eq!(ProviderKind::parse("google"), Some(ProviderKind::Gemini));
        assert_eq!(ProviderKind::parse("grok"), Some(ProviderKind::Xai));
        assert_eq!(ProviderKind::parse("nope"), None);
    }

    #[test]
    fn hosted_providers_have_defaults() {
        for kind in ProviderKind::ALL {
            if kind.requires_api_key() {
                assert!(kind.default_model().is_some(), "{kind}");
                assert!(!kind.api_key_env_vars().is_empty(), "{kind}");
                assert!(kind.default_base_url().starts_with("https://"), "{kind}");
            }
        }
    }
}
