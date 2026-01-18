use chrono::Utc;
use serde::Serialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

#[derive(Clone, Debug, Serialize)]
pub struct RecordingEvent {
    pub time: f64,
    pub kind: String,
    pub data: String,
}

pub struct AsciinemaRecorder {
    writer: BufWriter<File>,
    start: Instant,
    closed: bool,
}

impl AsciinemaRecorder {
    pub fn start(
        path: &Path,
        width: u16,
        height: u16,
        env: HashMap<String, String>,
    ) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);
        let header = serde_json::json!({
            "version": 2,
            "width": width,
            "height": height,
            "timestamp": Utc::now().timestamp(),
            "env": env,
        });
        writeln!(writer, "{}", header)?;
        writer.flush()?;
        Ok(Self {
            writer,
            start: Instant::now(),
            closed: false,
        })
    }

    pub fn record_output(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.record_event("o", data)
    }

    pub fn record_input(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.record_event("i", data)
    }

    fn record_event(&mut self, kind: &str, data: &[u8]) -> anyhow::Result<()> {
        if self.closed {
            return Ok(());
        }
        let t = self.start.elapsed().as_secs_f64();
        let payload = String::from_utf8_lossy(data).to_string();
        let entry = serde_json::json!([t, kind, payload]);
        writeln!(self.writer, "{}", entry)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn close(&mut self) -> anyhow::Result<()> {
        if !self.closed {
            self.closed = true;
            self.writer.flush()?;
        }
        Ok(())
    }
}
