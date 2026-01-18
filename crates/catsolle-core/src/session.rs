use crate::connection::{
    AuthMethod, Connection, ConnectionId, ConnectionStore, JumpHost, ProxyType,
};
use crate::error::CoreError;
use crate::events::{Event, EventBus};
use catsolle_config::AppConfig;
use catsolle_keychain::KeychainManager;
use catsolle_ssh::config::{HostKeyPolicy, KeyboardInteractiveHandler};
use catsolle_ssh::{
    AuthMethod as SshAuthMethod, JumpHost as SshJumpHost, ProxyConfig as SshProxyConfig,
    ProxyType as SshProxyType, SshClient, SshConnectConfig, SshSession,
};
use chrono::Utc;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::task;
use tracing::{error, info};
use uuid::Uuid;
use zeroize::Zeroizing;
#[derive(Clone)]
pub struct SessionHandle {
    pub id: Uuid,
    pub connection_id: ConnectionId,
    pub session: SshSession,
    pub state: SessionState,
}

#[derive(Clone, Debug)]
pub enum SessionState {
    Connecting,
    Connected,
    Disconnected,
    Failed(String),
}

#[derive(Clone)]
pub struct SessionManager {
    store: ConnectionStore,
    keychain: KeychainManager,
    config: AppConfig,
    bus: EventBus,
    sessions: Arc<Mutex<HashMap<Uuid, SessionHandle>>>,
}

