//! Storage backend abstraction. Each protocol implements [`Backend`]; the UI
//! only ever talks to the trait (through the worker in `crate::worker`).

pub mod ftp;
pub mod local;
pub mod s3;
pub mod sftp;

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{Notify, oneshot},
};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Ftp,
    Ftps,
    #[default]
    Sftp,
    S3,
}

impl Protocol {
    pub const ALL: [Protocol; 4] = [Protocol::Sftp, Protocol::Ftps, Protocol::Ftp, Protocol::S3];

    pub fn label(self) -> &'static str {
        match self {
            Protocol::Ftp => "FTP (plain, insecure)",
            Protocol::Ftps => "FTPS (explicit TLS)",
            Protocol::Sftp => "SFTP (SSH)",
            Protocol::S3 => "Amazon S3 / S3-compatible",
        }
    }

    pub fn scheme(self) -> &'static str {
        match self {
            Protocol::Ftp => "ftp",
            Protocol::Ftps => "ftps",
            Protocol::Sftp => "sftp",
            Protocol::S3 => "s3",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Protocol::Ftp | Protocol::Ftps => 21,
            Protocol::Sftp => 22,
            // Unused: S3 is addressed by endpoint URL.
            Protocol::S3 => 443,
        }
    }

    pub fn next(self, forward: bool) -> Protocol {
        let i = Self::ALL.iter().position(|p| *p == self).unwrap_or(0);
        let n = Self::ALL.len();
        Self::ALL[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    /// True for symlinks, even when `kind` was resolved to the link target's type.
    pub is_link: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub mode: Option<String>,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }

    /// The synthetic `..` row shown at the top of every pane.
    pub fn parent_dir() -> Self {
        Entry {
            name: "..".into(),
            kind: EntryKind::Dir,
            is_link: false,
            size: 0,
            modified: None,
            mode: None,
        }
    }

    pub fn is_parent_marker(&self) -> bool {
        self.name == ".."
    }
}

/// Shared between the transfer task (writer) and the UI (reader).
#[derive(Default)]
pub struct TransferCtl {
    done: AtomicU64,
    total: AtomicU64,
    cancelled: AtomicBool,
    wake: Notify,
}

impl TransferCtl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
    pub fn set_total(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
    }
    pub fn add_done(&self, n: u64) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
    /// Resolves once `cancel` has been called (or immediately if it already was).
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.wake.notified().await;
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        // `notify_one` stores a permit, so a cancel that lands between the flag
        // check and the `notified()` await is not lost.
        self.wake.notify_one();
    }
}

#[derive(Debug, thiserror::Error)]
#[error("transfer cancelled")]
pub struct Cancelled;

/// Copy `r` to `w` in 64 KiB chunks, updating progress and honouring cancel.
pub async fn copy_with_progress<R, W>(r: &mut R, w: &mut W, ctl: &TransferCtl) -> Result<()>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if ctl.is_cancelled() {
            return Err(Cancelled.into());
        }
        let step = async {
            let n = r.read(&mut buf).await?;
            if n > 0 {
                w.write_all(&buf[..n]).await?;
            }
            Ok::<usize, std::io::Error>(n)
        };
        tokio::select! {
            res = step => {
                let n = res?;
                if n == 0 {
                    break;
                }
                ctl.add_done(n as u64);
            }
            _ = ctl.wake.notified() => return Err(Cancelled.into()),
        }
    }
    w.flush().await?;
    Ok(())
}

/// Where a download is written before being renamed into place.
pub fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

/// Move a finished `.part` file into place, or remove it if the transfer failed.
pub async fn finalize_download(result: Result<()>, tmp: &Path, dest: &Path) -> Result<()> {
    match result {
        Ok(()) => tokio::fs::rename(tmp, dest)
            .await
            .with_context(|| format!("cannot move download into place at {}", dest.display())),
        Err(e) => {
            let _ = tokio::fs::remove_file(tmp).await;
            Err(e)
        }
    }
}

