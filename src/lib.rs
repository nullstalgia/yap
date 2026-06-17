#![deny(unused_must_use)]

use std::{
    net::{SocketAddr, TcpStream},
    path::Path,
    str::FromStr,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use app::{App, CrosstermEvent};
use camino::Utf8PathBuf;

use clap::Parser;
use crokey::crossterm::event::{KeyCode, KeyModifiers};
use fs_err as fs;
use panic_handler::initialize_panic_handler;
use ratatui::crossterm::{
    self,
    event::{DisableMouseCapture, EnableMouseCapture, MouseButton, MouseEventKind},
};

use serialport::{SerialPortInfo, SerialPortType, UsbPortInfo};
use tracing::{Level, debug, error, level_filters::LevelFilter};
use tracing_appender::non_blocking::WorkerGuard;

use crate::{cli::YapCli, serial::DeserializedUsb, settings::Settings};

mod app;
mod buffer;
mod cli;

mod event_carousel;
mod keybinds;
#[cfg(feature = "macros")]
mod macros;
mod notifications;
mod panic_handler;
mod serial;
mod settings;
mod text_input;
mod traits;
mod tui;
#[cfg(feature = "update-check")]
mod updates;

static CONFIG_PARENT_PATH_CELL: OnceLock<Utf8PathBuf> = OnceLock::new();
/// Joins the given path to the current working directory (adjacent to configs and logs).
pub fn config_adjacent_path<P: Into<Utf8PathBuf>>(path: P) -> Utf8PathBuf {
    let path = path.into();
    let config_path = CONFIG_PARENT_PATH_CELL.get_or_init(|| {
        determine_working_directory()
            .expect("failed to determine working directory")
            .try_into()
            .expect("working directory is not valid utf-8")
    });

    config_path.join(path)
}

static EXECUTABLE_FILE_STEM: OnceLock<Utf8PathBuf> = OnceLock::new();
/// Returns name of executable stripped of executable suffix.
pub fn get_executable_name() -> Utf8PathBuf {
    let exec_name = EXECUTABLE_FILE_STEM.get_or_init(|| {
        let exe_pathbuf = std::env::current_exe().expect("failed to get path of executable");
        let exec_name_str = exe_pathbuf
            .file_stem()
            .expect("can't have file without name")
            .to_str()
            .expect("executable name is not valid utf-8");

        exec_name_str.into()
    });

    exec_name.to_owned()
}

/// Wrapper runner so any fatal errors get properly logged, and to
/// have a clear line between spinning up the whole app and all it's threads,
/// and just parsing CLI args and possibly exiting early.
pub fn run() -> color_eyre::Result<()> {
    let cli_args = YapCli::parse();

    if cli_args.print_actions {
        keybinds::print_all_actions();
        return Ok(());
    }

    // println!("{cli_args:#?}");
    // return Ok(());

    initialize_panic_handler()?;

    if let Some(path) = &cli_args.config_path {
        CONFIG_PARENT_PATH_CELL
            .set(path.to_owned())
            .expect("expected uninitialized cell");
    }

    let root_path = config_adjacent_path("");
    if !root_path.exists() {
        fs::create_dir_all(root_path)?;
    }

    let config_path = {
        let mut exec_name = get_executable_name();
        exec_name.set_extension("toml");
        config_adjacent_path(exec_name)
    };

    let settings = Settings::load(config_path)?;

    let listener_address = settings.misc.log_tcp_socket;

    let mut log_path = config_adjacent_path(get_executable_name());
    log_path.set_extension("log");
    let (_log_guard, tcp_log_health) =
        initialize_logging(settings.get_log_level(), log_path, listener_address)?;

    let result = run_inner(cli_args, settings, tcp_log_health);
    if let Err(e) = &result {
        error!("App closed with error:");
        for (index, err) in e.chain().enumerate() {
            error!("{index}: {err}");
        }
    }

    result
}

fn run_inner(
    cli_args: YapCli,
    app_settings: Settings,
    tcp_log_health: Arc<TcpStreamHealth>,
) -> color_eyre::Result<()> {
    let (tx, rx) = crossbeam::channel::unbounded::<app::Event>();
    let (crossterm_tx, crossterm_rx) = crossbeam::channel::unbounded::<CrosstermEvent>();
    let (ctrl_c_tx, ctrl_c_rx) = crossbeam::channel::bounded::<()>(1);
    let _crossterm_thread = std::thread::spawn(move || {
        use crokey::crossterm::event::{Event, KeyEventKind};

        #[derive(Debug)]
        struct SendError;
        impl std::fmt::Display for SendError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "failed to send crossterm event")
            }
        }
        impl std::error::Error for SendError {}
        const CTRL_C_ACK_MAX_WAIT: Duration = Duration::from_secs(5);
        const CTRL_C_BURST_PERIOD: Duration = Duration::from_secs(1);
        const CTRL_C_BURST_AMOUNT: u8 = 3;

        let mut ctrl_c_count_and_start: Option<(u8, Instant)> = None;

        let filter_and_send = |event: Event| -> Result<(), SendError> {
            let send_event = |crossterm_event: CrosstermEvent| -> Result<(), SendError> {
                crossterm_tx.send(crossterm_event).map_err(|_| SendError)?;
                Ok(())
            };
            match event {
                Event::Resize(_, _) => {
                    send_event(CrosstermEvent::Resize)?;
                }
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    send_event(CrosstermEvent::KeyPress(key))?
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        send_event(CrosstermEvent::MouseScroll { up: true })?;
                    }
                    MouseEventKind::ScrollDown => {
                        send_event(CrosstermEvent::MouseScroll { up: false })?;
                    }
                    MouseEventKind::Down(MouseButton::Right) => {
                        send_event(CrosstermEvent::RightClick)?;
                    }
                    _ => (),
                },
                _ => (),
            }
            Ok(())
        };

        loop {
            let event = match crossterm::event::read() {
                Ok(ev) => ev,
                Err(e) => {
                    // maybe i shouldn't break here..?
                    // one time when waking pc from an overnight sleep, i think this thread died
                    // but the rest was fine?
                    error!("error encountered when reading crossterm event, shutting down. {e}");
                    // return Err(e);
                    break;
                }
            };

            // Order matters, try to consume any un-seen acks even if we weren't expect one
            // such as from espflash action completion.
            if ctrl_c_rx.try_recv().is_ok() && ctrl_c_count_and_start.is_some() {
                _ = ctrl_c_count_and_start.take();
            }

            if let Event::Key(key) = &event
                && is_ctrl_c(key)
            {
                if let Some((count, _)) = ctrl_c_count_and_start.as_mut() {
                    *count += 1;
                } else {
                    ctrl_c_count_and_start = Some((1, Instant::now()));
                };

                if let Some((count, instant)) = ctrl_c_count_and_start.as_ref() {
                    let time_since_ctrl_c = instant.elapsed();

                    if time_since_ctrl_c > CTRL_C_ACK_MAX_WAIT {
                        panic!(
                            "Ctrl-C was not acknowledged within {CTRL_C_ACK_MAX_WAIT:?}, force exiting!"
                        );
                    } else if time_since_ctrl_c <= CTRL_C_BURST_PERIOD
                        && *count >= CTRL_C_BURST_AMOUNT
                    {
                        panic!(
                            "Ctrl-C burst (of {CTRL_C_BURST_AMOUNT}) was not acknowledged within {CTRL_C_BURST_PERIOD:?}, force exiting!"
                        );
                    }
                }
            }

            if let Err(e) = filter_and_send(event) {
                error!("{e}");
                break;
            }
        }
        debug!("Crossterm thread closed!");
    });

    // for p in ports {
    //     println!("{p:#?}");
    //     info!("{p:?}");
    // }

    // if let Err(e) = crokey::Combiner::default().enable_combining() {
    //     error!("Failed to enable key combining! {e}");
    // };

    let allow_first_time_setup = cli_args.port.is_none();

    let mut app = App::build(
        tx,
        rx,
        ctrl_c_tx,
        crossterm_rx,
        app_settings,
        tcp_log_health,
        allow_first_time_setup,
    )?;

    #[cfg(feature = "defmt")]
    if let Some(defmt_path) = cli_args.defmt_elf {
        match app::_try_load_defmt_elf(
            &defmt_path,
            &mut app.buffer.defmt_decoder,
            &mut app.defmt_helpers.recent_elfs,
            #[cfg(feature = "logging")]
            &app.buffer.log_handle,
            #[cfg(feature = "defmt-watch")]
            &mut app.defmt_helpers.watcher_handle,
        ) {
            Ok(None) => (),
            // If any kind of error occurs with a CLI-supplied ELF, break early.
            Ok(Some(locs_err)) => {
                Err(locs_err)?;
            }
            Err(e) => {
                Err(e)?;
            }
        }
    }

    if let Some(port) = cli_args.port {
        let port_info = if port.contains(':') {
            let usb_query = DeserializedUsb::from_str(&port)?;
            SerialPortInfo {
                port_name: String::new(),
                port_type: SerialPortType::UsbPort(UsbPortInfo::from(usb_query)),
            }
        } else {
            SerialPortInfo {
                port_name: port,
                port_type: SerialPortType::Unknown,
            }
        };

        app.try_cli_connect(port_info, cli_args.baud)?;
    };

    let terminal = ratatui::init();
    crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;

    let app_result = app.run(terminal);

    ratatui::restore();
    crossterm::execute!(std::io::stdout(), DisableMouseCapture)?;

    app_result
}