impl SessionManager {
    pub fn new(
        store: ConnectionStore,
        keychain: KeychainManager,
        config: AppConfig,
        bus: EventBus,
    ) -> Self {
        Self {
            store,
            keychain,
            config,
            bus,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn connect_by_id(
        &self,
        id: ConnectionId,
        keyboard: Option<Arc<dyn KeyboardInteractiveHandler>>,
        master_password: Option<String>,
    ) -> Result<Uuid, CoreError> {
        let conn = self.get_connection(id).await?;
        self.connect(conn, keyboard, master_password).await
    }

    pub async fn connect(
        &self,
        conn: Connection,
        keyboard: Option<Arc<dyn KeyboardInteractiveHandler>>,
        master_password: Option<String>,
    ) -> Result<Uuid, CoreError> {
        let cfg = self.build_ssh_config(&conn, master_password.as_deref())?;
        self.connect_with_config(conn, cfg, keyboard).await
    }

    pub async fn connect_with_password(
        &self,
        mut conn: Connection,
        password: Zeroizing<String>,
        save: bool,
        master_password: Option<&str>,
    ) -> Result<Uuid, CoreError> {
        if save {
            self.set_connection_password(&mut conn, &password, master_password)?;
        }
        let auth = SshAuthMethod::Password { password };
        let cfg = self.build_ssh_config_with_auth(&conn, auth, master_password)?;
        self.connect_with_config(conn, cfg, None).await
    }

    pub fn set_connection_password(
        &self,
        conn: &mut Connection,
        password: &Zeroizing<String>,
        master_password: Option<&str>,
    ) -> Result<(), CoreError> {
        let secret_ref = match &conn.auth_method {
            AuthMethod::Password { secret_ref } => secret_ref.clone(),
            _ => format!("conn:{}:password", conn.id),
        };
        self.keychain
            .store_secret(&secret_ref, password, master_password)
            .map_err(|e| CoreError::Invalid(e.to_string()))?;
        conn.auth_method = AuthMethod::Password { secret_ref };
        conn.updated_at = Utc::now();
        self.store
            .update_connection(conn)
            .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    async fn connect_with_config(
        &self,
        conn: Connection,
        cfg: SshConnectConfig,
        keyboard: Option<Arc<dyn KeyboardInteractiveHandler>>,
    ) -> Result<Uuid, CoreError> {
        let session_id = Uuid::new_v4();
        info!(connection_id = %conn.id, session_id = %session_id, "session connect start");
        self.bus.send(Event::SessionStateChanged {
            session_id,
            state: SessionState::Connecting,
        });

        let session = match SshClient::connect(cfg, keyboard).await {
            Ok(session) => session,
            Err(err) => {
                error!(connection_id = %conn.id, error = %err, "session connect failed");
                return Err(CoreError::Ssh(err.to_string()));
            }
        };
        let _ = session.send_startup_commands().await;

        let handle = SessionHandle {
            id: session_id,
            connection_id: conn.id,
            session: session.clone(),
            state: SessionState::Connected,
        };

        {
            let mut map = self.sessions.lock();
            map.insert(session_id, handle.clone());
        }

        self.bus.send(Event::SessionStateChanged {
            session_id,
            state: SessionState::Connected,
        });
        info!(connection_id = %conn.id, session_id = %session_id, "session connected");

        let store = self.store.clone();
        let conn_id = conn.id;
        task::spawn_blocking(move || {
            if let Ok(mut conn) = store.get_connection(conn_id) {
                conn.last_connected_at = Some(Utc::now());
                conn.updated_at = Utc::now();
                let _ = store.update_connection(&conn);
            }
        });

        Ok(session_id)
    }

    pub fn get_session(&self, id: Uuid) -> Option<SessionHandle> {
        self.sessions.lock().get(&id).cloned()
    }

    pub fn list_sessions(&self) -> Vec<SessionHandle> {
        self.sessions.lock().values().cloned().collect()
    }

    pub async fn disconnect(&self, id: Uuid) {
        let mut map = self.sessions.lock();
        if let Some(mut handle) = map.remove(&id) {
            handle.state = SessionState::Disconnected;
            self.bus.send(Event::SessionStateChanged {
                session_id: id,
                state: SessionState::Disconnected,
            });
        }
    }

    async fn get_connection(&self, id: ConnectionId) -> Result<Connection, CoreError> {
        let store = self.store.clone();
        task::spawn_blocking(move || store.get_connection(id))
            .await
            .map_err(|e| CoreError::Database(e.to_string()))?
    }

    fn build_ssh_config(
        &self,
        conn: &Connection,
        master: Option<&str>,
    ) -> Result<SshConnectConfig, CoreError> {
        let auth = self.map_auth(&conn.auth_method, master)?;
        self.build_ssh_config_with_auth(conn, auth, master)
    }

    fn build_ssh_config_with_auth(
        &self,
        conn: &Connection,
        auth: SshAuthMethod,
        master: Option<&str>,
    ) -> Result<SshConnectConfig, CoreError> {
        let jump_hosts = conn
            .jump_hosts
            .iter()
            .map(|j| self.map_jump_host(j, master))
            .collect::<Result<Vec<_>, CoreError>>()?;
        let proxy = match &conn.proxy {
            Some(proxy) => {
                let password = if let Some(ref id) = proxy.password_ref {
                    self.keychain
                        .get_secret(id, master)
                        .map_err(|e| CoreError::Invalid(e.to_string()))?
                } else {
                    None
                };
                Some(SshProxyConfig {
                    proxy_type: match proxy.proxy_type {
                        ProxyType::Socks5 => SshProxyType::Socks5,
                        ProxyType::HttpConnect => SshProxyType::HttpConnect,
                    },
                    host: proxy.host.clone(),
                    port: proxy.port,
                    username: proxy.username.clone(),
                    password,
                })
            }
            None => None,
        };

        let known_hosts = default_known_hosts_path();

        Ok(SshConnectConfig {
            host: conn.host.clone(),
            port: conn.port,
            username: conn.username.clone(),
            auth_method: auth,
            jump_hosts,
            proxy,
            host_key_policy: HostKeyPolicy::AcceptNew,
            known_hosts_path: Some(known_hosts),
            keepalive_interval_secs: self.config.ssh.keepalive_interval_secs,
            connect_timeout_ms: self.config.ssh.connect_timeout_ms,
            request_pty: true,
            term: "xterm-256color".to_string(),
            term_width: 120,
            term_height: 40,
            env: conn
                .env_vars
                .iter()
                .map(|v| (v.key.clone(), v.value.clone()))
                .collect(),
            startup_commands: conn.startup_commands.clone(),
            agent_forwarding: self.config.ssh.agent_forwarding,
            x11_forwarding: self.config.ssh.x11_forwarding,
        })
    }

    fn map_auth(
        &self,
        auth: &AuthMethod,
        master: Option<&str>,
    ) -> Result<SshAuthMethod, CoreError> {
        match auth {
            AuthMethod::Password { secret_ref } => {
                let secret = self
                    .keychain
                    .get_secret(secret_ref, master)
                    .map_err(|e| CoreError::Invalid(e.to_string()))?
                    .ok_or_else(|| CoreError::Invalid("missing password".to_string()))?;
                Ok(SshAuthMethod::Password { password: secret })
            }
            AuthMethod::Key {
                private_key_path,
                passphrase_ref,
            } => {
                let passphrase = if let Some(ref id) = passphrase_ref {
                    self.keychain
                        .get_secret(id, master)
                        .map_err(|e| CoreError::Invalid(e.to_string()))?
                } else {
                    None
                };
                Ok(SshAuthMethod::Key {
                    private_key_path: private_key_path.clone(),
                    passphrase,
                })
            }
            AuthMethod::Agent => Ok(SshAuthMethod::Agent),
            AuthMethod::KeyboardInteractive => Ok(SshAuthMethod::KeyboardInteractive),
            AuthMethod::Certificate {
                cert_path,
                private_key_path,
                passphrase_ref,
            } => {
                let passphrase = if let Some(ref id) = passphrase_ref {
                    self.keychain
                        .get_secret(id, master)
                        .map_err(|e| CoreError::Invalid(e.to_string()))?
                } else {
                    None
                };
                Ok(SshAuthMethod::Certificate {
                    cert_path: cert_path.clone(),
                    private_key_path: private_key_path.clone(),
                    passphrase,
                })
            }
        }
    }

    fn map_jump_host(
        &self,
        jump: &JumpHost,
        master: Option<&str>,
    ) -> Result<SshJumpHost, CoreError> {
        Ok(SshJumpHost {
            host: jump.host.clone(),
            port: jump.port,
            username: jump.username.clone(),
            auth_method: self.map_auth(&jump.auth_method, master)?,
        })
    }
}

fn default_known_hosts_path() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".ssh").join("known_hosts");
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return PathBuf::from(profile).join(".ssh").join("known_hosts");
    }
    PathBuf::from("known_hosts")
}
