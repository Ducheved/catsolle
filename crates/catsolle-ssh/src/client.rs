use crate::config::{
    AuthMethod, HostKeyPolicy, JumpHost, KeyboardInteractiveHandler, ProxyConfig, SshConnectConfig,
};
use crate::known_hosts::{KnownHostResult, KnownHosts};
use crate::proxy::connect_via_proxy;
use crate::sftp::SftpClient;
use anyhow::Result;
use russh::client::{Config as ClientConfig, Handle};
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::Algorithm;
use russh::keys::{load_openssh_certificate, load_secret_key};
use russh::{client, ChannelMsg, ChannelWriteHalf};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};
use tracing::warn;

trait AsyncStream: AsyncRead + AsyncWrite {}
impl<T: AsyncRead + AsyncWrite + ?Sized> AsyncStream for T {}
type BoxedStream = Box<dyn AsyncStream + Unpin + Send>;

#[derive(Clone)]
pub struct SshClient;

#[derive(Clone)]
pub struct SshSession {
    inner: Arc<Mutex<SessionInner>>,
}

struct SessionInner {
    handle: Handle<ClientHandler>,
    jump_handles: Vec<Handle<ClientHandler>>,
    config: SshConnectConfig,
}

pub struct SshShell {
    writer: ChannelWriteHalf<russh::client::Msg>,
    output: mpsc::Receiver<Vec<u8>>,
    exit_status: Option<tokio::sync::oneshot::Receiver<Option<u32>>>,
}

impl SshClient {
    pub async fn connect(
        cfg: SshConnectConfig,
        keyboard: Option<Arc<dyn KeyboardInteractiveHandler>>,
    ) -> Result<SshSession> {
        let base_config = Arc::new(build_client_config(&cfg));
        let known_hosts = if let Some(path) = cfg.known_hosts_path.clone() {
            Some(Arc::new(Mutex::new(KnownHosts::load(path)?)))
        } else {
            None
        };

        let mut chain = cfg.jump_hosts.clone();
        chain.push(JumpHost {
            host: cfg.host.clone(),
            port: cfg.port,
            username: cfg.username.clone(),
            auth_method: cfg.auth_method.clone(),
        });

        let mut jump_handles: Vec<Handle<ClientHandler>> = Vec::new();

        for (idx, hop) in chain.iter().enumerate() {
            let handler = ClientHandler {
                host: hop.host.clone(),
                port: hop.port,
                policy: cfg.host_key_policy.clone(),
                known_hosts: known_hosts.clone(),
            };

            let mut handle = if idx == 0 {
                let sock =
                    connect_socket(&cfg.proxy, &hop.host, hop.port, cfg.connect_timeout_ms).await?;
                client::connect_stream(base_config.clone(), sock, handler).await?
            } else {
                let prev = jump_handles
                    .last_mut()
                    .ok_or_else(|| anyhow::anyhow!("jump chain empty"))?;
                let channel = prev
                    .channel_open_direct_tcpip(&hop.host, hop.port as u32, "127.0.0.1", 0)
                    .await?;
                client::connect_stream(base_config.clone(), channel.into_stream(), handler).await?
            };

            authenticate(
                &mut handle,
                &hop.username,
                &hop.auth_method,
                keyboard.clone(),
            )
            .await?;
            jump_handles.push(handle);
        }

        let handle = jump_handles
            .pop()
            .ok_or_else(|| anyhow::anyhow!("ssh connection not established"))?;

        let session = SshSession {
            inner: Arc::new(Mutex::new(SessionInner {
                handle,
                jump_handles,
                config: cfg,
            })),
        };
        Ok(session)
    }
}

