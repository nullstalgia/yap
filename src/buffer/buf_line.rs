use std::{borrow::Cow, ops::Range};

use chrono::{DateTime, Local};
use compact_str::{CompactString, format_compact};
use ratatui::{
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

#[cfg(feature = "defmt")]
use crate::settings::Defmt;
use crate::{
    buffer::{LineEnding, RangeSlice},
    settings::Rendering,
    traits::LineHelpers,
};

const TIME_FORMAT: &str = "[%H:%M:%S%.3f] ";

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
/// The shared base object-to-render between Port/User lines.
pub struct BufLine {
    pub(super) timestamp: DateTime<Local>,

    range_in_raw_buffer: Range<usize>,

    pub(super) value: Line<'static>,

    /// How many vertical lines are needed in the terminal to fully show this line.
    // Truncated from usize, since even the ratatui sizes are capped there.
    rendered_line_height: u16,

    pub line_type: LineType,
}

#[derive(Clone, Copy)]
pub struct RenderSettings<'a> {
    pub rendering: &'a Rendering,
    #[cfg(feature = "defmt")]
    pub defmt: &'a Defmt,
}

// impl PartialEq for BufLine {
//     fn eq(&self, other: &Self) -> bool {
//         self.timestamp == other.timestamp
//     }
// }

// impl Eq for BufLine {}

// impl PartialOrd for BufLine {
//     fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
//         self.timestamp.partial_cmp(&other.timestamp)
//     }
// }

// impl Ord for BufLine {
//     fn cmp(&self, other: &Self) -> std::cmp::Ordering {
//         self.timestamp.cmp(&other.timestamp)
//     }
// }

#[derive(Debug, Clone, PartialEq, Eq, strum::EnumIs)]
/// Termination status of a BufLine.
pub enum LineFinished {
    /// No line ending has yet been encountered.
    Unfinished {
        /// If Some, an ANSI Line Clear was found during parsing,
        /// containing the index after the Line Clear command (as any bytes before it can be ignored for rendering purposes),
        /// and the active Style at that point.
        ///
        /// If another is found during later consumptions, the later index and unterminated style is used.
        clear_occurred: Option<(usize, Style)>,
    },
    /// Line was finished! Contains escaped line ending.
    LineEnding(CompactString),
    /// Before a line ending could be found, the user sent a line that was visible, and thus
    /// cut this line short.
    CutShort,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LineType {
    /// Text from serial port
    Port(LineFinished),
    /// Color Rules have omitted this line's contents entirely.
    PortHidden(LineFinished),
    User {
        is_bytes: bool,
        #[cfg(feature = "macros")]
        is_macro: bool,
        escaped_line_ending: Option<CompactString>,
        /// Complete byte sequence that was sent to port, including line ending.
        reloggable_raw: Vec<u8>,
    },
    #[cfg(feature = "defmt")]
    PortDefmt {
        level: Option<defmt_parser::Level>,
        location: Option<FrameLocation>,
        /// Timestamp generated using a format string and device's local time-since-boot.
        device_timestamp: Option<CompactString>,
    },
}

impl LineType {
    pub fn line_finished(&self) -> Option<&LineFinished> {
        match self {
            LineType::Port(l) | LineType::PortHidden(l) => Some(l),
            _ => None,
        }
    }
}

#[cfg(feature = "defmt")]
#[derive(Debug, Clone, PartialEq, Eq)]
/// defmt log location in source files
pub struct FrameLocation {
    // Original type is u64 but I'm not storing that.
    line: u32,
    module: CompactString,
    file: CompactString,
}

#[cfg(feature = "defmt")]
impl From<&defmt_decoder::Location> for FrameLocation {
    fn from(value: &defmt_decoder::Location) -> Self {
        use compact_str::ToCompactString;

        Self {
            line: value.line.try_into().expect("line larger than u32::MAX??"),
            module: value.module.to_compact_string(),
            file: value.file.display().to_compact_string(),
        }
    }
}

impl LineType {
    pub(super) fn is_bytes(&self) -> bool {
        match *self {
            LineType::User { is_bytes, .. } => is_bytes,
            _ => false,
        }
    }

