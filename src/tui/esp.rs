use std::{
    borrow::Cow,
    time::{Duration, Instant},
};

use camino::Utf8PathBuf;
use compact_str::CompactString;
use espflash::{flasher::DeviceInfo, target::Chip};
use fs_err as fs;
use ratatui::{
    prelude::*,
    widgets::{Block, Clear, Gauge, Row, Table},
};
use ratatui_macros::{line, vertical};
use tracing::{debug, warn};

use crate::{
    config_adjacent_path,
    notifications::Notifications,
    serial::esp::{EspEvent, FlashProgress},
    traits::{LastIndex, LineHelpers},
};

const ESP_PROFILES_PATH: &str = "yap_espflash_profiles.toml";

use serde::Deserialize;

use super::centered_rect_size;

// TODO move file stuff out of TUI module

#[derive(Debug)]
pub enum EspProfile {
    Bins(EspBins),
    Elf(EspElf),
}

impl EspProfile {
    #[cfg(feature = "defmt")]
    pub fn defmt_elf_path(&self) -> Option<Utf8PathBuf> {
        match self {
            EspProfile::Bins(EspBins {
                defmt_elf_path: Some(path),
                ..
            }) => Some(path.to_owned()),

            EspProfile::Elf(EspElf {
                path, defmt: true, ..
            }) => Some(path.to_owned()),

            _ => None,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            EspProfile::Bins(EspBins { name, .. }) | EspProfile::Elf(EspElf { name, .. }) => {
                name.as_ref()
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct EspBins {
    pub name: CompactString,
    pub bins: Vec<(u32, Utf8PathBuf)>,
    pub upload_baud: Option<u32>,
    pub expected_chip: Option<Chip>,
    pub no_skip: bool,
    pub no_verify: bool,
    #[cfg(feature = "defmt")]
    pub defmt_elf_path: Option<Utf8PathBuf>,
}

#[derive(Debug, Clone)]
pub struct EspElf {
    pub name: CompactString,
    pub path: Utf8PathBuf,
    pub upload_baud: Option<u32>,
    pub expected_chip: Option<Chip>,
    pub partition_table: Option<Utf8PathBuf>,
    pub bootloader: Option<Utf8PathBuf>,
    pub no_skip: bool,
    pub no_verify: bool,
    pub ram: bool,
    #[cfg(feature = "defmt")]
    pub defmt: bool,
}

impl<'de> serde::Deserialize<'de> for EspElf {
    fn deserialize<D>(deserializer: D) -> Result<EspElf, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error, MapAccess, Visitor};
        use std::fmt;

        struct EspElfVisitor;

        impl<'de> Visitor<'de> for EspElfVisitor {
            type Value = EspElf;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a map representing EspElf with validation")
            }

            fn visit_map<A>(self, mut map: A) -> Result<EspElf, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut name = None;
                let mut path = None;
                let mut upload_baud = None;
                let mut expected_chip = None;
                let mut partition_table = None;
                let mut bootloader = None;
                let mut no_skip = false;
                let mut no_verify = false;
                let mut ram = false;
                #[cfg(feature = "defmt")]
                let mut defmt = false;
                let mut any_chip = false;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "name" => {
                            let value: CompactString = map.next_value()?;
                            name = Some(value);
                        }
                        "path" => {
                            let value: Utf8PathBuf = map.next_value()?;
                            path = Some(value);
                        }
                        "upload_baud" => {
                            upload_baud = Some(map.next_value()?);
                        }
                        "chip" => {
                            let value: String = map.next_value()?;
                            if value == "any" {
                                any_chip = true;
                            } else {
                                expected_chip =
                                    Some(value.parse::<Chip>().map_err(|_| {
                                        A::Error::custom("invalid chip type given")
                                    })?);
                            }
                        }
                        "partition_table" => {
                            partition_table = Some(map.next_value()?);
                        }
                        "bootloader" => {
                            bootloader = Some(map.next_value()?);
                        }
                        "no_skip" => {
                            no_skip = map.next_value()?;
                        }
                        "no_verify" => {
                            no_verify = map.next_value()?;
                        }
                        "ram" => {
                            ram = map.next_value()?;
                        }
                        #[cfg(feature = "defmt")]
                        "defmt" => {
                            defmt = map.next_value()?;
                        }
                        _ => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }

                let name = name.ok_or_else(|| A::Error::missing_field("name"))?;
                let path = path.ok_or_else(|| A::Error::missing_field("path"))?;

                if !any_chip && expected_chip.is_none() {
                    return Err(A::Error::missing_field("chip"));
                }

                Ok(EspElf {
                    name,
                    path,
                    upload_baud,
                    expected_chip,
                    partition_table,
                    bootloader,
                    no_skip,
                    no_verify,
                    ram,
                    #[cfg(feature = "defmt")]
                    defmt,
                })
            }
        }

