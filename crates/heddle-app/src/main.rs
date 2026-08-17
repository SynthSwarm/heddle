//! heddle — an agent-native Matrix client for the terminal.
//!
//! See `docs/SPEC.md` for the design and `docs/PLAN.md` for the delivery plan.

// The matrix-sdk async state machines nest deeply enough to exceed the default limit
// when the worker's futures are monomorphised into this crate.
#![recursion_limit = "512"]

mod app;
mod composer;
mod config;
mod doctor;
mod emoji;
mod keymap;
mod palette;
mod ui;

use anyhow::{Context, Result};
use app::{App, Geometry, EVENT_BUDGET};
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
use heddle_matrix::{session, Dispatch, Handle};
use std::io::stdout;
use std::time::Duration;

/// Log filter used when `HEDDLE_LOG` is unset.
///
/// Every heddle crate: `heddle=info` alone silences the worker.
const DEFAULT_LOG: &str = "heddle=info,heddle_matrix=info,heddle_agent=info,\
                           heddle_render=info,heddle_layout=info,warn";

/// How often to tick even with no input, so countdowns and spinners advance.
const TICK: Duration = Duration::from_millis(250);

/// Shortest interval between layout writes. A terminal killed outright loses at most
/// this much rearranging.
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

    #[arg(long)]
    check: bool,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Log in and save a session.
    ///
    /// Adds a matching `[profile.<name>]` block to config.toml if one is not there. The
    /// first profile added becomes the default.
    Login {
        #[arg(long)]
        homeserver: String,
        #[arg(long)]
        user: String,
        /// Environment variable holding the password.
        ///
        /// Never an argument: that leaks into shell history and the process table.
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
            // Normalised before it is used rather than before it is written, so the
            // session and the config block are created from the same string. Writing a
            // corrected value while logging in with the raw one is how a profile ends up
            // pointing somewhere the login never touched.
            let homeserver = config::normalise_homeserver(&homeserver);
            let name = cli.profile.as_deref().unwrap_or("default");
            let paths = session::Paths::for_profile(&dirs.data, name);
            let (client, cross_signing) =
                session::login_password(&homeserver, &user, &password, "heddle", &paths).await?;
            println!("logged in as {user}; profile `{name}` saved");

            // From the login response, not the typed `--user`, which may be a bare
            // localpart; the config wants the full `@user:server`.
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
                // Name the profile that was asked for; "none selected" is wrong when
                // one was and simply is not in the file.
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
        return check(&profile_name, &profile.homeserver, &dirs).await;
    }

    let paths = session::Paths::for_profile(&dirs.data, &profile_name);
    let client = session::restore(&profile_name, &paths)
        .await
        .with_context(|| format!("restoring profile `{profile_name}`"))?;

    let handle = heddle_matrix::spawn(client, config.agent.adapters())
        .await
        .context("starting the matrix worker")?;

    let layout_path = dirs.layout_file(&profile_name);
    let app = App::new(config, heddle_layout::Layout::load(&layout_path));

    tui(app, handle, layout_path).await
}

/// Three independent things can be wrong before the first frame with the same symptom,
/// so every section runs even when an earlier one fails.
async fn check(profile_name: &str, homeserver: &str, dirs: &Dirs) -> Result<()> {
    let mut fatal = false;

    println!("homeserver: {homeserver}");
    match heddle_matrix::check_homeserver(homeserver).await {
        Ok(caps) => {
            println!("  sliding sync (MSC4186):  {}", tick(caps.sliding_sync));
            println!("  threads (MSC3440):       {}", tick(caps.threads));
            println!("  cross-signing:           {}", tick(caps.cross_signing));
            println!("  spec versions:           {}", caps.versions.join(", "));
            for problem in caps.problems() {
                println!("  ! {problem}");
            }
            fatal |= !caps.is_usable();
        }
        Err(e) => {
            // Unreachable is not unusable; conflating them sends the user hunting for
            // a capability problem that is really a network one.
            println!("  unreachable:             {e}");
            fatal = true;
        }
    }

    println!("\nterminal:");
    fatal |= report(&doctor::terminal());

    let paths = session::Paths::for_profile(&dirs.data, profile_name);
    println!("\nstore (profile `{profile_name}`):");
    fatal |= report(&doctor::store(profile_name, &paths.store, &paths.session));

    if fatal {
        anyhow::bail!("heddle will not run correctly until the failures above are fixed");
    }
    println!("\nok");
    Ok(())
}

fn report(findings: &[doctor::Finding]) -> bool {
    let mut fatal = false;
    for finding in findings {
        println!("  {:24} {}", format!("{}:", finding.name), finding.status);
        if let Some(detail) = &finding.detail {
            println!("      {detail}");
        }
        fatal |= finding.status == doctor::Status::Fail;
    }
    fatal
}

fn tick(ok: bool) -> &'static str {
    if ok {
        "yes"
    } else {
        "NO"
    }
}

