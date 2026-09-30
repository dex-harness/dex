//! Runtime configuration.
//!
//! Values come from the process environment, overlaid on a `.env` file. The
//! `.env` file is for developer convenience and is gitignored; the environment
//! always wins, so a deployment can override anything without editing a file.
//!
//! The parsing here is deliberately hand-written and side-effect free (it takes
//! a `&str` and returns a map) so it can be tested without touching the
//! process-global environment.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::auth::{Authority, AuthorityParseError};
use crate::budget::ExecutionBudget;

pub const DEFAULT_BASE_URL: &str = "https://opencode.ai/zen/go/v1";
pub const DEFAULT_MODEL: &str = "kimi-k2.7-code";

/// How the runtime reaches the model.
#[derive(Clone, Debug)]
pub struct ProviderConfig {
    /// Root of an OpenAI-compatible API. Any provider exposing
    /// `POST {base_url}/chat/completions` works.
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// Header carrying the stable session id, which lets a gateway route and
    /// cache per conversation.
    pub session_header: String,
}

impl ProviderConfig {
    /// Full endpoint for chat completions.
    pub fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub provider: ProviderConfig,
    pub socket_path: PathBuf,
    pub memory_dir: PathBuf,
    pub max_sessions: usize,
    pub max_program_rounds: u32,
    pub budget: ExecutionBudget,
    pub authority: Authority,
}

