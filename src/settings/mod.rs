use std::{
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use derivative::Derivative;
use fs_err::{self as fs};
use serde::{Deserialize, Serialize};
use serde_with::{NoneAsEmptyString, serde_as};
use serialport::{DataBits, FlowControl, Parity, StopBits};
use struct_table::StructTable;
use strum::VariantArray;

// Copied a lot from my other project, redefaulter
// https://github.com/nullstalgia/redefaulter/blob/ad81fad9468891b50daaac3215b0532386b6d1aa/src/settings/mod.rs

use crate::{
    app::{COMMON_BAUD_TRUNC, DEFAULT_BAUD},
    buffer::UserEcho,
    serial::{DeserializedUsb, ReconnectionStrictness, SignalAssertion},
};

pub mod ser;
use ser::*;

pub mod line_ending;
use line_ending::*;

#[cfg(debug_assertions)]
const DEFAULT_LOG_LEVEL: Level = Level::Trace;
#[cfg(not(debug_assertions))]
const DEFAULT_LOG_LEVEL: Level = Level::Debug;

#[cfg(debug_assertions)]
const DEFAULT_LOG_SOCKET_OPT: Option<SocketAddr> = {
    use std::net::{IpAddr, Ipv4Addr};

    let addr = Ipv4Addr::new(127, 0, 0, 1);
    let port = 7331;

    Some(SocketAddr::new(IpAddr::V4(addr), port))
};
#[cfg(not(debug_assertions))]
const DEFAULT_LOG_SOCKET_OPT: Option<SocketAddr> = None;

#[cfg(not(debug_assertions))]
const DEFAULT_HIDE_LOOPBACK: bool = true;
#[cfg(debug_assertions)]
const DEFAULT_HIDE_LOOPBACK: bool = false;

#[derive(Debug, Clone, Serialize, Deserialize, Derivative)]
#[serde(default)]
#[derivative(Default)]
pub struct Settings {
    pub serial: PortSettings,

    pub rendering: Rendering,

    pub behavior: Behavior,

    pub misc: Misc,
    #[cfg(feature = "espflash")]
    pub espflash: Espflash,
    #[cfg(feature = "defmt")]
    pub defmt: Defmt,
    #[cfg(feature = "logging")]
    pub logging: Logging,

    pub updates: Updates,

    pub ignored_devices: Ignored,

    #[serde(skip)]
    pub path: PathBuf,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, Derivative)]
#[serde(default)]
#[derivative(Default)]
pub struct Misc {
    #[derivative(Default(value = "DEFAULT_LOG_LEVEL"))]
    pub log_level: Level,

    #[derivative(Default(value = "DEFAULT_LOG_SOCKET_OPT"))]
    #[serde_as(as = "NoneAsEmptyString")]
    pub log_tcp_socket: Option<SocketAddr>,
}

// TODO allow setting nicknames to devices?????

// TODO add Reset to Defaults somewhere in the UI

// TODO have flattened buffer behavior struct that gets sent to it on each change.
macro_rules! inclusive_increment {
    ($len:expr) => {{
        const LEN: usize = $len + 1;
        let mut arr = [0; LEN];
        let mut i = 0;
        while i < LEN {
            arr[i] = i;
            i += 1;
        }
        arr
    }};
}

#[derive(Debug, Clone, Serialize, Deserialize, StructTable, Derivative)]
#[serde(default)]
#[derivative(Default)]
pub struct Rendering {
    #[derivative(Default(value = "UserEcho::All"))]
    #[table(values = UserEcho::VARIANTS)]
    /// Show user input in buffer after sending.
    pub echo_user_input: UserEcho,

    #[derivative(Default(value = "true"))]
    /// Show timestamps next to each incoming line.
    pub timestamps: bool,

    /// Show line's buffer index and length next to line.
    pub show_indices: bool,

    /// Whether indices for "Show Indices" should be in hex format.
    pub indices_as_hex: bool,

    #[derivative(Default(value = "true"))]
    /// Wrap text longer than the screen.
    pub wrap_text: bool,

    #[derivative(Default(value = "true"))]
    /// Show line ending at end of recieved lines.
    pub show_line_ending: bool,

