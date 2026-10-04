//! Graphite neutrals, one blue accent; green/amber/red only ever mean status.
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};

pub const fn hex(rgb: u32) -> Color {
    Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

pub const BG: Color = hex(0x16161a);
pub const SURFACE: Color = hex(0x1f1f24);
pub const LINE: Color = hex(0x34343c);
pub const TEXT: Color = hex(0xececf1);
pub const MUTED: Color = hex(0xa1a1ab);
pub const DIM: Color = hex(0x6e6e78);
pub const FAINT: Color = hex(0x45454d);
pub const ACCENT: Color = hex(0x82aaff);
pub const GREEN: Color = hex(0x6fd39a);
pub const AMBER: Color = hex(0xe5b567);
pub const RED: Color = hex(0xf07178);

pub const SPIN: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub fn fg(color: Color) -> Style {
    Style::new().fg(color)
}

pub fn bold(color: Color) -> Style {
    Style::new().fg(color).add_modifier(Modifier::BOLD)
}

fn channels(color: Color) -> (f32, f32, f32) {
    match color {
        Color::Rgb(r, g, b) => (r as f32, g as f32, b as f32),
        _ => (0.0, 0.0, 0.0),
    }
}

pub fn rgb(color: Color) -> [u8; 3] {
    let (r, g, b) = channels(color);
    [r as u8, g as u8, b as u8]
}

/// `top` laid over `under` at opacity `alpha`.
pub fn blend(top: Color, under: Color, alpha: f32) -> Color {
    let (a, b) = (channels(top), channels(under));
    let mix = |t: f32, u: f32| (u + (t - u) * alpha).round() as u8;
    Color::Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// The cursor row of a menu: the accent at low alpha over the panel.
pub fn cursor_bg() -> Color {
    blend(ACCENT, SURFACE, 0.2)
}

/// A modal's border: the accent at about 50%.
pub fn border() -> Color {
    blend(ACCENT, SURFACE, 0.5)
}

/// Whether a cell carries an inline image (a Kitty placeholder or a raw escape sequence), which
/// darkening must leave alone: the placeholder's foreground *is* the image id.
fn is_image(symbol: &str) -> bool {
    symbol.contains('\u{10EEEE}') || symbol.contains('\x1b')
}

/// A backdrop for a modal: every cell pulled `amount` of the way to the background.
pub fn dim(buf: &mut Buffer, amount: f32) {
    for cell in &mut buf.content {
        if is_image(cell.symbol()) {
            continue;
        }
        cell.fg = blend(BG, cell.fg, amount);
        cell.bg = blend(BG, cell.bg, amount);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blending() {
        assert_eq!(blend(Color::Rgb(255, 255, 255), Color::Rgb(0, 0, 0), 0.5), Color::Rgb(128, 128, 128));
        assert_eq!(blend(ACCENT, SURFACE, 0.0), SURFACE);
        assert_eq!(blend(ACCENT, SURFACE, 1.0), ACCENT);
    }
}
