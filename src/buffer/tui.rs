#![allow(clippy::mut_range_bound)]
// Due to the `breaks` in determine_bytes_per_line not being
// seen by the lint, causing a false-positive.
// In addition, placing the allow directive inside the function
// was't silencing it, so I'm making it file-wide.

use std::cmp::Ordering;

use itertools::{Either, Itertools};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, Wrap},
};
use ratatui_macros::{horizontal, vertical};

use crate::{
    buffer::{LineType, buf_line::RenderSettings},
    config_adjacent_path,
    settings::HexHighlightStyle,
    traits::{ToggleBool, interleave_by},
    tui::color_rules::{COLOR_RULES_PATH, ColorRuleLoadError, ColorRules},
};

use super::{Buffer, UserEcho, buf_line::BufLine, hex_spans::*};

impl Buffer {
    /// Updates each BufLine's render height with the new terminal width, returning the sum total at the end
    pub fn update_wrapped_line_heights(&mut self) -> usize {
        self.styled_lines.rx.iter_mut().fold(0, |total, l| {
            let render_settings = RenderSettings {
                rendering: &self.rendering,
                #[cfg(feature = "defmt")]
                defmt: &self.defmt_settings,
            };
            let new_height = l.update_line_height(self.last_terminal_size.width, render_settings);

            total + new_height
        }) + self.styled_lines.tx.iter_mut().fold(0, |total, l| {
            let render_settings = RenderSettings {
                rendering: &self.rendering,
                #[cfg(feature = "defmt")]
                defmt: &self.defmt_settings,
            };
            let new_height = l.update_line_height(self.last_terminal_size.width, render_settings);

            total + new_height
        })
    }
    // #[cfg(feature = "defmt")]
    // fn rx_lines_iter(&self) -> impl Iterator<Item = &BufLine> {
    //     self.styled_lines.rx.iter().filter(|b| match b.line_type {
    //         super::LineType::PortDefmt {
    //             level: Some(level), ..
    //         } => self.defmt_settings.max_log_level <= crate::settings::Level::from(level),
    //         _ => true,
    //     })
    // }
    // #[cfg(not(feature = "defmt"))]
    // fn rx_lines_iter(&self) -> impl Iterator<Item = &BufLine> {
    //     self.styled_lines.rx.iter()
    // }
    fn buflines_iter(&self) -> impl Iterator<Item = &BufLine> {
        let rx_iter = self.styled_lines.rx.iter().filter(|bf| match bf.line_type {
            LineType::PortHidden(_) => self.rendering.show_hidden_lines,
            #[cfg(feature = "defmt")]
            LineType::PortDefmt {
                level: Some(level), ..
            } => self.defmt_settings.max_log_level <= crate::settings::Level::from(level),
            _ => true,
        });
        if self.rendering.echo_user_input == UserEcho::None {
            Either::Left(rx_iter)
        } else {
            Either::Right(interleave_by(
                rx_iter,
                self.styled_lines.tx.iter().filter(|l| {
                    self.rendering
                        .echo_user_input
                        .filter_user_line(&l.line_type)
                }),
                |port, user| match port.range().start.cmp(&user.range().start) {
                    Ordering::Equal => port.timestamp <= user.timestamp,
                    Ordering::Less => true,
                    Ordering::Greater => false,
                },
            ))
        }
    }
    pub fn lines_iter(&self) -> (impl Iterator<Item = Line<'_>>, u16) {
        let (buflines, wrapped_scroll) = self.visible_buflines_iter();
        (
            buflines.map(|l| l.as_line(self.line_render_settings())),
            wrapped_scroll,
        )
    }