    #[derivative(Default(value = "true"))]
    /// Show hidden chars and invalid UTF-8 byte sequences in \xFF notation.
    pub escape_unprintable_bytes: bool,

    #[derivative(Default(value = "true"))]
    /// Show a placeholder for lines who have had their entire content hidden by color rules.
    pub show_hidden_lines: bool,

    /// Show recieved bytes in a Hex+ASCII view.
    pub hex_view: bool,

    #[derivative(Default(value = "true"))]
    /// Show Address+Offset Markers+ASCII label above hex view.
    pub hex_view_header: bool,

    #[table(values = inclusive_increment!(48))]
    #[table(allow_unknown_values)]
    /// Set an optional maximum bytes per line.
    pub bytes_per_line: MaxBytesPerLine,

    #[derivative(Default(value = "HexHighlightStyle::HighlightAsciiSymbols"))]
    #[table(values = HexHighlightStyle::VARIANTS)]
    /// Show user input in buffer after sending.
    pub hex_view_highlights: HexHighlightStyle,

    #[derivative(Default(value = "true"))]
    /// Show how long the connection to the port has been active on the top-right.
    pub show_connection_time: bool,
}

// #[derive(
//     Debug, Clone, Copy, PartialEq, Serialize, Deserialize, strum::Display, strum::VariantArray,
// )]
// #[strum(serialize_all = "title_case")]
// pub enum ConnectionTime {
//     Hidden,
//     BelowText,
//     AboveText,
// }

#[derive(
    Debug, Clone, Copy, PartialEq, Serialize, Deserialize, strum::Display, strum::VariantArray,
)]
#[strum(serialize_all = "title_case")]
pub enum HexHighlightStyle {
    None,
    DarkenNulls,
    #[strum(serialize = "Highlight ASCII Symbols")]
    HighlightAsciiSymbols,
    // HighlightUnicode TODO
    // UseColorRules TODO
    // might need to do something wild like have a buffer with tagged-with-index spans
    StyleA,
    StyleB,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[repr(transparent)]
pub struct MaxBytesPerLine(u8);
impl PartialEq<u8> for MaxBytesPerLine {
    fn eq(&self, other: &u8) -> bool {
        &self.0 == other
    }
}

impl PartialEq<MaxBytesPerLine> for u8 {
    fn eq(&self, other: &MaxBytesPerLine) -> bool {
        self == &other.0
    }
}

impl PartialEq<usize> for MaxBytesPerLine {
    fn eq(&self, other: &usize) -> bool {
        usize::from(self.0) == *other
    }
}

impl PartialEq<MaxBytesPerLine> for usize {
    fn eq(&self, other: &MaxBytesPerLine) -> bool {
        *self == usize::from(other.0)
    }
}

impl From<u8> for MaxBytesPerLine {
    fn from(value: u8) -> Self {
        MaxBytesPerLine(value)
    }
}

impl From<usize> for MaxBytesPerLine {
    fn from(value: usize) -> Self {
        MaxBytesPerLine(value as u8)
    }
}

impl From<MaxBytesPerLine> for u8 {
    fn from(value: MaxBytesPerLine) -> Self {
        value.0
    }
}

impl From<MaxBytesPerLine> for usize {
    fn from(value: MaxBytesPerLine) -> Self {
        value.0 as usize
    }
}

impl From<MaxBytesPerLine> for Option<u8> {
    fn from(value: MaxBytesPerLine) -> Self {
        match value.0 {
            0 => None,
            x => Some(x),
        }
    }
}

impl std::fmt::Display for MaxBytesPerLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            0 => write!(f, "Fit to Screen"),
            x => write!(f, "{x}"),
        }
    }
}

#[cfg(feature = "logging")]
#[derive(Debug, Clone, Serialize, Deserialize, StructTable, Derivative)]
#[serde(default)]
#[derivative(Default)]
pub struct Logging {
    /// Whether to log the incoming input in a text file.
    pub log_text_to_file: bool,

    /// Whether to log the incoming input in a raw binary file.
    pub log_raw_input_to_file: bool,

