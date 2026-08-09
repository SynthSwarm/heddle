//! heddle — an agent-native Matrix client for the terminal.
//!
//! See `docs/SPEC.md` for the design and `docs/PLAN.md` for the delivery plan.

// The matrix-sdk async state machines nest deeply enough to exceed the default limit
// when the worker's futures are monomorphised into this crate.
#![recursion_limit = "512"]

mod app;
mod config;
mod keymap;
mod ui;

use anyhow::{Context, Result};
use app::{App, EVENT_BUDGET};
use clap::{Parser, Subcommand};
use config::{Config, Dirs};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyEventKind, MouseButton,
    MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures_util::StreamExt;
use heddle_matrix::{session, Handle};
use std::io::stdout;
use std::time::Duration;

/// How often to tick even with no input, so countdowns and spinners advance.
const TICK: Duration = Duration::from_millis(250);

#[derive(Parser)]
#[command(name = "heddle", version, about, long_about = None)]
struct Cli {
    /// Profile to use. Defaults to the one marked `default = true`.
    ///
    /// Accepted either before or after a subcommand. With `login` this names the
    /// profile the session is saved under, which need not exist in config.toml yet;
    /// otherwise it selects an existing profile from config.toml.
    #[arg(long, short, global = true)]
    profile: Option<String>,

    /// Verify the environment and exit.
    #[arg(long)]
    check: bool,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Log in and save a session.
    ///
    /// This writes the credential store only. The matching `[profile.<name>]` block in
    /// config.toml is not created for you and must be added by hand before plain
    /// `heddle` will start with that profile.
    Login {
        /// Homeserver URL, e.g. https://matrix.example.org.
        #[arg(long)]
        homeserver: String,
        /// Full Matrix user ID, e.g. @you:example.org.
        #[arg(long)]
        user: String,
        /// Environment variable holding the password.
        ///
        /// The password is only ever read from the environment: passing it as an
        /// argument would leak it into the shell history and the process table.
        #[arg(long, default_value = "HEDDLE_PASSWORD")]
        password_env: String,
    },
    /// Forget the saved session. E2EE keys are kept.
    Logout,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let dirs = Dirs::resolve()?;
    let _guard = init_tracing(&dirs)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime")?;

    runtime.block_on(run(cli, dirs))
}

async fn run(cli: Cli, dirs: Dirs) -> Result<()> {
    let config = Config::load(&dirs.config_file())
        .with_context(|| format!("reading {}", dirs.config_file().display()))?;

    match cli.command {
        Some(Cmd::Login {
            homeserver,
            user,
            password_env,
        }) => {
            let password = std::env::var(&password_env).with_context(|| {
                format!("${password_env} is not set; export it or pass --password-env")
            })?;
            let name = cli.profile.as_deref().unwrap_or("default");
            let paths = session::Paths::for_profile(&dirs.data, name);
            session::login_password(&homeserver, &user, &password, "heddle", &paths).await?;
            println!("logged in as {user}; profile `{name}` saved");
            return Ok(());
        }
        Some(Cmd::Logout) => {
            let name = cli.profile.as_deref().unwrap_or("default");
            session::forget(&session::Paths::for_profile(&dirs.data, name))?;
            println!("session for profile `{name}` forgotten");
            return Ok(());
        }
        None => {}
    }

    let (profile_name, profile) =
        config
            .resolve_profile(cli.profile.as_deref())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no profile selected. Add one to {} or pass --profile",
                    dirs.config_file().display()
                )
            })?;

    if cli.check {
        return check(&profile.homeserver).await;
    }

    let paths = session::Paths::for_profile(&dirs.data, &profile_name);
    let client = session::restore(&profile_name, &paths)
        .await
        .with_context(|| format!("restoring profile `{profile_name}`"))?;

    let handle = heddle_matrix::spawn(client)
        .await
        .context("starting the matrix worker")?;

    tui(App::new(config), handle).await
}

/// Verify the homeserver supports everything heddle needs.
async fn check(homeserver: &str) -> Result<()> {
    let caps = heddle_matrix::check_homeserver(homeserver).await?;

    println!("homeserver: {homeserver}");
    println!("  sliding sync (MSC4186):  {}", tick(caps.sliding_sync));
    println!("  threads (MSC3440):       {}", tick(caps.threads));
    println!("  cross-signing:           {}", tick(caps.cross_signing));
    println!("  spec versions:           {}", caps.versions.join(", "));

    for problem in caps.problems() {
        eprintln!("warning: {problem}");
    }

    if !caps.is_usable() {
        anyhow::bail!("homeserver is not usable by heddle");
    }
    println!("\nok");
    Ok(())
}

