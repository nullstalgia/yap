use std::{
    io::Write,
    sync::Arc,
    time::{Duration, Instant},
};

use arc_swap::ArcSwap;
use chrono::{DateTime, Local};
use crossbeam::channel::{Receiver, Sender};
use serialport::{SerialPort, SerialPortInfo, SerialPortType};
use tracing::{debug, error, info, warn};
use virtual_serialport::VirtualPort;

use crate::{
    app::{Event, Tick},
    serial::{SerialDisconnectReason, SerialEvent, port_status::PortStatus},
    settings::{Ignored, PortSettings},
    traits::ToggleBool,
};

use super::{
    ReconnectType, Reconnections,
    handle::{PortCommand, SerialWorkerCommand},
};

#[cfg(feature = "espflash")]
use super::esp::{EspCommand, EspEvent};

#[cfg(unix)]
pub type NativePort = serialport::TTYPort;
#[cfg(windows)]
pub type NativePort = serialport::COMPort;

#[derive(Default, strum::EnumIs)]
enum TakeablePort {
    #[default]
    /// No port is owned, either:
    /// - No connection has been made.
    /// - Connection failure has occurred.
    /// - Active connection has been broken.
    None,
    #[cfg(feature = "espflash")]
    /// Port object's ownership has been given to espflash,
    /// and we're awaiting it to be returned.
    /// If an error occurs and the port can not be recovered,
    /// it is considered broken.
    Borrowed,
    /// Port is owned by us and we can read/write to it.
    Native(NativePort),
    /// Mock/Loopback port is active, any bytes sent in will be echo'd back.
    Loopback(VirtualPort),
}

impl TakeablePort {
    /// Checks if port is owned/lent out.
    fn is_some(&self) -> bool {
        !self.is_none()
    }
    /// Checks if port is available to read/write to.
    fn is_available(&self) -> bool {
        self.is_native() || self.is_loopback()
    }
    /// Drop any held port, clearing any buffer contents on the way out.
    fn drop(&mut self) {
        if let Some(port) = self.as_mut_port() {
            debug!(
                "Input buffer len: {:?}, Output buffer len: {:?}",
                port.bytes_to_read(),
                port.bytes_to_write()
            );
            // This is needed mostly for *nix,
            // since phantom bytes can appear in `bytes_to_write`,
            // and cause the close() operation to block for ~30s.
            _ = port.clear(serialport::ClearBuffer::All);
            // Using port.flush() can also block for the ~30s period,
            // since close() and flush() both call `tcdrain` which give the connected device a
            // chance to drain the buffer before continuing.
            // Calling port.clear() calls `tcflush` instead, which just dumps the data instantly,
            // which will let close() run without waiting for a device that won't do anything.
        }
        *self = TakeablePort::None;
    }
    #[cfg(feature = "espflash")]
    fn take_native(&mut self) -> Option<NativePort> {
        if let TakeablePort::Native(_) = self {
            // TODO
            // This blocks me from impl-ing Drop to clear the buffer.
            if let TakeablePort::Native(port) = std::mem::replace(self, TakeablePort::Borrowed) {
                Some(port)
            } else {
                unreachable!()
            }
        } else {
            None
        }
    }
    fn return_native(&mut self, port: NativePort) {
        *self = TakeablePort::Native(port);
    }
    fn return_loopback(&mut self, port: VirtualPort) {
        *self = TakeablePort::Loopback(port);
    }
    fn as_mut_port(&mut self) -> Option<&mut dyn SerialPort> {
        match self {
            TakeablePort::Native(port) => Some(port),
            TakeablePort::Loopback(port) => Some(port),
            _ => None,
        }
    }
    // fn as_mut_native_port(&mut self) -> Option<&mut NativePort> {
    //     if let TakeablePort::Native(port) = self {
    //         Some(port)
    //     } else {
    //         None
    //     }
    // }
}

pub struct SerialWorker {
    command_rx: Receiver<SerialWorkerCommand>,
    event_tx: Sender<Event>,
    buffer_tx: Sender<(DateTime<Local>, Vec<u8>)>,
    port: TakeablePort,
    last_signal_check: Instant,
    scan_snapshot: Vec<SerialPortInfo>,
    rx_buffer: Vec<u8>,
    ignored_devices: Ignored,
    // These are held in ArcSwaps instead of just a "local" copy
    // to reduce the sources of truth I have to manage about the status of the current port.
    // When rendering, the main+UI thread reads from the ArcSwap so it always has the newest information.
    shared_status: Arc<ArcSwap<PortStatus>>,
    // I don't want to juggle baud separately, so I read Port Settings in the same way _to get the current baud_.
    // *Changing* these settings is still done with an actor message rather than the thread owning the Handle
    // just swapping these themselves, so that there's no ambiguity in the UI as to when the changes were recieved and handled.
    shared_settings: Arc<ArcSwap<PortSettings>>,
}

