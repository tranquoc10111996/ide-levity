//! AI configuration and its resolution from env vars, a config file and
//! local inference servers.
//!
//! Resolution precedence (first match selects the provider):
//! 1. `LEVITY_AI_PROVIDER` env var.
//! 2. The config file (`LEVITY_AI_CONFIG`, else `$XDG_CONFIG_HOME/ide-levity/ai.json`,
//!    else `~/.config/ide-levity/ai.json`, plus the platform config dir on
//!    macOS/Windows) when it names a provider or a base URL.
//! 3. `LEVITY_AI_BASE_URL` (generic OpenAI-compatible endpoint) or
//!    `LEVITY_AI_API_KEY` alone (provider guessed from the key prefix).
//! 4. Well-known provider key env vars (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, ...).
//! 5. A local Ollama or LM Studio server answering on its default port.
//!
//! After the provider is selected, `LEVITY_AI_MODEL`, `LEVITY_AI_BASE_URL`
//! and `LEVITY_AI_API_KEY` always override the corresponding field.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::provider::{ProviderKind, WireProtocol};

/// Default upper bound for generated tokens per request.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 16_384;
/// Default request timeout (idle time while streaming, total time otherwise).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
/// Default timeout for probing local inference servers.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// Env var selecting the provider explicitly.
pub const ENV_PROVIDER: &str = "LEVITY_AI_PROVIDER";
/// Env var overriding the model.
pub const ENV_MODEL: &str = "LEVITY_AI_MODEL";
/// Env var overriding the API base URL.
pub const ENV_BASE_URL: &str = "LEVITY_AI_BASE_URL";
/// Env var overriding the API key.
pub const ENV_API_KEY: &str = "LEVITY_AI_API_KEY";
/// Env var pointing at an explicit config file.
pub const ENV_CONFIG: &str = "LEVITY_AI_CONFIG";

/// Fully resolved settings for one provider endpoint.
#[derive(Clone, PartialEq)]
pub struct AiConfig {
    /// Which provider (and therefore which wire protocol) to use.
    pub provider: ProviderKind,
    /// API base URL without a trailing slash.
    pub base_url: String,
    /// API key; `None` for local servers.
    pub api_key: Option<String>,
    /// Model identifier sent to the provider.
    pub model: String,
    /// Upper bound for generated tokens (including reasoning tokens where
    /// the provider counts them).
    pub max_output_tokens: u32,
    /// Sampling temperature. `None` (the default) omits the field, which is
    /// required for current Anthropic and OpenAI reasoning models.
    pub temperature: Option<f32>,
    /// Request timeout: maximum idle time between received bytes, and the
    /// total budget for non-streaming requests.
    pub timeout: Duration,
}

impl AiConfig {
    /// Creates a config with the provider's default base URL and the given model.
    pub fn new(provider: ProviderKind, model: impl Into<String>) -> Self {
        AiConfig {
            provider,
            base_url: provider.default_base_url().to_string(),
            api_key: None,
            model: model.into(),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            temperature: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Sets the API key.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Sets the base URL (a trailing slash is removed).
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = normalize_base_url(&url.into());
        self
    }

    /// Wire protocol spoken by the configured provider.
    pub fn wire(&self) -> WireProtocol {
        self.provider.wire()
    }

    /// Short description such as `Anthropic claude-sonnet-5-5`.
    pub fn label(&self) -> String {
        format!("{} {}", self.provider.display_name(), self.model)
    }
}

impl fmt::Debug for AiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AiConfig")
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("model", &self.model)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("temperature", &self.temperature)
            .field("timeout", &self.timeout)
            .finish()
    }
}

fn normalize_base_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// An injectable snapshot of environment variables.
///
/// Empty or whitespace-only values are treated as unset.
#[derive(Clone, Default)]
pub struct EnvSnapshot {
    vars: HashMap<String, String>,
}

impl EnvSnapshot {
    /// Captures the current process environment.
    pub fn from_process() -> Self {
        EnvSnapshot {
            vars: std::env::vars().collect(),
        }
    }

    /// Builds a snapshot from explicit pairs (for tests and embedding).
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        EnvSnapshot {
            vars: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    /// Returns the trimmed value of `key`, or `None` if unset or blank.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars
            .get(key)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
    }
}