pub fn is_ctrl_c(key: &crossterm::event::KeyEvent) -> bool {
    key.kind == crossterm::event::KeyEventKind::Press
        && matches!(key.code, KeyCode::Char('c'))
        && key.modifiers == KeyModifiers::CONTROL
}

/// Shared health flag to be wrapped in an Arc and distributed.
pub struct TcpStreamHealth {
    ok: AtomicBool,
}
impl TcpStreamHealth {
    pub fn new(value: bool) -> Self {
        Self {
            ok: AtomicBool::new(value),
        }
    }
}
impl TcpStreamHealth {
    pub fn is_ok(&self) -> bool {
        self.ok.load(Ordering::Relaxed)
    }

    fn mark_failed(&self) {
        self.ok.store(false, Ordering::Relaxed);
    }
}

/// A simple newtype to monitor for failures during writing,
/// to hide a "connected" message on the main menu.
struct WrappedTcpStream {
    stream: TcpStream,
    health: Arc<TcpStreamHealth>,
}
impl WrappedTcpStream {
    fn new(stream: TcpStream, health: Arc<TcpStreamHealth>) -> Self {
        Self { stream, health }
    }
}
impl std::io::Write for WrappedTcpStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.stream.write(buf) {
            Ok(n) => Ok(n),
            Err(e) => {
                self.health.mark_failed();
                Err(e)
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self.stream.flush() {
            Ok(()) => Ok(()),
            Err(e) => {
                self.health.mark_failed();
                Err(e)
            }
        }
    }
}