/// A setting the runtime cannot start without.
#[derive(Clone, Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(
        "no API key found. Set {primary} (or {fallback}) in the environment or in .env; \
         see .env.example"
    )]
    MissingApiKey { primary: String, fallback: String },
    #[error("could not read {path}: {message}")]
    ReadEnv { path: String, message: String },
    #[error("DEX_AUTHORITY is not valid: {0}")]
    BadAuthority(#[from] AuthorityParseError),
    #[error("{name} must be a positive integer, got {value:?}")]
    BadNumber { name: &'static str, value: String },
    #[error("{name} must be a valid URL, got {value:?}")]
    BadUrl { name: &'static str, value: String },
}

/// Load configuration from the environment plus an optional `.env` file.
pub fn load(env_file: Option<&Path>) -> Result<RuntimeConfig, ConfigError> {
    let mut values = HashMap::new();
    if let Some(path) = env_file {
        if path.exists() {
            let text = std::fs::read_to_string(path).map_err(|e| ConfigError::ReadEnv {
                path: path.display().to_string(),
                message: e.to_string(),
            })?;
            values.extend(parse_env(&text));
        }
    }
    // Real environment wins over the file.
    for (key, value) in std::env::vars() {
        values.insert(key, value);
    }
    from_values(&values)
}

/// Build a configuration from an explicit map. Split out from [`load`] so tests
/// can exercise every branch without mutating the process environment.
pub fn from_values(values: &HashMap<String, String>) -> Result<RuntimeConfig, ConfigError> {
    let get = |key: &str| values.get(key).map(String::as_str);

    let api_key = get("OPENCODE_GO_API_KEY")
        .or_else(|| get("OPENCODE_API_KEY"))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or(ConfigError::MissingApiKey {
            primary: "OPENCODE_GO_API_KEY".to_string(),
            fallback: "OPENCODE_API_KEY".to_string(),
        })?
        .to_string();

    let base_url = get("DEX_BASE_URL")
        .unwrap_or(DEFAULT_BASE_URL)
        .trim_end_matches('/')
        .to_string();
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        return Err(ConfigError::BadUrl {
            name: "DEX_BASE_URL",
            value: base_url,
        });
    }

    let provider = ProviderConfig {
        base_url,
        api_key,
        model: get("DEX_MODEL")
            .unwrap_or(DEFAULT_MODEL)
            .trim()
            .to_string(),
        session_header: get("DEX_SESSION_HEADER")
            .unwrap_or("x-opencode-session")
            .trim()
            .to_string(),
    };

    let socket_path = expand_home(get("DEX_SOCKET").unwrap_or("~/.dex/dex.sock"));
    let memory_dir = expand_home(get("DEX_MEMORY_DIR").unwrap_or("~/.dex/memory"));

    let budget = ExecutionBudget {
        wall_clock: Duration::from_millis(number(values, "DEX_BUDGET_WALL_CLOCK_MS", 30_000)?),
        instructions: number(values, "DEX_BUDGET_INSTRUCTIONS", 20_000_000)?,
        capability_calls: number(values, "DEX_BUDGET_CAPABILITY_CALLS", 100)? as u32,
        read_bytes: number(values, "DEX_BUDGET_READ_BYTES", 33_554_432)?,
        write_bytes: number(values, "DEX_BUDGET_WRITE_BYTES", 8_388_608)?,
        output_bytes: number(values, "DEX_BUDGET_OUTPUT_BYTES", 262_144)?,
        command: Duration::from_millis(number(values, "DEX_BUDGET_COMMAND_MS", 120_000)?),
    };

    Ok(RuntimeConfig {
        provider,
        socket_path,
        memory_dir,
        max_sessions: number(values, "DEX_MAX_SESSIONS", 32)? as usize,
        max_program_rounds: number(values, "DEX_MAX_PROGRAM_ROUNDS", 8)? as u32,
        budget,
        authority: Authority::parse(get("DEX_AUTHORITY").unwrap_or(""))?,
    })
}

fn number(
    values: &HashMap<String, String>,
    key: &'static str,
    default: u64,
) -> Result<u64, ConfigError> {
    match values.get(key).map(|v| v.trim()).filter(|v| !v.is_empty()) {
        None => Ok(default),
        Some(raw) => raw
            .parse()
            .map_err(|_| ConfigError::BadNumber {
                name: key,
                value: raw.to_string(),
            }),
    }
}

/// Expand a leading `~` to the user's home directory.
pub fn expand_home(raw: &str) -> PathBuf {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    if raw == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(raw)
}

/// Parse `.env` content: `KEY=VALUE` lines, `#` comments, optional surrounding
/// quotes. Malformed lines are skipped rather than fatal, so a stray note in the
/// file cannot stop the runtime from starting.
pub fn parse_env(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = value.trim();
        let value = if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value[1..value.len() - 1].to_string()
        } else {
            // Strip a trailing inline comment only for unquoted values.
            match value.find(" #") {
                Some(index) => value[..index].trim_end().to_string(),
                None => value.to_string(),
            }
        };
        out.insert(key.to_string(), value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn with_key(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        let mut v = values(pairs);
        v.insert("OPENCODE_GO_API_KEY".into(), "test-key".into());
        v
    }

    #[test]
    fn env_parsing_handles_comments_quotes_and_export() {
        let parsed = parse_env(
            r#"
            # a comment
            PLAIN=value
            export EXPORTED=exported
            QUOTED="quoted value"
            SINGLE='single'
            WITH_COMMENT=value # trailing
            EMPTY=
            NO_EQUALS_SIGN
            "#,
        );
        assert_eq!(parsed["PLAIN"], "value");
        assert_eq!(parsed["EXPORTED"], "exported");
        assert_eq!(parsed["QUOTED"], "quoted value");
        assert_eq!(parsed["SINGLE"], "single");
        assert_eq!(parsed["WITH_COMMENT"], "value");
        assert_eq!(parsed["EMPTY"], "");
        assert!(!parsed.contains_key("NO_EQUALS_SIGN"));
    }

    #[test]
    fn a_quoted_value_keeps_its_hash() {
        let parsed = parse_env("K=\"a # b\"");
        assert_eq!(parsed["K"], "a # b");
    }

    #[test]
    fn a_missing_api_key_is_named_in_the_error() {
        let err = from_values(&HashMap::new()).expect_err("must fail");
        let text = err.to_string();
        assert!(text.contains("OPENCODE_GO_API_KEY"), "{text}");
        assert!(text.contains("OPENCODE_API_KEY"), "{text}");
    }

    #[test]
    fn the_fallback_key_is_accepted() {
        let config = from_values(&values(&[("OPENCODE_API_KEY", "fallback")])).expect("config");
        assert_eq!(config.provider.api_key, "fallback");
    }

    #[test]
    fn the_openai_go_key_wins_over_the_fallback() {
        let config = from_values(&values(&[
            ("OPENCODE_GO_API_KEY", "primary"),
            ("OPENCODE_API_KEY", "fallback"),
        ]))
        .expect("config");
        assert_eq!(config.provider.api_key, "primary");
    }

    #[test]
    fn the_chat_url_is_derived_from_the_base() {
        let mut v = with_key(&[]);
        v.insert("DEX_BASE_URL".into(), "https://example.com/v1/".into());
        let config = from_values(&v).expect("config");
        assert_eq!(
            config.provider.chat_completions_url(),
            "https://example.com/v1/chat/completions"
        );
    }

    #[test]
    fn a_non_http_base_url_is_rejected() {
        let mut v = with_key(&[]);
        v.insert("DEX_BASE_URL".into(), "not-a-url".into());
        assert!(from_values(&v).is_err());
    }

    #[test]
    fn budgets_are_configurable_and_default_sensibly() {
        let mut v = with_key(&[]);
        v.insert("DEX_BUDGET_CAPABILITY_CALLS".into(), "7".into());
        let config = from_values(&v).expect("config");
        assert_eq!(config.budget.capability_calls, 7);
        // Unset budgets keep their defaults.
        assert_eq!(config.budget.read_bytes, 33_554_432);
    }

    #[test]
    fn a_non_numeric_budget_is_rejected_with_its_name() {
        let mut v = with_key(&[]);
        v.insert("DEX_MAX_SESSIONS".into(), "many".into());
        let err = from_values(&v).expect_err("must fail");
        assert!(err.to_string().contains("DEX_MAX_SESSIONS"), "{err}");
    }

    #[test]
    fn an_unset_authority_denies_everything() {
        let config = from_values(&with_key(&[])).expect("config");
        assert!(!config.authority.grants("filesystem.read"));
    }

    #[test]
    fn authority_is_parsed_from_config() {
        let config = from_values(&with_key(&[(
            "DEX_AUTHORITY",
            "filesystem.read=/work/**;memory.write=*",
        )]))
        .expect("config");
        assert!(config.authority.check("filesystem.read", "/work/a.rs").is_ok());
        assert!(config.authority.check("memory.write", "anything").is_ok());
        assert!(config.authority.check("filesystem.write", "/work/a.rs").is_err());
    }
}
