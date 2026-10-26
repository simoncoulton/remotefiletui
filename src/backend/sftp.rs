//! SFTP over SSH via `russh` + `russh-sftp`, with `known_hosts` verification.

use std::{
    path::Path,
    path::PathBuf,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use directories::BaseDirs;
use russh::{
    Signer,
    client::{self, Handle},
    keys::{
        HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate,
        agent::{
            AgentIdentity,
            client::{AgentClient, AgentStream},
        },
        check_known_hosts,
        known_hosts::learn_known_hosts,
        load_secret_key,
    },
};
use russh_sftp::{client::SftpSession, protocol::FileType};
use tokio::{io::AsyncWriteExt, net::TcpStream, sync::oneshot};

use super::{
    Backend, CONNECT_TIMEOUT, ConnectParams, Entry, EntryKind, HostKeyPrompter, HostKeyQuery,
    TransferCtl, copy_with_progress, finalize_download, part_path,
};
use crate::util::{format_mode, is_listable};

#[derive(Debug, thiserror::Error)]
enum ClientError {
    #[error(transparent)]
    Ssh(#[from] russh::Error),
    #[error(
        "HOST KEY CHANGED for this server (known_hosts line {0}). This could be a \
         man-in-the-middle attack. Connection refused; fix known_hosts if the change is expected"
    )]
    KeyChanged(usize),
    #[error("server host key was not accepted")]
    Rejected,
    #[error("cannot verify host key: {0}")]
    Verify(String),
}

struct Handler {
    host: String,
    port: u16,
    prompter: HostKeyPrompter,
}

impl client::Handler for Handler {
    type Error = ClientError;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            return Err(ClientError::Verify(
                "certificate host keys are not supported".into(),
            ));
        };
        match check_known_hosts(&self.host, self.port, key) {
            Ok(true) => Ok(true),
            Ok(false) => {
                // Unknown host: trust on first use, but only if the user says so.
                let (tx, rx) = oneshot::channel();
                (self.prompter)(HostKeyQuery {
                    host: self.host.clone(),
                    port: self.port,
                    algorithm: key.algorithm().to_string(),
                    fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
                    reply: tx,
                });
                if !rx.await.unwrap_or(false) {
                    return Err(ClientError::Rejected);
                }
                learn_known_hosts(&self.host, self.port, key)
                    .map_err(|e| ClientError::Verify(format!("cannot update known_hosts: {e}")))?;
                Ok(true)
            }
            Err(russh::keys::Error::KeyChanged { line }) => Err(ClientError::KeyChanged(line)),
            Err(e) => Err(ClientError::Verify(e.to_string())),
        }
    }
}

pub async fn connect(p: ConnectParams, prompter: HostKeyPrompter) -> Result<Box<dyn Backend>> {
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    });

    let tcp = tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((p.host.as_str(), p.port)),
    )
    .await
    .with_context(|| format!("timed out connecting to {}:{}", p.host, p.port))?
    .with_context(|| format!("cannot connect to {}:{}", p.host, p.port))?;
    let _ = tcp.set_nodelay(true);

    let handler = Handler {
        host: p.host.clone(),
        port: p.port,
        prompter,
    };
    // No timeout here: the handshake may be waiting on the user to verify a host key.
    let mut session = client::connect_stream(config, tcp, handler).await?;

    authenticate(&mut session, &p).await?;

    let channel = session.channel_open_session().await?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .context("server refused the SFTP subsystem")?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .context("SFTP handshake failed")?;

    Ok(Box::new(SftpBackend { session, sftp }))
}

/// Adapts an ssh-agent connection to russh's `Signer`.
struct AgentSigner(AgentClient<Box<dyn AgentStream + Send + Unpin>>);

