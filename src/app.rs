use std::{
    borrow::Cow,
    collections::VecDeque,
    sync::Arc,
    thread::JoinHandle,
    time::{Duration, Instant},
};

use chrono::{DateTime, Local};

use color_eyre::eyre::{Context, Result};
use crokey::{KeyCombination, key};
use crossbeam::channel::{Receiver, Select, Sender, TrySendError};
use enum_rotate::EnumRotate;

use ratatui::{
    Frame, Terminal,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::{Constraint, Layout, Margin, Offset, Rect, Size},
    prelude::Backend,
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, HighlightSpacing, Paragraph, Row, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Table, TableState,
    },
};

use ratatui_macros::{horizontal, line, span, vertical};
use serialport::{SerialPortInfo, SerialPortType, UsbPortInfo};
use struct_table::{ArrowKey, StructTable};
use strum::{VariantArray, VariantNames};
use takeable::Takeable;

use tinyvec::ArrayVec;
use tracing::{debug, error, info, trace, warn};
use tui_big_text::{BigText, PixelSize};
use tui_input::{Input, backend::crossterm::EventHandler};

use crate::{
    TcpStreamHealth,
    buffer::Buffer,
    config_adjacent_path,
    event_carousel::CarouselHandle,
    is_ctrl_c,
    keybinds::{Action, BaseAction, BuiltinAction, Keybinds, PortAction, ShowPopupAction},
    notifications::{EMERGE_TIME, EXPAND_TIME, EXPIRE_TIME, Notifications, PAUSE_AND_SHOW_TIME},
    serial::{
        DeserializedUsb, PrintablePortInfo, ReconnectType, Reconnections, SerialDisconnectReason,
        SerialEvent,
        handle::{BlockingCommandError, SerialHandle},
        port_status::InnerPortStatus,
        worker::MOCK_PORT_NAME,
    },
    settings::{Behavior, PortSettings, Rendering, Settings},
    text_input::TextInput,
    traits::{FirstChars, LastIndex, LineHelpers, RequiresPort, ToggleBool},
    tui::{
        POPUP_MENU_SELECTOR_COUNT, centered_rect_size,
        color_rules::{COLOR_RULES_PATH, ColorRuleLoadError, ColorRules},
        prompts::{
            AttemptReconnectPrompt, DisconnectPrompt, IgnorePortByNamePrompt,
            IgnoreUsbDevicePrompt, PromptKeybind, PromptTable,
        },
        show_keybinds,
        single_line_selector::{SingleLineSelector, SingleLineSelectorState},
    },
    updates::{UpdateBeginPrompt, UpdateCheckConsentPrompt, UpdateHandle},
};

#[cfg(all(windows, feature = "self-replace"))]
use crate::updates::UpdateLaunchPrompt;

#[cfg(feature = "defmt")]
use crate::{
    buffer::defmt::{DefmtDecoder, DefmtLoadError, LocationsError},
    keybinds::DefmtSelectAction,
    settings::Defmt,
    tui::defmt::{DefmtHelpers, DefmtRecentElfs, DefmtRecentError},
};
#[cfg(feature = "defmt")]
use {camino::Utf8Path, ratatui_explorer::FileExplorer};

#[cfg(feature = "defmt-watch")]
use crate::buffer::defmt::elf_watcher::{ElfWatchEvent, ElfWatchHandle, ElfWatcherMissing};

#[cfg(feature = "macros")]
use crate::{
    keybinds::MacroBuiltinAction,
    macros::{MacroNameTag, MacroNotFound, Macros},
};

#[cfg(feature = "logging")]
use crate::{
    buffer::LoggingEvent, keybinds::LoggingAction, settings::Logging,
    tui::logging::sync_logs_button,
};

#[cfg(feature = "espflash")]
use crate::{
    keybinds::EspBuiltinAction,
    serial::{
        esp::{EspEvent, EspRestartType},
        handle::SerialWorkerMissing,
    },
    tui::esp::{self, EspFlashHelper},
};

use crate::updates::UpdateEvent;

#[derive(Clone, Debug)]
pub enum CrosstermEvent {
    Resize,
    KeyPress(KeyEvent),
    MouseScroll {
        up: bool,
    },
    /// Currently the only mouse click event handled, for pasting.
    RightClick,
}

impl From<CrosstermEvent> for Event {
    fn from(value: CrosstermEvent) -> Self {
        Self::Crossterm(value)
    }
}

#[derive(Debug)]
pub enum Event {
    /// Terminal-based event.
    Crossterm(CrosstermEvent),
    /// Serial port connection events/signals/errors.
    Serial(SerialEvent),
    /// Bytes recieved from attached device and when.
    RxBuffer((DateTime<Local>, Vec<u8>)),
    /// Typically sent from event_carousel to trigger UI updates/actions on a timer.
    Tick(Tick),
    #[cfg(feature = "logging")]
    /// Sent when log sync finishes or an error occurs.
    Logging(LoggingEvent),
    #[cfg(feature = "defmt-watch")]
    /// Sent when watched defmt ELF changes or an error occurs.
    DefmtElfWatch(ElfWatchEvent),
    #[cfg(feature = "defmt")]
    /// User selected a defmt ELF from an OS-provided file picker.
    // TODO have error too?
    DefmtFromFilePicker(camino::Utf8PathBuf),
    /// Update notifications and progress.
    Updates(UpdateEvent),
    /// Begin closing app gracefully.
    Quit,
}

