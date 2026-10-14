//! Language server registry: built-in server definitions per language id, user
//! overrides from `lsp.json`, executable discovery and project-root detection.

use crate::LspError;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// How to launch one language server.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerSpec {
    /// Stable identifier, used together with the root to key running servers
    /// (e.g. `rust-analyzer`, `typescript-language-server`).
    pub id: String,
    /// Executable name or path.
    pub command: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Files/directories marking a project root, in priority order.
    pub root_markers: Vec<String>,
    /// Human-readable installation instructions shown when the server is missing.
    pub install_hint: String,
    /// `initializationOptions` sent with `initialize`.
    pub initialization_options: Option<Value>,
    /// Settings returned for `workspace/configuration` (looked up by dotted section).
    pub settings: Value,
    /// Extra environment variables for the process.
    pub env: Vec<(String, String)>,
}

impl ServerSpec {
    fn new(id: &str, command: &str, args: &[&str], markers: &[&str], hint: &str) -> Self {
        Self {
            id: id.to_string(),
            command: command.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            root_markers: markers.iter().map(|s| s.to_string()).collect(),
            install_hint: hint.to_string(),
            initialization_options: None,
            settings: Value::Null,
            env: Vec::new(),
        }
    }

    fn with_init(mut self, init: Value) -> Self {
        self.initialization_options = Some(init);
        self
    }

    fn with_settings(mut self, settings: Value) -> Self {
        self.settings = settings;
        self
    }
}

/// Per-language override loaded from `lsp.json`.
///
/// ```json
/// {
///   "languages": {
///     "python": { "command": "pylsp", "args": [] },
///     "rust": { "settings": { "rust-analyzer": { "check": { "command": "clippy" } } } },
///     "markdown": { "enabled": false }
///   }
/// }
/// ```
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LanguageOverride {
    /// Replaces the built-in command (the built-in candidates are then ignored).
    pub command: Option<String>,
    /// Arguments for `command` (or replacement args for the built-in server).
    pub args: Option<Vec<String>>,
    /// `false` disables language server support for this language.
    pub enabled: Option<bool>,
    /// `initializationOptions` for `initialize`.
    pub initialization_options: Option<Value>,
    /// Settings answered to `workspace/configuration`.
    pub settings: Option<Value>,
    /// Root markers replacing the built-in ones.
    pub root_markers: Option<Vec<String>>,
    /// Extra environment variables.
    pub env: Option<HashMap<String, String>>,
}

/// User configuration (`$XDG_CONFIG_HOME/ide-levity/lsp.json`).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LspConfig {
    /// Overrides keyed by language id (`rust`, `python`, `typescript`, ...).
    #[serde(default)]
    pub languages: HashMap<String, LanguageOverride>,
}

impl LspConfig {
    /// Parses a configuration from JSON text.
    pub fn from_json_str(text: &str) -> Result<Self, LspError> {
        serde_json::from_str(text).map_err(|e| LspError::Config(e.to_string()))
    }

    /// Default location: `$XDG_CONFIG_HOME/ide-levity/lsp.json`, falling back to
    /// `%APPDATA%\ide-levity\lsp.json` on Windows and `~/.config/ide-levity/lsp.json`.
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                if cfg!(windows) {
                    std::env::var_os("APPDATA").map(PathBuf::from)
                } else {
                    None
                }
            })
            .or_else(|| home_dir().map(|h| h.join(".config")))?;
        Some(base.join("ide-levity").join("lsp.json"))
    }

    /// Loads the configuration from `path`; a missing file yields the default config.
    pub fn load(path: &Path) -> Result<Self, LspError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_json_str(&text)
                .map_err(|e| LspError::Config(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(LspError::Config(format!("{}: {e}", path.display()))),
        }
    }

    /// Loads the configuration from [`LspConfig::default_path`].
    pub fn load_default() -> Result<Self, LspError> {
        match Self::default_path() {
            Some(path) => Self::load(&path),
            None => Ok(Self::default()),
        }
    }
}