    fn visible_buflines_iter(&self) -> (impl Iterator<Item = &BufLine>, u16) {
        let last_size = &self.last_terminal_size;
        let area_height = last_size.height as usize;

        let more_lines_than_height =
            (area_height < self.styled_lines.rx.len()) || (area_height < self.combined_height());

        let entries_to_skip: usize;
        let entries_to_take: usize;

        let mut wrapped_scroll: u16 = 0;

        if more_lines_than_height {
            let desired_visible_lines = area_height;
            if self.rendering.wrap_text {
                let vert_scroll = self.state.vert_scroll;
                let (spillover_index, _spillover_lines_visible, spilt_line_total_height) = {
                    let mut current_line_index: usize = 0;
                    let mut current_line_height: usize = 0;

                    let mut lines_from_top: usize = 0;
                    for (index, entries_lines) in self
                        .buflines_iter()
                        .map(|l| l.get_line_height())
                        .enumerate()
                    {
                        current_line_index = index;
                        current_line_height = entries_lines as usize;

                        lines_from_top += entries_lines as usize;
                        if lines_from_top > vert_scroll {
                            break;
                        }
                    }

                    // Hmm, this crashed with an overflow when resizing once.
                    // TODO, ensure scrolls are valid for size calcing? Unsure.
                    let visible_lines = lines_from_top.saturating_sub(vert_scroll);

                    let spillover_lines = if current_line_height == visible_lines {
                        // If we can see all of the lines of this entry, then it's not spilling over
                        0
                    } else {
                        wrapped_scroll = (current_line_height - visible_lines) as u16;
                        // Returns how many lines are visibly spilling over from the
                        // entry being cropped by the top of the buffer window.
                        visible_lines
                    };

                    (current_line_index, spillover_lines, current_line_height)
                };

                // debug!("scroll: {vert_scroll}, index: {spillover_index}, spillover lines: {spillover_lines_visible}, wrapped scroll: {wrapped_scroll}");

                entries_to_skip = spillover_index;
                entries_to_take = {
                    let mut visible_lines: isize = -(spilt_line_total_height as isize);
                    let mut entries_to_take = 0;

                    for entry_lines in self
                        .buflines_iter()
                        .skip(entries_to_skip)
                        .map(|l| l.get_line_height())
                    {
                        entries_to_take += 1;
                        visible_lines += entry_lines as isize;

                        if visible_lines > desired_visible_lines as isize {
                            // debug!(
                            //     "visible_lines: {visible_lines}, desired: {desired_visible_lines}"
                            // );
                            break;
                        }
                    }

                    // debug!(
                    //     "entries_to_skip: {entries_to_skip}, entries_to_take: {entries_to_take}"
                    // );

                    entries_to_take
                };
            } else {
                entries_to_skip = self.state.vert_scroll;
                // self.lines.len() - last_size.height as usize;
                entries_to_take = desired_visible_lines;
            }
        } else {
            entries_to_skip = 0;
            entries_to_take = usize::MAX;
        }

        (
            self.buflines_iter()
                .skip(entries_to_skip)
                .take(entries_to_take),
            wrapped_scroll,
        )
    }