#[derive(Clone, Debug)]
pub enum Tick {
    /// Sent every second
    PerSecond,
    /// When just trying to update the UI a little early
    ///
    /// `&str` has the origin of the update request
    Requested(&'static str),
    /// Used to twiddle repeating_line_flip for each transmission
    Tx,
    /// Used to periodically scroll long text for UIs
    Scroll,
    /// Used to force UI updates when a notification is on screen
    Notification,
    /// Used to trigger further consumption of the Action Queue
    Action,
}

impl From<Tick> for Event {
    fn from(value: Tick) -> Self {
        Self::Tick(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// The main screens of the application.
pub enum Menu {
    PortSelection,
    Terminal,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub enum RunningState {
    #[default]
    Running,
    Finished,
}

#[derive(Debug, Clone, PartialEq, Eq, EnumRotate, VariantArray, VariantNames)]
#[repr(u8)]
#[strum(serialize_all = "title_case")]
pub enum SettingsMenu {
    SerialPort,
    Rendering,
    Behavior,
    #[cfg(feature = "logging")]
    Logging,
    #[cfg(feature = "defmt")]
    #[strum(serialize = "defmt")]
    Defmt,
}

#[cfg(any(feature = "espflash", feature = "macros"))]
#[derive(Debug, Clone, PartialEq, Eq, EnumRotate, VariantArray, VariantNames)]
#[repr(u8)]
#[strum(serialize_all = "title_case")]
pub enum ToolMenu {
    #[cfg(feature = "macros")]
    Macros,
    #[cfg(feature = "espflash")]
    #[strum(serialize = "ESP32 Flashing")]
    EspFlash,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq)]
pub enum Popup {
    SettingsMenu(SettingsMenu),
    #[cfg(any(feature = "espflash", feature = "macros"))]
    ToolMenu(ToolMenu),
    CurrentKeybinds,
    #[cfg(feature = "defmt")]
    DefmtNewElf(FileExplorer),
    #[cfg(feature = "defmt")]
    DefmtRecentElf,

    DisconnectPrompt,
    AttemptReconnectPrompt,
    IgnoreByUsb(String, UsbPortInfo),
    IgnoreByName(String),
    SerialConnectionFailed(String),

    UpdateCheckConsentPrompt,

    UpdateBeginPrompt,
    #[cfg(all(windows, feature = "self-replace"))]
    UpdateLaunchPrompt,
    #[cfg(feature = "self-replace")]
    UpdateDownloading(f64),
}

#[cfg(any(feature = "espflash", feature = "macros"))]
impl From<ToolMenu> for Popup {
    fn from(value: ToolMenu) -> Self {
        Self::ToolMenu(value)
    }
}
impl From<SettingsMenu> for Popup {
    fn from(value: SettingsMenu) -> Self {
        Self::SettingsMenu(value)
    }
}

// 0 is for a custom baud rate
/// Default baud options to give to user in port selection and settings popup.
pub const COMMON_BAUD: &[u32] = &[
    4800, 9600, 19200, 38400, 57600, 74880, 115200, 230400, 460800, 921600, 0,
];
const COMMON_BAUD_DEFAULT: usize = 6;

pub const DEFAULT_BAUD: u32 = {
    let baud = COMMON_BAUD[COMMON_BAUD_DEFAULT];
    assert!(baud == 115200);
    baud
};

/// Same as COMMON_BAUD but without the `0`/`Custom` entry.
pub const COMMON_BAUD_TRUNC: &[u32] = {
    let common_len_truncated = COMMON_BAUD.len() - 1;
    let (truncated, _) = COMMON_BAUD.split_at(common_len_truncated);
    truncated
};

/// How long to keep the input bar Red after trying to send to a disconnected port.
const FAILED_SEND_VISUAL_TIME: Duration = Duration::from_millis(750);

/// Max time to wait before erroring when connecting to a port.
const CONNECT_ATTEMPT_BLOCK_MAX: Duration = Duration::from_secs(15);

/// Max time to wait before erroring when recieving initial available ports.
const SCAN_BLOCK_MAX: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
enum NoSenders {
    #[error("Serial Buffer sender has hung up unexpectedly!")]
    SerialRx,
    #[error("All event senders have hung up unexpectedly!")]
    Events,
    #[error("Crossterm thread hung up unexpectedly!")]
    Crossterm,
}

// Maybe have the buffer in a TUI struct?

pub struct App {
    /// Settings loaded from `yap.toml`
    pub settings: Settings,
    /// Scratchpad space used by popups to edit safely.
    scratch: Settings,

    state: RunningState,
    menu: Menu,
    port_selection_scroll: usize,

    event_tx: Sender<Event>,
    event_rx: Receiver<Event>,
    serial_buf_rx: Receiver<(DateTime<Local>, Vec<u8>)>,
    crossterm_rx: Receiver<CrosstermEvent>,

    baud_selection_state: SingleLineSelectorState,
    /// User input destination for custom baud
    /// TODO update this and state properly when selecting a baud from settings menu
    baud_input: Input,

    pub popup: Option<Popup>,
    /// Selection index of current popup
    popup_menu_scroll: usize,
    /// Horizontal text scroll for Popup hints
    popup_hint_scroll: i32,

    pub notifs: Notifications,
    /// Last seen available ports to connect to,
    /// ignored_devices filtered out.
    ports: Vec<SerialPortInfo>,
    /// Handle to serial worker
    serial: SerialHandle,
    serial_thread: Takeable<JoinHandle<()>>,
    /// Handle to event carousel
    carousel: CarouselHandle,
    carousel_thread: Takeable<JoinHandle<()>>,

    /// User input destination and history for Pseudo-shell
    text_input: TextInput,

    /// Raw byte and post-processed lines storage
    pub buffer: Buffer,

    /// Flip the repeating pattern at the bottom of the Terminal view.
    ///
    /// ~-~-~- -> -~-~-~
    repeating_line_flip: bool,

    /// Last failed send attempt timestamp,
    /// for highlighting Input bar Red momentarily.
    failed_send_at: Option<Instant>,

    /// If `true` when not using Pseudo-shell, next key/keycombo
    /// will be handled by the app rather than sending to the port.
    escape_next_keypress: bool,

    /// When not using Psuedo-shell, the last transmitted key's codes
    /// are stored here to be displayed to the user.
    last_raw_sequence: ArrayVec<[u8; 16]>,

    keybinds: Keybinds,

    #[cfg(feature = "macros")]
    macros: Macros,

    /// Queue for actions from keybinds with multiple actions,
    /// and the keycombo thet triggered them
    action_queue: VecDeque<(KeyCombination, Action)>,

    /// User chose to break connection _and_ stay on Terminal view.
    user_broke_connection: bool,

    #[cfg(feature = "espflash")]
    espflash: EspFlashHelper,

    #[cfg(feature = "defmt")]
    /// Recent defmt ELFs and file update watcher handle
    pub defmt_helpers: DefmtHelpers,

    /// Each Ctrl-C must be acknowledged back to the thread actually recieving terminal events.
    ///
    /// If enough aren't acknowledged within a period of time,
    /// the crossterm thread has the authority to panic-close the whole app.
    ctrl_c_tx: Sender<()>,

    /// If sending Tracing events to `tcp_log_socket` ever fails, `ok` will contain `false`, and
    /// thus the main menu hint saying "we're connected and logging" should be hidden.
    tcp_log_health: Arc<TcpStreamHealth>,

    pub update_worker: UpdateHandle,
    pub update_found_version: Option<String>,

    /// If connecting directly to a port via CLI,
    /// assume "No, ask again later" to all first-time-setup questions
    /// and just let the user connect and work.
    allow_first_time_setup: bool,
}

impl App {
    /// Load all remaining configs and spin up worker threads.
    pub fn build(
        event_tx: Sender<Event>,
        event_rx: Receiver<Event>,
        ctrl_c_tx: Sender<()>,
        crossterm_rx: Receiver<CrosstermEvent>,
        settings: Settings,
        tcp_log_health: Arc<TcpStreamHealth>,
        allow_first_time_setup: bool,
    ) -> Result<Self> {
        let keybinds = Keybinds::load()?;

        let buffer_input = TextInput::default();

        let saved_baud_rate = settings.serial.baud_rate;
        let (baud_input, baud_index) = {
            if let Some(idx) = COMMON_BAUD.iter().position(|b| *b == saved_baud_rate) {
                (Input::default(), idx)
            } else {
                (
                    Input::new(saved_baud_rate.to_string()),
                    COMMON_BAUD.last_index(),
                )
            }
        };

        // debug!("{settings:#?}");

        let (serial_buf_tx, serial_buf_rx) = crossbeam::channel::unbounded();

        let (event_carousel, carousel_thread) = CarouselHandle::new(event_tx.clone());
        let (serial_handle, serial_thread, ports) = SerialHandle::build(
            event_tx.clone(),
            serial_buf_tx,
            settings.serial.clone(),
            settings.ignored_devices.clone(),
            SCAN_BLOCK_MAX,
        )
        .wrap_err("failed to build serial worker")?;

        event_carousel.add_repeating("PerSecond", Tick::PerSecond, Duration::from_secs(1))?;

        let line_ending = settings.serial.rx_line_ending.as_bytes();

        #[cfg(feature = "defmt")]
        let mut defmt_helpers = DefmtHelpers::build(
            #[cfg(feature = "defmt-watch")]
            event_tx.clone(),
        )?;

        let color_rules = ColorRules::load_from_file(config_adjacent_path(COLOR_RULES_PATH))?;

        let buffer = Buffer::new(
            line_ending,
            color_rules,
            &settings,
            #[cfg(feature = "logging")]
            event_tx.clone(),
        );

        // Silly but simple, since this doesn't always need to be mutable, and Clippy wants things proper.
        #[cfg(feature = "defmt")]
        let mut buffer = buffer;

        #[cfg(feature = "defmt")]
        if let Some(last_elf) = defmt_helpers.recent_elfs.last().map(ToOwned::to_owned)
            && last_elf.is_file()
        {
            match _try_load_defmt_elf(
                &last_elf,
                &mut buffer.defmt_decoder,
                &mut defmt_helpers.recent_elfs,
                #[cfg(feature = "logging")]
                &buffer.log_handle,
                #[cfg(feature = "defmt-watch")]
                &mut defmt_helpers.watcher_handle,
            ) {
                Ok(None) => (),
                Ok(Some(locs_err)) => {
                    use tracing::warn;
                    // TODO notify_str when we have history, so this won't clobber
                    // anything important.
                    warn!("Location Error from loaded ELF: {locs_err}");
                }
                Err(e) => {
                    let text = format!("loading last defmt ELF failed! {e}");
                    error!("{text}");
                    // self.notifs.notify_str(text, Color::Red);
                }
            }
        }

        #[cfg(feature = "macros")]
        let macros = {
            let (macros, errors) =
                Macros::load_from_folder(config_adjacent_path(crate::macros::MACROS_DIR_PATH))?;
            if let Some(e) = errors.into_iter().next() {
                return Err(e)?;
            }
            macros
        };

        let update_worker = UpdateHandle::new(event_tx.clone());

        if settings.updates.allow_checking_for_updates {
            update_worker.query_latest(settings.updates.allow_pre_releases)?;
        }

        // debug!("{buffer:#?}");
        Ok(Self {
            state: RunningState::Running,
            menu: Menu::PortSelection,
            port_selection_scroll: 0,
            popup: None,
            popup_hint_scroll: -2,
            baud_selection_state: SingleLineSelectorState::new().with_selected(baud_index),
            baud_input,
            popup_menu_scroll: 0,

            ports,

            carousel: event_carousel,
            carousel_thread: Takeable::new(carousel_thread),

            serial: serial_handle,
            serial_thread: Takeable::new(serial_thread),

            text_input: buffer_input,

            buffer,

            repeating_line_flip: false,
            failed_send_at: None,
            escape_next_keypress: false,
            last_raw_sequence: ArrayVec::new(),

            #[cfg(feature = "macros")]
            macros,
            action_queue: VecDeque::new(),
            scratch: settings.clone(),
            settings,
            keybinds,
            notifs: Notifications::new(event_tx.clone()),
            event_tx,
            event_rx,
            serial_buf_rx,
            crossterm_rx,

            #[cfg(feature = "espflash")]
            espflash: { EspFlashHelper::build().wrap_err("failed to load espflash profiles")? },

            #[cfg(feature = "defmt")]
            defmt_helpers,

            user_broke_connection: false,

            ctrl_c_tx,
            tcp_log_health,

            update_found_version: None,
            update_worker,
            allow_first_time_setup,
        })
    }
    fn is_running(&self) -> bool {
        self.state == RunningState::Running
    }
    // All prep has been complete, begin main loop.
    pub fn run(&mut self, mut terminal: Terminal<impl Backend>) -> Result<()> {
        if self.allow_first_time_setup {
            self.first_time_setup();
        }
        // Get initial size of buffer.
        self.buffer.update_terminal_size(&mut terminal)?;
        let mut max_draw = Duration::default();
        let mut max_rx_handle = Duration::default();
        let mut max_event_handle = Duration::default();
        let final_app_result = loop {
            let start = Instant::now();
            // TODO performance widget?
            self.draw(&mut terminal)?;
            let end = Instant::now();
            let end1 = end.saturating_duration_since(start);
            max_draw = max_draw.max(end1);

            // Waiting until we either get a normal app event,
            // a crossterm event, or an incoming serial buffer.
            //
            // Serial devices can sometimes just go *wild* and spit data like crazy,
            // and having other events like port (dis)connection, input handling, etc.,
            // potentially sitting behind queued serial buffers isn't ideal.
            // So Serial RX buffers and Crossterm events have their own dedicated channels,
            // and when either of them or the shared App Event channel wakes up the Select,
            // we check all three channels, draw the UI, and wait again.
            let mut channel_notifier = Select::new();
            channel_notifier.recv(&self.event_rx);
            channel_notifier.recv(&self.serial_buf_rx);
            channel_notifier.recv(&self.crossterm_rx);
            // Waiting...
            let _ready_index = channel_notifier.ready();
            // A channel is ready!
            let start2 = Instant::now();

            // If we're on our serial terminal screen...
            if self.menu == Menu::Terminal {
                // Normal byte-recieving behavior.
                match self.serial_buf_rx.try_recv() {
                    Ok(buf) => self.handle_event(Event::RxBuffer(buf), &mut terminal)?,
                    Err(crossbeam::channel::TryRecvError::Empty) => (),
                    Err(crossbeam::channel::TryRecvError::Disconnected) => {
                        return Err(NoSenders::SerialRx)?;
                    }
                }
            } else {
                // Otherwise, discard whatever we get.
                let mut bytes_discarded = 0;
                while let Ok((_timestamp, vec_to_discard)) = self.serial_buf_rx.try_recv() {
                    bytes_discarded += vec_to_discard.len();
                }
                if bytes_discarded > 0 {
                    warn!(
                        "RX buffer(s) received on port selection screen, discarding {bytes_discarded} bytes!"
                    );
                }
            }

            let end2 = start2.elapsed();
            let start3 = Instant::now();

            match self.crossterm_rx.try_recv() {
                Ok(event) => self.handle_event(event.into(), &mut terminal)?,
                Err(crossbeam::channel::TryRecvError::Empty) => (),
                Err(crossbeam::channel::TryRecvError::Disconnected) => {
                    return Err(NoSenders::Crossterm)?;
                }
            }

            match self.event_rx.try_recv() {
                Ok(event) => self.handle_event(event, &mut terminal)?,
                Err(crossbeam::channel::TryRecvError::Empty) => (),
                Err(crossbeam::channel::TryRecvError::Disconnected) => {
                    return Err(NoSenders::Events)?;
                }
            }

            let end3 = start3.elapsed();
            max_rx_handle = max_rx_handle.max(end2);
            max_event_handle = max_event_handle.max(end3);
            trace!(
                "Frame took {:?} to draw (max: {max_draw:?}), {:?} to handle RX (max: {max_rx_handle:?}), {:?} to handle event (max: {max_event_handle:?}) ",
                end1, end2, end3
            );
            // debug!("{msg:?}");

            // Don't wait for another loop iteration to start shutting down workers.
            if !self.is_running() {
                break Ok(());
            }
        };
        // Shutting down worker threads, with timeouts
        // do i even need to bother joining threads?
        // seems like good practice, so i _do_ do it, but blech
        debug!("Shutting down Serial worker");
        if self.serial.shutdown().is_ok() {
            let serial_thread = self.serial_thread.take();
            if serial_thread.join().is_err() {
                error!("Serial thread closed with an error!");
            }
        }
        debug!("Shutting down event carousel");
        if self.carousel.shutdown().is_ok() {
            let carousel = self.carousel_thread.take();
            if carousel.join().is_err() {
                error!("Carousel thread closed with an error!");
            }
        }
        final_app_result
    }
    fn handle_event(&mut self, event: Event, terminal: &mut Terminal<impl Backend>) -> Result<()> {
        match event {
            Event::Quit => self.shutdown(),

            Event::RxBuffer((timestamp, data)) => {
                self.buffer.fresh_rx_bytes(timestamp, data);
                self.buffer.scroll_by(0);

                self.repeating_line_flip.flip();
            }

            // TODO force re-draw every minute or so?
            Event::Crossterm(CrosstermEvent::Resize) => {
                terminal.autoresize()?;
                self.buffer.update_terminal_size(terminal)?;
            }
            Event::Crossterm(CrosstermEvent::KeyPress(key)) => self.handle_key_press(key)?,
            Event::Crossterm(CrosstermEvent::MouseScroll { up })
                if matches!(self.popup, Some(Popup::CurrentKeybinds)) =>
            {
                if up {
                    self.popup_menu_scroll = self.popup_menu_scroll.saturating_sub(1);
                } else {
                    self.popup_menu_scroll += 1;
                }
            }
            Event::Crossterm(CrosstermEvent::MouseScroll { up }) => {
                let amount = if up { 1 } else { -1 };
                self.buffer.scroll_by(amount);
            }

            Event::Crossterm(CrosstermEvent::RightClick)
                if self.menu == Menu::Terminal && self.popup.is_none() =>
            {
                if let Some(clipboard) = &mut self.text_input.clipboard {
                    match clipboard.get_text() {
                        Ok(clipboard_text) => {
                            self.text_input.append_to_input(&clipboard_text);
                        }
                        Err(e) => {
                            error!("error getting clipboard text: {e}");
                        }
                    }
                }
            }
            Event::Crossterm(CrosstermEvent::RightClick) => {}

            Event::Serial(SerialEvent::Connected(reconnect)) => {
                // Sync up the per-second tick for the "connected for" timer.
                self.carousel.add_repeating(
                    "PerSecond",
                    Tick::PerSecond,
                    Duration::from_secs(1),
                )?;
                if let Some(reconnect_type) = &reconnect {
                    info!("Reconnected!");
                    let text = match reconnect_type {
                        ReconnectType::PerfectMatch => "Reconnected to same device!",
                        ReconnectType::UsbStrict => "Reconnected to same device?",
                        ReconnectType::UsbLoose => "Connected to similar USB device.",
                        ReconnectType::LastDitch => "Connected to COM port by name.",
                    };

                    self.notifs.notify_str(text, Color::Green);
                } else {
                    // If starting session with device.
                    info!("Connected!");

                    // self.notifs.notify_str("Connected to port!", Color::Green);
                }

                {
                    let port_status_guard = self.serial.port_status.load();

                    if port_status_guard.current_port().is_none() {
                        error!("Was told about a port connection but no current port exists!");
                        panic!("Was told about a port connection but no current port exists!");
                    }

                    #[cfg(feature = "logging")]
                    if let Some(current_port) = port_status_guard.current_port() {
                        self.buffer
                            .log_handle
                            .log_port_connected(current_port.to_owned(), reconnect.clone())?;
                    }
                }

                // Dismiss attempt reconnect prompt if visible.
                if let Some(Popup::AttemptReconnectPrompt) = &self.popup {
                    self.popup = None;
                }

                self.buffer.scroll_by(0);
                self.user_broke_connection = false;
            }
            Event::Serial(SerialEvent::Disconnected(reason)) => {
                #[cfg(feature = "espflash")]
                self.espflash.reset_popup();
                // self.menu = Menu::PortSelection;
                // if let Some(reason) = reason {
                //     self.notify(format!("Disconnected from port! {reason}"), Color::Red);
                // }
                #[cfg(feature = "logging")]
                self.buffer.log_handle.log_port_disconnected(false)?;

                if let Some(Popup::DisconnectPrompt) = &self.popup
                    && self.reconnect_prompt_instead()
                {
                    self.show_popup(Popup::AttemptReconnectPrompt);
                }

                match reason {
                    SerialDisconnectReason::Intentional => (),
                    SerialDisconnectReason::UserBrokeConnection => {
                        self.user_broke_connection = true;
                        let text = "Broke serial connection! (Reconnections paused!)";
                        self.notifs.notify_str(text, Color::Red);
                    }
                    SerialDisconnectReason::Error(error) => {
                        error!("Serial worker reported error on disconnect! {error}");
                        let reconnect_text = match &self.settings.serial.reconnections {
                            Reconnections::Disabled => "Not attempting to reconnect",
                            Reconnections::LooseChecks => "Attempting to reconnect (loose checks)",
                            Reconnections::StrictChecks => {
                                "Attempting to reconnect (strict checks)"
                            }
                        };
                        self.notifs.notify_str(
                            format!("Port error: {error} - {reconnect_text}"),
                            Color::Red,
                        );
                    }
                }
            }
            Event::Serial(SerialEvent::PortScan(ports)) => {
                let last_ports_len = self.ports.len() as isize;
                let new_ports_len = ports.len() as isize;

                let diff = new_ports_len - last_ports_len;

                self.port_selection_scroll = self
                    .port_selection_scroll
                    .checked_add_signed(diff)
                    .unwrap_or_default();

                self.ports = ports;
            }
            Event::Serial(SerialEvent::UnsentTx(unsent)) => {
                error!(
                    "Serial worker reported an unsent buffer of len {}!",
                    unsent.len()
                );
                self.trigger_send_failed_visual()?;
            }
            #[cfg(feature = "espflash")]
            Event::Serial(SerialEvent::EspFlash(esp_event)) => match esp_event {
                EspEvent::Error(e) => {
                    self.notifs.notify_str(&e, Color::Red);
                    self.action_queue.clear();
                }
                _ => self
                    .espflash
                    .consume_event(esp_event, &mut self.notifs, &self.ctrl_c_tx),
            },
            #[cfg(feature = "logging")]
            Event::Logging(LoggingEvent::FinishedReconsumption) => self
                .notifs
                .notify_str("Finished syncing contents to log!", Color::Green),
            #[cfg(feature = "logging")]
            Event::Logging(LoggingEvent::Error(error)) => self
                .notifs
                .notify_str(format!("Logging error: {error}"), Color::Red),

            // Recieved once every second
            Event::Tick(Tick::PerSecond) => match self.menu {
                Menu::Terminal => {
                    // If disconnect prompt is open, pause reacting to the ticks
                    if let Some(popup) = &self.popup
                        && matches!(
                            popup,
                            Popup::AttemptReconnectPrompt | Popup::DisconnectPrompt
                        )
                    {
                        return Ok(());
                    }

                    let port_status = &self.serial.port_status.load().status();

                    let reconnections_allowed =
                        self.serial.port_settings.load().reconnections.allowed();
                    if !port_status.is_connected()
                        && !port_status.is_lent_out()
                        && reconnections_allowed
                        && !self.user_broke_connection
                    {
                        self.repeating_line_flip.flip();
                        self.serial.request_reconnect(None)?;
                    }
                }

                Menu::PortSelection => {
                    self.serial.request_port_scan()?;
                }
            },
            Event::Tick(Tick::Scroll) => {
                self.popup_hint_scroll += 1;

                let mut scroll_millis = 400;

                // Adjust scroll_millis based on text_scroll_speed setting:
                // Negative values = slower scrolling, positive values = faster scrolling
                let modifer = self.settings.behavior.text_scroll_speed;
                if modifer < 0 {
                    let factor = 1.0 + (-modifer as f32) * 0.5;
                    scroll_millis = (scroll_millis as f32 * factor) as u64;
                } else if modifer > 0 {
                    let factor = 1.0 / (1.0 + (modifer as f32) * 0.5);
                    scroll_millis = (scroll_millis as f32 * factor) as u64;
                    if scroll_millis < 40 {
                        scroll_millis = 40; // lower bound
                    }
                }

                if self.popup.is_some() {
                    self.carousel.add_oneshot(
                        "ScrollText",
                        Tick::Scroll,
                        Duration::from_millis(scroll_millis),
                    )?;
                }
            }
            Event::Tick(Tick::Action) => {
                self.consume_one_queued_action()?;
            }
            Event::Tick(Tick::Notification) => {
                // debug!("notif!");
                if let Some(notif) = &self.notifs.inner {
                    let emerging = notif.shown_for() <= EMERGE_TIME;
                    let collapsing = notif.shown_for() >= PAUSE_AND_SHOW_TIME;
                    let sleep_time = if emerging || collapsing {
                        Duration::from_millis(50)
                    } else if notif.replaced && notif.shown_for() <= EXPAND_TIME {
                        EXPAND_TIME.saturating_sub(notif.shown_for())
                    } else {
                        PAUSE_AND_SHOW_TIME.saturating_sub(notif.shown_for())
                    };
                    self.carousel
                        .add_oneshot("Notification", Tick::Notification, sleep_time)?;
                }
            }
            Event::Tick(Tick::Requested(origin)) => {
                debug!("Requested tick recieved from: {origin}");
                self.failed_send_at
                    .take_if(|i| i.elapsed() >= FAILED_SEND_VISUAL_TIME);
                #[cfg(feature = "espflash")]
                {
                    use crate::tui::esp::ERASE_FLASH_CONFIRM_PERIOD;

                    if let Some(duration) = self
                        .espflash
                        .first_erase_press
                        .as_ref()
                        .map(Instant::elapsed)
                        && duration >= ERASE_FLASH_CONFIRM_PERIOD
                    {
                        _ = self.espflash.first_erase_press.take();
                    }
                }
            }
            Event::Tick(Tick::Tx) => {
                self.repeating_line_flip.flip();
            }
            #[cfg(feature = "defmt")]
            Event::DefmtFromFilePicker(elf_path) => self.try_load_defmt_elf(
                &elf_path,
                #[cfg(feature = "defmt-watch")]
                false,
            ),
            #[cfg(feature = "defmt-watch")]
            Event::DefmtElfWatch(ElfWatchEvent::ElfUpdated(elf_path)) => {
                if self.settings.defmt.watch_elf_for_changes {
                    info!("ELF File Watch triggered, reloading ELF at {elf_path}");

                    self.try_load_defmt_elf(
                        &elf_path,
                        #[cfg(feature = "defmt-watch")]
                        true,
                    );
                } else {
                    trace!("Ignoring ELF file watch event!");
                }
            }
            #[cfg(feature = "defmt-watch")]
            Event::DefmtElfWatch(ElfWatchEvent::Error(err)) => {
                self.notifs.notify_str(err, Color::Red);
            }

            Event::Updates(UpdateEvent::UpToDate) => {
                info!("App is up-to-date!");
            }
            Event::Updates(UpdateEvent::UpdateFound(new)) => {
                if new != self.settings.updates.skipped_version {
                    info!("Update found! v{new}");
                    self.update_found_version = Some(new);
                } else {
                    info!("Update found, but ignoring! (v{new})");
                }
            }
            Event::Updates(UpdateEvent::UpdateCheckError(e)) => {
                // TODO maybe red version number?
                // or -> ??
                let report = color_eyre::Report::new(e);
                error!("error when checking for update: {report:#}");
            }
            #[cfg(feature = "self-replace")]
            Event::Updates(UpdateEvent::DownloadProgress(percentage)) => {
                self.popup = Some(Popup::UpdateDownloading(percentage))
            }
            #[cfg(feature = "self-replace")]
            Event::Updates(UpdateEvent::UpdateError(e)) => {
                error!("error when performing update: {e}");
                Err(e)?
            }

            #[cfg(all(windows, feature = "self-replace"))]
            Event::Updates(UpdateEvent::ReadyToLaunch) => {
                info!("Ready to start new version!");
                self.popup = Some(Popup::UpdateLaunchPrompt)
            }
            #[cfg(all(unix, feature = "self-replace"))]
            Event::Updates(UpdateEvent::ReadyToLaunch) => {
                info!("Starting new version!");
                self.update_worker.start_new_version()?;
            }
        }
        Ok(())
    }
    pub fn shutdown(&mut self) {
        self.state = RunningState::Finished;
    }
    #[cfg(feature = "macros")]
    fn send_one_macro(
        &mut self,
        macro_ref: MacroNameTag,
        key_combo_opt: Option<KeyCombination>,
    ) -> Result<()> {
        let (macro_tag, macro_content) = self
            .macros
            .all
            .iter()
            .find(|(tag, _string)| &&macro_ref == tag)
            .ok_or(MacroNotFound)?;

        let italic = Style::new().italic();

        assert!(!macro_content.is_empty());

        let (notif_line, notif_color) = match (key_combo_opt, macro_content) {
            // (_, _) if macro_content.is_empty() => (
            //     line!["Macro \"", span!(italic; macro_tag), "\" is empty!"],
            //     Color::Yellow,
            // ),
            (Some(key_combo), _) => (
                line![span!(italic; macro_tag), span!(" [{key_combo}]")],
                Color::Green,
            ),

            (None, _) => (line![span!(italic; "{macro_tag}")], Color::Green),
        };

        let default_macro_line_ending = self.settings.serial.macro_line_ending.as_bytes(
            &self.settings.serial.rx_line_ending,
            &self.settings.serial.tx_line_ending,
        );

        let macro_line_ending = if let Some(line_ending) = &macro_content.escaped_line_ending {
            use bstr::ByteVec;

            Cow::Owned(Vec::unescape_bytes(line_ending))
        } else {
            Cow::Borrowed(default_macro_line_ending)
        };

        // let macro_line_ending = macro_content
        //     .escaped_line_ending
        //     .as_ref()
        //     .map(CompactString::as_bytes)
        //     .unwrap_or(default_macro_line_ending);

        match macro_content {
            _ if macro_content.is_empty() => (),
            _ if macro_content.has_escaped_bytes => {
                let content = macro_content.unescape_bytes();
                self.serial
                    .send_bytes(content.clone(), Some(&macro_line_ending))?;

                debug!("{}", format!("Sending Macro Bytes: {:02X?}", content));
                self.buffer.append_user_bytes(
                    &content,
                    &macro_line_ending,
                    #[cfg(feature = "macros")]
                    Some(macro_content.sensitive),
                );
            }
            _ => {
                self.serial
                    .send_str(&macro_content.content, &macro_line_ending, true)?;
                self.buffer.append_user_text(
                    &macro_content.content,
                    &macro_line_ending,
                    #[cfg(feature = "macros")]
                    Some(macro_content.sensitive),
                );

                debug!(
                    "{}",
                    format!(
                        "Sending Macro Text: {}",
                        macro_content.as_str().escape_debug()
                    )
                );
            }
        };

        self.notifs.notify(notif_line, notif_color);

        // Scroll all the way down
        // TODO: Make this behavior a toggle
        self.buffer.scroll_by(i32::MIN);

        // TODO scroll bar is goofed with just user lines???

        Ok(())
    }
    // TODO fuzz this
    fn handle_key_press(&mut self, key_event: KeyEvent) -> Result<()> {
        // Intentionally putting this before the Ctrl-C ack-er,
        // so it can be used to break any potential hung instances during flashing
        // (not that I've encountered that behavior *yet*),
        // since I don't think I can unstick the serial worker thread
        // if it's stuck in external crate code.
        #[cfg(feature = "espflash")]
        if self.espflash.popup_active() && !self.espflash.device_info_shown() {
            return Ok(());
        }

        if is_ctrl_c(&key_event) {
            match self.ctrl_c_tx.try_send(()) {
                Ok(()) => (),
                Err(TrySendError::Full(_)) => panic!("Ctrl-C ack buffer full??"),
                Err(TrySendError::Disconnected(_)) => panic!("Failed to acknowledge Ctrl-C!"),
            }
        }

        // Dismiss the device info popup with any key.
        #[cfg(feature = "espflash")]
        if self.espflash.device_info_shown() {
            self.espflash.reset_popup();
            return Ok(());
        }

        let key_combo = KeyCombination::from(key_event);
        // debug!("{key_combo}");

        let mut port_selection_actions = false;
        let mut terminal_view_actions = false;
        // Filter for when we decide to handle user *text input*.
        // TODO move these into per-menu funcs.
        match self.popup {
            #[cfg(feature = "macros")]
            Some(Popup::ToolMenu(ToolMenu::Macros)) => {
                self.macros
                    .search_input
                    .handle_event(&ratatui::crossterm::event::Event::Key(key_event));
            }
            _ => (),
        }

        #[cfg(feature = "defmt")]
        if let Some(Popup::DefmtNewElf(file_explorer)) = &mut self.popup {
            let input = match key_event.code {
                KeyCode::Left | KeyCode::Char('h') => ratatui_explorer::Input::Left,
                KeyCode::Down | KeyCode::Char('j') => ratatui_explorer::Input::Down,
                KeyCode::Up | KeyCode::Char('k') => ratatui_explorer::Input::Up,
                KeyCode::Right | KeyCode::Char('l') => ratatui_explorer::Input::Right,
                KeyCode::Enter => ratatui_explorer::Input::Right,
                KeyCode::Backspace | KeyCode::BackTab => ratatui_explorer::Input::Left,
                KeyCode::PageUp => ratatui_explorer::Input::PageUp,
                KeyCode::PageDown => ratatui_explorer::Input::PageDown,
                KeyCode::Home => ratatui_explorer::Input::Home,
                KeyCode::End => ratatui_explorer::Input::End,

                _ => ratatui_explorer::Input::None,
            };
            if let Err(e) = file_explorer.handle(input) {
                error!("File Explorer Error: {e}");
                self.notifs
                    .notify_str(format!("Explorer Error: {e}"), Color::Red);
                self.dismiss_popup();
                return Ok(());
            };
            match input {
                ratatui_explorer::Input::None => (),
                ratatui_explorer::Input::Right => {
                    let current = file_explorer.current();
                    let is_file = current.is_file();
                    if is_file {
                        let path_buf = current.path().to_owned();

                        let Ok(elf_path) = camino::Utf8PathBuf::from_path_buf(path_buf) else {
                            self.notifs
                                .notify_str("Path is not valid UTF-8!", Color::Red);
                            return Ok(());
                        };

                        self.try_load_defmt_elf(
                            &elf_path,
                            #[cfg(feature = "defmt-watch")]
                            false,
                        );

                        self.dismiss_popup();
                    }
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }

        match (self.menu, &self.popup) {
            (Menu::Terminal, None) => {
                terminal_view_actions = true;
                match key_combo {
                    // Consuming Ctrl-A so input_box.handle_event doesn't move my cursor.
                    key!(ctrl - a) => (),
                    key!(del) | key!(backspace) if self.text_input.all_text_selected => (),

                    _text_input if self.settings.behavior.pseudo_shell => {
                        self.text_input.consume_typing_event(key_event)
                    }

                    _ if self
                        .keybinds
                        .key_has_single_action(key_combo, BaseAction::EscapeKeypress.into()) =>
                    {
                        // They pressed an escape key again? So they probably want to send it directly.
                        if self.escape_next_keypress {
                            self.escape_next_keypress = false;
                            self.send_crossterm_event_to_port(key_event)?;
                            return Ok(());
                        } else {
                            self.escape_next_keypress = true;
                            return Ok(());
                        }
                    }

                    _terminal_input if !self.escape_next_keypress => {
                        self.send_crossterm_event_to_port(key_event)?;
                        return Ok(());
                    }

                    // At this point:
                    // pseudo_shell is false,
                    // escape_next_keypress is true,
                    // and we're not processing a keybind for escaping keypresses.
                    //
                    // Unset the flag and don't return early so the keystroke is processed "normally".
                    _ => self.escape_next_keypress = false,
                }
            }
            (_, Some(Popup::DisconnectPrompt)) if !is_ctrl_c(&key_event) => {
                if let Some(pressed) = DisconnectPrompt::from_key_code(key_event.code) {
                    self.disconnect_prompt_choice(pressed)?;
                }
            }
            (_, Some(Popup::AttemptReconnectPrompt)) if !is_ctrl_c(&key_event) => {
                if let Some(pressed) = AttemptReconnectPrompt::from_key_code(key_event.code) {
                    let shift_pressed = key_event.modifiers.contains(KeyModifiers::SHIFT);
                    let ctrl_pressed = key_event.modifiers.contains(KeyModifiers::CONTROL);

                    self.reconnect_prompt_choice(pressed, shift_pressed, ctrl_pressed)?;
                }
            }
            (_, Some(Popup::IgnoreByName(_))) if !is_ctrl_c(&key_event) => {
                if let Some(pressed) = IgnorePortByNamePrompt::from_key_code(key_event.code) {
                    self.ignore_port_name_prompt_choice(pressed)?;
                }
            }
            (_, Some(Popup::IgnoreByUsb(_, _))) if !is_ctrl_c(&key_event) => {
                if let Some(pressed) = IgnoreUsbDevicePrompt::from_key_code(key_event.code) {
                    self.ignore_usb_device_prompt_choice(pressed)?;
                }
            }
            (_, Some(Popup::UpdateCheckConsentPrompt)) if !is_ctrl_c(&key_event) => {
                if let Some(pressed) = UpdateCheckConsentPrompt::from_key_code(key_event.code) {
                    self.update_check_consent_choice(pressed)?;
                }
            }
            (_, Some(Popup::UpdateBeginPrompt)) if !is_ctrl_c(&key_event) => {
                if let Some(pressed) = UpdateBeginPrompt::from_key_code(key_event.code) {
                    self.update_begin_choice(pressed)?;
                }
            }
            #[cfg(all(windows, feature = "self-replace"))]
            (_, Some(Popup::UpdateLaunchPrompt)) if !is_ctrl_c(&key_event) => {
                if let Some(pressed) = UpdateLaunchPrompt::from_key_code(key_event.code) {
                    self.update_launch_choice(pressed)?;
                }
            }

            (_, Some(Popup::SettingsMenu(SettingsMenu::SerialPort)))
                if self.get_corrected_popup_index() == Some(0) =>
            {
                // Intentionally only allowing ASCII digits and backspace,
                // since arrow keys are busy with menu navigation.
                if matches!(key_event.code, KeyCode::Char(c) if c.is_ascii_digit())
                    || matches!(key_event.code, KeyCode::Backspace)
                {
                    self.baud_input
                        .handle_event(&ratatui::crossterm::event::Event::Key(key_event));

                    if let Ok(baud_rate) = self.baud_input.value().parse::<u32>() {
                        self.scratch.serial.baud_rate = baud_rate;
                    }
                }
            }

            (Menu::Terminal, Some(_)) => (),

            (Menu::PortSelection, None) => {
                let is_custom_visible =
                    COMMON_BAUD.last_index_eq(self.baud_selection_state.current_index);
                let is_baud_selected =
                    self.port_selection_scroll == self.port_selection_item_count() - 2;

                if is_custom_visible && is_baud_selected {
                    // filtering out just letters from being put into the custom baud entry
                    // since filtering input to just ascii digits prevents backspace/cursor ops
                    // extra checks will be needed at parse stage to ensure non-digit chars arent present
                    if !matches!(key_event.code, KeyCode::Char(c) if c.is_alphabetic()) {
                        self.baud_input
                            .handle_event(&ratatui::crossterm::event::Event::Key(key_event));

                        if let Ok(baud_rate) = self.baud_input.value().parse::<u32>() {
                            self.scratch.serial.baud_rate = baud_rate;
                        }
                    }
                } else {
                    port_selection_actions = true;
                }
            }
            (Menu::PortSelection, Some(_)) => (),
        }
        let vim_scrollable_menu: bool = match (self.menu, &self.popup) {
            // (_, Some(PopupMenu::Macros), MacrosPrompt::Keybind) => false,
            #[cfg(feature = "macros")]
            (_, Some(Popup::ToolMenu(ToolMenu::Macros))) => false,
            (Menu::Terminal, None) => false,
            _ => true,
        };
        // TODO split this up into more functions based on menu
        match key_combo {
            // Start of _Hardcoded_ keybinds.
            key!(q) if port_selection_actions && self.popup.is_none() => self.shutdown(),
            key!(u)
                if port_selection_actions
                    && self.popup.is_none()
                    && self.update_found_version.is_some() =>
            {
                self.show_popup(Popup::UpdateBeginPrompt);
            }
            key!(ctrl - shift - c) => self.shutdown(),
            // move into ctrl-c func?
            key!(ctrl - c) => match (self.menu, &self.popup) {
                (_, Some(Popup::AttemptReconnectPrompt)) | (_, Some(Popup::DisconnectPrompt)) => {
                    self.shutdown()
                }
                (Menu::Terminal, None) => {
                    self.show_popup(Popup::DisconnectPrompt);
                }
                (_, Some(_)) => {
                    self.dismiss_popup();
                }
                _ => self.shutdown(),
            },
            key!(ctrl - a) if terminal_view_actions && !self.text_input.value().is_empty() => {
                self.text_input.all_text_selected = true;
            }
            key!(ctrl - backspace)
                if terminal_view_actions && !self.text_input.value().is_empty() =>
            {
                self.text_input.remove_one_word();
            }
            key!(home) if self.popup.is_some() => {
                self.popup_menu_scroll = 0;
            }
            key!(ctrl - pageup) | key!(shift - pageup) => self.buffer.scroll_by(i32::MAX),
            key!(ctrl - pagedown) | key!(shift - pagedown) => self.buffer.scroll_by(i32::MIN),
            key!(ctrl - shift - delete) | key!(ctrl - shift - backspace) => {
                self.text_input.clear();
            }
            key!(delete) | key!(backspace) if self.text_input.all_text_selected => {
                self.text_input.clear();
            }
            key!(pageup) => self.buffer.scroll_page_up(),
            key!(pagedown) => self.buffer.scroll_page_down(),
            // KeyCode::F(f_key) if ctrl_pressed && shift_pressed => {
            //     let meow = key!(ctrl - c);
            //     self.notify(format!("Pressed Ctrl-Shift-F{f_key}"), Color::Blue)
            // }
            // KeyCode::F(f_key) if ctrl_pressed => {
            //     self.notify(format!("Pressed Ctrl-F{f_key}"), Color::Blue)
            // }
            // KeyCode::F(f_key) if shift_pressed => {
            //     self.notify(format!("Pressed Shift+F{f_key}"), Color::Blue)
            // }
            // KeyCode::F(f_key) => self.notify(format!("Pressed F{f_key}"), Color::Blue),
            // KeyCode::End => self
            //     .event_carousel
            //     .add_event(Event::TickSecond, Duration::from_secs(3)),
            key!(h) if vim_scrollable_menu => self.left_pressed(),
            key!(j) if vim_scrollable_menu => self.down_pressed(),
            key!(k) if vim_scrollable_menu => self.up_pressed(),
            key!(l) if vim_scrollable_menu => self.right_pressed(),
            key!(i) if port_selection_actions && self.port_selection_scroll < self.ports.len() => {
                match self.ports.get(self.port_selection_scroll) {
                    None => (),
                    Some(SerialPortInfo {
                        port_name,
                        port_type: SerialPortType::UsbPort(usb),
                    }) => {
                        self.show_popup(Popup::IgnoreByUsb(port_name.to_owned(), usb.to_owned()));
                    }
                    Some(SerialPortInfo { port_name, .. }) => {
                        self.show_popup(Popup::IgnoreByName(port_name.to_owned()))
                    }
                }
            }
            key!(up) => self.up_pressed(),
            key!(down) => self.down_pressed(),
            key!(left) => self.left_pressed(),
            key!(right) => self.right_pressed(),
            key!(enter) => self.enter_pressed(false, false)?,
            key!(ctrl - enter) => self.enter_pressed(true, false)?,
            key!(shift - enter) => self.enter_pressed(false, true)?,
            key!(ctrl - shift - enter) => self.enter_pressed(true, true)?,
            key!(tab) if terminal_view_actions && self.popup.is_none() => {
                self.text_input.find_input_in_history();
            }
            // KeyCode::Tab => self.tab_pressed(),
            key!(ctrl - r) if self.popup == Some(Popup::CurrentKeybinds) => {
                self.run_builtin_action(BuiltinAction::Base(BaseAction::ReloadKeybinds))?;
            }
            #[cfg(feature = "macros")]
            key!(ctrl - r) if self.popup == Some(Popup::ToolMenu(ToolMenu::Macros)) => {
                self.run_builtin_action(BuiltinAction::MacroBuiltin(
                    MacroBuiltinAction::ReloadMacros,
                ))?;
            }
            #[cfg(feature = "espflash")]
            key!(ctrl - r) if self.popup == Some(Popup::ToolMenu(ToolMenu::EspFlash)) => {
                self.run_builtin_action(BuiltinAction::EspBuiltin(
                    EspBuiltinAction::ReloadProfiles,
                ))?;
            }
            key!(esc) => self.esc_pressed(),
            key_combo => {
                let Some(actions_str) = self.keybinds.action_strs_from_key_combo(key_combo)
                // .map(ToOwned::to_owned)
                else {
                    return Ok(());
                };

                let mut actions = Vec::new();

                for action in actions_str {
                    if let Some(action) = self.get_action_from_string(action) {
                        actions.push(action);
                    } else {
                        self.notifs.notify_str(
                            format!("Unrecognized keybind action: \"{action}\""),
                            Color::Yellow,
                        );
                        return Ok(());
                    }
                }

                debug!("{key_combo}: {actions:?}");

                self.queue_keybinds_action_set(actions, key_combo)?;
            }
        }
        Ok(())
    }
    fn send_crossterm_event_to_port(&mut self, key_event: KeyEvent) -> Result<()> {
        let serial_healthy = self.serial.port_status.load().status().is_connected();

        if let Ok(event) =
            terminput_crossterm::to_terminput(crossterm::event::Event::Key(key_event))
            && serial_healthy
        {
            let mut buf = [0; 16];
            if let Ok(n) = event.encode(&mut buf, terminput::Encoding::Xterm) {
                self.serial.send_bytes(buf[..n].to_owned(), None)?;
                self.last_raw_sequence = ArrayVec::from_array_len(buf, n);
            }

            self.repeating_line_flip.flip();
        } else {
            self.trigger_send_failed_visual()?;
            self.last_raw_sequence = Default::default();
        }
        Ok(())
    }
    fn queue_keybinds_action_set(
        &mut self,
        mut actions: Vec<Action>,
        key_combo: KeyCombination,
    ) -> Result<()> {
        assert!(
            !actions.is_empty(),
            "should never be asked to queue no actions"
        );

        // If it's just one action, and it's something we can handle now, we should.
        if actions.len() == 1
            && let Some(action) = actions.first()
        {
            let port_status = &self.serial.port_status.load().status();

            if action.requires_connection() && !port_status.is_connected() {
                self.notifs.notify_str(
                    "Action requires healthy port connection! Not acting...",
                    Color::Red,
                );
                return Ok(());
            } else if action.requires_terminal_view() && !matches!(self.menu, Menu::Terminal) {
                self.notifs.notify_str(
                    "Action requires terminal view active! Not acting...",
                    Color::Red,
                );
                return Ok(());
            }

            self.queued_keybind_action_dispatch(
                actions.pop().expect("checked for exactly one item?"),
                key_combo,
            )?;
            return Ok(());
        }

        self.action_queue
            .extend(actions.into_iter().map(|a| (key_combo, a)));

        self.event_tx.send(Tick::Action.into())?;

        Ok(())
    }
    pub fn get_action_from_string(&self, action: &str) -> Option<Action> {
        let action = action.trim();

        if action.is_empty() {
            return None;
        }

        // Check for matching built-in app method
        if let Ok(app_action) = action.parse::<BuiltinAction>() {
            return Some(Action::BuiltinAction(app_action));
        }

        #[cfg(feature = "espflash")]
        // Try to find esp profile by exact name match.
        // Profile search always should go before macro search,
        // since macros can use category + name to disambiguate.
        // Otherwise if there's a espflash profile matching a macro's name
        // and we return early from checking macros first,
        // it'd be impossible for the user to make clear what they actually want,
        // the macro by name, or the esp profile by name.
        if self.espflash.profile_from_name(action).is_some() {
            return Some(Action::EspFlashProfile(action.to_owned()));
        }

        #[cfg(feature = "macros")]
        // Get Macro by name and category, optionally categorically fuzzy
        if let Some(nametag) = self
            .macros
            .get_by_string(action, self.settings.behavior.fuzzy_macro_match)
        {
            return Some(Action::MacroInvocation(nametag));
        }

        let parse_duration = |s: &str| -> Option<Duration> {
            let pause_prefix = "pause_ms:";
            let s_start = s.first_chars(pause_prefix.len())?;
            if !s_start.eq_ignore_ascii_case(pause_prefix) {
                return None;
            }
            let delay_ms = s[pause_prefix.len()..].parse().ok()?;
            Some(Duration::from_millis(delay_ms))
        };

        // Check if it's a pause request
        if let Some(duration) = parse_duration(action) {
            return Some(Action::Pause(duration));
        }

        // Otherwise, it's nothing we recognize.
        None
    }
    // To be used only when chewing through queued actions.
    // Refrain from placing single-action keybind logic here.
    fn consume_one_queued_action(&mut self) -> Result<()> {
        if self.action_queue.is_empty() {
            return Ok(());
        }

        // if action.requires_port_connection() {
        let port_status_guard = self.serial.port_status.load().status();
        match port_status_guard {
            InnerPortStatus::Connected { .. } => (),
            #[cfg(feature = "espflash")]
            InnerPortStatus::LentOut { .. } => {
                self.carousel.add_oneshot(
                    "ActionQueue",
                    Tick::Action,
                    Duration::from_millis(500),
                )?;
                return Ok(());
            }
            // Currently this runs even for action chains that don't need the port,
            // but that's such a niche/rare thing right now, so meh.
            InnerPortStatus::Idle { .. } | InnerPortStatus::PrematureDisconnect { .. } => {
                // InnerPortStatus::Idle | InnerPortStatus::PrematureDisconnect if action.requires_connection() => {
                let text = if self.action_queue.len() == 1 {
                    "Port isn't ready! Not running action...".into()
                } else {
                    format!(
                        "Port isn't ready! Clearing {} queued actions...",
                        self.action_queue.len()
                    )
                };
                self.notifs.notify_str(text, Color::Red);
                self.action_queue.clear();
                return Ok(());
            }
        }
        // }

        let Some((key_combo_opt, action)) = self.action_queue.pop_front() else {
            unreachable!("queue was checked to not be empty")
        };

        let pause_duration_opt = self.queued_keybind_action_dispatch(action, key_combo_opt)?;

        let next_action_delay = pause_duration_opt.unwrap_or_default();

        self.carousel
            .add_oneshot("ActionQueue", Tick::Action, next_action_delay)?;

        Ok(())
    }
    // Any Err's from here should be considered fatal, things like
    // workers/macros/espflash profiles not existing.
    //
    // Port errors won't appear here.
    //
    // This shouldn't be called directly, rather it should be called as a result of
    // queue consumption ticks, or queue_action_set consuming a single action.
    fn queued_keybind_action_dispatch(
        &mut self,
        action: Action,
        key_combo: KeyCombination,
    ) -> Result<Option<Duration>> {
        debug!("Consuming action: {action:?} - Key: {key_combo:?}");

        // If the next action is a delay, this action shouldn't have one of its own.
        let post_action_pause_duration = match self.action_queue.front() {
            Some((_, Action::Pause(_))) => None,
            // Otherwise, use the default.
            _ => Some(self.settings.behavior.action_chain_delay),
        };

        match action {
            // But a running Pause action will override with it's own.
            Action::Pause(duration) => return Ok(Some(duration)),

            Action::BuiltinAction(method) => self.run_builtin_action(method)?,

            #[cfg(feature = "macros")]
            Action::MacroInvocation(name_tag) => {
                self.send_one_macro(name_tag, Some(key_combo))?;
                // if let Err(report) = self.send_one_macro(name_tag, key_combo_opt) {
                //     match report.downcast_ref::<MacroNotFound>() {
                //                           TODO maybe handle separately later?
                //         Some(MacroNotFound) => Err(MacroNotFound)?,
                //         None => Err(report)?,
                //     }
                // }
            }

            #[cfg(feature = "espflash")]
            Action::EspFlashProfile(profile) => {
                let Some(profile) = self.espflash.profile_from_name(&profile) else {
                    panic!("espflash profile existed but disappeared?")
                };

                self.notifs.notify_str(
                    format!("espflash profile: {} [{}]", profile.name(), key_combo),
                    Color::LightBlue,
                );

                self.esp_flash_profile(profile)?;
            }
        }

        Ok(post_action_pause_duration)
    }

    #[cfg(feature = "espflash")]
    fn esp_flash_profile(&mut self, profile: esp::EspProfile) -> Result<(), SerialWorkerMissing> {
        #[cfg(feature = "defmt")]
        let profile_defmt_path = profile.defmt_elf_path();

        self.serial.esp_flash_profile(profile)?;

        #[cfg(feature = "defmt")]
        if let Some(elf_path) = profile_defmt_path {
            self.try_load_defmt_elf(
                &elf_path,
                #[cfg(feature = "defmt-watch")]
                false,
            );
        }
        Ok(())
    }
    fn run_builtin_action(&mut self, action: BuiltinAction) -> Result<()> {
        let pretty_bool = |b: bool| {
            if b { "On" } else { "Off" }
        };
        use BuiltinAction as A;
        match action {
            A::Popup(popup) => self.show_popup_from_action(popup),

            A::Port(PortAction::ToggleDtr) => {
                self.serial.toggle_signals(true, false)?;
            }
            A::Port(PortAction::ToggleRts) => {
                self.serial.toggle_signals(false, true)?;
            }
            A::Port(PortAction::AssertDtr) => {
                self.serial.write_signals(Some(true), None)?;
            }
            A::Port(PortAction::DeassertDtr) => {
                self.serial.write_signals(Some(false), None)?;
            }
            A::Port(PortAction::AssertRts) => {
                self.serial.write_signals(None, Some(true))?;
            }
            A::Port(PortAction::DeassertRts) => {
                self.serial.write_signals(None, Some(false))?;
            }
            A::Port(PortAction::AttemptReconnectStrict) => {
                self.serial
                    .request_reconnect(Some(Reconnections::StrictChecks))?;
            }
            A::Port(PortAction::AttemptReconnectLoose) => {
                self.serial
                    .request_reconnect(Some(Reconnections::LooseChecks))?;
            }
            A::Base(BaseAction::ToggleTextwrap) => {
                let state = pretty_bool(self.settings.rendering.wrap_text.flip());
                self.buffer
                    .update_render_settings(self.settings.rendering.clone());
                self.settings.save()?;
                self.notifs
                    .notify_str(format!("Toggled Text Wrapping {state}"), Color::Gray);
            }
            A::Base(BaseAction::ToggleTimestamps) => {
                let state = pretty_bool(self.settings.rendering.timestamps.flip());
                self.buffer
                    .update_render_settings(self.settings.rendering.clone());
                self.settings.save()?;
                self.notifs
                    .notify_str(format!("Toggled Timestamps {state}"), Color::Gray);
            }

            A::Base(BaseAction::ToggleIndices) => {
                let state = pretty_bool(self.settings.rendering.show_indices.flip());
                self.buffer
                    .update_render_settings(self.settings.rendering.clone());
                self.settings.save()?;
                self.notifs.notify_str(
                    format!("Toggled Line Indices + Length {state}"),
                    Color::Gray,
                );
            }

            A::Base(BaseAction::ToggleIndicesHex) => {
                let state = pretty_bool(self.settings.rendering.indices_as_hex.flip());
                self.buffer
                    .update_render_settings(self.settings.rendering.clone());
                self.settings.save()?;
                self.notifs
                    .notify_str(format!("Toggled Indices as Hex {state}"), Color::Gray);
            }

            A::Base(BaseAction::ToggleHexView) => {
                let state = pretty_bool(self.settings.rendering.hex_view.flip());
                self.buffer
                    .update_render_settings(self.settings.rendering.clone());
                self.settings.save()?;
                self.buffer.scroll_by(0);
                self.notifs
                    .notify_str(format!("Toggled Hex View {state}"), Color::Gray);
            }

            A::Base(BaseAction::ToggleHexViewHeader) => {
                let state = pretty_bool(self.settings.rendering.hex_view_header.flip());
                self.buffer
                    .update_render_settings(self.settings.rendering.clone());
                self.settings.save()?;
                self.notifs
                    .notify_str(format!("Toggled Hex View Header {state}"), Color::Gray);
            }

            A::Base(BaseAction::TogglePseudoShell) => {
                let state = pretty_bool(self.settings.behavior.pseudo_shell.flip());
                self.settings.save()?;
                self.notifs
                    .notify_str(format!("Toggled Pseudo Shell {state}"), Color::Gray);
            }

            A::Base(BaseAction::TogglePseudoShellHex) => {
                debug!("{}", self.text_input.byte_entry_active());
                self.text_input.toggle_bytes_entry();
                debug!("{}", self.text_input.byte_entry_active());
            }

            A::Base(BaseAction::EscapeKeypress) => {
                if self.escape_next_keypress {
                    self.notifs
                        .notify_str("Keypress was already escaped!", Color::Yellow);
                } else if self.settings.behavior.pseudo_shell {
                    self.notifs.notify_str(
                        "Can only escape keypress when Pseudo Shell is disabled!",
                        Color::Yellow,
                    );
                }
            }

            #[cfg(feature = "macros")]
            A::MacroBuiltin(MacroBuiltinAction::ReloadMacros) => {
                match Macros::load_from_folder(config_adjacent_path(crate::macros::MACROS_DIR_PATH))
                {
                    Ok((macros, errors)) => {
                        let err_len = errors.len();
                        self.macros = macros;
                        if errors.is_empty() {
                            self.notifs
                                .notify_str("Reloaded Macros Successfully!", Color::Green);
                        } else {
                            self.notifs.notify_str(
                                format!("Reloaded Macros! {err_len} files had errors!"),
                                Color::Yellow,
                            );
                        }
                    }
                    Err(e) => {
                        self.notifs
                            .notify_str(format!("Error opening macros: {e}!"), Color::Red);
                    }
                }
            }

            A::Base(BaseAction::ReloadColors) => {
                if let Err(e) = self.buffer.reload_color_rules() {
                    let err_str: Cow<'_, str> = match &e {
                        ColorRuleLoadError::Deser(deser_err) => deser_err.message().into(),
                        err => err.to_string().into(),
                    };
                    self.notifs.notify_str(
                        format!("Error reloading Color Rules: {err_str}! See log for details."),
                        Color::Red,
                    );
                    let report = color_eyre::Report::new(e);
                    error!("Error reloading Color Rules: {report:#}");
                } else {
                    self.notifs
                        .notify_str("Reloaded Color Rules!", Color::Green);
                }
            }

            A::Base(BaseAction::ReloadKeybinds) => match Keybinds::load() {
                Ok(new) => {
                    self.keybinds = new;
                    self.notifs.notify_str("Reloaded Keybinds!", Color::Green);
                }
                Err(e) => {
                    self.notifs.notify_str(
                        format!("Error reloading Keybinds: {e}! See log for details."),
                        Color::Red,
                    );
                    let report = color_eyre::Report::new(e);
                    error!("Error reloading Keybinds: {report:#}");
                }
            },

            #[cfg(feature = "logging")]
            A::Logging(LoggingAction::Sync) => {
                let port_status_guard = self.serial.port_status.load();
                let Some(_) = port_status_guard.current_port() else {
                    self.notifs.notify_str(
                        "Not (previously) connected to port? Unable to sync log.",
                        Color::Yellow,
                    );
                    return Ok(());
                };
                self.buffer.relog_buffer()?;
                self.notifs
                    .notify_str("Requested logging start!", Color::Green);
            }

            #[cfg(feature = "espflash")]
            A::EspBuiltin(EspBuiltinAction::EspHardReset) => {
                self.serial.esp_restart(EspRestartType::UserCode)?;
            }

            #[cfg(feature = "espflash")]
            A::EspBuiltin(EspBuiltinAction::EspBootloader) => {
                self.serial
                    .esp_restart(EspRestartType::Bootloader { active: true })?;
            }

            #[cfg(feature = "espflash")]
            A::EspBuiltin(EspBuiltinAction::EspBootloaderUnchecked) => {
                self.serial
                    .esp_restart(EspRestartType::Bootloader { active: false })?;
            }

            #[cfg(feature = "espflash")]
            A::EspBuiltin(EspBuiltinAction::EspDeviceInfo) => {
                self.serial.esp_device_info()?;
            }

            #[cfg(feature = "espflash")]
            A::EspBuiltin(EspBuiltinAction::EspEraseFlash) => {
                self.serial.esp_erase_flash()?;
            }
            #[cfg(feature = "espflash")]
            A::EspBuiltin(EspBuiltinAction::ReloadProfiles) => {
                assert!(
                    !self.espflash.popup_active(),
                    "Shouldn't be able to reload profiles while using one!"
                );
                match EspFlashHelper::build() {
                    Ok(new_helper) => {
                        self.espflash = new_helper;
                        self.notifs
                            .notify_str("Reloaded espflash profiles!", Color::Green);
                    }
                    Err(e) => {
                        self.notifs.notify_str(
                            format!("Error reloading espflash profiles: {e}! See log for details."),
                            Color::Red,
                        );
                        let report = color_eyre::Report::new(e);
                        error!("Error reloading espflash profiles: {report:#}");
                    }
                }
            }
            #[cfg(feature = "defmt")]
            A::ShowDefmtSelect(DefmtSelectAction::SelectRecent) => {
                self.show_popup(Popup::DefmtRecentElf)
            }
            #[cfg(feature = "defmt")]
            A::ShowDefmtSelect(DefmtSelectAction::SelectTui) => {
                self.show_popup(Popup::DefmtNewElf(create_file_explorer()?))
            }
            #[cfg(feature = "defmt")]
            A::ShowDefmtSelect(DefmtSelectAction::SelectSystem) => {
                let tx = self.event_tx.clone();
                std::thread::spawn(move || {
                    let file_opt_res = native_dialog::DialogBuilder::file()
                        .add_filter("ELF (Executable and Linkable Format)", ["elf"])
                        .add_filter("All Files", [""])
                        .set_location("")
                        .open_single_file()
                        .show();
                    if let Ok(Some(file)) = file_opt_res {
                        if let Ok(file_utf8) = camino::Utf8PathBuf::from_path_buf(file) {
                            _ = tx.send(Event::DefmtFromFilePicker(file_utf8));
                        } else {
                            // TODO show in UI?
                            error!("Chosen file has non-UTF-8 path!");
                        }
                    } else {
                        debug!("No file chosen with system file picker?");
                    }
                });
            } // unknown => {
              //     warn!("Unknown keybind action: {unknown}");
              //     self.notifs.notify_str(
              //         format!("Unknown keybind action: \"{unknown}\""),
              //         Color::Yellow,
              //     );
              // }
        };
        Ok(())
    }
    /// Returns `true` if the AttemptReconnect prompt should be shown instead
    /// of the normal Disconnect prompt when user presses Esc on terminal view
    /// or a disconnect event occurs with the normal Disconnect prompt open.
    fn reconnect_prompt_instead(&self) -> bool {
        let port_status_guard = self.serial.port_status.load();
        let port_settings_guard = self.serial.port_settings.load();

        // If user broke the connection intentionally, easy check
        self.user_broke_connection ||
             // Otherwise, only show if we're not connected or lending the port to espflash
             (!port_status_guard.status().is_connected()
                && !port_status_guard.status().is_lent_out()
                // and if auto-reconnections is disabled.
                // (when auto-reconns. are enabled, the normal Disconnect prompt
                // is supposed to show to act as a pause for auto-reconnections)
                && !port_settings_guard.reconnections.allowed())
    }
    // fn tab_pressed(&mut self) {}
    fn esc_pressed(&mut self) {
        match self.popup {
            None => (),
            Some(_) => {
                self.dismiss_popup();
                return;
            }
        }

        match self.menu {
            Menu::Terminal if self.reconnect_prompt_instead() => {
                self.show_popup(Popup::AttemptReconnectPrompt)
            }
            Menu::Terminal => self.show_popup(Popup::DisconnectPrompt),
            Menu::PortSelection => self.shutdown(),
        }
    }
    fn up_pressed(&mut self) {
        self.text_input.all_text_selected = false;
        self.popup_hint_scroll = -2;
        match &self.popup {
            None => (),
            // Some(Popup::ErrorMessage(_)) => (),
            Some(Popup::CurrentKeybinds) => {
                self.popup_menu_scroll = self.popup_menu_scroll.saturating_sub(1);
            }
            #[cfg(not(any(feature = "espflash", feature = "macros")))]
            Some(Popup::SettingsMenu(_)) => match self.popup_menu_scroll {
                0 => self.select_last_popup_item(),
                _ => self.popup_menu_scroll -= 1,
            },
            #[cfg(any(feature = "espflash", feature = "macros"))]
            Some(Popup::SettingsMenu(_)) | Some(Popup::ToolMenu(_)) => {
                match self.popup_menu_scroll {
                    0 => self.select_last_popup_item(),
                    _ => self.popup_menu_scroll -= 1,
                }
            }
            #[cfg(feature = "defmt")]
            Some(Popup::DefmtNewElf(_)) => (),
            #[cfg(feature = "defmt")]
            Some(Popup::DefmtRecentElf) => match self.popup_menu_scroll {
                0 => self.select_last_popup_item(),
                _ => self.popup_menu_scroll -= 1,
            },
            Some(Popup::AttemptReconnectPrompt)
            | Some(Popup::DisconnectPrompt)
            | Some(Popup::IgnoreByName(_))
            | Some(Popup::IgnoreByUsb(_, _)) => match self.popup_menu_scroll {
                0 => self.select_last_popup_item(),
                _ => self.popup_menu_scroll -= 1,
            },

            Some(Popup::UpdateCheckConsentPrompt) | Some(Popup::UpdateBeginPrompt) => {
                match self.popup_menu_scroll {
                    0 => self.select_last_popup_item(),
                    _ => self.popup_menu_scroll -= 1,
                }
            }

            #[cfg(feature = "self-replace")]
            Some(Popup::UpdateDownloading(_)) => (),

            #[cfg(all(windows, feature = "self-replace"))]
            Some(Popup::UpdateLaunchPrompt) => match self.popup_menu_scroll {
                0 => self.select_last_popup_item(),
                _ => self.popup_menu_scroll -= 1,
            },

            Some(Popup::SerialConnectionFailed(_)) => (),
        }

        if self.popup.is_some() {
            return;
        }
        match self.menu {
            Menu::PortSelection => match self.port_selection_scroll {
                0 => self.port_selection_scroll = self.port_selection_item_count() - 1,
                _ => self.port_selection_scroll -= 1,
            },
            Menu::Terminal => self.text_input.scroll_history(true),
        }
    }
    fn down_pressed(&mut self) {
        self.text_input.all_text_selected = false;
        self.popup_hint_scroll = -2;
        match &self.popup {
            None => (),
            // Some(Popup::ErrorMessage(_)) => (),
            Some(Popup::CurrentKeybinds) => {
                self.popup_menu_scroll += 1;
            }
            #[cfg(not(any(feature = "espflash", feature = "macros")))]
            Some(Popup::SettingsMenu(_)) => match self.popup_menu_scroll {
                _last if self.last_popup_item_selected() => self.popup_menu_scroll = 0,
                _ => self.popup_menu_scroll += 1,
            },
            #[cfg(any(feature = "espflash", feature = "macros"))]
            Some(Popup::SettingsMenu(_)) | Some(Popup::ToolMenu(_)) => {
                match self.popup_menu_scroll {
                    _last if self.last_popup_item_selected() => self.popup_menu_scroll = 0,
                    _ => self.popup_menu_scroll += 1,
                }
            }
            #[cfg(feature = "defmt")]
            Some(Popup::DefmtNewElf(_)) => (),
            #[cfg(feature = "defmt")]
            Some(Popup::DefmtRecentElf) => match self.popup_menu_scroll {
                _last if self.last_popup_item_selected() => self.popup_menu_scroll = 0,
                _ => self.popup_menu_scroll += 1,
            },
            Some(Popup::AttemptReconnectPrompt)
            | Some(Popup::DisconnectPrompt)
            | Some(Popup::IgnoreByName(_))
            | Some(Popup::IgnoreByUsb(_, _)) => match self.popup_menu_scroll {
                _last if self.last_popup_item_selected() => self.popup_menu_scroll = 0,
                _ => self.popup_menu_scroll += 1,
            },

            Some(Popup::UpdateCheckConsentPrompt) | Some(Popup::UpdateBeginPrompt) => {
                match self.popup_menu_scroll {
                    _last if self.last_popup_item_selected() => self.popup_menu_scroll = 0,
                    _ => self.popup_menu_scroll += 1,
                }
            }

            #[cfg(feature = "self-replace")]
            Some(Popup::UpdateDownloading(_)) => (),

            #[cfg(all(windows, feature = "self-replace"))]
            Some(Popup::UpdateLaunchPrompt) => match self.popup_menu_scroll {
                _last if self.last_popup_item_selected() => self.popup_menu_scroll = 0,
                _ => self.popup_menu_scroll += 1,
            },

            Some(Popup::SerialConnectionFailed(_)) => (),
        }

        if self.popup.is_some() {
            return;
        }

        match self.menu {
            Menu::PortSelection => match self.port_selection_scroll {
                last if last >= self.port_selection_item_count() - 1 => {
                    self.port_selection_scroll = 0
                }
                _ => self.port_selection_scroll += 1,
            },

            Menu::Terminal => self.text_input.scroll_history(false),
        }
    } // TODO macro categories
    // when no "has bytes" macros exist
    // the highlight can get shoved up sometimes
    fn left_pressed(&mut self) {
        match &mut self.popup {
            None => (),
            // Some(Popup::ErrorMessage(_)) => (),
            Some(Popup::AttemptReconnectPrompt)
            | Some(Popup::DisconnectPrompt)
            | Some(Popup::IgnoreByName(_))
            | Some(Popup::IgnoreByUsb(_, _))
            | Some(Popup::SerialConnectionFailed(_))
            | Some(Popup::CurrentKeybinds) => (),
            #[cfg(not(any(feature = "espflash", feature = "macros")))]
            Some(Popup::SettingsMenu(_)) if self.popup_menu_scroll == 0 => {}
            #[cfg(any(feature = "espflash", feature = "macros"))]
            Some(Popup::SettingsMenu(_)) | Some(Popup::ToolMenu(_))
                if self.popup_menu_scroll == 0 =>
            {
                self.cycle_menu_type();
            }
            #[cfg(any(feature = "espflash", feature = "macros"))]
            Some(Popup::ToolMenu(_)) if self.popup_menu_scroll == 1 => {
                self.cycle_sub_menu(false);
            }
            Some(Popup::SettingsMenu(_)) if self.popup_menu_scroll == 1 => {
                self.cycle_sub_menu(false);
            }
            Some(Popup::SettingsMenu(SettingsMenu::SerialPort)) => {
                self.scratch
                    .serial
                    .handle_input(ArrowKey::Left, self.get_corrected_popup_index().unwrap())
                    .unwrap();

                if let Some(0) = self.get_corrected_popup_index() {
                    self.baud_input = self.scratch.serial.baud_rate.to_string().into();
                }
            }
            Some(Popup::SettingsMenu(SettingsMenu::Behavior)) => {
                self.scratch
                    .behavior
                    .handle_input(ArrowKey::Left, self.get_corrected_popup_index().unwrap())
                    .unwrap();
            }
            Some(Popup::SettingsMenu(SettingsMenu::Rendering)) => {
                self.scratch
                    .rendering
                    .handle_input(ArrowKey::Left, self.get_corrected_popup_index().unwrap())
                    .unwrap();
            }
            #[cfg(feature = "macros")]
            Some(Popup::ToolMenu(ToolMenu::Macros)) => {
                if !self.macros.search_input.value().is_empty() {
                    return;
                }
                self.macros.categories_selector.prev();
                if self.popup_menu_scroll >= POPUP_MENU_SELECTOR_COUNT {
                    if self.macros.none_visible() {
                        self.popup_menu_scroll = 1;
                    } else {
                        self.popup_menu_scroll = 2;
                    }
                }
            }
            #[cfg(feature = "espflash")]
            Some(Popup::ToolMenu(ToolMenu::EspFlash)) => {
                if self.popup_menu_scroll == POPUP_MENU_SELECTOR_COUNT + 1 {
                    self.espflash.unchecked_bootloader.flip();
                }
            }
            #[cfg(feature = "logging")]
            Some(Popup::SettingsMenu(SettingsMenu::Logging)) => {
                if self.popup_menu_scroll == POPUP_MENU_SELECTOR_COUNT + Logging::VISIBLE_FIELDS {
                    return;
                }

                self.scratch
                    .logging
                    .handle_input(ArrowKey::Left, self.get_corrected_popup_index().unwrap())
                    .unwrap();
            }
            #[cfg(feature = "defmt")]
            Some(Popup::SettingsMenu(SettingsMenu::Defmt)) => {
                use crate::tui::defmt::DEFMT_BUTTONS;

                if self.popup_menu_scroll < POPUP_MENU_SELECTOR_COUNT + DEFMT_BUTTONS {
                    return;
                }

                self.scratch
                    .defmt
                    .handle_input(ArrowKey::Left, self.get_corrected_popup_index().unwrap())
                    .unwrap();
            }
            #[cfg(feature = "defmt")]
            Some(Popup::DefmtNewElf(_)) => (),
            #[cfg(feature = "defmt")]
            Some(Popup::DefmtRecentElf) => (),

            Some(Popup::UpdateCheckConsentPrompt) | Some(Popup::UpdateBeginPrompt) => (),

            #[cfg(feature = "self-replace")]
            Some(Popup::UpdateDownloading(_)) => (),

            #[cfg(all(windows, feature = "self-replace"))]
            Some(Popup::UpdateLaunchPrompt) => (),
        }
        if self.popup.is_some() {
            return;
        }
        if matches!(self.menu, Menu::PortSelection)
            && self.port_selection_scroll == self.ports.len()
        {
            if self.baud_selection_state.current_index == 0 {
                self.baud_selection_state.select(COMMON_BAUD.last_index());
            } else {
                self.baud_selection_state.prev();
            }
        }
    }
    fn right_pressed(&mut self) {
        // KeyCode::Left if at_port_selection && self.single_line_state.active => {
        //     if self.single_line_state.current_index == 0 {
        //         self.single_line_state.select(COMMON_BAUD.last_index());
        //     } else {
        //         self.single_line_state.prev();
        //     }
        // }
        match &mut self.popup {
            None => (),
            // Some(Popup::ErrorMessage(_)) => (),
            Some(Popup::AttemptReconnectPrompt)
            | Some(Popup::DisconnectPrompt)
            | Some(Popup::IgnoreByName(_))
            | Some(Popup::IgnoreByUsb(_, _))
            | Some(Popup::SerialConnectionFailed(_))
            | Some(Popup::CurrentKeybinds) => (),
            #[cfg(not(any(feature = "espflash", feature = "macros")))]
            Some(Popup::SettingsMenu(_)) if self.popup_menu_scroll == 0 => {}
            #[cfg(any(feature = "espflash", feature = "macros"))]
            Some(Popup::SettingsMenu(_)) | Some(Popup::ToolMenu(_))
                if self.popup_menu_scroll == 0 =>
            {
                self.cycle_menu_type();
            }
            #[cfg(any(feature = "espflash", feature = "macros"))]
            Some(Popup::ToolMenu(_)) if self.popup_menu_scroll == 1 => {
                self.cycle_sub_menu(true);
            }
            Some(Popup::SettingsMenu(_)) if self.popup_menu_scroll == 1 => {
                self.cycle_sub_menu(true);
            }
            Some(Popup::SettingsMenu(SettingsMenu::SerialPort)) => {
                self.scratch
                    .serial
                    .handle_input(ArrowKey::Right, self.get_corrected_popup_index().unwrap())
                    .unwrap();

                if let Some(0) = self.get_corrected_popup_index() {
                    self.baud_input = self.scratch.serial.baud_rate.to_string().into();
                }
            }
            Some(Popup::SettingsMenu(SettingsMenu::Behavior)) => {
                self.scratch
                    .behavior
                    .handle_input(ArrowKey::Right, self.get_corrected_popup_index().unwrap())
                    .unwrap();
            }
            Some(Popup::SettingsMenu(SettingsMenu::Rendering)) => {
                self.scratch
                    .rendering
                    .handle_input(ArrowKey::Right, self.get_corrected_popup_index().unwrap())
                    .unwrap();
            }
            #[cfg(feature = "macros")]
            Some(Popup::ToolMenu(ToolMenu::Macros)) => {
                if !self.macros.search_input.value().is_empty() {
                    return;
                }
                self.macros.categories_selector.next();
                if self.popup_menu_scroll >= POPUP_MENU_SELECTOR_COUNT {
                    if self.macros.none_visible() {
                        self.popup_menu_scroll = 1;
                    } else {
                        self.popup_menu_scroll = 2;
                    }
                }
            }
            #[cfg(feature = "espflash")]
            Some(Popup::ToolMenu(ToolMenu::EspFlash)) => {
                if self.popup_menu_scroll == POPUP_MENU_SELECTOR_COUNT + 1 {
                    self.espflash.unchecked_bootloader.flip();
                }
            }
            #[cfg(feature = "logging")]
            Some(Popup::SettingsMenu(SettingsMenu::Logging)) => {
                if self.popup_menu_scroll == POPUP_MENU_SELECTOR_COUNT + Logging::VISIBLE_FIELDS {
                    return;
                }

                self.scratch
                    .logging
                    .handle_input(ArrowKey::Right, self.get_corrected_popup_index().unwrap())
                    .unwrap();
            }
            #[cfg(feature = "defmt")]
            Some(Popup::SettingsMenu(SettingsMenu::Defmt)) => {
                use crate::tui::defmt::DEFMT_BUTTONS;

                if self.popup_menu_scroll < POPUP_MENU_SELECTOR_COUNT + DEFMT_BUTTONS {
                    return;
                }

                self.scratch
                    .defmt
                    .handle_input(ArrowKey::Right, self.get_corrected_popup_index().unwrap())
                    .unwrap();
            }
            #[cfg(feature = "defmt")]
            Some(Popup::DefmtNewElf(_)) => (),
            #[cfg(feature = "defmt")]
            Some(Popup::DefmtRecentElf) => (),

            Some(Popup::UpdateCheckConsentPrompt) | Some(Popup::UpdateBeginPrompt) => (),

            #[cfg(feature = "self-replace")]
            Some(Popup::UpdateDownloading(_)) => (),

            #[cfg(all(windows, feature = "self-replace"))]
            Some(Popup::UpdateLaunchPrompt) => (),
        }
        if self.popup.is_some() {
            return;
        }
        if matches!(self.menu, Menu::PortSelection)
            && self.port_selection_scroll == self.ports.len()
        {
            if self.baud_selection_state.current_index == COMMON_BAUD.last_index() {
                self.baud_selection_state.select(0);
            } else {
                self.baud_selection_state.next();
            }
        }
    }
    fn enter_pressed(&mut self, ctrl_pressed: bool, shift_pressed: bool) -> Result<()> {
        let serial_healthy = self.serial.port_status.load().status().is_connected();
        let popup_was_some = self.popup.is_some();
        // debug!("{:?}", self.menu);
        match &self.popup {
            None => (),
            Some(Popup::SettingsMenu(_)) if self.popup_menu_scroll < POPUP_MENU_SELECTOR_COUNT => {
                return Ok(());
            }
            #[cfg(any(feature = "espflash", feature = "macros"))]
            Some(Popup::ToolMenu(_)) if self.popup_menu_scroll < POPUP_MENU_SELECTOR_COUNT => {
                return Ok(());
            }
            // Some(Popup::ErrorMessage(_)) => self.dismiss_popup(),
            Some(Popup::CurrentKeybinds) => self.dismiss_popup(),
            Some(Popup::SettingsMenu(SettingsMenu::SerialPort)) => {
                let baud_rate = match self.baud_input.value().parse::<u32>() {
                    Ok(baud) => baud,
                    Err(e) => {
                        self.notifs
                            .notify_str(format!("Invalid Baud Rate: {e}!"), Color::Red);
                        return Ok(());
                    }
                };
                self.scratch.serial.baud_rate = baud_rate;

                self.settings.serial = self.scratch.serial.clone();
                self.buffer
                    .update_line_ending(self.scratch.serial.rx_line_ending.as_bytes());

                self.serial.update_settings(self.scratch.serial.clone())?;

                self.settings.save()?;
                self.notifs.notify_str("Port settings saved!", Color::Green);

                self.dismiss_popup();
            }
            Some(Popup::SettingsMenu(SettingsMenu::Behavior)) => {
                self.settings.behavior = self.scratch.behavior.clone();

                self.settings.save()?;
                self.dismiss_popup();
                self.notifs
                    .notify_str("Behavior settings saved!", Color::Green);
            }
            Some(Popup::SettingsMenu(SettingsMenu::Rendering)) => {
                self.settings.rendering = self.scratch.rendering.clone();
                self.buffer
                    .update_render_settings(self.settings.rendering.clone());

                self.settings.save()?;
                self.dismiss_popup();
                self.notifs
                    .notify_str("Rendering settings saved!", Color::Green);
            }
            #[cfg(feature = "logging")]
            Some(Popup::SettingsMenu(SettingsMenu::Logging)) => {
                // if Sync Logs button was selected
                if self.popup_menu_scroll == POPUP_MENU_SELECTOR_COUNT + Logging::VISIBLE_FIELDS {
                    self.run_builtin_action(BuiltinAction::Logging(LoggingAction::Sync))?;
                    return Ok(());
                }
                // Otherwise, save settings.

                self.settings.logging = self.scratch.logging.clone();
                // let current_port = {
                //     let port_status_guard = self.serial.port_status.load();
                //     port_status_guard.current_port.clone()
                // };
                self.buffer
                    .update_logging_settings(self.settings.logging.clone())?;

                self.settings.save()?;
                self.dismiss_popup();
                self.notifs
                    .notify_str("Logging settings saved!", Color::Green);
            }
            #[cfg(feature = "defmt")]
            Some(Popup::SettingsMenu(SettingsMenu::Defmt)) => {
                if self.popup_menu_scroll == POPUP_MENU_SELECTOR_COUNT {
                    // open file selector
                    if shift_pressed || ctrl_pressed {
                        self.run_builtin_action(BuiltinAction::ShowDefmtSelect(
                            DefmtSelectAction::SelectSystem,
                        ))?;
                    } else {
                        self.run_builtin_action(BuiltinAction::ShowDefmtSelect(
                            DefmtSelectAction::SelectTui,
                        ))?;
                    }

                    return Ok(());
                } else if self.popup_menu_scroll == POPUP_MENU_SELECTOR_COUNT + 1 {
                    // open recent selector
                    self.show_popup(Popup::DefmtRecentElf);
                    return Ok(());
                }
                // Otherwise, save settings.

                self.settings.defmt = self.scratch.defmt.clone();

                self.buffer
                    .update_defmt_settings(self.settings.defmt.clone());

                self.settings.save()?;
                self.dismiss_popup();
                self.notifs
                    .notify_str("defmt settings saved!", Color::Green);
            }
            #[cfg(feature = "macros")]
            Some(Popup::ToolMenu(ToolMenu::Macros)) => {
                // Category selector active, just ignore.
                if self.popup_menu_scroll == 2 {
                    return Ok(());
                }
                let index = self
                    .get_corrected_popup_index()
                    .expect("expected cursor to be in table to get a macro");
                let (tag, content) = self.macros.filtered_macro_iter().nth(index).unwrap();
                let tag = tag.to_owned();
                // let macro_ref: MacroNameTag = macro_binding.into();

                if ctrl_pressed || shift_pressed {
                    if !serial_healthy {
                        self.notifs.notify_str("Port isn't ready!", Color::Red);
                        return Ok(());
                    }
                    // Putting macro content into buffer.
                    match content {
                        _ if content.is_empty() => (),
                        bytes if content.has_escaped_bytes => {
                            self.text_input
                                .replace_input_with_bytes(&bytes.unescape_bytes());
                            self.dismiss_popup();
                        }
                        text => {
                            self.text_input.replace_input_with_text(text.as_str());
                            self.dismiss_popup();
                        }
                    }
                } else {
                    if !serial_healthy {
                        self.notifs.notify_str("Port isn't ready!", Color::Red);
                        return Ok(());
                    }
                    match content {
                        _ if content.is_empty() => {
                            self.notifs.notify_str("Macro is empty!", Color::Yellow)
                        }
                        _ => {
                            self.send_one_macro(tag, None)?;
                            // self.action_queue.push_back((None, tag));
                            // self.tx.send(Tick::Action.into()).unwrap();
                        }
                    };
                }
            }
            #[cfg(feature = "espflash")]
            Some(Popup::ToolMenu(ToolMenu::EspFlash)) => {
                if !serial_healthy {
                    self.notifs.notify_str("Port isn't ready!", Color::Red);
                    return Ok(());
                }
                let selected = self.get_corrected_popup_index().unwrap();
                // If a profile is selected
                if self.popup_menu_scroll >= esp::ESPFLASH_BUTTON_COUNT + POPUP_MENU_SELECTOR_COUNT
                {
                    assert!(
                        !self.espflash.is_empty(),
                        "shouldn't have selected a non-existant flash profile"
                    );

                    self.esp_flash_profile(self.espflash.profile_from_index(selected).unwrap())?;
                } else {
                    match selected {
                        0 => self.run_builtin_action(EspBuiltinAction::EspHardReset.into())?,
                        1 if self.espflash.unchecked_bootloader => self
                            .run_builtin_action(EspBuiltinAction::EspBootloaderUnchecked.into())?,
                        1 if ctrl_pressed || shift_pressed => self
                            .run_builtin_action(EspBuiltinAction::EspBootloaderUnchecked.into())?,
                        1 => self.run_builtin_action(EspBuiltinAction::EspBootloader.into())?,
                        2 => self.run_builtin_action(EspBuiltinAction::EspDeviceInfo.into())?,
                        3 => {
                            use crate::tui::esp::ERASE_FLASH_CONFIRM_PERIOD;

                            self.carousel.add_oneshot(
                                "UnreadyEraseFlash",
                                Tick::Requested("UnreadyEraseFlash"),
                                ERASE_FLASH_CONFIRM_PERIOD,
                            )?;

                            let erase_now = if let Some(duration) = self
                                .espflash
                                .first_erase_press
                                .as_ref()
                                .map(Instant::elapsed)
                                && duration <= ERASE_FLASH_CONFIRM_PERIOD
                            {
                                true
                            } else if self.settings.espflash.skip_erase_confirm {
                                true
                            } else {
                                shift_pressed || ctrl_pressed
                            };

                            self.espflash.first_erase_press = if erase_now {
                                self.run_builtin_action(EspBuiltinAction::EspEraseFlash.into())?;
                                None
                            } else {
                                self.notifs.notify_str(
                                    "Press again to confirm erasing flash!",
                                    Color::Yellow,
                                );
                                Some(Instant::now())
                            };
                        }
                        unknown => unreachable!("unknown espflash command index {unknown}"),
                    }
                }
            }
            #[cfg(feature = "defmt")]
            Some(Popup::DefmtNewElf(_)) => (),
            #[cfg(feature = "defmt")]
            Some(Popup::DefmtRecentElf) => {
                if !self.defmt_helpers.recent_elfs.is_empty() {
                    let elf_path = self
                        .defmt_helpers
                        .recent_elfs
                        .nth_path(self.popup_menu_scroll)
                        .unwrap()
                        .to_owned();
                    self.try_load_defmt_elf(
                        &elf_path,
                        #[cfg(feature = "defmt-watch")]
                        false,
                    );
                }

                self.dismiss_popup();
            }
            Some(Popup::AttemptReconnectPrompt) => {
                self.reconnect_prompt_choice(
                    AttemptReconnectPrompt::try_from(self.popup_menu_scroll as u8).unwrap(),
                    shift_pressed,
                    ctrl_pressed,
                )?;
            }
            Some(Popup::DisconnectPrompt) => {
                self.disconnect_prompt_choice(
                    DisconnectPrompt::try_from(self.popup_menu_scroll as u8).unwrap(),
                )?;
            }
            Some(Popup::IgnoreByUsb(_, _)) => {
                self.ignore_usb_device_prompt_choice(
                    IgnoreUsbDevicePrompt::try_from(self.popup_menu_scroll as u8).unwrap(),
                )?;
            }
            Some(Popup::IgnoreByName(_)) => {
                self.ignore_port_name_prompt_choice(
                    IgnorePortByNamePrompt::try_from(self.popup_menu_scroll as u8).unwrap(),
                )?;
            }
            Some(Popup::SerialConnectionFailed(_)) => self.dismiss_popup(),
            Some(Popup::UpdateCheckConsentPrompt) => {
                self.update_check_consent_choice(
                    UpdateCheckConsentPrompt::try_from(self.popup_menu_scroll as u8).unwrap(),
                )?;
            }
            Some(Popup::UpdateBeginPrompt) => {
                self.update_begin_choice(
                    UpdateBeginPrompt::try_from(self.popup_menu_scroll as u8).unwrap(),
                )?;
            }
            #[cfg(all(windows, feature = "self-replace"))]
            Some(Popup::UpdateLaunchPrompt) => {
                self.update_launch_choice(
                    UpdateLaunchPrompt::try_from(self.popup_menu_scroll as u8).unwrap(),
                )?;
            }
            #[cfg(feature = "self-replace")]
            Some(Popup::UpdateDownloading(_)) => (),
        }
        if self.popup.is_some() || popup_was_some {
            return Ok(());
        }

        match self.menu {
            Menu::PortSelection => {
                match (
                    self.port_selection_scroll,
                    self.ports.get(self.port_selection_scroll),
                ) {
                    (scroll, Some(port_info)) if scroll < self.ports.len() => {
                        info!("Port {}", port_info.port_name);

                        let baud_rate = if COMMON_BAUD
                            .last_index_eq(self.baud_selection_state.current_index)
                        {
                            match self.baud_input.value().parse::<u32>() {
                                Ok(b) => b,
                                Err(e) => {
                                    self.notifs
                                        .notify_str(format!("Invalid Baud Rate: {e}!"), Color::Red);
                                    return Ok(());
                                }
                            }
                        } else {
                            COMMON_BAUD[self.baud_selection_state.current_index]
                        };

                        self.settings.serial.baud_rate = baud_rate;
                        self.settings.save()?;

                        match self.serial.connect_blocking(
                            port_info.clone(),
                            self.settings.serial.clone(),
                            Some(baud_rate),
                            CONNECT_ATTEMPT_BLOCK_MAX,
                        ) {
                            Ok(()) => {
                                self.menu = Menu::Terminal;
                            }
                            Err(BlockingCommandError::Worker(e)) => {
                                let report = color_eyre::Report::new(e);
                                let mut error_string = String::new();
                                for e in report.chain() {
                                    error_string.push_str(&format!("\n{e}"));
                                }
                                self.popup = Some(Popup::SerialConnectionFailed(error_string));
                            }
                            Err(e) => Err(e)?,
                        }
                    }
                    (scroll, None) if scroll < self.ports.len() => {
                        unreachable!()
                    }
                    (scroll, _) => {
                        if scroll == self.port_selection_item_count() - 1 {
                            self.show_popup_from_action(ShowPopupAction::ShowPortSettings);
                        }
                    }
                }
            }
            Menu::Terminal if !serial_healthy => {
                self.trigger_send_failed_visual()?;
            }
            Menu::Terminal if self.settings.behavior.pseudo_shell => {
                let user_input = self.text_input.value();

                let user_le = &self.settings.serial.tx_line_ending;
                let user_le_bytes = user_le.as_bytes(&self.settings.serial.rx_line_ending);

                if self.text_input.byte_entry_active() {
                    // Odd-length inputs can't be parsed to be sent.
                    if !user_input.len().is_multiple_of(2) {
                        self.trigger_send_failed_visual()?;
                        return Ok(());
                    }

                    let bytes = match hex::decode(user_input) {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            error!("Failed parsing user's bytes to send to port! {e}");
                            self.notifs
                                .notify_str(format!("Byte parse error! `{e}`"), Color::Red);
                            return Ok(());
                        }
                    };

                    let user_le_bytes = if self.settings.behavior.send_line_ending_with_bytes {
                        // maybe also if ctrl/shift pressed?
                        Some(user_le_bytes)
                    } else {
                        None
                    };

                    if user_le_bytes.is_some() || !bytes.is_empty() {
                        self.serial.send_bytes(bytes.clone(), user_le_bytes)?;
                        self.buffer.append_user_bytes(
                            &bytes,
                            user_le_bytes.unwrap_or(&[]),
                            #[cfg(feature = "macros")]
                            None,
                        );
                        self.repeating_line_flip.flip();
                    }
                } else if !user_input.is_empty() || !user_le_bytes.is_empty() {
                    self.serial.send_str(
                        user_input,
                        user_le_bytes,
                        self.settings.behavior.unescape_typed_bytes,
                    )?;
                    self.buffer.append_user_text(
                        user_input,
                        user_le_bytes,
                        #[cfg(feature = "macros")]
                        None,
                    );
                    self.repeating_line_flip.flip();
                }

                self.text_input.commit_input_to_history();

                // Scroll all the way down
                // TODO: Make this behavior a toggle
                self.buffer.scroll_by(i32::MIN);
            }
            Menu::Terminal => (),
        }
        Ok(())
    }
    fn trigger_send_failed_visual(&mut self) -> Result<()> {
        self.failed_send_at = Some(Instant::now());
        // Temporarily show text on red background when trying to send while unhealthy

        self.carousel.add_oneshot(
            "UnhealthyTxUi",
            Tick::Requested("Unhealthy TX Background Removal"),
            FAILED_SEND_VISUAL_TIME,
        )?;
        Ok(())
    }
    fn return_to_port_selection(&mut self) -> Result<()> {
        self.serial.request_disconnect()?;
        // Refresh port listings
        self.ports = self.serial.request_port_scan_blocking(SCAN_BLOCK_MAX)?;

        self.buffer.intentional_disconnect_clear()?;
        // Clear the input box, but keep the user history!
        self.text_input.clear();

        self.dismiss_popup();
        self.menu = Menu::PortSelection;
        self.port_selection_scroll = 0;

        Ok(())
    }
    fn disconnect_prompt_choice(&mut self, choice: DisconnectPrompt) -> Result<()> {
        match choice {
            DisconnectPrompt::ExitApp => self.shutdown(),
            DisconnectPrompt::Cancel => self.dismiss_popup(),
            DisconnectPrompt::OpenPortSettings => {
                self.show_popup(Popup::SettingsMenu(SettingsMenu::SerialPort));
            }
            DisconnectPrompt::DisconnectFromPort => {
                // This is intentionally being set true unconditionally here, and also when the event pops.
                // This is so that I or a user can forcibly trigger the pausing of reconnections/the appearance of the
                // manual reconnection Esc popup.
                self.user_broke_connection = true;
                self.show_popup(Popup::AttemptReconnectPrompt);
                self.serial.request_break_connection()?;

                // let port_status_guard = self.serial.port_status.load();
                // if port_status_guard.inner.is_healthy() {
                // } else {
                // self.notifs
                // .notify_str("Can't break connection!", Color::Red);
                // }
            }
            DisconnectPrompt::BackToPortSelection => self.return_to_port_selection()?,
        }
        Ok(())
    }
    fn reconnect_prompt_choice(
        &mut self,
        choice: AttemptReconnectPrompt,
        shift_pressed: bool,
        ctrl_pressed: bool,
    ) -> Result<()> {
        match choice {
            AttemptReconnectPrompt::ExitApp => self.shutdown(),
            AttemptReconnectPrompt::AttemptReconnect if self.user_broke_connection => {
                self.user_broke_connection = false;
                self.dismiss_popup();
                self.notifs
                    .notify_str("Unpausing reconnections!", Color::LightGreen);
            }
            AttemptReconnectPrompt::AttemptReconnect if shift_pressed || ctrl_pressed => {
                self.repeating_line_flip.flip();
                self.notifs
                    .notify_str("Attempting to reconnect! (Loose Checks)", Color::Yellow);
                self.serial
                    .request_reconnect(Some(Reconnections::LooseChecks))?;
            }
            AttemptReconnectPrompt::AttemptReconnect => {
                self.repeating_line_flip.flip();
                self.notifs
                    .notify_str("Attempting to reconnect! (Strict Checks)", Color::Yellow);
                self.serial
                    .request_reconnect(Some(Reconnections::StrictChecks))?;
            }
            AttemptReconnectPrompt::Cancel => self.dismiss_popup(),
            AttemptReconnectPrompt::OpenPortSettings => {
                self.show_popup_from_action(ShowPopupAction::ShowPortSettings)
            }

            AttemptReconnectPrompt::BackToPortSelection => self.return_to_port_selection()?,
        }
        Ok(())
    }
    fn ignore_usb_device_prompt_choice(&mut self, choice: IgnoreUsbDevicePrompt) -> Result<()> {
        let Some(Popup::IgnoreByUsb(name, usb)) = self.popup.take() else {
            unreachable!("Can't ignore usb device without info!");
        };

        let mut usb_entry = DeserializedUsb::from(usb);

        match choice {
            IgnoreUsbDevicePrompt::Cancel => (),
            IgnoreUsbDevicePrompt::IgnoreByName => self.add_ignored_name(name)?,
            IgnoreUsbDevicePrompt::IgnoreByVidPid => {
                usb_entry.serial_number = None;
                self.add_ignored_usb(usb_entry)?;
            }
            IgnoreUsbDevicePrompt::IgnoreByVidPidSerial => self.add_ignored_usb(usb_entry)?,
        }
        self.dismiss_popup();
        Ok(())
    }
    fn ignore_port_name_prompt_choice(&mut self, choice: IgnorePortByNamePrompt) -> Result<()> {
        let Some(Popup::IgnoreByName(name_to_ignore)) = self.popup.take() else {
            unreachable!("Can't ignore port without a name!");
        };

        match choice {
            IgnorePortByNamePrompt::Cancel => (),
            IgnorePortByNamePrompt::IgnoreByName => self.add_ignored_name(name_to_ignore)?,
        }
        self.dismiss_popup();
        Ok(())
    }
    fn add_ignored_name(&mut self, name: String) -> Result<()> {
        if name == MOCK_PORT_NAME {
            return Ok(());
        }
        self.settings.ignored_devices.name.push(name.to_owned());
        self.settings.save()?;

        self.serial
            .new_ignored(self.settings.ignored_devices.clone())?;
        self.ports.clear();
        self.serial.request_port_scan()?;
        Ok(())
    }
    fn add_ignored_usb(&mut self, usb: DeserializedUsb) -> Result<()> {
        self.settings.ignored_devices.usb.push(usb);
        self.settings.save()?;

        self.serial
            .new_ignored(self.settings.ignored_devices.clone())?;
        self.ports.clear();
        self.serial.request_port_scan()?;
        Ok(())
    }
    /// Get max number of selectable elements for port selection screen.
    ///
    /// Panics if on a different screen.
    fn port_selection_item_count(&self) -> usize {
        assert!(matches!(&self.menu, Menu::PortSelection));

        let is_custom_visible = COMMON_BAUD.last_index_eq(self.baud_selection_state.current_index);

        self.ports.len()
        + is_custom_visible as usize
        + 1 // baud selector
        + 1 // options button
    }
    /// Get max number of selectable elements for current popup.
    ///
    /// Includes popup category selectors if present.
    ///
    /// Panics if no popup is active.
    fn current_popup_selectable_item_count(&self) -> usize {
        let Some(popup) = &self.popup else {
            panic!("no popup means no item count!")
        };
        match popup {
            #[cfg(feature = "defmt")]
            Popup::DefmtRecentElf => self.defmt_helpers.recent_elfs.len(),
            Popup::SettingsMenu(settings) => {
                let items = match settings {
                    SettingsMenu::SerialPort => PortSettings::VISIBLE_FIELDS,
                    SettingsMenu::Behavior => Behavior::VISIBLE_FIELDS,
                    SettingsMenu::Rendering => Rendering::VISIBLE_FIELDS,
                    #[cfg(feature = "logging")]
                    SettingsMenu::Logging => {
                        1 + // Start/Stop Logging button
                Logging::VISIBLE_FIELDS
                    }
                    #[cfg(feature = "defmt")]
                    SettingsMenu::Defmt => {
                        2 + // Select New/Recent ELF buttons
                Defmt::VISIBLE_FIELDS
                    }
                };
                items + POPUP_MENU_SELECTOR_COUNT
            }
            #[cfg(any(feature = "espflash", feature = "macros"))]
            Popup::ToolMenu(tool) => {
                let items = match tool {
                    #[cfg(feature = "macros")]
                    ToolMenu::Macros => {
                        1 + // Macros' category selector
                    self.macros.visible_len()
                    }

                    #[cfg(feature = "espflash")]
                    // TODO proper scrollbar for espflash profiles
                    ToolMenu::EspFlash => esp::ESPFLASH_BUTTON_COUNT + self.espflash.len(),
                };
                items + POPUP_MENU_SELECTOR_COUNT
            }
            Popup::DisconnectPrompt => <DisconnectPrompt as VariantArray>::VARIANTS.len(),
            Popup::AttemptReconnectPrompt => {
                <AttemptReconnectPrompt as VariantArray>::VARIANTS.len()
            }
            Popup::IgnoreByName(_) => <IgnorePortByNamePrompt as VariantArray>::VARIANTS.len(),
            Popup::IgnoreByUsb(_, _) => <IgnoreUsbDevicePrompt as VariantArray>::VARIANTS.len(),
            Popup::UpdateBeginPrompt => <UpdateBeginPrompt as VariantArray>::VARIANTS.len(),
            Popup::UpdateCheckConsentPrompt => {
                <UpdateCheckConsentPrompt as VariantArray>::VARIANTS.len()
            }
            #[cfg(all(windows, feature = "self-replace"))]
            Popup::UpdateLaunchPrompt => <UpdateLaunchPrompt as VariantArray>::VARIANTS.len(),
            _ => unreachable!("popup {popup:?} has no item count"),
        }
    }

    /// Gets corrected index of selected element.
    ///
    /// Returns None if the popup category or subcategory selectors are active.
    ///
    /// Used to select the current active element in tables.
    ///
    /// Panics if no popup is active.
    fn get_corrected_popup_index(&self) -> Option<usize> {
        let Some(popup) = &self.popup else {
            unreachable!("popup {:?} has no item count", self.popup);
        };

        let raw_scroll = self.popup_menu_scroll;

        match (popup, raw_scroll) {
            // Menu selectors active
            (_, 0) => None,
            (_, 1) => None,
            #[cfg(feature = "macros")]
            // Macro Categories selector active
            (Popup::ToolMenu(ToolMenu::Macros), POPUP_MENU_SELECTOR_COUNT) => None,

            // Normal settings menus
            // Just correct for the category selector
            (Popup::SettingsMenu(SettingsMenu::SerialPort), _)
            | (Popup::SettingsMenu(SettingsMenu::Rendering), _)
            | (Popup::SettingsMenu(SettingsMenu::Behavior), _) => {
                Some(self.popup_menu_scroll - POPUP_MENU_SELECTOR_COUNT)
            }

            #[cfg(feature = "macros")]
            // Macros being selected
            (Popup::ToolMenu(ToolMenu::Macros), _) => {
                Some(raw_scroll - (1 + POPUP_MENU_SELECTOR_COUNT))
            }

            #[cfg(feature = "logging")]
            // Logging settings and sync button
            (Popup::SettingsMenu(SettingsMenu::Logging), _) => {
                Some(raw_scroll - POPUP_MENU_SELECTOR_COUNT)
            }

            #[cfg(feature = "espflash")]
            // espflash user profiles
            (Popup::ToolMenu(ToolMenu::EspFlash), _)
                if raw_scroll >= esp::ESPFLASH_BUTTON_COUNT + POPUP_MENU_SELECTOR_COUNT =>
            {
                Some(raw_scroll - (esp::ESPFLASH_BUTTON_COUNT + POPUP_MENU_SELECTOR_COUNT))
            }
            #[cfg(feature = "espflash")]
            // espflash pre-set action buttons
            (Popup::ToolMenu(ToolMenu::EspFlash), _) => {
                Some(raw_scroll - POPUP_MENU_SELECTOR_COUNT)
            }

            #[cfg(feature = "defmt")]
            // defmt settings
            (Popup::SettingsMenu(SettingsMenu::Defmt), _)
                if raw_scroll >= 2 + POPUP_MENU_SELECTOR_COUNT =>
            {
                Some(raw_scroll - (2 + POPUP_MENU_SELECTOR_COUNT))
            }
            #[cfg(feature = "defmt")]
            // defmt select new/recent elf buttons
            (Popup::SettingsMenu(SettingsMenu::Defmt), _) => {
                Some(raw_scroll - POPUP_MENU_SELECTOR_COUNT)
            }
            _ => unreachable!("popup {:?} has no item count", self.popup),
        }
    }
    /// Returns true if the final item in a popup is selected,
    /// used for item select wrapping purposes.
    ///
    /// Panics if no popup is active.
    fn last_popup_item_selected(&self) -> bool {
        let Some(_popup) = &self.popup else {
            unreachable!("popup {:?} has no item count", self.popup);
        };

        let current_popup_item_count = self.current_popup_selectable_item_count();
        // debug!("{raw_scroll}, {selector_corrected_scroll}, {current_popup_item_count}");
        self.popup_menu_scroll >= current_popup_item_count.saturating_sub(1)
    }
    /// Returns true if the final item in a popup is selected,
    /// used for item select wrapping purposes.
    ///
    /// Panics if no popup is active.
    fn select_last_popup_item(&mut self) {
        assert!(self.popup.is_some());

        self.popup_menu_scroll = self.current_popup_selectable_item_count().saturating_sub(1);
    }
    pub fn draw(&mut self, terminal: &mut Terminal<impl Backend>) -> Result<()> {
        // let start = Instant::now();
        terminal.draw(|frame| self.render_app(frame))?;
        // debug!("A4: {:?}", start.elapsed());
        Ok(())
    }
    fn render_app(&mut self, frame: &mut Frame) {
        // self.buffer.update_terminal_size(frame.area().as_size());
        // TODO, make more reactive based on frame size :)

        // let start = Instant::now();
        match self.menu {
            Menu::PortSelection => self.port_selection(frame),
            Menu::Terminal => self.terminal_menu(frame),
        }
        // debug!("a1: {:?}", start.elapsed());

        // let start = Instant::now();
        self.render_popups(frame, frame.area());
        // debug!("a2: {:?}", start.elapsed());

        // let start = Instant::now();
        self.render_notifs(frame, frame.area());
        // debug!("a3: {:?}", start.elapsed());

        #[cfg(feature = "espflash")]
        self.espflash.render_espflash_popups(frame, frame.area());

        // TODO:
        // self.render_error_messages(frame, frame.area());
    }
    fn render_notifs(&mut self, frame: &mut Frame, area: Rect) {
        if let Some(notif) = &self.notifs.inner {
            frame.render_widget(&self.notifs, area);
            if notif.shown_for() >= EXPIRE_TIME {
                _ = self.notifs.inner.take();
            }
        }
    }
    fn render_popups(&mut self, frame: &mut Frame, area: Rect) {
        let Some(popup) = &self.popup else {
            return;
        };
        match popup {
            Popup::SettingsMenu(_) => self.render_popup_menus(frame, area),
            #[cfg(any(feature = "espflash", feature = "macros"))]
            Popup::ToolMenu(_) => self.render_popup_menus(frame, area),
            Popup::CurrentKeybinds => {
                let mut scroll: u16 = self.popup_menu_scroll as u16;
                show_keybinds(&self.keybinds, &mut scroll, frame, area, self);
                self.popup_menu_scroll = scroll as usize;
            }
            #[cfg(feature = "defmt")]
            Popup::DefmtNewElf(file_explorer) => {
                let area = centered_rect_size(
                    Size {
                        width: 70,
                        height: 20,
                    },
                    area,
                );
                frame.render_widget(Clear, area);
                frame.render_widget(&file_explorer.widget(), area);
            }
            #[cfg(feature = "defmt")]
            Popup::DefmtRecentElf => {
                let area = centered_rect_size(
                    Size {
                        width: 80,
                        height: 15,
                    },
                    area,
                );

                let title = Line::raw(" Select from recently used ELFs: ")
                    .centered()
                    .reset();

                let block = Block::bordered()
                    .border_style(Style::new().light_red())
                    .title_top(title);

                let inner = block.inner(area);

                let mut table_state = TableState::new().with_selected(Some(self.popup_menu_scroll));

                frame.render_widget(Clear, area);
                frame.render_widget(block, area);
                frame.render_stateful_widget(
                    self.defmt_helpers.recent_elfs.as_table(),
                    inner,
                    &mut table_state,
                );
            }
            Popup::AttemptReconnectPrompt => {
                let user_broke_connection = if self.user_broke_connection {
                    Some("(Reconnections paused!)")
                } else {
                    None
                };
                let mut table_state = TableState::new().with_selected(Some(self.popup_menu_scroll));
                AttemptReconnectPrompt::render_prompt_block_popup(
                    Some("Reconnect to port?"),
                    user_broke_connection,
                    Style::new().red(),
                    frame,
                    area,
                    &mut table_state,
                )
            }
            Popup::DisconnectPrompt => {
                let port_state = { self.serial.port_status.load().status() };
                let reconns_paused = if self.settings.serial.reconnections.allowed()
                    && port_state.is_premature_disconnect()
                {
                    Some("(Auto-reconnections paused while open)")
                } else {
                    None
                };
                let mut table_state = TableState::new().with_selected(Some(self.popup_menu_scroll));
                DisconnectPrompt::render_prompt_block_popup(
                    Some("Disconnect from port?"),
                    reconns_paused,
                    Style::new().blue(),
                    frame,
                    area,
                    &mut table_state,
                );
            }
            Popup::IgnoreByUsb(name, usb) => {
                let UsbPortInfo {
                    vid,
                    pid,
                    serial_number,
                    ..
                } = usb;
                let mut table_state = TableState::new().with_selected(Some(self.popup_menu_scroll));
                let mut info = format!("VID: {vid:04X}, PID: {pid:04X}, Serial #: ");
                if let Some(serial_num) = serial_number {
                    info.push_str(serial_num);
                } else {
                    info.push_str("N/A");
                }
                IgnoreUsbDevicePrompt::render_prompt_block_popup(
                    Some(&format!("Hide {name} (USB) from Port Selection?")),
                    Some(&info),
                    Style::new().yellow(),
                    frame,
                    area,
                    &mut table_state,
                );
            }
            Popup::IgnoreByName(name) => {
                let mut table_state = TableState::new().with_selected(Some(self.popup_menu_scroll));
                IgnorePortByNamePrompt::render_prompt_block_popup(
                    Some(&format!("Hide {name} from Port Selection?")),
                    Some(name),
                    Style::new().yellow(),
                    frame,
                    area,
                    &mut table_state,
                );
            }
            Popup::SerialConnectionFailed(error) => {
                let title = "Error connecting to port!";
                let title_line = Line::styled(title, Style::new().reset());
                let block = Block::bordered()
                    .border_style(Style::new().red())
                    .title_top(title_line)
                    .title_alignment(ratatui::layout::Alignment::Center);
                let error_lines = error.lines().map(Line::raw).collect::<Vec<_>>();
                let error_lines_len = error_lines.len();
                let max_line_len = error.lines().map(str::len).max().unwrap();

                let area = centered_rect_size(
                    Size {
                        width: max_line_len.max(title.len()) as u16 + 6,
                        height: error_lines_len as u16 + 3,
                    },
                    area,
                );

                let para = Paragraph::new(error_lines).red().centered();

                frame.render_widget(Clear, area);
                frame.render_widget(&block, area);
                frame.render_widget(para, block.inner(area));
            }
            Popup::UpdateCheckConsentPrompt => {
                let mut table_state = TableState::new().with_selected(Some(self.popup_menu_scroll));

                UpdateCheckConsentPrompt::render_prompt_block_popup(
                    Some("Allow checking for updates on startup?"),
                    None,
                    Style::new().yellow(),
                    frame,
                    area,
                    &mut table_state,
                );
            }
            Popup::UpdateBeginPrompt => {
                let mut table_state = TableState::new().with_selected(Some(self.popup_menu_scroll));

                let current_version = env!("CARGO_PKG_VERSION");
                let new_version = self.update_found_version.as_ref().unwrap();

                UpdateBeginPrompt::render_prompt_block_popup(
                    Some(&format!("New version found! v{current_version}")),
                    Some(&format!("v{current_version} -> v{new_version}")),
                    Style::new().green(),
                    frame,
                    area,
                    &mut table_state,
                );
            }
            #[cfg(all(windows, feature = "self-replace"))]
            Popup::UpdateLaunchPrompt => {
                let mut table_state = TableState::new().with_selected(Some(self.popup_menu_scroll));

                UpdateLaunchPrompt::render_prompt_block_popup(
                    Some("Launch new version now?"),
                    None,
                    Style::new().green(),
                    frame,
                    area,
                    &mut table_state,
                );
            }
            #[cfg(feature = "self-replace")]
            Popup::UpdateDownloading(percentage) => {
                let center_area = centered_rect_size(
                    Size {
                        width: 60,
                        height: 8,
                    },
                    area,
                );

                // Create the outer block with borders and title
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().light_blue())
                    .title("Update Downloading...");

                // Get the inner area of the block
                let inner_area = block.inner(center_area);

                let color = if *percentage >= 1.0 {
                    Color::Green
                } else {
                    Color::LightBlue
                };

                let gauge = ratatui::widgets::Gauge::default()
                    .ratio(*percentage)
                    .gauge_style(color);

                frame.render_widget(Clear, inner_area);
                frame.render_widget(block, center_area);
                frame.render_widget(gauge, inner_area);
            }
        }
    }
    fn render_popup_menus(&mut self, frame: &mut Frame, area: Rect) {
        let popup_color = match &self.popup {
            Some(Popup::SettingsMenu(SettingsMenu::Rendering)) => Color::Red,
            Some(Popup::SettingsMenu(SettingsMenu::Behavior)) => Color::Blue,
            Some(Popup::SettingsMenu(SettingsMenu::SerialPort)) => Color::Cyan,
            #[cfg(feature = "espflash")]
            Some(Popup::ToolMenu(ToolMenu::EspFlash)) => Color::Magenta,
            #[cfg(feature = "defmt")]
            Some(Popup::SettingsMenu(SettingsMenu::Defmt)) => Color::LightRed,
            #[cfg(feature = "logging")]
            Some(Popup::SettingsMenu(SettingsMenu::Logging)) => Color::Yellow,
            #[cfg(feature = "macros")]
            Some(Popup::ToolMenu(ToolMenu::Macros)) => Color::Green,
            _ => return,
        };

        let center_area = centered_rect_size(
            Size {
                width: area.width.min(60),
                height: area.height.min(17),
            },
            area,
        );
        frame.render_widget(Clear, center_area);

        let block = Block::bordered().border_style(Style::from(popup_color));

        let block_render_area = {
            let mut area = center_area;
            area.height = area.height.saturating_sub(1);
            area.y += 1;

            area
        };

        frame.render_widget(&block, block_render_area);

        // let title_lines = ;
        let mut menu_selector_state = SingleLineSelectorState::new();
        menu_selector_state.active = self.popup_menu_scroll == 0;
        let popup_menu_title_selector = SingleLineSelector::new([
            "Settings".italic(),
            #[cfg(any(feature = "espflash", feature = "macros"))]
            "Tools".italic(),
        ])
        .with_next_symbol(">")
        .with_prev_symbol("<")
        .with_space_padding(true);

        let menu_category_selector_area = {
            let mut line = center_area;
            line.height = 1;
            line
        };

        let category_index = match &self.popup {
            Some(Popup::SettingsMenu(_)) => 0,
            #[cfg(any(feature = "espflash", feature = "macros"))]
            Some(Popup::ToolMenu(_)) => 1,
            _ => unreachable!("popup isnt a settings or tool menu"),
        };

        menu_selector_state.select(category_index);
        frame.render_stateful_widget(
            &popup_menu_title_selector,
            menu_category_selector_area,
            &mut menu_selector_state,
        );
        match &self.popup {
            Some(Popup::SettingsMenu(_)) => {
                self.render_settings_popup(frame, block.inner(center_area), popup_color)
            }
            #[cfg(any(feature = "espflash", feature = "macros"))]
            Some(Popup::ToolMenu(_)) => {
                self.render_tool_popup(frame, block.inner(center_area), popup_color)
            }
            _ => unreachable!("popup isnt a settings or tool menu"),
        }
    }
    fn render_settings_popup(
        &mut self,
        frame: &mut Frame,
        center_inner_area: Rect,
        block_color: Color,
    ) {
        let Some(Popup::SettingsMenu(popup)) = &self.popup else {
            return;
        };

        let block = Block::new()
            .borders(Borders::TOP | Borders::BOTTOM)
            .style(Style::from(block_color));

        let selected = self.get_corrected_popup_index();
        let mut table_state = TableState::new()
            .with_selected(selected)
            .with_selected_column(Some(usize::MAX));

        let setting_menu_selector =
            SingleLineSelector::new(<SettingsMenu as VariantNames>::VARIANTS.iter().copied())
                .with_next_symbol(">")
                .with_prev_symbol("<")
                .with_space_padding(true);
        let selector_area = {
            let mut line = center_inner_area;
            line.height = 1;
            line
        };
        let mut menu_selector_state = SingleLineSelectorState::new();
        menu_selector_state.select(
            <SettingsMenu as VariantArray>::VARIANTS
                .iter()
                .position(|v| v == popup)
                .expect("current menu must exist within enum variantarray"),
        );
        menu_selector_state.active = self.popup_menu_scroll == 1;
        frame.render_widget(&block, selector_area);
        frame.render_stateful_widget(
            &setting_menu_selector,
            selector_area,
            &mut menu_selector_state,
        );

        let settings_area = {
            let mut area = center_inner_area;
            area.height = area.height.saturating_sub(3);
            area.y += 1;
            area
        };

        let button_hint_text_area = {
            let mut area = center_inner_area;
            area.y = area.bottom();
            area.height = 1;
            area
        };

        let bottom_sep_line_area = {
            let mut area = center_inner_area;
            area.y = area.bottom().saturating_sub(2);
            area.height = 1;
            area
        };
        let scrolling_text_area = {
            let mut area = center_inner_area;
            area.y = area.bottom().saturating_sub(1);
            area.height = 1;
            area
        };

        frame.render_widget(
            Block::new()
                .borders(Borders::TOP)
                .border_style(Style::from(block_color)),
            bottom_sep_line_area,
        );

        let scrollbar_style = Style::new().reset();

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(scrollbar_style)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"));

        let height = settings_area.height;

        match popup {
            SettingsMenu::SerialPort => {
                frame.render_stateful_widget(
                    self.scratch.serial.as_table(),
                    settings_area,
                    &mut table_state,
                );

                // If Baud Rate is selected, render using the normal Input method
                if let Some(0) = self.get_corrected_popup_index() {
                    let [_, mut input_area] = horizontal![==50%, ==50%].areas(settings_area);
                    input_area.height = 1;
                    frame.render_widget(Clear, input_area);

                    let baud_text = self.baud_input.value();
                    let baud_line = Line::raw(baud_text).centered();

                    let width = input_area.width.max(1).saturating_sub(2); // Unsure why this required -1 more than the others.

                    // Calculate padding for centered text
                    let text_width = baud_line.width() as u16;
                    let pad_left = if width > text_width {
                        (width - text_width) / 2
                    } else {
                        0
                    };

                    let scroll = self.baud_input.visual_scroll(width as usize);
                    let input_text = Paragraph::new(baud_line)
                        .scroll((0, scroll as u16))
                        .reversed()
                        .italic();

                    frame.render_widget(input_text, input_area);

                    // Cursor logic: trailing edge after the last char, with center offset
                    let cursor_pos = self.baud_input.visual_cursor();
                    let centered_offset = pad_left as i32 + (cursor_pos as i32 - scroll as i32);
                    let cursor_x = input_area.x + centered_offset.max(0) as u16;

                    frame.set_cursor_position((cursor_x + 1, input_area.y));
                }

                let text: &str = table_state
                    .selected()
                    .map(|i| PortSettings::DOCSTRINGS[i])
                    .unwrap_or("");
                render_scrolling_line(
                    text,
                    frame,
                    scrolling_text_area,
                    &mut self.popup_hint_scroll,
                );
                frame.render_widget(
                    Line::raw("Esc: Cancel | Enter: Save")
                        .all_spans_styled(Color::DarkGray.into())
                        .centered(),
                    button_hint_text_area,
                );
            }
            SettingsMenu::Behavior => {
                frame.render_stateful_widget(
                    self.scratch.behavior.as_table(),
                    settings_area,
                    &mut table_state,
                );
                let text: &str = table_state
                    .selected()
                    .map(|i| Behavior::DOCSTRINGS[i])
                    .unwrap_or("");
                render_scrolling_line(
                    text,
                    frame,
                    scrolling_text_area,
                    &mut self.popup_hint_scroll,
                );
                frame.render_widget(
                    Line::raw("Esc: Cancel | Enter: Save")
                        .all_spans_styled(Color::DarkGray.into())
                        .centered(),
                    button_hint_text_area,
                );
            }
            SettingsMenu::Rendering => {
                frame.render_stateful_widget(
                    self.scratch.rendering.as_table(),
                    settings_area,
                    &mut table_state,
                );
                let text: &str = table_state
                    .selected()
                    .map(|i| Rendering::DOCSTRINGS[i])
                    .unwrap_or("");
                render_scrolling_line(
                    text,
                    frame,
                    scrolling_text_area,
                    &mut self.popup_hint_scroll,
                );
                frame.render_widget(
                    Line::raw("Esc: Cancel | Enter: Save")
                        .all_spans_styled(Color::DarkGray.into())
                        .centered(),
                    button_hint_text_area,
                );
            }
            #[cfg(feature = "logging")]
            SettingsMenu::Logging => {
                let button_area = {
                    let mut area = settings_area;
                    area.y = area.bottom().saturating_sub(1);
                    area.height = 1;
                    area
                };
                let new_separator = {
                    let mut area = settings_area;
                    area.y = area.bottom().saturating_sub(2);
                    area.height = 1;
                    area
                };
                let settings_area = {
                    let mut area = settings_area;
                    area.height = area.height.saturating_sub(2);
                    area
                };
                let line_block = Block::new()
                    .borders(Borders::TOP)
                    .border_style(Style::from(block_color));

                let logs_dir = config_adjacent_path("logs/");
                let log_path_text = format!("Saving to: {logs_dir}");
                let log_path_line = Line::raw(log_path_text)
                    .all_spans_styled(Color::DarkGray.into())
                    .centered();

                if log_path_line.width() <= bottom_sep_line_area.width as usize {
                    frame.render_widget(log_path_line, bottom_sep_line_area);
                } else {
                    frame.render_widget(log_path_line.right_aligned(), bottom_sep_line_area);
                }

                frame.render_widget(
                    Line::raw("Esc: Close | Enter: Select/Save")
                        .all_spans_styled(Color::DarkGray.into())
                        .centered(),
                    button_hint_text_area,
                );

                let sync_button = sync_logs_button();
                let log_sync_selected =
                    self.popup_menu_scroll == POPUP_MENU_SELECTOR_COUNT + Logging::VISIBLE_FIELDS;
                if log_sync_selected {
                    frame.render_stateful_widget(sync_button, button_area, &mut table_state);
                    frame.render_widget(&line_block, new_separator);
                    frame.render_widget(self.scratch.logging.as_table(), settings_area);

                    let text =
                        "Re-sync active log files with entire buffer content and current settings.";
                    render_scrolling_line(
                        text,
                        frame,
                        scrolling_text_area,
                        &mut self.popup_hint_scroll,
                    );
                } else {
                    frame.render_widget(sync_button, button_area);
                    frame.render_widget(&line_block, new_separator);
                    frame.render_stateful_widget(
                        self.scratch.logging.as_table(),
                        settings_area,
                        &mut table_state,
                    );

                    let text: &str = table_state
                        .selected()
                        .map(|i| Logging::DOCSTRINGS[i])
                        .unwrap_or("");
                    render_scrolling_line(
                        text,
                        frame,
                        scrolling_text_area,
                        &mut self.popup_hint_scroll,
                    );
                }
            }
            #[cfg(feature = "defmt")]
            SettingsMenu::Defmt => {
                let new_separator = {
                    let mut area = center_inner_area;
                    area.y = area.top().saturating_add(5);
                    area.height = 1;
                    area
                };
                let defmt_settings_area = {
                    let mut area = center_inner_area;
                    area.y = area.top().saturating_add(6);
                    area.height = area.height.saturating_sub(8);
                    area
                };
                let line_block = Block::new()
                    .borders(Borders::TOP)
                    .border_style(Style::from(block_color));
                frame.render_widget(
                    Line::raw("Powered by knurling-rs/defmt v1.0.0!")
                        .all_spans_styled(Color::DarkGray.into())
                        .centered(),
                    bottom_sep_line_area,
                );

                let [
                    elf_title,
                    current_elf,
                    select_new_elf,
                    select_recent_elf,
                    _rest,
                ] = vertical![==1,==1,==1,==1,*=1].areas(settings_area);

                frame.render_widget(
                    Line::raw("Esc: Close | Enter: Select/Save")
                        .all_spans_styled(Color::DarkGray.into())
                        .centered(),
                    button_hint_text_area,
                );

                let settings_selected = self.popup_menu_scroll >= POPUP_MENU_SELECTOR_COUNT + 2;

                // frame.render_widget(esp::espflash_buttons(), settings_area);
                frame.render_widget(&line_block, new_separator);

                let current_elf_str = if let Some(decoder) = &self.buffer.defmt_decoder {
                    decoder.elf_path.as_str()
                } else {
                    "None"
                };

                let select_style = if self.popup_menu_scroll == 2 {
                    Style::new().reversed()
                } else {
                    Style::new()
                };
                let recent_style = if self.popup_menu_scroll == 3 {
                    Style::new().reversed()
                } else {
                    Style::new()
                };

                let current_elf_text = if let Some(decoder) = &self.buffer.defmt_decoder {
                    Cow::Owned(format!(
                        "Current ELF MD5: {}",
                        &decoder.elf_md5.as_str()[..8]
                    ))
                } else {
                    Cow::Borrowed("Current ELF:")
                };

                frame.render_widget(
                    Line::raw(current_elf_text).centered().dark_gray(),
                    elf_title,
                );
                frame.render_widget(Line::raw(current_elf_str).centered(), current_elf);
                frame.render_widget(
                    Line::raw("[Select ELF File]")
                        .centered()
                        .all_spans_styled(select_style),
                    select_new_elf,
                );
                frame.render_widget(
                    Line::raw("[Select Recent ELF]")
                        .centered()
                        .all_spans_styled(recent_style),
                    select_recent_elf,
                );

                if settings_selected {
                    use crate::settings::Defmt;

                    frame.render_stateful_widget(
                        self.scratch.defmt.as_table(),
                        defmt_settings_area,
                        &mut table_state,
                    );
                    let text: &str = table_state
                        .selected()
                        .map(|i| Defmt::DOCSTRINGS[i])
                        .unwrap_or("");
                    render_scrolling_line(
                        text,
                        frame,
                        scrolling_text_area,
                        &mut self.popup_hint_scroll,
                    );
                } else {
                    let corrected_index = self.get_corrected_popup_index();

                    frame.render_widget(self.scratch.defmt.as_table(), defmt_settings_area);

                    let hints = [
                        "Select an ELF file to decode defmt packets with. Shift/Ctrl to use native system file picker.",
                        "Select from a list of recently used ELFs.",
                    ];
                    if let Some(idx) = corrected_index
                        && let Some(&hint_text) = hints.get(idx)
                    {
                        render_scrolling_line(
                            hint_text,
                            frame,
                            scrolling_text_area,
                            &mut self.popup_hint_scroll,
                        );
                    }
                }
                frame.render_widget(
                    Line::raw("Settings:")
                        .all_spans_styled(Color::DarkGray.into())
                        .centered(),
                    new_separator,
                );
            }
        }

        let content_length = self.current_popup_selectable_item_count();
        let mut scrollbar_state = ScrollbarState::new(
            content_length
                .saturating_sub(height as usize)
                .saturating_sub(POPUP_MENU_SELECTOR_COUNT),
        )
        .position(table_state.offset());

        frame.render_stateful_widget(
            scrollbar,
            center_inner_area
                .offset(Offset { x: 1, y: 0 })
                .inner(Margin {
                    horizontal: 0,
                    vertical: 1,
                }),
            &mut scrollbar_state,
        );
    }
    #[cfg(any(feature = "espflash", feature = "macros"))]
    fn render_tool_popup(
        &mut self,
        frame: &mut Frame,
        center_inner_area: Rect,
        block_color: Color,
    ) {
        let Some(Popup::ToolMenu(popup)) = &self.popup else {
            return;
        };

        let mut menu_selector_state = SingleLineSelectorState::new();

        let selected = self.get_corrected_popup_index();
        let mut table_state = TableState::new()
            .with_selected(selected)
            .with_selected_column(Some(usize::MAX));

        let popup_menu_title_selector =
            SingleLineSelector::new(<ToolMenu as VariantNames>::VARIANTS.iter().copied())
                .with_next_symbol(">")
                .with_prev_symbol("<")
                .with_space_padding(true);
        let selector_area = {
            let mut line = center_inner_area;
            line.height = 1;
            line
        };
        menu_selector_state.select(
            <ToolMenu as VariantArray>::VARIANTS
                .iter()
                .position(|v| v == popup)
                .unwrap(),
        );
        menu_selector_state.active = self.popup_menu_scroll == 1;
        frame.render_stateful_widget(
            &popup_menu_title_selector,
            selector_area,
            &mut menu_selector_state,
        );

        #[cfg(feature = "espflash")]
        let bins_area = {
            let mut area = center_inner_area;
            area.height = area.height.saturating_sub(8);
            area.y = area.top().saturating_add(6);
            area
        };

        let hint_text_area = {
            let mut area = center_inner_area;
            area.y = area.bottom();
            area.height = 1;
            area
        };

        let line_area = {
            let mut area = center_inner_area;
            area.y = area.bottom().saturating_sub(2);
            area.height = 1;
            area
        };
        let scrolling_text_area = {
            let mut area = center_inner_area;
            area.y = area.bottom().saturating_sub(1);
            area.height = 1;
            area
        };

        #[cfg(feature = "macros")]
        let macros_table_area = {
            let mut area = center_inner_area;
            area.height = area.height.saturating_sub(5);
            area.y += 3;
            area
        };

        frame.render_widget(
            Block::new()
                .borders(Borders::TOP)
                .border_style(Style::from(block_color)),
            line_area,
        );

        let scrollbar_style = Style::new().reset();

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(scrollbar_style)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"));

        let height = match popup {
            #[cfg(feature = "macros")]
            ToolMenu::Macros => macros_table_area.height,
            #[cfg(feature = "espflash")]
            ToolMenu::EspFlash => bins_area.height,
        };

        match popup {
            #[cfg(feature = "macros")]
            ToolMenu::Macros => {
                let new_separator = {
                    let mut area = center_inner_area;
                    area.height = 1;
                    area.y += 2;
                    area
                };
                let categories_area = {
                    let mut area = center_inner_area;
                    area.height = 1;
                    area.y += 1;
                    area
                };
                frame.render_widget(
                    Block::new()
                        .borders(Borders::TOP)
                        .border_style(Style::from(block_color)),
                    new_separator,
                );

                if self.macros.search_input.value().is_empty() {
                    let categories_iter = ["Has Bytes", "Strings Only", "All Macros"]
                        .iter()
                        .copied()
                        .map(String::from)
                        .map(Line::raw)
                        .chain(self.macros.categories().map(String::from).map(Line::raw));
                    let categories_selector = SingleLineSelector::new(categories_iter)
                        .with_next_symbol(">")
                        .with_prev_symbol("<")
                        .with_size_hint(popup_menu_title_selector.max_chars());
                    self.macros.categories_selector.active = self.popup_menu_scroll == 2;
                    frame.render_stateful_widget(
                        &categories_selector,
                        categories_area,
                        &mut self.macros.categories_selector,
                    );
                } else {
                    // Get the search text
                    let search_text = self.macros.search_input.value();
                    // Center the search line in the area
                    let search_line = Line::raw(search_text).centered();

                    let width = categories_area.width.max(1).saturating_sub(1); // So the cursor doesn't bleed off the edge

                    // Calculate padding for centered text
                    let text_width = search_line.width() as u16;
                    let pad_left = if width > text_width {
                        (width - text_width) / 2
                    } else {
                        0
                    };

                    // Can't scroll centered lines horizontally??
                    let scroll = self.macros.search_input.visual_scroll(width as usize);
                    let input_text = Paragraph::new(search_line).scroll((0, scroll as u16));

                    frame.render_widget(input_text, categories_area);

                    // Cursor logic: trailing edge after the last char, with center offset
                    let cursor_pos = self.macros.search_input.visual_cursor();
                    let centered_offset = pad_left as i32 + (cursor_pos as i32 - scroll as i32);
                    let cursor_x = categories_area.x + centered_offset.max(0) as u16;

                    frame.set_cursor_position((cursor_x + 1, categories_area.y));
                }

                let table = self
                    .macros
                    .as_table(&self.keybinds, self.settings.behavior.fuzzy_macro_match);

                frame.render_stateful_widget(table, macros_table_area, &mut table_state);

                frame.render_widget(
                    Line::raw("Ctrl-R: Reload")
                        .all_spans_styled(Color::DarkGray.into())
                        .centered(),
                    line_area,
                );

                frame.render_widget(
                    Line::raw("Esc: Close | Enter: Send")
                        .all_spans_styled(Color::DarkGray.into())
                        .centered(), // .dark_gray()
                    hint_text_area,
                );

                if let Some(index) = table_state.selected() {
                    let (_, content) = self.macros.filtered_macro_iter().nth(index).unwrap();
                    // for now i guess
                    // TOOD replace with fancy line preview
                    let macro_preview = content.as_str();
                    let line = if !content.sensitive {
                        use ratatui::text::ToLine;

                        macro_preview.to_line().italic()
                    } else {
                        Line::from(span!("[SENSITIVE]")).italic()
                    };
                    // let line = if matches!(macro_binding.content, MacroContent::Bytes { .. }) {
                    //     line.light_blue()
                    // } else {
                    //     line
                    // };
                    render_scrolling_line(
                        line,
                        frame,
                        scrolling_text_area,
                        &mut self.popup_hint_scroll,
                    );
                } else if self.macros.is_empty() {
                    frame.render_widget(
                        Line::raw("No macros! Try making one!").centered(),
                        scrolling_text_area,
                    );
                } else {
                    frame.render_widget(
                        Line::raw("Select a macro to preview.").centered(),
                        scrolling_text_area,
                    );
                }
            }
            #[cfg(feature = "espflash")]
            ToolMenu::EspFlash => {
                let new_separator = {
                    let mut area = center_inner_area;
                    area.height = 1;
                    area.y = area.top().saturating_add(5);
                    area
                };
                let espflash_buttons_area = {
                    let mut area = center_inner_area;
                    area.height = area.height.saturating_sub(3);
                    area.y += 1;
                    area
                };
                let line_block = Block::new()
                    .borders(Borders::TOP)
                    .border_style(Style::from(block_color));
                frame.render_widget(
                    Line::raw("Powered by esp-rs/espflash v4.1.0!")
                        .all_spans_styled(Color::DarkGray.into())
                        .centered(),
                    line_area,
                );

                frame.render_widget(
                    Line::raw("Esc: Close | Enter: Select")
                        .all_spans_styled(Color::DarkGray.into())
                        .centered(),
                    hint_text_area,
                );

                let profiles_selected = self.popup_menu_scroll
                    >= esp::ESPFLASH_BUTTON_COUNT + POPUP_MENU_SELECTOR_COUNT;

                // if self.popup_menu_item ==
                if profiles_selected {
                    frame.render_widget(
                        esp::espflash_buttons(
                            self.espflash.unchecked_bootloader,
                            self.espflash.first_erase_press.is_some(),
                        ),
                        espflash_buttons_area,
                    );
                    frame.render_widget(&line_block, new_separator);
                    frame.render_stateful_widget(
                        self.espflash.profiles_table(),
                        bins_area,
                        &mut table_state,
                    );
                    if let Some(corrected_index) = table_state.selected()
                        && let Some(profile) = self.espflash.profile_from_index(corrected_index)
                    {
                        use crate::tui::esp::{EspBins, EspElf, EspProfile};

                        let upper_chip = |chip: &espflash::target::Chip| {
                            use compact_str::ToCompactString;

                            chip.to_compact_string().to_ascii_uppercase()
                        };
                        let chip = match &profile {
                            EspProfile::Bins(EspBins { expected_chip, .. })
                            | EspProfile::Elf(EspElf { expected_chip, .. }) => {
                                if let Some(chip) = expected_chip {
                                    Cow::from(upper_chip(chip))
                                } else {
                                    Cow::from("ESP")
                                }
                            }
                        };

                        let hint_text = match &profile {
                            esp::EspProfile::Bins(bins) if bins.bins.len() == 1 => {
                                format!("Flash selected profile binary to {chip} Flash.")
                            }
                            esp::EspProfile::Bins(_) => {
                                format!("Flash selected profile binaries to {chip} Flash.")
                            }
                            esp::EspProfile::Elf(profile) if profile.ram => {
                                format!("Load selected profile ELF into {chip} RAM.")
                            }
                            esp::EspProfile::Elf(_) => {
                                format!("Flash selected profile ELF to {chip} Flash.")
                            }
                        };
                        render_scrolling_line(
                            hint_text,
                            frame,
                            scrolling_text_area,
                            &mut self.popup_hint_scroll,
                        );
                    }
                } else {
                    frame.render_stateful_widget(
                        esp::espflash_buttons(
                            self.espflash.unchecked_bootloader,
                            self.espflash.first_erase_press.is_some(),
                        ),
                        espflash_buttons_area,
                        &mut table_state,
                    );
                    frame.render_widget(&line_block, new_separator);
                    frame.render_widget(self.espflash.profiles_table(), bins_area);

                    let hints = [
                        "Attempt to remotely reset the chip.",
                        "Attempt to reboot into bootloader. Shift/Ctrl to skip check.",
                        "Query ESP for Flash Size, MAC Address, etc.",
                        "Erase all flash contents.",
                    ];
                    if let Some(button_index) = table_state.selected()
                        && let Some(&hint_text) = hints.get(button_index)
                    {
                        render_scrolling_line(
                            hint_text,
                            frame,
                            scrolling_text_area,
                            &mut self.popup_hint_scroll,
                        );
                    }
                }
                frame.render_widget(
                    Line::raw("Flash Profiles | Ctrl-R: Reload")
                        .all_spans_styled(Color::DarkGray.into())
                        .centered(),
                    new_separator,
                );
            }
        }
        // TODO
        // shrink scrollbar and change content length based on if its for a submenu or not
        let content_length = self.current_popup_selectable_item_count();
        let mut scrollbar_state = ScrollbarState::new(
            content_length
                .saturating_sub(height as usize)
                .saturating_sub(POPUP_MENU_SELECTOR_COUNT),
        )
        .position(table_state.offset());

        frame.render_stateful_widget(
            scrollbar,
            center_inner_area
                .offset(Offset { x: 1, y: 0 })
                .inner(Margin {
                    horizontal: 0,
                    vertical: 1,
                }),
            &mut scrollbar_state,
        );
    }

    /// The main screen rendered when connected to a serial device.
    pub fn terminal_menu(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let popup_shown = self.popup.is_some();
        let [terminal_area, line_area, whole_input_area] = vertical![*=1, ==1, ==1].areas(area);
        let [input_symbol_area, input_area] = horizontal![==1, *=1].areas(whole_input_area);
        let dark_gray = Style::new().dark_gray();

        fn render_connection_timer(
            frame: &mut Frame,
            area: Rect,
            duration_opt: Option<Duration>,
            style: Style,
            lent_out_time: bool,
        ) {
            let conn_stopwatch_string: Cow<'_, str> = if let Some(duration) = duration_opt {
                let secs = duration.as_secs();
                let day = secs / 86400;
                let hour = (secs % 86400) / 3600;
                let min = (secs % 3600) / 60;
                let sec = secs % 60;

                let output = if day > 0 {
                    format!("{day:02}:{hour:02}:{min:02}:{sec:02}")
                } else {
                    format!("{hour:02}:{min:02}:{sec:02}")
                };

                let output = if lent_out_time {
                    format!("({output})")
                } else {
                    output
                };

                output.into()
            } else {
                "XX:XX:XX".into()
            };

            let [top_line] = vertical![==1].areas(area.inner(Margin {
                horizontal: 1,
                vertical: lent_out_time as u16,
            }));

            let line = Line::from(Span::styled(conn_stopwatch_string, style)).right_aligned();

            frame.render_widget(line, top_line);
        }

        fn render_connection_timers(frame: &mut Frame, area: Rect, port_state: &InnerPortStatus) {
            let dark_gray = Style::new().dark_gray();
            match port_state {
                InnerPortStatus::Connected { connected_at } => render_connection_timer(
                    frame,
                    area,
                    Some(connected_at.elapsed()),
                    dark_gray,
                    false,
                ),
                #[cfg(feature = "espflash")]
                InnerPortStatus::LentOut {
                    connected_at,
                    lent_out_at,
                } => {
                    render_connection_timer(
                        frame,
                        area,
                        Some(connected_at.elapsed()),
                        dark_gray,
                        false,
                    );
                    render_connection_timer(
                        frame,
                        area,
                        Some(lent_out_at.elapsed()),
                        Color::Yellow.into(),
                        true,
                    );
                }
                _ => render_connection_timer(frame, area, None, dark_gray, false),
            }
        }

        let (port_status, serial_signals, port_text) = {
            let port_status_guard = self.serial.port_status.load();
            let port_state = port_status_guard.status();

            let port_text = match &port_status_guard.current_port() {
                Some(port_info) => {
                    if port_state.is_connected() || port_state.is_lent_out() {
                        let baud_rate = self.serial.port_settings.load().baud_rate;
                        port_info.info_as_string(Some(baud_rate))
                    } else {
                        // Might remove later
                        let info = port_info.info_as_string(None);
                        format!("[!] {info} [!]")
                    }
                }
                None => {
                    error!("Port info missing during render!");
                    "Port info missing!".to_owned()
                }
            };

            (port_state, port_status_guard.signals().clone(), port_text)
        };

        let time_shown = self.settings.rendering.show_connection_time;

        // const SHOW_ABOVE_TIME: Duration = Duration::from_secs(30);

        // let time_above_text = match (self.settings.rendering.show_connection_time, port_status) {
        //     (Ct::BelowText, InnerPortStatus::Connected { connected_at })
        //         if connected_at.elapsed() <= SHOW_ABOVE_TIME =>
        //     {
        //         true
        //     }
        //     (Ct::BelowText, InnerPortStatus::LentOut { connected_at, .. })
        //         if connected_at.elapsed() <= SHOW_ABOVE_TIME =>
        //     {
        //         true
        //     }
        //     (Ct::BelowText, InnerPortStatus::PrematureDisconnect { disconnected_at })
        //         if disconnected_at.elapsed() <= SHOW_ABOVE_TIME =>
        //     {
        //         true
        //     }
        //     (Ct::AboveText, _) => true,
        //     (_, _) => false,
        // };
        // if time_shown && !time_above_text {
        //     render_connection_timers(frame, area, &port_status);
        // }

        if self.settings.rendering.hex_view {
            self.buffer.render_hex(terminal_area, frame.buffer_mut());
        } else {
            frame.render_widget(&mut self.buffer, terminal_area);
        }

        if time_shown {
            render_connection_timers(frame, area, &port_status);
        }

        repeating_pattern_widget(frame, line_area, self.repeating_line_flip, port_status);

        let widget_margin: u16 = if area.width >= 100 { 3 } else { 0 };

        #[cfg(debug_assertions)]
        {
            let line = Line::raw(format!(
                "Entries: {} | Lines: {}",
                self.buffer.port_lines_len(),
                self.buffer.combined_height()
            ))
            .right_aligned();
            frame.render_widget(
                line,
                line_area.inner(Margin {
                    horizontal: widget_margin,
                    vertical: 0,
                }),
            );
        }

        #[cfg(not(debug_assertions))]
        {
            let line =
                Line::raw(format!("Lines: {}", self.buffer.port_lines_len())).right_aligned();
            frame.render_widget(
                line,
                line_area.inner(Margin {
                    horizontal: widget_margin,
                    vertical: 0,
                }),
            );
        }

        if self.action_queue.is_empty() {
            let port_name_line = Line::raw(port_text).centered();
            frame.render_widget(port_name_line, line_area);
        } else {
            let port_name_line =
                Line::raw(format!("Queued Actions: {}", self.action_queue.len())).centered();
            frame.render_widget(port_name_line, line_area);
        }

        {
            let reversed_if_true = |signal: bool| -> Modifier {
                if signal {
                    Modifier::REVERSED
                } else {
                    Modifier::empty()
                }
            };

            let (dtr, rts, cts, dsr, ri, cd) = {
                let dtr = reversed_if_true(serial_signals.dtr);
                let rts = reversed_if_true(serial_signals.rts);

                let cts = reversed_if_true(serial_signals.cts);
                let dsr = reversed_if_true(serial_signals.dsr);
                let ri = reversed_if_true(serial_signals.ri);
                let cd = reversed_if_true(serial_signals.cd);

                (dtr, rts, cts, dsr, ri, cd)
            };

            let signals_spans = vec![
                span!["["],
                span![dtr;"DTR"],
                span![" "],
                span![rts;"RTS"],
                span!["|"],
                span![cts;"CTS"],
                span![" "],
                span![dsr;"DSR"],
                span![" "],
                span![ri;"RI"],
                span![" "],
                span![cd;"CD"],
                span!["]"],
            ];
            let signals_line = Line::from(signals_spans);
            frame.render_widget(
                signals_line,
                line_area.offset(Offset {
                    x: widget_margin as i32,
                    y: 0,
                }),
            );
        }

        let input_style = match (&self.failed_send_at, self.text_input.all_text_selected) {
            (Some(instant), _) if instant.elapsed() < FAILED_SEND_VISUAL_TIME => {
                Style::new().on_red()
            }
            (Some(instant), true) if instant.elapsed() < FAILED_SEND_VISUAL_TIME => {
                Style::new().reversed().on_red()
            }
            (_, true) => Style::new().reversed(),
            _ => Style::new(),
        };

        if self.settings.behavior.pseudo_shell {
            let input_symbol_style = if port_status.is_connected() {
                input_style.not_reversed().green()
            } else {
                input_style.red()
            };

            let input_symbol = if self.text_input.byte_entry_active() {
                "§"
            } else {
                ">"
            };

            frame.render_widget(
                Span::raw(input_symbol).style(input_symbol_style),
                input_symbol_area,
            );
        } else if self.popup.is_none() {
            let value = if self.last_raw_sequence.is_empty() {
                Cow::Borrowed("N/A")
            } else {
                Cow::Owned(
                    self.last_raw_sequence
                        .iter()
                        .map(|b| format!("\\x{b:02X}"))
                        .collect(),
                )
            };

            let line = line![span!(dark_gray; "Last sent: "), span!(dark_gray; value)];

            frame.render_widget(line, whole_input_area);
        }

        let should_position_cursor = !popup_shown;

        match (
            self.settings.behavior.pseudo_shell,
            self.text_input.value().is_empty(),
        ) {
            (true, true) => {
                // Leading space leaves room for full-width cursors.
                let port_settings_hint = self.keybinds.port_settings_hint();
                let input_type = if self.text_input.byte_entry_active() {
                    "Hex Bytes input goes here."
                } else {
                    "Text input goes here."
                };
                let input_hint = Line::raw(format!(
                    " {input_type} `{port_settings_hint}` for port settings.",
                ))
                .style(input_style)
                .dark_gray()
                .italic();
                frame.render_widget(input_hint, input_area);
                if should_position_cursor {
                    frame.set_cursor_position(input_area.as_position());
                }
            }
            (true, false) if self.text_input.byte_entry_active() => {
                pub fn visual_scroll(
                    cursor_pos: usize,
                    width: usize,
                    mut chars: impl Iterator<Item = char>,
                ) -> usize {
                    let scroll = (cursor_pos).max(width) - width;
                    let mut uscroll = 0;
                    while uscroll < scroll {
                        match chars.next() {
                            Some(c) => {
                                uscroll += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                            }
                            None => break,
                        }
                    }
                    uscroll
                }

                let spaced_bytes_iter =
                    itertools::Itertools::intersperse(self.text_input.entered_bytes_iter(), " ");
                let bytes_text = Line::from_iter(spaced_bytes_iter);
                let width = input_area.width.max(1).saturating_sub(1); // So the cursor doesn't bleed off the edge
                let adj_cursor = {
                    // note: the actual input_box holds no spaces for our bytes,
                    // which is why we add them in the iterators.
                    let cursor = self.text_input.input_box().visual_cursor();
                    // adding however many spaces are in front of the current bytes
                    cursor + cursor / 2
                };
                let input_para = Paragraph::new(bytes_text).style(input_style);

                let spaced_bytes_iter =
                    itertools::Itertools::intersperse(self.text_input.entered_bytes_iter(), " ");
                let chars_iter = spaced_bytes_iter.flat_map(|s| s.chars());

                let scroll = visual_scroll(adj_cursor, width as usize, chars_iter) as u16;

                frame.render_widget(input_para.scroll((0, scroll as u16)), input_area);
                if should_position_cursor {
                    frame.set_cursor_position((
                        // Put cursor past the end of the input text
                        input_area.x + ((adj_cursor as u16).max(scroll) - scroll) as u16,
                        input_area.y,
                    ));
                }
            }
            (true, false) => {
                let width = input_area.width.max(1).saturating_sub(1); // So the cursor doesn't bleed off the edge
                let scroll = self.text_input.input_box().visual_scroll(width as usize);
                let input_text = Paragraph::new(self.text_input.input_box().value())
                    .scroll((0, scroll as u16))
                    .style(input_style);
                frame.render_widget(input_text, input_area);
                if should_position_cursor {
                    frame.set_cursor_position((
                        // Put cursor past the end of the input text
                        input_area.x
                            + ((self.text_input.input_box().visual_cursor()).max(scroll) - scroll)
                                as u16,
                        input_area.y,
                    ));
                }
            }
            (false, _) if self.escape_next_keypress => {
                let input_hint =
                    Line::raw("Escaping next keypress, will not be sent to connected device.")
                        .style(input_style)
                        .yellow()
                        .centered();
                frame.render_widget(input_hint, whole_input_area);
            }
            (false, _) if self.popup.is_some() => {
                let input_hint = Line::raw("Popup is active, not sending keypresses.")
                    .style(input_style)
                    .dark_gray()
                    .centered();
                frame.render_widget(input_hint, whole_input_area);
            }
            (false, _) => {
                let escape_hint = self.keybinds.escape_keypress_hint();
                let input_hint = Line::raw(format!(
                    "Keyboard input is being sent directly. Press `{escape_hint}` to escape a keypress.",
                ))
                .style(input_style)
                .dark_gray()
                .centered();
                frame.render_widget(input_hint, whole_input_area);
            }
        }

        // debug!("2: {:?}", start.elapsed());
    }

    fn port_selection(&mut self, frame: &mut Frame) {
        let frame_area = frame.area();
        let vertical_slices = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Fill(4),
            Constraint::Fill(1),
        ])
        .split(frame_area);