impl fmt::Debug for EnvSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut keys: Vec<&String> = self.vars.keys().collect();
        keys.sort();
        f.debug_struct("EnvSnapshot").field("keys", &keys).finish()
    }
}

/// On-disk config file format (`ai.json`). Every field is optional.
///
/// ```json
/// { "provider": "anthropic", "model": "claude-sonnet-5-5",
///   "api_key_env": "MY_CLAUDE_KEY", "max_output_tokens": 16384 }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AiFileConfig {
    /// Provider id, see [`ProviderKind::parse`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Model id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// API base URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Literal API key (prefer `api_key_env`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Name of an env var holding the API key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Max generated tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Request timeout in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl AiFileConfig {
    /// Parses a config file from JSON text.
    pub fn from_json(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|e| e.to_string())
    }

    /// Writes the config as pretty JSON, creating parent directories.
    /// On Unix the file is created with mode 0600 because it may hold a key.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        let mut file = options.open(path)?;
        file.write_all(json.as_bytes())?;
        file.write_all(b"\n")
    }
}

/// Candidate config file locations for this platform, in priority order.
pub fn default_config_paths(env: &EnvSnapshot) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let rel = Path::new("ide-levity").join("ai.json");
    if let Some(xdg) = env.get("XDG_CONFIG_HOME") {
        paths.push(PathBuf::from(xdg).join(&rel));
    }
    if cfg!(windows) {
        if let Some(appdata) = env.get("APPDATA") {
            paths.push(PathBuf::from(appdata).join(&rel));
        }
    }
    if let Some(home) = env.get("HOME").or_else(|| env.get("USERPROFILE")) {
        let home = PathBuf::from(home);
        paths.push(home.join(".config").join(&rel));
        if cfg!(target_os = "macos") {
            paths.push(
                home.join("Library")
                    .join("Application Support")
                    .join(&rel),
            );
        }
    }
    paths.dedup();
    paths
}

/// Where the selected provider came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// Selected by a `LEVITY_AI_*` env var (the variable name).
    EnvOverride(String),
    /// Selected by the config file at this path.
    ConfigFile(PathBuf),
    /// Auto-detected from a well-known API key env var (the variable name).
    EnvApiKey(String),
    /// Discovered by probing a local inference server.
    LocalServer,
}

impl ConfigSource {
    /// Short human readable description, e.g. `env ANTHROPIC_API_KEY`.
    pub fn describe(&self) -> String {
        match self {
            ConfigSource::EnvOverride(var) | ConfigSource::EnvApiKey(var) => format!("env {var}"),
            ConfigSource::ConfigFile(path) => format!("config {}", path.display()),
            ConfigSource::LocalServer => "local server".to_string(),
        }
    }
}

/// Outcome of configuration resolution.
#[derive(Debug, Clone, PartialEq)]
pub enum AiSetup {
    /// A usable configuration was found.
    Ready {
        /// The resolved configuration.
        config: AiConfig,
        /// What selected the provider.
        source: ConfigSource,
        /// `LEVITY_AI_*` variables applied on top of the source.
        overrides: Vec<String>,
        /// Non-fatal problems (bad config file fields, unknown values).
        warnings: Vec<String>,
    },
    /// Nothing usable was found.
    NotConfigured {
        /// Why each candidate was rejected, for display.
        warnings: Vec<String>,
    },
}

impl AiSetup {
    /// The resolved config, if any.
    pub fn config(&self) -> Option<&AiConfig> {
        match self {
            AiSetup::Ready { config, .. } => Some(config),
            AiSetup::NotConfigured { .. } => None,
        }
    }

    /// Warnings collected during resolution.
    pub fn warnings(&self) -> &[String] {
        match self {
            AiSetup::Ready { warnings, .. } | AiSetup::NotConfigured { warnings } => warnings,
        }
    }

    /// One-line status, e.g. `AI: Anthropic claude-sonnet-5-5 (env ANTHROPIC_API_KEY)`
    /// or `AI: not configured`.
    pub fn status_line(&self) -> String {
        match self {
            AiSetup::Ready {
                config,
                source,
                overrides,
                ..
            } => {
                let mut detail = match source {
                    ConfigSource::LocalServer => format!("local server {}", config.base_url),
                    other => other.describe(),
                };
                if !overrides.is_empty() {
                    detail.push_str("; override ");
                    detail.push_str(&overrides.join(", "));
                }
                format!("AI: {} ({detail})", config.label())
            }
            AiSetup::NotConfigured { .. } => "AI: not configured".to_string(),
        }
    }
}

