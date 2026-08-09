//! heddle — an agent-native Matrix client for the terminal.
//!
//! See `docs/SPEC.md` for the design and `docs/PLAN.md` for the delivery plan.

// The matrix-sdk async state machines nest deeply enough to exceed the default limit
// when the worker's futures are monomorphised into this crate.
#![recursion_limit = "512"]

mod app;
mod composer;
mod config;
mod emoji;
mod keymap;
mod palette;
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

/// Log filter used when `HEDDLE_LOG` is unset.
///
/// Names every heddle crate, not just the binary. `heddle=info` alone silences the
/// worker, which is where anything interesting happens.
const DEFAULT_LOG: &str = "heddle=info,heddle_matrix=info,heddle_agent=info,\
                           heddle_render=info,heddle_layout=info,warn";

/// How often to tick even with no input, so countdowns and spinners advance.
const TICK: Duration = Duration::from_millis(250);

/// Shortest interval between layout writes.
///
/// Long enough that holding a resize key does not thrash the disk, short enough that a
/// terminal killed outright loses at most a couple of seconds of rearranging.
const LAYOUT_FLUSH: Duration = Duration::from_secs(2);

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
    /// This saves the session and adds a matching `[profile.<name>]` block to
    /// config.toml if one is not already there. The first profile added becomes the
    /// default, so a single-account install works with a bare `heddle`.
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
            let (client, cross_signing) =
                session::login_password(&homeserver, &user, &password, "heddle", &paths).await?;
            println!("logged in as {user}; profile `{name}` saved");

            // Written from the login response rather than the typed `--user`, which may
            // be a bare localpart: the config wants the full `@user:server` form.
            let user_id = client
                .user_id()
                .map_or_else(|| user.clone(), ToString::to_string);
            let entry = config::Profile {
                user_id,
                homeserver: homeserver.clone(),
                default: false,
            };
            let path = dirs.config_file();
            if config::append_profile(&path, name, &entry)? {
                println!("added [profile.{name}] to {}", path.display());
            }

            match cross_signing {
                session::CrossSigning::Created => {
                    println!("cross-signing identity created for this account");
                }
                session::CrossSigning::AlreadyPresent => {
                    println!("cross-signing identity already existed; left untouched");
                }
                session::CrossSigning::NeedsInteractiveAuth => {
                    println!(
                        "warning: the homeserver wants an auth flow heddle cannot drive, \
                         so this account has no cross-signing identity yet"
                    );
                }
            }
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
            .ok_or_else(|| match cli.profile.as_deref() {
                // Naming the profile that was asked for matters: the old wording claimed
                // none had been selected, when in fact one had and simply was not there.
                Some(name) => anyhow::anyhow!(
                    "no `[profile.{name}]` in {}. Log in with `heddle --profile {name} login` \
                     to create it, or add the block by hand",
                    dirs.config_file().display()
                ),
                None if config.has_profiles() => anyhow::anyhow!(
                    "several profiles are configured and none is marked `default = true`. \
                     Pass --profile, or set the flag in {}",
                    dirs.config_file().display()
                ),
                None => anyhow::anyhow!(
                    "no profiles configured. Run `heddle login` to create one, or add a \
                     block to {}",
                    dirs.config_file().display()
                ),
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

    let layout_path = dirs.layout_file(&profile_name);
    let app = App::new(config, heddle_layout::Layout::load(&layout_path));

    tui(app, handle, layout_path).await
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
async fn tui(mut app: App, mut handle: Handle, layout_path: std::path::PathBuf) -> Result<()> {
    let mouse = app.config.ui.mouse;
    let mut terminal = enter_terminal(mouse)?;

    // Restore the terminal even on panic; a raw-mode terminal left behind is
    // unusable and the user would have to blind-type `reset`.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal(mouse);
        hook(info);
    }));

    let result = event_loop(&mut terminal, &mut app, &mut handle, &layout_path).await;

    // Unconditionally, and before anything that can fail: whatever went wrong, the
    // arrangement the user built is not the thing to punish them by losing.
    save_layout(&app, &layout_path);

    restore_terminal(mouse)?;
    handle.shutdown().await;
    result
}

/// Write the layout if it has changed, clearing the dirty flag either way.
///
/// A failure is logged and dropped. Nothing the user is doing depends on this file, and
/// a full disk should not be allowed to end a conversation.
fn save_layout(app: &App, path: &std::path::Path) {
    if let Err(error) = app.layout().save(path) {
        tracing::warn!(?error, "could not save the layout");
    }
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    handle: &mut Handle,
    layout_path: &std::path::Path,
) -> Result<()> {
    let mut input = EventStream::new();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_layout_save = std::time::Instant::now();

    app.open_focused_view();
    dispatch(app, handle);

    loop {
        // A full clear discards ratatui's diff state, forcing every cell to be written
        // again. Needed when the screen and ratatui's model of it have diverged; see
        // App::focus_moved.
        //
        // Deliberately not `Terminal::clear`, which first reads the cursor position back
        // from the terminal: it writes `ESC[6n` and waits for the reply to arrive on
        // stdin. heddle's own `EventStream` owns stdin, so it swallows that reply as an
        // ordinary input event, crossterm times out after two seconds and the error
        // takes the whole app down. The round trip buys nothing here -- the next line
        // redraws every cell and `ui::draw` places the cursor itself -- so the buffer is
        // reset directly and the clear is issued as a plain escape sequence.
        if app.needs_redraw {
            crossterm::execute!(
                std::io::stdout(),
                crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
            )?;
            // Resets the "previous" buffer, so the next draw diffs against a blank slate
            // and rewrites every cell.
            terminal.swap_buffers();
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
                let since_epoch = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                // Hermes times approvals out server-side but the resolution event can
                // be lost; without this a pane would stay blocked for ever.
                app.agents.expire_pending(since_epoch.as_secs());
                // Typing notices are driven from here rather than from the keypress, so
                // that holding a key is not one request per character.
                app.tick_typing(since_epoch.as_millis() as u64);

                // Layout is flushed from the tick rather than from the keypress that
                // changed it: holding a resize key would otherwise be one write per
                // repeat. Throttled on top of that, because the tick is four times a
                // second and this file is worth almost nothing.
                if app.layout_dirty && last_layout_save.elapsed() >= LAYOUT_FLUSH {
                    save_layout(app, layout_path);
                    app.layout_dirty = false;
                    last_layout_save = std::time::Instant::now();
                }
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
                // Then borders. A press on the seam between two panes is the start of a
                // resize, not a click into whichever pane happens to own that cell.
                if app.begin_drag(mouse.column, mouse.row) {
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
            MouseEventKind::Drag(MouseButton::Left) => app.drag_to(mouse.column, mouse.row),
            MouseEventKind::Up(MouseButton::Left) => app.end_drag(),
            // Scrolling mid-drag would fight the resize, and a wheel event is not a
            // reason to let go of the border either.
            MouseEventKind::ScrollUp if !app.is_dragging() => {
                app.apply_action(keymap::Action::ScrollUp(3));
            }
            MouseEventKind::ScrollDown if !app.is_dragging() => {
                app.apply_action(keymap::Action::ScrollDown(3));
            }
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
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG)),
        )
        .init();

    Ok(guard)
}