impl SshSession {
    pub async fn open_shell(&self) -> Result<SshShell> {
        let inner = self.inner.lock().await;
        let channel = inner.handle.channel_open_session().await?;
        if inner.config.request_pty {
            channel
                .request_pty(
                    true,
                    &inner.config.term,
                    inner.config.term_width,
                    inner.config.term_height,
                    0,
                    0,
                    &[],
                )
                .await?;
        }
        for (k, v) in &inner.config.env {
            channel.set_env(true, k, v).await?;
        }
        channel.request_shell(true).await?;

        let (mut reader, writer) = channel.split();
        let (tx, rx) = mpsc::channel(1024);
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let mut exit_status: Option<u32> = None;
            while let Some(msg) = reader.wait().await {
                match msg {
                    ChannelMsg::Data { data } => {
                        let _ = tx.send(data.to_vec()).await;
                    }
                    ChannelMsg::ExtendedData { data, .. } => {
                        let _ = tx.send(data.to_vec()).await;
                    }
                    ChannelMsg::ExitStatus {
                        exit_status: status,
                    } => {
                        exit_status = Some(status);
                    }
                    ChannelMsg::Eof => break,
                    _ => {}
                }
            }
            let _ = exit_tx.send(exit_status);
        });

        Ok(SshShell {
            writer,
            output: rx,
            exit_status: Some(exit_rx),
        })
    }

    pub async fn open_sftp(&self) -> Result<SftpClient> {
        let inner = self.inner.lock().await;
        let channel = inner.handle.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;
        let stream = channel.into_stream();
        SftpClient::new(stream).await
    }

    pub async fn exec(&self, command: &str) -> Result<(i32, Vec<u8>)> {
        let inner = self.inner.lock().await;
        let channel = inner.handle.channel_open_session().await?;
        channel.exec(true, command).await?;
        let (mut reader, _) = channel.split();
        let mut output = Vec::new();
        let mut status = 0;
        while let Some(msg) = reader.wait().await {
            match msg {
                ChannelMsg::Data { data } => output.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, .. } => output.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => status = exit_status as i32,
                ChannelMsg::Eof => break,
                _ => {}
            }
        }
        Ok((status, output))
    }

    pub async fn send_startup_commands(&self) -> Result<()> {
        let cmds = {
            let inner = self.inner.lock().await;
            inner.config.startup_commands.clone()
        };
        for cmd in cmds {
            let _ = self.exec(&cmd).await?;
        }
        Ok(())
    }

    pub async fn is_closed(&self) -> bool {
        let inner = self.inner.lock().await;
        if inner.handle.is_closed() {
            return true;
        }
        inner.jump_handles.iter().any(|handle| handle.is_closed())
    }
}

impl SshShell {
    pub async fn write(&mut self, data: &[u8]) -> Result<()> {
        let mut writer = self.writer.make_writer();
        writer.write_all(data).await?;
        Ok(())
    }

    pub async fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        self.writer.window_change(width, height, 0, 0).await?;
        Ok(())
    }

    pub async fn read(&mut self) -> Option<Vec<u8>> {
        self.output.recv().await
    }

    pub async fn wait_exit(&mut self) -> Option<u32> {
        if let Some(rx) = self.exit_status.take() {
            rx.await.ok().flatten()
        } else {
            None
        }
    }
}

#[derive(Clone)]
struct ClientHandler {
    host: String,
    port: u16,
    policy: HostKeyPolicy,
    known_hosts: Option<Arc<Mutex<KnownHosts>>>,
}

impl client::Handler for ClientHandler {
    type Error = anyhow::Error;

    fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send {
        let host = self.host.clone();
        let port = self.port;
        let policy = self.policy.clone();
        let known_hosts = self.known_hosts.clone();
        async move {
            match policy {
                HostKeyPolicy::InsecureAcceptAny => {
                    warn!("accepting any host key for {}:{}", host, port);
                    Ok(true)
                }
                HostKeyPolicy::Strict | HostKeyPolicy::AcceptNew => {
                    let Some(kh) = known_hosts else {
                        warn!("known_hosts not configured for {}:{}", host, port);
                        return Ok(false);
                    };
                    let mut kh = kh.lock().await;
                    match kh.check(&host, port, server_public_key) {
                        KnownHostResult::Match => Ok(true),
                        KnownHostResult::NotFound => {
                            if let HostKeyPolicy::AcceptNew = policy {
                                kh.add(&host, port, server_public_key, "catsolle")?;
                                Ok(true)
                            } else {
                                Ok(false)
                            }
                        }
                        KnownHostResult::Mismatch | KnownHostResult::Revoked => Ok(false),
                    }
                }
            }
        }
    }
}

fn build_client_config(cfg: &SshConnectConfig) -> ClientConfig {
    ClientConfig {
        keepalive_interval: Some(Duration::from_secs(cfg.keepalive_interval_secs)),
        keepalive_max: 3,
        ..Default::default()
    }
}

async fn connect_socket(
    proxy: &Option<ProxyConfig>,
    host: &str,
    port: u16,
    timeout_ms: u64,
) -> Result<BoxedStream> {
    let fut = async {
        if let Some(proxy) = proxy {
            let stream = connect_via_proxy(proxy, host, port).await?;
            Ok::<BoxedStream, anyhow::Error>(Box::new(stream))
        } else {
            let stream = tokio::net::TcpStream::connect((host, port)).await?;
            Ok::<BoxedStream, anyhow::Error>(Box::new(stream))
        }
    };
    let stream = tokio::time::timeout(Duration::from_millis(timeout_ms), fut).await??;
    Ok(stream)
}