/// Result of looking up a server for a language.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerAvailability {
    /// A server was found on this machine.
    Available {
        /// The chosen server.
        spec: ServerSpec,
        /// Resolved executable path.
        executable: PathBuf,
    },
    /// Servers are known for this language but none is installed.
    Missing {
        /// Commands that were searched for, in preference order.
        commands: Vec<String>,
        /// Installation instructions.
        install_hint: String,
    },
    /// Disabled by user configuration.
    Disabled,
    /// No server is known for this language.
    Unsupported,
}

/// Normalizes common aliases (`tsx`, `jsx`, `ts`, `js`, `py`, `rs`, ...) to LSP language ids.
pub fn normalize_language_id(language_id: &str) -> String {
    let lower = language_id.trim().to_ascii_lowercase();
    match lower.as_str() {
        "tsx" => "typescriptreact",
        "jsx" => "javascriptreact",
        "ts" => "typescript",
        "js" => "javascript",
        "py" => "python",
        "rs" => "rust",
        "md" => "markdown",
        "kt" | "kts" => "kotlin",
        "sh" | "bash" => "shellscript",
        other => other,
    }
    .to_string()
}

/// Maps a file path to its LSP language id by extension (or well-known file name).
pub fn language_id_for_path(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?;
    match name {
        "Cargo.lock" => return None,
        "tsconfig.json" | "jsconfig.json" | ".eslintrc.json" => return Some("jsonc"),
        _ => {}
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "json" => "json",
        "jsonc" => "jsonc",
        "md" | "markdown" => "markdown",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "less" => "less",
        _ => return None,
    })
}

fn builtin_servers() -> Vec<(&'static [&'static str], Vec<ServerSpec>)> {
    const NPM_TS: &str = "npm install -g typescript typescript-language-server";
    const NPM_VSCODE: &str = "npm install -g vscode-langservers-extracted";
    let web_markers = ["tsconfig.json", "jsconfig.json", "package.json"];
    vec![
        (
            &["rust"][..],
            vec![ServerSpec::new(
                "rust-analyzer",
                "rust-analyzer",
                &[],
                &["Cargo.toml", "rust-project.json"],
                "rust-analyzer not found. Install it with `rustup component add rust-analyzer`.",
            )],
        ),
        (
            &[
                "typescript",
                "javascript",
                "typescriptreact",
                "javascriptreact",
            ][..],
            vec![ServerSpec::new(
                "typescript-language-server",
                "typescript-language-server",
                &["--stdio"],
                &web_markers,
                &format!("typescript-language-server not found. Install it with `{NPM_TS}`."),
            )],
        ),
        (
            &["python"][..],
            vec![
                ServerSpec::new(
                    "pyright",
                    "pyright-langserver",
                    &["--stdio"],
                    &[
                        "pyproject.toml",
                        "pyrightconfig.json",
                        "setup.py",
                        "setup.cfg",
                        "requirements.txt",
                        "Pipfile",
                    ],
                    "No Python language server found. Install one with `npm install -g pyright` or `pip install python-lsp-server`.",
                )
                .with_settings(json!({
                    "python": {
                        "analysis": {
                            "autoSearchPaths": true,
                            "useLibraryCodeForTypes": true,
                            "diagnosticMode": "openFilesOnly"
                        }
                    }
                })),
                ServerSpec::new(
                    "pylsp",
                    "pylsp",
                    &[],
                    &["pyproject.toml", "setup.py", "setup.cfg", "requirements.txt"],
                    "No Python language server found. Install one with `npm install -g pyright` or `pip install python-lsp-server`.",
                ),
            ],
        ),
        (
            &["json", "jsonc"][..],
            vec![ServerSpec::new(
                "vscode-json-language-server",
                "vscode-json-language-server",
                &["--stdio"],
                &["package.json"],
                &format!("vscode-json-language-server not found. Install it with `{NPM_VSCODE}`."),
            )
            .with_init(json!({ "provideFormatter": true }))
            .with_settings(json!({ "json": { "validate": { "enable": true }, "format": { "enable": true } } }))],
        ),
        (
            &["html"][..],
            vec![ServerSpec::new(
                "vscode-html-language-server",
                "vscode-html-language-server",
                &["--stdio"],
                &["package.json"],
                &format!("vscode-html-language-server not found. Install it with `{NPM_VSCODE}`."),
            )
            .with_init(json!({ "provideFormatter": true }))],
        ),
        (
            &["css", "scss", "less"][..],
            vec![ServerSpec::new(
                "vscode-css-language-server",
                "vscode-css-language-server",
                &["--stdio"],
                &["package.json"],
                &format!("vscode-css-language-server not found. Install it with `{NPM_VSCODE}`."),
            )
            .with_init(json!({ "provideFormatter": true }))
            .with_settings(json!({
                "css": { "validate": true },
                "scss": { "validate": true },
                "less": { "validate": true }
            }))],
        ),
        (
            &["markdown"][..],
            vec![ServerSpec::new(
                "marksman",
                "marksman",
                &["server"],
                &[".marksman.toml"],
                "marksman not found. Download it from https://github.com/artempyanykh/marksman/releases (or `brew install marksman`).",
            )],
        ),
        (
            &["kotlin"][..],
            vec![ServerSpec::new(
                "kotlin-language-server",
                "kotlin-language-server",
                &[],
                &[
                    "settings.gradle.kts",
                    "settings.gradle",
                    "build.gradle.kts",
                    "build.gradle",
                    "pom.xml",
                ],
                "kotlin-language-server not found. Install it from https://github.com/fwcd/kotlin-language-server/releases (or `brew install kotlin-language-server`).",
            )],
        ),
        (
            &["swift"][..],
            vec![ServerSpec::new(
                "sourcekit-lsp",
                "sourcekit-lsp",
                &[],
                &["Package.swift", "compile_commands.json"],
                "sourcekit-lsp not found. It ships with Xcode and with Swift toolchains from https://swift.org/install.",
            )],
        ),
    ]
}