    #[cfg(feature = "macros")]
    pub(super) fn is_macro(&self) -> bool {
        match *self {
            LineType::User { is_macro, .. } => is_macro,
            _ => false,
        }
    }
}

/// Helper struct to lower function parameter count :sweat_smile:
pub struct BufLineKit<'a> {
    /// Original slice (containing line ending) and it's range in RawBuffer.
    pub full_range_slice: RangeSlice<'a>,
    /// Timestamp of slice's arrival from port
    pub timestamp: DateTime<Local>,
    /// `last_terminal_size`'s width field copied in
    pub area_width: u16,
    /// References to Rendering setting and defmt Settings
    pub render: RenderSettings<'a>,
}

// Many changes needed, esp. in regards to current app-state things (index, width, color, showing timestamp)
impl BufLine {
    fn new_inner(line: Line<'static>, kit: BufLineKit, line_type: LineType) -> Self {
        let timestamp = kit.timestamp;

        let mut bufline = Self {
            // timestamp_str: timestamp.format(TIME_FORMAT).to_compact_string(),
            timestamp,
            range_in_raw_buffer: kit.full_range_slice.range,
            value: line,
            rendered_line_height: 0,
            line_type,
        };
        // bufline.populate_line_ending(raw_value, line_ending);
        bufline.update_line_height(kit.area_width, kit.render);
        bufline
    }
    /// Normal case, taking in an `ansi-to-tui`-parsed Line and converting to a BufLine
    pub fn port_text_line(
        line: Line<'static>,
        kit: BufLineKit,
        clear_info: Option<(usize, Style)>,
        line_ending: &LineEnding,
    ) -> Self {
        let line_type = LineType::Port(
            // Checking if the line's backing slice was terminated with the current line ending.
            if let Some(escaped_line_ending) = line_ending.escaped_from(kit.full_range_slice.slice)
            {
                LineFinished::LineEnding(escaped_line_ending)
            } else {
                LineFinished::Unfinished {
                    clear_occurred: clear_info,
                }
            },
        );

        Self::new_inner(line, kit, line_type)
    }
    /// Create a port line with no internal content but has a valid line ending.
    ///
    /// Thus hollow, not exactly empty, but not filled with content either.
    pub fn hollow_port_line(kit: BufLineKit, line_ending: &LineEnding) -> Self {
        let escaped_line_ending = line_ending.as_escaped().expect(
            "should only be called when the line's content is **only** a non-empty line ending",
        );
        let line_type = LineType::Port(LineFinished::LineEnding(escaped_line_ending));

        Self::new_inner(Line::default(), kit, line_type)
    }
    /// Create a port line in the case where all contents were _omitted_ by the user's color rules
    /// (censoring _replaces_ contents, won't _omit_ them).
    pub fn hidden_content_port_line(kit: BufLineKit, line_ending: &LineEnding) -> Self {
        let span = Span::styled(
            "[All content was omitted by color rules.]",
            Style::new().dark_gray(),
        );

        let line_type = LineType::PortHidden(
            if let Some(escaped_line_ending) = line_ending.escaped_from(kit.full_range_slice.slice)
            {
                LineFinished::LineEnding(escaped_line_ending)
            } else {
                LineFinished::Unfinished {
                    clear_occurred: None,
                }
            },
        );

        Self::new_inner(span.into(), kit, line_type)
    }
    #[cfg(feature = "defmt")]
    /// Creating port line from post-processed defmt message text and frame info.
    pub fn port_defmt_line(
        line: Line<'static>,
        kit: BufLineKit,
        level: Option<defmt_parser::Level>,
        device_timestamp: Option<&dyn std::fmt::Display>,
        location: Option<FrameLocation>,
    ) -> Self {
        let line_type = LineType::PortDefmt {
            level,
            device_timestamp: device_timestamp.map(|ts| format_compact!("[{ts}] ")),
            location,
        };

        Self::new_inner(line, kit, line_type)
    }
    #[cfg(feature = "defmt")]
    pub fn hidden_content_port_defmt_line(
        kit: BufLineKit,
        level: Option<defmt_parser::Level>,
        device_timestamp: Option<&dyn std::fmt::Display>,
        location: Option<FrameLocation>,
    ) -> Self {
        let span = Span::styled(
            "[All content was omitted by color rules.]",
            Style::new().dark_gray(),
        );
        let line_type = LineType::PortDefmt {
            level,
            device_timestamp: device_timestamp.map(|ts| format_compact!("[{ts}] ")),
            location,
        };

        Self::new_inner(span.into(), kit, line_type)
    }
    pub fn user_line(
        line: Line<'static>,
        kit: BufLineKit,
        tx_line_ending: &LineEnding,
        is_bytes: bool,
        #[cfg(feature = "macros")] is_macro: bool,
        reloggable_raw: Vec<u8>,
    ) -> Self {
        let line_type = LineType::User {
            is_bytes,
            #[cfg(feature = "macros")]
            is_macro,
            escaped_line_ending: tx_line_ending.escaped_from(&reloggable_raw),
            reloggable_raw,
        };

        Self::new_inner(line, kit, line_type)
    }