impl SerialWorker {
    pub fn new(
        command_rx: Receiver<SerialWorkerCommand>,
        event_tx: Sender<Event>,
        buffer_tx: Sender<(DateTime<Local>, Vec<u8>)>,
        port_status: Arc<ArcSwap<PortStatus>>,
        port_settings: Arc<ArcSwap<PortSettings>>,
        ignored_devices: Ignored,
    ) -> Self {
        Self {
            command_rx,
            event_tx,
            buffer_tx,
            shared_status: port_status,
            shared_settings: port_settings,
            port: TakeablePort::default(),
            last_signal_check: Instant::now(),
            scan_snapshot: vec![],
            rx_buffer: vec![0; 1024 * 1024],
            ignored_devices,
        }
    }
    // Primary loop for this thread.
    // Only use `?` on operations here that we expect to run flawlessly, and that
    // should kill the __whole app__ if encountered.
    pub fn work_loop(&mut self) -> Result<(), WorkerError> {
        loop {
            // I wonder if I can toy with the timeouts when writing
            // to see if I can tell if I'm hitting the S3's ring buffer limit
            let sleep_time = if self.port.is_some() {
                Duration::from_millis(10)
            } else {
                Duration::from_millis(100)
            };
            // TODO Fuzz testing with this + buffer
            // Checking for commands
            match self.command_rx.recv_timeout(sleep_time) {
                Ok(SerialWorkerCommand::Shutdown(shutdown_tx)) => {
                    debug!("Got shutdown request, dropping port!");
                    self.port.drop();

                    self.shared_status
                        .store(Arc::new(PortStatus::new_idle(&PortSettings::default())));

                    if shutdown_tx.send(()).is_err() {
                        error!("Failed to reply to shutdown request!");
                        break Err(WorkerError::ShutdownReply);
                    } else {
                        break Ok(());
                    }
                }
                Ok(SerialWorkerCommand::PortCommand(port_cmd)) => {
                    if let Err(e) = self.handle_port_command(port_cmd) {
                        error!("Port command error: {e}");
                        self.unhealthy_disconnection();
                        self.event_tx
                            .send(SerialDisconnectReason::Error(e.to_string()).into())?;
                    }
                }
                Ok(cmd) => self.handle_worker_command(cmd)?,

                // no message, just move on
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => (),
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                    error!("Serial worker handle got dropped! Shutting down!");
                    // Drop port explicity if it's present, since normal drop can hang.
                    self.port.drop();
                    break Err(WorkerError::HandleDropped);
                }
            }

            // Reading from port
            if let Some(port) = self.port.as_mut_port() {
                // info!(
                //     "bytes incoming: {}, bytes outcoming: {}",
                //     port.bytes_to_read().unwrap(),
                //     port.bytes_to_write().unwrap()
                // );
                match port.read(self.rx_buffer.as_mut_slice()) {
                    Ok(t) if t > 0 => {
                        let recieved_at = Local::now();
                        let cloned_buff = self.rx_buffer[..t].to_owned();
                        // info!("{:?}", &serial_buf[..t]);
                        self.buffer_tx.send((recieved_at, cloned_buff))?;
                        // if let Err(e) = self.buffer_tx.send(cloned_buff) {
                        //     self.port.drop();
                        //     Err(e)?;
                        // }
                    }
                    // 0-size read, ignoring
                    Ok(_) => (),

                    Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => (),
                    Err(e) => {
                        error!("Error when reading from port: {e}");
                        self.unhealthy_disconnection();
                        self.event_tx
                            .send(SerialDisconnectReason::Error(e.to_string()).into())?;
                    }
                }

                // Periodic signal check
                if self.last_signal_check.elapsed() >= Duration::from_millis(100) {
                    self.last_signal_check = Instant::now();
                    if let Err(e) = self.read_and_share_serial_signals(false) {
                        error!("Error when checking serial signals: {e}");
                        self.unhealthy_disconnection();
                        self.event_tx
                            .send(SerialDisconnectReason::Error(e.to_string()).into())?;
                    }
                }
            }
        }
    }

    fn unhealthy_disconnection(&mut self) {
        // This used to be an assertion but if the main+UI thread accidentally sends a command
        // when the port is missing (it's supposed to check for port health, but that introduces
        // time-of-check/time-of-use race conditions, so we'll just gracefully disconnect),
        // we shouldn't just panic.
        if !self.port.is_some() {
            error!("had a connection error with no port object held??");
        }

        self.port.drop();

        let last_status = self.shared_status.load().as_ref().clone();
        let known_port_ref = last_status
            .current_port()
            .expect("shouldn't have been connected to port without data saved");

        // Check if the port is still seen by the system
        if let Ok(mut latest_ports) = self.scan_for_serial_ports() {
            let current_port_index = latest_ports.iter().position(|s| s == known_port_ref);
            if let Some(idx) = current_port_index {
                // If so, remove it from the snapshot.
                latest_ports.remove(idx);
                self.scan_snapshot = latest_ports;
            } else {
                // If not, then we're (probably) safe to just use it as-is for
                // our "at-disconnect" snapshot to avoid already-connected
                // similar-looking USB devices triggering reconnections.
                self.scan_snapshot = latest_ports;
            }
        } else {
            error!("Failed to scan for ports right after a disconnection!")
        };

        let disconnected_status = {
            last_status
                // Ensure we keep around the old SerialPortInfo to use
                // as a reference for reconnections!
                .into_unhealthy()
        };
        self.shared_status.store(Arc::new(disconnected_status));
        // Disconnection Event TX should be done by caller.
    }

    // Errors returned should be treated as fatal Worker errors.
    fn handle_worker_command(&mut self, command: SerialWorkerCommand) -> Result<(), WorkerError> {
        match command {
            SerialWorkerCommand::ConnectBlocking {
                port,
                baud,
                mut settings,
                result_tx: oneshot_tx,
            } => {
                settings.baud_rate = baud.unwrap_or(settings.baud_rate);

                self.update_settings(settings)?;

                // If no port name was supplied, it was likely a USB PID:VID search request
                let port_info_res = if port.port_name.is_empty() {
                    let SerialPortInfo {
                        port_type: SerialPortType::UsbPort(usb_query),
                        ..
                    } = port
                    else {
                        unreachable!("Should only have USB Query info in here");
                    };

                    let current_ports = self.scan_for_serial_ports()?;

                    let check_serial_number = usb_query.serial_number.is_some();

                    // Try to find a connected device matching the requested characteristics.
                    current_ports
                        .iter()
                        .find(|p| match &p.port_type {
                            SerialPortType::UsbPort(usb) => {
                                if check_serial_number {
                                    usb.pid == usb_query.pid
                                        && usb.vid == usb_query.vid
                                        && usb.serial_number == usb_query.serial_number
                                } else {
                                    usb.pid == usb_query.pid && usb.vid == usb_query.vid
                                }
                            }
                            // Ignoring all non-usb devices
                            _ => false,
                        })
                        .cloned()
                        .ok_or(WorkerError::RequestedUsbMissing)
                } else {
                    // Otherwise we're expecting this to be the path to the port
                    Ok(port)
                };

                match port_info_res {
                    Ok(port_info) => {
                        oneshot_tx.send(
                            self.connect_to_port(&port_info, None)
                                .map_err(WorkerError::SerialPort),
                        )?;
                    }
                    Err(e) => {
                        oneshot_tx.send(Err(e))?;
                    }
                }
            }
            SerialWorkerCommand::Disconnect {
                user_wants_break: false,
            } => {
                // self.port_status = SerialStatus::idle();

                let settings = self.shared_settings.load();

                let previous_status = { self.shared_status.load().as_ref().clone() };

                self.shared_status
                    .store(Arc::new(previous_status.into_idle(&settings)));
                self.port.drop();
                self.event_tx
                    .send(SerialDisconnectReason::Intentional.into())?;
            }

            SerialWorkerCommand::Disconnect {
                user_wants_break: true,
            } if self.port.is_some() => {
                debug!("Worker got serial connection break request!");
                self.unhealthy_disconnection();

                self.event_tx
                    .send(SerialDisconnectReason::UserBrokeConnection.into())?;
            }
            SerialWorkerCommand::Disconnect {
                user_wants_break: true,
            } => warn!("No owned port connection to break!"),

            SerialWorkerCommand::RequestPortScan => {
                let ports = self.scan_for_serial_ports()?;
                self.scan_snapshot = ports.clone();
                self.event_tx.send(SerialEvent::PortScan(ports).into())?;
            }
            SerialWorkerCommand::RequestPortScanBlocking(sender) => {
                let ports = self.scan_for_serial_ports()?;
                self.scan_snapshot = ports.clone();
                sender.send(ports)?;
            }
            SerialWorkerCommand::RequestReconnect(strictness_opt) => {
                if let Err(e) = self.attempt_reconnect(strictness_opt) {
                    // TODO maybe show on UI?
                    // Maybe direct to notification history?
                    error!("Failed reconnect attempt: {e}");
                }
            }
            SerialWorkerCommand::NewIgnored(ignored) => self.ignored_devices = ignored,
            SerialWorkerCommand::Shutdown(_) => unreachable!("shutdown handled in work_loop"),
            SerialWorkerCommand::PortCommand(_) => {
                unreachable!("handled by other function in caller")
            }
        }
        Ok(())
    }
    // Errors returned should break existing port connection,
    // and begin reconnect attempts (if allowed)
    fn handle_port_command(&mut self, command: PortCommand) -> Result<(), WorkerError> {
        match command {
            PortCommand::PortSettings(settings) => self.update_settings(settings)?,
            #[cfg(feature = "espflash")]
            PortCommand::Esp(esp_command) => self.handle_esp_command(esp_command)?,

            PortCommand::WriteSignals { dtr, rts } => {
                assert!(dtr.is_some() || rts.is_some());
                let mut status: PortStatus = self.shared_status.load().as_ref().clone();
                if let Some(dtr) = dtr {
                    status.signals.dtr = dtr;
                }
                if let Some(rts) = rts {
                    status.signals.rts = rts;
                }
                if let Some(port) = self.port.as_mut_port() {
                    // Sending both signals regardless of which one changed
                    // to keep them in line with the expected state in the struct.
                    port.write_data_terminal_ready(status.signals.dtr)?;
                    port.write_request_to_send(status.signals.rts)?;
                }
                self.shared_status.store(Arc::new(status));
                self.read_and_share_serial_signals(true)?;
            }
            PortCommand::ToggleSignals { dtr, rts } => {
                assert!(dtr || rts);

                let mut status: PortStatus = self.shared_status.load().as_ref().clone();

                if dtr {
                    status.signals.dtr.flip();
                }
                if rts {
                    status.signals.rts.flip();
                }
                if let Some(port) = self.port.as_mut_port() {
                    // Sending both signals regardless of which one changed
                    // to keep them in line with the expected state in the struct.
                    port.write_data_terminal_ready(status.signals.dtr)?;
                    port.write_request_to_send(status.signals.rts)?;
                }
                self.shared_status.store(Arc::new(status));
                self.read_and_share_serial_signals(true)?;
            }
            // This should maybe reply with a success/fail in case the
            // port is having an issue, so the user's input buffer isn't consumed visually
            // TODO maybe hold user's input in memory until the Send Ok was recieved?
            // and return it if it fails?
            PortCommand::TxBuffer(data) if self.port.is_available() => {
                let port = self
                    .port
                    .as_mut_port()
                    .expect("was told port was available");

                let mut buf = &data[..];

                // Blech... This is because the ESP32-S3's virtual USB serial port
                // has an issue with payloads larger than 256 bytes????
                // (Sending too fast causes the buffer to fill up too quickly for the
                // actual firmware to notice anything present and drain it before it hits the cap)
                let slow_writes = self.shared_settings.load().limit_tx_speed;

                let max_bytes = 8;

                while !buf.is_empty() {
                    let write_size = if slow_writes {
                        std::cmp::min(max_bytes, buf.len())
                    } else {
                        buf.len()
                    };
                    match port.write(&buf[..write_size]) {
                        Ok(0) => {
                            debug!("Unexpected EOF on serial write!");
                            self.unhealthy_disconnection();
                            self.event_tx.send(
                                SerialDisconnectReason::Error("Unexpected EOF on write".into())
                                    .into(),
                            )?;
                            return Ok(());
                        }
                        Ok(n) => {
                            // info!("buf n: {n}");
                            buf = &buf[n..];
                            self.event_tx.send(Tick::Tx.into())?;
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(e) => {
                            self.unhealthy_disconnection();
                            self.event_tx
                                .send(SerialDisconnectReason::Error(e.to_string()).into())?;
                            return Ok(());
                        }
                    }
                }
            }
            // Tried to send with no port
            PortCommand::TxBuffer(unsent) => {
                let len = unsent.len();
                warn!("Got a buffer of {len} len that can't be sent! Returning buffer...");
                self.event_tx.send(SerialEvent::UnsentTx(unsent).into())?;
            }
        }
        Ok(())
    }
    fn update_settings(&mut self, settings: PortSettings) -> Result<(), serialport::Error> {
        let status = { self.shared_status.load().as_ref().clone() };
        if let Some(port) = self.port.as_mut_port() {
            port.set_baud_rate(settings.baud_rate)?;
            port.set_parity(settings.parity_bits)?;
            port.set_stop_bits(settings.stop_bits)?;
            port.set_data_bits(settings.data_bits)?;
            port.set_flow_control(settings.flow_control)?;

            port.write_data_terminal_ready(status.signals.dtr)?;
            port.write_request_to_send(status.signals.rts)?;
        } else {
            warn!("Received new port settings when no port connected!");
        }
        self.shared_status.store(Arc::new(status));
        self.shared_settings.store(Arc::new(settings));
        Ok(())
    }

    /// A return value of `Ok(())` only means no errors were encountered,
    /// not that reconnection was successful.
    fn attempt_reconnect(
        &mut self,
        strictness_opt: Option<Reconnections>,
    ) -> Result<(), WorkerError> {
        let port_available = self.port.is_available();
        #[cfg(feature = "espflash")]
        let port_available = port_available || self.port.is_borrowed();
        if port_available {
            error!("Got request to reconnect when already connected to port! Not acting...");
            return Ok(());
        }

        let reconnections =
            strictness_opt.unwrap_or_else(|| self.shared_settings.load().reconnections.clone());

        if reconnections == Reconnections::Disabled {
            warn!("Got request to reconnect when reconnections are disabled!");
            return Ok(());
        }

        // scan_for_serial_ports pre-filters out any user-ignored devices!
        let current_ports = self.scan_for_serial_ports()?;

        let port_guard = self.shared_status.load();
        let desired_port = port_guard
            .current_port()
            .expect("shouldn't have been connected to port without data saved");

        // Checking for a perfect match
        // TODO look into the USB Interface field for SerialPortInfo, see if we should keep it in mind
        // for the potential extra+less strict check
        if let Some(port) = current_ports.iter().find(|p| *p == desired_port) {
            info!("Perfect match found! Reconnecting to: {}", port.port_name);
            // Sleeping to give the device some time to intialize with Windows
            // (Otherwise Access Denied errors can occur from trying to connect too quick)
            std::thread::sleep(Duration::from_secs(1));
            self.connect_to_port(port, Some(ReconnectType::PerfectMatch))?;
            return Ok(());
        };

        // Implicitly, reaching this means either Strict or Loose reconnections is enabled,
        // so let's start with the strict check unconditionally.

        // Fuzzy USB searches
        if let SerialPortType::UsbPort(desired_usb) = &desired_port.port_type {
            // Strict check, trying to find a *new* port that has all the same USB characteristics
            if let Some(port) = current_ports
                .iter()
                // Only searching for USB Serial port devices
                .filter(|p| matches!(p.port_type, SerialPortType::UsbPort(_)))
                // Filtering out ports that didn't change across scans
                // (so we don't connect to an identical device that was already present and not being used)
                .filter(|p| !self.scan_snapshot.contains(p))
                .find(|p| p.port_type == desired_port.port_type)
            {
                info!(
                    "[STRICT] Connecting to similar USB device with port: {}",
                    port.port_name
                );
                std::thread::sleep(Duration::from_secs(1));
                self.connect_to_port(port, Some(ReconnectType::UsbStrict))?;
                return Ok(());
            };

            // Loose check
            if reconnections == Reconnections::LooseChecks
                && let Some(port) = current_ports
                    .iter()
                    // Filtering out ports that didn't change across scans
                    .filter(|p| !self.scan_snapshot.contains(p))
                    // Trying to find another USB device with *just* matching USB PID & VID
                    // Use cases: Some devices seem to change their Serial # arbitrarily?
                    //          - And for interfacing with several identical devices (one at a time) without reconnecting via TUI
                    // Needs a toggle with Strict/Loose options, as the extra behavior isn't always desirable.
                    .find(|p| match &p.port_type {
                        SerialPortType::UsbPort(usb) => {
                            usb.vid == desired_usb.vid && usb.pid == desired_usb.pid
                        }
                        _ => false,
                    })
            {
                info!(
                    "[NON-STRICT] Connecting to similar USB device with port: {}",
                    port.port_name
                );
                std::thread::sleep(Duration::from_secs(1));
                self.connect_to_port(port, Some(ReconnectType::UsbLoose))?;
                return Ok(());
            };
        }

        // Loose check
        if reconnections == Reconnections::LooseChecks
            // Last ditch effort, just try to connect to the same port_name if it's present.
            && let Some(port) = current_ports
                .iter()
                .find(|p| *p.port_name == desired_port.port_name)
        {
            info!("Last ditch connect attempt on: {}", port.port_name);
            std::thread::sleep(Duration::from_secs(1));
            self.connect_to_port(port, Some(ReconnectType::LastDitch))?;
            return Ok(());
        }

        // {}

        Ok(())
    }

    fn scan_for_serial_ports(&self) -> Result<Vec<SerialPortInfo>, serialport::Error> {
        let mut ports = serialport::available_ports()?;

        // ports
        //     .iter()
        //     .map(|p| match &p.port_type {
        //         SerialPortType::UsbPort(usb) => info!("{usb:#?}"),
        //         _ => (),
        //     })
        //     .count();

        ports.retain(|p| match &p.port_type {
            _ if self.ignored_devices.name.contains(&p.port_name) => false,

            #[cfg(unix)]
            _ if !self.ignored_devices.show_ttys_ports && p.port_name.starts_with("/dev/ttyS") => {
                false
            }

            SerialPortType::UsbPort(usb) => !self.ignored_devices.usb.iter().any(|ig| ig == usb),
            _ => true,
        });

        if !self.ignored_devices.hide_loopback_port {
            ports.push(SerialPortInfo {
                port_name: MOCK_PORT_NAME.to_owned(),
                port_type: SerialPortType::Unknown,
            });
        }

        // info!("Serial port scanning found {} ports", ports.len());
        Ok(ports)
    }
    fn connect_to_port(
        &mut self,
        port_info: &SerialPortInfo,
        reconnect_type: Option<ReconnectType>,
    ) -> Result<(), serialport::Error> {
        let mut port_status: PortStatus = self.shared_status.load().as_ref().clone();
        // If this is a normal connection, then this should be set to settings.dtr_on_open
        // otherwise, if we're reconnecting, then this should match the state of DTR at the time of disconnection
        let dtr_on_open = port_status.signals.dtr;
        let settings = self.shared_settings.load();
        let baud_rate = settings.baud_rate;

        if port_info.port_name.eq(MOCK_PORT_NAME) {
            let mut virt_port =
                virtual_serialport::VirtualPort::loopback(baud_rate, MOCK_DATA.len() as u32)?;
            virt_port.write_all(MOCK_DATA)?;

            self.port.return_loopback(virt_port);
        } else {
            let port = serialport::new(&port_info.port_name, baud_rate)
                .data_bits(settings.data_bits)
                .flow_control(settings.flow_control)
                .parity(settings.parity_bits)
                .stop_bits(settings.stop_bits)
                .dtr_on_open(dtr_on_open)
                .open_native()?;

            self.port.return_native(port);
        };

        let port = self
            .port
            .as_mut_port()
            .expect("port just populated, should be present");
        port.set_timeout(Duration::from_millis(100))?;
        port.write_request_to_send(port_status.signals.rts)?;

        port_status = port_status.into_connected(
            port,
            port_info.clone(),
            Instant::now(),
            settings.as_ref(),
        )?;

        self.shared_status.store(Arc::new(port_status));

        info!(
            "Serial worker connected to: {} @ {baud_rate} baud",
            port_info.port_name
        );

        if self
            .event_tx
            .send(SerialEvent::Connected(reconnect_type).into())
            .is_err()
        {
            let text = "App handle closed after successful port connection!";
            error!("{text}");
            panic!("{text}");
        } else {
            Ok(())
        }
    }

    fn read_and_share_serial_signals(&mut self, force_share: bool) -> Result<(), WorkerError> {
        let mut port_status: PortStatus = self.shared_status.load().as_ref().clone();

        match self.port.as_mut_port() {
            // If no port is present, just skip.
            None => return Ok(()),
            Some(port) => {
                // debug_assert later?
                // assert!(self.baud_rate == port_status.baud_rate);

                let changed = port_status.signals.update_slave_signals(port)?;
                // Only update the shared status if there's actually a change
                if changed {
                    self.shared_status.store(Arc::new(port_status));
                }
                // But always send the UI update tick if a DTR/RTS change requested it
                if changed || force_share {
                    self.event_tx
                        .send(Tick::Requested("Serial Signals").into())?;
                }
            }
        }

        Ok(())
    }

    #[cfg(feature = "espflash")]
    // Returning an error from here means that we couldn't recover,
    // and the connection needs to be re-established.
    fn handle_esp_command(&mut self, esp_command: EspCommand) -> Result<(), WorkerError> {
        if !self.port.is_native() {
            error!("ESP Command given when we don't own native port!");
            return Err(WorkerError::MissingPort);
        }

        use fs_err as fs;
        use std::borrow::Cow;

        use compact_str::ToCompactString;
        use espflash::flasher::{FlashData, FlashSettings};
        use serialport::UsbPortInfo;

        use crate::{
            serial::esp::{EspRestartType, ProgressPropagator},
            tui::esp::EspProfile,
        };

        let mut status: PortStatus = self.shared_status.load().as_ref().clone();

        let usb_port_info = {
            match status.current_port() {
                None => unreachable!("esp command shouldn't be sent with no port"),
                Some(info) => match &info.port_type {
                    SerialPortType::UsbPort(e) => e.clone(),
                    _not_usb => UsbPortInfo {
                        vid: 0,
                        pid: 0,
                        serial_number: None,
                        manufacturer: None,
                        product: None,
                    },
                },
            }
        };

        status = status.into_lent_out(Instant::now());

        self.shared_status.store(Arc::new(status.clone()));

        self.event_tx.send(EspEvent::Connecting.into())?;

        let lent_port = self
            .port
            .take_native()
            .expect("worker should have native port ownership");

        let returned_port = match esp_command {
            EspCommand::DeviceInfo => {
                let mut flasher =
                    self.connect_esp_flasher(lent_port, usb_port_info, true, true, None)?;

                if let Ok(esp_info) = flasher.device_info() {
                    debug!("{esp_info:#?}");

                    self.event_tx.send(EspEvent::DeviceInfo(esp_info).into())?;
                }

                flasher.connection().reset()?;

                flasher.into_connection().into_serial()
            }

            EspCommand::Restart(restart_type) => match restart_type {
                EspRestartType::Bootloader { active: true } => {
                    let flasher =
                        self.connect_esp_flasher(lent_port, usb_port_info, true, true, None)?;

                    self.event_tx.send(
                        EspEvent::BootloaderSuccess {
                            chip: flasher.chip().to_compact_string().to_uppercase(),
                        }
                        .into(),
                    )?;

                    flasher.into_connection().into_serial()
                }
                EspRestartType::Bootloader { active: false } => {
                    let mut connection = espflash::connection::Connection::new(
                        lent_port,
                        usb_port_info,
                        espflash::connection::ResetAfterOperation::HardReset,
                        espflash::connection::ResetBeforeOperation::DefaultReset,
                        115200,
                    );

                    connection.reset_to_flash(true)?;

                    self.event_tx.send(EspEvent::BootloaderAttempt.into())?;

                    connection.into_serial()
                }
                EspRestartType::UserCode => {
                    let mut connection = espflash::connection::Connection::new(
                        lent_port,
                        usb_port_info,
                        espflash::connection::ResetAfterOperation::HardReset,
                        espflash::connection::ResetBeforeOperation::DefaultReset,
                        115200,
                    );

                    connection.reset()?;

                    self.event_tx.send(EspEvent::HardResetAttempt.into())?;

                    connection.into_serial()
                }
            },
            EspCommand::FlashProfile(EspProfile::Bins(bins)) => {
                assert!(!bins.bins.is_empty(), "expected at least one bin to flash");
                let mut flasher = self.connect_esp_flasher(
                    lent_port,
                    usb_port_info,
                    !bins.no_verify,
                    !bins.no_skip,
                    bins.upload_baud,
                )?;

                let chip_matches_expected = bins.expected_chip.is_none_or(|expected| {
                    if expected == flasher.chip() {
                        true
                    } else {
                        warn!("Not acting! Chip doesn't match!");
                        false
                    }
                });

                if chip_matches_expected {
                    use espflash::image_format::Segment;
                    use itertools::Itertools;

                    if let Some(baud) = bins.upload_baud {
                        flasher.change_baud(baud)?;
                    }

                    let (rom_segs, mut errs): (Vec<Segment>, Vec<std::io::Error>) = bins
                        .bins
                        .iter()
                        .map(|(addr, path)| -> Result<Segment, std::io::Error> {
                            let bytes = fs::read(path)?;
                            Ok(Segment {
                                addr: *addr,
                                data: Cow::Owned(bytes),
                            })
                        })
                        .partition_result();

                    if let Some(err) = errs.pop() {
                        return Err(err)?;
                    }

                    let filenames: Vec<_> = bins
                        .bins
                        .iter()
                        .map(|(_addr, path)| path.file_name().unwrap_or_default())
                        .collect();
                    if filenames.iter().any(|n| n.is_empty()) {
                        return Err(WorkerError::FileMissingName);
                    }

                    let mut propagator = ProgressPropagator::new(
                        self.event_tx.clone(),
                        flasher.chip().to_compact_string().to_uppercase(),
                        filenames,
                    );

                    if let Err(e) = flasher.write_bins_to_flash(&rom_segs, &mut propagator) {
                        self.event_tx
                            .send(EspEvent::Error(format!("espflash error: {e}")).into())?;
                        error!("Error during binary flashing: {e}");
                    }
                } else {
                    self.event_tx.send(
                        EspEvent::Error(
                            "Not flashing! ESP variant doesn't match expected!".to_owned(),
                        )
                        .into(),
                    )?;
                }

                flasher.into_connection().into_serial()
            }
            EspCommand::FlashProfile(EspProfile::Elf(elf)) => {
                // assert!(!elf.path.is_empty(), "expected path");
                let mut flasher = self.connect_esp_flasher(
                    lent_port,
                    usb_port_info,
                    !elf.no_verify,
                    !elf.no_skip,
                    elf.upload_baud,
                )?;

                let chip_matches_expected = elf.expected_chip.is_none_or(|expected| {
                    if expected == flasher.chip() {
                        true
                    } else {
                        warn!("Not acting! Chip doesn't match!");
                        false
                    }
                });

                if chip_matches_expected {
                    if let Some(baud) = elf.upload_baud {
                        flasher.change_baud(baud)?;
                    }

                    let elf_data = fs::read(&elf.path)?;

                    let mut propagator = ProgressPropagator::new(
                        self.event_tx.clone(),
                        flasher.chip().to_compact_string().to_uppercase(),
                        vec![],
                    );

                    if elf.ram {
                        if let Err(e) = flasher.load_elf_to_ram(&elf_data, &mut propagator) {
                            self.event_tx
                                .send(EspEvent::Error(format!("espflash error: {e}")).into())?;
                            error!("Error during RAM load: {e}");
                        }
                    } else if let Ok(esp_info) = flasher.device_info() {
                        use espflash::image_format::idf::IdfBootloaderFormat;

                        // TODO? dunno tbh
                        let min_chip_rev = 0;
                        let mmu_page_size = None;
                        let flash_data = FlashData::new(
                            // Not sure how important it is I populate the other fields (or any at all?)
                            FlashSettings::new(None, Some(esp_info.flash_size), None),
                            min_chip_rev,
                            mmu_page_size,
                            esp_info.chip,
                            esp_info.crystal_frequency,
                        );

                        let format_res = IdfBootloaderFormat::new(
                            &elf_data,
                            &flash_data,
                            elf.partition_table.as_ref().map(AsRef::as_ref),
                            elf.bootloader.as_ref().map(AsRef::as_ref),
                            None,
                            None,
                        );

                        let format = match format_res {
                            Ok(f) => f,
                            Err(e) => {
                                return Err(WorkerError::ImageFormat(e));
                            }
                        };

                        if let Err(e) = flasher.load_image_to_flash(&mut propagator, format.into())
                        {
                            self.event_tx
                                .send(EspEvent::Error(format!("espflash error: {e}")).into())?;
                            error!("Error during ELF flashing: {e}");
                        }
                    }
                } else {
                    self.event_tx.send(
                        EspEvent::Error(
                            "Not flashing! ESP variant doesn't match expected!".to_owned(),
                        )
                        .into(),
                    )?;
                }

                flasher.into_connection().into_serial()
            }
            EspCommand::EraseFlash => {
                let mut flasher =
                    self.connect_esp_flasher(lent_port, usb_port_info, true, true, None)?;
                let esp_chip = flasher.chip();
                let chip = esp_chip.to_compact_string().to_uppercase();

                self.event_tx
                    .send(EspEvent::EraseStart { chip: chip.clone() }.into())?;

                if let Ok(()) = flasher.erase_flash() {
                    self.event_tx.send(EspEvent::EraseSuccess { chip }.into())?;
                }

                flasher.into_connection().into_serial()
            }
        };

        self.return_native_port(returned_port)?;

        Ok(())
    }

    #[cfg(feature = "espflash")]
    /// Used when an espflash operation has finished (successfully or otherwise),
    /// and we were able to regain ownership of the port object.
    fn return_native_port(&mut self, mut port: NativePort) -> Result<(), WorkerError> {
        let mut status = self.shared_status.load().as_ref().clone();

        // Re-applying our expected settings.
        // TODO consolidate with connect fn?
        port.set_timeout(Duration::from_millis(100))?;
        let settings = self.shared_settings.load();
        let baud_rate = settings.baud_rate;
        port.set_baud_rate(baud_rate)?;

        status = status.returning_lent_port(&mut port, settings.as_ref())?;

        self.port.return_native(port);

        self.shared_status.store(Arc::new(status));

        // Let the main+UI thread know to update
        // self.event_tx.send(SerialEvent::Connected(None).into())?;
        self.event_tx.send(EspEvent::PortReturned.into())?;

        Ok(())
    }
    #[cfg(feature = "espflash")]
    fn connect_esp_flasher(
        &self,
        lent_port: NativePort,
        usb_port_info: serialport::UsbPortInfo,
        verify: bool,
        skip: bool,
        upload_baud: Option<u32>,
    ) -> Result<espflash::flasher::Flasher, WorkerError> {
        use compact_str::ToCompactString;

        let connection = espflash::connection::Connection::new(
            lent_port,
            usb_port_info,
            espflash::connection::ResetAfterOperation::HardReset,
            espflash::connection::ResetBeforeOperation::DefaultReset,
            upload_baud.unwrap_or(115200),
        );

        let flasher =
            espflash::flasher::Flasher::connect(connection, true, verify, skip, None, upload_baud)?;

        let esp_chip = flasher.chip();
        let chip = esp_chip.to_compact_string().to_uppercase();

        self.event_tx.send(EspEvent::Connected { chip }.into())?;

        Ok(flasher)
    }
}