/// Registry of language servers with user overrides applied.
#[derive(Debug, Clone)]
pub struct ServerRegistry {
    languages: HashMap<String, Vec<ServerSpec>>,
    disabled: Vec<String>,
    extra_search_dirs: Vec<PathBuf>,
}

impl Default for ServerRegistry {
    fn default() -> Self {
        Self::builtin()
    }
}

impl ServerRegistry {
    /// Built-in servers only.
    pub fn builtin() -> Self {
        let mut languages = HashMap::new();
        for (ids, specs) in builtin_servers() {
            for id in ids {
                languages.insert(id.to_string(), specs.clone());
            }
        }
        Self {
            languages,
            disabled: Vec::new(),
            extra_search_dirs: default_extra_search_dirs(),
        }
    }

    /// Built-in servers with `config` overrides applied.
    pub fn with_config(config: &LspConfig) -> Self {
        let mut registry = Self::builtin();
        for (language, ov) in &config.languages {
            let language = normalize_language_id(language);
            if ov.enabled == Some(false) {
                registry.disabled.push(language.clone());
                continue;
            }
            let env: Vec<(String, String)> = ov
                .env
                .as_ref()
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default();
            let mut specs = match &ov.command {
                Some(command) => {
                    let id = Path::new(command)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(command)
                        .to_string();
                    let existing_markers = registry
                        .languages
                        .get(&language)
                        .and_then(|c| c.first())
                        .map(|s| s.root_markers.clone())
                        .unwrap_or_default();
                    vec![ServerSpec {
                        id,
                        command: command.clone(),
                        args: Vec::new(),
                        root_markers: existing_markers,
                        install_hint: format!(
                            "`{command}` (configured for {language} in lsp.json) was not found on PATH."
                        ),
                        initialization_options: None,
                        settings: Value::Null,
                        env: Vec::new(),
                    }]
                }
                None => registry.languages.get(&language).cloned().unwrap_or_default(),
            };
            for spec in &mut specs {
                if let Some(args) = &ov.args {
                    spec.args = args.clone();
                }
                if let Some(init) = &ov.initialization_options {
                    spec.initialization_options = Some(init.clone());
                }
                if let Some(settings) = &ov.settings {
                    spec.settings = settings.clone();
                }
                if let Some(markers) = &ov.root_markers {
                    spec.root_markers = markers.clone();
                }
                spec.env.extend(env.iter().cloned());
            }
            if !specs.is_empty() {
                registry.languages.insert(language, specs);
            }
        }
        registry
    }

