use hmac::{Hmac, Mac};
use russh::keys::ssh_key::known_hosts::{
    Entry, HostPatterns, KnownHosts as KnownHostsFile, Marker,
};
use russh::keys::PublicKey;
use sha1::Sha1;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct KnownHosts {
    path: PathBuf,
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownHostResult {
    Match,
    Mismatch,
    NotFound,
    Revoked,
}

impl KnownHosts {
    pub fn load(path: PathBuf) -> anyhow::Result<Self> {
        let entries = if path.exists() {
            KnownHostsFile::read_file(&path)?
        } else {
            Vec::new()
        };
        Ok(Self { path, entries })
    }

    pub fn check(&self, host: &str, port: u16, key: &PublicKey) -> KnownHostResult {
        let host_for_match = host_to_pattern(host, port);
        for entry in &self.entries {
            if let Some(marker) = entry.marker() {
                if marker == &Marker::Revoked
                    && host_matches(entry.host_patterns(), &host_for_match, host, port)
                {
                    return KnownHostResult::Revoked;
                }
            }
            if host_matches(entry.host_patterns(), &host_for_match, host, port) {
                if entry.public_key() == key {
                    return KnownHostResult::Match;
                }
                return KnownHostResult::Mismatch;
            }
        }
        KnownHostResult::NotFound
    }

    pub fn add(
        &mut self,
        host: &str,
        port: u16,
        key: &PublicKey,
        _comment: &str,
    ) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let host_pattern = host_to_pattern(host, port);
        let line = format!("{} {}\n", host_pattern, key.to_openssh()?);
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?
            .write_all(line.as_bytes())?;
        self.entries = KnownHostsFile::read_file(&self.path)?;
        Ok(())
    }
}

fn host_to_pattern(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{}]:{}", host, port)
    }
}

fn host_matches(patterns: &HostPatterns, host_for_match: &str, host: &str, port: u16) -> bool {
    match patterns {
        HostPatterns::Patterns(list) => match_plain_patterns(list, host_for_match),
        HostPatterns::HashedName { salt, hash } => {
            let target = if port == 22 {
                host.to_string()
            } else {
                format!("[{}]:{}", host, port)
            };
            let computed = hash_hostname(salt, &target);
            hash == &computed
        }
    }
}

fn match_plain_patterns(patterns: &[String], host: &str) -> bool {
    let mut matched = false;
    for pattern in patterns {
        let (negated, pattern) = if let Some(stripped) = pattern.strip_prefix('!') {
            (true, stripped)
        } else {
            (false, pattern.as_str())
        };
        if glob_match(pattern, host) {
            if negated {
                return false;
            }
            matched = true;
        }
    }
    matched
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let bytes_p = pattern.as_bytes();
    let bytes_t = text.as_bytes();
    let mut star_idx: Option<usize> = None;
    let mut match_idx = 0usize;

    while t < bytes_t.len() {
        if p < bytes_p.len() && (bytes_p[p] == b'?' || bytes_p[p] == bytes_t[t]) {
            p += 1;
            t += 1;
        } else if p < bytes_p.len() && bytes_p[p] == b'*' {
            star_idx = Some(p);
            match_idx = t;
            p += 1;
        } else if let Some(si) = star_idx {
            p = si + 1;
            match_idx += 1;
            t = match_idx;
        } else {
            return false;
        }
    }

    while p < bytes_p.len() && bytes_p[p] == b'*' {
        p += 1;
    }

    p == bytes_p.len()
}

fn hash_hostname(salt: &[u8], host: &str) -> [u8; 20] {
    let mut mac = Hmac::<Sha1>::new_from_slice(salt).expect("hmac");
    mac.update(host.as_bytes());
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; 20];
    out.copy_from_slice(&result[..20]);
    out
}

use std::io::Write;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches() {
        assert!(glob_match("example.com", "example.com"));
        assert!(glob_match("*.example.com", "a.example.com"));
        assert!(!glob_match("*.example.com", "example.net"));
        assert!(glob_match("??.example.com", "ab.example.com"));
    }
}
