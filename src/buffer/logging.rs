use std::{
    borrow::Cow,
    io::{Seek, Write},
    thread::JoinHandle,
    time::Duration,
};

#[cfg(feature = "defmt")]
use std::sync::Arc;

use chrono::{DateTime, Local};
use crossbeam::channel::{Receiver, RecvError, SendError, Sender};
use fs_err as fs;
use serialport::SerialPortInfo;
use tracing::{debug, error, warn};

#[cfg(feature = "defmt")]
use crate::{changed, settings::Defmt};

use crate::{
    app::Event, config_adjacent_path, serial::ReconnectType, settings::Logging,
    traits::ByteSuffixCheck,
};

use super::{LineEnding, line_ending_iter};

#[cfg_attr(test, derive(Clone))]
pub struct LoggingHandle {
    command_tx: Sender<LoggingCommand>,
}

pub const DEFAULT_TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.9f";

pub struct TxPayload {
    pub timestamp: DateTime<Local>,
    pub bytes: Vec<u8>,
    pub line_ending: Vec<u8>,
}

pub enum LoggingCommand {
    PortConnected(DateTime<Local>, SerialPortInfo, Option<ReconnectType>),
    PortDisconnect {
        timestamp: DateTime<Local>,
        back_to_port_selection: bool,
    },
    BeginRelogging(Receiver<SyncBatch>),
    RxBytes(DateTime<Local>, Vec<u8>),
    TxBytes(TxPayload),
    LineEndingChange(LineEnding),
    Settings(Logging),
    #[cfg(feature = "defmt")]
    DefmtSettings(Defmt),
    #[cfg(feature = "defmt")]
    DefmtDecoder(Option<Arc<super::defmt::DefmtDecoder>>),
    Shutdown(Sender<()>),
}

// When user requests to re-sync all buffer contents to file,
// we batch together consecutive RX/TX entries
// (just to minimize the amount of chatter between threads)
pub enum SyncBatch {
    RxBatch(Vec<(DateTime<Local>, Vec<u8>)>),
    TxBatch(Vec<TxPayload>),
    Done,
}

enum LineType {
    /// Recieved from port
    Rx,
    /// Sent by user
    Tx { line_ending: Vec<u8> },
    /// Decoded from port
    #[cfg(feature = "defmt")]
    DefmtRx,
}

#[derive(Debug)]
pub enum LoggingEvent {
    FinishedReconsumption,
    Error(String),
}

impl From<LoggingEvent> for Event {
    fn from(value: LoggingEvent) -> Self {
        Self::Logging(value)
    }
}

struct LoggingWorker {
    event_tx: Sender<Event>,
    command_rx: Receiver<LoggingCommand>,

    text_file: Option<fs::File>,
    raw_file: Option<fs::File>,
    started_logging_at: Option<DateTime<Local>>,
    settings: Logging,
    line_ending: LineEnding,

    /// Flag for if continued RX content could be appended without adding a newline first.
    last_rx_completed: bool,

    current_port: Option<SerialPortInfo>,

    #[cfg(feature = "defmt")]
    defmt: DefmtKit,
}

type HandleResult<T> = Result<T, LoggingWorkerMissing>;

#[derive(Debug, thiserror::Error)]
#[error("logging rx handle dropped")]
pub struct LoggingWorkerMissing;

impl<T> From<crossbeam::channel::SendError<T>> for LoggingWorkerMissing {
    fn from(_: crossbeam::channel::SendError<T>) -> Self {
        Self
    }
}

#[cfg(feature = "defmt")]
#[derive(Default)]
struct DefmtKit {
    /// Since the worker typically writes the recieved buffer to disk and then drops it,
    /// there's no existing cache for when an incomplete defmt frame is recieved,
    /// so this holds the incomplete frame's bytes and when it was first being recieved.
    unconsumed: Option<(DateTime<Local>, Vec<u8>)>,
    settings: Defmt,
    decoder: Option<Arc<super::defmt::DefmtDecoder>>,
    /// Flag if last defmt raw/uncompressed decode attempt failed.
    /// Further parsing attempts will not be allowed if set to true.
    defmt_raw_malformed: bool,
}