    /// Adds a directory searched for executables after `PATH`.
    pub fn add_search_dir(&mut self, dir: PathBuf) {
        self.extra_search_dirs.push(dir);
    }

    /// Language ids with at least one known server.
    pub fn languages(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.languages.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Candidate servers for a language, in preference order.
    pub fn candidates(&self, language_id: &str) -> &[ServerSpec] {
        self.languages
            .get(&normalize_language_id(language_id))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Finds the first installed candidate for `language_id`.
    pub fn resolve(&self, language_id: &str) -> ServerAvailability {
        let language = normalize_language_id(language_id);
        if self.disabled.contains(&language) {
            return ServerAvailability::Disabled;
        }
        let candidates = self.candidates(&language);
        if candidates.is_empty() {
            return ServerAvailability::Unsupported;
        }
        for spec in candidates {
            if let Some(executable) = find_executable(&spec.command, &self.extra_search_dirs) {
                return ServerAvailability::Available {
                    spec: spec.clone(),
                    executable,
                };
            }
        }
        ServerAvailability::Missing {
            commands: candidates.iter().map(|s| s.command.clone()).collect(),
            install_hint: candidates[0].install_hint.clone(),
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Directories searched after `PATH`: GUI apps launched from a desktop session often
/// lack shell-profile `PATH` entries such as `~/.cargo/bin` or nvm's node directory.
fn default_extra_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = home_dir() {
        dirs.push(home.join(".cargo").join("bin"));
        dirs.push(home.join(".local").join("bin"));
        dirs.push(home.join(".npm-global").join("bin"));
        dirs.push(home.join(".bun").join("bin"));
        dirs.push(home.join(".volta").join("bin"));
        let nvm = home.join(".nvm").join("versions").join("node");
        if let Ok(entries) = std::fs::read_dir(&nvm) {
            let mut versions: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_dir())
                .collect();
            // Newest first ("v24.1.0" > "v18.0.0" with numeric comparison).
            versions.sort_by_key(|p| {
                std::cmp::Reverse(
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| {
                            n.trim_start_matches('v')
                                .split('.')
                                .map(|x| x.parse::<u64>().unwrap_or(0))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                )
            });
            dirs.extend(versions.into_iter().map(|v| v.join("bin")));
        }
        if cfg!(windows) {
            if let Some(appdata) = std::env::var_os("APPDATA") {
                dirs.push(PathBuf::from(appdata).join("npm"));
            }
        }
    }
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Finds an executable by name on `PATH` (then in `extra_dirs`), or validates an
/// explicit path. On Windows the `PATHEXT` extensions are tried.
pub fn find_executable(command: &str, extra_dirs: &[PathBuf]) -> Option<PathBuf> {
    let candidate_names: Vec<String> = if cfg!(windows) && Path::new(command).extension().is_none()
    {
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into());
        std::iter::once(command.to_string())
            .chain(exts.split(';').map(|e| format!("{command}{}", e.to_ascii_lowercase())))
            .collect()
    } else {
        vec![command.to_string()]
    };

    let cmd_path = Path::new(command);
    if cmd_path.components().count() > 1 || cmd_path.is_absolute() {
        return candidate_names
            .iter()
            .map(PathBuf::from)
            .find(|p| is_executable_file(p));
    }

    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.extend(extra_dirs.iter().cloned());
    for dir in dirs {
        for name in &candidate_names {
            let full = dir.join(name);
            if is_executable_file(&full) {
                return Some(full);
            }
        }
    }
    None
}

/// Matches one path segment against a pattern supporting `*` and `?`.
fn segment_matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Matches a relative path (as segments) against a Cargo `members` glob.
fn glob_matches(pattern: &[&str], path: &[String]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| glob_matches(rest, &path[skip..])),
        Some((first, rest)) => match path.split_first() {
            Some((seg, path_rest)) => segment_matches(first, seg) && glob_matches(rest, path_rest),
            None => false,
        },
    }
}

/// `true` when `ws_dir/Cargo.toml` declares a `[workspace]` that includes `member_dir`.
fn cargo_workspace_includes(ws_dir: &Path, member_dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(ws_dir.join("Cargo.toml")) else {
        return false;
    };
    let Ok(manifest) = text.parse::<toml::Table>() else {
        return false;
    };
    let Some(workspace) = manifest.get("workspace").and_then(|w| w.as_table()) else {
        return false;
    };
    let Ok(rel) = member_dir.strip_prefix(ws_dir) else {
        return false;
    };
    let rel: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if rel.is_empty() {
        return true;
    }
    let patterns = |key: &str| -> Vec<String> {
        workspace
            .get(key)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.trim_end_matches('/').to_string()))
                    .collect()
            })
            .unwrap_or_default()
    };
    let matches_any = |pats: &[String], prefix_ok: bool| {
        pats.iter().any(|p| {
            let segs: Vec<&str> = p.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
            glob_matches(&segs, &rel)
                || (prefix_ok
                    && (1..rel.len()).any(|n| glob_matches(&segs, &rel[..n])))
        })
    };
    if matches_any(&patterns("exclude"), true) {
        return false;
    }
    matches_any(&patterns("members"), false)
}