/// Multi-line instructions shown when no provider is configured.
pub fn setup_instructions() -> &'static str {
    "No AI provider is configured. Any one of these works:\n\
     1. Export a provider key, e.g. ANTHROPIC_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY, \
     OPENROUTER_API_KEY, GROQ_API_KEY, DEEPSEEK_API_KEY, MISTRAL_API_KEY, XAI_API_KEY or TOGETHER_API_KEY.\n\
     2. Run a local model: install Ollama (https://ollama.com), then `ollama pull qwen2.5-coder:3b`; \
     or start the LM Studio server on port 1234.\n\
     3. Set LEVITY_AI_PROVIDER, LEVITY_AI_MODEL, LEVITY_AI_BASE_URL and LEVITY_AI_API_KEY explicitly.\n\
     4. Create ~/.config/ide-levity/ai.json, e.g. {\"provider\": \"anthropic\", \"model\": \"claude-sonnet-5-5\", \"api_key_env\": \"ANTHROPIC_API_KEY\"}.\n\
     Then restart the IDE or reload the AI settings."
}

/// Inputs for [`resolve_config`].
#[derive(Debug, Clone)]
pub struct ResolveOptions {
    /// Environment to read.
    pub env: EnvSnapshot,
    /// Explicit config file path; `None` uses `LEVITY_AI_CONFIG` or the
    /// platform defaults from [`default_config_paths`].
    pub config_path: Option<PathBuf>,
    /// Whether to probe local servers when nothing else is configured.
    pub probe_local: bool,
    /// Timeout for each local probe.
    pub probe_timeout: Duration,
    /// Ollama OpenAI-compatible base URL (honours `OLLAMA_HOST`).
    pub ollama_base_url: String,
    /// LM Studio base URL.
    pub lmstudio_base_url: String,
}

impl ResolveOptions {
    /// Options reading the given environment, with local probing enabled.
    pub fn with_env(env: EnvSnapshot) -> Self {
        let ollama_base_url = env
            .get("OLLAMA_HOST")
            .map(ollama_host_to_base_url)
            .unwrap_or_else(|| ProviderKind::Ollama.default_base_url().to_string());
        ResolveOptions {
            env,
            config_path: None,
            probe_local: true,
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
            ollama_base_url,
            lmstudio_base_url: ProviderKind::LmStudio.default_base_url().to_string(),
        }
    }

    /// Options reading the real process environment.
    pub fn from_process_env() -> Self {
        Self::with_env(EnvSnapshot::from_process())
    }
}

