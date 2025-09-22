// This status struct leaves a bit to be desired
// especially in terms of the signals and their initial states
// (between connections and app start)
// maybe something better will come to me.

use std::time::Instant;

use serialport::{SerialPort, SerialPortInfo};

use crate::serial::SerialSignals;
use crate::settings::PortSettings;

#[cfg(feature = "espflash")]
use crate::serial::worker::NativePort;

#[derive(Debug, Clone, Default)]
/// Port status, shared with the main+UI thread.
pub struct PortStatus {
    /// The actual state of the port.
    inner: InnerPortStatus,

    /// Some: contains currently/last-connected port.
    ///
    /// Also used as the "desired device" to reconnect to if the port disconnects unexpectedly.
    ///
    /// None: Idle, not yet connected or disconnected intentionally.
    current_port: Option<SerialPortInfo>,

    /// The current state of the auxillary RS232 signals.
    pub(super) signals: SerialSignals,
}

impl PortStatus {
    pub fn new_idle(settings: &PortSettings) -> Self {
        Self {
            signals: SerialSignals {
                dtr: settings.dtr_on_connect,
                rts: settings.rts_on_connect,
                ..Default::default()
            },
            ..Default::default()
        }
    }
    /// Used when a port disconnects without the user's stated intent to do so.
    pub fn into_unhealthy(self) -> Self {
        Self {
            inner: InnerPortStatus::PrematureDisconnect,

            ..self
        }
    }
    /// Used when the user chooses to disconnect from the serial port
    pub fn into_idle(self, settings: &PortSettings) -> Self {
        Self {
            inner: InnerPortStatus::Idle,
            current_port: None,
            signals: SerialSignals {
                dtr: settings.dtr_on_connect,
                rts: settings.rts_on_connect,
                ..Default::default()
            },
        }
    }
    /// Used when giving espflash _ownership_ of the serialport object temporarily.
    #[cfg(feature = "espflash")]
    pub fn into_lent_out(self, lent_out_at: Instant) -> Self {
        let InnerPortStatus::Connected { connected_at } = self.inner else {
            panic!("can't lend a non-connected port")
        };

        Self {
            inner: InnerPortStatus::LentOut {
                initial_connection_at: connected_at,
                lent_out_at,
            },
            ..self
        }
    }
    /// Used when an espflash session finished gracefully enough for us to get the port back without reconnecting
    #[cfg(feature = "espflash")]
    pub fn returning_lent_port(
        self,
        port: &mut NativePort,
        settings: &PortSettings,
    ) -> Result<Self, serialport::Error> {
        use crate::serial::SerialSignals;

        let InnerPortStatus::LentOut {
            initial_connection_at,
            ..
        } = self.inner
        else {
            panic!("can't return a non-lent port")
        };

        port.write_data_terminal_ready(self.signals.dtr)?;
        port.write_request_to_send(self.signals.rts)?;

        let status = Self {
            inner: InnerPortStatus::Connected {
                connected_at: initial_connection_at,
            },
            signals: SerialSignals {
                dtr: settings.dtr_on_connect,
                rts: settings.rts_on_connect,
                ..Default::default()
            },
            ..self
        };

        Ok(status)
    }
    pub fn into_connected(
        self,
        port: &mut dyn SerialPort,
        port_info: SerialPortInfo,
        connected_at: Instant,
        settings: &PortSettings,
    ) -> Result<Self, serialport::Error> {
        let mut signals = self.signals;

        signals.update_slave_signals(port)?;

        let status = Self {
            current_port: Some(port_info),
            inner: InnerPortStatus::Connected { connected_at },
            signals: SerialSignals {
                rts: settings.dtr_on_connect,
                dtr: settings.dtr_on_connect,
                ..signals
            },
        };

        Ok(status)
    }
    pub fn current_port(&self) -> Option<&SerialPortInfo> {
        self.current_port.as_ref()
    }
    pub fn status(&self) -> InnerPortStatus {
        self.inner
    }
    pub fn signals(&self) -> &SerialSignals {
        &self.signals
    }
}

#[derive(Debug, Clone, Copy, Default, strum::EnumIs)]
pub enum InnerPortStatus {
    #[default]
    /// No connection has been made.
    Idle,
    /// Port was lost unexpectedly.
    PrematureDisconnect,
    #[cfg(feature = "espflash")]
    /// Port is temporarily owned by espflash.
    LentOut {
        initial_connection_at: Instant,
        lent_out_at: Instant,
    },
    /// Port is owned by us and we can read/write to it.
    Connected { connected_at: Instant },
}

#[cfg(not(feature = "espflash"))]
impl InnerPortStatus {
    pub fn is_lent_out(&self) -> bool {
        false
    }
}