impl LoggingHandle {
    pub(super) fn new(
        line_ending: LineEnding,
        settings: Logging,
        event_tx: Sender<Event>,
        #[cfg(feature = "defmt")] defmt_settings: Defmt,
    ) -> (Self, JoinHandle<()>) {
        let (command_tx, command_rx) = crossbeam::channel::unbounded();

        let mut worker = LoggingWorker {
            event_tx,
            command_rx,
            settings,
            line_ending,
            text_file: None,
            raw_file: None,
            started_logging_at: None,
            last_rx_completed: true,
            current_port: None,
            #[cfg(feature = "defmt")]
            defmt: DefmtKit {
                settings: defmt_settings,
                ..Default::default()
            },
        };

        let worker = std::thread::spawn(move || {
            if let Err(e) = worker.work_loop() {
                error!("Logging worker encountered an unexpected fatal error! {e}")
            } else {
                debug!("Logging worker closed gracefully.")
            }
        });

        (Self { command_tx }, worker)
    }
    pub fn log_port_connected(
        &self,
        port_info: SerialPortInfo,
        reconnect_type: Option<ReconnectType>,
    ) -> HandleResult<()> {
        self.command_tx.send(LoggingCommand::PortConnected(
            Local::now(),
            port_info,
            reconnect_type,
        ))?;
        Ok(())
    }
    pub(super) fn begin_relogging(&self, receiver: Receiver<SyncBatch>) -> HandleResult<()> {
        self.command_tx
            .send(LoggingCommand::BeginRelogging(receiver))?;
        Ok(())
    }
    pub(super) fn log_rx_bytes(
        &self,
        timestamp: DateTime<Local>,
        bytes: Vec<u8>,
    ) -> HandleResult<()> {
        self.command_tx
            .send(LoggingCommand::RxBytes(timestamp, bytes))?;
        Ok(())
    }
    // TODO error with repeated line endings
    pub(super) fn log_tx_bytes(
        &self,
        timestamp: DateTime<Local>,
        bytes: Vec<u8>,
        line_ending: Vec<u8>,
    ) -> HandleResult<()> {
        self.command_tx.send(LoggingCommand::TxBytes(TxPayload {
            timestamp,
            bytes,
            line_ending,
        }))?;
        Ok(())
    }
    #[cfg(feature = "defmt")]
    pub fn update_defmt_settings(&self, settings: Defmt) -> HandleResult<()> {
        self.command_tx
            .send(LoggingCommand::DefmtSettings(settings))?;
        Ok(())
    }
    #[cfg(feature = "defmt")]
    pub fn update_defmt_decoder(
        &self,
        decoder: Option<Arc<super::defmt::DefmtDecoder>>,
    ) -> HandleResult<()> {
        self.command_tx
            .send(LoggingCommand::DefmtDecoder(decoder))?;
        Ok(())
    }
    pub(super) fn update_line_ending(&self, line_ending: LineEnding) -> HandleResult<()> {
        self.command_tx
            .send(LoggingCommand::LineEndingChange(line_ending))?;
        Ok(())
    }
    pub(super) fn update_settings(&self, logging: Logging) -> HandleResult<()> {
        self.command_tx.send(LoggingCommand::Settings(logging))?;
        Ok(())
    }
    pub fn log_port_disconnected(&self, back_to_port_selection: bool) -> HandleResult<()> {
        self.command_tx.send(LoggingCommand::PortDisconnect {
            timestamp: Local::now(),
            back_to_port_selection,
        })?;
        Ok(())
    }
    pub(super) fn shutdown(&self) -> Result<(), ()> {
        let (shutdown_tx, shutdown_rx) = crossbeam::channel::bounded(0);
        if self
            .command_tx
            .send(LoggingCommand::Shutdown(shutdown_tx))
            .is_ok()
        {
            if shutdown_rx.recv_timeout(Duration::from_secs(15)).is_ok() {
                Ok(())
            } else {
                error!("Logging thread didn't react to shutdown request.");
                Err(())
            }
        } else {
            error!("Couldn't send logging shutdown.");
            Err(())
        }
    }
}

