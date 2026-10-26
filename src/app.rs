//! Application state and input handling. Rendering lives in `ui.rs`.

use std::{
    collections::{BTreeSet, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use directories::BaseDirs;
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    widgets::{ListState, TableState},
};
use tokio::{
    sync::{mpsc::UnboundedSender, oneshot},
    task::JoinHandle,
};

use crate::{
    backend::{ConnectParams, Entry, HostKeyQuery, Protocol, TransferCtl, local},
    config::{COLORS, Config, Filter, PasswordManager, Profile, TreeRow, clean_tags},
    form::FormValues,
    input::TextInput,
    onepassword,
    theme::{self, Theme},
    util::{format_size, remote_basename, remote_join, remote_parent, validate_name},
    worker::{self, AppEvent, Cmd, Outcome, Session},
};

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// ───────────────────────────── panes ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Local,
    Remote,
}

impl Side {
    fn other(self) -> Side {
        match self {
            Side::Local => Side::Remote,
            Side::Remote => Side::Local,
        }
    }
}

/// What to select after (re)loading a directory.
#[derive(Debug, Clone)]
pub enum Keep {
    First,
    Current,
    Name(String),
}

pub struct Pane {
    pub side: Side,
    pub cwd: String,
    raw: Vec<Entry>,
    pub view: Vec<Entry>,
    pub state: TableState,
    pub loading: bool,
    /// Visible rows, written by the renderer, used for PageUp/PageDown.
    pub page: usize,
}

impl Pane {
    fn new(side: Side, cwd: String) -> Self {
        Self {
            side,
            cwd,
            raw: Vec::new(),
            view: Vec::new(),
            state: TableState::default(),
            loading: false,
            page: 10,
        }
    }

    fn has_parent(&self) -> bool {
        match self.side {
            Side::Remote => !self.cwd.trim_end_matches('/').is_empty(),
            Side::Local => Path::new(&self.cwd).parent().is_some(),
        }
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.state.selected().and_then(|i| self.view.get(i))
    }

    fn set_listing(&mut self, cwd: String, raw: Vec<Entry>, show_hidden: bool, keep: Keep) {
        self.cwd = cwd;
        self.raw = raw;
        self.loading = false;
        self.rebuild(show_hidden, keep);
    }

    #[cfg(test)]
    fn move_by(&mut self, delta: isize) {
        let n = self.view.len();
        if n > 0 {
            let cur = self.state.selected().unwrap_or(0) as isize;
            self.state
                .select(Some((cur + delta).clamp(0, n as isize - 1) as usize));
        }
    }

    fn rebuild(&mut self, show_hidden: bool, keep: Keep) {
        let wanted = match keep {
            Keep::First => None,
            Keep::Name(n) => Some(n),
            Keep::Current => self.selected().map(|e| e.name.clone()),
        };
        let previous_index = self.state.selected();

        let mut view: Vec<Entry> = self
            .raw
            .iter()
            .filter(|e| show_hidden || !e.name.starts_with('.'))
            .cloned()
            .collect();
        view.sort_by(|a, b| {
            b.is_dir()
                .cmp(&a.is_dir())
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.name.cmp(&b.name))
        });
        if self.has_parent() {
            view.insert(0, Entry::parent_dir());
        }
        self.view = view;

        let index = match wanted {
            Some(name) => self
                .view
                .iter()
                .position(|e| e.name == name)
                .or(previous_index),
            None => Some(0),
        };
        self.state.select(match self.view.len() {
            0 => None,
            n => Some(index.unwrap_or(0).min(n - 1)),
        });
    }
}

// ───────────────────────────── browser ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Upload,
    Download,
}

#[derive(Debug, Clone)]
pub struct TransferPlan {
    pub direction: Direction,
    pub name: String,
    pub local: PathBuf,
    pub remote: String,
}

pub struct ActiveTransfer {
    pub plan: TransferPlan,
    pub ctl: Arc<TransferCtl>,
    pub started: Instant,
}

struct PendingList {
    seq: u64,
    keep: Keep,
    /// Where to go instead if this listing fails (the default path was unusable).
    fallback: Option<String>,
}

pub struct BrowserState {
    pub label: String,
    /// The connection's name (saved name, else host).
    pub name: String,
    /// Protocol and endpoint for the header.
    pub detail: String,
    /// Colour name of the saved connection this session came from.
    pub color: Option<String>,
    pub local: Pane,
    pub remote: Pane,
    pub active: Side,
    pub show_hidden: bool,
    pub transfer: Option<ActiveTransfer>,
    cmd: UnboundedSender<Cmd>,
    worker: Option<JoinHandle<()>>,
    seq: u64,
    pending: Option<PendingList>,
}

impl BrowserState {
    fn new(name: String, color: Option<String>, session: Session, local_cwd: String) -> Self {
        Self {
            label: session.label.clone(),
            name,
            detail: session.detail.clone(),
            color,
            local: Pane::new(Side::Local, local_cwd),
            remote: Pane::new(Side::Remote, session.home),
            active: Side::Remote,
            show_hidden: false,
            transfer: None,
            cmd: session.cmd,
            worker: Some(session.worker),
            seq: 0,
            pending: None,
        }
    }

    pub fn pane(&self, side: Side) -> &Pane {
        match side {
            Side::Local => &self.local,
            Side::Remote => &self.remote,
        }
    }

    fn pane_mut(&mut self, side: Side) -> &mut Pane {
        match side {
            Side::Local => &mut self.local,
            Side::Remote => &mut self.remote,
        }
    }

    fn send(&self, cmd: Cmd) -> Result<(), String> {
        self.cmd
            .send(cmd)
            .map_err(|_| "the connection is closed".to_string())
    }

    fn request_remote(&mut self, path: String, keep: Keep) -> Result<(), String> {
        self.request_remote_or(path, keep, None)
    }

    fn request_remote_or(
        &mut self,
        path: String,
        keep: Keep,
        fallback: Option<String>,
    ) -> Result<(), String> {
        self.seq += 1;
        self.pending = Some(PendingList {
            seq: self.seq,
            keep,
            fallback,
        });
        self.remote.loading = true;
        self.send(Cmd::List {
            seq: self.seq,
            path,
        })
    }

    fn load_local(&mut self, path: String, keep: Keep) -> Result<(), String> {
        match local::list(Path::new(&path)) {
            Ok(raw) => {
                self.local.set_listing(path, raw, self.show_hidden, keep);
                Ok(())
            }
            Err(e) => Err(format!("{path}: {e}")),
        }
    }

    fn load(&mut self, side: Side, path: String, keep: Keep) -> Result<(), String> {
        match side {
            Side::Local => self.load_local(path, keep),
            Side::Remote => self.request_remote(path, keep),
        }
    }

    fn refresh(&mut self, side: Side) -> Result<(), String> {
        let cwd = self.pane(side).cwd.clone();
        self.load(side, cwd, Keep::Current)
    }

    fn dir_path(&self, side: Side, name: &str) -> String {
        let cwd = &self.pane(side).cwd;
        match side {
            Side::Remote => remote_join(cwd, name),
            Side::Local => Path::new(cwd).join(name).to_string_lossy().into_owned(),
        }
    }

    fn open_selected(&mut self) -> Result<(), String> {
        let side = self.active;
        let Some(entry) = self.pane(side).selected().cloned() else {
            return Ok(());
        };
        if entry.is_parent_marker() {
            return self.go_up();
        }
        // Remote symlinks may point at directories we couldn't resolve; try them.
        let can_enter = entry.is_dir() || (side == Side::Remote && entry.is_link);
        if !can_enter {
            return Ok(());
        }
        let target = self.dir_path(side, &entry.name);
        self.load(side, target, Keep::First)
    }

    fn go_up(&mut self) -> Result<(), String> {
        let side = self.active;
        let cwd = self.pane(side).cwd.clone();
        let (parent, came_from) = match side {
            Side::Remote => (remote_parent(&cwd), remote_basename(&cwd).to_string()),
            Side::Local => {
                let p = Path::new(&cwd);
                let Some(parent) = p.parent() else {
                    return Ok(());
                };
                (
                    parent.to_string_lossy().into_owned(),
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
            }
        };
        if parent == cwd {
            return Ok(());
        }
        self.load(side, parent, Keep::Name(came_from))
    }

    /// Build a plan for copying the selected file to the other pane. The bool
    /// says whether the destination already exists.
    fn plan_transfer(&self) -> Result<(TransferPlan, bool), String> {
        if self.transfer.is_some() {
            return Err("A transfer is already running".into());
        }
        let side = self.active;
        let entry = self.pane(side).selected().ok_or("Nothing selected")?;
        if entry.is_parent_marker() {
            return Err("Select a file to copy".into());
        }
        if entry.is_dir() {
            return Err("Copying directories is not supported yet".into());
        }
        if !matches!(
            entry.kind,
            crate::backend::EntryKind::File | crate::backend::EntryKind::Symlink
        ) {
            return Err("Only regular files can be copied".into());
        }
        validate_name(&entry.name).map_err(|e| format!("Unsafe file name: {e}"))?;

        let name = entry.name.clone();
        let local = Path::new(&self.local.cwd).join(&name);
        let remote = remote_join(&self.remote.cwd, &name);
        let (direction, exists) = match side {
            Side::Local => (
                Direction::Upload,
                self.remote.view.iter().any(|e| e.name == name),
            ),
            Side::Remote => (Direction::Download, local.symlink_metadata().is_ok()),
        };
        Ok((
            TransferPlan {
                direction,
                name,
                local,
                remote,
            },
            exists,
        ))
    }

    fn begin_transfer(&mut self, plan: TransferPlan) -> Result<(), String> {
        let ctl = TransferCtl::new();
        let cmd = match plan.direction {
            Direction::Download => Cmd::Download {
                remote: plan.remote.clone(),
                local: plan.local.clone(),
                ctl: ctl.clone(),
            },
            Direction::Upload => Cmd::Upload {
                local: plan.local.clone(),
                remote: plan.remote.clone(),
                ctl: ctl.clone(),
            },
        };
        self.send(cmd)?;
        self.transfer = Some(ActiveTransfer {
            plan,
            ctl,
            started: Instant::now(),
        });
        Ok(())
    }

    /// Ask the worker to stop and release the connection.
    fn close(&mut self) {
        if let Some(t) = &self.transfer {
            t.ctl.cancel();
        }
        let _ = self.cmd.send(Cmd::Close);
    }
}

// ───────────────────────────── connect form ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Profiles,
    Name,
    Protocol,
    Host,
    Port,
    User,
    Pass,
    Key,
    // S3 only
    Region,
    Bucket,
    AwsProfile,
    /// Directory to open after connecting.
    Path,
    /// The activity log has focus (Tab reaches it; it isn't a form field).
    Log,
}

pub struct ConnectState {
    /// Optional connection name; empty falls back to the host.
    pub name: TextInput,
    pub protocol: Protocol,
    pub host: TextInput,
    pub port: TextInput,
    pub user: TextInput,
    pub pass: TextInput,
    pub key: TextInput,
    pub region: TextInput,
    pub bucket: TextInput,
    pub aws_profile: TextInput,
    /// Directory to open after connecting; empty = the server's default.
    pub path: TextInput,
    pub focus: Focus,
    /// Selected row of the saved-connections tree.
    pub profiles: ListState,
    pub collapsed: HashSet<String>,
    /// Saved-connections filter: free text (name, host, user, folder, bucket)
    /// and tags that must all be present.
    pub filter_text: String,
    pub filter_tags: BTreeSet<String>,
    /// Name of the saved connection whose values were last loaded into the form.
    pub loaded_from: Option<String>,
    /// The loaded values are only a preview of the highlighted connection (it is
    /// not being edited yet); the first move into the form makes it an edit.
    pub previewing: bool,
    pub error: Option<String>,
}

impl ConnectState {
    fn new(has_profiles: bool) -> Self {
        let protocol = Protocol::Sftp;
        Self {
            name: TextInput::default(),
            protocol,
            host: TextInput::default(),
            port: TextInput::new(protocol.default_port().to_string()),
            user: TextInput::default(),
            pass: TextInput::default(),
            key: TextInput::default(),
            region: TextInput::default(),
            bucket: TextInput::default(),
            aws_profile: TextInput::default(),
            path: TextInput::default(),
            focus: if has_profiles {
                Focus::Profiles
            } else {
                Focus::Host
            },
            profiles: ListState::default().with_selected(has_profiles.then_some(0)),
            collapsed: HashSet::new(),
            filter_text: String::new(),
            filter_tags: BTreeSet::new(),
            loaded_from: None,
            previewing: false,
            error: None,
        }
    }

    pub fn order(&self) -> Vec<Focus> {
        let mut v = vec![Focus::Profiles, Focus::Name, Focus::Protocol, Focus::Host];
        match self.protocol {
            Protocol::S3 => v.extend([
                Focus::Region,
                Focus::Bucket,
                Focus::User,
                Focus::Pass,
                Focus::AwsProfile,
            ]),
            Protocol::Sftp => v.extend([Focus::Port, Focus::User, Focus::Pass, Focus::Key]),
            Protocol::Ftp | Protocol::Ftps => v.extend([Focus::Port, Focus::User, Focus::Pass]),
        }
        v.push(Focus::Path);
        v
    }

    pub fn has_filter(&self) -> bool {
        !self.filter_text.trim().is_empty() || !self.filter_tags.is_empty()
    }

    /// The active filter for the list title, e.g. `prod eu "bob"`.
    pub fn filter_summary(&self) -> String {
        let mut parts: Vec<String> = self.filter_tags.iter().cloned().collect();
        let text = self.filter_text.trim();
        if !text.is_empty() {
            parts.push(format!("\"{text}\""));
        }
        parts.join(" ")
    }

    /// The first and last stops of the Tab cycle.
    fn edges(&self, has_profiles: bool) -> (Focus, Focus) {
        let order: Vec<Focus> = self
            .order()
            .into_iter()
            .filter(|f| has_profiles || *f != Focus::Profiles)
            .collect();
        (order[0], order[order.len() - 1])
    }