    pub fn terminal_paragraph(&self) -> Paragraph<'_> {
        let (lines_iter, vert_scroll) = self.lines_iter();
        let lines: Vec<_> = lines_iter.collect();
        let para = Paragraph::new(lines)
            .block(Block::new().borders(Borders::RIGHT))
            .scroll((vert_scroll, 0));
        if self.rendering.wrap_text {
            para.wrap(Wrap { trim: false })
        } else {
            para
        }
    }
    pub fn scroll_page_up(&mut self) {
        let amount = self.last_terminal_size.height - 2;
        self.scroll_by(amount as i32);
    }

    pub fn scroll_page_down(&mut self) {
        let amount = self.last_terminal_size.height - 2;
        let amount = -(amount as i32);
        self.scroll_by(amount);
    }

    pub fn scroll_by(&mut self, up: i32) {
        let total_lines = self.combined_height();

        match up {
            0 => (), // Used to trigger scroll update actions from non-user scrolling events.
            // Scroll all the way up
            i32::MAX => {
                self.state.vert_scroll = 0;
                self.state.stuck_to_bottom = false;
            }
            // Scroll all the way down
            i32::MIN => self.state.vert_scroll = total_lines,

            // Scroll up
            x if up > 0 => {
                self.state.vert_scroll = self.state.vert_scroll.saturating_sub(x as usize);
            }
            // Scroll down
            x if up < 0 => {
                self.state.vert_scroll = self
                    .state
                    .vert_scroll
                    .saturating_add(x.unsigned_abs() as usize);
            }
            _ => unreachable!("all cases handled"),
        }

        let last_size = &self.last_terminal_size;
        let more_lines_than_height = total_lines > last_size.height as usize;

        if up > 0 && more_lines_than_height {
            self.state.stuck_to_bottom = false;
        } else if self.state.vert_scroll + last_size.height as usize >= total_lines {
            self.state.vert_scroll = total_lines;
            self.state.stuck_to_bottom = true;
        }

        if self.state.stuck_to_bottom {
            let new_pos = total_lines.saturating_sub(last_size.height as usize);
            self.state.vert_scroll = new_pos;
        }
        self.state.scrollbar_state = self
            .state
            .scrollbar_state
            .position(self.state.vert_scroll)
            .content_length(total_lines.saturating_sub(last_size.height as usize));
    }
    // fn wrapped_line_count(&self) -> usize {
    //     self.buflines_iter().map(|l| l.get_line_height()).sum()
    // }

    /// Returns the total amount of lines that can be rendered,
    /// taking into account if text wrapping is enabled or not.
    pub fn combined_height(&self) -> usize {
        if let Some(cached) = self.cached_combined_height.get() {
            return cached;
        }
        if self.raw.inner.is_empty() {
            return 0;
        }
        let result = if !self.rendering.hex_view {
            if self.rendering.wrap_text {
                self.buflines_iter()
                    .map(|l| l.get_line_height() as usize)
                    .sum()
            } else {
                // self.buflines_iter().count()
                //
                // inlining some of its functionality instead
                // to avoid .count()-ing a potentially huge iter (100k+ items)
                // (especially when wrapping is disabled, when its even more dead simple)
                // when we can just read the length of a vector
                let can_use_vector_length =
                    { !self.rendering.wrap_text && self.rendering.show_hidden_lines };
                #[cfg(feature = "defmt")]
                let can_use_vector_length = {
                    can_use_vector_length
                        && self.defmt_settings.max_log_level == crate::settings::Level::Trace
                };
                let rx_lines = if can_use_vector_length {
                    self.styled_lines.rx.len()
                } else {
                    self.buflines_iter().count()
                };
                if self.rendering.echo_user_input == UserEcho::None {
                    rx_lines
                } else {
                    rx_lines
                        + self
                            .styled_lines
                            .tx
                            .iter()
                            .filter(|l| {
                                self.rendering
                                    .echo_user_input
                                    .filter_user_line(&l.line_type)
                            })
                            .count()
                }
            }
        } else {
            let header_margin = { if self.rendering.hex_view_header { 2 } else { 0 } };
            (self.raw.inner.len() as f64 / (self.state.hex_bytes_per_line as f64)).ceil() as usize
                + header_margin
        };

        self.cached_combined_height.set(Some(result));
        result
    }

    pub fn invalidate_height_cache(&self) {
        self.cached_combined_height.set(None);
    }

    pub fn port_lines_len(&self) -> usize {
        self.styled_lines.rx.len()
    }

    pub fn update_terminal_size<B>(
        &mut self,
        terminal: &mut ratatui::Terminal<B>,
    ) -> std::io::Result<()>
    where
        B: Backend,
        <B as ratatui::backend::Backend>::Error: Sync + Send + 'static,
        std::io::Error: From<<B as ratatui::backend::Backend>::Error>,
    {
        self.last_terminal_size = {
            let mut terminal_size = terminal.size()?;
            // `2` is the lines from the repeating_pattern_widget and the input buffer.
            // Might need to make more dynamic later?
            terminal_size.height = terminal_size.height.saturating_sub(2);
            terminal_size
        };
        self.update_wrapped_line_heights();
        self.determine_bytes_per_line(self.rendering.bytes_per_line.into());
        self.scroll_by(0);
        Ok(())
    }

    pub fn reload_color_rules(&mut self) -> Result<(), ColorRuleLoadError> {
        self.color_rules = ColorRules::load_from_file(config_adjacent_path(COLOR_RULES_PATH))?;
        self.reconsume_raw_buffer();
        Ok(())
    }

    pub fn correct_hex_view_scroll(&mut self) {
        self.state.stuck_to_bottom = false;
        if self.raw.inner.is_empty() {
            return;
        }
        let transitioning_from = !self.rendering.hex_view;
        match transitioning_from {
            true => {
                // get the raw buffer index we're topping at
                let closest_index = self.state.vert_scroll * self.state.hex_bytes_per_line as usize;
                let closest_line = self.styled_lines.rx.windows(2).find_position(|w| {
                    let [l, r] = w else { unreachable!() };
                    l.range().start <= closest_index && closest_index <= r.range().start
                });
                if let Some((index, _)) = closest_line {
                    let lines_up_to: usize = self
                        .styled_lines
                        .rx
                        .iter()
                        .map(|l| l.get_line_height() as usize)
                        .take(index)
                        .sum();
                    let scroll =
                        lines_up_to
                            // .saturating_sub(self.last_terminal_size.height as usize)
                        ;
                    self.state.vert_scroll = scroll;
                } else {
                    self.scroll_by(i32::MIN);
                }
            }
            false => {
                let Some(top_visible) = self.visible_buflines_iter().0.next() else {
                    return;
                };

                self.state.vert_scroll = (top_visible.range().start as f64
                    / self.state.hex_bytes_per_line as f64)
                    .floor() as usize
                    // .saturating_sub(self.last_terminal_size.height as usize)
                ;
            }
        }
    }
    pub fn line_render_settings(&self) -> RenderSettings<'_> {
        RenderSettings {
            rendering: &self.rendering,
            #[cfg(feature = "defmt")]
            defmt: &self.defmt_settings,
        }
    }
}