impl LoggingWorker {
    fn work_loop(&mut self) -> Result<(), LoggingError> {
        loop {
            match self.command_rx.recv() {
                Ok(LoggingCommand::Shutdown(shutdown_tx)) => {
                    self.close_files(true)?;
                    if shutdown_tx.send(()).is_err() {
                        error!("Failed to reply to shutdown request!");
                        break Err(LoggingError::ShutdownReply);
                    } else {
                        break Ok(());
                    }
                }
                Ok(cmd) => self.handle_command(cmd)?,
                Err(RecvError) => break Err(LoggingError::HandleDropped),
            }
        }
    }
    fn log_connection_event(
        &mut self,
        timestamp: DateTime<Local>,
        connected_to: Option<&SerialPortInfo>,
    ) -> Result<(), std::io::Error> {
        if !self.settings.log_connection_events {
            return Ok(());
        }
        if let Some(text_file) = &mut self.text_file {
            if !self.last_rx_completed {
                write_line_ending(text_file)?;
                self.last_rx_completed = true;
            }

            let timestamp_format = if self.settings.timestamp.trim().is_empty() {
                DEFAULT_TIMESTAMP_FORMAT
            } else {
                &self.settings.timestamp
            };

            let time = timestamp.format(timestamp_format);

            let text = if let Some(port_info) = connected_to {
                let port_name = &port_info.port_name;
                format!("{time} | Connected to {port_name}!")
            } else {
                format!("{time} | Disconnected from port!")
            };

            text_file.write_all(text.as_bytes())?;
            write_line_ending(text_file)?;
        }

        Ok(())
    }
    fn handle_command(&mut self, cmd: LoggingCommand) -> Result<(), LoggingError> {
        match cmd {
            LoggingCommand::PortConnected(timestamp, port_info, _reconnect_type) => {
                self.create_and_close_log_files(timestamp, &port_info)?;
                self.log_connection_event(timestamp, Some(&port_info))?;
                self.current_port = Some(port_info);
            }
            LoggingCommand::BeginRelogging(receiver) => {
                let Some(port_info) = &self.current_port else {
                    self.event_tx.send(
                        LoggingEvent::Error("Logging worker missing port info, can't sync.".into())
                            .into(),
                    )?;
                    return Ok(());
                };
                if let Some(text) = &mut self.text_file {
                    text.set_len(0)?;
                    text.seek(std::io::SeekFrom::Start(0))?;
                    write_header_to_text_file(text, port_info)?;
                }
                if let Some(raw) = &mut self.raw_file {
                    raw.set_len(0)?;
                    raw.seek(std::io::SeekFrom::Start(0))?;
                }

                for msg in receiver.into_iter() {
                    match msg {
                        SyncBatch::RxBatch(rx_batch) => {
                            for (timestamp, bytes) in rx_batch {
                                if let Some(raw_file) = &mut self.raw_file {
                                    raw_file.write_all(&bytes)?;
                                }
                                if self.text_file.is_some() {
                                    self.consume_bytes_for_text_file(timestamp, bytes)?;
                                }
                            }
                        }
                        SyncBatch::TxBatch(tx_batch) => {
                            for TxPayload {
                                timestamp,
                                bytes,
                                line_ending,
                            } in tx_batch
                            {
                                let Some(text_file) = &mut self.text_file else {
                                    warn!("not logging tx bytes, no text file!");
                                    return Ok(());
                                };
                                if !self.settings.log_user_input {
                                    warn!("not logging tx bytes, user log disabled!");
                                    return Ok(());
                                }
                                self.last_rx_completed = write_buffer_to_text_file(
                                    timestamp,
                                    &self.settings.timestamp,
                                    &bytes,
                                    self.last_rx_completed,
                                    text_file,
                                    &self.line_ending,
                                    // self.settings.timestamps,
                                    LineType::Tx { line_ending },
                                )?;
                            }
                        }
                        SyncBatch::Done => {
                            self.flush_files(false)?;
                            self.event_tx
                                .send(LoggingEvent::FinishedReconsumption.into())?;
                        }
                    }
                }
            }
            LoggingCommand::RxBytes(timestamp, buf) => {
                if let Some(raw_file) = &mut self.raw_file {
                    raw_file.write_all(&buf)?;
                }
                if self.text_file.is_some() {
                    self.consume_bytes_for_text_file(timestamp, buf)?;
                }
            }
            LoggingCommand::TxBytes(TxPayload {
                timestamp,
                bytes,
                line_ending,
            }) => {
                let Some(text_file) = &mut self.text_file else {
                    warn!("not logging tx bytes, no text file!");
                    return Ok(());
                };
                if !self.settings.log_user_input {
                    warn!("not logging tx bytes, user log disabled!");
                    return Ok(());
                }
                self.last_rx_completed = write_buffer_to_text_file(
                    timestamp,
                    &self.settings.timestamp,
                    &bytes,
                    self.last_rx_completed,
                    text_file,
                    &self.line_ending,
                    // self.settings.timestamps,
                    LineType::Tx { line_ending },
                )?;
            }
            LoggingCommand::LineEndingChange(new_ending) => self.line_ending = new_ending,
            LoggingCommand::Settings(new) => {
                _ = std::mem::replace(&mut self.settings, new);
                // let new = &self.settings;

                if let Some(current_port) = self.current_port.clone() {
                    self.create_and_close_log_files(Local::now(), &current_port)?;
                }
            }
            #[cfg(feature = "defmt")]
            LoggingCommand::DefmtSettings(defmt_settings) => {
                let old = std::mem::replace(&mut self.defmt.settings, defmt_settings);
                let new = &self.defmt.settings;

                if changed!(old, new, defmt_parsing) && self.defmt.unconsumed.is_some() {
                    self.defmt.defmt_raw_malformed = false;
                    _ = self.defmt.unconsumed.take();
                }
            }
            #[cfg(feature = "defmt")]
            LoggingCommand::DefmtDecoder(decoder) => {
                self.defmt.decoder = decoder;
            }
            LoggingCommand::PortDisconnect {
                timestamp,
                back_to_port_selection,
            } => {
                if self.raw_file.is_none() && self.text_file.is_none() {
                    return Ok(());
                }
                self.log_connection_event(timestamp, None)?;
                if back_to_port_selection {
                    self.close_files(false)?;
                } else {
                    self.flush_files(false)?;
                }
            }
            LoggingCommand::Shutdown(_) => unreachable!("shutdown handled in work_loop"),
        }
        Ok(())
    }