/// Determines the project root for `file` given a server's root markers.
///
/// The nearest ancestor containing any marker wins. For Cargo projects, an
/// enclosing `Cargo.toml` that declares `[workspace]` is preferred so all members of
/// a workspace share one rust-analyzer. Without markers, the nearest `.git` ancestor
/// is used, and finally the file's own directory.
pub fn find_root(file: &Path, root_markers: &[String]) -> PathBuf {
    let start = if file.is_dir() {
        file.to_path_buf()
    } else {
        file.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| file.to_path_buf())
    };

    let nearest_marker = start
        .ancestors()
        .find(|dir| root_markers.iter().any(|m| dir.join(m).exists()))
        .map(Path::to_path_buf);

    if let Some(found) = &nearest_marker {
        if root_markers.iter().any(|m| m == "Cargo.toml") && found.join("Cargo.toml").exists() {
            if let Some(ws) = found
                .ancestors()
                .filter(|dir| cargo_workspace_includes(dir, found))
                .last()
            {
                return ws.to_path_buf();
            }
        }
        return found.clone();
    }

    start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
        .unwrap_or(start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ide-lsp-registry-{name}-{}-{}",
            std::process::id(),
            fastrand::u64(..)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn language_ids_from_paths_and_aliases() {
        assert_eq!(language_id_for_path(Path::new("a/b.rs")), Some("rust"));
        assert_eq!(
            language_id_for_path(Path::new("x.TSX")),
            Some("typescriptreact")
        );
        assert_eq!(language_id_for_path(Path::new("tsconfig.json")), Some("jsonc"));
        assert_eq!(language_id_for_path(Path::new("Makefile")), None);
        assert_eq!(normalize_language_id("tsx"), "typescriptreact");
        assert_eq!(normalize_language_id("Rust"), "rust");
    }

    #[test]
    fn registry_resolves_and_reports_missing() {
        let reg = ServerRegistry::builtin();
        assert_eq!(reg.resolve("cobol"), ServerAvailability::Unsupported);
        assert!(reg.candidates("javascript")[0].id == "typescript-language-server");
        assert_eq!(reg.candidates("python").len(), 2);
        let config = LspConfig::from_json_str(
            r#"{"languages":{"rust":{"command":"/nonexistent/ra-xyz","args":["--x"]},"markdown":{"enabled":false},"python":{"settings":{"python":{"x":1}}}}}"#,
        )
        .unwrap();
        let reg = ServerRegistry::with_config(&config);
        assert_eq!(reg.resolve("markdown"), ServerAvailability::Disabled);
        match reg.resolve("rust") {
            ServerAvailability::Missing { commands, install_hint } => {
                assert_eq!(commands, vec!["/nonexistent/ra-xyz".to_string()]);
                assert!(install_hint.contains("lsp.json"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(reg.candidates("rust")[0].args, vec!["--x".to_string()]);
        assert_eq!(reg.candidates("python")[0].settings["python"]["x"], 1);
        assert!(LspConfig::from_json_str(r#"{"languages":{"rust":{"bogus":1}}}"#).is_err());
    }

    #[test]
    fn config_load_handles_missing_file() {
        let dir = scratch("cfg");
        assert_eq!(
            LspConfig::load(&dir.join("absent.json")).unwrap(),
            LspConfig::default()
        );
        std::fs::write(dir.join("lsp.json"), r#"{"languages":{"sh":{"command":"sh"}}}"#).unwrap();
        let cfg = LspConfig::load(&dir.join("lsp.json")).unwrap();
        let reg = ServerRegistry::with_config(&cfg);
        assert!(matches!(
            reg.resolve("shellscript"),
            ServerAvailability::Available { .. }
        ));
    }

    #[test]
    fn finds_executables_on_path() {
        assert!(find_executable("sh", &[]).is_some());
        assert!(find_executable("no-such-binary-ide-levity", &[]).is_none());
        assert!(find_executable("/bin/sh", &[]).is_some());
    }

    #[test]
    fn root_detection_prefers_cargo_workspace_then_markers_then_git() {
        let base = scratch("root");
        let ws = base.join("ws");
        let member = ws.join("crates").join("member");
        std::fs::create_dir_all(member.join("src")).unwrap();
        std::fs::write(ws.join("Cargo.toml"), "[workspace]\nmembers=[\"crates/*\"]\n").unwrap();
        std::fs::write(member.join("Cargo.toml"), "[package]\nname=\"m\"\n").unwrap();
        let markers = vec!["Cargo.toml".to_string()];
        assert_eq!(find_root(&member.join("src/lib.rs"), &markers), ws);

        // A crate nested inside the workspace directory but not listed as a member
        // (like a scratch project under `target/`) keeps its own root.
        let stray = ws.join("target").join("tmp").join("stray");
        std::fs::create_dir_all(stray.join("src")).unwrap();
        std::fs::write(stray.join("Cargo.toml"), "[package]\nname=\"s\"\n").unwrap();
        assert_eq!(find_root(&stray.join("src/main.rs"), &markers), stray);
        assert!(segment_matches("ide-*", "ide-lsp"));
        assert!(!segment_matches("ide-*", "core"));
        assert!(glob_matches(
            &["crates", "**"],
            &["crates".into(), "a".into(), "b".into()]
        ));

        let lone = base.join("lone");
        std::fs::create_dir_all(lone.join("src")).unwrap();
        std::fs::write(lone.join("Cargo.toml"), "[package]\nname=\"l\"\n").unwrap();
        assert_eq!(find_root(&lone.join("src/main.rs"), &markers), lone);

        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("a/b")).unwrap();
        assert_eq!(
            find_root(&repo.join("a/b/x.py"), &["pyproject.toml".to_string()]),
            repo
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
