use std::str::FromStr;

use serde_untagged::UntaggedEnumVisitor;
use serialport::{SerialPort, SerialPortInfo, SerialPortType};

use crate::app::Event;

mod ignorable;
pub use ignorable::*;

pub mod handle;
pub mod port_status;
pub mod worker;

#[cfg(feature = "espflash")]
pub mod esp;
#[cfg(feature = "espflash")]
use esp::EspEvent;

#[derive(Debug, Clone)]
/// Describes the type of Reconnection that has occurred.
pub enum ReconnectType {
    /// A device was found matching all known characteristics of the last port (Port Name + Port Type).
    PerfectMatch,
    /// A USB device was found matching _all_ the last known device's USB Characteristics (PID, VID, Serial, and more).
    UsbStrict,
    /// A USB device was found matching just the last device's USB PID and VID.
    UsbLoose,
    /// We found a port with a matching name to the last one. That's it.
    LastDitch,
}

#[derive(Debug, Clone)]
pub enum SerialEvent {
    /// All found serial ports. Ignored devices are filtered out by the worker already.
    PortScan(Vec<SerialPortInfo>),
    /// Successful port connection. Option indicates if a reconnect from a premature disconnect occurred.
    Connected(Option<ReconnectType>),
    /// The worker was not able to send the given buffer to the port (due to no connection/error during sending), and has been returned in whole.
    UnsentTx(Vec<u8>),
    #[cfg(feature = "espflash")]
    EspFlash(EspEvent),
    Disconnected(SerialDisconnectReason),
}

#[derive(Debug, Clone)]
pub enum SerialDisconnectReason {
    /// User has chosen to return to port selection.
    Intentional,
    /// User has chosen to break serial connection but remain in the terminal view.
    UserBrokeConnection,
    /// An error has occurred.
    Error(String),
}

impl From<SerialDisconnectReason> for Event {
    fn from(value: SerialDisconnectReason) -> Self {
        SerialEvent::Disconnected(value).into()
    }
}

impl From<SerialEvent> for Event {
    fn from(value: SerialEvent) -> Self {
        Self::Serial(value)
    }
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    strum::Display,
    strum::VariantArray,
)]
#[strum(serialize_all = "title_case")]
/// Allowance level of Auto-Reconnections
pub enum ReconnectionStrictness {
    /// No auto reconnections
    Disabled,

    /// Also known as "Strict"
    /// Will only reconnect to devices that either:
    /// 1. Match the last device exactly (Port Name + Port Type) -> PerfectMatch
    /// 2. Match the USB characteristics exactly (PID, VID, Serial, and the rest) -> UsbStrict
    #[serde(alias = "StrictChecks")]
    High,

    /// Also known as "Loose"
    /// Will first try the Strict Checks and if those fail, will try to connect to devices that:
    /// 3. Match the USB PID and VID of the last device -> UsbLoose
    Med,

    /// Also known as "Best-Effort"
    /// Will first try the Strict+Loose Checks and if those fail, will try to connect to devices that:
    /// 4. Any port at the same path of the last device -> LastDitch
    #[serde(alias = "LooseChecks")]
    Low,
}

impl ReconnectionStrictness {
    pub fn allowed(&self) -> bool {
        !matches!(self, ReconnectionStrictness::Disabled)
    }
}

pub trait PrintablePortInfo {
    fn info_as_string(&self, baud_rate: Option<u32>) -> String;
}

impl PrintablePortInfo for SerialPortInfo {
    fn info_as_string(&self, baud_rate: Option<u32>) -> String {
        let extra_info = match &self.port_type {
            SerialPortType::UsbPort(usb) => {
                format!("VID: 0x{:04X}, PID: 0x{:04X}", usb.vid, usb.pid)
            }
            SerialPortType::Unknown => String::new(),
            SerialPortType::PciPort => "PCI".to_owned(),
            SerialPortType::BluetoothPort => "Bluetooth".to_owned(),
        };
        let port = &self.port_name;

        match (baud_rate, extra_info.is_empty()) {
            (Some(baud), false) => format!("{port} @ {baud} | {extra_info}"),
            (Some(baud), true) => format!("{port} @ {baud}"),
            (None, false) => format!("{port} | {extra_info}"),
            (None, true) => port.to_owned(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SerialSignals {
    // Host-controlled
    /// RTS (Request To Send)
    pub rts: bool, // TODO maybe make an option if leaving entirely untouched (for unknown state)?
    /// DTR (Data Terminal Ready)
    pub dtr: bool,
    // Slave-controlled, polled periodically
    /// CTS (Clear To Send)
    pub cts: bool,
    /// DSR (Data Set Ready)
    pub dsr: bool,
    /// RI (Ring Indicator)
    pub ri: bool,
    /// CD (Carrier Detect)
    pub cd: bool,
}

impl SerialSignals {
    fn update_slave_signals(
        &mut self,
        slave: &mut dyn SerialPort,
    ) -> Result<bool, serialport::Error> {
        let cts = slave.read_clear_to_send()?;
        let dsr = slave.read_data_set_ready()?;
        let ri = slave.read_ring_indicator()?;
        let cd = slave.read_carrier_detect()?;

        let changed = self.cts != cts || self.dsr != dsr || self.ri != ri || self.cd != cd;

        self.cts = cts;
        self.dsr = dsr;
        self.ri = ri;
        self.cd = cd;

        Ok(changed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::Display, strum::EnumString)]
pub enum SignalAssertion {
    /// On connect, assert the signal to the given level.
    #[strum(transparent)]
    Bool(bool),
    /// On connect, take no action to the signal.
    Untouched,
    /// Reconnects only, use the Connect's chosen behavior for Reconnects.
    InheritConnect,
}

impl serde::Serialize for SignalAssertion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            SignalAssertion::Bool(b) => serializer.serialize_bool(*b),
            _ => serializer.serialize_str(&self.to_string()),
        }
    }
}

impl<'de> serde::Deserialize<'de> for SignalAssertion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        UntaggedEnumVisitor::new()
            .string(|single| {
                SignalAssertion::from_str("InheritConnect").map_err(|_| {
                    serde::de::Error::unknown_variant(
                        single,
                        &["true", "false", "Untouched", "InheritConnect"],
                    )
                })
            })
            .bool(|b| Ok(SignalAssertion::Bool(b)))
            .deserialize(deserializer)
    }
}

impl From<bool> for SignalAssertion {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<SignalAssertion> for bool {
    fn from(value: SignalAssertion) -> Self {
        match value {
            SignalAssertion::Bool(b) => b,
            _ => false,
        }
    }
}
