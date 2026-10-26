//! FTP and explicit-TLS FTPS via `suppaftp`.

use std::{net::SocketAddr, path::Path, sync::Arc};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use suppaftp::{
    list::{File as FtpFile, ListParser},
    tokio::{AsyncNoTlsStream, AsyncRustlsConnector, AsyncRustlsStream, ImplAsyncFtpStream},
    tokio_rustls::{TlsConnector, rustls},
    types::FileType,
};
use tokio::net::lookup_host;

use super::{
    Backend, CONNECT_TIMEOUT, ConnectParams, Entry, EntryKind, Protocol, TransferCtl,
    copy_with_progress, finalize_download, part_path,
};
use crate::util::is_listable;

pub async fn connect(p: ConnectParams) -> Result<Box<dyn Backend>> {
    let addrs: Vec<SocketAddr> = lookup_host((p.host.as_str(), p.port))
        .await
        .with_context(|| format!("cannot resolve {}", p.host))?
        .collect();
    if addrs.is_empty() {
        bail!("no address found for {}", p.host);
    }

    // Anonymous login when no username is given.
    let (user, pass) = if p.username.is_empty() {
        ("anonymous".to_string(), "anonymous@".to_string())
    } else {
        (p.username.clone(), p.password.clone())
    };

    match p.protocol {
        Protocol::Ftp => {
            let mut ftp = connect_first(&addrs, &p.host, |addr| {
                ImplAsyncFtpStream::<AsyncNoTlsStream>::connect_timeout(addr, CONNECT_TIMEOUT)
            })
            .await?;
            ftp.login(user, pass).await.context("login failed")?;
            Ok(Box::new(FtpBackend::init(ftp).await?))
        }
        Protocol::Ftps => {
            let roots = trusted_roots().await;
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let connector = AsyncRustlsConnector::from(TlsConnector::from(Arc::new(config)));

            let ftp = connect_first(&addrs, &p.host, |addr| {
                ImplAsyncFtpStream::<AsyncRustlsStream>::connect_timeout(addr, CONNECT_TIMEOUT)
            })
            .await?;
            let mut ftp = ftp
                .into_secure(connector, &p.host)
                .await
                .context("could not secure the connection with TLS (the server may not support explicit FTPS, or its certificate is not trusted)")?;
            ftp.login(user, pass).await.context("login failed")?;
            Ok(Box::new(FtpBackend::init(ftp).await?))
        }
        Protocol::Sftp | Protocol::S3 => unreachable!("handled by their own backends"),
    }
}

/// Try each resolved address in order (so `localhost` works whether the server
/// listens on IPv4 or IPv6) and keep the first that accepts the connection.
async fn connect_first<S, F, Fut>(addrs: &[SocketAddr], host: &str, connect: F) -> Result<S>
where
    F: Fn(SocketAddr) -> Fut,
    Fut: std::future::Future<Output = suppaftp::FtpResult<S>>,
{
    let mut last = String::new();
    for addr in addrs {
        match connect(*addr).await {
            Ok(stream) => return Ok(stream),
            Err(e) => last = format!("{addr}: {e}"),
        }
    }
    bail!("cannot connect to {host} ({last})")
}