    fn consume_bytes_for_text_file(
        &mut self,
        timestamp: DateTime<Local>,
        buf: Vec<u8>,
    ) -> Result<(), LoggingError> {
        #[cfg(feature = "defmt")]
        if let Some(text_file) = &mut self.text_file {
            use crate::settings::DefmtSupport;

            match self.defmt.settings.defmt_parsing {
                DefmtSupport::Disabled => {
                    self.last_rx_completed = write_buffer_to_text_file(
                        timestamp,
                        &self.settings.timestamp,
                        &buf,
                        self.last_rx_completed,
                        text_file,
                        &self.line_ending,
                        LineType::Rx,
                    )?;
                }
                DefmtSupport::FramedRzcobs | DefmtSupport::UnframedRzcobs | DefmtSupport::Raw => {
                    if let Some((_, existing_buf)) = &mut self.defmt.unconsumed {
                        existing_buf.extend(buf);
                    } else {
                        _ = self.defmt.unconsumed.insert((timestamp, buf));
                    }
                    self.consume_with_defmt()?;
                }
            }
        }
        #[cfg(not(feature = "defmt"))]
        if let Some(text_file) = &mut self.text_file {
            self.last_rx_completed = write_buffer_to_text_file(
                timestamp,
                &self.settings.timestamp,
                &buf,
                self.last_rx_completed,
                text_file,
                &self.line_ending,
                // self.settings.timestamps,
                LineType::Rx,
            )?;
        }
        Ok(())
    }