impl Drop for SerialWorker {
    fn drop(&mut self) {
        self.port.drop();
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum WorkerError {
    #[error("requested usb device could not be found")]
    RequestedUsbMissing,
    #[error("serial port error")]
    SerialPort(#[from] serialport::Error),
    #[error("no parent app receiver to send to")]
    FailedSend,
    #[error("failed to reply to shutdown request in time")]
    ShutdownReply,
    #[error("handle dropped, can't recieve commands")]
    HandleDropped,

    #[cfg(feature = "espflash")]
    #[error("espflash error: {0}")]
    EspFlash(#[from] espflash::Error),

    #[cfg(feature = "espflash")]
    #[error("file error: {0}")]
    File(#[from] std::io::Error),

    #[cfg(feature = "espflash")]
    #[error("given file has invalid name")]
    FileMissingName,

    #[cfg(feature = "espflash")]
    #[error("failed creating FlashData")]
    ImageFormat(#[source] espflash::Error),

    #[cfg(feature = "espflash")]
    #[error("tried to act on missing port")]
    MissingPort,
}

impl<T> From<crossbeam::channel::SendError<T>> for WorkerError {
    fn from(_: crossbeam::channel::SendError<T>) -> Self {
        Self::FailedSend
    }
}

pub const MOCK_PORT_NAME: &str = "lorem-ipsum";

const MOCK_DATA: &[u8] = b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0A\x0B\x0C\x0D\x0E\x0F\x10\x11\x12\x13\x14\x15\x16\x17\x18\x19\x1A\x1B\x1C\x1D\x1E\x1F\x20\x21\x22\x23\x24\x25\x26\x27\x28\x29\x2A\x2B\x2C\x2D\x2E\x2F\x30\x31\x32\x33\x34\x35\x36\x37\x38\x39\x3A\x3B\x3C\x3D\x3E\x3F\x40\x41\x42\x43\x44\x45\x46\x47\x48\x49\x4A\x4B\x4C\x4D\x4E\x4F\x50\x51\x52\x53\x54\x55\x56\x57\x58\x59\x5A\x5B\x5C\x5D\x5E\x5F\x60\x61\x62\x63\x64\x65\x66\x67\x68\x69\x6A\x6B\x6C\x6D\x6E\x6F\x70\x71\x72\x73\x74\x75\x76\x77\x78\x79\x7A\x7B\x7C\x7D\x7E\x7F\x80\x81\x82\x83\x84\x85\x86\x87\x88\x89\x8A\x8B\x8C\x8D\x8E\x8F\x90\x91\x92\x93\x94\x95\x96\x97\x98\x99\x9A\x9B\x9C\x9D\x9E\x9F\xA0\xA1\xA2\xA3\xA4\xA5\xA6\xA7\xA8\xA9\xAA\xAB\xAC\xAD\xAE\xAF\xB0\xB1\xB2\xB3\xB4\xB5\xB6\xB7\xB8\xB9\xBA\xBB\xBC\xBD\xBE\xBF\xC0\xC1\xC2\xC3\xC4\xC5\xC6\xC7\xC8\xC9\xCA\xCB\xCC\xCD\xCE\xCF\xD0\xD1\xD2\xD3\xD4\xD5\xD6\xD7\xD8\xD9\xDA\xDB\xDC\xDD\xDE\xDF\xE0\xE1\xE2\xE3\xE4\xE5\xE6\xE7\xE8\xE9\xEA\xEB\xEC\xED\xEE\xEF\xF0\xF1\xF2\xF3\xF4\xF5\xF6\xF7\xF8\xF9\xFA\xFB\xFC\xFD\xFE\xFF";

/*
const MOCK_DATA: &str = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. Duis porta volutpat magna non suscipit. Fusce rhoncus placerat metus, in posuere elit porta eget. Praesent ut nulla euismod, pulvinar tellus a, interdum ipsum. Integer in risus vulputate, finibus sem a, mattis ipsum. Aenean nec hendrerit tellus. Fusce risus dolor, sagittis non libero tristique, mattis vulputate libero. Proin ultrices luctus malesuada. Vestibulum non condimentum augue. Vestibulum ante ipsum primis in faucibus orci luctus et ultrices posuere cubilia curae; Vestibulum ultricies quis neque non pharetra. Nam fringilla nisl at tortor malesuada cursus. Nulla dictum, sem ac dignissim ullamcorper, est purus interdum tellus, at sagittis arcu risus suscipit neque. Mauris varius mauris vitae mi sollicitudin eleifend.

Donec feugiat, arcu sit amet ullamcorper consequat, nibh dolor laoreet risus, ut tincidunt tortor felis sed lacus. Aenean facilisis, mi nec feugiat rhoncus, dui urna malesuada erat, id mollis ipsum lectus ut ex. Curabitur semper vel tortor in finibus. Maecenas elit dui, cursus condimentum venenatis nec, cursus eget nisl. Proin consequat rhoncus tempor. Etiam dictum purus erat, sed aliquam mauris euismod vitae. Vivamus ut eros varius, posuere dolor eget, pretium tellus. Nam non lorem quis massa luctus hendrerit. Phasellus lobortis sodales quam in scelerisque. Morbi euismod et enim id dignissim. Sed commodo purus non est pellentesque euismod. Donec tincidunt dolor a ante aliquam auctor. Nam eget blandit felis.

Curabitur in tincidunt nunc. Phasellus in metus est. Nulla facilisi. Mauris dapibus augue non urna efficitur, eu ultrices est pellentesque. Nam semper vel nisi a pretium. Aenean malesuada sagittis mi, sit amet tempor mi. Donec at bibendum felis. Mauris a tortor luctus, tincidunt dui tristique, egestas turpis. Proin facilisis justo orci, vitae tristique nulla convallis eu. Cras bibendum non ante quis consectetur. Vivamus vestibulum accumsan felis, eu ornare arcu euismod semper. Aenean faucibus fringilla est, ut vulputate mi sodales id. Aenean ullamcorper enim ipsum, vitae sodales quam tincidunt condimentum. Vivamus aliquet elit sed consectetur mollis. Sed blandit lectus eget neque accumsan rutrum.

Fusce id tellus dictum, dignissim ante ac, fermentum dui. Sed eget auctor eros. Vivamus vel tristique urna. Nam ullamcorper sapien urna, vitae scelerisque eros facilisis et. Sed bibendum turpis id velit fermentum, eu mattis erat posuere. Vivamus ornare est sit amet felis condimentum condimentum. Ut id iaculis arcu. Mauris pharetra vestibulum est sit amet finibus. Sed at neque risus. Mauris nulla mauris, efficitur et iaculis et, tincidunt vitae libero. Nunc euismod nulla eget erat convallis blandit vitae id tortor. Pellentesque vitae magna a tortor scelerisque cursus laoreet nec erat. Praesent congue dui in turpis placerat, id ultricies orci varius.

Curabitur malesuada magna eu elit venenatis rhoncus. Nunc id elit eu nisi euismod dictum sit amet quis nulla. Cras hendrerit neque tellus, sed viverra ante tristique nec. Fusce sagittis porttitor purus, eu imperdiet sapien bibendum ac. Aliquam erat volutpat. Vestibulum vitae purus non dolor efficitur ullamcorper. Nunc velit mauris, accumsan eu porttitor quis, mattis eu augue. Nunc suscipit nec sapien nec feugiat. Ut elementum, ante at commodo consequat, ex enim venenatis mauris, tempus elementum lacus quam eu risus. Proin erat lorem, aliquam vitae vulputate sit amet, sagittis vitae dolor. Duis vel neque ligula. Cras semper ligula id viverra gravida. Nulla tempus nibh et tempor commodo. Sed bibendum sed quam commodo cursus. ";
*/