        let big_text = BigText::builder()
            .pixel_size(PixelSize::Quadrant)
            .style(Style::new().blue())
            .centered()
            .lines(vec!["yap".blue().into()])
            .build();
        frame.render_widget(big_text, vertical_slices[0]);

        let dark_gray = Style::new().dark_gray();
        let green = Style::new().green();

        let [_, credit_and_version_area] = vertical![*=1, ==1].areas(frame_area);

        let current_version = Span::styled(format!("v{}", env!("CARGO_PKG_VERSION")), dark_gray);

        let version = if let Some(new) = &self.update_found_version {
            let meow = Span::styled(format!(" -> v{new}! [U]pdate found!"), green);
            Line::from_iter([current_version, meow])
        } else {
            Line::from(current_version)
        };

        let me_in_current_year = "nullstalgia, 2025".dark_gray();

        frame.render_widget(me_in_current_year, credit_and_version_area);
        frame.render_widget(version.right_aligned(), credit_and_version_area);

        let area = if vertical_slices[1].width < 45 {
            vertical_slices[1]
        } else {
            let [_, middle_area, _] = horizontal![==25%, ==50%, ==25%].areas(vertical_slices[1]);
            middle_area
        };

        let show_keybinds_hint = self.keybinds.show_keybinds_hint();
        let controls = line![
            span!(dark_gray;"Ignore port: [I] | Show Keybinds: [{show_keybinds_hint}] | Select: [Enter]")
        ].centered();