    #[cfg(feature = "defmt")]
    fn consume_with_defmt(&mut self) -> Result<(), std::io::Error> {
        if self.defmt.defmt_raw_malformed {
            _ = self.defmt.unconsumed.take();
            return Ok(());
        }

        use crate::settings::DefmtSupport;

        let Some((timestamp, unconsumed_buf)) = &mut self.defmt.unconsumed else {
            unreachable!();
        };

        let Some(text_file) = &mut self.text_file else {
            unreachable!();
        };

        let Some(decoder) = &self.defmt.decoder else {
            unconsumed_buf.clear();

            let defmt_encoding = match self.defmt.settings.defmt_parsing {
                DefmtSupport::Disabled => unreachable!(),
                DefmtSupport::FramedRzcobs => "framed rzcobs",
                DefmtSupport::UnframedRzcobs => "rzcobs",
                DefmtSupport::Raw => "uncompressed",
            };

            let mut text = format!("defmt table missing, can't decode ({defmt_encoding}): ");
            text.extend(unconsumed_buf.iter().map(|b| format!("{b:X}")));

            if !self.last_rx_completed {
                write_line_ending(text_file)?;
                self.last_rx_completed = true;
            }

            write_buffer_to_text_file(
                *timestamp,
                &self.settings.timestamp,
                text.as_bytes(),
                true,
                text_file,
                &LineEnding::None,
                LineType::DefmtRx,
            )?;
            write_line_ending(text_file)?;

            return Ok(());
        };

        use crate::buffer::defmt::rzcobs_decode;
        use defmt_decoder::DecodeError;

        match self.defmt.settings.defmt_parsing {
            DefmtSupport::Disabled => unreachable!("shouldn't be called when disabled"),
            DefmtSupport::Raw => loop {
                match decoder.table.decode(unconsumed_buf) {
                    Ok((decoded_frame, _consumed)) => {
                        self.last_rx_completed = write_defmt_frame_to_text_file(
                            *timestamp,
                            &self.settings.timestamp,
                            &decoded_frame,
                            self.last_rx_completed,
                            text_file,
                        )?;
                    }
                    Err(DecodeError::UnexpectedEof) => break,
                    Err(DecodeError::Malformed) => {
                        self.defmt.defmt_raw_malformed = true;

                        self.last_rx_completed = write_buffer_to_text_file(
                            *timestamp,
                            &self.settings.timestamp,
                            "malformed defmt packet, ceasing further decode attempts".as_bytes(),
                            self.last_rx_completed,
                            text_file,
                            &LineEnding::None,
                            LineType::DefmtRx,
                        )?;

                        break;
                    }
                }
            },
            // let mut rest = &unconsumed_buf[..];
            DefmtSupport::UnframedRzcobs => loop {
                use crate::buffer::{DelimitedSlice, defmt::frame_delimiting::zero_delimited};

                let unconsumed_len = unconsumed_buf.len();
                let Ok((rest, delimited_slice)) = zero_delimited(unconsumed_buf) else {
                    break;
                };

                let DelimitedSlice::DefmtRzcobs { inner, .. } = delimited_slice else {
                    unreachable!();
                };

                let Ok(uncompressed) = rzcobs_decode(inner) else {
                    self.last_rx_completed = write_buffer_to_text_file(
                        *timestamp,
                        &self.settings.timestamp,
                        "malformed rzcobs packet".as_bytes(),
                        self.last_rx_completed,
                        text_file,
                        &LineEnding::None,
                        LineType::DefmtRx,
                    )?;

                    unconsumed_buf.drain(..unconsumed_len - rest.len());
                    continue;
                };

                match decoder.table.decode(&uncompressed) {
                    Ok((decoded_frame, _consumed)) => {
                        self.last_rx_completed = write_defmt_frame_to_text_file(
                            *timestamp,
                            &self.settings.timestamp,
                            &decoded_frame,
                            self.last_rx_completed,
                            text_file,
                        )?;
                    }
                    Err(_) => {
                        self.last_rx_completed = write_buffer_to_text_file(
                            *timestamp,
                            &self.settings.timestamp,
                            "malformed defmt packet".as_bytes(),
                            true,
                            text_file,
                            &LineEnding::None,
                            LineType::DefmtRx,
                        )?;

                        unconsumed_buf.drain(..unconsumed_len - rest.len());
                        continue;
                    }
                }

                unconsumed_buf.drain(..unconsumed_len - rest.len());
            },
            DefmtSupport::FramedRzcobs => loop {
                use crate::buffer::{
                    DelimitedSlice, defmt::frame_delimiting::esp_println_delimited,
                };

                let unconsumed_len = unconsumed_buf.len();
                let Ok((rest, delimited_slice)) = esp_println_delimited(unconsumed_buf) else {
                    break;
                };

                match delimited_slice {
                    DelimitedSlice::DefmtRzcobs { inner, .. } => {
                        let Ok(uncompressed) = rzcobs_decode(inner) else {
                            self.last_rx_completed = write_buffer_to_text_file(
                                *timestamp,
                                &self.settings.timestamp,
                                "malformed rzcobs packet".as_bytes(),
                                self.last_rx_completed,
                                text_file,
                                &LineEnding::None,
                                LineType::DefmtRx,
                            )?;

                            unconsumed_buf.drain(..unconsumed_len - rest.len());
                            continue;
                        };

                        match decoder.table.decode(&uncompressed) {
                            Ok((decoded_frame, _consumed)) => {
                                self.last_rx_completed = write_defmt_frame_to_text_file(
                                    *timestamp,
                                    &self.settings.timestamp,
                                    &decoded_frame,
                                    self.last_rx_completed,
                                    text_file,
                                )?;
                            }
                            Err(_) => {
                                self.last_rx_completed = write_buffer_to_text_file(
                                    *timestamp,
                                    &self.settings.timestamp,
                                    "malformed defmt packet".as_bytes(),
                                    self.last_rx_completed,
                                    text_file,
                                    &LineEnding::None,
                                    LineType::DefmtRx,
                                )?;

                                unconsumed_buf.drain(..unconsumed_len - rest.len());
                                continue;
                            }
                        }
                    }
                    DelimitedSlice::Unknown(potentially_text) => {
                        self.last_rx_completed = write_buffer_to_text_file(
                            *timestamp,
                            &self.settings.timestamp,
                            potentially_text,
                            self.last_rx_completed,
                            text_file,
                            &self.line_ending,
                            LineType::Rx,
                        )?;
                    }
                    DelimitedSlice::DefmtRaw(_) => unreachable!(),
                }

                unconsumed_buf.drain(..unconsumed_len - rest.len());
            },
        }
        Ok(())
    }

