use crate::error::CoreError;
use crate::events::{Event, EventBus};
use crate::session::SessionManager;
use catsolle_config::TransferConfig;
use catsolle_ssh::SftpClient;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{error, info};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub enum TransferEndpoint {
    Local { path: PathBuf },
    Remote { session_id: Uuid, path: String },
}

#[derive(Clone, Debug)]
pub struct TransferFile {
    pub source_path: String,
    pub dest_path: String,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Clone, Debug)]
pub struct TransferOptions {
    pub overwrite: OverwriteMode,
    pub preserve_permissions: bool,
    pub preserve_times: bool,
    pub verify_checksum: bool,
    pub resume: bool,
    pub buffer_size: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverwriteMode {
    Ask,
    Replace,
    Skip,
    IfNewer,
}

#[derive(Clone, Debug)]
pub enum TransferState {
    Queued,
    InProgress,
    Paused,
    Completed,
    Failed { error: String },
    Cancelled,
}

#[derive(Clone, Debug, Default)]
pub struct TransferProgress {
    pub bytes_transferred: u64,
    pub bytes_total: u64,
    pub files_completed: usize,
    pub files_total: usize,
    pub current_file: Option<String>,
    pub speed_bps: u64,
    pub eta_seconds: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct TransferJob {
    pub id: Uuid,
    pub source: TransferEndpoint,
    pub dest: TransferEndpoint,
    pub files: Vec<TransferFile>,
    pub options: TransferOptions,
    pub state: TransferState,
    pub progress: TransferProgress,
    pub created_at: DateTime<Utc>,
}

struct CopyContext<'a> {
    cfg: &'a TransferConfig,
    bus: &'a EventBus,
    started_at: Instant,
    last_emit: &'a mut Instant,
    last_bytes: &'a mut u64,
}

#[derive(Clone)]
pub struct TransferQueue {
    sender: mpsc::Sender<TransferJob>,
}

impl TransferQueue {
    pub fn new(session_manager: Arc<SessionManager>, bus: EventBus, cfg: TransferConfig) -> Self {
        let (tx, mut rx) = mpsc::channel::<TransferJob>(32);
        let options = cfg;
        tokio::spawn(async move {
            while let Some(mut job) = rx.recv().await {
                job.state = TransferState::InProgress;
                info!(job_id = %job.id, "transfer start");
                let result = process_job(&mut job, &session_manager, &bus, &options).await;
                job.state = match result {
                    Ok(_) => TransferState::Completed,
                    Err(err) => TransferState::Failed {
                        error: err.to_string(),
                    },
                };
                if let TransferState::Failed { ref error } = job.state {
                    error!(job_id = %job.id, error = %error, "transfer failed");
                }
                bus.send(Event::TransferProgress {
                    job_id: job.id,
                    progress: job.progress.clone(),
                });
            }
        });
        Self { sender: tx }
    }