impl Buffer {
    fn buffer_hex_digits(&self) -> u16 {
        hex_digits(self.raw.inner.len()).max(8) as u16
    }
    pub(super) fn determine_bytes_per_line(&mut self, max_bytes: Option<u8>) {
        let buffer_hex_digits = self.buffer_hex_digits();

        let optional_spacing = 2;
        let line_width = 1;

        let (bytes_per_line, hex_area_width) = {
            let mut hex_area_width = 0;

            let mut remaining_width = self.last_terminal_size.width
                .saturating_sub(optional_spacing)
                .saturating_sub(buffer_hex_digits)
                .saturating_sub(line_width)
                .saturating_sub(optional_spacing)
                .saturating_sub(line_width) // line after hex
                .saturating_sub(1) // scrollbar
            ;
            if remaining_width == 0 {
                return;
            }

            let mut bytes_per_line = 0;
            for _free_cell in 0..remaining_width {
                bytes_per_line += 1;
                hex_area_width += 2; // byte as str
                remaining_width = remaining_width
                    .saturating_sub(1) // ASCII cell
                    .saturating_sub(2) // byte as str
                ;
                if let Some(max_per_line) = max_bytes
                    && bytes_per_line == max_per_line
                {
                    break;
                }
                if remaining_width <= 3 {
                    break;
                }
                hex_area_width += 1; // space between bytes
                remaining_width = remaining_width
                    .saturating_sub(1) // space between bytes
                ;

                if bytes_per_line != 0 && bytes_per_line.is_multiple_of(8) {
                    if remaining_width < 4 {
                        break;
                    }
                    hex_area_width += 1; // extra space
                    remaining_width = remaining_width
                        .saturating_sub(1) // extra space
                    ;
                }

                if remaining_width < 3 {
                    break;
                }
            }
            (bytes_per_line, hex_area_width)
        };

        self.state.hex_bytes_per_line = bytes_per_line;
        self.state.hex_section_width = hex_area_width;
    }