        let block = Block::bordered()
            .title("Port Selection")
            .border_style(Style::new().blue())
            .title_style(Style::reset())
            .title_alignment(ratatui::layout::Alignment::Center);

        let rows: Vec<Row> = self
            .ports
            .iter()
            .map(|p| {
                Row::new(vec![
                    // Column 1: Port name
                    Cow::Borrowed(p.port_name.as_str()),
                    // Column 2: Port info
                    match &p.port_type {
                        SerialPortType::UsbPort(usb) => {
                            let mut text = format!("[USB] {:04X}:{:04X}", usb.vid, usb.pid);
                            if let Some(serial_number) = &usb.serial_number {
                                text.push_str(" S/N: ");
                                text.push_str(serial_number);
                            }

                            Cow::Owned(text)
                        }
                        SerialPortType::BluetoothPort => Cow::Borrowed("[Bluetooth]"),
                        SerialPortType::PciPort => Cow::Borrowed("[PCI]"),
                        SerialPortType::Unknown if p.port_name == MOCK_PORT_NAME => {
                            Cow::Borrowed("[Mock Testing Port]")
                        }
                        #[cfg(unix)]
                        SerialPortType::Unknown if p.port_name.starts_with("/dev/ttyS") => {
                            Cow::Borrowed("[Virtual Console (TTY)]")
                        }
                        SerialPortType::Unknown => Cow::Borrowed("[Unspecified]"),
                    },
                ])
            })
            .collect();
        let widths = [Constraint::Percentage(25), Constraint::Percentage(75)];