    fn flush_files(&mut self, ignore_errors: bool) -> Result<(), std::io::Error> {
        let flush_file = |f: &mut fs::File| -> Result<(), std::io::Error> {
            match f.flush() {
                Ok(()) => (),
                Err(e) if ignore_errors => error!("Error flushing file, ignoring: {e}"),
                Err(e) => {
                    error!("Error flushing file: {e}");
                    return Err(e);
                }
            }
            match f.sync_all() {
                Ok(()) => (),
                Err(e) if ignore_errors => error!("Error flushing file, ignoring: {e}"),
                Err(e) => {
                    error!("Error flushing file: {e}");
                    return Err(e);
                }
            }
            Ok(())
        };
        if let Some(raw_file) = &mut self.raw_file {
            flush_file(raw_file)?;
        }
        if let Some(text_file) = &mut self.text_file {
            flush_file(text_file)?;
        }

        Ok(())
    }

    fn close_files(&mut self, ignore_errors: bool) -> Result<(), LoggingError> {
        _ = self.started_logging_at.take();

        self.flush_files(ignore_errors)?;
        _ = self.raw_file.take();
        _ = self.text_file.take();

        _ = self.current_port.take();

        Ok(())
    }

    fn create_and_close_log_files(
        &mut self,
        started_at: DateTime<Local>,
        port_info: &SerialPortInfo,
    ) -> Result<(), LoggingError> {
        let logs_dir = config_adjacent_path("logs/");
        match logs_dir.try_exists() {
            Ok(true) => (),
            Ok(false) => fs::create_dir_all(logs_dir)?,
            Err(e) => {
                error!("Error checking for logs dir!");
                Err(e)?;
            }
        }

        let make_binary_log = || -> Result<fs::File, std::io::Error> {
            let timestamped_name = started_at.format("yap-%Y-%m-%d_%H-%M-%S.bin");

            fs::File::create(config_adjacent_path(format!("logs/{timestamped_name}")))
        };

        let make_text_log = |port_info: &SerialPortInfo| -> Result<fs::File, std::io::Error> {
            let timestamped_name = started_at.format("yap-%Y-%m-%d_%H-%M-%S.txt");

            let path = config_adjacent_path(format!("logs/{timestamped_name}"));
            fs::File::create(path).and_then(|mut file| {
                write_header_to_text_file(&mut file, port_info)?;
                Ok(file)
            })
        };

        if self.text_file.is_none() {
            self.last_rx_completed = true;
        }

        match (self.settings.log_raw_input_to_file, &mut self.raw_file) {
            // No action needed
            (true, Some(_)) | (false, None) => (),
            // Need to open a file
            (true, empty_raw @ None) => {
                let new_raw = make_binary_log()?;
                _ = empty_raw.insert(new_raw);
            }
            // Need to close our file
            (false, raw @ Some(_)) => {
                let mut raw_file = raw.take().unwrap();
                raw_file.flush()?;
                raw_file.sync_all()?;
            }
        }

        match (self.settings.log_text_to_file, &mut self.text_file) {
            // No action needed
            (true, Some(_)) | (false, None) => (),
            // Need to open a file
            (true, empty_text @ None) => {
                let new_text = make_text_log(port_info)?;
                _ = empty_text.insert(new_text);
            }
            // Need to close our file
            (false, text @ Some(_)) => {
                let mut text_file = text.take().unwrap();
                text_file.flush()?;
                text_file.sync_all()?;
            }
        }

        Ok(())
    }
}