    pub fn render_hex(&mut self, area: Rect, buf: &mut ratatui::prelude::Buffer) {
        let [labels, sep_line_area, hex_area] = if self.rendering.hex_view_header {
            vertical![==1, ==1, *=1].areas(area)
        } else {
            [Rect::default(), Rect::default(), area]
        };

        // let [_, _hex_sep, _, _dunno] = horizontal![==13, *=3, *=1, *=1].areas(sep_line_area);

        let buffer_hex_digits = self.buffer_hex_digits();
        let optional_spacing = 2;
        let line_width = 1;
        let bytes_per_line = self.state.hex_bytes_per_line;
        let hex_area_width = self.state.hex_section_width;

        let [
            addresses_area,
            line_1_area,
            _opt_spacing_1,
            hex_area,
            _opt_spacing_2,
            line_2_area,
            ascii_area,
            mut scrollbar_area
        ] = horizontal![==buffer_hex_digits, ==line_width, ==optional_spacing,==hex_area_width, ==optional_spacing, ==line_width, ==bytes_per_line as u16, ==1]
            .areas(hex_area);

        let [offset_text,
            _,
            _,
            hex_text,
            _,
            _,
            ascii_text,
            _,
        ] =
            horizontal![==buffer_hex_digits, ==line_width, ==optional_spacing, ==hex_area_width, ==optional_spacing, ==line_width, ==bytes_per_line as u16, ==1]
                .areas(labels);

        if self.rendering.hex_view_header {
            Line::raw("Offset").centered().render(offset_text, buf);
            byte_markers(bytes_per_line, hex_text, buf);
            // Line::raw("Incoming Data:").render(hex_text, buf);
            Line::raw("ASCII").centered().render(ascii_text, buf);
            Block::new()
                .borders(Borders::TOP)
                .border_style(Style::new().dark_gray())
                .render(sep_line_area, buf);
        }
        scrollbar_area.height = area.height;
        scrollbar_area.y = 0;

        let vert_block = Block::new()
            .borders(Borders::LEFT)
            .border_style(Style::new().dark_gray());

        let vert_line = &vert_block;

        let vert_area = Rect {
            height: area.height,
            width: 1,
            x: 0,
            y: 0,
        };

        vert_line.render(
            Rect {
                x: line_1_area.x,
                ..vert_area
            },
            buf,
        );

        vert_line.render(
            Rect {
                x: line_2_area.x,
                ..vert_area
            },
            buf,
        );

        if self.combined_height() > hex_area.height as usize {
            let vert_block = Block::new()
                .borders(Borders::LEFT)
                .border_style(Style::reset());

            vert_block.render(
                Rect {
                    x: scrollbar_area.x,
                    ..vert_area
                },
                buf,
            );
        } else {
            vert_line.render(
                Rect {
                    x: scrollbar_area.x,
                    ..vert_area
                },
                buf,
            );
            if scrollbar_area.x > 0 && sep_line_area.y > 0 {
                buf.set_string(
                    scrollbar_area.x,
                    sep_line_area.y,
                    symbols::line::CROSS,
                    Style::new().dark_gray(),
                );
            }
        }

        if line_1_area.x > 0 && sep_line_area.y > 0 {
            buf.set_string(
                line_1_area.x,
                sep_line_area.y,
                symbols::line::CROSS,
                Style::new().dark_gray(),
            );
        }

        if line_2_area.x > 0 && sep_line_area.y > 0 {
            buf.set_string(
                line_2_area.x,
                sep_line_area.y,
                symbols::line::CROSS,
                Style::new().dark_gray(),
            );
        }

        render_offsets(
            &self.raw.inner,
            bytes_per_line,
            self.state.vert_scroll,
            addresses_area,
            buf,
        );

        render_bytes(
            &self.raw.inner,
            bytes_per_line,
            self.state.vert_scroll,
            self.rendering.hex_view_highlights,
            hex_area,
            buf,
        );

        render_ascii(
            &self.raw.inner,
            bytes_per_line,
            self.state.vert_scroll,
            self.rendering.hex_view_highlights,
            ascii_area,
            buf,
        );

        if !self.state.stuck_to_bottom {
            let scroll_notice = Line::raw("More... Shift+PgDn to jump to newest").dark_gray();
            let notice_area = {
                let mut rect = area;
                rect.y = rect.bottom().saturating_sub(1);
                rect.height = 1;
                rect
            };
            Clear.render(notice_area, buf);
            scroll_notice.render(notice_area, buf);
        }

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"));
        scrollbar.render(scrollbar_area, buf, &mut self.state.scrollbar_state);
    }
}
struct AlternatingStyles(bool, Style, Style);

impl From<AlternatingStyles> for Style {
    fn from(value: AlternatingStyles) -> Self {
        match value.0 {
            true => value.1,
            false => value.2,
        }
    }
}