/// Asked of the user when an SSH server presents a key not in `known_hosts`.
pub struct HostKeyQuery {
    pub host: String,
    pub port: u16,
    pub algorithm: String,
    pub fingerprint: String,
    pub reply: oneshot::Sender<bool>,
}

pub type HostKeyPrompter = Arc<dyn Fn(HostKeyQuery) + Send + Sync>;

pub struct ConnectParams {
    pub protocol: Protocol,
    pub host: String,
    pub port: u16,
    pub username: String,
    /// Password, or the key passphrase when `key_path` is set.
    pub password: String,
    pub key_path: Option<PathBuf>,
    /// `IdentityFile`s from `~/.ssh/config` (SFTP).
    pub identity_files: Vec<PathBuf>,
    /// `IdentitiesOnly yes`: don't try ssh-agent or default keys.
    pub identities_only: bool,
    /// `IdentityAgent` from ssh config (socket path or `none`); `None` = `SSH_AUTH_SOCK`.
    pub identity_agent: Option<String>,
    /// S3 only. `host` is the endpoint URL (empty = AWS), `username` the access key
    /// id and `password` the secret key (both empty = the default credential chain).
    pub region: String,
    /// S3 only: a single bucket to work in; empty lists all buckets.
    pub bucket: String,
    /// S3 only: a named profile from `~/.aws/config`.
    pub aws_profile: String,
}

#[async_trait]
pub trait Backend: Send {
    /// The directory the session starts in.
    async fn home(&mut self) -> Result<String>;
    async fn list(&mut self, path: &str) -> Result<Vec<Entry>>;
    async fn mkdir(&mut self, path: &str) -> Result<()>;
    async fn remove_file(&mut self, path: &str) -> Result<()>;
    async fn remove_dir(&mut self, path: &str) -> Result<()>;
    async fn rename(&mut self, from: &str, to: &str) -> Result<()>;
    async fn download(&mut self, remote: &str, local: &Path, ctl: &TransferCtl) -> Result<()>;
    async fn upload(&mut self, local: &Path, remote: &str, ctl: &TransferCtl) -> Result<()>;
    /// Called periodically while idle to stop the server dropping the session.
    async fn keepalive(&mut self) -> Result<()> {
        Ok(())
    }
    async fn close(&mut self);
}

pub async fn connect(params: ConnectParams, prompter: HostKeyPrompter) -> Result<Box<dyn Backend>> {
    match params.protocol {
        Protocol::Ftp | Protocol::Ftps => ftp::connect(params).await,
        Protocol::Sftp => sftp::connect(params, prompter).await,
        Protocol::S3 => s3::connect(params).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn copy_reports_progress() {
        let data = vec![7u8; 200_000];
        let mut src = &data[..];
        let mut dst = Vec::new();
        let ctl = TransferCtl::new();
        copy_with_progress(&mut src, &mut dst, &ctl).await.unwrap();
        assert_eq!(dst, data);
        assert_eq!(ctl.done(), 200_000);
    }

    #[tokio::test]
    async fn copy_honours_cancel() {
        let (mut rd, _keep_open) = tokio::io::duplex(64);
        let mut dst = Vec::new();
        let ctl = TransferCtl::new();
        let ctl2 = ctl.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            ctl2.cancel();
        });
        // The read side never yields data: only cancel can end this.
        let err = copy_with_progress(&mut rd, &mut dst, &ctl)
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<Cancelled>().is_some());
    }

    #[test]
    fn part_names() {
        assert_eq!(
            part_path(Path::new("/a/b.txt")),
            PathBuf::from("/a/b.txt.part")
        );
    }

    #[test]
    fn protocol_cycle() {
        assert_eq!(Protocol::Sftp.next(true), Protocol::Ftps);
        // The list wraps around: SFTP, FTPS, FTP, S3.
        assert_eq!(Protocol::Sftp.next(false), Protocol::S3);
        assert_eq!(Protocol::S3.next(true), Protocol::Sftp);
        assert_eq!(Protocol::Ftps.next(true), Protocol::Ftp);
    }
}
