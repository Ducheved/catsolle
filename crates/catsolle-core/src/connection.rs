use crate::error::CoreError;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection as SqlConnection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub type ConnectionId = Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Connection {
    pub id: ConnectionId,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_method: AuthMethod,
    pub jump_hosts: Vec<JumpHost>,
    pub proxy: Option<ProxyConfig>,
    pub startup_commands: Vec<String>,
    pub env_vars: Vec<EnvVar>,
    pub group_id: Option<Uuid>,
    pub tags: Vec<ConnectionTag>,
    pub color: Option<String>,
    pub icon: Option<String>,
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_connected_at: Option<DateTime<Utc>>,
    pub is_favorite: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectionTag {
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectionGroup {
    pub id: Uuid,
    pub name: String,
    pub parent_id: Option<Uuid>,
    pub sort_order: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AuthMethod {
    Password {
        secret_ref: String,
    },
    Key {
        private_key_path: PathBuf,
        passphrase_ref: Option<String>,
    },
    Agent,
    KeyboardInteractive,
    Certificate {
        cert_path: PathBuf,
        private_key_path: PathBuf,
        passphrase_ref: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JumpHost {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_method: AuthMethod,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub proxy_type: ProxyType,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password_ref: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ProxyType {
    Socks5,
    HttpConnect,
}

#[derive(Clone, Debug)]
pub struct ConnectionStore {
    db_path: PathBuf,
}

impl ConnectionStore {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    pub fn init(&self) -> Result<(), CoreError> {
        if let Some(parent) = self.db_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = self.open()?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS connections (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                host TEXT NOT NULL,
                port INTEGER NOT NULL DEFAULT 22,
                username TEXT NOT NULL,
                auth_method TEXT NOT NULL,
                auth_data TEXT NOT NULL,
                jump_hosts TEXT,
                proxy TEXT,
                startup_commands TEXT,
                env_vars TEXT,
                group_id TEXT,
                tags TEXT,
                color TEXT,
                icon TEXT,
                notes TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                last_connected_at TEXT,
                is_favorite INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS connection_groups (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                parent_id TEXT,
                sort_order INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS history (
                id TEXT PRIMARY KEY,
                connection_id TEXT NOT NULL,
                connected_at TEXT NOT NULL,
                disconnected_at TEXT,
                duration_seconds INTEGER
            );
            CREATE TABLE IF NOT EXISTS bookmarks (
                id TEXT PRIMARY KEY,
                connection_id TEXT,
                path TEXT NOT NULL,
                name TEXT NOT NULL,
                is_local INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_connections_group ON connections(group_id);
            CREATE INDEX IF NOT EXISTS idx_connections_favorite ON connections(is_favorite);
            CREATE INDEX IF NOT EXISTS idx_history_connection ON history(connection_id);
            CREATE INDEX IF NOT EXISTS idx_history_date ON history(connected_at);
            "#,
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn create_connection(&self, conn: &Connection) -> Result<(), CoreError> {
        let db = self.open()?;
        db.execute(
            r#"
            INSERT INTO connections (
                id, name, host, port, username, auth_method, auth_data, jump_hosts, proxy,
                startup_commands, env_vars, group_id, tags, color, icon, notes,
                created_at, updated_at, last_connected_at, is_favorite
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
            "#,
            params![
                conn.id.to_string(),
                conn.name,
                conn.host,
                conn.port as i64,
                conn.username,
                conn.auth_method.as_key(),
                serde_json::to_string(&conn.auth_method).map_err(|e| CoreError::Invalid(e.to_string()))?,
                serde_json::to_string(&conn.jump_hosts).map_err(|e| CoreError::Invalid(e.to_string()))?,
                conn.proxy
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|e| CoreError::Invalid(e.to_string()))?,
                serde_json::to_string(&conn.startup_commands)
                    .map_err(|e| CoreError::Invalid(e.to_string()))?,
                serde_json::to_string(&conn.env_vars).map_err(|e| CoreError::Invalid(e.to_string()))?,
                conn.group_id.map(|id| id.to_string()),
                serde_json::to_string(&conn.tags).map_err(|e| CoreError::Invalid(e.to_string()))?,
                conn.color,
                conn.icon,
                conn.notes,
                conn.created_at.to_rfc3339(),
                conn.updated_at.to_rfc3339(),
                conn.last_connected_at.map(|v| v.to_rfc3339()),
                if conn.is_favorite { 1 } else { 0 },
            ],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn update_connection(&self, conn: &Connection) -> Result<(), CoreError> {
        let db = self.open()?;
        db.execute(
            r#"
            UPDATE connections SET
                name = ?2,
                host = ?3,
                port = ?4,
                username = ?5,
                auth_method = ?6,
                auth_data = ?7,
                jump_hosts = ?8,
                proxy = ?9,
                startup_commands = ?10,
                env_vars = ?11,
                group_id = ?12,
                tags = ?13,
                color = ?14,
                icon = ?15,
                notes = ?16,
                updated_at = ?17,
                last_connected_at = ?18,
                is_favorite = ?19
            WHERE id = ?1
            "#,
            params![
                conn.id.to_string(),
                conn.name,
                conn.host,
                conn.port as i64,
                conn.username,
                conn.auth_method.as_key(),
                serde_json::to_string(&conn.auth_method)
                    .map_err(|e| CoreError::Invalid(e.to_string()))?,
                serde_json::to_string(&conn.jump_hosts)
                    .map_err(|e| CoreError::Invalid(e.to_string()))?,
                conn.proxy
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|e| CoreError::Invalid(e.to_string()))?,
                serde_json::to_string(&conn.startup_commands)
                    .map_err(|e| CoreError::Invalid(e.to_string()))?,
                serde_json::to_string(&conn.env_vars)
                    .map_err(|e| CoreError::Invalid(e.to_string()))?,
                conn.group_id.map(|id| id.to_string()),
                serde_json::to_string(&conn.tags).map_err(|e| CoreError::Invalid(e.to_string()))?,
                conn.color,
                conn.icon,
                conn.notes,
                conn.updated_at.to_rfc3339(),
                conn.last_connected_at.map(|v| v.to_rfc3339()),
                if conn.is_favorite { 1 } else { 0 },
            ],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn delete_connection(&self, id: ConnectionId) -> Result<(), CoreError> {
        let db = self.open()?;
        db.execute(
            "DELETE FROM connections WHERE id = ?1",
            params![id.to_string()],
        )
        .map_err(|e| CoreError::Database(e.to_string()))?;
        Ok(())
    }

    pub fn get_connection(&self, id: ConnectionId) -> Result<Connection, CoreError> {
        let db = self.open()?;
        let mut stmt = db
            .prepare("SELECT * FROM connections WHERE id = ?1")
            .map_err(|e| CoreError::Database(e.to_string()))?;
        let row = stmt
            .query_row(params![id.to_string()], Self::row_to_connection)
            .optional()
            .map_err(|e| CoreError::Database(e.to_string()))?;
        row.ok_or(CoreError::NotFound)
    }

    pub fn list_connections(&self) -> Result<Vec<Connection>, CoreError> {
        let db = self.open()?;
        let mut stmt = db
            .prepare("SELECT * FROM connections ORDER BY name ASC")
            .map_err(|e| CoreError::Database(e.to_string()))?;
        let rows = stmt
            .query_map([], Self::row_to_connection)
            .map_err(|e| CoreError::Database(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| CoreError::Database(e.to_string()))?);
        }
        Ok(out)
    }

    pub fn list_recent(&self, limit: usize) -> Result<Vec<Connection>, CoreError> {
        let db = self.open()?;
        let mut stmt = db
            .prepare("SELECT * FROM connections ORDER BY last_connected_at DESC LIMIT ?1")
            .map_err(|e| CoreError::Database(e.to_string()))?;
        let rows = stmt
            .query_map([limit as i64], Self::row_to_connection)
            .map_err(|e| CoreError::Database(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| CoreError::Database(e.to_string()))?);
        }
        Ok(out)
    }

    pub fn import_from_ssh_config(&self, path: &Path) -> Result<Vec<Connection>, CoreError> {
        let content = fs::read_to_string(path)?;
        let mut entries = Vec::new();
        let mut current_hosts: Vec<String> = Vec::new();
        let mut current: HashMap<String, String> = HashMap::new();
        let existing = self.list_connections().unwrap_or_default();
        let mut known = HashSet::new();
        for conn in existing {
            known.insert((conn.host, conn.port, conn.username));
        }

        let mut flush = |hosts: &mut Vec<String>, map: &mut HashMap<String, String>| {
            if hosts.is_empty() {
                return;
            }
            let host_name = hosts
                .iter()
                .find(|h| !h.contains('*') && !h.contains('?'))
                .cloned()
                .unwrap_or_else(|| hosts[0].clone());
            let host = map
                .get("hostname")
                .cloned()
                .unwrap_or_else(|| host_name.clone());
            let username = map.get("user").cloned().unwrap_or_else(whoami::username);
            let port = map
                .get("port")
                .and_then(|v| v.parse::<u16>().ok())
                .unwrap_or(22);
            let identity = map.get("identityfile").map(PathBuf::from);
            let auth_method = if let Some(identity) = identity {
                AuthMethod::Key {
                    private_key_path: identity,
                    passphrase_ref: None,
                }
            } else {
                AuthMethod::Agent
            };
            let key = (host.clone(), port, username.clone());
            if known.contains(&key) {
                return;
            }
            known.insert(key);
            let now = Utc::now();
            entries.push(Connection {
                id: Uuid::new_v4(),
                name: host_name.clone(),
                host,
                port,
                username,
                auth_method,
                jump_hosts: Vec::new(),
                proxy: None,
                startup_commands: Vec::new(),
                env_vars: Vec::new(),
                group_id: None,
                tags: Vec::new(),
                color: None,
                icon: None,
                notes: None,
                created_at: now,
                updated_at: now,
                last_connected_at: None,
                is_favorite: false,
            });
        };

        for raw in content.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let key = parts.next().unwrap_or("").to_lowercase();
            let value = parts.collect::<Vec<&str>>().join(" ");
            if key == "host" {
                flush(&mut current_hosts, &mut current);
                current_hosts = value.split_whitespace().map(|v| v.to_string()).collect();
                current = HashMap::new();
            } else if !key.is_empty() {
                current.insert(key, value);
            }
        }
        flush(&mut current_hosts, &mut current);

        for entry in &entries {
            let _ = self.create_connection(entry);
        }

        Ok(entries)
    }

    fn row_to_connection(row: &rusqlite::Row<'_>) -> Result<Connection, rusqlite::Error> {
        let id: String = row.get("id")?;
        let auth_data: String = row.get("auth_data")?;
        let jump_hosts: String = row.get("jump_hosts")?;
        let proxy: Option<String> = row.get("proxy")?;
        let startup_commands: String = row.get("startup_commands")?;
        let env_vars: String = row.get("env_vars")?;
        let tags: String = row.get("tags")?;

        let auth_method: AuthMethod = serde_json::from_str(&auth_data).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
        let jump_hosts: Vec<JumpHost> = serde_json::from_str(&jump_hosts).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
        let proxy: Option<ProxyConfig> = match proxy {
            Some(p) => serde_json::from_str(&p).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?,
            None => None,
        };
        let startup_commands: Vec<String> =
            serde_json::from_str(&startup_commands).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
        let env_vars: Vec<EnvVar> = serde_json::from_str(&env_vars).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
        let tags: Vec<ConnectionTag> = serde_json::from_str(&tags).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;

        let created_at: String = row.get("created_at")?;
        let updated_at: String = row.get("updated_at")?;
        let last_connected_at: Option<String> = row.get("last_connected_at")?;

        Ok(Connection {
            id: Uuid::parse_str(&id).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?,
            name: row.get("name")?,
            host: row.get("host")?,
            port: row.get::<_, i64>("port")? as u16,
            username: row.get("username")?,
            auth_method,
            jump_hosts,
            proxy,
            startup_commands,
            env_vars,
            group_id: row
                .get::<_, Option<String>>("group_id")?
                .and_then(|v| Uuid::parse_str(&v).ok()),
            tags,
            color: row.get("color")?,
            icon: row.get("icon")?,
            notes: row.get("notes")?,
            created_at: DateTime::parse_from_rfc3339(&created_at)
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?
                .with_timezone(&Utc),
            updated_at: DateTime::parse_from_rfc3339(&updated_at)
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?
                .with_timezone(&Utc),
            last_connected_at: last_connected_at
                .and_then(|v| DateTime::parse_from_rfc3339(&v).ok())
                .map(|v| v.with_timezone(&Utc)),
            is_favorite: row.get::<_, i64>("is_favorite")? == 1,
        })
    }

    fn open(&self) -> Result<SqlConnection, CoreError> {
        SqlConnection::open(&self.db_path).map_err(|e| CoreError::Database(e.to_string()))
    }
}

impl AuthMethod {
    pub fn as_key(&self) -> &str {
        match self {
            AuthMethod::Password { .. } => "password",
            AuthMethod::Key { .. } => "key",
            AuthMethod::Agent => "agent",
            AuthMethod::KeyboardInteractive => "keyboard-interactive",
            AuthMethod::Certificate { .. } => "certificate",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_connection() -> Connection {
        Connection {
            id: Uuid::new_v4(),
            name: "test".to_string(),
            host: "127.0.0.1".to_string(),
            port: 22,
            username: "user".to_string(),
            auth_method: AuthMethod::Agent,
            jump_hosts: Vec::new(),
            proxy: None,
            startup_commands: Vec::new(),
            env_vars: Vec::new(),
            group_id: None,
            tags: Vec::new(),
            color: None,
            icon: None,
            notes: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_connected_at: None,
            is_favorite: false,
        }
    }

    #[test]
    fn create_and_get_connection() {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("test.db");
        let store = ConnectionStore::new(db);
        store.init().unwrap();

        let conn = sample_connection();
        store.create_connection(&conn).unwrap();
        let loaded = store.get_connection(conn.id).unwrap();

        assert_eq!(loaded.name, conn.name);
        assert_eq!(loaded.host, conn.host);
        assert_eq!(loaded.username, conn.username);
    }
}