pub fn initialize_logging<P: AsRef<Path>>(
    max_level: Level,
    log_file_path: P,
    log_socket_addr: Option<SocketAddr>,
) -> color_eyre::Result<(WorkerGuard, Arc<TcpStreamHealth>)> {
    let log_file_path = log_file_path.as_ref();
    use rolling_file::{BasicRollingFileAppender, RollingConditionBasic};
    // use tracing::info;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{
        fmt::time::ChronoLocal, layer::SubscriberExt, util::SubscriberInitExt,
    };
    // let console = console_subscriber::spawn();
    let file_appender = BasicRollingFileAppender::new(
        log_file_path,
        RollingConditionBasic::new().max_size(1024 * 1024 * 5),
        2,
    )
    .unwrap();
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    let time_fmt = ChronoLocal::new("%Y-%m-%d %H:%M:%S%.6f".to_owned());
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        // .pretty()
        .with_file(false)
        .with_ansi(false)
        .with_target(true)
        .with_timer(time_fmt.clone())
        .with_line_number(true)
        .with_filter(LevelFilter::from_level(max_level));

    // let (fmt_layer, reload_handle) = tracing_subscriber::reload::Layer::new(fmt_layer);
    // Allow everything through but limit a few crate's levels.
    let env_filter = tracing_subscriber::EnvFilter::new("trace,espflash=info,notify=debug");
    let registry = tracing_subscriber::registry()
        // .with(console)
        .with(env_filter)
        .with(fmt_layer);
    let tcp_socket_health = Arc::new(TcpStreamHealth::new(true));

    // Try to connect to tcp_log_listener
    if let Some(listener_addr) = log_socket_addr
        && let Ok(stream) = TcpStream::connect_timeout(&listener_addr, Duration::from_millis(50))
    {
        let stream = WrappedTcpStream::new(stream, tcp_socket_health.clone());

        let tcp_layer = tracing_subscriber::fmt::layer()
            .with_writer(Mutex::new(stream))
            // .pretty()
            .with_file(false)
            .with_ansi(true)
            .with_target(true)
            .with_timer(time_fmt)
            .with_line_number(true)
            .with_filter(LevelFilter::from_level(max_level));
        registry.with(tcp_layer).init();
    } else {
        tcp_socket_health.mark_failed();
        registry.init();
    }
    Ok((guard, tcp_socket_health))
}

