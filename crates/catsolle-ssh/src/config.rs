use async_trait::async_trait;
use std::path::PathBuf;
use zeroize::Zeroizing;

#[derive(Clone, Debug)]
pub struct SshConnectConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_method: AuthMethod,
    pub jump_hosts: Vec<JumpHost>,
    pub proxy: Option<ProxyConfig>,
    pub host_key_policy: HostKeyPolicy,
    pub known_hosts_path: Option<PathBuf>,
    pub keepalive_interval_secs: u64,
    pub connect_timeout_ms: u64,
    pub request_pty: bool,
    pub term: String,
    pub term_width: u32,
    pub term_height: u32,
    pub env: Vec<(String, String)>,
    pub startup_commands: Vec<String>,
    pub agent_forwarding: bool,
    pub x11_forwarding: bool,
}

#[derive(Clone, Debug)]
pub enum AuthMethod {
    Password {
        password: Zeroizing<String>,
    },
    Key {
        private_key_path: PathBuf,
        passphrase: Option<Zeroizing<String>>,
    },
    Agent,
    KeyboardInteractive,
    Certificate {
        cert_path: PathBuf,
        private_key_path: PathBuf,
        passphrase: Option<Zeroizing<String>>,
    },
}

#[derive(Clone, Debug)]
pub struct JumpHost {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_method: AuthMethod,
}

#[derive(Clone, Debug)]
pub enum HostKeyPolicy {
    Strict,
    AcceptNew,
    InsecureAcceptAny,
}

#[derive(Clone, Debug)]
pub struct ProxyConfig {
    pub proxy_type: ProxyType,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<Zeroizing<String>>,
}

#[derive(Clone, Debug)]
pub enum ProxyType {
    Socks5,
    HttpConnect,
}

#[derive(Clone, Debug)]
pub struct KeyboardPrompt {
    pub prompt: String,
    pub echo: bool,
}

#[async_trait]
pub trait KeyboardInteractiveHandler: Send + Sync {
    async fn respond(&self, prompts: Vec<KeyboardPrompt>) -> anyhow::Result<Vec<String>>;
}