        let table = Table::new(rows, widths)
            .row_highlight_style(Style::new().reversed())
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_symbol(">>");

        let [
            table_area,
            mut filler_or_custom_baud_entry,
            mut baud_text_area,
            mut baud_selector,
            more_options,
        ] = vertical![*=1, ==1, ==1, ==1, ==1].areas(block.inner(area));

        let custom_visible = COMMON_BAUD.last_index_eq(self.baud_selection_state.current_index);
        let ports_selected = self.port_selection_scroll < self.ports.len();
        let baud_selected = self.port_selection_scroll == self.port_selection_item_count() - 2;
        let baud_selected_when_custom =
            custom_visible && self.port_selection_scroll == self.port_selection_item_count() - 3;
        let options_selected = self.port_selection_scroll == self.port_selection_item_count() - 1;
        if custom_visible {
            // ------ "Baud Rate:" [115200]
            std::mem::swap(&mut filler_or_custom_baud_entry, &mut baud_text_area);
            //  "Baud Rate:" ------ [115200]
            std::mem::swap(&mut filler_or_custom_baud_entry, &mut baud_selector);
            //  "Baud Rate:" [115200] ------
        }

        // let static_baud = line![format!("← {current_baud} →")];

        let baud_text = line!["Baud Rate:"];

        let more_options_button = span![format!("[More options]")];