/// Converts an `OLLAMA_HOST` value (`host`, `host:port`, `http://host:port`)
/// into an OpenAI-compatible base URL. A `0.0.0.0` bind address maps to loopback.
pub fn ollama_host_to_base_url(host: &str) -> String {
    let host = host.trim().trim_end_matches('/');
    let (scheme, rest) = match host.split_once("://") {
        Some((s, r)) => (s.to_string(), r.to_string()),
        None => ("http".to_string(), host.to_string()),
    };
    let rest = rest.replace("0.0.0.0", "127.0.0.1");
    let has_port = rest
        .rsplit_once(':')
        .is_some_and(|(_, p)| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    let rest = if has_port {
        rest
    } else {
        format!("{rest}:11434")
    };
    format!("{scheme}://{rest}/v1")
}

/// Guesses the provider from an API key prefix.
pub fn guess_provider_from_key(key: &str) -> Option<ProviderKind> {
    let key = key.trim();
    Some(if key.starts_with("sk-ant-") {
        ProviderKind::Anthropic
    } else if key.starts_with("sk-or-") {
        ProviderKind::OpenRouter
    } else if key.starts_with("AIza") {
        ProviderKind::Gemini
    } else if key.starts_with("gsk_") {
        ProviderKind::Groq
    } else if key.starts_with("xai-") {
        ProviderKind::Xai
    } else if key.starts_with("sk-") {
        ProviderKind::OpenAi
    } else {
        return None;
    })
}

#[derive(Debug, Clone)]
struct Partial {
    provider: ProviderKind,
    base_url: Option<String>,
    api_key: Option<String>,
    api_key_env: Option<String>,
    model: Option<String>,
    max_output_tokens: Option<u32>,
    temperature: Option<f32>,
    timeout: Option<Duration>,
}

impl Partial {
    fn new(provider: ProviderKind) -> Self {
        Partial {
            provider,
            base_url: None,
            api_key: None,
            api_key_env: None,
            model: None,
            max_output_tokens: None,
            temperature: None,
            timeout: None,
        }
    }

    fn merge_generic(&mut self, file: &AiFileConfig) {
        self.max_output_tokens = file.max_output_tokens.or(self.max_output_tokens);
        self.temperature = file.temperature.or(self.temperature);
        self.timeout = file
            .timeout_secs
            .map(Duration::from_secs)
            .or(self.timeout);
    }

    fn merge_provider_specific(&mut self, file: &AiFileConfig) {
        self.merge_generic(file);
        self.model = file.model.clone().or(self.model.take());
        self.base_url = file.base_url.clone().or(self.base_url.take());
        self.api_key = file.api_key.clone().or(self.api_key.take());
        self.api_key_env = file.api_key_env.clone().or(self.api_key_env.take());
    }
}

async fn load_file_config(
    opts: &ResolveOptions,
    warnings: &mut Vec<String>,
) -> Option<(PathBuf, AiFileConfig)> {
    let explicit = opts
        .config_path
        .clone()
        .or_else(|| opts.env.get(ENV_CONFIG).map(PathBuf::from));
    let candidates = match &explicit {
        Some(path) => vec![path.clone()],
        None => default_config_paths(&opts.env),
    };
    for path in candidates {
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => match AiFileConfig::from_json(&text) {
                Ok(cfg) => return Some((path, cfg)),
                Err(e) => {
                    warnings.push(format!("ignoring invalid config file {}: {e}", path.display()));
                    return None;
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if explicit.is_some() {
                    warnings.push(format!("config file {} does not exist", path.display()));
                }
            }
            Err(e) => {
                warnings.push(format!("cannot read config file {}: {e}", path.display()));
                return None;
            }
        }
    }
    None
}

/// Resolves the AI configuration following the documented precedence.
///
/// Never fails: problems are reported through [`AiSetup::NotConfigured`] and
/// warnings. Local probing uses short timeouts so this returns quickly.
pub async fn resolve_config(opts: &ResolveOptions) -> AiSetup {
    let env = &opts.env;
    let mut warnings = Vec::new();
    let file = load_file_config(opts, &mut warnings).await;

    let env_provider = match env.get(ENV_PROVIDER) {
        Some(value) => match ProviderKind::parse(value) {
            Some(kind) => Some(kind),
            None => {
                warnings.push(format!("ignoring unknown {ENV_PROVIDER}={value}"));
                None
            }
        },
        None => None,
    };
    let file_provider = file.as_ref().and_then(|(path, cfg)| {
        let value = cfg.provider.as_deref()?;
        let parsed = ProviderKind::parse(value);
        if parsed.is_none() {
            warnings.push(format!(
                "ignoring unknown provider \"{value}\" in {}",
                path.display()
            ));
        }
        parsed
    });

    let mut source_var: Option<&'static str> = None;
    let mut selected: Option<(Partial, ConfigSource)> = None;

    if let Some(kind) = env_provider {
        let mut partial = Partial::new(kind);
        if let Some((_, cfg)) = &file {
            if file_provider == Some(kind) {
                partial.merge_provider_specific(cfg);
            } else {
                partial.merge_generic(cfg);
            }
        }
        source_var = Some(ENV_PROVIDER);
        selected = Some((partial, ConfigSource::EnvOverride(ENV_PROVIDER.to_string())));
    }

    if selected.is_none() {
        if let Some((path, cfg)) = &file {
            let kind = file_provider.or_else(|| {
                cfg.base_url
                    .as_ref()
                    .map(|_| ProviderKind::OpenAiCompatible)
            });
            if let Some(kind) = kind {
                let mut partial = Partial::new(kind);
                partial.merge_provider_specific(cfg);
                selected = Some((partial, ConfigSource::ConfigFile(path.clone())));
            }
        }
    }

    if selected.is_none() {
        if env.get(ENV_BASE_URL).is_some() {
            source_var = Some(ENV_BASE_URL);
            selected = Some((
                Partial::new(ProviderKind::OpenAiCompatible),
                ConfigSource::EnvOverride(ENV_BASE_URL.to_string()),
            ));
        } else if let Some(key) = env.get(ENV_API_KEY) {
            match guess_provider_from_key(key) {
                Some(kind) => {
                    source_var = Some(ENV_API_KEY);
                    selected = Some((
                        Partial::new(kind),
                        ConfigSource::EnvOverride(ENV_API_KEY.to_string()),
                    ));
                }
                None => warnings.push(format!(
                    "{ENV_API_KEY} is set but its provider cannot be guessed; set {ENV_PROVIDER}"
                )),
            }
        }
    }

    if selected.is_none() {
        'detect: for kind in ProviderKind::ALL {
            for var in kind.api_key_env_vars() {
                if let Some(key) = env.get(var) {
                    let mut partial = Partial::new(kind);
                    partial.api_key = Some(key.to_string());
                    selected = Some((partial, ConfigSource::EnvApiKey((*var).to_string())));
                    break 'detect;
                }
            }
        }
    }

    if selected.is_none() && opts.probe_local {
        let ollama = crate::client::probe_models(&opts.ollama_base_url, opts.probe_timeout);
        let lmstudio = crate::client::probe_models(&opts.lmstudio_base_url, opts.probe_timeout);
        let (ollama, lmstudio) = tokio::join!(ollama, lmstudio);
        for (kind, base, models) in [
            (ProviderKind::Ollama, &opts.ollama_base_url, ollama),
            (ProviderKind::LmStudio, &opts.lmstudio_base_url, lmstudio),
        ] {
            let Some(models) = models else { continue };
            match pick_local_model(&models) {
                Some(model) => {
                    let mut partial = Partial::new(kind);
                    partial.base_url = Some(base.clone());
                    partial.model = Some(model);
                    selected = Some((partial, ConfigSource::LocalServer));
                    break;
                }
                None => warnings.push(format!(
                    "{} is running at {base} but has no chat model installed",
                    kind.display_name()
                )),
            }
        }
    }

    let Some((mut partial, source)) = selected else {
        if let Some((_, cfg)) = &file {
            if cfg.provider.is_none() && cfg.base_url.is_none() {
                warnings.push("the config file names no provider".to_string());
            }
        }
        return AiSetup::NotConfigured { warnings };
    };

    // Apply generic file settings (token limit etc.) when the file did not
    // select the provider itself.
    if let (Some((_, cfg)), ConfigSource::EnvApiKey(_) | ConfigSource::LocalServer) =
        (&file, &source)
    {
        partial.merge_generic(cfg);
    }

    let mut overrides = Vec::new();
    for (var, slot) in [
        (ENV_MODEL, &mut partial.model),
        (ENV_BASE_URL, &mut partial.base_url),
        (ENV_API_KEY, &mut partial.api_key),
    ] {
        if let Some(value) = env.get(var) {
            *slot = Some(value.to_string());
            if source_var != Some(var) {
                overrides.push(var.to_string());
            }
        }
    }

    finish(partial, source, overrides, warnings, opts).await
}