    #[derivative(Default(value = "String::from(crate::buffer::DEFAULT_TIMESTAMP_FORMAT)"))]
    #[table(skip)]
    /// Format for output timestamps.
    pub timestamp: String,

    // Just always doing instead right now, no need for the option.
    // #[serde_inline_default_parent]
    // #[derivative(Default(value = "true"))]
    // /// Escape invalid UTF-8 byte sequences in \xFF notation in text outputs.
    // pub escape_unprintable_bytes: bool,
    #[derivative(Default(value = "true"))]
    /// Record user input in text outputs.
    pub log_user_input: bool,

    #[derivative(Default(value = "true"))]
    /// Log any disconnect and reconnect events in text outputs.
    pub log_connection_events: bool,
    // TODO maybe add option to strip ansi escapes for text output?
}

#[derive(Debug, Clone, Serialize, Deserialize, StructTable, Derivative)]
#[serde(default)]
#[derivative(Default)]
pub struct Behavior {
    // TODO find a better name for this.
    // Text Buffer or something?
    #[derivative(Default(value = "true"))]
    /// Use text box to type in before sending, with history. If disabled, sends keyboard inputs directly.
    pub pseudo_shell: bool,

    #[derivative(Default(value = "true"))]
    /// Interpret typed escape sequences such as \n or \xFF and send corresponding byte values in text inputs.
    pub unescape_typed_bytes: bool,

    /// Whether to always send the TX Line Ending when using Pseudo Shell's typed byte input mode.
    pub send_line_ending_with_bytes: bool,

    #[table(allow_unknown_values)]
    #[table(display = ["-3", "-2", "-1", "Default", "+1", "+2", "+3"])]
    #[table(values = [-3, -2, -1, 0, 1, 2, 3])]
    /// Text scroll speed modifier, positive increases, negative decreases.
    pub text_scroll_speed: i8,

    // #[serde(default)]
    // /// Persist Pseudo Shell's command history across sessions (TODO).
    // pub retain_history: bool,
    //
    #[derivative(Default(value = "Duration::from_millis(500)"))]
    #[table(allow_unknown_values)]
    #[table(display = Debug)]
    #[table(values = [Duration::from_millis(10), Duration::from_millis(100), Duration::from_millis(250), Duration::from_millis(500), Duration::from_secs(1)])]
    #[serde(rename = "action_chain_delay_ms")]
    #[serde(
        serialize_with = "serialize_duration_as_ms",
        deserialize_with = "deserialize_duration_from_ms"
    )]
    // https://docs.rs/serde_with/3.14.0/serde_with/struct.DurationSecondsWithFrac.html
    // just found this ^
    /// Default delay between chained Actions in keybinds. Can be overwritten with "pause_ms:XXX" in chains.
    pub action_chain_delay: Duration,

    #[cfg(feature = "macros")]
    #[derivative(Default(value = "true"))]
    /// Allow entering Macros in keybinds without a category.
    pub fuzzy_macro_match: bool,
}

#[cfg(feature = "defmt")]
#[derive(
    Debug,
    Default,
    Clone,
    PartialEq,
    Serialize,
    Deserialize,
    strum::VariantArray,
    strum::EnumString,
    strum::Display,
)]
/// Expected format of incoming bytes in relation to defmt support.
pub enum DefmtSupport {
    /// Expects defmt logs to be framed by `0xFF 0x00 ... 0x00`, any data outside of
    /// those frames is asssumed to be text.
    FramedRzcobs,
    /// Expects all input to be rzCOBS-compressed frames, delimited by 0x00.
    UnframedRzcobs,
    /// Expects all input to be uncompressed defmt frames, as-is.
    Raw,
    #[default]
    /// Defmt support is disabled, all input is assumed to be text.
    Disabled,
}

#[cfg(feature = "defmt")]
#[derive(
    Debug,
    Default,
    Clone,
    PartialEq,
    Serialize,
    Deserialize,
    strum::VariantArray,
    strum::EnumString,
    strum::Display,
    strum::EnumIs,
)]
pub enum DefmtLocation {
    Shortened,
    #[default]
    Full,
    Hidden,
}