    fn move_focus(&mut self, forward: bool, has_profiles: bool) {
        let order: Vec<Focus> = self
            .order()
            .into_iter()
            .filter(|f| has_profiles || *f != Focus::Profiles)
            .collect();
        let i = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let n = order.len();
        self.focus = order[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }];
    }

    pub fn input_mut(&mut self) -> Option<&mut TextInput> {
        match self.focus {
            Focus::Name => Some(&mut self.name),
            Focus::Host => Some(&mut self.host),
            Focus::Port => Some(&mut self.port),
            Focus::User => Some(&mut self.user),
            Focus::Pass => Some(&mut self.pass),
            Focus::Key => Some(&mut self.key),
            Focus::Region => Some(&mut self.region),
            Focus::Bucket => Some(&mut self.bucket),
            Focus::AwsProfile => Some(&mut self.aws_profile),
            Focus::Path => Some(&mut self.path),
            Focus::Profiles | Focus::Protocol | Focus::Log => None,
        }
    }

    /// Empty the form for a brand-new connection (not tied to a saved one).
    fn clear(&mut self) {
        let protocol = Protocol::Sftp;
        for input in [
            &mut self.name,
            &mut self.host,
            &mut self.user,
            &mut self.pass,
            &mut self.key,
            &mut self.region,
            &mut self.bucket,
            &mut self.aws_profile,
            &mut self.path,
        ] {
            input.set(String::new());
        }
        self.protocol = protocol;
        self.port.set(protocol.default_port().to_string());
        self.loaded_from = None;
        self.previewing = false;
        self.error = None;
    }

    fn set_protocol(&mut self, protocol: Protocol) {
        // Move the port along only if the user hadn't customised it.
        if self.port.value().trim() == self.protocol.default_port().to_string() {
            self.port.set(protocol.default_port().to_string());
        }
        self.protocol = protocol;
        // The focused field may not exist for the new protocol.
        if !self.order().contains(&self.focus) {
            self.focus = Focus::Protocol;
        }
    }

    fn apply(&mut self, p: &Profile) {
        self.previewing = false;
        self.loaded_from = Some(p.name.clone());
        self.name.set(p.name.clone());
        self.protocol = p.protocol;
        self.host.set(p.host.clone());
        self.port.set(p.port.to_string());
        self.user.set(p.username.clone());
        self.key.set(p.key_path.clone().unwrap_or_default());
        self.region.set(p.region.clone().unwrap_or_default());
        self.bucket.set(p.bucket.clone().unwrap_or_default());
        self.aws_profile
            .set(p.aws_profile.clone().unwrap_or_default());
        self.path.set(p.path.clone().unwrap_or_default());
        self.pass.set(String::new());
        self.error = None;
    }

    /// A plain-data snapshot of the form.
    pub fn values(&self) -> FormValues {
        FormValues {
            protocol: self.protocol,
            name: self.name.value().to_string(),
            host: self.host.value().to_string(),
            port: self.port.value().to_string(),
            user: self.user.value().to_string(),
            pass: self.pass.value().to_string(),
            key: self.key.value().to_string(),
            region: self.region.value().to_string(),
            bucket: self.bucket.value().to_string(),
            aws_profile: self.aws_profile.value().to_string(),
            path: self.path.value().to_string(),
        }
    }

    fn build_params(&self) -> Result<ConnectParams, String> {
        self.values().build()
    }

    /// A profile from the form as typed (an ssh_config alias stays an alias),
    /// named by the Name field, else the host.
    fn to_profile(&self, manager: bool) -> Result<Profile, String> {
        // S3 has nothing to validate for saving (the secret is never stored, and
        // is empty after a connect), so only validate the other protocols. With a
        // password manager, details left empty are filled in when connecting, so
        // they aren't required here either.
        if self.protocol != Protocol::S3 && !manager {
            self.build_params()?;
        }
        self.snapshot()
    }

    /// The form as a profile, without validating it (no ssh_config or key file
    /// access), so it is cheap enough to compare against the saved one on every
    /// redraw.
    fn snapshot(&self) -> Result<Profile, String> {
        let values = self.values();
        let mut name = values.effective_name();
        if name.is_empty() {
            if self.protocol == Protocol::S3 {
                name = "amazon-s3".into();
            } else {
                return Err("Enter a host or a name first".into());
            }
        }
        let name = Config::clean_profile_name(&name).map_err(str::to_string)?;
        let host = self.host.value().trim().to_string();
        let username = self.user.value().trim().to_string();
        let non_empty = |i: &TextInput| {
            let v = i.value().trim();
            (!v.is_empty()).then(|| v.to_string())
        };

        if self.protocol == Protocol::S3 {
            return Ok(Profile {
                name,
                protocol: Protocol::S3,
                host,
                port: Protocol::S3.default_port(),
                username,
                region: non_empty(&self.region),
                bucket: non_empty(&self.bucket),
                aws_profile: non_empty(&self.aws_profile),
                path: non_empty(&self.path),
                ..Profile::default()
            });
        }

        let port: u16 = self
            .port
            .value()
            .trim()
            .parse()
            .unwrap_or(self.protocol.default_port());
        Ok(Profile {
            name,
            protocol: self.protocol,
            host,
            port,
            username,
            key_path: self.key_path_for_profile(),
            path: non_empty(&self.path),
            ..Profile::default()
        })
    }

    fn key_path_for_profile(&self) -> Option<String> {
        let k = self.key.value().trim();
        (self.protocol == Protocol::Sftp && !k.is_empty()).then(|| k.to_string())
    }
}

// ───────────────────────────── modals ─────────────────────────────

pub struct HostKeyPrompt {
    pub host: String,
    pub port: u16,
    pub algorithm: String,
    pub fingerprint: String,
    reply: Option<oneshot::Sender<bool>>,
}

impl HostKeyPrompt {
    fn answer(&mut self, accept: bool) {
        if let Some(tx) = self.reply.take() {
            let _ = tx.send(accept);
        }
    }
}

pub enum InputAction {
    Mkdir(Side),
    Rename(Side, String),
    /// Create a folder; optionally move a saved connection into it afterwards.
    NewFolder {
        move_profile: Option<String>,
        /// The folder to create it in (`None` = top level).
        parent: Option<String>,
    },
    RenameFolder(String),
    RenameProfile(String),
    EditTags(String),
    /// Save the form's connection under a name the user chooses.
    SaveProfile(Box<Profile>),
}

/// What to do once unsaved changes in the form have been dealt with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Load this saved connection into the form for editing.
    Edit(String),
    /// Start a blank connection.
    New,
    /// Connect to this saved connection.
    Connect(String),
    /// Copy the selected connection and edit the copy.
    Copy,
    /// Connect with what is in the form.
    ConnectForm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterFocus {
    Text,
    Tags,
}

/// An entry in the "move to folder" picker.
#[derive(Clone)]
pub enum FolderChoice {
    TopLevel,
    Folder(String),
    New,
}

pub enum ConfirmAction {
    Delete { side: Side, entry: Entry },
    Overwrite(TransferPlan),
    Disconnect,
    Quit,
    DeleteProfile(String),
    DeleteFolder(String),
}

pub enum Modal {
    Help {
        scroll: u16,
    },
    /// Pick a colour for a saved connection (`None` = no colour).
    Colors {
        profile: String,
        index: usize,
    },
    /// Filter the saved-connections list: a text field above a tag picker.
    Filter {
        text: TextInput,
        tags: Vec<(String, usize)>,
        selected: BTreeSet<String>,
        index: usize,
        focus: FilterFocus,
    },
    /// Pick a folder for a saved connection.
    Folders {
        choices: Vec<FolderChoice>,
        index: usize,
        /// The connection name, or the folder path when `folder` is set.
        profile: String,
        folder: bool,
    },
    Error {
        title: String,
        message: String,
    },
    /// The form has unsaved changes and the user is about to leave them.
    Unsaved {
        message: String,
        next: Next,
    },
    Connecting {
        label: String,
    },
    HostKey(HostKeyPrompt),
    Input {
        title: String,
        input: TextInput,
        error: Option<String>,
        action: InputAction,
    },
    Confirm {
        message: String,
        action: ConfirmAction,
    },
    /// Theme picker: `index` is previewed live, `original` is restored on cancel.
    Themes {
        index: usize,
        original: usize,
        original_transparent: bool,
    },
    /// Choose the password manager (index 0 is "none").
    PasswordManager {
        index: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Ok,
    Warn,
    Error,
}

/// One line of the activity log (connections, navigation, operations).
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub time: String,
    pub level: LogLevel,
    pub text: String,
}

const LOG_LIMIT: usize = 500;

pub struct Notice {
    pub text: String,
    pub is_error: bool,
}

// ───────────────────────────── app ─────────────────────────────

pub struct App {
    pub form: ConnectState,
    pub browser: Option<BrowserState>,
    pub modal: Option<Modal>,
    pub notice: Option<Notice>,
    /// Activity log, oldest first. `log_scroll` is how many lines it is scrolled
    /// back from the newest (0 follows new entries).
    pub log: VecDeque<LogEntry>,
    pub log_scroll: usize,
    /// The log has keyboard focus: `log_sel` (an index into `log`) is highlighted.
    pub log_focus: bool,
    pub log_sel: usize,
    /// Whether the log panel fits on screen (set by the renderer).
    pub log_visible: bool,
    pub config: Config,
    pub tick: usize,
    pub should_quit: bool,
    pub theme: Theme,
    /// Every theme, in picker order (built-ins, theme files, `themes:`).
    pub themes: Vec<Theme>,
    pub theme_index: usize,
    pub transparent: bool,
    pub truecolor: bool,
    /// The config file exists but couldn't be parsed; never overwrite it.
    config_unreadable: bool,
    tx: UnboundedSender<AppEvent>,
    connect_task: Option<JoinHandle<()>>,
    connecting_label: String,
    connecting_color: Option<String>,
    connecting_path: Option<String>,
    /// Carried out once a save that was asked for by the unsaved-changes prompt
    /// completes.
    after_save: Option<Next>,
    /// `g` was the previous key, so another `g` means "top" (vim `gg`).
    g_prev: bool,
    g_pending: bool,
    /// Where the config lives, shown in the header (`~/…`).
    pub config_hint: String,
}

fn transfer_start_line(plan: &TransferPlan) -> String {
    let verb = match plan.direction {
        Direction::Upload => "Uploading",
        Direction::Download => "Downloading",
    };
    format!("{verb} {}…", plan.name)
}

/// Move within a list the way vim does. `page` is the visible height.
enum Nav {
    To(usize),
    /// The first `g` of `gg`.
    G,
    No,
}

fn nav(key: &KeyEvent, g_prev: bool, index: usize, last: usize, page: usize) -> Nav {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let half = (page / 2).max(1);
    let page = page.max(1);
    Nav::To(match key.code {
        KeyCode::Up | KeyCode::Char('k') if !ctrl => index.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') if !ctrl => (index + 1).min(last),
        KeyCode::Char('g') if !ctrl => {
            if g_prev {
                0
            } else {
                return Nav::G;
            }
        }
        KeyCode::Home => 0,
        KeyCode::End | KeyCode::Char('G') => last,
        KeyCode::Char('d') if ctrl => (index + half).min(last),
        KeyCode::Char('u') if ctrl => index.saturating_sub(half),
        KeyCode::PageDown => (index + page).min(last),
        KeyCode::Char('f') if ctrl => (index + page).min(last),
        KeyCode::PageUp => index.saturating_sub(page),
        KeyCode::Char('b') if ctrl => index.saturating_sub(page),
        _ => return Nav::No,
    })
}

/// Turn the form's default path into an absolute remote path: `~` and relative
/// paths are taken from the server's default directory.
fn resolve_start_path(path: &str, home: &str) -> Option<String> {
    let p = path.trim();
    if p.is_empty() {
        return None;
    }
    let resolved = if p == "~" {
        home.to_string()
    } else if let Some(rest) = p.strip_prefix("~/") {
        remote_join(home, rest)
    } else if p.starts_with('/') {
        p.to_string()
    } else {
        remote_join(home, p)
    };
    let trimmed = resolved.trim_end_matches('/');
    Some(if trimmed.is_empty() {
        "/".into()
    } else {
        trimmed.to_string()
    })
}

impl App {
    pub fn new(
        tx: UnboundedSender<AppEvent>,
        config: Config,
        startup_error: Option<String>,
    ) -> Self {
        let form = ConnectState::new(!config.profiles.is_empty());
        let config_dir = config.dir().map(Path::to_path_buf);
        let truecolor = theme::detect_truecolor();
        let config_unreadable = startup_error.is_some();
        let transparent = config.transparent;
        // Theme problems (a bad theme file, an unknown theme name) are reported
        // and the built-ins used; they never stop connections from being saved.
        let (themes, mut theme_error) = match config.dir() {
            Some(dir) => match theme::load_themes(&dir.join("themes"), &config.themes) {
                Ok(t) => (t, None),
                Err(e) => (theme::builtin_themes(), Some(format!("{e:#}"))),
            },
            None => (theme::all_themes_unfiled(&config.themes), None),
        };
        let wanted = config.theme.as_deref().unwrap_or("default");
        let theme_index = theme::position(&themes, wanted).unwrap_or_else(|| {
            theme_error.get_or_insert_with(|| {
                format!("unknown theme '{wanted}' in config.yml (see the themes/ folder for names)")
            });
            0
        });
        let mut app = Self {
            form,
            browser: None,
            modal: None,
            notice: None,
            log: VecDeque::new(),
            log_scroll: 0,
            log_focus: false,
            log_sel: 0,
            log_visible: false,
            config,
            tick: 0,
            should_quit: false,
            theme: themes[theme_index]
                .clone()
                .transparent(transparent)
                .quantized(truecolor),
            themes,
            transparent,
            theme_index,
            truecolor,
            config_unreadable,
            tx,
            connect_task: None,
            connecting_label: String::new(),
            connecting_color: None,
            connecting_path: None,
            after_save: None,
            g_prev: false,
            g_pending: false,
            config_hint: config_dir
                .map(|p| format!("{}/", crate::util::tilde(&p)))
                .unwrap_or_default(),
        };
        if let Some(message) = startup_error {
            app.error("Config problem", message);
        } else if let Some(message) = theme_error {
            app.error("Theme problem", message);
        }
        app.preview_selected();
        app
    }

    /// Write the config, unless it was unreadable at startup (that would
    /// replace the user's file with ours).
    fn save_config(&self) -> anyhow::Result<()> {
        if self.config_unreadable {
            anyhow::bail!(
                "not saving: the existing config file could not be parsed. \
                 Fix or remove it, then restart"
            );
        }
        self.config.save()
    }

    fn set_theme(&mut self, index: usize) {
        self.theme_index = index;
        self.apply_theme();
    }

    fn apply_theme(&mut self) {
        self.theme = self.themes[self.theme_index]
            .clone()
            .transparent(self.transparent)
            .quantized(self.truecolor);
    }

    /// Change one setting in `config.yml`, unless the config is unreadable.
    fn save_setting(&self, key: &str, value: &str) -> anyhow::Result<()> {
        if self.config_unreadable {
            anyhow::bail!(
                "not saving: the existing config file could not be parsed. \
                 Fix or remove it, then restart"
            );
        }
        self.config.save_setting(key, value)
    }

    fn open_theme_picker(&mut self) {
        self.modal = Some(Modal::Themes {
            index: self.theme_index,
            original: self.theme_index,
            original_transparent: self.transparent,
        });
    }

    pub fn is_busy(&self) -> bool {
        matches!(self.modal, Some(Modal::Connecting { .. }))
            || self
                .browser
                .as_ref()
                .is_some_and(|b| b.remote.loading || b.transfer.is_some())
    }

    /// Append to the activity log. Scrolled-back views stay where they are.
    pub fn log(&mut self, level: LogLevel, text: impl Into<String>) {
        let text: String = text.into();
        // One line per entry; an error with several lines keeps its first.
        let text = text.lines().next().unwrap_or("").trim().to_string();
        self.log.push_back(LogEntry {
            time: chrono::Local::now().format("%H:%M:%S").to_string(),
            level,
            text,
        });
        if self.log.len() > LOG_LIMIT {
            self.log.pop_front();
            self.log_sel = self.log_sel.saturating_sub(1);
        }
        if self.log_scroll > 0 {
            self.log_scroll = (self.log_scroll + 1).min(self.log.len());
        }
    }

