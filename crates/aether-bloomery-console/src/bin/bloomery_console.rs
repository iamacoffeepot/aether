//! `bloomery-console` — arg parsing, terminal setup/restore, the event loop.

use std::env;
use std::io::{self, stdout};
use std::panic;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use aether_bloomery_console::dto::ViewDocument;
use aether_bloomery_console::http::{self, Endpoint};
use aether_bloomery_console::state::BoardState;
use aether_bloomery_console::ui;
use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

/// Coordinator REST bind when `AETHER_HTTP_PORT` is unset — the same default
/// `aether-chassis-bloomery` uses.
const DEFAULT_HTTP_PORT: u16 = 8910;
const DEFAULT_POLL_MILLIS: u64 = 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_millis(1000);
const INPUT_SLICE: Duration = Duration::from_millis(100);

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Read-only live operator board for the Bloomery coordinator.
#[derive(Parser, Debug)]
#[command(name = "bloomery-console", about = "Live operator board for the Bloomery coordinator")]
struct Args {
    /// Coordinator REST host.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Coordinator REST port. Defaults to `AETHER_HTTP_PORT`, then 8910.
    #[arg(long)]
    port: Option<u16>,

    /// How often to poll `GET /view`, in milliseconds.
    #[arg(long, default_value_t = DEFAULT_POLL_MILLIS)]
    poll_millis: u64,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let endpoint = Endpoint { host: args.host, port: args.port.unwrap_or_else(coordinator_port) };
    let poll = Duration::from_millis(args.poll_millis);
    run(endpoint, poll)
}

/// `AETHER_HTTP_PORT`, then the coordinator's compiled default.
fn coordinator_port() -> u16 {
    // Operator tooling reading the coordinator's REST bind — not cap config.
    #[allow(clippy::disallowed_methods)]
    env::var("AETHER_HTTP_PORT").ok().and_then(|value| value.parse().ok()).unwrap_or(DEFAULT_HTTP_PORT)
}

fn run(endpoint: Endpoint, poll: Duration) -> Result<()> {
    install_panic_hook();
    install_shutdown_signals();
    enable_raw_mode().context("enter raw mode")?;
    if let Err(error) = execute!(stdout(), EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error).context("enter alternate screen");
    }
    let result = Terminal::new(CrosstermBackend::new(stdout()))
        .context("open terminal")
        .and_then(|mut terminal| event_loop(&mut terminal, endpoint, poll));
    restore_terminal();
    result
}

fn event_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, endpoint: Endpoint, poll: Duration) -> Result<()> {
    let mut state = BoardState::new(endpoint.label());
    let mut last_poll = Instant::now();
    let mut force = true;

    loop {
        if SHUTDOWN.load(Ordering::SeqCst) {
            return Ok(());
        }

        if force || last_poll.elapsed() >= poll {
            match http::get_json::<ViewDocument>(&endpoint, "/view", REQUEST_TIMEOUT) {
                Ok(view) => state.apply_view(view),
                Err(error) => state.apply_error(error),
            }
            last_poll = Instant::now();
            force = false;
        }

        terminal.draw(|frame| ui::render(frame, &state)).context("draw board")?;

        let wait = poll.saturating_sub(last_poll.elapsed()).min(INPUT_SLICE);
        if !event::poll(wait).context("poll input")? {
            continue;
        }
        let Event::Key(key) = event::read().context("read input")? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('q') => return Ok(()),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Char('j') | KeyCode::Down => state.select_next(),
            KeyCode::Char('k') | KeyCode::Up => state.select_prev(),
            KeyCode::Char('r') => force = true,
            _ => {}
        }
    }
}

fn install_panic_hook() {
    let original = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore_terminal();
        original(info);
    }));
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), LeaveAlternateScreen);
}

#[cfg(unix)]
fn install_shutdown_signals() {
    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;

    unsafe extern "C" {
        fn signal(signum: i32, handler: extern "C" fn(i32)) -> extern "C" fn(i32);
    }

    extern "C" fn on_signal(_: i32) {
        restore_terminal();
        SHUTDOWN.store(true, Ordering::SeqCst);
    }

    // SAFETY: POSIX `signal` installs a process-wide handler. `on_signal`
    // only stores to an AtomicBool and best-effort restores tty modes.
    unsafe {
        signal(SIGINT, on_signal);
    }
    // SAFETY: same contract as the SIGINT install above.
    unsafe {
        signal(SIGTERM, on_signal);
    }
}

#[cfg(not(unix))]
fn install_shutdown_signals() {}