#[derive(
    Debug,
    Default,
    Clone,
    PartialEq,
    PartialOrd,
    Serialize,
    Deserialize,
    strum::VariantArray,
    strum::EnumString,
    strum::Display,
)]
#[strum(serialize_all = "title_case")]
#[strum(ascii_case_insensitive)]
// #[serde(try_from = "String")]
pub enum Level {
    #[default]
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

// Might want to redo this but find some way to keep the
// "expected one of these variants: []"
// impl TryFrom<String> for Level {
//     type Error = strum::ParseError;
//     fn try_from(value: String) -> Result<Self, strum::ParseError> {
//         Self::try_from(value.as_str())
//     }
// }
impl From<&Level> for tracing::Level {
    fn from(level: &Level) -> Self {
        match level {
            Level::Trace => tracing::Level::TRACE,
            Level::Debug => tracing::Level::DEBUG,
            Level::Info => tracing::Level::INFO,
            Level::Warn => tracing::Level::WARN,
            Level::Error => tracing::Level::ERROR,
        }
    }
}
#[cfg(feature = "defmt")]
impl From<Level> for defmt_parser::Level {
    fn from(value: Level) -> Self {
        match value {
            Level::Trace => defmt_parser::Level::Trace,
            Level::Debug => defmt_parser::Level::Debug,
            Level::Info => defmt_parser::Level::Info,
            Level::Warn => defmt_parser::Level::Warn,
            Level::Error => defmt_parser::Level::Error,
        }
    }
}
#[cfg(feature = "defmt")]
impl From<defmt_parser::Level> for Level {
    fn from(value: defmt_parser::Level) -> Self {
        match value {
            defmt_parser::Level::Trace => Level::Trace,
            defmt_parser::Level::Debug => Level::Debug,
            defmt_parser::Level::Info => Level::Info,
            defmt_parser::Level::Warn => Level::Warn,
            defmt_parser::Level::Error => Level::Error,
        }
    }
}

#[cfg(feature = "espflash")]
#[derive(Debug, Clone, Serialize, Deserialize, Derivative)]
#[serde(default)]
#[derivative(Default)]
pub struct Espflash {
    /// Skip requirement for double-pressing Enter within a period of time
    /// when selecting Erase Flash on ESP32 Flashing menu.
    pub skip_erase_confirm: bool,
}

#[cfg(feature = "defmt")]
#[derive(Debug, Clone, Serialize, Deserialize, StructTable, Derivative)]
#[serde(default)]
#[derivative(Default)]
pub struct Defmt {
    #[table(values = DefmtSupport::VARIANTS)]
    /// Enable parsing RX'd serial data as defmt packets.
    pub defmt_parsing: DefmtSupport,

    #[cfg(feature = "defmt-watch")]
    #[table(rename = "Watch ELF for Changes")]
    /// Reload defmt data from ELF when file is updated.
    pub watch_elf_for_changes: bool,

    #[derivative(Default(value = "Level::Trace"))]
    #[table(display = Debug)]
    #[table(values = Level::VARIANTS)]
    /// Maximum log level to display. Items without a level are always shown.
    pub max_log_level: Level,

    #[derivative(Default(value = "true"))]
    /// Show device-derived timestamps, if available.
    pub device_timestamp: bool,

    #[table(values = DefmtLocation::VARIANTS)]
    /// Show module where log originated from, if available.
    pub show_module: DefmtLocation,

    #[table(values = DefmtLocation::VARIANTS)]
    /// Show file where log originated from, if available.
    pub show_file: DefmtLocation,

    #[derivative(Default(value = "true"))]
    /// Show line number in file where log originated from, if available.
    pub show_line_number: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, StructTable)]
#[serde(default)]
pub struct PortSettings {
    /// The baud rate in symbols-per-second.
    #[table(allow_unknown_values)]
    #[table(values = COMMON_BAUD_TRUNC)]
    pub baud_rate: u32,

