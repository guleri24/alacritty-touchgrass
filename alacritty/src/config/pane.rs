use alacritty_config_derive::ConfigDeserialize;
use serde::Serialize;

use crate::display::color::Rgb;

/// Configuration for terminal panes.
#[derive(ConfigDeserialize, Serialize, Clone, Debug, PartialEq)]
pub struct PaneConfig {
    /// Width of the border between panes in pixels.
    pub border_width: usize,

    /// Color of inactive pane borders.
    pub border_color: Rgb,

    /// Color of the active (focused) pane border.
    pub active_border_color: Rgb,

    /// Minimum width of a pane in columns.
    pub min_width: usize,

    /// Minimum height of a pane in lines.
    pub min_height: usize,
}

impl Default for PaneConfig {
    fn default() -> Self {
        Self {
            border_width: 2,
            border_color: Rgb::new(0x66, 0x66, 0x66),
            active_border_color: Rgb::new(0xcc, 0xcc, 0xcc),
            min_width: 20,
            min_height: 3,
        }
    }
}