    /// Move keyboard focus to the log, highlighting its newest line.
    fn enter_log(&mut self) {
        self.log_focus = true;
        self.log_sel = self.log.len().saturating_sub(1);
        self.log_scroll = 0;
        if self.browser.is_none() {
            self.form.focus = Focus::Log;
        }
    }

    /// Leave the log, to the start (`forward`) or the end of the focus cycle.
    fn leave_log(&mut self, forward: bool) {
        self.log_focus = false;
        let has_profiles =
            !self.config.profiles.is_empty() || !self.config.folder_names().is_empty();
        if let Some(b) = self.browser.as_mut() {
            b.active = if forward { Side::Local } else { Side::Remote };
        } else {
            let order: Vec<Focus> = self
                .form
                .order()
                .into_iter()
                .filter(|f| has_profiles || *f != Focus::Profiles)
                .collect();
            self.form.focus = if forward {
                order[0]
            } else {
                order[order.len() - 1]
            };
        }
    }

    /// Keys while the log has focus: move the highlighted line.
    fn on_log_key(&mut self, key: KeyEvent) {
        let n = self.log.len();
        if n > 0 {
            match nav(&key, self.g_prev, self.log_sel.min(n - 1), n - 1, 4) {
                Nav::To(i) => {
                    self.log_sel = i;
                    return;
                }
                Nav::G => {
                    self.g_pending = true;
                    return;
                }
                Nav::No => {}
            }
        }
        match key.code {
            KeyCode::Tab => self.leave_log(true),
            KeyCode::BackTab | KeyCode::Esc => self.leave_log(false),
            KeyCode::Char('?') | KeyCode::F(1) => self.modal = Some(Modal::Help { scroll: 0 }),
            KeyCode::Char('q') if self.browser.is_none() => self.request_quit(),
            _ => {}
        }
    }

    /// Scroll the log: J / K when not typing, PageUp / PageDown anywhere on the
    /// connect form's text fields. Returns whether the key was used.
    fn log_key(&mut self, key: &KeyEvent) -> bool {
        let typing =
            self.browser.is_none() && !matches!(self.form.focus, Focus::Profiles | Focus::Protocol);
        let (up, step) = match key.code {
            KeyCode::Char('K') if !typing => (true, 1),
            KeyCode::Char('J') if !typing => (false, 1),
            KeyCode::PageUp if typing || self.browser.is_none() => (true, 4),
            KeyCode::PageDown if typing || self.browser.is_none() => (false, 4),
            _ => return false,
        };
        // The renderer clamps to what can really be scrolled.
        self.log_scroll = if up {
            (self.log_scroll + step).min(self.log.len())
        } else {
            self.log_scroll.saturating_sub(step)
        };
        true
    }