    /// Number of bits per character.
    #[table(values = [DataBits::Five, DataBits::Six, DataBits::Seven, DataBits::Eight])]
    #[serde(
        serialize_with = "serialize_as_u8",
        deserialize_with = "deserialize_from_u8"
    )]
    pub data_bits: DataBits,

    /// Flow control modes.
    #[table(values = [FlowControl::None, FlowControl::Software, FlowControl::Hardware])]
    pub flow_control: FlowControl,

    /// Parity bit modes.
    #[table(values = [Parity::None, Parity::Odd, Parity::Even])]
    pub parity_bits: Parity,

    /// Number of stop bits.
    #[table(values = [StopBits::One, StopBits::Two])]
    #[serde(
        serialize_with = "serialize_as_u8",
        deserialize_with = "deserialize_from_u8"
    )]
    pub stop_bits: StopBits,

    /// Assert DTR to this state on port connect.
    #[table(rename = "DTR on Connect")]
    #[table(values = [SignalAssertion::Bool(true), SignalAssertion::Bool(false), SignalAssertion::Untouched])]
    pub dtr_on_connect: SignalAssertion,
    /// Assert RTS to this state on port connect.
    #[table(rename = "RTS on Connect")]
    #[table(values = [SignalAssertion::Bool(true), SignalAssertion::Bool(false), SignalAssertion::Untouched])]
    pub rts_on_connect: SignalAssertion,

    /// Assert DTR to this state on port reconnect.
    #[table(rename = "DTR on Reconnect")]
    #[table(values = [SignalAssertion::Bool(true), SignalAssertion::Bool(false), SignalAssertion::Untouched, SignalAssertion::InheritConnect])]
    pub dtr_on_reconnect: SignalAssertion,
    /// Assert RTS to this state on port reconnect.
    #[table(rename = "RTS on Reconnect")]
    #[table(values = [SignalAssertion::Bool(true), SignalAssertion::Bool(false), SignalAssertion::Untouched, SignalAssertion::InheritConnect])]
    pub rts_on_reconnect: SignalAssertion,

    /// Limit output to 8kbps, regardless of baud. Some devices will overwrite unread data if sent too fast.
    #[table(rename = "Limit TX Speed")]
    pub limit_tx_speed: bool,

    /// Enable reconnections with checks. High checks USB PID+VID+Serial#, Med checks USB PID+VID, and Low checks for *any* similar USB device or COM port.
    #[table(values = ReconnectionStrictness::VARIANTS)]
    pub reconnection_strictness: ReconnectionStrictness,

    /// Line endings for RX'd data.
    #[table(display = ["\\n", "\\r", "\\r\\n", "None"])]
    #[table(rename = "RX Line Ending")]
    #[table(values = [RxLineEnding::Preset("\\n", b"\n"), RxLineEnding::Preset("\\r", b"\r"), RxLineEnding::Preset("\\r\\n", b"\r\n"), RxLineEnding::Preset("", b"")])]
    #[table(allow_unknown_values)]
    #[serde(
        serialize_with = "serialize_as_string",
        deserialize_with = "deserialize_from_str"
    )]
    pub rx_line_ending: RxLineEnding,

    /// Line endings for TX'd data.
    #[table(display = ["Inherit RX", "\\n", "\\r", "\\r\\n", "None"])]
    #[table(rename = "TX Line Ending")]
    #[table(values = [TxLineEnding::InheritRx, TxLineEnding::Preset("\\n", b"\n"), TxLineEnding::Preset("\\r", b"\r"), TxLineEnding::Preset("\\r\\n", b"\r\n"), TxLineEnding::Preset("", b"")])]
    #[table(allow_unknown_values)]
    #[serde(
        serialize_with = "serialize_as_string",
        deserialize_with = "deserialize_from_str"
    )]
    pub tx_line_ending: TxLineEnding,

    #[cfg(feature = "macros")]
    /// Default line ending for sent macros.
    #[table(display = ["Inherit TX", "Inherit RX", "\\n", "\\r", "\\r\\n", "None"])]
    #[table(values = [MacroTxLineEnding::InheritTx, MacroTxLineEnding::InheritRx, MacroTxLineEnding::Preset("\\n", b"\n"), MacroTxLineEnding::Preset("\\r", b"\r"), MacroTxLineEnding::Preset("\\r\\n", b"\r\n"), MacroTxLineEnding::Preset("", b"")])]
    #[table(allow_unknown_values)]
    #[serde(
        serialize_with = "serialize_as_string",
        deserialize_with = "deserialize_from_str"
    )]
    pub macro_line_ending: MacroTxLineEnding,
}

