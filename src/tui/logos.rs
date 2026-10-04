//! Tool logos, each encoded once for the terminal's graphics protocol (Kitty in Ghostty, kitty and
//! WezTerm; iTerm2; sixel; else half blocks) and drawn with the stateless `Image` widget, so
//! redraws never re-encode.
use std::collections::HashMap;

use image::{DynamicImage, Rgba, RgbaImage, imageops};
use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use ratatui::widgets::Widget;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{FilterType, Image, Resize};

use ratatui::style::Color;

use super::theme::{BG, blend, rgb};

/// The widest a logo may be, in cells; every tile is this wide (`view::TILE`).
const MAX_WIDTH: u16 = 9;
pub const HEIGHT: u16 = 2;

const PNGS: [(&str, &[u8]); 6] = [
    ("T3", include_bytes!("../../assets/logos/t3.png")),
    ("Codex", include_bytes!("../../assets/logos/codex.png")),
    ("Claude", include_bytes!("../../assets/logos/claude.png")),
    ("OpenCode", include_bytes!("../../assets/logos/opencode.png")),
    ("Grok", include_bytes!("../../assets/logos/grok.png")),
    ("Pi", include_bytes!("../../assets/logos/pi.png")),
];

pub struct Logos {
    normal: HashMap<&'static str, Protocol>,
    /// Darkened copies for behind a modal: a graphics protocol's cells can't be dimmed like text.
    dimmed: HashMap<&'static str, Protocol>,
    /// Whether `dimmed` is in use: only the graphics protocols need it, half blocks are plain cells.
    pub separate_dim: bool,
}

/// The logo on the card background, so it needs no alpha (half blocks have none), `amount` of the way
/// to the background for the dimmed copy.
fn flatten(png: &[u8], amount: f32) -> DynamicImage {
    let src = image::load_from_memory(png).expect("bundled logo").to_rgba8();
    let [r, g, b] = rgb(BG);
    let mut out = RgbaImage::from_pixel(src.width(), src.height(), Rgba([r, g, b, 255]));
    imageops::overlay(&mut out, &src, 0, 0);
    for p in out.pixels_mut() {
        let [r, g, b] = rgb(blend(Color::Rgb(p[0], p[1], p[2]), BG, amount));
        *p = Rgba([r, g, b, 255]);
    }
    out.into()
}

fn encode(picker: &Picker, png: &[u8], amount: f32) -> Option<Protocol> {
    let image = flatten(png, amount);
    picker.new_protocol(image, Size::new(MAX_WIDTH, HEIGHT), Resize::Scale(Some(FilterType::Lanczos3))).ok()
}

impl Logos {
    pub fn new(picker: &Picker) -> Self {
        let build = |amount: f32| -> HashMap<&'static str, Protocol> {
            PNGS.iter().filter_map(|(name, png)| Some((*name, encode(picker, png, amount)?))).collect()
        };
        Logos {
            normal: build(1.0),
            dimmed: build(0.35),
            separate_dim: picker.protocol_type() != ProtocolType::Halfblocks,
        }
    }

    /// Half blocks: what the tests (and a terminal with no graphics) get.
    pub fn halfblocks() -> Self {
        Self::new(&Picker::halfblocks())
    }

    /// The logo's size in cells, so a tile can center it.
    pub fn size(&self, name: &str) -> Size {
        self.normal.get(name).map_or(Size::new(0, 0), Protocol::size)
    }

    pub fn draw(&self, name: &str, area: Rect, buf: &mut Buffer, dim: bool) {
        let set = if dim && self.separate_dim { &self.dimmed } else { &self.normal };
        if let Some(p) = set.get(name) {
            Image::new(p).allow_clipping(true).render(area, buf);
        }
    }

    /// Draw every logo once, anywhere: a graphics protocol sends an image's data with the first
    /// cell drawn, which a card scrolled out of view or a modal drawn on top would lose.
    pub fn prime(&self, buf: &mut Buffer) {
        let mut x = 0;
        for set in [&self.normal, &self.dimmed] {
            for p in set.values() {
                let size = p.size();
                if x + size.width > buf.area.width || size.height > buf.area.height {
                    continue;
                }
                Image::new(p).render(Rect::new(x, 0, size.width, size.height), buf);
                x += size.width;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logos_fit_a_tile() {
        let logos = Logos::halfblocks();
        for (name, _) in PNGS {
            let s = logos.size(name);
            assert!(s.height == HEIGHT && s.width > 0 && s.width <= MAX_WIDTH, "{name}: {s:?}");
        }
    }
}
