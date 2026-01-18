use anyhow::Result;
use catsolle_cli::{Cli, Command, KeyCommand};
use catsolle_config::{AppPaths, ConfigManager, I18n};
use catsolle_core::{ConnectionStore, EventBus, SessionManager, TransferQueue};
use catsolle_keychain::{AgentManager, KeyAlgorithm, KeyManager, KeychainManager};
use catsolle_ssh::{AuthMethod, HostKeyPolicy, SshClient, SshConnectConfig};
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = AppPaths::new()?;
    let config_manager = ConfigManager::new(paths.clone());
    let config = config_manager.load(std::env::current_dir().ok().as_deref(), None)?;
    let interactive = matches!(cli.command, None | Some(Command::Connect { .. }));
    let _log_guard = init_logging(&config, &paths, config.logging.stdout && !interactive)?;
    let i18n = I18n::new(
        &config.locale.default_language,
        &config.locale.preferred_languages,
    )?;

    let store = ConnectionStore::new(paths.db_file.clone());
    store.init()?;

    let keychain = KeychainManager::new(
        "catsolle",
        paths.data_dir.join("secrets.enc"),
        config.keychain.use_encrypted_file_fallback,
    );

    let bus = EventBus::new(256);
    let session_manager = Arc::new(SessionManager::new(
        store.clone(),
        keychain,
        config.clone(),
        bus.clone(),
    ));
    let transfer_queue = TransferQueue::new(
        session_manager.clone(),
        bus.clone(),
        config.transfer.clone(),
    );

    match cli.command {
        Some(Command::Config { init }) => {
            if init {
                config_manager.save_default()?;
                println!("config initialized at {}", paths.config_file.display());
            }
        }
        Some(Command::Keys { command }) => {
            handle_keys(command).await?;
        }
        Some(Command::Connect { target, quick: _ }) => {
            connect_quick(&target).await?;
        }
        None => {
            catsolle_tui::run(
                store,
                session_manager,
                transfer_queue,
                bus,
                config_manager,
                config,
                i18n,
            )
            .await?;
        }
    }

    Ok(())
}

fn init_logging(
    config: &catsolle_config::AppConfig,
    paths: &AppPaths,
    enable_stdout: bool,
) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    std::fs::create_dir_all(&paths.log_dir)?;
    let file_appender = tracing_appender::rolling::daily(&paths.log_dir, "catsolle.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.logging.level));

    let file_layer = if config.logging.json {
        tracing_subscriber::fmt::layer()
            .json()
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_target(true)
            .boxed()
    } else {
        tracing_subscriber::fmt::layer()
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_target(true)
            .boxed()
    };

    let subscriber = tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer);

    if enable_stdout {
        let stdout_layer = if config.logging.json {
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(std::io::stdout)
                .boxed()
        } else {
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stdout)
                .boxed()
        };
        tracing::subscriber::set_global_default(subscriber.with(stdout_layer))?;
    } else {
        tracing::subscriber::set_global_default(subscriber)?;
    }

    Ok(guard)
}

async fn handle_keys(command: KeyCommand) -> Result<()> {
    match command {
        KeyCommand::Generate {
            name,
            algorithm,
            bits,
            curve,
        } => {
            let base_dir = default_ssh_dir();
            let manager = KeyManager::new(base_dir);
            let alg = match algorithm.as_str() {
                "ed25519" => KeyAlgorithm::Ed25519,
                "rsa" => KeyAlgorithm::Rsa {
                    bits: bits.unwrap_or(4096),
                },
                "ecdsa" => {
                    let curve = match curve.as_deref() {
                        Some("p256") => ssh_key::EcdsaCurve::NistP256,
                        Some("p384") => ssh_key::EcdsaCurve::NistP384,
                        Some("p521") => ssh_key::EcdsaCurve::NistP521,
                        _ => ssh_key::EcdsaCurve::NistP256,
                    };
                    KeyAlgorithm::Ecdsa { curve }
                }
                _ => KeyAlgorithm::Ed25519,
            };
            let key = manager.generate(&name, alg, None, Some(name.clone()))?;
            println!("private key: {}", key.private_key_path.display());
            println!("public key: {}", key.public_key_path.display());
            println!("fingerprint: {}", key.fingerprint);
        }
        KeyCommand::List => {
            let dir = default_ssh_dir();
            let entries = std::fs::read_dir(dir)?;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("pub") {
                    println!("{}", path.display());
                }
            }
        }
        KeyCommand::AddAgent { path } => {
            let mut agent = AgentManager::connect().await?;
            agent
                .add_identity_from_file(PathBuf::from(path).as_path(), None)
                .await?;
            println!("added to agent");
        }
    }
    Ok(())
}

async fn connect_quick(target: &str) -> Result<()> {
    let (user, host, port) = parse_target(target)?;
    let cfg = SshConnectConfig {
        host,
        port,
        username: user,
        auth_method: AuthMethod::Agent,
        jump_hosts: Vec::new(),
        proxy: None,
        host_key_policy: HostKeyPolicy::AcceptNew,
        known_hosts_path: Some(default_known_hosts_path()),
        keepalive_interval_secs: 10,
        connect_timeout_ms: 15000,
        request_pty: true,
        term: "xterm-256color".to_string(),
        term_width: 120,
        term_height: 40,
        env: Vec::new(),
        startup_commands: Vec::new(),
        agent_forwarding: false,
        x11_forwarding: false,
    };

    let session = SshClient::connect(cfg, None).await?;
    let mut shell = session.open_shell().await?;

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    let mut read_buf = [0u8; 4096];

    loop {
        tokio::select! {
            n = stdin.read(&mut read_buf) => {
                let n = n?;
                if n == 0 { break; }
                shell.write(&read_buf[..n]).await?;
            }
            out = shell.read() => {
                if let Some(data) = out {
                    stdout.write_all(&data).await?;
                    stdout.flush().await?;
                } else {
                    break;
                }
            }
        }
    }

    Ok(())
}

fn parse_target(target: &str) -> Result<(String, String, u16)> {
    let mut user_host = target;
    let mut user = whoami::username();
    let mut port = 22;

    if let Some(at) = target.find('@') {
        user = target[..at].to_string();
        user_host = &target[at + 1..];
    }
    let host = if let Some(colon) = user_host.rfind(':') {
        if let Ok(p) = user_host[colon + 1..].parse::<u16>() {
            port = p;
            &user_host[..colon]
        } else {
            user_host
        }
    } else {
        user_host
    };
    Ok((user, host.to_string(), port))
}

fn default_ssh_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".ssh");
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return PathBuf::from(profile).join(".ssh");
    }
    PathBuf::from(".ssh")
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