fn tick(ok: bool) -> &'static str {
    if ok {
        "yes"
    } else {
        "NO"
    }
}

/// Run the terminal UI.
async fn tui(mut app: App, mut handle: Handle) -> Result<()> {
    let mouse = app.config.ui.mouse;
    let mut terminal = enter_terminal(mouse)?;

    // Restore the terminal even on panic; a raw-mode terminal left behind is
    // unusable and the user would have to blind-type `reset`.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal(mouse);
        hook(info);
    }));

    let result = event_loop(&mut terminal, &mut app, &mut handle).await;

    restore_terminal(mouse)?;
    handle.shutdown().await;
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    handle: &mut Handle,
) -> Result<()> {
    let mut input = EventStream::new();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    app.open_focused_view();
    dispatch(app, handle);

    loop {
        // A full clear discards ratatui's diff state, forcing every cell to be written
        // again. Needed when the screen and ratatui's model of it have diverged; see
        // App::focus_moved.
        if app.needs_redraw {
            terminal.clear()?;
            app.needs_redraw = false;
        }
        terminal.draw(|frame| ui::draw(frame, app))?;
        if app.should_quit {
            return Ok(());
        }

        tokio::select! {
            // Input first: a keypress must not queue behind a sync burst.
            biased;

            Some(Ok(event)) = input.next() => {
                handle_input(app, event);
            }

            Some(worker_event) = handle.next() => {
                app.apply_worker_event(worker_event);
                // Fold whatever else has already arrived, up to the budget, so a burst
                // costs one frame rather than one frame each.
                for extra in handle.drain(EVENT_BUDGET) {
                    app.apply_worker_event(extra);
                }
            }

            _ = ticker.tick() => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                // Hermes times approvals out server-side but the resolution event can
                // be lost; without this a pane would stay blocked for ever.
                app.agents.expire_pending(now);
            }
        }

        dispatch(app, handle);
    }
}

fn handle_input(app: &mut App, event: Event) {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            let (action, mode) = keymap::map(key, app.mode, app.prefix);
            app.mode = mode;
            app.apply_action(action);
        }
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // Bars first. They sit above the tiling and own their rows, so a click
                // there must never fall through and focus a pane instead.
                if app.click_bar(mouse.column, mouse.row) {
                    return;
                }
                let room = app
                    .workspaces
                    .focused()
                    .and_then(|w| w.focused_tab())
                    .map(|t| t.room_id.clone());
                if let Some(id) = room
                    .and_then(|r| app.tilings.get(&r))
                    .and_then(|t| t.pane_at(mouse.column, mouse.row))
                {
                    app.focus_pane_id(id);
                }
            }
            MouseEventKind::ScrollUp => app.apply_action(keymap::Action::ScrollUp(3)),
            MouseEventKind::ScrollDown => app.apply_action(keymap::Action::ScrollDown(3)),
            _ => {}
        },
        Event::Resize(..) => {}
        _ => {}
    }
}

/// Forward the app's queued commands to the worker.
fn dispatch(app: &mut App, handle: &Handle) {
    for command in app.take_commands() {
        if !handle.send(command) {
            app.status = Some("worker stopped".into());
            app.should_quit = true;
            return;
        }
    }
}

fn enter_terminal(mouse: bool) -> Result<ratatui::DefaultTerminal> {
    enable_raw_mode()?;
    if mouse {
        crossterm::execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    } else {
        crossterm::execute!(stdout(), EnterAlternateScreen)?;
    }
    Ok(ratatui::Terminal::new(
        ratatui::backend::CrosstermBackend::new(stdout()),
    )?)
}

fn restore_terminal(mouse: bool) -> Result<()> {
    if mouse {
        crossterm::execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    } else {
        crossterm::execute!(stdout(), LeaveAlternateScreen)?;
    }
    disable_raw_mode()?;
    Ok(())
}

/// Log to a file. Logging to stdout would corrupt the alternate screen.
fn init_tracing(dirs: &Dirs) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    std::fs::create_dir_all(&dirs.state)?;
    let file = tracing_appender::rolling::never(&dirs.state, "heddle.log");
    let (writer, guard) = tracing_appender::non_blocking(file);

    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("HEDDLE_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("heddle=info,warn")),
        )
        .init();

    Ok(guard)
}
