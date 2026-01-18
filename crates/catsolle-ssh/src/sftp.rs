use anyhow::Result;
use russh_sftp::client::fs::{File, Metadata};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::io::{AsyncRead, AsyncWrite};

#[derive(Debug, Clone)]
pub struct SftpEntry {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    pub modified: Option<u64>,
    pub permissions: Option<u32>,
}

pub struct SftpClient {
    inner: SftpSession,
}

impl SftpClient {
    pub async fn new<S>(stream: S) -> Result<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let inner = SftpSession::new(stream).await?;
        Ok(Self { inner })
    }

    pub async fn read_dir(&self, path: &str) -> Result<Vec<SftpEntry>> {
        let mut entries = Vec::new();
        let rd = self.inner.read_dir(path).await?;
        for entry in rd {
            let name = entry.file_name();
            let meta = entry.metadata();
            entries.push(Self::entry_from_meta(path, name, &meta));
        }
        Ok(entries)
    }

    pub async fn metadata(&self, path: &str) -> Result<Metadata> {
        Ok(self.inner.metadata(path).await?)
    }

    pub async fn open_read(&self, path: &str) -> Result<File> {
        Ok(self.inner.open(path).await?)
    }

    pub async fn open_write(&self, path: &str, truncate: bool) -> Result<File> {
        let mut flags = OpenFlags::WRITE | OpenFlags::CREATE;
        if truncate {
            flags |= OpenFlags::TRUNCATE;
        }
        Ok(self.inner.open_with_flags(path, flags).await?)
    }

    pub async fn create_dir_all(&self, path: &str) -> Result<()> {
        let mut current = if path.starts_with('/') {
            "/".to_string()
        } else {
            String::new()
        };
        for part in path.split('/') {
            if part.is_empty() {
                continue;
            }
            if !current.ends_with('/') && !current.is_empty() {
                current.push('/');
            }
            current.push_str(part);
            let _ = self.inner.create_dir(current.clone()).await;
        }
        Ok(())
    }

    pub async fn remove_file(&self, path: &str) -> Result<()> {
        self.inner.remove_file(path).await?;
        Ok(())
    }

    pub async fn remove_dir(&self, path: &str) -> Result<()> {
        self.inner.remove_dir(path).await?;
        Ok(())
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.inner.rename(from, to).await?;
        Ok(())
    }

    pub async fn set_permissions(&self, path: &str, perm: u32) -> Result<()> {
        let mut meta = self.inner.metadata(path).await?;
        meta.permissions = Some(perm);
        self.inner.set_metadata(path, meta).await?;
        Ok(())
    }

    fn entry_from_meta(base: &str, name: String, meta: &Metadata) -> SftpEntry {
        let file_type = meta.file_type();
        let is_dir = file_type.is_dir();
        let path = if base.ends_with('/') {
            format!("{}{}", base, name)
        } else {
            format!("{}/{}", base, name)
        };
        SftpEntry {
            name,
            path,
            size: meta.size.unwrap_or(0),
            is_dir,
            modified: meta.mtime.map(|t| t as u64),
            permissions: meta.permissions,
        }
    }
}