        deserializer.deserialize_map(EspElfVisitor)
    }
}

// fn deserialize_chip<'de, D>(deserializer: D) -> Result<Option<Chip>, D::Error>
// where
//     D: serde::Deserializer<'de>,
// {
//     use serde::de::Error;
//     use std::str::FromStr;
//     let opt = Option::<String>::deserialize(deserializer)?;
//     match opt {
//         Some(s) => Chip::from_str(&s)
//             .map(Some)
//             .map_err(|_| D::Error::custom("invalid chip type given")),
//         None => Ok(None),
//     }
// }

impl<'de> Deserialize<'de> for EspBins {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, Visitor};
        use std::fmt;

        struct EspBinsVisitor;

        impl<'de> Visitor<'de> for EspBinsVisitor {
            type Value = EspBins;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a table for EspBins in custom TOML format")
            }

            fn visit_map<A>(self, mut map: A) -> Result<EspBins, A::Error>
            where
                A: MapAccess<'de>,
            {
                use serde::de::Error;

                let mut name = None;
                let mut bins = Vec::new();
                let mut upload_baud = None;
                let mut expected_chip = None;
                let mut no_skip = false;
                let mut no_verify = false;
                let mut any_chip = false;
                #[cfg(feature = "defmt")]
                let mut defmt_elf_path = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "name" => {
                            let value: CompactString = map.next_value()?;
                            name = Some(value);
                        }
                        "upload_baud" => {
                            upload_baud = Some(map.next_value()?);
                        }
                        "chip" => {
                            let value: String = map.next_value()?;
                            if value == "any" {
                                any_chip = true;
                            } else {
                                expected_chip =
                                    Some(value.parse::<Chip>().map_err(|_| {
                                        A::Error::custom("invalid chip type given")
                                    })?);
                            }
                        }
                        "no_verify" => {
                            no_verify = map.next_value()?;
                        }
                        "no_skip" => {
                            no_skip = map.next_value()?;
                        }
                        #[cfg(feature = "defmt")]
                        "defmt_elf_path" => {
                            defmt_elf_path = map.next_value()?;
                        }
                        other if other.starts_with("0x") => {
                            let offset_num =
                                u32::from_str_radix(other.trim_start_matches("0x"), 16).map_err(
                                    |_| {
                                        A::Error::custom(format!("invalid bin offset key: {other}"))
                                    },
                                )?;
                            let path_val: String = map.next_value()?;
                            bins.push((offset_num, Utf8PathBuf::from(path_val)));
                        }
                        _ => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }

                let name = name.ok_or_else(|| A::Error::missing_field("name"))?;

                if !any_chip && expected_chip.is_none() {
                    return Err(A::Error::missing_field("chip"));
                }

                Ok(EspBins {
                    name,
                    bins,
                    upload_baud,
                    expected_chip,
                    no_skip,
                    no_verify,
                    #[cfg(feature = "defmt")]
                    defmt_elf_path,
                })
            }
        }

        deserializer.deserialize_map(EspBinsVisitor)
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct SerializedEspFiles {
    #[serde(rename = "elf")]
    #[serde(default)]
    elfs: Vec<EspElf>,
    #[serde(rename = "bin")]
    #[serde(default)]
    bins: Vec<EspBins>,
}

