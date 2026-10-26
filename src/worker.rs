//! Owns the live connection on a background task so the UI never blocks on
//! network I/O. The UI sends [`Cmd`]s and receives [`AppEvent`]s.
//!
//! Commands run strictly in order: FTP has a single control channel, and
//! serialising SFTP operations as well keeps behaviour identical across
//! backends (a long transfer delays later commands rather than interleaving).

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    task::JoinHandle,
};

use crate::{
    backend::{
        self, Backend, Cancelled, ConnectParams, Entry, HostKeyPrompter, HostKeyQuery, Protocol,
        TransferCtl,
    },
    form::FormValues,
    onepassword,
};

pub enum Cmd {
    List {
        seq: u64,
        path: String,
    },
    Mkdir {
        path: String,
    },
    RemoveFile {
        path: String,
    },
    RemoveDir {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
    Download {
        remote: String,
        local: PathBuf,
        ctl: Arc<TransferCtl>,
    },
    Upload {
        local: PathBuf,
        remote: String,
        ctl: Arc<TransferCtl>,
    },
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Cancelled,
    Failed(String),
}

pub struct Session {
    /// How the connection is described in the pane title.
    pub label: String,
    /// Protocol and endpoint, for the header (e.g. `SFTP  alice@host:22`).
    pub detail: String,
    pub cmd: UnboundedSender<Cmd>,
    pub worker: JoinHandle<()>,
    pub home: String,
}

pub enum AppEvent {
    Connected(Result<Session, String>),
    /// A non-fatal problem worth showing (e.g. 1Password was unavailable).
    Warn(String),
    HostKey(HostKeyQuery),
    Listed {
        seq: u64,
        path: String,
        result: Result<Vec<Entry>, String>,
    },
    /// A mkdir/remove/rename finished. `what` is a past-tense description.
    OpDone {
        what: String,
        result: Result<(), String>,
    },
    TransferDone(Outcome),
    ConnectionLost(String),
}

fn fmt_err(e: &anyhow::Error) -> String {
    format!("{e:#}")
}

/// Describe a connection for the pane title.
pub fn label_for(params: &ConnectParams) -> String {
    if params.protocol == Protocol::S3 {
        let target = if params.bucket.is_empty() {
            "(all buckets)"
        } else {
            params.bucket.as_str()
        };
        let mut l = format!("s3://{target}");
        if !params.host.is_empty() {
            let bare = params
                .host
                .split_once("://")
                .map_or(params.host.as_str(), |(_, h)| h);
            l.push_str(&format!(" @ {bare}"));
        }
        return l;
    }
    let mut l = format!("{}://", params.protocol.scheme());
    if !params.username.is_empty() {
        l.push_str(&params.username);
        l.push('@');
    }
    l.push_str(&format!("{}:{}", params.host, params.port));
    l
}

/// One line for the header: protocol, then who and where.
pub fn detail_for(params: &ConnectParams) -> String {
    let proto = params.protocol.scheme().to_uppercase();
    if params.protocol == Protocol::S3 {
        let mut parts = vec![proto];
        if !params.region.is_empty() {
            parts.push(params.region.clone());
        }
        if !params.host.is_empty() {
            parts.push(
                params
                    .host
                    .split_once("://")
                    .map_or(params.host.as_str(), |(_, h)| h)
                    .to_string(),
            );
        }
        return parts.join("  ");
    }
    let who = if params.username.is_empty() {
        String::new()
    } else {
        format!("{}@", params.username)
    };
    format!("{proto}  {who}{}:{}", params.host, params.port)
}

/// Connect on a background task. If `op` is set, first fill the form's empty
/// fields from the 1Password item named after the connection. The returned
/// handle can be aborted to cancel (an `op` run in progress is killed with it).
pub fn spawn_connect(
    mut values: FormValues,
    op: Option<onepassword::Settings>,
    tx: UnboundedSender<AppEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Some(settings) = op {
            // The item is named after the connection as typed, before any fill-in.
            match onepassword::lookup(&values.effective_name(), &settings).await {
                onepassword::Lookup::Found(item) => item.fill(&mut values),
                onepassword::Lookup::NotFound => {}
                onepassword::Lookup::Unavailable(msg) => {
                    let _ = tx.send(AppEvent::Warn(msg));
                }
            }
        }
        match values.build() {
            Ok(params) => connect_with(params, tx).await,
            Err(e) => {
                let _ = tx.send(AppEvent::Connected(Err(e)));
            }
        }
    })
}