/// Returns the directory that logs, config, and other files should be placed in by default (if no override was given).
// The rules for how it determines the directory is as follows:
// If the app is built with the portable feature, it will just return it's parent directory.
// If there is a config file present adjacent to the executable, the executable's parent path is returned.
// Otherwise, it will return the `directories` `config_dir` output.
//
// Debug builds are always portable by default. Release builds can optionally have the "portable" feature enabled.
fn determine_working_directory() -> Option<std::path::PathBuf> {
    let exe_path = std::env::current_exe().expect("Failed to get executable path");
    let exe_parent = exe_path
        .parent()
        .expect("Couldn't get parent dir of executable")
        .to_path_buf();
    let config_path = exe_path.with_extension("toml");

    if default_to_portable() || config_path.exists() {
        Some(exe_parent)
    } else {
        get_user_dir()
    }
}

#[cfg(any(debug_assertions, feature = "portable"))]
fn default_to_portable() -> bool {
    true
}

#[cfg(not(any(debug_assertions, feature = "portable")))]
fn default_to_portable() -> bool {
    false
}

#[cfg(any(debug_assertions, feature = "portable"))]
fn get_user_dir() -> Option<std::path::PathBuf> {
    None
}

#[cfg(not(any(debug_assertions, feature = "portable")))]
fn get_user_dir() -> Option<std::path::PathBuf> {
    if let Some(base_dirs) = directories::BaseDirs::new() {
        let mut config_dir = base_dirs.config_dir().to_owned();
        config_dir.push(env!("CARGO_PKG_NAME"));
        Some(config_dir)
    } else {
        None
    }
}

#[macro_export]
/// Macro to check if any field has changed between two objects.
///
/// Usage: `changed!(old, new, field_name1, field_name2, ...)`
macro_rules! changed {
    // I technically don't need to keep the single field version,
    // but rust-analyzer doesn't suggest field names on the multi-field version,
    // and I wanted to keep the ergonomics of using LSP-suggested names
    // instead of going to the type myself.
    ($a:expr, $b:expr, $field:ident) => {
        ($a.$field != $b.$field)
    };
    ($a:expr, $b:expr, $($field:ident),+) => {
        {
            false $(|| $a.$field != $b.$field)+
        }
    };
}