    pub fn replace_contents_with(&mut self, mut new: BufLine) {
        assert!(matches!(
            self.line_type,
            LineType::Port(LineFinished::Unfinished { .. })
                | LineType::PortHidden(LineFinished::Unfinished { .. })
        ));

        new.timestamp = self.timestamp;

        *self = new;
    }

    /// Determine and cache how many vertical lines this BufLine would take to show fully on screen.
    pub fn update_line_height(&mut self, terminal_width: u16, rendering: RenderSettings) -> usize {
        // Acting as if the scrollbar is always visible, since otherwise it appearing would require
        // redoing the line height check again.
        let width_minus_scrollbar = terminal_width.saturating_sub(1);

        let para = Paragraph::new(self.as_line(rendering)).wrap(Wrap { trim: false });
        // Paragraph::line_count comes from an unstable ratatui feature (unstable-rendered-line-info)
        // which may be changed/removed in the future. If so, I'll need to roll my own wrapping/find someone's to steal.
        let height = para.line_count(width_minus_scrollbar);
        self.rendered_line_height = height as u16;
        height
    }

    pub fn get_line_height(&self) -> u16 {
        self.rendered_line_height
    }

    /// Returns an owned `ratatui::Line` that borrows from the BufLine's actual text spans,
    /// and appending optional Spans depending on line type and user's rendering/defmt settings.
    pub fn as_line(&self, rendering: RenderSettings) -> Line<'_> {
        let borrowed_spans = self.value.borrowed_spans_iter();

        let dark_gray = Style::new().dark_gray();

        let indices_and_len = std::iter::once(&self.line_type)
            .filter_map(|lt| match lt {
                _ if !rendering.rendering.show_indices => None,
                LineType::User { reloggable_raw, .. } => Some(make_user_index_info(
                    self.range(),
                    reloggable_raw.len(),
                    rendering.rendering.indices_as_hex,
                )),
                _ => Some(make_index_info(
                    self.range(),
                    rendering.rendering.indices_as_hex,
                )),
            })
            .map(|i| Span::styled(i, dark_gray));

        let timestamp = rendering
            .rendering
            .timestamps
            .then(|| Span::styled(self.timestamp.format(TIME_FORMAT).to_string(), dark_gray))
            .into_iter();

        #[cfg(feature = "defmt")]
        let defmt_device_timestamp = std::iter::once(&self.line_type).filter_map(|lt| match lt {
            _ if !rendering.defmt.device_timestamp => None,
            LineType::PortDefmt {
                device_timestamp: Some(device_timestamp),
                ..
            } => Some(Span::styled(device_timestamp, dark_gray)),
            _ => None,
        });

        #[cfg(feature = "defmt")]
        let defmt_level = std::iter::once(&self.line_type)
            .filter_map(|lt| match lt {
                LineType::PortDefmt { level, .. } => {
                    Some(super::tui::defmt::defmt_level_bracketed(*level))
                }
                _ => None,
            })
            .flatten();

        #[cfg(feature = "defmt")]
        fn shorten_module_path(full_module_path: &str) -> &str {
            full_module_path
                .split("::")
                .last()
                .unwrap_or(full_module_path)
        }

        #[cfg(feature = "defmt")]
        fn shorten_file_path(full_file_path: &str) -> &str {
            full_file_path
                .split(&['/', '\\'])
                .next_back()
                .unwrap_or(full_file_path)
        }