        frame.render_widget(block, area);
        frame.render_widget(
            controls,
            Rect {
                y: vertical_slices[1].bottom().saturating_sub(1),
                height: 1,
                ..vertical_slices[1]
            },
        );

        let table_state = if self.popup.is_none() && ports_selected {
            let mut table_state = TableState::new()
                .with_selected(Some(self.port_selection_scroll))
                .with_selected_column(Some(usize::MAX));

            frame.render_stateful_widget(table, table_area, &mut table_state);

            table_state
        } else {
            frame.render_widget(table, table_area);

            TableState::default()
        };

        frame.render_widget(baud_text.centered(), baud_text_area);

        let selector = SingleLineSelector::new(COMMON_BAUD.iter().map(|&b| {
            if b == 0 {
                "Custom:".to_string()
            } else {
                format!("{b:^6}")
            }
        }));

        self.baud_selection_state.active =
            (!custom_visible && baud_selected) || (custom_visible && baud_selected_when_custom);

        frame.render_stateful_widget(&selector, baud_selector, &mut self.baud_selection_state);

        if custom_visible {
            let [left, input_area, right] =
                horizontal![*=1, ==10, *=1].areas(filler_or_custom_baud_entry);

            let style = if baud_selected {
                Style::new().reversed()
            } else {
                Style::new()
            };

            frame.render_widget(Line::from(Span::styled("[", style)).right_aligned(), left);

            let user_text: &str = self.baud_input.value();

            let user_input = Line::raw(if user_text.is_empty() { " " } else { user_text });

            let width = input_area.width.max(1).saturating_sub(1); // So the cursor doesn't bleed off the edge
            let scroll = self.baud_input.visual_scroll(width as usize);
            let input_text = Paragraph::new(user_input)
                .scroll((0, scroll as u16))
                .style(style);
            frame.render_widget(input_text, input_area);

            if baud_selected {
                frame.set_cursor_position((
                    // Put cursor past the end of the input text
                    input_area.x + ((self.baud_input.visual_cursor()).max(scroll) - scroll) as u16,
                    input_area.y,
                ));
            }

            frame.render_widget(Line::from(Span::styled("]", style)).left_aligned(), right);
        }

