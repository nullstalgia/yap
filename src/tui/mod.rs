use ratatui::layout::{Rect, Size};

#[cfg(feature = "defmt")]
pub mod defmt;
#[cfg(feature = "espflash")]
pub mod esp;
#[cfg(feature = "logging")]
pub mod logging;

// pub mod buffer;
pub mod color_rules;
pub mod modifiers;
pub mod prompts;
mod show_keybinds;
pub mod single_line_selector;
pub use show_keybinds::show_keybinds;

/// Popup category selectors count.
pub const POPUP_MENU_SELECTOR_COUNT: usize = 1;

// /// Returns a `Rect` with the provided percentage of the parent `Rect` and centered.
// pub fn centered_rect_ratio(percent_x: u16, percent_y: u16, parent: Rect) -> Rect {
//     let width = parent.width * percent_x / 100;
//     let height = parent.height * percent_y / 100;
//     Rect {
//         width,
//         height,
//         x: (parent.width.saturating_sub(width)) / 2,
//         y: (parent.height.saturating_sub(height)) / 2,
//     }
// }

/// Returns a centered `Rect` with the provided size inside of the parent `Rect`.
pub fn centered_rect_size(size: Size, parent: Rect) -> Rect {
    Rect {
        width: size.width.min(parent.width),
        height: size.height.min(parent.height),
        x: (parent.width.saturating_sub(size.width)) / 2,
        y: (parent.height.saturating_sub(size.height)) / 2,
    }
}
