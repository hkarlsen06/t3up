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
use ratatui_image::{FilterType, FontSize, Image, Resize};

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
    /// Tile-wide copies, by (name, odd, dimmed): see `tile_image`.
    tiles: HashMap<(&'static str, bool, bool), Protocol>,
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

/// The logo on a canvas exactly a tile wide (`MAX_WIDTH` x `HEIGHT` cells), placed to the pixel under
/// the line of text below it. Text sits in whole cells, so in a 9-cell tile an odd-width line centers at
/// 4.5 cells and an even one at 4: the logo follows it there, which whole cells couldn't.
fn tile_image(png: &[u8], amount: f32, font: FontSize, odd: bool) -> DynamicImage {
    let (fw, fh) = (font.width.max(1) as u32, font.height.max(1) as u32);
    let (w, h) = (MAX_WIDTH as u32 * fw, HEIGHT as u32 * fh);
    let logo = flatten(png, amount).resize(w, h, FilterType::Lanczos3);
    let center = if odd { MAX_WIDTH as u32 * fw / 2 } else { (MAX_WIDTH as u32 - 1) * fw / 2 };
    let [r, g, b] = rgb(BG);
    let mut canvas = RgbaImage::from_pixel(w, h, Rgba([r, g, b, 255]));
    let x = center.saturating_sub(logo.width() / 2).min(w - logo.width());
    imageops::overlay(&mut canvas, &logo.to_rgba8(), x as i64, ((h - logo.height()) / 2) as i64);
    canvas.into()
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
        let graphics = picker.protocol_type() != ProtocolType::Halfblocks;
        let mut tiles = HashMap::new();
        // Half blocks move in whole cells only: shifting one half a cell would just blur it.
        for (name, png) in PNGS.into_iter().filter(|_| graphics) {
            for (odd, dim) in [(false, false), (true, false), (false, true), (true, true)] {
                let image = tile_image(png, if dim { 0.35 } else { 1.0 }, picker.font_size(), odd);
                let size = Size::new(MAX_WIDTH, HEIGHT);
                if let Ok(p) = picker.new_protocol(image, size, Resize::Scale(Some(FilterType::Lanczos3))) {
                    tiles.insert((name, odd, dim), p);
                }
            }
        }
        Logos { normal: build(1.0), dimmed: build(0.35), tiles, separate_dim: graphics }
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

    /// A tile's logo (a tile wide), lined up with the text under it: `odd` if that's an odd width.
    pub fn draw_tile(&self, name: &str, odd: bool, area: Rect, buf: &mut Buffer, dim: bool) {
        let name = PNGS.iter().map(|(n, _)| *n).find(|n| *n == name).unwrap_or_default();
        if let Some(p) = self.tiles.get(&(name, odd, dim)) {
            Image::new(p).allow_clipping(true).render(area, buf);
        } else {
            // Half blocks: centered in whole cells.
            let size = self.size(name);
            let x = area.x + area.width.saturating_sub(size.width) / 2;
            self.draw(name, Rect::new(x, area.y, size.width.min(area.width), size.height), buf, dim);
        }
    }

    /// Draw every logo once, anywhere: a graphics protocol sends an image's data with the first
    /// cell drawn, which a card scrolled out of view or a modal drawn on top would lose.
    pub fn prime(&self, buf: &mut Buffer) {
        let (mut x, mut y) = (0, 0);
        for p in self.normal.values().chain(self.dimmed.values()).chain(self.tiles.values()) {
            let size = p.size();
            if x + size.width > buf.area.width {
                (x, y) = (0, y + HEIGHT);
            }
            if x + size.width > buf.area.width || y + size.height > buf.area.height {
                continue;
            }
            Image::new(p).render(Rect::new(x, y, size.width, size.height), buf);
            x += size.width;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_logos_center_where_the_text_under_them_does() {
        let font = FontSize::new(10, 20);
        let bg = rgb(BG);
        for (name, png) in PNGS {
            for (odd, cells) in [(true, 4.5), (false, 4.0)] {
                let image = tile_image(png, 1.0, font, odd).to_rgba8();
                assert_eq!(image.dimensions(), (90, 40));
                // The middle of the logo's ink, in pixels.
                let ink: Vec<u32> =
                    image.enumerate_pixels().filter(|(_, _, p)| p.0[..3] != bg).map(|(x, _, _)| x).collect();
                let center = (ink.iter().min().unwrap() + ink.iter().max().unwrap() + 1) as f32 / 2.0;
                assert!((center - cells * 10.0).abs() <= 1.0, "{name} odd={odd}: {center}");
            }
        }
    }

    #[test]
    fn graphics_terminals_get_tile_wide_logos_at_their_own_size() {
        #[allow(deprecated)] // the one way to pick a font size without a terminal to ask
        let mut picker = Picker::from_fontsize(FontSize::new(10, 20));
        picker.set_protocol_type(ProtocolType::Kitty);
        let logos = Logos::new(&picker);
        for (name, _) in PNGS {
            for odd in [false, true] {
                assert_eq!(logos.tiles[&(name, odd, false)].size(), Size::new(MAX_WIDTH, HEIGHT), "{name}");
            }
        }
    }

    #[test]
    fn logos_fit_a_tile() {
        let logos = Logos::halfblocks();
        for (name, _) in PNGS {
            let s = logos.size(name);
            assert!(s.height == HEIGHT && s.width > 0 && s.width <= MAX_WIDTH, "{name}: {s:?}");
        }
    }
}