        // let mut state = SingleLineSelectorState {
        //     current_index: COMMON_BAUD_DEFAULT,
        // };

        // frame.render_widget(static_baud.centered(), baud_selector);

        if options_selected {
            frame.render_widget(
                Line::from(more_options_button.reversed()).centered(),
                more_options,
            );
        } else {
            frame.render_widget(Line::from(more_options_button).centered(), more_options);
        }

        let scrollbar_style = Style::new().reset();

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(scrollbar_style)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"));

        let height = table_area.height.saturating_sub(2);

        // Scrollbar for ports
        let content_length = self.ports.len();
        let mut scrollbar_state =
            ScrollbarState::new(content_length.saturating_sub(height as usize))
                .position(table_state.offset());

        frame.render_stateful_widget(
            scrollbar,
            table_area.offset(Offset { x: 1, y: 0 }),
            &mut scrollbar_state,
        );

        let mut misc_lines = Vec::new();

        let bindings_with_unrecognized_actions = {
            self.keybinds
                .keybindings
                .iter()
                .filter(|(_, v)| v.iter().any(|a| self.get_action_from_string(a).is_none()))
                .count()
        };

        let show_keybinds_hint = self.keybinds.show_keybinds_hint();
        if bindings_with_unrecognized_actions > 0 {
            let line = Line::raw(format!(
                "{bindings_with_unrecognized_actions} keybindings with unknown actions, {show_keybinds_hint} to see all bindings."
            ))
            .centered()
            .yellow();
            misc_lines.push(line);
        } else {
            let line = Line::raw(format!("{show_keybinds_hint} to see all keybindings."))
                .centered()
                .dark_gray();
            misc_lines.push(line);
        }