async fn finish(
    mut partial: Partial,
    source: ConfigSource,
    overrides: Vec<String>,
    mut warnings: Vec<String>,
    opts: &ResolveOptions,
) -> AiSetup {
    let kind = partial.provider;
    let env = &opts.env;
    if partial.api_key.is_none() {
        if let Some(var) = &partial.api_key_env {
            match env.get(var) {
                Some(key) => partial.api_key = Some(key.to_string()),
                None => warnings.push(format!("api_key_env {var} is not set")),
            }
        }
    }
    if partial.api_key.is_none() {
        partial.api_key = kind
            .api_key_env_vars()
            .iter()
            .find_map(|var| env.get(var))
            .map(str::to_string);
    }
    let base_url = normalize_base_url(
        partial
            .base_url
            .as_deref()
            .unwrap_or_else(|| match kind {
                ProviderKind::Ollama => &opts.ollama_base_url,
                ProviderKind::LmStudio => &opts.lmstudio_base_url,
                _ => kind.default_base_url(),
            }),
    );
    if base_url.is_empty() {
        warnings.push(format!(
            "{} needs a base URL; set {ENV_BASE_URL} or base_url in the config file",
            kind.display_name()
        ));
        return AiSetup::NotConfigured { warnings };
    }
    if kind.requires_api_key() && partial.api_key.is_none() {
        let vars: Vec<&str> = kind.api_key_env_vars().to_vec();
        warnings.push(format!(
            "{} was selected ({}) but no API key was found; set {} or {ENV_API_KEY}",
            kind.display_name(),
            source.describe(),
            vars.join(" / ")
        ));
        return AiSetup::NotConfigured { warnings };
    }
    let model = match partial.model.clone().or(kind.default_model().map(str::to_string)) {
        Some(model) => model,
        None => {
            // Local or generic endpoint without an explicit model: ask it.
            let timeout = opts.probe_timeout.max(Duration::from_secs(2));
            match crate::client::probe_models(&base_url, timeout).await {
                Some(models) => match pick_local_model(&models) {
                    Some(model) => model,
                    None => {
                        warnings.push(format!("{base_url} lists no chat model; set {ENV_MODEL}"));
                        return AiSetup::NotConfigured { warnings };
                    }
                },
                None => {
                    warnings.push(format!(
                        "{} at {base_url} is not reachable and no model is configured; set {ENV_MODEL}",
                        kind.display_name()
                    ));
                    return AiSetup::NotConfigured { warnings };
                }
            }
        }
    };
    let config = AiConfig {
        provider: kind,
        base_url,
        api_key: partial.api_key,
        model,
        max_output_tokens: partial
            .max_output_tokens
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        temperature: partial.temperature,
        timeout: partial.timeout.unwrap_or(DEFAULT_TIMEOUT),
    };
    AiSetup::Ready {
        config,
        source,
        overrides,
        warnings,
    }
}