impl Default for PortSettings {
    fn default() -> Self {
        Self {
            baud_rate: DEFAULT_BAUD,
            data_bits: DataBits::Eight,
            flow_control: FlowControl::None,
            parity_bits: Parity::None,
            stop_bits: StopBits::One,
            rx_line_ending: "\n".into(),
            tx_line_ending: TxLineEnding::InheritRx,
            #[cfg(feature = "macros")]
            macro_line_ending: MacroTxLineEnding::InheritTx,
            dtr_on_connect: SignalAssertion::Bool(true),
            rts_on_connect: SignalAssertion::Bool(true),
            dtr_on_reconnect: SignalAssertion::InheritConnect,
            rts_on_reconnect: SignalAssertion::InheritConnect,
            limit_tx_speed: true,
            reconnection_strictness: ReconnectionStrictness::Med,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, Derivative)]
#[serde(default)]
pub struct Updates {
    pub user_dismissed_prompt: bool,

    pub allow_checking_for_updates: bool,

    pub skipped_version: String,

    pub allow_pre_releases: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Derivative)]
/// Hide certain devices from the Port Selection screen.
///
/// Does not effect CLI USB entry.
#[serde(default)]
#[derivative(Default)]
pub struct Ignored {
    #[cfg(unix)]

    /// Show /dev/ttyS* ports.
    pub show_ttys_ports: bool,

    #[derivative(Default(value = "default_hidden_usb()"))]
    /// Devices in VID:PID[:SERIAL] format to not show in port selection.
    ///
    /// Entries without a Serial # act as a wildcard,
    /// entries containing a Serial # require an exact match.
    pub usb: Vec<DeserializedUsb>,

    /// Hide any serial ports matching these paths/names.
    pub name: Vec<String>,

    #[derivative(Default(value = "DEFAULT_HIDE_LOOPBACK"))]
    /// Hide the Lorem Ipsum/Loopback testing port.
    pub hide_loopback_port: bool,
}

fn default_hidden_usb() -> Vec<DeserializedUsb> {
    vec![
        // Valve Index/Bigscreen Beyond's Bluetooth COM Port (Watchman)
        DeserializedUsb {
            vid: 0x28DE,
            pid: 0x2102,
            serial_number: None,
        },
    ]
}

impl Settings {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, SettingsError> {
        let path = path.as_ref();
        if !path.exists() {
            let default = Settings {
                path: path.into(),
                ..Default::default()
            };
            default.save()?;
            return Ok(default);
        }
        let settings_toml = fs::read_to_string(path).map_err(SettingsError::FileRead)?;
        let mut config: Settings = toml::from_str(&settings_toml)?;
        config.path = path.into();
        config.save()?;
        Ok(config)
    }
    pub fn save(&self) -> Result<(), SettingsError> {
        assert_ne!(self.path.components().count(), 0);
        self.save_at(&self.path)?;
        Ok(())
    }
    // TODO write all enum variants next to each field?
    fn save_at(&self, config_path: &Path) -> Result<(), SettingsError> {
        let toml_config = toml::to_string(self)?;
        fs::File::create(config_path)
            .and_then(|mut file| {
                file.write_all(toml_config.as_bytes())?;
                file.flush()?;
                file.sync_all()
            })
            .map_err(SettingsError::FileWrite)?;

        Ok(())
    }
    pub fn get_log_level(&self) -> tracing::Level {
        tracing::Level::from(&self.misc.log_level)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("failed reading from app settings file")]
    FileRead(#[source] std::io::Error),
    #[error("failed saving to app settings file")]
    FileWrite(#[source] std::io::Error),
    #[error("invalid app settings")]
    Deser(#[from] toml::de::Error),
    #[error("failed settings serialization")]
    Ser(#[from] toml::ser::Error),
}