fn write_header_to_text_file(
    file: &mut fs::File,
    // started_at: DateTime<Local>,
    // timestamp_fmt: &str,
    port_info: &SerialPortInfo,
) -> Result<(), std::io::Error> {
    let port_text = match &port_info.port_type {
        serialport::SerialPortType::BluetoothPort => Cow::from("BT"),
        serialport::SerialPortType::PciPort => Cow::from("PCI"),
        serialport::SerialPortType::Unknown => Cow::from("Unknown"),
        serialport::SerialPortType::UsbPort(serialport::UsbPortInfo {
            pid,
            vid,
            serial_number: Some(serial),
            ..
        }) => Cow::from(format!("USB ({pid:04X}:{vid:04X}:{serial})")),
        serialport::SerialPortType::UsbPort(serialport::UsbPortInfo {
            pid,
            vid,
            serial_number: None,
            ..
        }) => Cow::from(format!("USB ({pid:04X}:{vid:04X})")),
    };

    // let header_timestamp_format = if timestamp_fmt.trim().is_empty() {
    //     DEFAULT_TIMESTAMP_FORMAT
    // } else {
    //     timestamp_fmt
    // };

    // let header_timestamp = started_at.format(header_timestamp_format);

    // let file_header = format!(
    //     "{time} | Port: {name} | {port_text}",
    //     name = port_info.port_name,
    //     time = header_timestamp,
    // );

    let file_header = format!("Port: {name} | {port_text}", name = port_info.port_name,);

    file.write_all(file_header.as_bytes())?;
    write_line_ending(file)?;

    Ok(())
}

