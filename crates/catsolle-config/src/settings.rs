use crate::paths::AppPaths;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml error: {0}")]
    Toml(#[from] toml::de::Error),
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AppConfig {
    pub locale: LocaleConfig,
    pub ui: UiConfig,
    pub ai: AiConfig,
    pub ssh: SshDefaults,
    pub transfer: TransferConfig,
    pub logging: LoggingConfig,
    pub keychain: KeychainConfig,
    pub recording: RecordingConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AppConfigLayer {
    pub locale: Option<LocaleConfigLayer>,
    pub ui: Option<UiConfigLayer>,
    pub ai: Option<AiConfigLayer>,
    pub ssh: Option<SshDefaultsLayer>,
    pub transfer: Option<TransferConfigLayer>,
    pub logging: Option<LoggingConfigLayer>,
    pub keychain: Option<KeychainConfigLayer>,
    pub recording: Option<RecordingConfigLayer>,
}

impl AppConfigLayer {
    pub fn apply_to(self, cfg: &mut AppConfig) {
        if let Some(layer) = self.locale {
            cfg.locale.apply(layer);
        }
        if let Some(layer) = self.ui {
            cfg.ui.apply(layer);
        }
        if let Some(layer) = self.ai {
            cfg.ai.apply(layer);
        }
        if let Some(layer) = self.ssh {
            cfg.ssh.apply(layer);
        }
        if let Some(layer) = self.transfer {
            cfg.transfer.apply(layer);
        }
        if let Some(layer) = self.logging {
            cfg.logging.apply(layer);
        }
        if let Some(layer) = self.keychain {
            cfg.keychain.apply(layer);
        }
        if let Some(layer) = self.recording {
            cfg.recording.apply(layer);
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocaleConfig {
    pub default_language: String,
    pub fallback_language: String,
    pub preferred_languages: Vec<String>,
}

impl Default for LocaleConfig {
    fn default() -> Self {
        Self {
            default_language: "en".to_string(),
            fallback_language: "en".to_string(),
            preferred_languages: vec!["en".to_string()],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct LocaleConfigLayer {
    pub default_language: Option<String>,
    pub fallback_language: Option<String>,
    pub preferred_languages: Option<Vec<String>>,
}

impl LocaleConfig {
    fn apply(&mut self, layer: LocaleConfigLayer) {
        if let Some(v) = layer.default_language {
            self.default_language = v;
        }
        if let Some(v) = layer.fallback_language {
            self.fallback_language = v;
        }
        if let Some(v) = layer.preferred_languages {
            self.preferred_languages = v;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiConfig {
    pub theme: String,
    pub layout: String,
    pub scrollback_lines: usize,
    pub show_hidden_files: bool,
    pub keybindings: Option<PathBuf>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "neko-dark".to_string(),
            layout: "split-horizontal".to_string(),
            scrollback_lines: 20000,
            show_hidden_files: false,
            keybindings: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct UiConfigLayer {
    pub theme: Option<String>,
    pub layout: Option<String>,
    pub scrollback_lines: Option<usize>,
    pub show_hidden_files: Option<bool>,
    pub keybindings: Option<PathBuf>,
}

impl UiConfig {
    fn apply(&mut self, layer: UiConfigLayer) {
        if let Some(v) = layer.theme {
            self.theme = v;
        }
        if let Some(v) = layer.layout {
            self.layout = v;
        }
        if let Some(v) = layer.scrollback_lines {
            self.scrollback_lines = v;
        }
        if let Some(v) = layer.show_hidden_files {
            self.show_hidden_files = v;
        }
        if layer.keybindings.is_some() {
            self.keybindings = layer.keybindings;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AiConfig {
    pub enabled: bool,
    pub provider: String,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub history_max: usize,
    pub timeout_ms: u64,
    pub system_prompt: String,
    pub streaming: bool,
    pub agent_enabled: bool,
    pub auto_mode: bool,
    pub max_steps: u32,
    pub tools_enabled: bool,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "ollama".to_string(),
            endpoint: "http://localhost:11434".to_string(),
            api_key: None,
            model: "qwen2.5:3b".to_string(),
            temperature: 0.2,
            max_tokens: 512,
            history_max: 40,
            timeout_ms: 20000,
            system_prompt: "Answer in Russian. Be concise.".to_string(),
            streaming: true,
            agent_enabled: true,
            auto_mode: false,
            max_steps: 6,
            tools_enabled: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AiConfigLayer {
    pub enabled: Option<bool>,
    pub provider: Option<String>,
    pub endpoint: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub history_max: Option<usize>,
    pub timeout_ms: Option<u64>,
    pub system_prompt: Option<String>,
    pub streaming: Option<bool>,
    pub agent_enabled: Option<bool>,
    pub auto_mode: Option<bool>,
    pub max_steps: Option<u32>,
    pub tools_enabled: Option<bool>,
}

impl AiConfig {
    fn apply(&mut self, layer: AiConfigLayer) {
        if let Some(v) = layer.enabled {
            self.enabled = v;
        }
        if let Some(v) = layer.provider {
            self.provider = v;
        }
        if let Some(v) = layer.endpoint {
            self.endpoint = v;
        }
        if layer.api_key.is_some() {
            self.api_key = layer.api_key;
        }
        if let Some(v) = layer.model {
            self.model = v;
        }
        if let Some(v) = layer.temperature {
            self.temperature = v;
        }
        if let Some(v) = layer.max_tokens {
            self.max_tokens = v;
        }
        if let Some(v) = layer.history_max {
            self.history_max = v;
        }
        if let Some(v) = layer.timeout_ms {
            self.timeout_ms = v;
        }
        if let Some(v) = layer.system_prompt {
            self.system_prompt = v;
        }
        if let Some(v) = layer.streaming {
            self.streaming = v;
        }
        if let Some(v) = layer.agent_enabled {
            self.agent_enabled = v;
        }
        if let Some(v) = layer.auto_mode {
            self.auto_mode = v;
        }
        if let Some(v) = layer.max_steps {
            self.max_steps = v;
        }
        if let Some(v) = layer.tools_enabled {
            self.tools_enabled = v;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SshDefaults {
    pub port: u16,
    pub connect_timeout_ms: u64,
    pub keepalive_interval_secs: u64,
    pub reconnect: bool,
    pub reconnect_backoff_ms: u64,
    pub agent_forwarding: bool,
    pub x11_forwarding: bool,
    pub preferred_kex: Vec<String>,
    pub preferred_ciphers: Vec<String>,
    pub preferred_macs: Vec<String>,
}

impl Default for SshDefaults {
    fn default() -> Self {
        Self {
            port: 22,
            connect_timeout_ms: 15000,
            keepalive_interval_secs: 15,
            reconnect: true,
            reconnect_backoff_ms: 1000,
            agent_forwarding: false,
            x11_forwarding: false,
            preferred_kex: Vec::new(),
            preferred_ciphers: Vec::new(),
            preferred_macs: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SshDefaultsLayer {
    pub port: Option<u16>,
    pub connect_timeout_ms: Option<u64>,
    pub keepalive_interval_secs: Option<u64>,
    pub reconnect: Option<bool>,
    pub reconnect_backoff_ms: Option<u64>,
    pub agent_forwarding: Option<bool>,
    pub x11_forwarding: Option<bool>,
    pub preferred_kex: Option<Vec<String>>,
    pub preferred_ciphers: Option<Vec<String>>,
    pub preferred_macs: Option<Vec<String>>,
}

impl SshDefaults {
    fn apply(&mut self, layer: SshDefaultsLayer) {
        if let Some(v) = layer.port {
            self.port = v;
        }
        if let Some(v) = layer.connect_timeout_ms {
            self.connect_timeout_ms = v;
        }
        if let Some(v) = layer.keepalive_interval_secs {
            self.keepalive_interval_secs = v;
        }
        if let Some(v) = layer.reconnect {
            self.reconnect = v;
        }
        if let Some(v) = layer.reconnect_backoff_ms {
            self.reconnect_backoff_ms = v;
        }
        if let Some(v) = layer.agent_forwarding {
            self.agent_forwarding = v;
        }
        if let Some(v) = layer.x11_forwarding {
            self.x11_forwarding = v;
        }
        if let Some(v) = layer.preferred_kex {
            self.preferred_kex = v;
        }
        if let Some(v) = layer.preferred_ciphers {
            self.preferred_ciphers = v;
        }
        if let Some(v) = layer.preferred_macs {
            self.preferred_macs = v;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransferConfig {
    pub buffer_size: usize,
    pub overwrite_mode: String,
    pub preserve_permissions: bool,
    pub preserve_times: bool,
    pub verify_checksum: bool,
    pub resume: bool,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            buffer_size: 1024 * 256,
            overwrite_mode: "ask".to_string(),
            preserve_permissions: true,
            preserve_times: true,
            verify_checksum: true,
            resume: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct TransferConfigLayer {
    pub buffer_size: Option<usize>,
    pub overwrite_mode: Option<String>,
    pub preserve_permissions: Option<bool>,
    pub preserve_times: Option<bool>,
    pub verify_checksum: Option<bool>,
    pub resume: Option<bool>,
}

impl TransferConfig {
    fn apply(&mut self, layer: TransferConfigLayer) {
        if let Some(v) = layer.buffer_size {
            self.buffer_size = v;
        }
        if let Some(v) = layer.overwrite_mode {
            self.overwrite_mode = v;
        }
        if let Some(v) = layer.preserve_permissions {
            self.preserve_permissions = v;
        }
        if let Some(v) = layer.preserve_times {
            self.preserve_times = v;
        }
        if let Some(v) = layer.verify_checksum {
            self.verify_checksum = v;
        }
        if let Some(v) = layer.resume {
            self.resume = v;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
    pub json: bool,
    pub stdout: bool,
    pub file_max_size_mb: u64,
    pub file_max_count: usize,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            json: false,
            stdout: true,
            file_max_size_mb: 50,
            file_max_count: 10,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct LoggingConfigLayer {
    pub level: Option<String>,
    pub json: Option<bool>,
    pub stdout: Option<bool>,
    pub file_max_size_mb: Option<u64>,
    pub file_max_count: Option<usize>,
}

impl LoggingConfig {
    fn apply(&mut self, layer: LoggingConfigLayer) {
        if let Some(v) = layer.level {
            self.level = v;
        }
        if let Some(v) = layer.json {
            self.json = v;
        }
        if let Some(v) = layer.stdout {
            self.stdout = v;
        }
        if let Some(v) = layer.file_max_size_mb {
            self.file_max_size_mb = v;
        }
        if let Some(v) = layer.file_max_count {
            self.file_max_count = v;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeychainConfig {
    pub store_private_keys: bool,
    pub store_passphrases: bool,
    pub use_encrypted_file_fallback: bool,
}

impl Default for KeychainConfig {
    fn default() -> Self {
        Self {
            store_private_keys: false,
            store_passphrases: true,
            use_encrypted_file_fallback: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct KeychainConfigLayer {
    pub store_private_keys: Option<bool>,
    pub store_passphrases: Option<bool>,
    pub use_encrypted_file_fallback: Option<bool>,
}

impl KeychainConfig {
    fn apply(&mut self, layer: KeychainConfigLayer) {
        if let Some(v) = layer.store_private_keys {
            self.store_private_keys = v;
        }
        if let Some(v) = layer.store_passphrases {
            self.store_passphrases = v;
        }
        if let Some(v) = layer.use_encrypted_file_fallback {
            self.use_encrypted_file_fallback = v;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordingConfig {
    pub enabled: bool,
    pub directory: Option<PathBuf>,
    pub format: String,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: None,
            format: "asciinema".to_string(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RecordingConfigLayer {
    pub enabled: Option<bool>,
    pub directory: Option<PathBuf>,
    pub format: Option<String>,
}

impl RecordingConfig {
    fn apply(&mut self, layer: RecordingConfigLayer) {
        if let Some(v) = layer.enabled {
            self.enabled = v;
        }
        if layer.directory.is_some() {
            self.directory = layer.directory;
        }
        if let Some(v) = layer.format {
            self.format = v;
        }
    }
}

#[derive(Clone, Debug)]
pub struct ConfigManager {
    pub paths: AppPaths,
}

impl ConfigManager {
    pub fn new(paths: AppPaths) -> Self {
        Self { paths }
    }

    pub fn load(&self, cwd: Option<&Path>, overrides: Option<AppConfigLayer>) -> Result<AppConfig> {
        let mut cfg = AppConfig::default();

        if self.paths.config_file.exists() {
            let layer = Self::load_layer(&self.paths.config_file)?;
            layer.apply_to(&mut cfg);
        }

        if let Some(dir) = cwd {
            let project_path = AppPaths::project_config_path(dir);
            if project_path.exists() {
                let layer = Self::load_layer(&project_path)?;
                layer.apply_to(&mut cfg);
            }
        }

        if let Some(layer) = overrides {
            layer.apply_to(&mut cfg);
        }

        Ok(cfg)
    }

    pub fn load_layer(path: &Path) -> Result<AppConfigLayer> {
        let content = fs::read_to_string(path)?;
        let layer: AppConfigLayer = toml::from_str(&content)?;
        Ok(layer)
    }

    pub fn save_default(&self) -> Result<()> {
        if let Some(parent) = self.paths.config_file.parent() {
            fs::create_dir_all(parent)?;
        }
        let cfg = AppConfig::default();
        let content = toml::to_string_pretty(&cfg).map_err(|e| anyhow::anyhow!(e))?;
        fs::write(&self.paths.config_file, content)?;
        Ok(())
    }

    pub fn save_config(&self, cfg: &AppConfig) -> Result<()> {
        if let Some(parent) = self.paths.config_file.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(cfg).map_err(|e| anyhow::anyhow!(e))?;
        fs::write(&self.paths.config_file, content)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_layer_overrides() {
        let mut cfg = AppConfig::default();
        let layer = AppConfigLayer {
            logging: Some(LoggingConfigLayer {
                level: Some("debug".to_string()),
                json: Some(true),
                stdout: Some(false),
                file_max_size_mb: Some(10),
                file_max_count: Some(3),
            }),
            ssh: Some(SshDefaultsLayer {
                port: Some(2222),
                connect_timeout_ms: Some(1234),
                keepalive_interval_secs: Some(7),
                reconnect: Some(false),
                reconnect_backoff_ms: Some(10),
                agent_forwarding: Some(true),
                x11_forwarding: Some(true),
                preferred_kex: Some(vec!["kex".to_string()]),
                preferred_ciphers: Some(vec!["cipher".to_string()]),
                preferred_macs: Some(vec!["mac".to_string()]),
            }),
            ..Default::default()
        };
        layer.apply_to(&mut cfg);
        assert_eq!(cfg.logging.level, "debug");
        assert!(cfg.logging.json);
        assert!(!cfg.logging.stdout);
        assert_eq!(cfg.ssh.port, 2222);
        assert_eq!(cfg.ssh.connect_timeout_ms, 1234);
        assert_eq!(cfg.ssh.keepalive_interval_secs, 7);
        assert!(!cfg.ssh.reconnect);
    }
}
