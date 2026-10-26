//! A plain-data snapshot of the connection form and its validation. Kept apart
//! from the UI state so the connect task can fill in missing values (from
//! 1Password) and validate them off the UI thread.

use std::path::PathBuf;

use directories::BaseDirs;

use crate::backend::{ConnectParams, Protocol};

#[derive(Clone, Default)]
pub struct FormValues {
    pub protocol: Protocol,
    pub name: String,
    pub host: String,
    pub port: String,
    pub user: String,
    pub pass: String,
    pub key: String,
    pub region: String,
    pub bucket: String,
    pub aws_profile: String,
    /// Directory to open after connecting (empty = the server's default).
    pub path: String,
}

// Not derived: the password must never end up in a debug dump.
impl std::fmt::Debug for FormValues {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FormValues")
            .field("protocol", &self.protocol)
            .field("name", &self.name)
            .field("host", &self.host)
            .field("user", &self.user)
            .field("pass", &"<redacted>")
            .finish_non_exhaustive()
    }
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(dirs) = BaseDirs::new()
    {
        return dirs.home_dir().join(rest);
    }
    PathBuf::from(p)
}

impl FormValues {
    /// The connection's name: the Name field, else the host (for S3 the bucket,
    /// then the endpoint's host). Used for saving and as the 1Password item name.
    pub fn effective_name(&self) -> String {
        let name = self.name.trim();
        if !name.is_empty() {
            return name.to_string();
        }
        let host = self.host.trim();
        if self.protocol == Protocol::S3 {
            let bucket = self.bucket.trim();
            if !bucket.is_empty() {
                return bucket.to_string();
            }
            if !host.is_empty() {
                let bare = host.split_once("://").map_or(host, |(_, h)| h);
                return bare.trim_end_matches('/').to_string();
            }
            return String::new();
        }
        host.to_string()
    }

    /// Validate and turn the form into connection parameters.
    pub fn build(&self) -> Result<ConnectParams, String> {
        let typed_host = self.host.trim().to_string();
        if self.protocol == Protocol::S3 {
            // Everything is optional: empty credentials use the AWS default chain.
            let (key, secret) = (self.user.trim(), self.pass.as_str());
            if key.is_empty() != secret.is_empty() {
                return Err(
                    "Enter both the access key id and the secret key, or leave both empty".into(),
                );
            }
            return Ok(ConnectParams {
                protocol: Protocol::S3,
                host: typed_host,
                port: Protocol::S3.default_port(),
                username: key.to_string(),
                password: secret.to_string(),
                key_path: None,
                identity_files: Vec::new(),
                identities_only: false,
                identity_agent: None,
                region: self.region.trim().to_string(),
                bucket: self.bucket.trim().to_string(),
                aws_profile: self.aws_profile.trim().to_string(),
            });
        }
        if typed_host.is_empty() {
            return Err("Host is required".into());
        }
        let mut port: u16 = self
            .port
            .trim()
            .parse()
            .ok()
            .filter(|p| *p != 0)
            .ok_or("Port must be a number from 1 to 65535")?;
        let mut username = self.user.trim().to_string();
        let mut host = typed_host.clone();
        let mut identity_files = Vec::new();
        let mut identities_only = false;
        let mut identity_agent = None;

        if self.protocol == Protocol::Sftp {
            // Honour ~/.ssh/config like `ssh <host>` does. Explicit form values win;
            // a port left at the default defers to the config.
            let cfg = crate::ssh_config::resolve_for_user(&typed_host);
            if let Some(proxy) = &cfg.proxy {
                return Err(format!(
                    "~/.ssh/config reaches '{typed_host}' via a proxy ({proxy}); \
                     ProxyJump/ProxyCommand are not supported"
                ));
            }
            if let Some(h) = cfg.hostname {
                host = h;
            }
            if port == self.protocol.default_port()
                && let Some(p) = cfg.port
            {
                port = p;
            }
            if username.is_empty() {
                username = cfg.user.unwrap_or_default();
            }
            identity_files = cfg.identity_files;
            identities_only = cfg.identities_only;
            identity_agent = cfg.identity_agent;
        }

        if self.protocol == Protocol::Sftp && username.is_empty() {
            return Err("Username is required for SFTP (or set User in ~/.ssh/config)".into());
        }
        let key_path = match (self.protocol, self.key.trim()) {
            (Protocol::Sftp, k) if !k.is_empty() => {
                let path = expand_tilde(k);
                if !path.is_file() {
                    return Err(format!("Key file not found: {}", path.display()));
                }
                Some(path)
            }
            _ => None,
        };
        Ok(ConnectParams {
            protocol: self.protocol,
            host,
            port,
            username,
            password: self.pass.clone(),
            key_path,
            identity_files,
            identities_only,
            identity_agent,
            region: String::new(),
            bucket: String::new(),
            aws_profile: String::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sftp() -> FormValues {
        FormValues {
            protocol: Protocol::Sftp,
            port: "22".into(),
            ..FormValues::default()
        }
    }

    #[test]
    fn name_falls_back_to_host() {
        let mut v = sftp();
        v.host = " alpha.example.com ".into();
        assert_eq!(v.effective_name(), "alpha.example.com");
        v.name = "  Prod box ".into();
        assert_eq!(v.effective_name(), "Prod box");
        v.name.clear();
        v.host.clear();
        assert_eq!(v.effective_name(), "");
    }

    #[test]
    fn s3_name_falls_back_to_bucket_then_endpoint() {
        let mut v = FormValues {
            protocol: Protocol::S3,
            ..FormValues::default()
        };
        assert_eq!(v.effective_name(), "");
        v.host = "https://minio.local:9000/".into();
        assert_eq!(v.effective_name(), "minio.local:9000");
        v.bucket = "assets".into();
        assert_eq!(v.effective_name(), "assets");
        v.name = "R2 assets".into();
        assert_eq!(v.effective_name(), "R2 assets");
    }

    #[test]
    fn validation() {
        let mut v = sftp();
        assert!(v.build().is_err()); // host
        v.host = "example.com".into();
        assert!(v.build().is_err()); // user
        v.user = "me".into();
        assert!(v.build().is_ok());
        v.port = "0".into();
        assert!(v.build().is_err());
        v.port = "70000".into();
        assert!(v.build().is_err());
    }

    #[test]
    fn debug_never_shows_the_password() {
        let v = FormValues {
            pass: "hunter2".into(),
            ..sftp()
        };
        assert!(!format!("{v:?}").contains("hunter2"));
    }
}