        if let Some(socket_addr) = &self.settings.misc.log_tcp_socket
            && self.tcp_log_health.is_ok()
        {
            let line = line![
                "Tracing connected to TCP Listener at: ",
                socket_addr.to_string()
            ]
            .centered()
            .dark_gray();
            misc_lines.push(line);
        }

        let config_path = config_adjacent_path("");
        let config_path_line = line!["Config and logs at: ", config_path.to_string()]
            .centered()
            .dark_gray();
        misc_lines.push(config_path_line);

        // if !misc_lines.is_empty() {
        frame.render_widget(Paragraph::new(misc_lines), vertical_slices[2]);
        // }
    }
    fn refresh_scratch(&mut self) {
        self.scratch = self.settings.clone();
        #[cfg(feature = "espflash")]
        {
            self.espflash.unchecked_bootloader = false;
        }

        self.baud_input = self.settings.serial.baud_rate.to_string().into();
    }
    fn show_popup(&mut self, popup: Popup) {
        match &popup {
            Popup::CurrentKeybinds
            | Popup::AttemptReconnectPrompt
            | Popup::DisconnectPrompt
            | Popup::IgnoreByName(_)
            | Popup::IgnoreByUsb(_, _)
            | Popup::UpdateBeginPrompt
            | Popup::UpdateCheckConsentPrompt => self.popup_menu_scroll = 0,

            #[cfg(feature = "defmt")]
            Popup::DefmtRecentElf => {
                if self.defmt_helpers.recent_elfs.is_empty() {
                    self.notifs.notify_str(
                        "No recent ELFs to select from! Try loading some!",
                        Color::Red,
                    );
                    return;
                } else {
                    self.popup_menu_scroll = 0
                }
            }
            _ => self.popup_menu_scroll = 1,
        }

        self.popup = Some(popup);
        self.refresh_scratch();
        self.popup_hint_scroll = -2;

        let has_scrollable_text = match self.popup.as_ref().unwrap() {
            Popup::SettingsMenu(_) => true,
            #[cfg(any(feature = "espflash", feature = "macros"))]
            Popup::ToolMenu(_) => true,
            _ => false,
        };

        if has_scrollable_text {
            self.event_tx
                .send(Tick::Scroll.into())
                .map_err(|e| e.to_string())
                .expect("failed to send into own event queue?");
        }
    }
    fn show_popup_from_action(&mut self, popup: ShowPopupAction) {
        let popup_menu = match popup {
            ShowPopupAction::ShowKeybinds => Popup::CurrentKeybinds,
            ShowPopupAction::ShowPortSettings => Popup::SettingsMenu(SettingsMenu::SerialPort),
            ShowPopupAction::ShowBehavior => Popup::SettingsMenu(SettingsMenu::Behavior),
            ShowPopupAction::ShowRendering => Popup::SettingsMenu(SettingsMenu::Rendering),
            #[cfg(feature = "macros")]
            ShowPopupAction::ShowMacros => Popup::ToolMenu(ToolMenu::Macros),
            #[cfg(feature = "espflash")]
            ShowPopupAction::ShowEspFlash => Popup::ToolMenu(ToolMenu::EspFlash),
            #[cfg(feature = "logging")]
            ShowPopupAction::ShowLogging => Popup::SettingsMenu(SettingsMenu::Logging),
            #[cfg(feature = "defmt")]
            ShowPopupAction::ShowDefmt => Popup::SettingsMenu(SettingsMenu::Defmt),
        };

        // If user pressed the same popup keybind again, just dismiss it.
        if let Some(current) = &self.popup
            && *current == popup_menu
        {
            self.dismiss_popup();
        } else {
            self.show_popup(popup_menu);
        }
    }
    pub fn dismiss_popup(&mut self) {
        // These shouldn't be allowed to be dismissed ever.
        match &self.popup {
            #[cfg(feature = "self-replace")]
            Some(Popup::UpdateDownloading(_)) => return,
            #[cfg(all(windows, feature = "self-replace"))]
            Some(Popup::UpdateLaunchPrompt) => return,
            _ => (),
        }

        self.refresh_scratch();
        self.popup.take();
        self.popup_menu_scroll = 0;
        self.popup_hint_scroll = -2;
    }
    fn cycle_sub_menu(&mut self, next: bool) {
        match &mut self.popup {
            Some(Popup::SettingsMenu(popup)) => {
                let mut new_popup = if next { popup.next() } else { popup.prev() };
                std::mem::swap(popup, &mut new_popup);
            }
            #[cfg(any(feature = "espflash", feature = "macros"))]
            Some(Popup::ToolMenu(popup)) => {
                let mut new_popup = if next { popup.next() } else { popup.prev() };
                std::mem::swap(popup, &mut new_popup);
            }
            _ => return,
        }

        self.refresh_scratch();
        self.popup_hint_scroll = -2;
        #[cfg(feature = "macros")]
        self.macros.search_input.reset();
    }
    #[cfg(any(feature = "espflash", feature = "macros"))]
    fn cycle_menu_type(&mut self) {
        match &self.popup {
            Some(Popup::SettingsMenu(_)) => self.popup.insert(Popup::ToolMenu(
                <ToolMenu as VariantArray>::VARIANTS[0].clone(),
            )),
            Some(Popup::ToolMenu(_)) => self.popup.insert(Popup::SettingsMenu(
                <SettingsMenu as VariantArray>::VARIANTS[0].clone(),
            )),
            _ => return,
        };

        self.refresh_scratch();
        self.popup_hint_scroll = -2;
        #[cfg(feature = "macros")]
        self.macros.search_input.reset();
    }
    pub fn try_cli_connect(
        &mut self,
        port_info: SerialPortInfo,
        baud: Option<u32>,
    ) -> color_eyre::Result<()> {
        self.serial.connect_blocking(
            port_info.clone(),
            self.settings.serial.clone(),
            baud,
            CONNECT_ATTEMPT_BLOCK_MAX,
        )?;

        self.menu = Menu::Terminal;

        Ok(())
    }
    #[cfg(feature = "defmt")]
    /// Try to load the given file path as a defmt elf,
    /// if successful, loads decoder into Buffer and Logger
    /// and informs the ELF Watcher about this latest file.
    ///
    /// If you want to handle any actual errors from this process,
    /// use `_try_load_defmt_elf`
    pub fn try_load_defmt_elf(
        &mut self,
        path: &Utf8Path,
        #[cfg(feature = "defmt-watch")] reload: bool,
    ) {
        let success_text = {
            #[cfg(feature = "defmt-watch")]
            if reload {
                "defmt ELF reloaded due to file update!"
            } else {
                "defmt ELF loaded successfully!"
            }
            #[cfg(not(feature = "defmt-watch"))]
            "defmt ELF loaded successfully!"
        };
        let fail_text = {
            #[cfg(feature = "defmt-watch")]
            if reload {
                "defmt ELF auto-reload failed!"
            } else {
                "defmt ELF load failed!"
            }
            #[cfg(not(feature = "defmt-watch"))]
            "defmt ELF load failed!"
        };

        match _try_load_defmt_elf(
            path,
            &mut self.buffer.defmt_decoder,
            &mut self.defmt_helpers.recent_elfs,
            #[cfg(feature = "logging")]
            &self.buffer.log_handle,
            #[cfg(feature = "defmt-watch")]
            &mut self.defmt_helpers.watcher_handle,
        ) {
            Ok(None) => {
                self.notifs.notify_str(success_text, Color::Green);
            }
            Ok(Some(locs_err)) => {
                self.notifs.notify_str(
                    format!("defmt ELF had location data err: {locs_err}"),
                    Color::Yellow,
                );
            }
            Err(e) => {
                let text = format!("{fail_text} {e}");
                error!("{text}");
                self.notifs.notify_str(text, Color::Red);
            }
        }
    }
    fn first_time_setup(&mut self) {
        if !self.settings.updates.user_dismissed_prompt {
            self.show_popup(Popup::UpdateCheckConsentPrompt);
        }
    }
}

#[cfg(feature = "defmt")]
#[derive(Debug, thiserror::Error)]
pub enum YapLoadDefmtError {
    #[error("error adding elf to recents")]
    Recents(#[from] DefmtRecentError),
    #[error("failed parsing defmt from elf")]
    DefmtLoad(#[from] DefmtLoadError),
    #[cfg(feature = "logging")]
    #[error("failed to send logging worker new defmt table")]
    LoggingWorker(#[from] crate::buffer::LoggingWorkerMissing),
    #[cfg(feature = "defmt-watch")]
    #[error(transparent)]
    ElfWatcher(#[from] ElfWatcherMissing),
}

#[cfg(feature = "defmt")]
pub fn _try_load_defmt_elf(
    path: &Utf8Path,
    decoder_opt: &mut Option<Arc<DefmtDecoder>>,
    recent_elfs: &mut DefmtRecentElfs,
    #[cfg(feature = "logging")] logging: &crate::buffer::LoggingHandle,
    #[cfg(feature = "defmt-watch")] watcher_handle: &mut ElfWatchHandle,
) -> Result<Option<LocationsError>, YapLoadDefmtError> {
    let new_decoder = DefmtDecoder::from_elf_path(path);
    match new_decoder {
        Ok((new_decoder, locations_err_opt)) => {
            let decoder_arc = Arc::new(new_decoder);
            let _ = decoder_opt.insert(decoder_arc.clone());
            recent_elfs.elf_loaded(path)?;
            #[cfg(feature = "logging")]
            logging.update_defmt_decoder(Some(decoder_arc.clone()))?;
            #[cfg(feature = "defmt-watch")]
            watcher_handle.watch_path(path)?;

            Ok(locations_err_opt)
        }
        Err(e) => {
            error!("error loading defmt elf {e}");
            Err(e)?
        }
    }
}

pub fn repeating_pattern_widget(
    frame: &mut Frame,
    area: Rect,
    swap: bool,
    port_state: InnerPortStatus,
) {
    let repeat_count = area.width as usize / 2;
    let remainder = area.width as usize % 2;
    let base_pattern = if swap { "-~" } else { "~-" };

    let pattern = if remainder == 0 {
        base_pattern.repeat(repeat_count)
    } else {
        base_pattern.repeat(repeat_count) + &base_pattern[..1]
    };

    let pattern_widget = ratatui::widgets::Paragraph::new(pattern);
    let pattern_widget = match port_state {
        InnerPortStatus::Connected { .. } => pattern_widget.green(),
        #[cfg(feature = "espflash")]
        InnerPortStatus::LentOut { .. } => pattern_widget.yellow(),
        InnerPortStatus::PrematureDisconnect { .. } => pattern_widget.red(),
        InnerPortStatus::Idle { .. } => pattern_widget.red(),
    };
    frame.render_widget(pattern_widget, area);
}

/// If the given text is longer than the supplied area, it will be scrolled out of and then back in to the area.
pub fn render_scrolling_line<'a, T: Into<Line<'a>>>(
    text: T,
    frame: &mut Frame,
    mut area: Rect,
    scroll: &mut i32,
) {
    let orig_area = area;
    assert_eq!(area.height, 1, "Scrolling line expects a height of 1 only.");

    let line: Line = text.into();
    let total_width: usize = line.width();

    // let enough_room = total_width as u16 <= area.width;
    let overflow_amount = (total_width as u16).saturating_sub(area.width);

    let (scroll_x, offset_x): (u16, u16) = {
        if total_width as u16 <= area.width {
            (0, 0)
        } else if overflow_amount < 10 {
            match scroll {
                _pause if *scroll <= 0 => (0, 0),
                to_left if *scroll <= overflow_amount as i32 => (*to_left as u16, 0),
                _left_pause if *scroll <= (overflow_amount as i32) + 3 => (overflow_amount, 0),
                to_right if *scroll <= (overflow_amount as i32) + 3 + (overflow_amount as i32) => (
                    (overflow_amount) - ((*to_right as u16) - ((overflow_amount) + 3)),
                    0,
                ),
                scroll_reset
                    if *scroll > (overflow_amount as i32) + 3 + (overflow_amount as i32) =>
                {
                    *scroll_reset = -2;
                    (0, 0)
                }
                _ => (0, 0),
            }
        } else {
            match scroll {
                _pause if *scroll <= 0 => (0, 0),
                to_left if *scroll <= total_width as i32 => (*to_left as u16, 0),
                from_right if *scroll < (total_width as u16 + area.width) as i32 => {
                    (0, (*from_right as u16 - total_width as u16))
                }
                scroll_reset if *scroll >= (total_width as u16 + area.width) as i32 => {
                    *scroll_reset = -2;
                    (0, 0)
                }
                _ => (0, 0),
            }
        }
    };
    // debug!("scroll_x: {scroll_x}, offset_x: {offset_x}");
    let para = Paragraph::new(line).scroll((0, if offset_x > 0 { 0 } else { scroll_x }));
    if offset_x > 0 {
        area.width = u16::min(offset_x, area.width);
    }
    frame.render_widget(
        para,
        area.offset(Offset {
            x: if offset_x > 0 {
                orig_area.width.saturating_sub(offset_x).into()
            } else {
                0
            },
            y: 0,
        }),
    );
}

#[cfg(feature = "defmt")]
fn create_file_explorer() -> Result<FileExplorer, std::io::Error> {
    use ratatui_explorer::FileExplorer;

    let explorer_theme = ratatui_explorer::Theme::default()
        .with_scroll_padding(1)
        .add_default_title();

    // let root_path = std::path::PathBuf::from("");

    // let base_dirs_opt = directories::BaseDirs::new();

    // let starting_dir = base_dirs_opt
    //     .as_ref()
    //     .map(|base_dirs| base_dirs.home_dir())
    //     .unwrap_or(&root_path);

    FileExplorer::with_theme(explorer_theme)
    // .and_then(|mut e| e.set_cwd(starting_dir).map(|_| e))
}