    fn info(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice {
            text: text.into(),
            is_error: false,
        });
    }

    fn warn(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice {
            text: text.into(),
            is_error: true,
        });
    }

    fn error(&mut self, title: &str, message: impl Into<String>) {
        self.modal = Some(Modal::Error {
            title: title.into(),
            message: message.into(),
        });
    }

    // ───────── saved-connections tree ─────────

    fn filter(&self) -> Filter {
        Filter::new(&self.form.filter_text, self.form.filter_tags.iter())
    }

    /// The saved connection whose values are in the form, if it still exists.
    pub fn editing(&self) -> Option<&Profile> {
        if self.form.previewing {
            return None;
        }
        let name = self.form.loaded_from.as_deref()?;
        self.config.profiles.iter().find(|p| p.name == name)
    }

    /// Unsaved work in the form: changes to the connection being edited, or a
    /// new connection that has been started but not saved.
    pub fn form_dirty(&self) -> bool {
        if self.form.previewing {
            return false;
        }
        if self.editing().is_some() {
            return self.form_modified();
        }
        let f = &self.form;
        [
            &f.name,
            &f.host,
            &f.user,
            &f.key,
            &f.region,
            &f.bucket,
            &f.aws_profile,
            &f.path,
        ]
        .iter()
        .any(|i| !i.value().trim().is_empty())
    }

    /// Show the highlighted connection's values in the form as a read-only
    /// preview (a folder or nothing shows a blank form). Leaves the form alone
    /// while it holds an edit in progress or an unsaved new connection.
    fn preview_selected(&mut self) {
        if !self.form.previewing && (self.editing().is_some() || self.form_dirty()) {
            return;
        }
        match self.selected_profile().cloned() {
            Some(p) => {
                self.form.apply(&p);
                self.form.previewing = true;
            }
            None => self.form.clear(),
        }
    }

    /// Do `next`, first asking what to do with unsaved changes in the form.
    fn request(&mut self, next: Next) {
        // Staying on the entry already in the form loses nothing.
        let same = match &next {
            Next::Edit(n) | Next::Connect(n) => {
                self.form.loaded_from.as_deref() == Some(n.as_str())
            }
            _ => false,
        };
        // Connecting with an unsaved new form is fine; only edits to a saved
        // connection are worth a prompt there.
        let connect_new = next == Next::ConnectForm && self.editing().is_none();
        if same || connect_new || !self.form_dirty() {
            return self.perform(next);
        }
        let message = match self.editing() {
            Some(p) => format!(
                "'{}' has unsaved changes. Save them first?",
                crate::util::sanitize(&p.name)
            ),
            None => "This new connection isn't saved. Save it first?".to_string(),
        };
        self.modal = Some(Modal::Unsaved { message, next });
    }

    fn perform(&mut self, next: Next) {
        match next {
            Next::New => self.new_connection(),
            Next::Copy => self.duplicate_profile(),
            Next::ConnectForm => self.start_connect(),
            Next::Connect(name) => {
                // The form already holds it (as a preview or an edit) when it was
                // highlighted; otherwise load it first.
                if self.form.loaded_from.as_deref() != Some(name.as_str()) {
                    let Some(p) = self
                        .config
                        .profiles
                        .iter()
                        .find(|p| p.name == name)
                        .cloned()
                    else {
                        return;
                    };
                    self.form.apply(&p);
                }
                self.start_connect();
            }
            Next::Edit(name) => self.edit_profile(&name),
        }
    }

    fn run_after_save(&mut self) {
        if let Some(next) = self.after_save.take() {
            self.perform(next);
        }
    }

    /// Whether the form differs from the saved connection being edited.
    pub fn form_modified(&self) -> bool {
        let Some(saved) = self.editing() else {
            return false;
        };
        match self.form.snapshot() {
            Ok(mut p) => {
                p.folder = saved.folder.clone();
                p.color = saved.color.clone();
                p.tags = saved.tags.clone();
                p != *saved
            }
            Err(_) => true,
        }
    }

    fn rows(&self) -> Vec<TreeRow> {
        self.config.tree(&self.form.collapsed, &self.filter())
    }

    fn selected_row(&self) -> Option<TreeRow> {
        let i = self.form.profiles.selected()?;
        self.rows().get(i).cloned()
    }

    /// Whether the saved list has a row selected, and if so whether it is a connection.
    pub fn selection(&self) -> (bool, bool) {
        let row = self.selected_row();
        (row.is_some(), matches!(row, Some(TreeRow::Profile { .. })))
    }

    pub fn selected_profile(&self) -> Option<&Profile> {
        match self.selected_row()? {
            TreeRow::Profile { index, .. } => self.config.profiles.get(index),
            TreeRow::Folder { .. } => None,
        }
    }

    fn selected_folder(&self) -> Option<String> {
        match self.selected_row()? {
            TreeRow::Folder { name, .. } => Some(name),
            TreeRow::Profile { .. } => None,
        }
    }

    /// The folder a newly saved connection should be filed under: the selected
    /// folder, or the folder of the selected connection.
    fn context_folder(&self) -> Option<String> {
        match self.selected_row()? {
            TreeRow::Folder { name, .. } => Some(name),
            TreeRow::Profile { index, .. } => self.config.profiles.get(index)?.folder.clone(),
        }
    }

    /// Keep the selection valid after rows were added or removed.
    fn clamp_selection(&mut self) {
        let n = self.rows().len();
        let sel = self.form.profiles.selected().unwrap_or(0);
        self.form
            .profiles
            .select(if n == 0 { None } else { Some(sel.min(n - 1)) });
        if n == 0 && self.form.focus == Focus::Profiles {
            self.form.focus = Focus::Host;
        }
    }

    /// Expand `folder` and every folder above it, so it is visible.
    fn expand_to(&mut self, folder: &str) {
        let mut cur = Some(folder);
        while let Some(f) = cur {
            self.form.collapsed.remove(f);
            cur = Config::folder_parent(f);
        }
    }

    /// Keep collapsed state when a folder (and what is in it) changes path.
    fn remap_collapsed(&mut self, old: &str, new: &str) {
        let moved: Vec<String> = self
            .form
            .collapsed
            .iter()
            .filter(|c| Config::folder_within(c, old))
            .cloned()
            .collect();
        for c in moved {
            self.form.collapsed.remove(&c);
            self.form
                .collapsed
                .insert(format!("{new}{}", &c[old.len()..]));
        }
    }

    fn select_profile(&mut self, name: &str) {
        if let Some(folder) = self
            .config
            .profiles
            .iter()
            .find(|p| p.name == name)
            .and_then(|p| p.folder.clone())
        {
            self.expand_to(&folder);
        }
        let idx = self.rows().iter().position(|r| {
            matches!(r, TreeRow::Profile { index, .. } if self.config.profiles[*index].name == name)
        });
        if idx.is_some() {
            self.form.profiles.select(idx);
        }
    }

    fn select_folder(&mut self, name: &str) {
        if let Some(parent) = Config::folder_parent(name) {
            let parent = parent.to_string();
            self.expand_to(&parent);
        }
        let idx = self
            .rows()
            .iter()
            .position(|r| matches!(r, TreeRow::Folder { name: n, .. } if n == name));
        if idx.is_some() {
            self.form.profiles.select(idx);
        }
    }

    /// Copy the selected connection into the form (folders load nothing).
    fn load_selected_profile(&mut self) {
        if let Some(p) = self.selected_profile().cloned() {
            self.form.apply(&p);
        }
    }

    fn toggle_folder(&mut self, name: &str, expand: Option<bool>) {
        let collapsed = self.form.collapsed.contains(name);
        match expand {
            Some(true) if !collapsed => return,
            Some(false) if collapsed => return,
            _ => {}
        }
        if collapsed {
            self.form.collapsed.remove(name);
        } else {
            self.form.collapsed.insert(name.to_string());
        }
        self.select_folder(name);
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    /// Cancel work and close the session; call before exiting.
    pub async fn shutdown(&mut self) {
        if let Some(t) = self.connect_task.take() {
            t.abort();
        }
        if let Some(mut b) = self.browser.take() {
            b.close();
            if let Some(w) = b.worker.take() {
                let _ = tokio::time::timeout(Duration::from_secs(2), w).await;
            }
        }
    }

    fn disconnect(&mut self) {
        if let Some(mut b) = self.browser.take() {
            self.log(LogLevel::Info, format!("Disconnected from {}", b.label));
            b.close(); // the worker finishes closing on its own
        }
        self.info("Disconnected");
    }

    // ───────── background events ─────────

    pub fn on_app_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Connected(Ok(session)) => self.on_connected(session),
            AppEvent::Connected(Err(message)) => {
                self.connect_task = None;
                self.log(LogLevel::Error, format!("Connection failed: {message}"));
                self.error("Connection failed", message);
            }
            AppEvent::Warn(message) => {
                self.log(LogLevel::Warn, message.clone());
                self.warn(message)
            }
            AppEvent::HostKey(q) => self.on_host_key(q),
            AppEvent::Listed { seq, path, result } => self.on_listed(seq, path, result),
            AppEvent::OpDone { what, result } => {
                match result {
                    Ok(()) => {
                        self.log(LogLevel::Ok, what.clone());
                        self.info(what)
                    }
                    Err(e) => {
                        self.log(LogLevel::Error, format!("{what}: {e}"));
                        self.error("Operation failed", e)
                    }
                }
                self.refresh_after(Side::Remote);
            }
            AppEvent::TransferDone(outcome) => self.on_transfer_done(outcome),
            AppEvent::ConnectionLost(message) => {
                if self.browser.take().is_some() {
                    self.log(LogLevel::Error, format!("Connection lost: {message}"));
                    self.error("Connection lost", message);
                }
            }
        }
    }

    fn on_connected(&mut self, session: Session) {
        self.connect_task = None;
        if matches!(
            self.modal,
            Some(Modal::Connecting { .. } | Modal::HostKey(_))
        ) {
            self.modal = None;
        }
        let local_cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.into_os_string().into_string().ok())
            .or_else(|| BaseDirs::new().and_then(|d| d.home_dir().to_str().map(str::to_owned)))
            .unwrap_or_else(|| "/".into());
        let home = session.home.clone();
        self.log(
            LogLevel::Ok,
            format!("Connected to {} ({})", session.label, session.detail),
        );
        self.log(LogLevel::Info, format!("Server default directory: {home}"));
        let start = self
            .connecting_path
            .take()
            .and_then(|p| resolve_start_path(&p, &home))
            .filter(|p| *p != home);
        if let Some(path) = &start {
            self.log(LogLevel::Info, format!("Opening default path {path}"));
        }
        let mut browser = BrowserState::new(
            self.connecting_label.clone(),
            self.connecting_color.take(),
            session,
            local_cwd.clone(),
        );

        let mut problem = browser.load_local(local_cwd, Keep::First).err();
        // Try the default path first; if it can't be opened the listing of the
        // server's own default directory follows, and the connection stays up.
        let remote = match start {
            Some(path) => browser.request_remote_or(path, Keep::First, Some(home)),
            None => browser.request_remote(home, Keep::First),
        };
        problem = problem.or(remote.err());
        self.browser = Some(browser);
        self.form.pass.set(String::new()); // don't keep the secret around
        if let Some(p) = problem {
            self.error("Listing failed", p);
        }
    }

    fn on_host_key(&mut self, q: HostKeyQuery) {
        if !matches!(self.modal, Some(Modal::Connecting { .. })) {
            let _ = q.reply.send(false); // the attempt was cancelled
            return;
        }
        self.log(
            LogLevel::Warn,
            format!(
                "Unknown host key for {}:{} ({} {})",
                q.host, q.port, q.algorithm, q.fingerprint
            ),
        );
        self.modal = Some(Modal::HostKey(HostKeyPrompt {
            host: q.host,
            port: q.port,
            algorithm: q.algorithm,
            fingerprint: q.fingerprint,
            reply: Some(q.reply),
        }));
    }

    fn on_listed(&mut self, seq: u64, path: String, result: Result<Vec<Entry>, String>) {
        let Some(b) = self.browser.as_mut() else {
            return;
        };
        let Some(pending) = b.pending.take_if(|p| p.seq == seq) else {
            return; // stale response from an older navigation
        };
        match result {
            Ok(raw) => {
                let quiet = matches!(pending.keep, Keep::Current);
                let line = format!("Remote ({} entries): {path}", raw.len());
                b.remote.set_listing(path, raw, b.show_hidden, pending.keep);
                // Refreshes after an operation would only repeat themselves.
                if !quiet {
                    self.log(LogLevel::Info, line);
                }
            }
            Err(e) => {
                b.remote.loading = false;
                if let Some(home) = pending.fallback {
                    let shown = home.clone();
                    let msg = format!("Could not open {path}: {e}. Showing {shown} instead");
                    match b.request_remote(home, Keep::First) {
                        Ok(()) => {
                            self.log(LogLevel::Warn, msg.clone());
                            self.warn(msg)
                        }
                        Err(e) => self.error("Cannot list directory", e),
                    }
                } else {
                    self.log(LogLevel::Error, format!("Cannot list {path}: {e}"));
                    self.error("Cannot list directory", format!("{path}: {e}"));
                }
            }
        }
    }

    fn on_transfer_done(&mut self, outcome: Outcome) {
        let Some(b) = self.browser.as_mut() else {
            return;
        };
        let Some(t) = b.transfer.take() else { return };
        let target = match t.plan.direction {
            Direction::Upload => Side::Remote,
            Direction::Download => Side::Local,
        };
        let verb = match t.plan.direction {
            Direction::Upload => "Uploaded",
            Direction::Download => "Downloaded",
        };
        match outcome {
            Outcome::Done => {
                let msg = format!(
                    "{verb} {} ({}) in {:.1}s",
                    t.plan.name,
                    format_size(t.ctl.done()),
                    t.started.elapsed().as_secs_f64()
                );
                self.log(LogLevel::Ok, msg.clone());
                self.info(msg);
            }
            Outcome::Cancelled => {
                self.log(LogLevel::Warn, format!("Cancelled {}", t.plan.name));
                self.warn(format!("Cancelled {}", t.plan.name))
            }
            Outcome::Failed(e) => {
                self.log(LogLevel::Error, format!("{} failed: {e}", t.plan.name));
                self.error("Transfer failed", format!("{}: {e}", t.plan.name))
            }
        }
        self.refresh_after(target);
    }

    fn refresh_after(&mut self, side: Side) {
        let Some(b) = self.browser.as_mut() else {
            return;
        };
        // A navigation already in flight was queued after the operation that just
        // finished, so its listing is fresh; refreshing would only cancel it.
        if side == Side::Remote && b.pending.is_some() {
            return;
        }
        if let Err(e) = b.refresh(side) {
            self.error("Refresh failed", e);
        }
    }

    // ───────── input ─────────

    pub fn on_paste(&mut self, text: &str) {
        match &mut self.modal {
            Some(Modal::Input { input, .. }) => input.insert_str(text),
            Some(_) => {}
            None if self.browser.is_none() => {
                if let Some(input) = self.form.input_mut() {
                    input.insert_str(text);
                }
            }
            None => {}
        }
    }

    fn request_quit(&mut self) {
        let message = match self.browser.as_ref() {
            Some(b) if b.transfer.is_some() => {
                "A transfer is running. Cancel it and quit remotefiletui?"
            }
            Some(_) => "Disconnect and quit remotefiletui?",
            None => "Quit remotefiletui?",
        };
        self.modal = Some(Modal::Confirm {
            message: message.into(),
            action: ConfirmAction::Quit,
        });
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        self.notice = None;
        self.g_prev = std::mem::take(&mut self.g_pending);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'q')) {
            // A second Ctrl-C on the quit prompt confirms it.
            if matches!(
                self.modal,
                Some(Modal::Confirm {
                    action: ConfirmAction::Quit,
                    ..
                })
            ) {
                self.should_quit = true;
            } else {
                self.request_quit();
            }
            return;
        }
        if let Some(modal) = self.modal.take() {
            self.modal = self.on_modal_key(modal, key);
            return;
        }
        if ctrl && key.code == KeyCode::Char('t') {
            self.open_theme_picker();
            return;
        }
        if self.log_focus {
            self.on_log_key(key);
            return;
        }
        if self.log_key(&key) {
            return;
        }
        if self.browser.is_some() {
            self.on_browser_key(key);
        } else {
            self.on_connect_key(key);
        }
    }

    // ───────── connect screen ─────────

    fn on_connect_key(&mut self, key: KeyEvent) {
        self.on_connect_key_inner(key);
        // Moving into the form makes the preview an edit.
        if self.form.focus != Focus::Profiles {
            self.form.previewing = false;
        }
    }

    fn on_connect_key_inner(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let has_profiles =
            !self.config.profiles.is_empty() || !self.config.folder_names().is_empty();
        self.form.error = None;

        // Keys that work in every field.
        match key.code {
            KeyCode::Char('s') if ctrl => return self.save_profile(),
            // In the saved list Ctrl-D pages down, as in vim; `d` deletes there.
            KeyCode::Char('d') if ctrl && self.form.focus != Focus::Profiles => {
                return self.start_delete();
            }
            KeyCode::Char('p') if ctrl => return self.open_password_manager_picker(),
            KeyCode::Char('l') if ctrl => return self.request(Next::New),
            KeyCode::Char('b') if ctrl => return self.focus_saved(),
            KeyCode::Char('n') if ctrl => return self.focus_details(),
            KeyCode::Enter => {
                if self.form.focus == Focus::Profiles
                    && let Some(folder) = self.selected_folder()
                {
                    return self.toggle_folder(&folder, None);
                }
                // In the list Enter connects to the highlighted connection (its
                // values are already previewed in the form); `e` edits it.
                if self.form.focus == Focus::Profiles
                    && let Some(name) = self.selected_profile().map(|p| p.name.clone())
                {
                    return self.request(Next::Connect(name));
                }
                return self.request(Next::ConnectForm);
            }
            // Tab walks the fields, then on to the log, then round again.
            KeyCode::Tab => {
                if self.log_visible && self.form.focus == self.form.edges(has_profiles).1 {
                    return self.enter_log();
                }
                return self.form.move_focus(true, has_profiles);
            }
            KeyCode::BackTab => {
                if self.log_visible && self.form.focus == self.form.edges(has_profiles).0 {
                    return self.enter_log();
                }
                return self.form.move_focus(false, has_profiles);
            }
            KeyCode::F(1) => {
                self.modal = Some(Modal::Help { scroll: 0 });
                return;
            }
            _ => {}
        }

        match self.form.focus {
            Focus::Profiles => self.on_tree_key(key),
            Focus::Protocol => match key.code {
                KeyCode::Left | KeyCode::Char('h') => {
                    let p = self.form.protocol.next(false);
                    self.form.set_protocol(p);
                }
                KeyCode::Right | KeyCode::Char('l' | ' ') => {
                    let p = self.form.protocol.next(true);
                    self.form.set_protocol(p);
                }
                KeyCode::Up | KeyCode::Char('k') => self.form.move_focus(false, has_profiles),
                KeyCode::Down | KeyCode::Char('j') => self.form.move_focus(true, has_profiles),
                KeyCode::Char('S') => self.focus_saved(),
                KeyCode::Char('C') => self.focus_details(),
                // Esc with no field selected stops editing: back to a new connection.
                KeyCode::Esc if self.editing().is_some() => self.request(Next::New),
                KeyCode::Char('n') => self.request(Next::New),
                // vim: i / a start typing in the first field.
                KeyCode::Char('i' | 'a') => self.focus_details(),
                KeyCode::Char('q') => self.request_quit(),
                KeyCode::Char('?') => self.modal = Some(Modal::Help { scroll: 0 }),
                _ => {}
            },
            _ => match key.code {
                KeyCode::Up => self.form.move_focus(false, has_profiles),
                KeyCode::Down => self.form.move_focus(true, has_profiles),
                // Esc leaves the text field; `?`, `q`, `1` and `2` work there.
                KeyCode::Esc => self.form.focus = Focus::Protocol,
                _ => {
                    if let Some(input) = self.form.input_mut() {
                        input.handle_key(key);
                    }
                }
            },
        }
    }

    /// Keys while the saved-connections tree has focus.
    fn on_tree_key(&mut self, key: KeyEvent) {
        let rows = self.rows().len();
        if rows > 0 {
            let cur = self.form.profiles.selected().unwrap_or(0);
            match nav(&key, self.g_prev, cur, rows - 1, 8) {
                Nav::To(i) => {
                    self.form.profiles.select(Some(i));
                    self.preview_selected();
                    return;
                }
                Nav::G => {
                    self.g_pending = true;
                    return;
                }
                Nav::No => {}
            }
        }
        match key.code {
            KeyCode::Right | KeyCode::Char('l') => {
                if let Some(f) = self.selected_folder() {
                    self.toggle_folder(&f, Some(true));
                }
            }
            KeyCode::Char(' ') => {
                if let Some(f) = self.selected_folder() {
                    self.toggle_folder(&f, None);
                }
            }
            KeyCode::Left | KeyCode::Char('h') => match self.selected_row() {
                // Collapse an open folder; from a closed one go up to its parent.
                Some(TreeRow::Folder {
                    name, collapsed, ..
                }) => match Config::folder_parent(&name).map(str::to_string) {
                    Some(parent) if collapsed => self.select_folder(&parent),
                    _ => self.toggle_folder(&name, Some(false)),
                },
                // From a connection inside a folder, go to the folder row.
                Some(TreeRow::Profile { index, .. }) => {
                    if let Some(f) = self.config.profiles[index].folder.clone() {
                        self.select_folder(&f);
                    }
                }
                None => {}
            },
            KeyCode::Char('n') => self.request(Next::New),
            KeyCode::Char('e') => self.start_edit(),
            KeyCode::Char('y') => self.request(Next::Copy),
            KeyCode::Char('f') => {
                let parent = self.context_folder();
                self.modal = Some(Modal::Input {
                    title: match &parent {
                        Some(p) => format!("New folder in {}", crate::util::sanitize(p)),
                        None => "New folder".into(),
                    },
                    input: TextInput::default(),
                    error: None,
                    action: InputAction::NewFolder {
                        move_profile: None,
                        parent,
                    },
                });
            }
            KeyCode::Char('m') => self.start_move(),
            KeyCode::Char('c') => self.start_color(),
            KeyCode::Char('t') => self.start_tags(),
            KeyCode::Char('/') => self.start_filter(),
            KeyCode::Esc if self.form.has_filter() => {
                self.set_filter(String::new(), BTreeSet::new())
            }
            KeyCode::Esc if self.editing().is_some() => self.request(Next::New),
            KeyCode::Char('r') => self.start_rename_saved(),
            KeyCode::Char('d') | KeyCode::Delete => self.start_delete(),
            KeyCode::Char('C') => self.focus_details(),
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('?') => self.modal = Some(Modal::Help { scroll: 0 }),
            _ => {}
        }
    }

    /// Start a blank connection in the form. The selected folder (if any) is
    /// where it will be filed when saved.
    fn new_connection(&mut self) {
        self.form.clear();
        self.form.focus = Focus::Host;
        self.info("New connection: fill in the form, Ctrl-S saves it");
    }

    /// Move into the form to change the selected saved connection.
    fn start_edit(&mut self) {
        match self.selected_profile().map(|p| p.name.clone()) {
            Some(name) => self.request(Next::Edit(name)),
            None => self.warn("Select a saved connection to edit"),
        }
    }

    fn edit_profile(&mut self, name: &str) {
        // Already editing this one: just go back to its fields.
        if self.editing().is_some_and(|p| p.name == name) {
            self.form.focus = Focus::Host;
            return;
        }
        let Some(p) = self
            .config
            .profiles
            .iter()
            .find(|p| p.name == name)
            .cloned()
        else {
            return;
        };
        self.form.apply(&p);
        self.form.focus = Focus::Host;
        self.info(format!(
            "Editing {name}: Ctrl-S saves your changes, Esc stops editing"
        ));
    }

    /// Copy the selected connection under a new name, ready to be edited.
    fn duplicate_profile(&mut self) {
        let Some(original) = self.selected_profile().cloned() else {
            return self.warn("Select a saved connection to copy");
        };
        let base = format!("{} copy", original.name);
        let mut name = base.clone();
        let mut n = 2;
        while self.config.has_profile(&name) {
            name = format!("{base} {n}");
            n += 1;
        }
        let mut copy = original;
        copy.name = name.clone();
        self.config.upsert(copy);
        self.select_profile(&name);
        self.load_selected_profile();
        self.form.focus = Focus::Name;
        self.persist(format!("Copied as {name}: rename or edit it, then Ctrl-S"));
    }

    fn open_password_manager_picker(&mut self) {
        // Index 0 is "none"; the supported managers follow.
        let index = self
            .config
            .password_manager
            .and_then(|m| PasswordManager::ALL.iter().position(|x| *x == m))
            .map_or(0, |i| i + 1);
        self.modal = Some(Modal::PasswordManager { index });
    }

    fn choose_password_manager(&mut self, choice: Option<PasswordManager>) {
        self.config.password_manager = choice;
        let msg = match choice {
            Some(m) => format!(
                "Password manager: {}. Empty details are filled from an item named after the connection",
                m.label()
            ),
            None => "No password manager: passwords are typed each time and never saved".into(),
        };
        let saved = if self.config_unreadable {
            Err(anyhow::anyhow!(
                "not saving: the existing config file could not be parsed. Fix or remove it, then restart"
            ))
        } else {
            self.config.save_password_manager(choice)
        };
        match saved {
            Ok(()) => self.info(msg),
            Err(e) => self.warn(format!("Applied for this session only: {e:#}")),
        }
    }

    fn focus_saved(&mut self) {
        if self.config.profiles.is_empty() && self.config.folder_names().is_empty() {
            self.warn("No saved connections yet (Ctrl-S saves the current form)");
        } else {
            self.form.focus = Focus::Profiles;
        }
    }

    fn focus_details(&mut self) {
        if matches!(self.form.focus, Focus::Profiles | Focus::Protocol) {
            self.form.focus = Focus::Host;
        }
    }

    /// Persist the config and report; failures are shown as a notice (a modal
    /// opened here would be discarded by the modal-key handler's return value).
    fn persist(&mut self, ok: impl Into<String>) {
        match self.save_config() {
            Ok(()) => self.info(ok),
            Err(e) => self.warn(format!("Not saved: {e:#}")),
        }
    }

    /// Ctrl-S. A connection loaded from the list is updated in place (a changed
    /// Name renames it); a new one asks what to call it.
    fn save_profile(&mut self) {
        if self.form.previewing {
            return self.warn("Press Enter (or e) to edit the highlighted connection first");
        }
        let mut profile = match self.form.to_profile(self.config.password_manager.is_some()) {
            Ok(p) => p,
            Err(e) => {
                self.after_save = None;
                return self.form.error = Some(e);
            }
        };
        if let Some(old) = self.editing().map(|p| p.name.clone()) {
            if !self.form_modified() {
                return self.info(format!("No changes to save in {old}"));
            }
            let name = profile.name.clone();
            if name != old
                && let Err(e) = self.config.rename_profile(&old, &name)
            {
                self.after_save = None;
                return self.form.error = Some(format!("Cannot rename: {e}"));
            }
            // Keeps the folder, colour and tags it already had.
            profile.folder = None;
            self.config.upsert(profile);
            self.form.loaded_from = Some(name.clone());
            self.form.name.set(name.clone());
            self.select_profile(&name);
            self.persist(format!("Updated {name} (passwords are never stored)"));
            return self.run_after_save();
        }
        profile.folder = self.context_folder();
        // The Name field, else the host.
        let suggestion = profile.name.clone();
        self.modal = Some(Modal::Input {
            title: "Save new connection as".into(),
            input: TextInput::new(suggestion),
            error: None,
            action: InputAction::SaveProfile(Box::new(profile)),
        });
    }

    fn start_delete(&mut self) {
        match self.selected_row() {
            Some(TreeRow::Profile { index, .. }) => {
                let name = self.config.profiles[index].name.clone();
                self.modal = Some(Modal::Confirm {
                    message: format!(
                        "Delete saved connection '{}'?",
                        crate::util::sanitize(&name)
                    ),
                    action: ConfirmAction::DeleteProfile(name),
                });
            }
            Some(TreeRow::Folder { name, count, .. }) => {
                let tail = match count {
                    0 => String::new(),
                    1 => " Its contents move up a level.".into(),
                    n => format!(" Its {n} connections move up a level."),
                };
                self.modal = Some(Modal::Confirm {
                    message: format!("Delete folder '{}'?{tail}", crate::util::sanitize(&name)),
                    action: ConfirmAction::DeleteFolder(name),
                });
            }
            None => self.warn("Select a saved connection or folder first"),
        }
    }

    fn start_move(&mut self) {
        let (name, current, is_folder) = match self.selected_row() {
            Some(TreeRow::Profile { index, .. }) => {
                let p = &self.config.profiles[index];
                (p.name.clone(), p.folder.clone(), false)
            }
            Some(TreeRow::Folder { name, .. }) => {
                let parent = Config::folder_parent(&name).map(str::to_string);
                (name, parent, true)
            }
            None => return self.warn("Select a saved connection or folder to move"),
        };
        let mut choices = vec![FolderChoice::TopLevel];
        choices.extend(
            self.config
                .folder_names()
                .into_iter()
                // A folder can't go into itself or anything inside it.
                .filter(|f| !is_folder || !Config::folder_within(f, &name))
                .map(FolderChoice::Folder),
        );
        if !is_folder {
            choices.push(FolderChoice::New);
        }
        let index = choices
            .iter()
            .position(|c| match (c, &current) {
                (FolderChoice::TopLevel, None) => true,
                (FolderChoice::Folder(f), Some(cur)) => f == cur,
                _ => false,
            })
            .unwrap_or(0);
        self.modal = Some(Modal::Folders {
            choices,
            index,
            profile: name,
            folder: is_folder,
        });
    }

    fn start_color(&mut self) {
        let Some(p) = self.selected_profile() else {
            return self.warn("Select a saved connection to colour");
        };
        let name = p.name.clone();
        let index = p
            .color
            .as_deref()
            .and_then(|c| COLORS.iter().position(|x| *x == c))
            .map_or(0, |i| i + 1);
        self.modal = Some(Modal::Colors {
            profile: name,
            index,
        });
    }

    fn start_tags(&mut self) {
        let Some(p) = self.selected_profile() else {
            return self.warn("Select a saved connection to tag");
        };
        self.modal = Some(Modal::Input {
            title: "Tags (comma or space separated)".into(),
            input: TextInput::new(p.tags.join(", ")),
            error: None,
            action: InputAction::EditTags(p.name.clone()),
        });
    }

    /// Open the filter: a text field, and the tags in use to pick from.
    fn start_filter(&mut self) {
        let tags = self.config.all_tags();
        let focus = FilterFocus::Text;
        self.modal = Some(Modal::Filter {
            text: TextInput::new(self.form.filter_text.clone()),
            tags,
            selected: self.form.filter_tags.clone(),
            index: 0,
            focus,
        });
    }

    /// Replace the filter and put the selection on the first visible row.
    fn set_filter(&mut self, text: String, tags: BTreeSet<String>) {
        self.form.filter_text = text.trim().to_string();
        self.form.filter_tags = tags;
        let n = self.rows().len();
        self.form.profiles.select((n > 0).then_some(0));
        // Land on a connection rather than a folder.
        if let Some(i) = self
            .rows()
            .iter()
            .position(|r| matches!(r, TreeRow::Profile { .. }))
        {
            self.form.profiles.select(Some(i));
        }
        self.preview_selected();
        if !self.form.has_filter() {
            self.info("Filter cleared");
        } else if n == 0 {
            self.warn(format!("Nothing matches {}", self.form.filter_summary()));
        }
    }

    fn start_rename_saved(&mut self) {
        let (title, current, action) = match self.selected_row() {
            Some(TreeRow::Profile { index, .. }) => {
                let name = self.config.profiles[index].name.clone();
                (
                    "Rename connection",
                    name.clone(),
                    InputAction::RenameProfile(name),
                )
            }
            Some(TreeRow::Folder { name, .. }) => (
                "Rename folder",
                Config::folder_leaf(&name).to_string(),
                InputAction::RenameFolder(name),
            ),
            None => return self.warn("Select a saved connection or folder first"),
        };
        self.modal = Some(Modal::Input {
            title: title.into(),
            input: TextInput::new(current),
            error: None,
            action,
        });
    }

    fn start_connect(&mut self) {
        let values = self.form.values();
        let op = (self.config.password_manager == Some(PasswordManager::OnePassword)).then(|| {
            onepassword::Settings {
                vault: self.config.onepassword_vault.clone(),
            }
        });
        // Without 1Password everything must already be filled in, so say what's
        // wrong right here. With it, validation happens after the lookup.
        if op.is_none()
            && let Err(e) = values.build()
        {
            return self.form.error = Some(e);
        }

        let name = values.effective_name();
        self.connecting_path = Some(values.path.clone()).filter(|p| !p.trim().is_empty());
        // A saved connection's colour follows it into the session.
        let saved = self
            .form
            .loaded_from
            .clone()
            .filter(|n| self.config.has_profile(n))
            .unwrap_or_else(|| name.clone());
        self.connecting_color = self
            .config
            .profiles
            .iter()
            .find(|q| q.name == saved)
            .and_then(|q| q.color.clone());

        if let Some(old) = self.connect_task.take() {
            old.abort();
        }
        self.connect_task = Some(worker::spawn_connect(values, op, self.tx.clone()));
        let label = if name.is_empty() {
            "the server".to_string()
        } else {
            name
        };
        self.connecting_label = label.clone();
        self.log(
            LogLevel::Info,
            format!(
                "Connecting to {label} ({} {}:{})",
                self.form.protocol.scheme(),
                self.form.host.value().trim(),
                self.form.port.value().trim()
            ),
        );
        self.modal = Some(Modal::Connecting { label });
    }

    // ───────── modals ─────────

    fn on_modal_key(&mut self, modal: Modal, key: KeyEvent) -> Option<Modal> {
        match modal {
            Modal::Help { scroll } => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let to = |scroll| Some(Modal::Help { scroll });
                match key.code {
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | '?') | KeyCode::F(1) => {
                        None
                    }
                    KeyCode::Up | KeyCode::Char('k') => to(scroll.saturating_sub(1)),
                    KeyCode::Down | KeyCode::Char('j') => to(scroll.saturating_add(1)),
                    KeyCode::PageUp => to(scroll.saturating_sub(10)),
                    KeyCode::PageDown => to(scroll.saturating_add(10)),
                    KeyCode::Char('u') if ctrl => to(scroll.saturating_sub(8)),
                    KeyCode::Char('d') if ctrl => to(scroll.saturating_add(8)),
                    KeyCode::Char('b') if ctrl => to(scroll.saturating_sub(16)),
                    KeyCode::Char('f') if ctrl => to(scroll.saturating_add(16)),
                    KeyCode::Home => to(0),
                    KeyCode::Char('g') => {
                        if self.g_prev {
                            to(0)
                        } else {
                            self.g_pending = true;
                            to(scroll)
                        }
                    }
                    // The renderer clamps this to the real maximum.
                    KeyCode::End | KeyCode::Char('G') => to(u16::MAX),
                    _ => to(scroll),
                }
            }
            Modal::Colors { profile, index } => {
                let last = COLORS.len(); // index 0 is "no colour"
                match nav(&key, self.g_prev, index, last, 4) {
                    Nav::To(index) => return Some(Modal::Colors { profile, index }),
                    Nav::G => {
                        self.g_pending = true;
                        return Some(Modal::Colors { profile, index });
                    }
                    Nav::No => {}
                }
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => None,
                    KeyCode::Enter => {
                        let color = index.checked_sub(1).map(|i| COLORS[i]);
                        self.config.set_color(&profile, color);
                        self.select_profile(&profile);
                        self.persist(match color {
                            Some(c) => format!("{profile}: colour set to {c}"),
                            None => format!("{profile}: colour removed"),
                        });
                        None
                    }
                    _ => Some(Modal::Colors { profile, index }),
                }
            }
            Modal::Filter {
                mut text,
                tags,
                mut selected,
                mut index,
                mut focus,
            } => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let mut apply = false;
                match (focus, key.code) {
                    (_, KeyCode::Enter) => apply = true,
                    (_, KeyCode::Esc) => return None,
                    (FilterFocus::Tags, KeyCode::Char('q')) => return None,
                    (FilterFocus::Text, KeyCode::Tab | KeyCode::Down) if !tags.is_empty() => {
                        focus = FilterFocus::Tags;
                    }
                    (FilterFocus::Text, KeyCode::Tab | KeyCode::Down | KeyCode::BackTab) => {}
                    (FilterFocus::Text, _) => {
                        text.handle_key(key);
                    }
                    // vim: `/` goes back to typing; Tab / Shift-Tab cycle.
                    (
                        FilterFocus::Tags,
                        KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('/' | 'i'),
                    ) => {
                        focus = FilterFocus::Text;
                    }
                    (FilterFocus::Tags, KeyCode::Char(' ' | 'x')) if !tags.is_empty() => {
                        let t = tags[index].0.clone();
                        if !selected.remove(&t) {
                            selected.insert(t);
                        }
                    }
                    (FilterFocus::Tags, KeyCode::Char('c')) if !ctrl => {
                        selected.clear();
                        text.set(String::new());
                    }
                    (FilterFocus::Tags, _) if !tags.is_empty() => {
                        match nav(&key, self.g_prev, index, tags.len() - 1, 8) {
                            Nav::To(i) => index = i,
                            Nav::G => self.g_pending = true,
                            Nav::No => {}
                        }
                    }
                    (FilterFocus::Tags, _) => {}
                }
                if apply {
                    self.set_filter(text.value().to_string(), selected);
                    return None;
                }
                Some(Modal::Filter {
                    text,
                    tags,
                    selected,
                    index,
                    focus,
                })
            }
            Modal::Folders {
                choices,
                index,
                profile,
                folder,
            } => {
                let keep = |choices, index, profile| {
                    Some(Modal::Folders {
                        choices,
                        index,
                        profile,
                        folder,
                    })
                };
                match nav(&key, self.g_prev, index, choices.len() - 1, 6) {
                    Nav::To(index) => return keep(choices, index, profile),
                    Nav::G => {
                        self.g_pending = true;
                        return keep(choices, index, profile);
                    }
                    Nav::No => {}
                }
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => None,
                    KeyCode::Enter => match choices[index].clone() {
                        FolderChoice::New => Some(Modal::Input {
                            title: "New folder".into(),
                            input: TextInput::default(),
                            error: None,
                            action: InputAction::NewFolder {
                                move_profile: Some(profile),
                                parent: None,
                            },
                        }),
                        choice => {
                            let dest = match &choice {
                                FolderChoice::Folder(f) => Some(f.as_str()),
                                _ => None,
                            };
                            if folder {
                                match self.config.move_folder(&profile, dest) {
                                    Ok(new) => {
                                        self.remap_collapsed(&profile, &new);
                                        self.select_folder(&new);
                                        self.persist(format!("Moved folder to {new}"));
                                    }
                                    Err(e) => self.warn(e),
                                }
                                return None;
                            }
                            self.config.set_folder(&profile, dest);
                            self.select_profile(&profile);
                            let msg = match dest {
                                Some(f) => format!("Moved {profile} to {f}"),
                                None => format!("Moved {profile} to the top level"),
                            };
                            self.persist(msg);
                            None
                        }
                    },
                    _ => keep(choices, index, profile),
                }
            }
            Modal::Themes {
                index,
                original,
                original_transparent,
            } => {
                let last = self.themes.len() - 1;
                let keep = |index| Modal::Themes {
                    index,
                    original,
                    original_transparent,
                };
                match nav(&key, self.g_prev, index, last, 6) {
                    Nav::To(i) => {
                        self.set_theme(i);
                        return Some(keep(i));
                    }
                    Nav::G => {
                        self.g_pending = true;
                        return Some(keep(index));
                    }
                    Nav::No => {}
                }
                match key.code {
                    // Transparent background, previewed live.
                    KeyCode::Char('b') | KeyCode::Char(' ') => {
                        self.transparent = !self.transparent;
                        self.apply_theme();
                        Some(keep(index))
                    }
                    KeyCode::Enter => {
                        let name = self.themes[index].name.clone();
                        self.config.theme = Some(name.clone());
                        self.config.transparent = self.transparent;
                        let msg = if self.transparent {
                            format!("Theme: {name} (transparent background)")
                        } else {
                            format!("Theme: {name}")
                        };
                        let saved = self
                            .save_setting("theme", &crate::config::yaml_scalar(&name))
                            .and_then(|()| {
                                self.save_setting(
                                    "transparent_background",
                                    &self.transparent.to_string(),
                                )
                            });
                        match saved {
                            Ok(()) => self.info(msg),
                            Err(e) => {
                                self.warn(format!("Theme applied for this session only: {e:#}"))
                            }
                        }
                        None
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.transparent = original_transparent;
                        self.set_theme(original);
                        None
                    }
                    _ => Some(keep(index)),
                }
            }
            Modal::Unsaved { message, next } => match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.after_save = Some(next);
                    self.save_profile();
                    // Saving may have opened the name prompt, or carried on to
                    // `next` (which may have opened a dialog of its own).
                    self.modal.take()
                }
                KeyCode::Char('n' | 'N') => {
                    self.perform(next);
                    self.modal.take()
                }
                KeyCode::Esc | KeyCode::Char('c' | 'C') => None,
                _ => Some(Modal::Unsaved { message, next }),
            },
            Modal::PasswordManager { index } => {
                let last = PasswordManager::ALL.len();
                match nav(&key, self.g_prev, index, last, 4) {
                    Nav::To(index) => return Some(Modal::PasswordManager { index }),
                    Nav::G => {
                        self.g_pending = true;
                        return Some(Modal::PasswordManager { index });
                    }
                    Nav::No => {}
                }
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => None,
                    KeyCode::Enter => {
                        let choice = index.checked_sub(1).map(|i| PasswordManager::ALL[i]);
                        self.choose_password_manager(choice);
                        None
                    }
                    _ => Some(Modal::PasswordManager { index }),
                }
            }
            Modal::Error { title, message } => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | ' ') => None,
                _ => Some(Modal::Error { title, message }),
            },
            Modal::Connecting { label } => {
                if key.code == KeyCode::Esc {
                    if let Some(t) = self.connect_task.take() {
                        t.abort();
                    }
                    None
                } else {
                    Some(Modal::Connecting { label })
                }
            }
            Modal::HostKey(mut prompt) => match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.log(
                        LogLevel::Info,
                        format!("Trusted host key for {}", prompt.host),
                    );
                    prompt.answer(true);
                    Some(Modal::Connecting {
                        label: self.connecting_label.clone(),
                    })
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.log(
                        LogLevel::Warn,
                        format!("Rejected host key for {}", prompt.host),
                    );
                    prompt.answer(false);
                    Some(Modal::Connecting {
                        label: self.connecting_label.clone(),
                    })
                }
                _ => Some(Modal::HostKey(prompt)),
            },
            Modal::Input {
                title,
                mut input,
                action,
                ..
            } => match key.code {
                KeyCode::Esc => {
                    self.after_save = None;
                    None
                }
                KeyCode::Enter => {
                    let value = input.value().trim().to_string();
                    match self.submit_input(&action, &value) {
                        // The action may have opened another dialog.
                        Ok(()) => self.modal.take(),
                        Err(e) => Some(Modal::Input {
                            title,
                            input,
                            error: Some(e),
                            action,
                        }),
                    }
                }
                _ => {
                    input.handle_key(key);
                    Some(Modal::Input {
                        title,
                        input,
                        error: None,
                        action,
                    })
                }
            },
            Modal::Confirm { message, action } => match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.run_confirmed(action);
                    // run_confirmed may have opened an error modal.
                    self.modal.take()
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => None,
                _ => Some(Modal::Confirm { message, action }),
            },
        }
    }

    fn submit_input(&mut self, action: &InputAction, value: &str) -> Result<(), String> {
        match action {
            InputAction::NewFolder {
                move_profile,
                parent,
            } => {
                let name = self
                    .config
                    .add_folder_in(parent.as_deref(), value)
                    .map_err(str::to_string)?;
                self.expand_to(&name);
                if let Some(p) = move_profile {
                    self.config.set_folder(p, Some(&name));
                    self.select_profile(p);
                } else {
                    self.select_folder(&name);
                }
                self.persist(format!("Created folder {name}"));
                return Ok(());
            }
            InputAction::RenameFolder(old) => {
                let new = self
                    .config
                    .rename_folder(old, value)
                    .map_err(str::to_string)?;
                self.remap_collapsed(old, &new);
                self.select_folder(&new);
                self.persist(format!("Renamed folder to {}", Config::folder_leaf(&new)));
                return Ok(());
            }
            InputAction::RenameProfile(old) => {
                self.config
                    .rename_profile(old, value)
                    .map_err(str::to_string)?;
                if self.form.loaded_from.as_deref() == Some(old.as_str()) {
                    self.form.loaded_from = Some(value.trim().to_string());
                }
                if self.form.name.value().trim() == old.as_str() {
                    self.form.name.set(value.trim().to_string());
                }
                self.select_profile(value.trim());
                self.persist(format!("Renamed to {value}"));
                return Ok(());
            }
            InputAction::SaveProfile(profile) => {
                let name = Config::clean_profile_name(value).map_err(str::to_string)?;
                // Updating the entry being edited is fine; taking over another
                // entry's name is not (rename or delete that one first).
                let editing = self.form.loaded_from.as_deref() == Some(name.as_str());
                if self.config.has_profile(&name) && !editing {
                    return Err(format!(
                        "'{name}' is already used by another saved connection"
                    ));
                }
                let mut profile = (**profile).clone();
                profile.name = name.clone();
                self.config.upsert(profile);
                // The saved name is now this connection's name.
                self.form.loaded_from = Some(name.clone());
                self.form.name.set(name.clone());
                self.select_profile(&name);
                self.persist(format!("Saved {name} (passwords are never stored)"));
                self.run_after_save();
                return Ok(());
            }
            InputAction::EditTags(name) => {
                let tags = clean_tags(value)?;
                self.config.set_tags(name, tags.clone());
                self.select_profile(name);
                self.persist(if tags.is_empty() {
                    format!("{name}: tags cleared")
                } else {
                    format!("{name}: tags {}", tags.join(", "))
                });
                return Ok(());
            }
            InputAction::Mkdir(_) | InputAction::Rename(..) => {}
        }
        validate_name(value).map_err(str::to_string)?;
        let Some(b) = self.browser.as_mut() else {
            return Ok(());
        };
        match action {
            InputAction::Mkdir(side) => {
                let path = b.dir_path(*side, value);
                match side {
                    Side::Local => {
                        std::fs::create_dir(&path).map_err(|e| format!("{path}: {e}"))?;
                        b.refresh(Side::Local)?;
                        self.log(LogLevel::Ok, format!("Created local {path}"));
                        self.info(format!("Created {path}"));
                    }
                    Side::Remote => b.send(Cmd::Mkdir { path })?,
                }
            }
            InputAction::NewFolder { .. }
            | InputAction::RenameFolder(_)
            | InputAction::RenameProfile(_)
            | InputAction::EditTags(_)
            | InputAction::SaveProfile(_) => unreachable!("handled above"),
            InputAction::Rename(side, old) => {
                if value == old {
                    return Ok(());
                }
                let from = b.dir_path(*side, old);
                let to = b.dir_path(*side, value);
                match side {
                    Side::Local => {
                        if Path::new(&to).symlink_metadata().is_ok() {
                            return Err(format!("{value} already exists"));
                        }
                        std::fs::rename(&from, &to).map_err(|e| format!("{e}"))?;
                        let cwd = b.local.cwd.clone();
                        b.load(Side::Local, cwd, Keep::Name(value.to_string()))?;
                        self.log(LogLevel::Ok, format!("Renamed local {from} to {to}"));
                        self.info(format!("Renamed to {value}"));
                    }
                    Side::Remote => {
                        if b.remote.view.iter().any(|e| e.name == value) {
                            return Err(format!("{value} already exists"));
                        }
                        b.send(Cmd::Rename { from, to })?;
                    }
                }
            }
        }
        Ok(())
    }

    fn run_confirmed(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::Disconnect => self.disconnect(),
            ConfirmAction::Quit => self.should_quit = true,
            ConfirmAction::DeleteProfile(name) => {
                self.config.remove(&name);
                // Deleting the connection being edited leaves a blank new one.
                if self.form.loaded_from.as_deref() == Some(name.as_str()) {
                    self.form.clear();
                }
                self.clamp_selection();
                self.preview_selected();
                self.persist(format!("Deleted {name}"));
            }
            ConfirmAction::DeleteFolder(name) => {
                self.config.remove_folder(&name);
                self.form.collapsed.remove(&name);
                self.clamp_selection();
                self.persist(format!("Deleted folder {name}"));
            }
            ConfirmAction::Overwrite(plan) => {
                let line = transfer_start_line(&plan);
                let Some(b) = self.browser.as_mut() else {
                    return;
                };
                match b.begin_transfer(plan) {
                    Ok(()) => self.log(LogLevel::Info, line),
                    Err(e) => self.error("Cannot start transfer", e),
                }
            }
            ConfirmAction::Delete { side, entry } => {
                let Some(b) = self.browser.as_mut() else {
                    return;
                };
                let path = b.dir_path(side, &entry.name);
                // Never rmdir a symlink: unlink it instead.
                let as_dir = entry.is_dir() && !entry.is_link;
                match side {
                    Side::Local => {
                        let result = if as_dir {
                            std::fs::remove_dir(&path)
                        } else {
                            std::fs::remove_file(&path)
                        };
                        match result {
                            Ok(()) => {
                                let _ = b.refresh(Side::Local);
                                self.log(LogLevel::Ok, format!("Deleted local {path}"));
                                self.info(format!("Deleted {path}"));
                            }
                            Err(e) => {
                                let hint = if as_dir {
                                    " (only empty directories can be deleted)"
                                } else {
                                    ""
                                };
                                self.error("Delete failed", format!("{path}: {e}{hint}"));
                            }
                        }
                    }
                    Side::Remote => {
                        let cmd = if as_dir {
                            Cmd::RemoveDir { path }
                        } else {
                            Cmd::RemoveFile { path }
                        };
                        if let Err(e) = b.send(cmd) {
                            self.error("Delete failed", e);
                        }
                    }
                }
            }
        }
    }

    // ───────── browser screen ─────────

    fn on_browser_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(b) = self.browser.as_mut() else {
            return;
        };
        let page = b.pane(b.active).page.max(1) as isize;
        let side = b.active;

        let n = b.pane(side).view.len();
        if n > 0 {
            let cur = b.pane(side).state.selected().unwrap_or(0);
            match nav(&key, self.g_prev, cur, n - 1, page as usize) {
                Nav::To(i) => {
                    b.pane_mut(side).state.select(Some(i));
                    return;
                }
                Nav::G => {
                    self.g_pending = true;
                    return;
                }
                Nav::No => {}
            }
        }

        let local_before = b.local.cwd.clone();
        let mut result: Result<(), String> = Ok(());
        match key.code {
            // Local -> Remote -> log -> Local.
            KeyCode::Tab | KeyCode::BackTab => {
                let to_log = self.log_visible
                    && match key.code {
                        KeyCode::Tab => side == Side::Remote,
                        _ => side == Side::Local,
                    };
                if to_log {
                    return self.enter_log();
                }
                b.active = side.other();
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => result = b.open_selected(),
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h' | '-') => result = b.go_up(),
            KeyCode::Char('r') if ctrl => result = b.refresh(side),
            KeyCode::Char('R') => result = b.refresh(side),
            KeyCode::Char('.') => {
                b.show_hidden = !b.show_hidden;
                let hidden = b.show_hidden;
                b.local.rebuild(hidden, Keep::Current);
                b.remote.rebuild(hidden, Keep::Current);
            }
            KeyCode::Esc => {
                if let Some(t) = &b.transfer {
                    t.ctl.cancel();
                    self.info("Cancelling transfer…");
                }
            }
            KeyCode::Char('c') | KeyCode::F(5) => return self.start_copy(),
            KeyCode::Char('m') | KeyCode::F(7) => return self.prompt_mkdir(),
            KeyCode::Char('r') | KeyCode::F(2) => return self.prompt_rename(),
            KeyCode::Char('x') | KeyCode::Delete | KeyCode::F(8) => return self.prompt_delete(),
            KeyCode::Char('t') => self.open_theme_picker(),
            KeyCode::Char('?') | KeyCode::F(1) => self.modal = Some(Modal::Help { scroll: 0 }),
            KeyCode::Char('q') => {
                if b.transfer.is_some() {
                    self.modal = Some(Modal::Confirm {
                        message: "A transfer is running. Cancel it and disconnect?".into(),
                        action: ConfirmAction::Disconnect,
                    });
                } else {
                    self.disconnect();
                }
            }
            _ => {}
        }
        // Local directories load immediately; remote ones are logged when they arrive.
        if side == Side::Local
            && result.is_ok()
            && let Some(b) = self.browser.as_ref()
            && b.local.cwd != local_before
        {
            let line = format!("Local ({} entries): {}", b.local.raw.len(), b.local.cwd);
            self.log(LogLevel::Info, line);
        }
        if let Err(e) = result {
            self.log(LogLevel::Error, e.clone());
            self.error("Error", e);
        }
    }

    fn start_copy(&mut self) {
        let Some(b) = self.browser.as_mut() else {
            return;
        };
        match b.plan_transfer() {
            Err(e) => self.warn(e),
            Ok((plan, true)) => {
                self.modal = Some(Modal::Confirm {
                    message: format!(
                        "{} already exists. Overwrite it?",
                        crate::util::sanitize(&plan.name)
                    ),
                    action: ConfirmAction::Overwrite(plan),
                });
            }
            Ok((plan, false)) => {
                let line = transfer_start_line(&plan);
                match b.begin_transfer(plan) {
                    Ok(()) => self.log(LogLevel::Info, line),
                    Err(e) => self.error("Cannot start transfer", e),
                }
            }
        }
    }

    fn prompt_mkdir(&mut self) {
        let Some(b) = self.browser.as_ref() else {
            return;
        };
        self.modal = Some(Modal::Input {
            title: "New directory".into(),
            input: TextInput::default(),
            error: None,
            action: InputAction::Mkdir(b.active),
        });
    }

    fn prompt_rename(&mut self) {
        let Some(b) = self.browser.as_ref() else {
            return;
        };
        let side = b.active;
        match b.pane(side).selected() {
            Some(e) if !e.is_parent_marker() => {
                self.modal = Some(Modal::Input {
                    title: "Rename".into(),
                    input: TextInput::new(e.name.clone()),
                    error: None,
                    action: InputAction::Rename(side, e.name.clone()),
                });
            }
            _ => self.warn("Select something to rename"),
        }
    }

    fn prompt_delete(&mut self) {
        let Some(b) = self.browser.as_ref() else {
            return;
        };
        let side = b.active;
        match b.pane(side).selected() {
            Some(e) if !e.is_parent_marker() => {
                let what = if e.is_dir() && !e.is_link {
                    "directory"
                } else {
                    "file"
                };
                self.modal = Some(Modal::Confirm {
                    message: format!("Delete {what} '{}'?", crate::util::sanitize(&e.name)),
                    action: ConfirmAction::Delete {
                        side,
                        entry: e.clone(),
                    },
                });
            }
            _ => self.warn("Select something to delete"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::EntryKind;

    fn entry(name: &str, kind: EntryKind) -> Entry {
        Entry {
            name: name.into(),
            kind,
            is_link: false,
            size: 1,
            modified: None,
            mode: None,
        }
    }

    fn pane() -> Pane {
        let mut p = Pane::new(Side::Remote, "/data".into());
        p.set_listing(
            "/data".into(),
            vec![
                entry("zeta.txt", EntryKind::File),
                entry("Alpha", EntryKind::Dir),
                entry(".hidden", EntryKind::File),
                entry("beta.txt", EntryKind::File),
                entry("docs", EntryKind::Dir),
            ],
            false,
            Keep::First,
        );
        p
    }

    fn names(p: &Pane) -> Vec<&str> {
        p.view.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn sorts_dirs_first_hides_dotfiles_and_adds_parent() {
        let p = pane();
        assert_eq!(names(&p), ["..", "Alpha", "docs", "beta.txt", "zeta.txt"]);
        assert_eq!(p.state.selected(), Some(0));
    }

    #[test]
    fn root_has_no_parent_row() {
        let mut p = Pane::new(Side::Remote, "/".into());
        p.set_listing(
            "/".into(),
            vec![entry("a", EntryKind::File)],
            false,
            Keep::First,
        );
        assert_eq!(names(&p), ["a"]);
    }

    #[test]
    fn toggling_hidden_keeps_selection() {
        let mut p = pane();
        p.state.select(Some(4)); // zeta.txt
        p.rebuild(true, Keep::Current);
        assert_eq!(p.selected().unwrap().name, "zeta.txt");
        assert!(names(&p).contains(&".hidden"));
    }

    #[test]
    fn keep_name_and_clamping() {
        let mut p = pane();
        p.rebuild(false, Keep::Name("docs".into()));
        assert_eq!(p.selected().unwrap().name, "docs");
        p.move_by(100);
        assert_eq!(p.selected().unwrap().name, "zeta.txt");
        p.move_by(-100);
        assert_eq!(p.selected().unwrap().name, "..");
    }

    #[test]
    fn empty_listing_has_no_selection_at_root() {
        let mut p = Pane::new(Side::Remote, "/".into());
        p.set_listing("/".into(), vec![], false, Keep::First);
        assert_eq!(p.state.selected(), None);
        p.move_by(1); // must not panic
    }

    #[test]
    fn form_validation() {
        let mut f = ConnectState::new(false);
        assert!(f.build_params().is_err()); // no host
        f.host.set("example.com");
        assert!(f.build_params().is_err()); // sftp needs a user
        f.user.set("me");
        assert!(f.build_params().is_ok());
        f.port.set("0");
        assert!(f.build_params().is_err());
        f.port.set("70000");
        assert!(f.build_params().is_err());
    }

    #[test]
    fn protocol_change_moves_default_port_only() {
        let mut f = ConnectState::new(false);
        f.set_protocol(Protocol::Ftp);
        assert_eq!(f.port.value(), "21");
        f.port.set("2121");
        f.set_protocol(Protocol::Sftp);
        assert_eq!(f.port.value(), "2121");
    }

    #[test]
    fn profile_never_contains_password() {
        let mut f = ConnectState::new(false);
        f.host.set("h");
        f.user.set("u");
        f.pass.set("hunter2");
        let p = f.to_profile(false).unwrap();
        assert_eq!(p.name, "h", "the name falls back to the host");
        assert!(!format!("{p:?}").contains("hunter2"));
    }

    #[test]
    fn s3_form_saves_without_the_secret_and_names_the_target() {
        let mut f = ConnectState::new(false);
        f.set_protocol(Protocol::S3);
        f.host.set("http://127.0.0.1:5000");
        f.region.set("eu-west-1");
        f.bucket.set("my-bucket");
        f.user.set("AKIAEXAMPLE");
        // Secret empty (as after a connect): saving works, connecting says why not.
        let p = f.to_profile(false).unwrap();
        assert_eq!(p.name, "my-bucket", "S3 falls back to the bucket");
        assert_eq!(p.region.as_deref(), Some("eu-west-1"));
        assert_eq!(p.bucket.as_deref(), Some("my-bucket"));
        assert!(f.build_params().is_err());
        f.pass.set("secret-value");
        let params = f.build_params().unwrap();
        assert_eq!(params.bucket, "my-bucket");
        assert!(!format!("{p:?}").contains("secret-value"));
        // All-buckets, default endpoint, credential chain: nothing required.
        let mut g = ConnectState::new(false);
        g.set_protocol(Protocol::S3);
        assert_eq!(g.to_profile(false).unwrap().name, "amazon-s3");
        assert!(g.build_params().is_ok());
    }

    #[test]
    fn s3_focus_order_has_no_port_or_key_file() {
        let mut f = ConnectState::new(false);
        f.set_protocol(Protocol::S3);
        let o = f.order();
        assert!(
            o.contains(&Focus::Region)
                && o.contains(&Focus::Bucket)
                && o.contains(&Focus::AwsProfile)
        );
        assert!(!o.contains(&Focus::Port) && !o.contains(&Focus::Key));
        // Switching away from S3 moves focus off a field that no longer exists.
        f.focus = Focus::AwsProfile;
        f.set_protocol(Protocol::Ftp);
        assert_eq!(f.focus, Focus::Protocol);
    }

    #[test]
    fn name_field_overrides_the_host_and_is_validated() {
        let mut f = ConnectState::new(false);
        f.host.set("alpha.example.com");
        f.user.set("me");
        assert_eq!(f.to_profile(false).unwrap().name, "alpha.example.com");
        f.name.set("  Prod SFTP ");
        assert_eq!(f.to_profile(false).unwrap().name, "Prod SFTP");
        f.name.set("x".repeat(81));
        assert!(f.to_profile(false).is_err(), "over-long names are refused");
        f.name.set("");
        f.host.set("");
        assert!(
            f.to_profile(false).is_err(),
            "no name and no host: nothing to call it"
        );
        // The Name field is part of the focus order, right after the saved list.
        let o = f.order();
        assert_eq!(&o[..3], [Focus::Profiles, Focus::Name, Focus::Protocol]);
    }

    #[test]
    fn loading_a_saved_connection_fills_the_name() {
        let mut f = ConnectState::new(false);
        let p = Profile {
            name: "My Server".into(),
            host: "h.example.com".into(),
            username: "u".into(),
            port: 22,
            ..Profile::default()
        };
        f.apply(&p);
        assert_eq!(f.name.value(), "My Server");
        assert_eq!(f.values().effective_name(), "My Server");
        assert_eq!(f.loaded_from.as_deref(), Some("My Server"));
    }

    // ───────── editing, filtering, default path, vim motions ─────────

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    /// An app whose config can never be written to disk (the unreadable-config
    /// guard), so these tests cannot touch the real config.
    fn test_app(profiles: Vec<Profile>) -> App {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = Config::default();
        for p in profiles {
            config.upsert(p);
        }
        let mut app = App::new(tx, config, Some("test".into()));
        app.modal = None;
        app
    }

    fn saved(name: &str, host: &str) -> Profile {
        Profile {
            name: name.into(),
            protocol: Protocol::Sftp,
            host: host.into(),
            port: 22,
            username: "me".into(),
            ..Profile::default()
        }
    }

    #[test]
    fn start_path_is_resolved_against_the_default_directory() {
        let r = |p| resolve_start_path(p, "/home/me");
        assert_eq!(r(""), None);
        assert_eq!(r("   "), None);
        assert_eq!(r("/var/www/").as_deref(), Some("/var/www"));
        assert_eq!(r("/").as_deref(), Some("/"));
        assert_eq!(r("~").as_deref(), Some("/home/me"));
        assert_eq!(r("~/site").as_deref(), Some("/home/me/site"));
        assert_eq!(r("site/logs").as_deref(), Some("/home/me/site/logs"));
    }

    #[test]
    fn vim_motions() {
        let k = |c| key(KeyCode::Char(c));
        let to = |n: Nav| match n {
            Nav::To(i) => Some(i),
            _ => None,
        };
        assert_eq!(to(nav(&k('j'), false, 0, 9, 10)), Some(1));
        assert_eq!(to(nav(&k('k'), false, 0, 9, 10)), Some(0));
        assert_eq!(to(nav(&k('G'), false, 0, 9, 10)), Some(9));
        assert!(matches!(nav(&k('g'), false, 4, 9, 10), Nav::G));
        assert_eq!(to(nav(&k('g'), true, 4, 9, 10)), Some(0));
        assert_eq!(to(nav(&ctrl('d'), false, 0, 99, 10)), Some(5));
        assert_eq!(to(nav(&ctrl('u'), false, 7, 99, 10)), Some(2));
        assert_eq!(to(nav(&ctrl('f'), false, 0, 99, 10)), Some(10));
        assert_eq!(to(nav(&ctrl('b'), false, 3, 99, 10)), Some(0));
        assert!(matches!(nav(&k('x'), false, 0, 9, 10), Nav::No));
    }

    #[test]
    fn gg_goes_to_the_top_of_the_saved_list() {
        let mut app = test_app(vec![
            saved("a", "a.x"),
            saved("b", "b.x"),
            saved("c", "c.x"),
        ]);
        app.form.focus = Focus::Profiles;
        app.on_key(key(KeyCode::Char('G')));
        assert_eq!(app.form.profiles.selected(), Some(2));
        app.on_key(key(KeyCode::Char('g')));
        assert_eq!(
            app.form.profiles.selected(),
            Some(2),
            "one g does nothing yet"
        );
        app.on_key(key(KeyCode::Char('g')));
        assert_eq!(app.form.profiles.selected(), Some(0));
        // A different key in between cancels the pending g.
        app.on_key(key(KeyCode::Char('G')));
        app.on_key(key(KeyCode::Char('g')));
        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Char('g')));
        assert_eq!(app.form.profiles.selected(), Some(2));
    }

    #[test]
    fn editing_updates_in_place_and_keeps_organisation() {
        let mut p = saved("prod", "old.example.com");
        p.folder = Some("Work".into());
        p.color = Some("red".into());
        p.tags = vec!["eu".into()];
        p.path = Some("/srv".into());
        let mut app = test_app(vec![p]);
        app.form.focus = Focus::Profiles;
        app.select_profile("prod");
        app.load_selected_profile();
        assert!(!app.form_modified());

        app.on_key(key(KeyCode::Char('e')));
        assert_eq!(app.form.focus, Focus::Host);
        assert!(app.editing().is_some());
        app.form.host.set("new.example.com");
        assert!(app.form_modified());
        app.on_key(ctrl('s'));

        assert!(app.modal.is_none(), "no name prompt for an existing entry");
        assert_eq!(app.config.profiles.len(), 1);
        let p = &app.config.profiles[0];
        assert_eq!(p.name, "prod");
        assert_eq!(p.host, "new.example.com");
        assert_eq!(p.folder.as_deref(), Some("Work"));
        assert_eq!(p.color.as_deref(), Some("red"));
        assert_eq!(p.tags, ["eu"]);
        assert_eq!(p.path.as_deref(), Some("/srv"));
        assert!(!app.form_modified());
    }

    #[test]
    fn saving_without_changes_says_so() {
        let mut app = test_app(vec![saved("prod", "h.example.com")]);
        app.on_key(key(KeyCode::Char('e')));
        app.form.focus = Focus::Host;
        app.on_key(ctrl('s'));
        assert!(app.modal.is_none());
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|n| n.text.contains("No changes"))
        );
    }

    #[test]
    fn changing_the_name_while_editing_renames() {
        let mut app = test_app(vec![
            saved("prod", "h.example.com"),
            saved("dev", "d.example.com"),
        ]);
        app.select_profile("prod");
        app.load_selected_profile();
        app.form.name.set("production");
        app.on_key(ctrl('s'));
        let names: Vec<_> = app
            .config
            .profiles
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["dev", "production"]);
        assert_eq!(app.form.loaded_from.as_deref(), Some("production"));

        // A name that belongs to another entry is refused.
        app.form.name.set("dev");
        app.on_key(ctrl('s'));
        assert!(
            app.form
                .error
                .as_deref()
                .is_some_and(|e| e.contains("already"))
        );
        assert_eq!(app.config.profiles.len(), 2);
    }

    #[test]
    fn a_new_connection_still_asks_for_a_name() {
        let mut app = test_app(vec![]);
        app.form.host.set("new.example.com");
        app.form.user.set("me");
        app.on_key(ctrl('s'));
        assert!(matches!(
            app.modal,
            Some(Modal::Input {
                action: InputAction::SaveProfile(_),
                ..
            })
        ));
    }

    #[test]
    fn y_copies_a_connection_under_a_new_name() {
        let mut p = saved("prod", "h.example.com");
        p.tags = vec!["eu".into()];
        p.path = Some("/srv".into());
        let mut app = test_app(vec![p]);
        app.form.focus = Focus::Profiles;
        app.select_profile("prod");
        app.on_key(key(KeyCode::Char('y')));
        app.on_key(key(KeyCode::Esc));
        app.form.focus = Focus::Profiles;
        app.select_profile("prod");
        app.on_key(key(KeyCode::Char('y')));
        let names: Vec<_> = app
            .config
            .profiles
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["prod", "prod copy", "prod copy 2"]);
        let copy = &app.config.profiles[2];
        assert_eq!(copy.tags, ["eu"]);
        assert_eq!(copy.path.as_deref(), Some("/srv"));
        assert_eq!(app.form.focus, Focus::Name);
        assert_eq!(app.form.loaded_from.as_deref(), Some("prod copy 2"));
    }

    #[test]
    fn default_path_is_part_of_the_profile() {
        let mut f = ConnectState::new(false);
        f.host.set("h.example.com");
        f.user.set("me");
        assert_eq!(f.to_profile(false).unwrap().path, None);
        f.path.set("  /var/www ");
        assert_eq!(
            f.to_profile(false).unwrap().path.as_deref(),
            Some("/var/www")
        );
        let p = saved("x", "h");
        f.apply(&p);
        assert_eq!(f.path.value(), "");
        assert!(f.order().contains(&Focus::Path));
        f.protocol = Protocol::S3;
        assert_eq!(f.order().last(), Some(&Focus::Path));
    }

    fn tagged(name: &str, tags: &[&str]) -> Profile {
        Profile {
            tags: tags.iter().map(|t| t.to_string()).collect(),
            ..saved(name, &format!("{name}.example.com"))
        }
    }

    #[test]
    fn filter_picker_combines_text_and_tags_without_hashes() {
        let mut app = test_app(vec![
            tagged("alpha", &["prod", "eu"]),
            tagged("alps", &["dev"]),
            tagged("beta", &["prod"]),
        ]);
        app.form.focus = Focus::Profiles;
        app.on_key(key(KeyCode::Char('/')));
        let Some(Modal::Filter { tags, focus, .. }) = &app.modal else {
            panic!("filter picker should open");
        };
        assert_eq!(*focus, FilterFocus::Text);
        let names: Vec<_> = tags.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(names, ["dev", "eu", "prod"]);

        // Letters are text while the search field has focus, even `q`.
        typed(&mut app, "alp");
        assert!(matches!(app.modal, Some(Modal::Filter { .. })));
        app.on_key(key(KeyCode::Tab)); // to the tag list
        app.on_key(key(KeyCode::Char('j'))); // eu
        app.on_key(key(KeyCode::Char('j'))); // prod
        app.on_key(key(KeyCode::Char(' ')));
        app.on_key(key(KeyCode::Enter));

        assert!(app.modal.is_none());
        assert_eq!(app.form.filter_text, "alp");
        assert_eq!(app.form.filter_tags.iter().collect::<Vec<_>>(), ["prod"]);
        assert_eq!(app.form.filter_summary(), "prod \"alp\"");
        let rows = app.rows();
        assert_eq!(rows.len(), 1, "only alpha is prod and matches 'alp'");
        assert_eq!(app.selected_profile().unwrap().name, "alpha");

        // Esc clears both.
        app.on_key(key(KeyCode::Esc));
        assert!(!app.form.has_filter());
        assert_eq!(app.rows().len(), 3);
    }

    #[tokio::test]
    async fn an_unreachable_default_path_falls_back_and_stays_connected() {
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let session = Session {
            label: "sftp://me@h:22".into(),
            detail: "SFTP  me@h:22".into(),
            cmd: cmd_tx,
            worker: tokio::spawn(async {}),
            home: "/home/me".into(),
        };
        let mut app = test_app(vec![]);
        let mut b = BrowserState::new("h".into(), None, session, "/".into());
        b.request_remote_or("/nope".into(), Keep::First, Some("/home/me".into()))
            .unwrap();
        app.browser = Some(b);

        app.on_app_event(AppEvent::Listed {
            seq: 1,
            path: "/nope".into(),
            result: Err("No such file".into()),
        });

        assert!(app.browser.is_some(), "the connection stays open");
        assert!(app.modal.is_none(), "a warning, not a blocking error");
        let n = app.notice.as_ref().expect("a warning is shown");
        assert!(n.is_error && n.text.contains("/nope") && n.text.contains("/home/me"));
        // Drain: the first request, then the retry of the default directory.
        let first = cmd_rx.recv().await.unwrap();
        assert!(matches!(first, Cmd::List { ref path, .. } if path == "/nope"));
        let retry = cmd_rx.recv().await.unwrap();
        assert!(matches!(retry, Cmd::List { ref path, .. } if path == "/home/me"));
        // The retry has no further fallback: a failure there is a real error.
        app.on_app_event(AppEvent::Listed {
            seq: 2,
            path: "/home/me".into(),
            result: Err("denied".into()),
        });
        assert!(matches!(app.modal, Some(Modal::Error { .. })));
    }

    #[test]
    fn n_starts_a_blank_connection_even_with_one_selected() {
        let mut p = saved("prod", "h.example.com");
        p.path = Some("/srv".into());
        let mut app = test_app(vec![p]);
        app.form.focus = Focus::Profiles;
        app.select_profile("prod");
        app.load_selected_profile();
        assert!(app.editing().is_some());
        app.on_key(key(KeyCode::Char('n')));
        assert!(app.editing().is_none());
        assert_eq!(app.form.focus, Focus::Host);
        assert_eq!(app.form.host.value(), "");
        assert_eq!(app.form.path.value(), "");
        assert_eq!(app.form.port.value(), "22");
        // Saving it asks for a name rather than overwriting the selected entry.
        app.form.host.set("new.example.com");
        app.form.user.set("me");
        app.on_key(ctrl('s'));
        assert!(matches!(
            app.modal,
            Some(Modal::Input {
                action: InputAction::SaveProfile(_),
                ..
            })
        ));
        assert_eq!(app.config.profiles.len(), 1);
        // Ctrl-L does the same from inside a text field.
        app.modal = None;
        app.form.focus = Focus::Host;
        app.select_profile("prod");
        app.load_selected_profile();
        app.on_key(ctrl('l'));
        assert!(app.editing().is_none() && app.form.host.value().is_empty());
    }

    #[test]
    fn navigating_the_list_previews_each_connection_in_the_form() {
        let mut app = test_app(vec![saved("a", "a.x"), saved("b", "b.x")]);
        assert_eq!(app.form.focus, Focus::Profiles);
        // The first connection is previewed from the start; none is "being edited".
        assert_eq!(app.form.host.value(), "a.x");
        assert!(app.editing().is_none() && !app.form_dirty());
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.form.host.value(), "b.x");
        assert!(app.editing().is_none());
        // Ctrl-S on a preview doesn't create a copy.
        app.on_key(ctrl('s'));
        assert!(app.modal.is_none() && app.config.profiles.len() == 2);
        // e starts editing it, on the first field.
        app.on_key(key(KeyCode::Char('e')));
        assert_eq!(app.editing().unwrap().name, "b");
        assert_eq!(app.form.focus, Focus::Host);
        // While editing, moving around the list leaves the form alone.
        app.form.host.set("changed.x");
        app.form.focus = Focus::Profiles;
        app.on_key(key(KeyCode::Char('k')));
        assert_eq!(app.form.host.value(), "changed.x");
        assert_eq!(app.editing().unwrap().name, "b");
    }

    #[test]
    fn tabbing_into_a_preview_makes_it_an_edit() {
        let mut app = test_app(vec![saved("a", "a.x")]);
        assert!(app.editing().is_none());
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.editing().unwrap().name, "a");
        // And a folder row previews a blank form.
        app.config.add_folder("Work").unwrap();
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::Esc));
        app.form.focus = Focus::Profiles;
        app.select_folder("Work");
        app.on_key(key(KeyCode::Char('k')));
        app.on_key(key(KeyCode::Char('j')));
        assert!(app.form.host.value().is_empty() || app.selected_profile().is_some());
    }

    #[test]
    fn esc_with_no_field_selected_stops_editing() {
        let mut app = test_app(vec![saved("a", "a.x")]);
        app.on_key(key(KeyCode::Char('e')));
        app.on_key(key(KeyCode::Esc)); // leaves the text field
        assert_eq!(app.form.focus, Focus::Protocol);
        assert!(app.editing().is_some());
        app.on_key(key(KeyCode::Esc)); // nothing selected: back to a new connection
        assert!(app.editing().is_none() && app.form.host.value().is_empty());
        assert!(app.modal.is_none(), "nothing was modified, so no prompt");
    }

    #[test]
    fn leaving_a_modified_connection_asks_first() {
        let mut app = test_app(vec![saved("a", "a.x"), saved("b", "b.x")]);
        app.on_key(key(KeyCode::Char('e')));
        app.form.host.set("changed.x");
        // Esc to the protocol row, Esc again to stop editing.
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.modal, Some(Modal::Unsaved { .. })));
        // Esc cancels: still editing, nothing lost.
        app.on_key(key(KeyCode::Esc));
        assert!(app.modal.is_none() && app.form.host.value() == "changed.x");
        assert_eq!(app.config.profiles[0].host, "a.x");

        // n (new connection) asks again; n then discards and carries on.
        app.on_key(key(KeyCode::Char('n')));
        assert!(matches!(app.modal, Some(Modal::Unsaved { .. })));
        app.on_key(key(KeyCode::Char('n')));
        assert!(app.modal.is_none() && app.editing().is_none());
        assert_eq!(app.config.profiles[0].host, "a.x", "discarded");
    }

    #[test]
    fn saving_from_the_prompt_then_continues() {
        let mut app = test_app(vec![saved("a", "a.x"), saved("b", "b.x")]);
        app.on_key(key(KeyCode::Char('e'))); // editing a
        app.form.host.set("changed.x");
        app.form.focus = Focus::Profiles;
        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Char('e'))); // wants b
        assert!(matches!(app.modal, Some(Modal::Unsaved { .. })));
        app.on_key(key(KeyCode::Char('y')));
        assert!(app.modal.is_none());
        assert_eq!(app.config.profiles[0].host, "changed.x", "a was saved");
        assert_eq!(app.editing().unwrap().name, "b", "then b was opened");
        assert!(app.after_save.is_none());
    }

    #[test]
    fn an_unsaved_new_connection_is_protected_too() {
        let mut app = test_app(vec![saved("a", "a.x")]);
        app.on_key(key(KeyCode::Char('n')));
        app.on_key(key(KeyCode::Char('h'))); // typed into Host
        assert_eq!(app.form.host.value(), "h");
        app.form.focus = Focus::Profiles;
        app.on_key(key(KeyCode::Char('e')));
        assert!(matches!(app.modal, Some(Modal::Unsaved { .. })));
        // Saving a new connection asks for its name, then carries on.
        app.form.user.set("me");
        app.on_key(key(KeyCode::Char('y')));
        assert!(matches!(
            app.modal,
            Some(Modal::Input {
                action: InputAction::SaveProfile(_),
                ..
            })
        ));
        app.on_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.config.profiles.len(), 2);
        assert_eq!(
            app.editing().unwrap().name,
            "a",
            "now editing the one asked for"
        );
    }

    #[test]
    fn cancelling_the_name_prompt_drops_the_pending_action() {
        let mut app = test_app(vec![saved("a", "a.x")]);
        app.on_key(key(KeyCode::Char('n')));
        app.on_key(key(KeyCode::Char('h')));
        app.form.focus = Focus::Profiles;
        app.on_key(key(KeyCode::Char('e')));
        app.on_key(key(KeyCode::Char('y')));
        app.on_key(key(KeyCode::Esc)); // cancel the name prompt
        assert!(app.modal.is_none() && app.after_save.is_none());
        assert_eq!(app.form.host.value(), "h");
        assert_eq!(app.config.profiles.len(), 1);
    }

    #[tokio::test]
    async fn connecting_while_editing_asks_to_save_first() {
        let mut app = test_app(vec![saved("a", "a.x")]);
        app.on_key(key(KeyCode::Char('e')));
        // Unmodified: connects straight away.
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Connecting { .. })));
        app.modal = None;
        if let Some(t) = app.connect_task.take() {
            t.abort();
        }
        // Modified: asks first; n connects without saving, y saves then connects.
        app.form.host.set("changed.x");
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Unsaved { .. })));
        app.on_key(key(KeyCode::Esc));
        assert!(app.modal.is_none() && app.connect_task.is_none());
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Char('y')));
        assert_eq!(app.config.profiles[0].host, "changed.x");
        assert!(matches!(app.modal, Some(Modal::Connecting { .. })));
        if let Some(t) = app.connect_task.take() {
            t.abort();
        }
    }

    #[test]
    fn s_and_c_jump_between_the_sections() {
        let mut app = test_app(vec![saved("a", "a.x")]);
        app.form.focus = Focus::Protocol;
        app.on_key(key(KeyCode::Char('S')));
        assert_eq!(app.form.focus, Focus::Profiles);
        app.on_key(key(KeyCode::Char('C')));
        assert_eq!(app.form.focus, Focus::Host);
        // In a text field they are just letters.
        app.on_key(key(KeyCode::Char('S')));
        assert!(app.form.host.value().ends_with('S'));
    }

    #[test]
    fn new_folders_are_created_inside_the_selected_folder() {
        let mut app = test_app(vec![saved("a", "a.x")]);
        app.config.add_folder("Work").unwrap();
        app.form.focus = Focus::Profiles;
        app.select_folder("Work");
        app.on_key(key(KeyCode::Char('f')));
        for c in "Prod".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        assert!(app.config.folder_names().contains(&"Work/Prod".to_string()));
        // It is selected, and new connections saved there are filed in it.
        assert_eq!(app.context_folder().as_deref(), Some("Work/Prod"));
    }

    #[test]
    fn the_log_keeps_one_line_per_entry_and_is_capped() {
        let mut app = test_app(vec![]);
        app.log(LogLevel::Error, "first line\nsecond line");
        assert_eq!(app.log[0].text, "first line");
        assert_eq!(app.log[0].time.len(), 8);
        for i in 0..LOG_LIMIT + 20 {
            app.log(LogLevel::Info, format!("n{i}"));
        }
        assert_eq!(app.log.len(), LOG_LIMIT);
        assert_eq!(app.log.back().unwrap().text, format!("n{}", LOG_LIMIT + 19));
    }

    #[test]
    fn j_and_k_scroll_the_log_but_only_when_not_typing() {
        let mut app = test_app(vec![saved("a", "a.x")]);
        for i in 0..10 {
            app.log(LogLevel::Info, format!("n{i}"));
        }
        // Saved list focused: K scrolls back, J forward.
        app.on_key(key(KeyCode::Char('K')));
        app.on_key(key(KeyCode::Char('K')));
        assert_eq!(app.log_scroll, 2);
        app.on_key(key(KeyCode::Char('J')));
        assert_eq!(app.log_scroll, 1);
        // New entries don't move a scrolled-back view.
        app.log(LogLevel::Info, "new");
        assert_eq!(app.log_scroll, 2);
        // In a text field they are letters; PageUp / PageDown scroll instead.
        app.on_key(key(KeyCode::Char('e')));
        app.on_key(key(KeyCode::Char('K')));
        assert!(app.form.host.value().ends_with('K'));
        let before = app.log_scroll;
        app.on_key(key(KeyCode::PageUp));
        assert_eq!(app.log_scroll, before + 4);
        app.on_key(key(KeyCode::PageDown));
        assert_eq!(app.log_scroll, before);
    }

    #[tokio::test]
    async fn connection_events_are_logged() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let session = Session {
            label: "sftp://me@h:22".into(),
            detail: "SFTP  me@h:22".into(),
            cmd: cmd_tx,
            worker: tokio::spawn(async {}),
            home: "/home/me".into(),
        };
        let mut app = test_app(vec![]);
        app.on_app_event(AppEvent::Connected(Ok(session)));
        app.on_app_event(AppEvent::Listed {
            seq: 1,
            path: "/home/me".into(),
            result: Ok(vec![]),
        });
        let text: Vec<&str> = app.log.iter().map(|e| e.text.as_str()).collect();
        assert!(
            text.iter()
                .any(|t| t.starts_with("Connected to sftp://me@h:22"))
        );
        assert!(text.contains(&"Remote (0 entries): /home/me"));
        app.on_app_event(AppEvent::ConnectionLost("gone".into()));
        assert!(
            app.log
                .back()
                .unwrap()
                .text
                .contains("Connection lost: gone")
        );
        assert_eq!(app.log.back().unwrap().level, LogLevel::Error);
    }

    #[test]
    fn host_is_optional_to_save_when_a_password_manager_is_set() {
        let mut app = test_app(vec![]);
        app.form.name.set("work-box");
        app.form.focus = Focus::Host;
        // No manager: the host is required to save.
        app.on_key(ctrl('s'));
        assert!(app.modal.is_none());
        assert_eq!(app.form.error.as_deref(), Some("Host is required"));
        // With one, a name is enough.
        app.config.password_manager = Some(PasswordManager::OnePassword);
        app.on_key(ctrl('s'));
        assert!(matches!(
            app.modal,
            Some(Modal::Input {
                action: InputAction::SaveProfile(_),
                ..
            })
        ));
    }

    #[test]
    fn ctrl_o_picks_a_password_manager() {
        let mut app = test_app(vec![]);
        assert!(app.config.password_manager.is_none());
        app.on_key(ctrl('p'));
        assert!(matches!(
            app.modal,
            Some(Modal::PasswordManager { index: 0 })
        ));
        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(
            app.config.password_manager,
            Some(PasswordManager::OnePassword)
        );
        // Reopens on the current choice; selecting "None" turns it off again.
        app.on_key(ctrl('p'));
        assert!(matches!(
            app.modal,
            Some(Modal::PasswordManager { index: 1 })
        ));
        app.on_key(key(KeyCode::Char('k')));
        app.on_key(key(KeyCode::Enter));
        assert!(app.config.password_manager.is_none());
        // Esc leaves it alone.
        app.on_key(ctrl('p'));
        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Esc));
        assert!(app.config.password_manager.is_none());
    }

    #[test]
    fn tab_reaches_the_log_and_j_k_move_through_it() {
        let mut app = test_app(vec![]);
        app.log_visible = true;
        for i in 0..5 {
            app.log(LogLevel::Info, format!("line {i}"));
        }
        // No saved connections: the cycle runs Name .. Path, then the log.
        let last = app.form.edges(false).1;
        assert_eq!(last, Focus::Path);
        app.form.focus = Focus::Path;
        app.on_key(key(KeyCode::Tab));
        assert!(app.log_focus && app.form.focus == Focus::Log);
        assert_eq!(app.log_sel, 4, "starts on the newest line");
        app.on_key(key(KeyCode::Char('k')));
        app.on_key(key(KeyCode::Char('k')));
        assert_eq!(app.log_sel, 2);
        app.on_key(key(KeyCode::Char('g')));
        app.on_key(key(KeyCode::Char('g')));
        assert_eq!(app.log_sel, 0);
        app.on_key(key(KeyCode::Char('G')));
        assert_eq!(app.log_sel, 4);
        // Letters in the log don't type into the form.
        app.on_key(key(KeyCode::Char('x')));
        assert!(app.form.host.value().is_empty() && app.form.path.value().is_empty());
        // Tab wraps round to the first field; Shift-Tab / Esc go back to the last.
        app.on_key(key(KeyCode::Tab));
        assert!(!app.log_focus);
        assert_eq!(app.form.focus, app.form.edges(false).0);
        app.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert!(app.log_focus);
        app.on_key(key(KeyCode::Esc));
        assert!(!app.log_focus);
        assert_eq!(app.form.focus, Focus::Path);
    }

    #[test]
    fn the_log_is_skipped_when_it_is_not_shown() {
        let mut app = test_app(vec![]);
        app.log_visible = false;
        app.form.focus = Focus::Path;
        app.on_key(key(KeyCode::Tab));
        assert!(!app.log_focus);
        assert_eq!(app.form.focus, app.form.edges(false).0);
    }
}