    pub async fn enqueue(&self, job: TransferJob) -> Result<(), CoreError> {
        self.sender
            .send(job)
            .await
            .map_err(|e| CoreError::Invalid(e.to_string()))
    }
}

async fn process_job(
    job: &mut TransferJob,
    session_manager: &SessionManager,
    bus: &EventBus,
    cfg: &TransferConfig,
) -> Result<(), CoreError> {
    let started_at = Instant::now();
    let mut last_emit = Instant::now();
    let mut last_bytes = 0u64;

    let total_bytes: u64 = job.files.iter().map(|f| f.size).sum();
    job.progress.bytes_total = total_bytes;
    job.progress.files_total = job.files.len();
    update_progress(job, bus, started_at, &mut last_emit, &mut last_bytes, true);

    let files = job.files.clone();
    for file in files {
        job.progress.current_file = Some(file.source_path.clone());
        update_progress(job, bus, started_at, &mut last_emit, &mut last_bytes, true);
        match (&job.source, &job.dest) {
            (TransferEndpoint::Local { .. }, TransferEndpoint::Remote { session_id, .. }) => {
                let session = session_manager
                    .get_session(*session_id)
                    .ok_or_else(|| CoreError::Invalid("session not found".to_string()))?;
                let sftp = session
                    .session
                    .open_sftp()
                    .await
                    .map_err(|e| CoreError::Ssh(e.to_string()))?;
                {
                    let mut ctx = CopyContext {
                        cfg,
                        bus,
                        started_at,
                        last_emit: &mut last_emit,
                        last_bytes: &mut last_bytes,
                    };
                    copy_local_to_remote(job, &file, &sftp, &mut ctx).await?;
                }
            }
            (TransferEndpoint::Remote { session_id, .. }, TransferEndpoint::Local { .. }) => {
                let session = session_manager
                    .get_session(*session_id)
                    .ok_or_else(|| CoreError::Invalid("session not found".to_string()))?;
                let sftp = session
                    .session
                    .open_sftp()
                    .await
                    .map_err(|e| CoreError::Ssh(e.to_string()))?;
                {
                    let mut ctx = CopyContext {
                        cfg,
                        bus,
                        started_at,
                        last_emit: &mut last_emit,
                        last_bytes: &mut last_bytes,
                    };
                    copy_remote_to_local(job, &file, &sftp, &mut ctx).await?;
                }
            }
            (TransferEndpoint::Local { .. }, TransferEndpoint::Local { .. }) => {
                let mut ctx = CopyContext {
                    cfg,
                    bus,
                    started_at,
                    last_emit: &mut last_emit,
                    last_bytes: &mut last_bytes,
                };
                copy_local_to_local(job, &file, &mut ctx).await?;
            }
            _ => {
                return Err(CoreError::Invalid(
                    "remote to remote copy not supported".to_string(),
                ));
            }
        }
        job.progress.files_completed += 1;
        update_progress(job, bus, started_at, &mut last_emit, &mut last_bytes, true);
    }

    Ok(())
}

async fn copy_local_to_remote(
    job: &mut TransferJob,
    file: &TransferFile,
    sftp: &SftpClient,
    ctx: &mut CopyContext<'_>,
) -> Result<(), CoreError> {
    let src = PathBuf::from(&file.source_path);
    let dest = &file.dest_path;
    if file.is_dir {
        sftp.create_dir_all(dest)
            .await
            .map_err(|e| CoreError::Ssh(e.to_string()))?;
        return Ok(());
    }

    let mut local = tokio::fs::File::open(&src).await?;
    let mut remote = sftp
        .open_write(dest, job.options.overwrite == OverwriteMode::Replace)
        .await
        .map_err(|e| CoreError::Ssh(e.to_string()))?;

    let mut buf = vec![0u8; ctx.cfg.buffer_size];
    let mut hasher = Sha256::new();
    loop {
        let n = local.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        remote
            .write_all(&buf[..n])
            .await
            .map_err(|e| CoreError::Ssh(e.to_string()))?;
        if job.options.verify_checksum {
            hasher.update(&buf[..n]);
        }
        job.progress.bytes_transferred += n as u64;
        update_progress(
            job,
            ctx.bus,
            ctx.started_at,
            ctx.last_emit,
            ctx.last_bytes,
            false,
        );
    }

    if job.options.verify_checksum {
        let local_hash = hasher.finalize();
        let remote_hash = hash_remote(sftp, dest).await?;
        if local_hash.as_slice() != remote_hash.as_slice() {
            return Err(CoreError::Invalid("checksum mismatch".to_string()));
        }
    }

    Ok(())
}

async fn copy_remote_to_local(
    job: &mut TransferJob,
    file: &TransferFile,
    sftp: &SftpClient,
    ctx: &mut CopyContext<'_>,
) -> Result<(), CoreError> {
    let dest = PathBuf::from(&file.dest_path);
    if file.is_dir {
        tokio::fs::create_dir_all(&dest).await?;
        return Ok(());
    }

    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut remote = sftp
        .open_read(&file.source_path)
        .await
        .map_err(|e| CoreError::Ssh(e.to_string()))?;
    let mut local = tokio::fs::File::create(&dest).await?;
    let mut buf = vec![0u8; ctx.cfg.buffer_size];
    let mut hasher = Sha256::new();

    loop {
        let n = remote
            .read(&mut buf)
            .await
            .map_err(|e| CoreError::Ssh(e.to_string()))?;
        if n == 0 {
            break;
        }
        local.write_all(&buf[..n]).await?;
        if job.options.verify_checksum {
            hasher.update(&buf[..n]);
        }
        job.progress.bytes_transferred += n as u64;
        update_progress(
            job,
            ctx.bus,
            ctx.started_at,
            ctx.last_emit,
            ctx.last_bytes,
            false,
        );
    }

    if job.options.verify_checksum {
        let remote_hash = hasher.finalize();
        let local_hash = hash_local(&dest).await?;
        if remote_hash.as_slice() != local_hash.as_slice() {
            return Err(CoreError::Invalid("checksum mismatch".to_string()));
        }
    }

    Ok(())
}

async fn copy_local_to_local(
    job: &mut TransferJob,
    file: &TransferFile,
    ctx: &mut CopyContext<'_>,
) -> Result<(), CoreError> {
    let src = PathBuf::from(&file.source_path);
    let dest = PathBuf::from(&file.dest_path);
    if file.is_dir {
        tokio::fs::create_dir_all(&dest).await?;
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let size = tokio::fs::copy(&src, &dest).await?;
    job.progress.bytes_transferred += size;
    update_progress(
        job,
        ctx.bus,
        ctx.started_at,
        ctx.last_emit,
        ctx.last_bytes,
        true,
    );
    Ok(())
}

fn update_progress(
    job: &mut TransferJob,
    bus: &EventBus,
    started_at: Instant,
    last_emit: &mut Instant,
    last_bytes: &mut u64,
    force: bool,
) {
    let now = Instant::now();
    let elapsed = now.duration_since(started_at);
    if elapsed.as_secs_f64() > 0.0 {
        let speed = (job.progress.bytes_transferred as f64 / elapsed.as_secs_f64()) as u64;
        job.progress.speed_bps = speed;
        if job.progress.bytes_total > 0
            && job.progress.bytes_transferred <= job.progress.bytes_total
            && speed > 0
        {
            let remaining = job.progress.bytes_total - job.progress.bytes_transferred;
            job.progress.eta_seconds = Some(remaining / speed);
        } else {
            job.progress.eta_seconds = None;
        }
    } else {
        job.progress.speed_bps = 0;
        job.progress.eta_seconds = None;
    }

    let bytes_since = job.progress.bytes_transferred.saturating_sub(*last_bytes);
    let should_emit = force
        || bytes_since >= 256 * 1024
        || now.duration_since(*last_emit) >= Duration::from_millis(250);
    if should_emit {
        bus.send(Event::TransferProgress {
            job_id: job.id,
            progress: job.progress.clone(),
        });
        *last_emit = now;
        *last_bytes = job.progress.bytes_transferred;
    }
}

async fn hash_remote(sftp: &SftpClient, path: &str) -> Result<Vec<u8>, CoreError> {
    let mut remote = sftp
        .open_read(path)
        .await
        .map_err(|e| CoreError::Ssh(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 128];
    loop {
        let n = remote
            .read(&mut buf)
            .await
            .map_err(|e| CoreError::Ssh(e.to_string()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_vec())
}

async fn hash_local(path: &Path) -> Result<Vec<u8>, CoreError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 128];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_vec())
}