#[derive(Debug, thiserror::Error)]
enum SignError {
    #[error(transparent)]
    Send(#[from] russh::SendError),
    #[error(transparent)]
    Agent(#[from] russh::keys::Error),
}

impl Signer for AgentSigner {
    type Error = SignError;

    async fn auth_sign(
        &mut self,
        key: &AgentIdentity,
        hash_alg: Option<HashAlg>,
        to_sign: Vec<u8>,
    ) -> Result<Vec<u8>, Self::Error> {
        Ok(self.0.sign_request(key, hash_alg, to_sign).await?)
    }
}

/// Default identity files OpenSSH tries, in its order.
fn default_identity_files() -> Vec<PathBuf> {
    let Some(home) = BaseDirs::new().map(|d| d.home_dir().to_path_buf()) else {
        return Vec::new();
    };
    ["id_ed25519", "id_ecdsa", "id_rsa"]
        .iter()
        .map(|n| home.join(".ssh").join(n))
        .filter(|p| p.is_file())
        .collect()
}

/// Try each key file; encrypted ones are unlocked with the typed passphrase if any.
async fn try_key_files(
    session: &mut Handle<Handler>,
    user: &str,
    files: &[PathBuf],
    passphrase: Option<&str>,
    tried: &mut Vec<String>,
) -> Result<bool> {
    for path in files {
        let loaded = load_secret_key(path, None).or_else(|e| match passphrase {
            Some(pw) => load_secret_key(path, Some(pw)),
            None => Err(e),
        });
        let Ok(key) = loaded else {
            tried.push(format!(
                "{} (unreadable or passphrase-protected, skipped)",
                path.display()
            ));
            continue;
        };
        let hash = session.best_supported_rsa_hash().await?.flatten();
        let result = session
            .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
            .await?;
        if result.success() {
            return Ok(true);
        }
        tried.push(path.display().to_string());
    }
    Ok(false)
}

/// Try every identity in the agent. `socket` is an `IdentityAgent` path, or
/// `None` for `SSH_AUTH_SOCK`.
async fn try_agent(
    session: &mut Handle<Handler>,
    user: &str,
    socket: Option<&str>,
    tried: &mut Vec<String>,
) -> Result<bool> {
    let connected = match socket {
        Some(path) => AgentClient::connect_uds(path).await,
        None => AgentClient::connect_env().await,
    };
    let Ok(agent) = connected else {
        return Ok(false);
    };
    let mut signer = AgentSigner(agent.dynamic());
    let Ok(identities) = signer.0.request_identities().await else {
        return Ok(false);
    };
    let hash = session.best_supported_rsa_hash().await?.flatten();
    let count = identities.len();
    for identity in identities {
        let pubkey = identity.public_key().into_owned();
        if let Ok(r) = session
            .authenticate_publickey_with(user, pubkey, hash, &mut signer)
            .await
            && r.success()
        {
            return Ok(true);
        }
    }
    tried.push(format!("ssh-agent ({count} keys)"));
    Ok(false)
}

/// Authenticate in OpenSSH's order. With an explicit key file only that key is
/// used. Otherwise: a typed password, then the `IdentityFile`s from ssh config
/// (before the agent, so a server's attempt limit isn't spent on unrelated
/// agent keys), then ssh-agent (skipped with `IdentitiesOnly yes` or
/// `IdentityAgent none`), then the default `~/.ssh/id_*` keys if no
/// `IdentityFile` was configured.
async fn authenticate(session: &mut Handle<Handler>, p: &ConnectParams) -> Result<()> {
    let user = p.username.as_str();
    let passphrase = (!p.password.is_empty()).then_some(p.password.as_str());

    if let Some(path) = &p.key_path {
        let key = load_secret_key(path, passphrase)
            .with_context(|| format!("cannot load key {} (wrong passphrase?)", path.display()))?;
        let hash = session.best_supported_rsa_hash().await?.flatten();
        let result = session
            .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
            .await?;
        if result.success() {
            return Ok(());
        }
        bail!(
            "the server rejected key {} for user '{user}'",
            path.display()
        );
    }

    let mut tried: Vec<String> = Vec::new();

    if let Some(password) = passphrase {
        tried.push("password".into());
        if session
            .authenticate_password(user, password)
            .await?
            .success()
        {
            return Ok(());
        }
    }

    let configured: Vec<PathBuf> = p
        .identity_files
        .iter()
        .filter(|f| f.is_file())
        .cloned()
        .collect();
    if try_key_files(session, user, &configured, passphrase, &mut tried).await? {
        return Ok(());
    }

    let agent_disabled = p.identities_only || p.identity_agent.as_deref() == Some("none");
    if !agent_disabled && try_agent(session, user, p.identity_agent.as_deref(), &mut tried).await? {
        return Ok(());
    }

    if p.identity_files.is_empty()
        && !p.identities_only
        && try_key_files(
            session,
            user,
            &default_identity_files(),
            passphrase,
            &mut tried,
        )
        .await?
    {
        return Ok(());
    }

    if tried.is_empty() {
        bail!(
            "no credentials to try for '{user}': enter a password, set a key file, \
             or load a key into ssh-agent"
        );
    }
    bail!(
        "authentication failed for user '{user}' (tried: {})",
        tried.join(", ")
    );
}

struct SftpBackend {
    session: Handle<Handler>,
    sftp: SftpSession,
}

#[async_trait]
impl Backend for SftpBackend {
    async fn home(&mut self) -> Result<String> {
        Ok(self.sftp.canonicalize(".").await?)
    }

    async fn list(&mut self, path: &str) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        for item in self.sftp.read_dir(path).await? {
            let name = item.file_name();
            if !is_listable(&name) {
                continue;
            }
            let meta = item.metadata();
            let mut kind = match item.file_type() {
                FileType::Dir => EntryKind::Dir,
                FileType::File => EntryKind::File,
                FileType::Symlink => EntryKind::Symlink,
                _ => EntryKind::Other,
            };
            let mut size = meta.size.unwrap_or(0);
            let is_link = kind == EntryKind::Symlink;
            if is_link {
                // readdir reports the link itself; stat follows it.
                if let Ok(target) = self.sftp.metadata(item.path()).await {
                    if target.is_dir() {
                        kind = EntryKind::Dir;
                    }
                    size = target.size.unwrap_or(size);
                }
            }
            out.push(Entry {
                name,
                kind,
                is_link,
                size: if kind == EntryKind::Dir { 0 } else { size },
                modified: meta
                    .mtime
                    .map(|s| UNIX_EPOCH + Duration::from_secs(u64::from(s))),
                mode: meta
                    .permissions
                    .map(|m| format_mode(m, kind == EntryKind::Dir, is_link)),
            });
        }
        Ok(out)
    }

    async fn mkdir(&mut self, path: &str) -> Result<()> {
        Ok(self.sftp.create_dir(path).await?)
    }

    async fn remove_file(&mut self, path: &str) -> Result<()> {
        Ok(self.sftp.remove_file(path).await?)
    }

    async fn remove_dir(&mut self, path: &str) -> Result<()> {
        Ok(self.sftp.remove_dir(path).await?)
    }

    async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        Ok(self.sftp.rename(from, to).await?)
    }

