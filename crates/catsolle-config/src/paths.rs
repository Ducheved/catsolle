use anyhow::Result;
use directories::ProjectDirs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub log_dir: PathBuf,
    pub config_file: PathBuf,
    pub db_file: PathBuf,
    pub recordings_dir: PathBuf,
}

impl AppPaths {
    pub fn new() -> Result<Self> {
        let proj = ProjectDirs::from("org", "catsolle", "catsolle")
            .ok_or_else(|| anyhow::anyhow!("project dirs unavailable"))?;
        let config_dir = proj.config_dir().to_path_buf();
        let data_dir = proj.data_dir().to_path_buf();
        let cache_dir = proj.cache_dir().to_path_buf();
        let log_dir = data_dir.join("logs");
        let config_file = config_dir.join("config.toml");
        let db_file = data_dir.join("catsolle.db");
        let recordings_dir = data_dir.join("recordings");
        Ok(Self {
            config_dir,
            data_dir,
            cache_dir,
            log_dir,
            config_file,
            db_file,
            recordings_dir,
        })
    }

    pub fn project_config_path(base: impl AsRef<Path>) -> PathBuf {
        base.as_ref().join(".catsolle.toml")
    }
}