async fn authenticate(
    handle: &mut Handle<ClientHandler>,
    username: &str,
    auth: &AuthMethod,
    keyboard: Option<Arc<dyn KeyboardInteractiveHandler>>,
) -> Result<()> {
    let user = username.to_string();
    match auth {
        AuthMethod::Password { password } => {
            let res = handle
                .authenticate_password(user.clone(), password.to_string())
                .await?;
            ensure_auth(res)
        }
        AuthMethod::Key {
            private_key_path,
            passphrase,
        } => {
            let key = load_private_key(private_key_path, passphrase.as_ref().map(|v| v.as_str()))?;
            let hash = if matches!(key.algorithm(), Algorithm::Rsa { .. }) {
                handle.best_supported_rsa_hash().await?.flatten()
            } else {
                None
            };
            let key_with_hash = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
            let res = handle
                .authenticate_publickey(user.clone(), key_with_hash)
                .await?;
            ensure_auth(res)
        }
        AuthMethod::Agent => {
            if authenticate_with_agent(handle, &user).await? {
                Ok(())
            } else {
                Err(anyhow::anyhow!("agent authentication failed"))
            }
        }
        AuthMethod::KeyboardInteractive => {
            let mut response = handle
                .authenticate_keyboard_interactive_start(user.clone(), None)
                .await?;
            loop {
                match response {
                    russh::client::KeyboardInteractiveAuthResponse::Success => return Ok(()),
                    russh::client::KeyboardInteractiveAuthResponse::Failure { .. } => {
                        return Err(anyhow::anyhow!("keyboard-interactive failed"));
                    }
                    russh::client::KeyboardInteractiveAuthResponse::InfoRequest {
                        name: _,
                        instructions: _,
                        prompts,
                    } => {
                        let prompts = prompts
                            .into_iter()
                            .map(|p| crate::config::KeyboardPrompt {
                                prompt: p.prompt,
                                echo: p.echo,
                            })
                            .collect::<Vec<_>>();
                        let Some(handler) = keyboard.clone() else {
                            return Err(anyhow::anyhow!("keyboard-interactive handler missing"));
                        };
                        let answers = handler.respond(prompts).await?;
                        response = handle
                            .authenticate_keyboard_interactive_respond(answers)
                            .await?;
                    }
                }
            }
        }
        AuthMethod::Certificate {
            cert_path,
            private_key_path,
            passphrase,
        } => {
            let key = load_private_key(private_key_path, passphrase.as_ref().map(|v| v.as_str()))?;
            let cert = load_openssh_certificate(cert_path)?;
            let res = handle
                .authenticate_openssh_cert(user.clone(), Arc::new(key), cert)
                .await?;
            ensure_auth(res)
        }
    }
}

fn ensure_auth(res: russh::client::AuthResult) -> Result<()> {
    match res {
        russh::client::AuthResult::Success => Ok(()),
        russh::client::AuthResult::Failure { .. } => Err(anyhow::anyhow!("authentication failed")),
    }
}

fn load_private_key(path: &PathBuf, passphrase: Option<&str>) -> Result<russh::keys::PrivateKey> {
    load_secret_key(path, passphrase).map_err(|e| anyhow::anyhow!(e))
}

async fn authenticate_with_agent(handle: &mut Handle<ClientHandler>, user: &str) -> Result<bool> {
    #[cfg(unix)]
    let mut client = russh::keys::agent::client::AgentClient::connect_env().await?;

    #[cfg(windows)]
    let mut client = {
        use tokio::net::windows::named_pipe::ClientOptions;
        let sock = std::env::var("SSH_AUTH_SOCK")
            .unwrap_or_else(|_| "\\\\.\\pipe\\openssh-ssh-agent".to_string());
        let stream = ClientOptions::new().open(sock)?;
        russh::keys::agent::client::AgentClient::connect(stream)
    };

    let keys = client.request_identities().await?;
    for key in keys {
        let hash = if matches!(key.algorithm(), Algorithm::Rsa { .. }) {
            handle.best_supported_rsa_hash().await?.flatten()
        } else {
            None
        };
        let res = handle
            .authenticate_publickey_with(user.to_string(), key, hash, &mut client)
            .await?;
        if matches!(res, russh::client::AuthResult::Success) {
            return Ok(true);
        }
    }
    Ok(false)
}