/// OS trust store (honours `SSL_CERT_FILE` / `SSL_CERT_DIR`), falling back to
/// the bundled Mozilla roots if the platform store yields nothing usable.
async fn trusted_roots() -> rustls::RootCertStore {
    let native = tokio::task::spawn_blocking(rustls_native_certs::load_native_certs)
        .await
        .map(|r| r.certs)
        .unwrap_or_default();
    let mut roots = rustls::RootCertStore::empty();
    for cert in native {
        let _ = roots.add(cert); // skip certificates rustls can't parse
    }
    if roots.is_empty() {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    roots
}

struct FtpBackend<T>
where
    T: suppaftp::tokio::TokioTlsStream + Send + Sync + 'static,
{
    ftp: ImplAsyncFtpStream<T>,
    use_mlsd: bool,
}

impl<T> FtpBackend<T>
where
    T: suppaftp::tokio::TokioTlsStream + Send + Sync + 'static,
{
    async fn init(mut ftp: ImplAsyncFtpStream<T>) -> Result<Self> {
        ftp.transfer_type(FileType::Binary)
            .await
            .context("cannot switch to binary transfer mode")?;
        // MLSD gives machine-readable listings; fall back to LIST when absent.
        let use_mlsd = ftp
            .feat()
            .await
            .map(|f| f.keys().any(|k| k.eq_ignore_ascii_case("MLST")))
            .unwrap_or(false);
        Ok(Self { ftp, use_mlsd })
    }

    fn parse_line(&self, line: &str) -> Option<FtpFile> {
        if self.use_mlsd {
            ListParser::parse_mlsd(line).ok()
        } else {
            ListParser::parse_posix(line)
                .or_else(|_| ListParser::parse_dos(line))
                .ok()
        }
    }
}

#[async_trait]
impl<T> Backend for FtpBackend<T>
where
    T: suppaftp::tokio::TokioTlsStream + Send + Sync + 'static,
{
    async fn home(&mut self) -> Result<String> {
        Ok(self.ftp.pwd().await?)
    }

    async fn list(&mut self, path: &str) -> Result<Vec<Entry>> {
        let lines = if self.use_mlsd {
            match self.ftp.mlsd(Some(path)).await {
                Ok(lines) => lines,
                Err(_) => {
                    // Some servers advertise MLST but reject MLSD.
                    self.use_mlsd = false;
                    self.ftp.list(Some(path)).await?
                }
            }
        } else {
            self.ftp.list(Some(path)).await?
        };

        let mut out = Vec::with_capacity(lines.len());
        for line in &lines {
            let Some(f) = self.parse_line(line) else {
                continue;
            };
            let name = f.name().to_string();
            if !is_listable(&name) {
                continue;
            }
            let kind = if f.is_directory() {
                EntryKind::Dir
            } else if f.is_symlink() {
                EntryKind::Symlink
            } else if f.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            out.push(Entry {
                name,
                kind,
                is_link: f.is_symlink(),
                size: if kind == EntryKind::Dir {
                    0
                } else {
                    f.size() as u64
                },
                modified: Some(f.modified()),
                mode: None,
            });
        }
        Ok(out)
    }

    async fn mkdir(&mut self, path: &str) -> Result<()> {
        Ok(self.ftp.mkdir(path).await?)
    }

    async fn remove_file(&mut self, path: &str) -> Result<()> {
        Ok(self.ftp.rm(path).await?)
    }

    async fn remove_dir(&mut self, path: &str) -> Result<()> {
        Ok(self.ftp.rmdir(path).await?)
    }

    async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        Ok(self.ftp.rename(from, to).await?)
    }

    async fn download(&mut self, remote: &str, local: &Path, ctl: &TransferCtl) -> Result<()> {
        if let Ok(size) = self.ftp.size(remote).await {
            ctl.set_total(size as u64);
        }
        let tmp = part_path(local);
        let mut file = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("cannot create {}", tmp.display()))?;
        let mut stream = self.ftp.retr_as_stream(remote).await?;

        let copied = copy_with_progress(&mut stream, &mut file, ctl).await;
        let result = match copied {
            Ok(()) => stream.finish().await.map_err(Into::into),
            Err(e) => {
                // Tell the server to stop sending so the control channel stays usable.
                let _ = self.ftp.abort(stream).await;
                Err(e)
            }
        };
        drop(file);
        finalize_download(result, &tmp, local).await
    }

    async fn upload(&mut self, local: &Path, remote: &str, ctl: &TransferCtl) -> Result<()> {
        let mut file = tokio::fs::File::open(local)
            .await
            .with_context(|| format!("cannot open {}", local.display()))?;
        ctl.set_total(file.metadata().await?.len());
        let mut stream = self.ftp.put_with_stream(remote).await?;

        let copied = copy_with_progress(&mut file, &mut stream, ctl).await;
        let result: Result<()> = match copied {
            Ok(()) => stream.finish().await.map_err(Into::into),
            Err(e) => {
                let _ = self.ftp.abort(stream).await;
                Err(e)
            }
        };
        if result.is_err() {
            // Don't leave a truncated file behind.
            let _ = self.ftp.rm(remote).await;
        }
        result
    }

    async fn keepalive(&mut self) -> Result<()> {
        Ok(self.ftp.noop().await?)
    }

    async fn close(&mut self) {
        let _ = self.ftp.quit().await;
    }
}