impl SerializedEspFiles {
    pub fn check_for_duplicate_names(&self) -> Result<(), EspProfileError> {
        use std::collections::HashSet;
        let mut names = HashSet::new();
        for elf in &self.elfs {
            if !names.insert(elf.name.as_str()) {
                return Err(EspProfileError::DuplicateProfileName(elf.name.to_string()));
            }
        }
        for bin in &self.bins {
            if !names.insert(bin.name.as_str()) {
                return Err(EspProfileError::DuplicateProfileName(bin.name.to_string()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub enum SegmentAction {
    #[default]
    Flashing,
    Verifying,
    Skipped,
    Finished,
}

#[derive(Debug, Default)]
pub struct Flashing {
    current_file_name: Option<String>,
    chip: CompactString,
    current_file_addr: u32,
    current_file_len: usize,
    current_progress: usize,
    current_action: SegmentAction,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum EspPopup {
    Connecting,
    Connected { chip: CompactString },
    DeviceInfo(Table<'static>),
    Flashing(Flashing),
    Erasing { chip: CompactString },
}

#[derive(Default)]
pub struct EspFlashHelper {
    popup: Option<EspPopup>,
    bins: Vec<EspBins>,
    elfs: Vec<EspElf>,
    pub first_erase_press: Option<Instant>,
    pub unchecked_bootloader: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum EspProfileError {
    #[error("failed reading profiles file")]
    FileRead(#[source] std::io::Error),
    #[error("failed saving to profiles file")]
    FileWrite(#[source] std::io::Error),
    #[error("invalid espflash profile")]
    Deser(#[from] toml::de::Error),
    #[error("duplicate profile name found: `{0}`")]
    DuplicateProfileName(String),
    // #[error("failed serializing espflash profiles: {0}")]
    // Ser(#[from] toml::ser::Error),
}

impl EspFlashHelper {
    pub fn build() -> Result<Self, EspProfileError> {
        let toml_path = config_adjacent_path(ESP_PROFILES_PATH);
        if toml_path.exists() {
            let profiles_toml = fs::read_to_string(toml_path).map_err(EspProfileError::FileRead)?;
            let profiles_wrapper: SerializedEspFiles = toml::from_str(&profiles_toml)?;
            profiles_wrapper.check_for_duplicate_names()?;

            let SerializedEspFiles { elfs, bins } = profiles_wrapper;

            Ok(Self {
                bins,
                elfs,
                ..Default::default()
            })
        } else {
            warn!("espflash profiles file was missing! creating...");
            fs::write(
                toml_path,
                include_str!("../../example_configs/yap_espflash_profiles.toml.blank").as_bytes(),
            )
            .map_err(EspProfileError::FileWrite)?;
            Ok(Self::default())
        }
    }
    pub fn consume_event(
        &mut self,
        event: EspEvent,
        notifs: &mut Notifications,
        ctrl_c_tx: &crossbeam::channel::Sender<()>,
    ) {
        match event {
            EspEvent::BootloaderSuccess { chip } => notifs.notify_str(
                format!("{chip} successfully reset into bootloader!"),
                Color::Green,
            ),
            EspEvent::EraseSuccess { chip } => {
                notifs.notify_str(format!("{chip} flash erased!"), Color::Green)
            }
            EspEvent::BootloaderAttempt => notifs.notify_str(
                "Attempted ESP reset into bootloader! (Unchecked)",
                Color::LightYellow,
            ),
            EspEvent::HardResetAttempt => {
                notifs.notify_str("Attempted ESP hard reset!", Color::LightYellow)
            }

            EspEvent::DeviceInfo(info) => {
                debug!("{info:#?}");
                let DeviceInfo {
                    chip,
                    revision,
                    crystal_frequency,
                    flash_size,
                    features,
                    mac_address: mac_address_opt,
                } = info;

                let mac_address: Cow<'_, str> = mac_address_opt
                    .map(|mac| mac.to_ascii_uppercase().into())
                    .unwrap_or("???".into());
                let revision: Cow<'_, str> = revision
                    .map(|(major, minor)| format!("v{major}.{minor}").into())
                    .unwrap_or("???".into());
                let features = features.join(", ");

                let rows: Vec<Row> = vec![
                    Row::new([
                        line!["Chip:"].right_aligned(),
                        line![chip.to_string().to_uppercase()].centered(),
                    ]),
                    Row::new([
                        line!["Revision:"].right_aligned(),
                        line![revision].centered(),
                    ]),
                    Row::new([
                        line!["Crystal Osc. Frequency:"].right_aligned(),
                        line![format!("{crystal_frequency}")].centered(),
                    ]),
                    Row::new([
                        line!["Flash Size:"].right_aligned(),
                        line![format!("{flash_size}")].centered(),
                    ]),
                    Row::new([
                        line!["Features:"].right_aligned(),
                        line![features].centered(),
                    ]),
                    Row::new([
                        line!["MAC Address:"].right_aligned(),
                        line![mac_address].centered(),
                    ]),
                ];

                let table = Table::new(
                    rows,
                    [Constraint::Percentage(45), Constraint::Percentage(55)],
                );

                self.popup = Some(EspPopup::DeviceInfo(table));
            }
            EspEvent::FlashProgress(progress) => match progress {
                FlashProgress::SegmentInit {
                    chip,
                    addr,
                    size,
                    file_name,
                } => {
                    self.popup = Some(EspPopup::Flashing(Flashing {
                        chip,
                        current_file_addr: addr,
                        current_file_len: size,
                        current_progress: 0,
                        current_file_name: file_name,
                        current_action: SegmentAction::Flashing,
                    }));
                }
                FlashProgress::Progress(progress) => {
                    let Some(EspPopup::Flashing(flashing)) = self.popup.take() else {
                        unreachable!("expected progress to update");
                    };
                    self.popup = Some(EspPopup::Flashing(Flashing {
                        current_progress: progress,
                        ..flashing
                    }));
                }
                FlashProgress::Verifying => {
                    let Some(EspPopup::Flashing(flashing)) = self.popup.take() else {
                        unreachable!("expected progress to update");
                    };
                    self.popup = Some(EspPopup::Flashing(Flashing {
                        current_action: SegmentAction::Verifying,
                        ..flashing
                    }));
                }
                FlashProgress::SegmentFinished { skipped } => {
                    let Some(EspPopup::Flashing(flashing)) = self.popup.take() else {
                        unreachable!("expected progress to update");
                    };
                    let current_action = if skipped {
                        SegmentAction::Skipped
                    } else {
                        SegmentAction::Finished
                    };
                    self.popup = Some(EspPopup::Flashing(Flashing {
                        current_progress: flashing.current_file_len,
                        current_action,
                        ..flashing
                    }));
                }
            },
            EspEvent::Connecting => self.popup = Some(EspPopup::Connecting),
            EspEvent::Connected { chip } => self.popup = Some(EspPopup::Connected { chip }),
            EspEvent::EraseStart { chip } => self.popup = Some(EspPopup::Erasing { chip }),
            EspEvent::PortReturned => {
                // Always dismiss a popup *unless* it's the device info,
                // since otherwise it'll just be instantly dismissed.
                if !self.device_info_shown() {
                    self.popup = None;
                }
                match ctrl_c_tx.try_send(()) {
                    Ok(()) => (),
                    // Already an ack to be seen, don't need to act.
                    Err(crossbeam::channel::TrySendError::Full(_)) => (),
                    Err(crossbeam::channel::TrySendError::Disconnected(_)) => {
                        panic!("Failed to ack potentially buffered Ctrl-C!")
                    }
                }
            }
            EspEvent::Error(_) => unreachable!("should be handled by caller"),
        }
    }
    pub fn reset_popup(&mut self) {
        _ = self.popup.take();
    }
    pub fn popup_active(&self) -> bool {
        self.popup.is_some()
    }
    pub fn device_info_shown(&self) -> bool {
        matches!(&self.popup, Some(EspPopup::DeviceInfo(_)))
    }
    pub fn render_espflash_popups(&self, frame: &mut Frame, screen: Rect) {
        let center_area = centered_rect_size(
            Size {
                width: 60,
                height: 8,
            },
            screen,
        );

        let Some(popup) = &self.popup else {
            return;
        };

        frame.render_widget(Clear, center_area);

        let border_color = match popup {
            EspPopup::Connected { .. } => Color::Green,
            EspPopup::Connecting => Color::Cyan,
            EspPopup::DeviceInfo { .. } => Color::LightGreen,
            EspPopup::Erasing { .. } => Color::Yellow,
            EspPopup::Flashing { .. } => Color::Blue,
        };

        let block_title = match popup {
            // EspPopup::Connected { .. } => " Connected! ",
            // EspPopup::Connecting { .. } => " Connecting... ",
            // EspPopup::Erasing { .. } => " Erasing... ",
            EspPopup::Erasing { .. } | EspPopup::Connected { .. } | EspPopup::Connecting => {
                Cow::from("")
            }
            EspPopup::DeviceInfo { .. } => Cow::from(" Retrieved ESP Info "),
            EspPopup::Flashing(Flashing { chip, .. }) => Cow::from(format!(" Flashing {chip}... ")),
        };

        let block = Block::bordered()
            .border_style(Style::from(border_color))
            .title_top(
                Line::raw(block_title)
                    .centered()
                    .all_spans_styled(Style::reset()),
            );

        frame.render_widget(&block, center_area);

        let inner_area = block.inner(center_area);

        let [
            title_area,
            body1_area,
            body2_area,
            chunks_text,
            which_bytes,
            progress_area,
        ] = vertical![==1,==1,*=1,==1,==1,==1].areas(inner_area);

        match popup {
            EspPopup::Flashing(Flashing {
                chip: _chip,
                current_file_addr,
                current_file_len,
                current_progress,
                current_file_name,
                current_action,
            }) => {
                let mut file_and_addr_line =
                    line!["@ 0x", format!("{current_file_addr:06X}")].centered();
                if let Some(file_name) = current_file_name {
                    file_and_addr_line.spans.insert(0, Span::raw(file_name));
                    file_and_addr_line.spans.insert(1, Span::raw(" "));
                }
                frame.render_widget(file_and_addr_line, title_area);

                frame.render_widget(line!["Chunks: "].centered(), chunks_text);
                frame.render_widget(
                    line![format!("{current_progress} / {current_file_len}")].centered(),
                    which_bytes,
                );

                let ratio = if *current_file_len == 0 {
                    0.0
                } else {
                    *current_progress as f64 / *current_file_len as f64
                };

                let label = match current_action {
                    SegmentAction::Flashing => Cow::from(format!("{:.2}%", ratio * 100.0)),
                    SegmentAction::Verifying => Cow::from("Verifying..."),
                    SegmentAction::Skipped => Cow::from("Skipped! (checksum matches)"),
                    SegmentAction::Finished => Cow::from("Segment flashed successfully!"),
                };

                let gauge_style: Style = match current_action {
                    SegmentAction::Flashing => Color::Green.into(),
                    SegmentAction::Verifying => Color::LightMagenta.into(),
                    SegmentAction::Skipped => Color::LightBlue.into(),
                    SegmentAction::Finished => Color::LightGreen.into(),
                };

                let progressbar = Gauge::default()
                    .gauge_style(gauge_style)
                    .label(label)
                    .ratio(ratio);

                frame.render_widget(progressbar, progress_area);
            }
            EspPopup::Connecting => {
                frame.render_widget(
                    line!["Connecting to Espressif device..."].centered(),
                    body1_area,
                );
                frame.render_widget(
                    line!["Try holding down BOOT/IO0"].centered().dark_gray(),
                    chunks_text,
                );
                frame.render_widget(
                    line!["during connection if unreliable."]
                        .centered()
                        .dark_gray(),
                    which_bytes,
                );
            }
            EspPopup::Connected { chip } => {
                frame.render_widget(line!["Connected to ", chip, "!"].centered(), body2_area);
            }
            EspPopup::Erasing { chip } => {
                frame.render_widget(
                    line!["Erasing ", chip, " flash contents..."].centered(),
                    body2_area,
                );
            }
            EspPopup::DeviceInfo(text) => {
                frame.render_widget(text, inner_area);
            }
        }
    }
    pub fn profiles_table(&self) -> Table<'_> {
        let cell_highlight_style = Style::new().reversed().italic();

        let rows: Vec<_> = self
            .profiles()
            .map(|(name, is_elf, is_elf_ram)| {
                let flavor = if is_elf_ram {
                    "Load ELF!"
                } else if is_elf {
                    "Flash ELF!"
                } else {
                    "Flash BIN!"
                };
                Row::new([
                    Text::raw(format!("{name} ")).right_aligned(),
                    Text::raw(flavor).centered().italic(),
                ])
            })
            .collect();

        Table::new(
            rows,
            [Constraint::Percentage(60), Constraint::Percentage(40)],
        )
        .cell_highlight_style(cell_highlight_style)
    }
    pub fn profiles(&self) -> impl DoubleEndedIterator<Item = (&str, bool, bool)> {
        let elf_iter = self.elfs.iter().map(|e| (e.name.as_str(), true, e.ram));
        let bin_iter = self.bins.iter().map(|b| (b.name.as_str(), false, false));

        elf_iter.chain(bin_iter)
    }
    pub fn profile_from_name(&self, query: &str) -> Option<EspProfile> {
        // Search elfs first
        if let Some(elf) = self.elfs.iter().find(|e| e.name == query) {
            return Some(EspProfile::Elf(elf.clone()));
        }
        // Then bins
        if let Some(bin) = self.bins.iter().find(|b| b.name == query) {
            return Some(EspProfile::Bins(bin.clone()));
        }
        None
    }
    pub fn profile_from_index(&self, index: usize) -> Option<EspProfile> {
        // Elfs first, then bins, matching the order in profiles()
        let elf_len = self.elfs.len();
        if index < elf_len {
            self.elfs.get(index).cloned().map(EspProfile::Elf)
        } else {
            let bin_index = index - elf_len;
            self.bins.get(bin_index).cloned().map(EspProfile::Bins)
        }
    }
    pub fn is_empty(&self) -> bool {
        self.elfs.is_empty() && self.bins.is_empty()
    }
    pub fn len(&self) -> usize {
        self.elfs.len() + self.bins.len()
    }
}

impl LastIndex for EspFlashHelper {
    fn last_index_checked(&self) -> Option<usize> {
        if self.is_empty() {
            None
        } else {
            Some((self.elfs.len() + self.bins.len()) - 1)
        }
    }
}

pub const ERASE_FLASH_CONFIRM_PERIOD: Duration = Duration::from_millis(1500);

pub const ESPFLASH_BUTTON_COUNT: usize = 4;

pub fn espflash_buttons(
    unchecked_bootloader: bool,
    awaiting_erase_confirm: bool,
) -> Table<'static> {
    let cell_highlight_style = Style::new().reversed().italic();

    let rows: Vec<Row> = vec![
        Row::new([
            Text::raw("ESP->User Code  ").right_aligned(),
            Text::raw("Reboot!").centered().italic(),
        ]),
        Row::new([
            Text::raw("ESP->Bootloader ").right_aligned(),
            // TODO < > ?
            Text::raw(if unchecked_bootloader {
                "Reboot! (Unchecked)"
            } else {
                "Reboot!"
            })
            .centered()
            .italic(),
        ]),
        Row::new([
            Text::raw("ESP->Device Info").right_aligned(),
            Text::raw("Get!").centered().italic(),
        ]),
        Row::new([
            Text::raw("ESP->Erase Flash").right_aligned(),
            Text::raw(if awaiting_erase_confirm {
                "Confirm?"
            } else {
                "Erase!"
            })
            .centered()
            .italic(),
        ]),
    ];

    Table::new(
        rows,
        [Constraint::Percentage(60), Constraint::Percentage(40)],
    )
    .cell_highlight_style(cell_highlight_style)
}