async fn connect_with(params: ConnectParams, tx: UnboundedSender<AppEvent>) {
    let label = label_for(&params);
    let detail = detail_for(&params);
    let prompter: HostKeyPrompter = {
        let tx = tx.clone();
        Arc::new(move |query| {
            let _ = tx.send(AppEvent::HostKey(query));
        })
    };

    let connected = async {
        let mut backend = backend::connect(params, prompter).await?;
        match backend.home().await {
            Ok(home) => Ok((backend, home)),
            Err(e) => {
                backend.close().await;
                Err(e)
            }
        }
    }
    .await;

    let event = match connected {
        Ok((backend, home)) => {
            let (cmd, rx) = unbounded_channel();
            let inner = tokio::spawn(run(backend, rx, tx.clone()));
            // A panic in the worker must not leave the UI waiting forever.
            let worker = tokio::spawn({
                let tx = tx.clone();
                async move {
                    if inner.await.is_err_and(|e| e.is_panic()) {
                        let _ = tx.send(AppEvent::ConnectionLost(
                            "internal error: the connection task crashed".into(),
                        ));
                    }
                }
            });
            AppEvent::Connected(Ok(Session {
                label,
                detail,
                cmd,
                worker,
                home,
            }))
        }
        Err(e) => AppEvent::Connected(Err(fmt_err(&e))),
    };
    let _ = tx.send(event);
}

fn outcome(result: Result<()>) -> Outcome {
    match result {
        Ok(()) => Outcome::Done,
        Err(e) if e.downcast_ref::<Cancelled>().is_some() => Outcome::Cancelled,
        Err(e) => Outcome::Failed(fmt_err(&e)),
    }
}

fn op_result(result: Result<()>) -> Result<(), String> {
    result.map_err(|e| fmt_err(&e))
}

async fn run(
    mut backend: Box<dyn Backend>,
    mut rx: UnboundedReceiver<Cmd>,
    tx: UnboundedSender<AppEvent>,
) {
    let mut keepalive = tokio::time::interval(Duration::from_secs(30));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    keepalive.tick().await; // the first tick fires immediately

    loop {
        let event = tokio::select! {
            cmd = rx.recv() => match cmd {
                None | Some(Cmd::Close) => break,
                Some(cmd) => Some(handle(backend.as_mut(), cmd).await),
            },
            _ = keepalive.tick() => match backend.keepalive().await {
                Ok(()) => None,
                Err(e) => {
                    let _ = tx.send(AppEvent::ConnectionLost(fmt_err(&e)));
                    break;
                }
            },
        };
        if let Some(event) = event
            && tx.send(event).is_err()
        {
            break; // UI is gone
        }
    }
    backend.close().await;
}

async fn handle(b: &mut dyn Backend, cmd: Cmd) -> AppEvent {
    match cmd {
        Cmd::List { seq, path } => {
            let result = b.list(&path).await.map_err(|e| fmt_err(&e));
            AppEvent::Listed { seq, path, result }
        }
        Cmd::Mkdir { path } => AppEvent::OpDone {
            what: format!("Created {path}"),
            result: op_result(b.mkdir(&path).await),
        },
        Cmd::RemoveFile { path } => AppEvent::OpDone {
            what: format!("Deleted {path}"),
            result: op_result(b.remove_file(&path).await),
        },
        Cmd::RemoveDir { path } => AppEvent::OpDone {
            what: format!("Deleted {path}"),
            result: op_result(b.remove_dir(&path).await),
        },
        Cmd::Rename { from, to } => AppEvent::OpDone {
            what: format!("Renamed to {to}"),
            result: op_result(b.rename(&from, &to).await),
        },
        Cmd::Download { remote, local, ctl } => {
            AppEvent::TransferDone(outcome(b.download(&remote, &local, &ctl).await))
        }
        Cmd::Upload { local, remote, ctl } => {
            AppEvent::TransferDone(outcome(b.upload(&local, &remote, &ctl).await))
        }
        Cmd::Close => unreachable!("Close is handled by the run loop"),
    }
}