/// Output a line ending, not for rendering [`LineEndings`].
fn write_line_ending(file: &mut fs::File) -> Result<(), std::io::Error> {
    file.write_all(b"\n")
}

fn write_buffer_to_text_file(
    timestamp: DateTime<Local>,
    timestamp_fmt: &str,
    bytes: &[u8],
    mut last_line_was_completed: bool,
    text_file: &mut fs::File,
    line_ending: &LineEnding,
    // with_timestamp: bool,
    line_type: LineType,
) -> Result<bool, std::io::Error> {
    // Incomplete UTF-8 sequences aren't handled as gracefully here as they are with the actual UI....
    let is_tx_line = matches!(&line_type, LineType::Tx { .. });
    let appendable_text = matches!(&line_type, LineType::Rx);
    let timestamp_string = if timestamp_fmt.trim().is_empty() {
        None
    } else {
        Some(timestamp.format(timestamp_fmt).to_string())
    };

    for (_trunc, orig, _indices) in line_ending_iter(bytes, line_ending) {
        if last_line_was_completed || !appendable_text {
            let line_to_write = {
                let line_capacity = orig.len();
                let mut output = String::with_capacity(line_capacity);
                if let Some(timestamp_str) = &timestamp_string {
                    output.push_str(timestamp_str);
                    output.push_str(": ");
                }
                if is_tx_line {
                    output.push_str("[USER] ")
                }

                output
            };
            if !last_line_was_completed && !appendable_text {
                write_line_ending(text_file)?;
            }
            text_file.write_all(line_to_write.as_bytes())?;
        }

        // for c in orig.escape_bytes() {
        //     let mut buf = [0; 4]; // Max bytes for any UTF-8 char
        //     let encoded = c.encode_utf8(&mut buf);
        //     text_file.write_all(encoded.as_bytes())?;
        // }

        for c in orig.escape_ascii() {
            text_file.write_all(&[c])?;
        }
        if let LineType::Tx { line_ending } = &line_type {
            for c in line_ending.escape_ascii() {
                text_file.write_all(&[c])?;
            }
        }

        last_line_was_completed = orig.has_line_ending(line_ending) || !appendable_text;
        if last_line_was_completed {
            write_line_ending(text_file)?;
        }
    }

    let last_line_is_completed = last_line_was_completed;
    Ok(last_line_is_completed)
}

#[cfg(feature = "defmt")]
fn write_defmt_frame_to_text_file(
    timestamp: DateTime<Local>,
    timestamp_fmt: &str,
    frame: &defmt_decoder::Frame,
    last_line_was_completed: bool,
    text_file: &mut fs::File,
) -> Result<bool, std::io::Error> {
    if !last_line_was_completed {
        write_line_ending(text_file)?;
    }

    let timestamp_string = if timestamp_fmt.trim().is_empty() {
        None
    } else {
        Some(timestamp.format(timestamp_fmt).to_string())
    };
    let defmt_message = format!("{}", frame.display(false));
    let lines = defmt_message.lines();

    for line in lines {
        let mut output = String::new();

        if let Some(timestamp_str) = &timestamp_string {
            output.push_str(timestamp_str);
            output.push_str(": ");
        }

        let text = format!("[defmt] {line}");
        output.push_str(&text);

        text_file.write_all(output.as_bytes())?;
        write_line_ending(text_file)?;
    }

    Ok(true)
}

#[derive(Debug, thiserror::Error)]
enum LoggingError {
    #[error("logging file error")]
    File(#[from] std::io::Error),
    #[error("failed to send event to main app")]
    EventSend,
    #[error("failed to reply to shutdown request in time")]
    ShutdownReply,
    #[error("handle dropped, can't recieve commands")]
    HandleDropped,
}

impl<T> From<SendError<T>> for LoggingError {
    fn from(_value: SendError<T>) -> Self {
        Self::EventSend
    }
}