#[inline]
fn byte_color_ascii(byte: u8, colors: AlternatingStyles) -> Style {
    match byte {
        // newlines
        b'\n' | b'\r' => Style::new().black().on_light_green(),
        // control chars
        0x1..=0x1F | 0x7F => Style::new().black().on_light_blue(),
        // digit
        0x30..=0x39 => Style::new().red(),
        // brackets/braces/parenthenses
        0x28 | 0x29 | 0x3C | 0x3E | 0x5B | 0x5D | 0x7B | 0x7D => Style::new().yellow(),
        // punctuation
        0x21..=0x2F | 0x3A..=0x40 | 0x5B..=0x60 | 0x7B..=0x7E => Style::new().light_yellow(),
        // actual text
        0x41..=0x5A | 0x61..=0x7A => {
            if colors.0 {
                colors.1
            } else {
                colors.2
            }
        }
        b' ' => Style::new().green(),

        _ => Style::new().dark_gray(),
    }
}

fn style_select(
    byte: u8,
    fallback_alternating_styles: AlternatingStyles,
    highlight_config: HexHighlightStyle,
) -> Style {
    match highlight_config {
        HexHighlightStyle::None => fallback_alternating_styles.into(),
        HexHighlightStyle::DarkenNulls => match byte {
            0x00 => Style::new().dark_gray(),
            _ => fallback_alternating_styles.into(),
        },
        HexHighlightStyle::HighlightAsciiSymbols => {
            byte_color_ascii(byte, fallback_alternating_styles)
        }
        HexHighlightStyle::StyleA => match byte {
            0x00 => Style::new().dark_gray(),
            0x01..=0x1F => Style::new().blue(),
            0x20..=0x3F => Style::new().red(),
            0x40..=0x5F => Style::new().green(),
            0x60..=0x7F => Style::new().cyan(),
            0x80..=0x9F => Style::new().yellow(),
            0xA0..=0xBF => Style::new().light_blue(),
            0xC0..=0xDF => Style::new().light_red(),
            0xE0..=0xFF => Style::new().light_green(),
        },
        HexHighlightStyle::StyleB => match byte {
            0x00 => Style::new().dark_gray(),
            0x01..=0x4F => Style::new().green(),
            0x50..=0x7F => Style::new().light_blue(),
            0x80..=0xCF => Style::new().magenta(),
            _ => Style::new().yellow(),
        },
    }
}
fn render_ascii(
    slice: &[u8],
    bytes_per_line: u8,
    scroll: usize,
    hex_highlight_style: HexHighlightStyle,
    area: Rect,
    buf: &mut ratatui::prelude::Buffer,
) {
    // This function renders the ASCII representation of the byte lines in the hex viewer.
    //
    // Each line only shows printable ASCII characters (0x20..=0x7E), other bytes are shown as '.'.
    // - slice: the bytes to render
    // - bytes_per_line: number of bytes per row
    // - scroll: starting row index (for scrolling/paging)
    // - area: ratatui area to render in
    // - buf: output buffer

    let total_lines = area.height.min(
        ((slice
            .len()
            .saturating_sub(scroll * (bytes_per_line as usize))) as f64
            / (bytes_per_line as f64))
            .ceil() as u16,
    );

    let mut style_bool = !scroll.is_multiple_of(2);

    for line in 0..total_lines {
        style_bool.flip();
        let offset = (scroll + line as usize) * (bytes_per_line as usize);
        let line_bytes = &slice[offset..slice.len().min(offset + bytes_per_line as usize)];

        let mut ascii_spans = Vec::with_capacity(bytes_per_line as usize);
        for &byte in line_bytes {
            let span_style = style_select(
                byte,
                AlternatingStyles(style_bool, Style::new().white(), Style::new().gray()),
                hex_highlight_style,
            );
            let ch = match (byte, hex_highlight_style) {
                (0x20, HexHighlightStyle::HighlightAsciiSymbols) => Span::styled("_", span_style),
                (0x20..=0x7E, _) => {
                    Span::styled(ASCII_PRINTABLE[(byte - 0x20) as usize], span_style)
                }
                _ => Span::styled(".", Style::new().dark_gray()),
            };

            ascii_spans.push(ch);
        }
        // Pad to full width if line is short
        for _ in line_bytes.len()..(bytes_per_line as usize) {
            ascii_spans.push(Span::raw(" "));
        }

        let row_rect = Rect {
            x: area.x,
            y: area.y + line,
            width: area.width,
            height: 1,
        };
        Line::from(ascii_spans).render(row_rect, buf);
    }
}

