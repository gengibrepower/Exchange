//! Gruvbox dark.

use ratatui::style::{Color, Modifier, Style};

pub const BG: Color = Color::Rgb(0x28, 0x28, 0x28);
pub const BG1: Color = Color::Rgb(0x3c, 0x38, 0x36);
pub const FG: Color = Color::Rgb(0xeb, 0xdb, 0xb2);
pub const GRAY: Color = Color::Rgb(0x92, 0x83, 0x74);
pub const RED: Color = Color::Rgb(0xfb, 0x49, 0x34);
pub const GREEN: Color = Color::Rgb(0xb8, 0xbb, 0x26);
pub const YELLOW: Color = Color::Rgb(0xfa, 0xbd, 0x2f);
pub const ORANGE: Color = Color::Rgb(0xfe, 0x80, 0x19);
pub const AQUA: Color = Color::Rgb(0x8e, 0xc0, 0x7c);

pub fn base() -> Style {
    Style::new().fg(FG).bg(BG)
}

pub fn dim() -> Style {
    Style::new().fg(GRAY)
}

pub fn title() -> Style {
    Style::new().fg(AQUA).add_modifier(Modifier::BOLD)
}

pub fn key() -> Style {
    Style::new().fg(YELLOW)
}

pub fn border(focused: bool) -> Style {
    Style::new().fg(if focused { YELLOW } else { GRAY })
}

pub fn selected() -> Style {
    Style::new().bg(BG1).add_modifier(Modifier::BOLD)
}

pub fn buy() -> Style {
    Style::new().fg(GREEN)
}

pub fn sell() -> Style {
    Style::new().fg(RED)
}

pub fn last() -> Style {
    Style::new().fg(ORANGE)
}