async fn tui(mut app: App, mut handle: Handle, layout_path: std::path::PathBuf) -> Result<()> {
    let mouse = app.config.ui.mouse;
    let mut terminal = enter_terminal(mouse)?;

    // Restore on panic too: a raw-mode terminal left behind needs a blind `reset`.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal(mouse);
        hook(info);
    }));

    let result = event_loop(&mut terminal, &mut app, &mut handle, &layout_path).await;

    // Before anything that can fail, whatever went wrong.
    save_layout(&app, &layout_path);

    restore_terminal(mouse)?;
    handle.shutdown().await;
    result
}

/// Write the layout if it has changed, clearing the dirty flag either way.
///
/// Failures are logged and dropped: a full disk must not end a conversation.
fn save_layout(app: &App, path: &std::path::Path) {
    if let Err(error) = app.layout().save(path) {
        tracing::warn!(?error, "could not save the layout");
    }
}

/// Blank the screen and make the next draw rewrite every cell.
///
/// For when the screen and ratatui's model of it have diverged: a glyph painting wider
/// than it measured leaves stale cells ratatui believes are correct. See
/// `App::focus_moved`.
///
/// The clear is a raw escape sequence, not `Terminal::clear`, which first writes
/// `ESC[6n` and waits on stdin for the cursor position. heddle's `EventStream` owns
/// stdin and swallows the reply as an input event, so crossterm times out after two
/// seconds and the error takes the client down.
///
/// One `swap_buffers`, not two: `Terminal::draw` already swaps at the end of every
/// frame, so the current buffer is empty and the previous holds the last frame. The
/// single swap here clears that previous buffer too.
fn force_full_redraw(terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
    )?;
    terminal.swap_buffers();
    Ok(())
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
        if app.needs_redraw {
            force_full_redraw(terminal)?;
            app.needs_redraw = false;
        }

        // Before the frame, not during it: the tiling has to be told which rectangle it
        // is laying out into, and that is a decision. `autoresize` first so the area
        // asked about is the one the frame will actually get.
        terminal.autoresize()?;
        let area = terminal.get_frame().area();
        let placements = app.lay_out_panes(ui::pane_band(app, area));

        let mut geometry = Geometry::default();
        terminal.draw(|frame| geometry = ui::draw(frame, app, &placements))?;
        app.geometry = geometry;

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
                // Fold what else has arrived, up to the budget, so a burst costs one
                // frame rather than one each.
                for extra in handle.drain(EVENT_BUDGET) {
                    app.apply_worker_event(extra);
                }
            }

            _ = ticker.tick() => {
                let since_epoch = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                // Hermes times approvals out server-side, but the resolution event can
                // be lost and the pane would stay blocked.
                app.agents.expire_pending(since_epoch.as_secs());
                // From the tick, not the keypress: otherwise one request per
                // character.
                app.tick_typing(since_epoch.as_millis() as u64);

                // From the tick, not the keypress that changed it, and throttled on
                // top: holding a resize key would be one write per repeat.
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
                // Bars own their rows, so a click there must not fall through.
                if app.click_bar(mouse.column, mouse.row) {
                    return;
                }
                // A press on the seam between panes starts a resize.
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
            // Scrolling mid-drag would fight the resize.
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

fn dispatch(app: &mut App, handle: &Handle) {
    for command in app.take_commands() {
        match handle.send(command) {
            Dispatch::Queued => {}
            // The worker is alive, so this is a status line rather than a shutdown --
            // but if it was a message, the user watched it vanish.
            Dispatch::Dropped => {
                app.status = Some("the worker is behind; a command was dropped".into());
            }
            Dispatch::Stopped => {
                app.status = Some("worker stopped".into());
                app.should_quit = true;
                return;
            }
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use ratatui::backend::{Backend, ClearType, TestBackend};
    use ratatui::text::Line;
    use ratatui::widgets::Paragraph;
    use ratatui::Terminal;

    /// Draw the same frame twice with a forced redraw in between, having blanked the
    /// backend to stand in for the escape sequence heddle writes to the real terminal.
    ///
    /// The frame deliberately does not change between draws. A test that varied the
    /// content would pass whether or not the redraw path works, since changed cells get
    /// repainted either way; the whole question is whether *unchanged* cells survive
    /// having the screen cleared underneath them.
    #[test]
    fn a_forced_redraw_rewrites_cells_that_did_not_change() {
        let backend = TestBackend::new(12, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");

        let render = |frame: &mut ratatui::Frame| {
            frame.render_widget(Paragraph::new(Line::from("hello")), frame.area());
        };

        terminal.draw(render).expect("first draw");
        terminal.backend().assert_buffer_lines(["hello       "]);

        // Stand in for the physical clear: the screen is now blank.
        terminal
            .backend_mut()
            .clear_region(ClearType::All)
            .expect("clear");
        terminal.swap_buffers();

        terminal.draw(render).expect("second draw");
        terminal.backend().assert_buffer_lines(["hello       "]);
    }
}