    async fn download(&mut self, remote: &str, local: &Path, ctl: &TransferCtl) -> Result<()> {
        let mut src = self.sftp.open(remote).await?;
        if let Ok(meta) = src.metadata().await {
            ctl.set_total(meta.size.unwrap_or(0));
        }
        let tmp = part_path(local);
        let mut dst = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("cannot create {}", tmp.display()))?;
        let result = copy_with_progress(&mut src, &mut dst, ctl).await;
        drop(dst);
        finalize_download(result, &tmp, local).await
    }

    async fn upload(&mut self, local: &Path, remote: &str, ctl: &TransferCtl) -> Result<()> {
        let mut src = tokio::fs::File::open(local)
            .await
            .with_context(|| format!("cannot open {}", local.display()))?;
        ctl.set_total(src.metadata().await?.len());
        let mut dst = self.sftp.create(remote).await?;
        let mut result = copy_with_progress(&mut src, &mut dst, ctl).await;
        if result.is_ok() {
            // Closes the remote handle, surfacing any deferred write error.
            result = dst.shutdown().await.map_err(Into::into);
        } else {
            drop(dst);
        }
        if result.is_err() {
            let _ = self.sftp.remove_file(remote).await;
        }
        result
    }

    async fn close(&mut self) {
        let _ = self.sftp.close().await;
        let _ = self
            .session
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await;
    }
}