/// Picks the best chat model from a local server's model list: skips
/// embedding/speech/reranker models and prefers code models, otherwise keeps
/// the server's order (Ollama lists the most recently pulled model first).
pub fn pick_local_model(models: &[String]) -> Option<String> {
    const NON_CHAT: [&str; 6] = ["embed", "whisper", "tts", "rerank", "clip", "moderation"];
    let chat: Vec<&String> = models
        .iter()
        .filter(|m| {
            let lower = m.to_ascii_lowercase();
            !NON_CHAT.iter().any(|bad| lower.contains(bad))
        })
        .collect();
    chat.iter()
        .find(|m| m.to_ascii_lowercase().contains("code"))
        .or_else(|| chat.first())
        .map(|m| (*m).clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(pairs: &[(&str, &str)]) -> ResolveOptions {
        let mut o = ResolveOptions::with_env(EnvSnapshot::from_pairs(pairs.iter().copied()));
        o.probe_local = false;
        o
    }

    fn ready(setup: &AiSetup) -> (&AiConfig, &ConfigSource, &Vec<String>) {
        match setup {
            AiSetup::Ready {
                config,
                source,
                overrides,
                ..
            } => (config, source, overrides),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    fn write_config(json: &str) -> (tempdir::TempDir, PathBuf) {
        let dir = tempdir::TempDir::new();
        let path = dir.path().join("ai.json");
        std::fs::write(&path, json).unwrap();
        (dir, path)
    }

    /// Minimal self-cleaning temp dir (avoids an extra dependency).
    mod tempdir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU64, Ordering};

        pub struct TempDir(PathBuf);
        impl TempDir {
            pub fn new() -> Self {
                static N: AtomicU64 = AtomicU64::new(0);
                let path = std::env::temp_dir().join(format!(
                    "ide-ai-test-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::SeqCst)
                ));
                std::fs::create_dir_all(&path).unwrap();
                TempDir(path)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[tokio::test]
    async fn nothing_configured() {
        let setup = resolve_config(&opts(&[])).await;
        assert_eq!(setup.status_line(), "AI: not configured");
        assert!(setup.config().is_none());
    }

    #[tokio::test]
    async fn autodetect_follows_documented_order() {
        let setup = resolve_config(&opts(&[
            ("OPENAI_API_KEY", "sk-openai"),
            ("ANTHROPIC_API_KEY", "sk-ant-x"),
            ("GROQ_API_KEY", "gsk_x"),
        ]))
        .await;
        let (cfg, source, _) = ready(&setup);
        assert_eq!(cfg.provider, ProviderKind::Anthropic);
        assert_eq!(cfg.api_key.as_deref(), Some("sk-ant-x"));
        assert_eq!(cfg.model, "claude-sonnet-5-5");
        assert_eq!(*source, ConfigSource::EnvApiKey("ANTHROPIC_API_KEY".into()));
        assert_eq!(
            setup.status_line(),
            "AI: Anthropic claude-sonnet-5-5 (env ANTHROPIC_API_KEY)"
        );
    }

    #[tokio::test]
    async fn google_api_key_is_a_gemini_fallback_and_blank_values_are_ignored() {
        let setup = resolve_config(&opts(&[
            ("ANTHROPIC_API_KEY", "   "),
            ("GOOGLE_API_KEY", "AIzaXYZ"),
        ]))
        .await;
        let (cfg, source, _) = ready(&setup);
        assert_eq!(cfg.provider, ProviderKind::Gemini);
        assert_eq!(*source, ConfigSource::EnvApiKey("GOOGLE_API_KEY".into()));
        assert_eq!(cfg.base_url, "https://generativelanguage.googleapis.com/v1beta");
    }

    #[tokio::test]
    async fn explicit_provider_beats_file_and_autodetect() {
        let (_dir, path) = write_config(r#"{"provider":"openai","model":"gpt-6-luna"}"#);
        let mut o = opts(&[
            ("LEVITY_AI_PROVIDER", "groq"),
            ("GROQ_API_KEY", "gsk_1"),
            ("ANTHROPIC_API_KEY", "sk-ant-1"),
        ]);
        o.config_path = Some(path);
        let setup = resolve_config(&o).await;
        let (cfg, source, overrides) = ready(&setup);
        assert_eq!(cfg.provider, ProviderKind::Groq);
        assert_eq!(cfg.model, "openai/gpt-oss-120b", "file model is for another provider");
        assert_eq!(cfg.api_key.as_deref(), Some("gsk_1"));
        assert_eq!(*source, ConfigSource::EnvOverride("LEVITY_AI_PROVIDER".into()));
        assert!(overrides.is_empty());
    }

    #[tokio::test]
    async fn file_beats_autodetect_and_env_overrides_apply_on_top() {
        let (_dir, path) = write_config(
            r#"{"provider":"anthropic","model":"claude-haiku-5-5","api_key_env":"MY_KEY",
                "max_output_tokens":4096,"temperature":0.5,"timeout_secs":42}"#,
        );
        let mut o = opts(&[
            ("OPENAI_API_KEY", "sk-openai"),
            ("MY_KEY", "sk-ant-mine"),
            ("LEVITY_AI_MODEL", "claude-opus-5-5"),
        ]);
        o.config_path = Some(path.clone());
        let setup = resolve_config(&o).await;
        let (cfg, source, overrides) = ready(&setup);
        assert_eq!(cfg.provider, ProviderKind::Anthropic);
        assert_eq!(cfg.model, "claude-opus-5-5");
        assert_eq!(cfg.api_key.as_deref(), Some("sk-ant-mine"));
        assert_eq!(cfg.max_output_tokens, 4096);
        assert_eq!(cfg.temperature, Some(0.5));
        assert_eq!(cfg.timeout, Duration::from_secs(42));
        assert_eq!(*source, ConfigSource::ConfigFile(path.clone()));
        assert_eq!(overrides, &vec!["LEVITY_AI_MODEL".to_string()]);
        assert!(setup.status_line().contains("override LEVITY_AI_MODEL"));
    }

    #[tokio::test]
    async fn levity_config_env_points_at_file() {
        let (_dir, path) = write_config(r#"{"provider":"deepseek","api_key":"ds-key"}"#);
        let mut o = opts(&[("LEVITY_AI_CONFIG", path.to_str().unwrap())]);
        o.config_path = None;
        let setup = resolve_config(&o).await;
        let (cfg, _, _) = ready(&setup);
        assert_eq!(cfg.provider, ProviderKind::DeepSeek);
        assert_eq!(cfg.model, "deepseek-flash");
        assert_eq!(cfg.base_url, "https://api.deepseek.com");
    }

    #[tokio::test]
    async fn xdg_config_home_is_searched() {
        let dir = tempdir::TempDir::new();
        let path = dir.path().join("ide-levity").join("ai.json");
        AiFileConfig {
            provider: Some("mistral".into()),
            api_key: Some("m-key".into()),
            ..Default::default()
        }
        .write_to(&path)
        .unwrap();
        let mut o = opts(&[("XDG_CONFIG_HOME", dir.path().to_str().unwrap())]);
        o.config_path = None;
        let setup = resolve_config(&o).await;
        let (cfg, source, _) = ready(&setup);
        assert_eq!(cfg.provider, ProviderKind::Mistral);
        assert_eq!(*source, ConfigSource::ConfigFile(path));
    }

    #[tokio::test]
    async fn invalid_file_is_reported_and_skipped() {
        let (_dir, path) = write_config(r#"{"provider":"openai","modle":"typo"}"#);
        let mut o = opts(&[("XAI_API_KEY", "xai-1")]);
        o.config_path = Some(path);
        let setup = resolve_config(&o).await;
        let (cfg, _, _) = ready(&setup);
        assert_eq!(cfg.provider, ProviderKind::Xai);
        assert!(setup.warnings()[0].contains("invalid config file"));
        assert!(setup.warnings()[0].contains("modle"));
    }

    #[tokio::test]
    async fn selected_provider_without_key_is_not_configured() {
        let setup = resolve_config(&opts(&[("LEVITY_AI_PROVIDER", "anthropic")])).await;
        assert!(setup.config().is_none());
        assert!(setup.warnings()[0].contains("ANTHROPIC_API_KEY"));
    }

    #[tokio::test]
    async fn base_url_alone_means_generic_endpoint() {
        let setup = resolve_config(&opts(&[
            ("LEVITY_AI_BASE_URL", "http://10.0.0.5:8000/v1/"),
            ("LEVITY_AI_MODEL", "my-model"),
            ("OPENAI_API_KEY", "sk-ignored"),
        ]))
        .await;
        let (cfg, source, overrides) = ready(&setup);
        assert_eq!(cfg.provider, ProviderKind::OpenAiCompatible);
        assert_eq!(cfg.base_url, "http://10.0.0.5:8000/v1");
        assert_eq!(cfg.model, "my-model");
        assert_eq!(cfg.api_key, None);
        assert_eq!(*source, ConfigSource::EnvOverride("LEVITY_AI_BASE_URL".into()));
        assert_eq!(overrides, &vec!["LEVITY_AI_MODEL".to_string()]);
    }

    #[tokio::test]
    async fn api_key_alone_guesses_provider() {
        let setup = resolve_config(&opts(&[("LEVITY_AI_API_KEY", "sk-or-v1-abc")])).await;
        let (cfg, _, _) = ready(&setup);
        assert_eq!(cfg.provider, ProviderKind::OpenRouter);
        assert_eq!(cfg.model, "openrouter/auto");
    }

    #[tokio::test]
    async fn unknown_provider_value_warns_and_falls_through() {
        let setup = resolve_config(&opts(&[
            ("LEVITY_AI_PROVIDER", "skynet"),
            ("TOGETHER_API_KEY", "t"),
        ]))
        .await;
        let (cfg, _, _) = ready(&setup);
        assert_eq!(cfg.provider, ProviderKind::Together);
        assert!(setup.warnings()[0].contains("skynet"));
    }

    #[test]
    fn debug_output_redacts_secrets() {
        let cfg = AiConfig::new(ProviderKind::OpenAi, "m").with_api_key("sk-secret-123");
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("sk-secret-123"));
        assert!(dbg.contains("<redacted>"));
        let env = EnvSnapshot::from_pairs([("OPENAI_API_KEY", "sk-secret-123")]);
        assert!(!format!("{env:?}").contains("sk-secret-123"));
    }

    #[test]
    fn ollama_host_parsing() {
        assert_eq!(ollama_host_to_base_url("0.0.0.0"), "http://127.0.0.1:11434/v1");
        assert_eq!(ollama_host_to_base_url("myhost:9999"), "http://myhost:9999/v1");
        assert_eq!(
            ollama_host_to_base_url("https://gpu.example.com:443/"),
            "https://gpu.example.com:443/v1"
        );
    }

    #[test]
    fn local_model_choice_prefers_code_models_and_skips_embeddings() {
        let models: Vec<String> = ["nomic-embed-text", "llama3.2:3b", "qwen2.5-coder:3b"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(pick_local_model(&models).as_deref(), Some("qwen2.5-coder:3b"));
        assert_eq!(
            pick_local_model(&models[..2]).as_deref(),
            Some("llama3.2:3b")
        );
        assert_eq!(pick_local_model(&models[..1]), None);
    }
}
