use anyhow::Result;
use russh::keys::agent::client::AgentClient;
use russh::keys::{HashAlg, PrivateKey, PublicKey};
use std::path::Path;

#[derive(Clone, Debug)]
pub struct AgentKey {
    pub public_key: PublicKey,
    pub fingerprint: String,
}

pub struct AgentManager {
    client: AgentClient<Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>>,
}

impl AgentManager {
    pub async fn connect() -> Result<Self> {
        #[cfg(unix)]
        let client = AgentClient::connect_env().await?;

        #[cfg(windows)]
        let client = {
            use tokio::net::windows::named_pipe::ClientOptions;
            let sock = std::env::var("SSH_AUTH_SOCK")
                .unwrap_or_else(|_| "\\\\.\\pipe\\openssh-ssh-agent".to_string());
            let stream = ClientOptions::new().open(sock)?;
            AgentClient::connect(stream)
        };

        Ok(Self {
            client: client.dynamic(),
        })
    }

    pub async fn list_identities(&mut self) -> Result<Vec<AgentKey>> {
        let keys = self.client.request_identities().await?;
        let mut out = Vec::new();
        for key in keys {
            let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
            out.push(AgentKey {
                public_key: key,
                fingerprint,
            });
        }
        Ok(out)
    }

    pub async fn add_identity(&mut self, key: &PrivateKey) -> Result<()> {
        self.client.add_identity(key, &[]).await?;
        Ok(())
    }

    pub async fn remove_identity(&mut self, key: &PublicKey) -> Result<()> {
        self.client.remove_identity(key).await?;
        Ok(())
    }

    pub async fn remove_all(&mut self) -> Result<()> {
        self.client.remove_all_identities().await?;
        Ok(())
    }

    pub async fn add_identity_from_file(
        &mut self,
        path: &Path,
        passphrase: Option<&str>,
    ) -> Result<()> {
        let data = std::fs::read_to_string(path)?;
        let mut key = PrivateKey::from_openssh(&data)?;
        if key.is_encrypted() {
            let pass = passphrase.ok_or_else(|| anyhow::anyhow!("passphrase required"))?;
            key = key.decrypt(pass)?;
        }
        self.add_identity(&key).await?;
        Ok(())
    }
}
