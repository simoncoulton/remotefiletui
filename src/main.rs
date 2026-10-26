mod app;
mod backend;
mod config;
mod form;
mod input;
mod onepassword;
mod ssh_config;
mod theme;
mod ui;
mod util;
mod worker;

use std::{io, time::Duration};

use anyhow::{Context, Result};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    crossterm::{
        event::{DisableBracketedPaste, EnableBracketedPaste, Event, EventStream},
        execute,
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    },
};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio_stream::StreamExt;

use crate::{app::App, config::Config, worker::AppEvent};

type Tui = Terminal<CrosstermBackend<io::Stdout>>;

const USAGE: &str = "\
remotefiletui - a terminal UI for FTP, FTPS and SFTP

USAGE:
    remotefiletui

OPTIONS:
    -h, --help       Print this help
    -V, --version    Print the version

Saved connections live in the platform config directory (profiles.toml).
Passwords are never written to disk.";

fn setup_terminal() -> Result<Tui> {
    // Restore the terminal before the default hook prints, or the message is lost
    // in the alternate screen. Only the main thread owns the terminal: a panic in a
    // background task must not tear the UI down (it surfaces as a lost connection).
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some("main") {
            restore_terminal();
            default_hook(info);
        }
    }));

    enable_raw_mode().context("cannot enable raw mode (is stdout a terminal?)")?;
    let mut stdout = io::stdout();
    if let Err(e) = execute!(stdout, EnterAlternateScreen, EnableBracketedPaste) {
        restore_terminal();
        return Err(e).context("cannot initialise the terminal");
    }
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
}

#[tokio::main]
async fn main() -> Result<()> {
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "-h" | "--help" => println!("{USAGE}"),
            "-V" | "--version" => println!("remotefiletui {}", env!("CARGO_PKG_VERSION")),
            other => {
                eprintln!("remotefiletui: unknown argument '{other}'\n\n{USAGE}");
                std::process::exit(2);
            }
        }
        return Ok(());
    }

    let (config, startup_error) = match Config::load() {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(format!("{e:#}"))),
    };
    let (tx, rx) = unbounded_channel();
    let mut app = App::new(tx, config, startup_error);

    let mut terminal = setup_terminal()?;
    let result = run(&mut terminal, &mut app, rx).await;
    restore_terminal();
    app.shutdown().await;
    result
}

#[cfg(unix)]
async fn termination_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut hup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) else {
        return std::future::pending().await;
    };
    tokio::select! { _ = term.recv() => {}, _ = hup.recv() => {} }
}

#[cfg(not(unix))]
async fn termination_signal() {
    std::future::pending::<()>().await
}

async fn run(terminal: &mut Tui, app: &mut App, mut rx: UnboundedReceiver<AppEvent>) -> Result<()> {
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut shutdown = std::pin::pin!(termination_signal());
    let mut dirty = true;

    while !app.should_quit {
        if dirty {
            terminal.draw(|frame| ui::draw(frame, app))?;
            dirty = false;
        }
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) => { app.on_key(key); dirty = true; }
                Some(Ok(Event::Paste(text))) => { app.on_paste(&text); dirty = true; }
                Some(Ok(Event::Resize(..))) => dirty = true,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e).context("terminal input error"),
                None => break, // stdin closed
            },
            Some(event) = rx.recv() => { app.on_app_event(event); dirty = true; }
            _ = ticker.tick() => {
                app.on_tick();
                // Only redraw on ticks while something is animating.
                dirty |= app.is_busy();
            }
            _ = &mut shutdown => break,
        }
    }
    Ok(())
}