fn byte_markers(bytes_per_line: u8, area: Rect, buf: &mut ratatui::prelude::Buffer) {
    // This function renders a row of hex offset values as byte markers for the columns in the hex view.
    //
    // Example for bytes_per_line = 16:
    //   00 01 02 03 04 05 06 07  08 09 0A 0B 0C 0D 0E 0F
    //
    // 'area' should be a single line tall & wide enough for the column.

    let mut spans = Vec::with_capacity(bytes_per_line as usize * 2 - 1);
    for i in 0..bytes_per_line {
        if i != 0 && i.is_multiple_of(8) {
            // Extra space between 8th and 9th column for readability
            spans.push(ratatui::text::Span::raw("  "));
        } else if i != 0 {
            spans.push(ratatui::text::Span::raw(" "));
        }

        spans.push(ratatui::text::Span::styled(
            format!("{i:02X}"),
            Style::default().dark_gray(),
        ));
    }
    let marker_line = Line::from(spans);

    debug_assert!(
        marker_line.width() <= area.width as usize,
        "byte_markers line width ({}) exceeds area width ({})",
        marker_line.width(),
        area.width
    );

    marker_line.render(area, buf);
}

fn render_offsets(
    slice: &[u8],
    bytes_per_line: u8,
    scroll: usize,
    area: Rect,
    buf: &mut ratatui::prelude::Buffer,
) {
    // Render the offset (address) column for the hex view.
    //
    // Each line in the hex display starts with its offset rendered as 8-digit hexadecimal,
    // e.g. 00000000, 00000010, etc.
    //
    // - slice: The bytes to render.
    // - bytes_per_line: Number of bytes per row (usually 16).
    // - scroll: Starting row index (for paging/scrolling).
    // - area: The Rect area for the offset column (should be wide enough for 8 digits & padding).
    // - buf: The ratatui Buffer in which to render.
    //
    // Note: Y coordinate of `area` should be the top-most row to render.
    //       This draws vertically downward, one offset per line.

    let mut style_bool = !scroll.is_multiple_of(2);

    let styles = (Style::new().light_yellow(), Style::new().yellow());

    let total_rows = area.height.min(
        ((slice
            .len()
            .saturating_sub(scroll * (bytes_per_line as usize))) as f64
            / (bytes_per_line as f64))
            .ceil() as u16,
    );
    for i in 0..total_rows {
        style_bool.flip();
        let line_index = scroll + (i as usize);
        let offset = line_index * (bytes_per_line as usize);
        let rect = Rect {
            x: area.x,
            y: area.y + i,
            width: area.width,
            height: 1,
        };

        let dark_zeroes = "0".repeat(rect.width as usize);
        Line::styled(dark_zeroes, Style::new().dark_gray()).render(rect, buf);
        let offset_str = format!("{offset:X}");
        Line::from(vec![Span::styled(
            offset_str,
            if style_bool { styles.0 } else { styles.1 },
        )])
        .right_aligned()
        .render(rect, buf);
    }
}