        #[cfg(feature = "defmt")]
        let defmt_location = std::iter::once(&self.line_type).filter_map(|lt| match lt {
            LineType::PortDefmt {
                location:
                    Some(FrameLocation {
                        line: defmt_line_num,
                        module: defmt_module,
                        file: defmt_file,
                    }),
                ..
            } => {
                use crate::settings::DefmtLocation;

                let RenderSettings { defmt, .. } = rendering;

                let module = &defmt.show_module;
                let file = &defmt.show_file;
                let line_num = defmt.show_line_number;

                #[allow(clippy::single_match)]
                match (module, file, line_num) {
                    (DefmtLocation::Hidden, DefmtLocation::Hidden, false) => return None,
                    _ => (),
                };

                let module_file_separator =
                    if !module.is_hidden() && (!file.is_hidden() || line_num) {
                        " @ "
                    } else {
                        ""
                    };
                let file_line_separator = if !file.is_hidden() && line_num {
                    ":"
                } else {
                    ""
                };

                let module = match module {
                    DefmtLocation::Hidden => "",
                    DefmtLocation::Shortened => shorten_module_path(defmt_module),
                    DefmtLocation::Full => defmt_module,
                };
                let file = match file {
                    DefmtLocation::Hidden => "",
                    DefmtLocation::Shortened => shorten_file_path(defmt_file),
                    DefmtLocation::Full => defmt_file,
                };
                let line_num = if line_num {
                    Cow::Owned(defmt_line_num.to_string())
                } else {
                    Cow::Borrowed("")
                };

                Some(Span::styled(
                    format!(
                        " {module}{module_file_separator}{file}{file_line_separator}{line_num}"
                    ),
                    Style::new().dark_gray(),
                ))
            }
            _ => None,
        });

        let line_ending = std::iter::once(&self.line_type).filter_map(|lt| match lt {
            _ if !rendering.rendering.show_line_ending => None,

            LineType::Port(LineFinished::LineEnding(line_ending)) => {
                Some(Span::styled(Cow::Borrowed(line_ending.as_str()), dark_gray))
            }
            LineType::PortHidden(LineFinished::LineEnding(line_ending)) => {
                Some(Span::styled(Cow::Borrowed(line_ending.as_str()), dark_gray))
            }

            LineType::Port(LineFinished::CutShort) => None,
            LineType::Port(LineFinished::Unfinished { .. }) => None,
            LineType::PortHidden(LineFinished::CutShort) => None,
            LineType::PortHidden(LineFinished::Unfinished { .. }) => None,

            LineType::User {
                escaped_line_ending: Some(line_ending),
                ..
            } => Some(Span::styled(Cow::Borrowed(line_ending.as_str()), dark_gray)),

            LineType::User {
                escaped_line_ending: None,
                ..
            } => None,

            #[cfg(feature = "defmt")]
            LineType::PortDefmt { .. } => None,
        });

        // A little silly but it works.

        let spans = timestamp;

        #[cfg(feature = "defmt")]
        let spans = spans.chain(defmt_device_timestamp);

        let spans = spans.chain(indices_and_len);

        #[cfg(feature = "defmt")]
        let spans = spans.chain(defmt_level);

        let spans = spans.chain(borrowed_spans).chain(line_ending);

        #[cfg(feature = "defmt")]
        let spans = spans.chain(defmt_location);

        Line::from_iter(spans)
    }

    pub fn range(&self) -> &Range<usize> {
        &self.range_in_raw_buffer
    }
}

fn make_index_info(range: &Range<usize>, hex: bool) -> CompactString {
    let start = range.start;
    let end = range.end;
    let len = end - start;

    if hex {
        format_compact!("({start:#08X}..{end:#08X}, {len:#4X}) ")
    } else {
        format_compact!("({start:06}..{end:06}, {len:3}) ")
    }
}

fn make_user_index_info(range: &Range<usize>, len: usize, hex: bool) -> CompactString {
    let start = range.start;

    if hex {
        format_compact!("({start:#08X}=={start:#08X}, {len:#4X}) ")
    } else {
        format_compact!("({start:06}=={start:06}, {len:3}) ")
    }
}