fn render_bytes(
    slice: &[u8],
    bytes_per_line: u8,
    scroll: usize,
    hex_highlight_style: HexHighlightStyle,
    area: Rect,
    buf: &mut ratatui::prelude::Buffer,
) {
    // Render the bytes as hexadecimal only, with a space between each byte,
    // and an extra space every 8 bytes for readability.
    //
    // Example (bytes_per_line = 16):
    //   00 01 02 03 04 05 06 07  08 09 0A 0B 0C 0D 0E 0F
    //
    // - slice: bytes to render
    // - bytes_per_line: number of bytes per row (e.g., 16)
    // - scroll: starting row index
    // - area: destination area in ratatui buffer
    // - buf: the output ratatui buffer

    let total_lines = area.height.min(
        ((slice
            .len()
            .saturating_sub(scroll * (bytes_per_line as usize))) as f64
            / (bytes_per_line as f64))
            .ceil() as u16,
    );

    let mut style_bool = !scroll.is_multiple_of(2);

    for line in 0..total_lines {
        style_bool.flip();
        let offset = (scroll + line as usize) * (bytes_per_line as usize);
        let line_bytes = &slice[offset..slice.len().min(offset + bytes_per_line as usize)];

        let mut spans = Vec::with_capacity(line_bytes.len() * 3); // allow for spaces
        for (i, byte) in line_bytes.iter().enumerate() {
            if i != 0 {
                // Single space between bytes, and extra space after every 8 bytes (except at start)
                if i.is_multiple_of(8) {
                    spans.push(Span::raw("  "));
                } else {
                    spans.push(Span::raw(" "));
                }
            }
            let span_style = style_select(
                *byte,
                AlternatingStyles(style_bool, Style::new().white(), Style::new().gray()),
                hex_highlight_style,
            );
            spans.push(Span::styled(
                HEX_UPPER[(*byte) as usize],
                // format!("{:02X}", byte),
                span_style,
            ));
        }
        let row_rect = Rect {
            x: area.x,
            y: area.y + line,
            width: area.width,
            height: 1,
        };
        Line::from(spans).render(row_rect, buf);
    }
}

/// Calculates the number of hexadecimal digits needed to represent the given value `n`.
///
/// The formula essentially computes `floor(log2(n) / 4) + 1`, since each hex digit
/// represents 4 bits. This ensures at least 1 digit is returned (e.g., for `n == 0`).
fn hex_digits(n: usize) -> usize {
    (1 + ((n as f64).log2() / 4.0).floor() as usize).max(1)
}

// Maybe StatefulWidget would make more sense? Unsure.
// What's called to render all recieved data to screen.
impl Widget for &mut Buffer {
    fn render(self, area: Rect, buf: &mut ratatui::prelude::Buffer)
    where
        Self: Sized,
    {
        let para = self.terminal_paragraph();
        para.render(area, buf);

        if !self.state.stuck_to_bottom {
            let scroll_notice = Line::raw("More... Shift+PgDn to jump to newest").dark_gray();
            let notice_area = {
                let mut rect = area;
                rect.y = rect.bottom().saturating_sub(1);
                rect.height = 1;
                rect
            };
            Clear.render(notice_area, buf);
            scroll_notice.render(notice_area, buf);
        }

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"));
        scrollbar.render(area, buf, &mut self.state.scrollbar_state);
    }
}

#[cfg(feature = "defmt")]
pub mod defmt {
    use defmt_parser::Level;
    use ratatui::prelude::*;

    pub fn defmt_level_color(level: Option<Level>) -> Color {
        match level {
            Some(Level::Error) => Color::Red,
            Some(Level::Warn) => Color::Yellow,
            Some(Level::Info) => Color::Green,
            Some(Level::Debug) => Color::Blue,
            Some(Level::Trace) => Color::Magenta,
            None => Color::Gray,
        }
    }

    fn defmt_level_span(level: Option<Level>) -> Span<'static> {
        match level {
            Some(Level::Error) => Span::styled("ERROR", defmt_level_color(level)),
            Some(Level::Warn) => Span::styled("WARN", defmt_level_color(level)),
            Some(Level::Info) => Span::styled("INFO", defmt_level_color(level)),
            Some(Level::Debug) => Span::styled("DEBUG", defmt_level_color(level)),
            Some(Level::Trace) => Span::styled("TRACE", defmt_level_color(level)),
            None => Span::styled("???", defmt_level_color(level)),
        }
    }

    pub fn defmt_level_bracketed(level: Option<Level>) -> Vec<Span<'static>> {
        let end_bracket = match level {
            None => "]   ",
            Some(Level::Info) | Some(Level::Warn) => "]  ",
            Some(_) => "] ",
        };
        let dark_gray = Style::new().dark_gray();
        vec![
            Span::styled("[", dark_gray),
            defmt_level_span(level),
            Span::styled(end_bracket, dark_gray),
        ]
    }
}
