//! CPU text rendering: cosmic-text shapes lines, swash rasterises glyphs, and we blend
//! them straight into the window's pixel buffer.

use crate::highlight::StyledRun;
use cosmic_text::{
    fontdb, Attrs, AttrsList, Color, Family, FontSystem, Hinting, LayoutLine, PlatformFallback, ShapeLine,
    Shaping, Style, SwashCache, SwashContent, Weight, Wrap,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::rc::Rc;

/// Font database and glyph rasteriser, shared by every window.
///
/// Scanning every installed font is the slowest part of a cold start, so launches after
/// the first load only the font files that were used last time (listed in
/// `fonts.json`) and scan the rest on a background thread; see [`Fonts::scan`].
pub struct Fonts {
    system: FontSystem,
    swash: SwashCache,
    /// The editor font in use, and the platform's default monospace font.
    mono: Option<String>,
    default_mono: Option<String>,
    /// False while only the cached subset of fonts is loaded.
    complete: bool,
    /// Faces that have drawn text, recorded so the next launch can load just those.
    used: HashSet<fontdb::ID>,
    /// How many of `used` were in the last cache written, to skip redundant writes.
    saved: usize,
}

#[derive(Serialize, Deserialize)]
struct FontCache {
    locale: String,
    files: Vec<PathBuf>,
}

fn cache_path() -> Option<PathBuf> {
    crate::session::data_dir().map(|d| d.join("fonts.json"))
}

/// cosmic-text's default family choices, applied to databases we build ourselves.
fn configure(db: &mut fontdb::Database) {
    db.set_monospace_family("Noto Sans Mono");
    db.set_sans_serif_family("Open Sans");
    db.set_serif_family("DejaVu Serif");
}

impl Fonts {
    /// Loads every installed font. Slow on a cold disk cache; prefer [`Fonts::from_cache`].
    pub fn new() -> Self {
        Self::with_system(FontSystem::new(), true)
    }

    /// Loads only the fonts used last time, or `None` if there is no usable cache.
    pub fn from_cache() -> Option<Self> {
        let cache: FontCache = serde_json::from_str(&std::fs::read_to_string(cache_path()?).ok()?).ok()?;
        let mut db = fontdb::Database::new();
        for file in cache.files.iter().filter(|f| f.exists()) {
            let _ = db.load_font_file(file);
        }
        configure(&mut db);
        let system = FontSystem::new_with_locale_and_db_and_fallback(cache.locale, db, PlatformFallback);
        let fonts = Self::with_system(system, false);
        fonts.default_mono.is_some().then_some(fonts)
    }

    /// Scans every installed font on a background thread and hands the database to
    /// `done`, for [`Fonts::replace_database`].
    pub fn scan(done: impl FnOnce(fontdb::Database) + Send + 'static) {
        std::thread::spawn(move || {
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            configure(&mut db);
            done(db);
        });
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Switches to the full font database. Returns an error if the chosen font turns
    /// out not to be installed. Every layout must be rebuilt afterwards (face ids change).
    pub fn replace_database(&mut self, db: fontdb::Database) -> Result<(), String> {
        let wanted = self.mono.clone();
        let locale = self.system.locale().to_string();
        let system = FontSystem::new_with_locale_and_db_and_fallback(locale, db, PlatformFallback);
        *self = Self::with_system(system, true);
        if wanted != self.default_mono {
            self.set_family(wanted.as_deref())?;
        }
        Ok(())
    }

    fn with_system(system: FontSystem, complete: bool) -> Self {
        let candidates: &[&str] = if cfg!(target_os = "macos") {
            &["Menlo", "SF Mono", "Monaco"]
        } else if cfg!(windows) {
            &["Cascadia Mono", "Consolas", "Courier New"]
        } else {
            &["JetBrains Mono", "DejaVu Sans Mono", "Noto Sans Mono", "Liberation Mono"]
        };
        let mut fonts = Self {
            system,
            swash: SwashCache::new(),
            mono: None,
            default_mono: None,
            complete,
            used: HashSet::new(),
            saved: 0,
        };
        fonts.default_mono =
            candidates.iter().find(|name| fonts.has_family(name)).map(|name| name.to_string());
        fonts.mono = fonts.default_mono.clone();
        fonts
    }

    /// Records the files of every face used so far, for [`Fonts::from_cache`].
    pub fn save_cache(&mut self) {
        // A partial database would only record what it was started with.
        if !self.complete || self.used.len() == self.saved {
            return;
        }
        self.saved = self.used.len();
        let mut files: Vec<PathBuf> = self
            .used
            .iter()
            .filter_map(|id| match &self.system.db().face(*id)?.source {
                fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => Some(path.clone()),
                _ => None,
            })
            .collect();
        files.sort();
        files.dedup();
        let cache = FontCache { locale: self.system.locale().to_string(), files };
        if let (Some(path), Ok(json)) = (cache_path(), serde_json::to_string(&cache)) {
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            let _ = std::fs::write(path, json);
        }
    }

    fn has_family(&self, name: &str) -> bool {
        self.system.db().faces().any(|f| f.families.iter().any(|(family, _)| family == name))
    }

    /// Uses `name` for editor text, or the platform default for `None`. While only cached
    /// fonts are loaded, an unknown name is accepted and checked once the scan finishes.
    pub fn set_family(&mut self, name: Option<&str>) -> Result<(), String> {
        match name {
            None => self.mono = self.default_mono.clone(),
            Some(name) if self.has_family(name) || !self.complete => self.mono = Some(name.to_string()),
            Some(name) => return Err(format!("font \"{name}\" is not installed")),
        }
        Ok(())
    }

    fn family(&self, ui: bool) -> Family<'_> {
        match (&self.mono, ui) {
            (_, true) => Family::SansSerif,
            (Some(name), false) => Family::Name(name),
            (None, false) => Family::Monospace,
        }
    }
}

pub struct Metrics {
    pub font_size: f32,
    pub line_height: f32,
    /// Distance from the top of a line box to the text baseline.
    pub baseline: f32,
    pub char_width: f32,
}

/// Shaped lines for one window, reused across frames until they go off screen.
pub struct TextLayout {
    pub metrics: Metrics,
    tab_width: u16,
    current: HashMap<u64, Rc<LayoutLine>>,
    previous: HashMap<u64, Rc<LayoutLine>>,
}

impl TextLayout {
    pub fn new(fonts: &mut Fonts, font_size: f32, tab_width: u16) -> Self {
        let mut layout = Self {
            metrics: Metrics { font_size, line_height: 0.0, baseline: 0.0, char_width: 0.0 },
            tab_width,
            current: HashMap::new(),
            previous: HashMap::new(),
        };
        let sample = layout.line(fonts, "0", &[], 0, false);
        let line_height = (font_size * 1.5).round();
        let glyph_height = sample.max_ascent + sample.max_descent;
        layout.metrics.line_height = line_height;
        layout.metrics.baseline = ((line_height - glyph_height) / 2.0 + sample.max_ascent).round();
        layout.metrics.char_width = sample.w;
        layout
    }

    /// Drops lines that were not used since the previous call.
    pub fn end_frame(&mut self) {
        if self.current.len() > 256 {
            self.previous = std::mem::take(&mut self.current);
        }
    }

    pub fn line(
        &mut self,
        fonts: &mut Fonts,
        text: &str,
        runs: &[StyledRun],
        default_color: u32,
        ui: bool,
    ) -> Rc<LayoutLine> {
        let mut hasher = std::hash::DefaultHasher::new();
        (text, runs, default_color, ui, self.metrics.font_size.to_bits(), self.tab_width).hash(&mut hasher);
        let key = hasher.finish();
        if let Some(line) = self.current.get(&key) {
            return line.clone();
        }
        if let Some(line) = self.previous.remove(&key) {
            self.current.insert(key, line.clone());
            return line;
        }

        let family = fonts.family(ui);
        let base = Attrs::new().family(family).color(to_color(default_color));
        let mut attrs = AttrsList::new(&base);
        let mut offset = 0;
        for run in runs {
            // Styles can be one edit behind the text; clamp them to valid boundaries.
            let start = floor_boundary(text, offset);
            offset += run.len as usize;
            let end = floor_boundary(text, offset);
            if start < end {
                let mut span = base.clone().color(to_color(run.color));
                if run.bold {
                    span = span.weight(Weight::BOLD);
                }
                if run.italic {
                    span = span.style(Style::Italic);
                }
                attrs.add_span(start..end, &span);
            }
        }
        let shaped = ShapeLine::new(&mut fonts.system, text, &attrs, Shaping::Advanced, self.tab_width);
        // Hinting snaps glyph advances to whole pixels (layout is already in physical pixels).
        let line = shaped
            .layout(self.metrics.font_size, None, Wrap::None, None, None, Hinting::Enabled)
            .into_iter()
            .next()
            .unwrap_or_else(|| LayoutLine {
                w: 0.0,
                max_ascent: 0.0,
                max_descent: 0.0,
                line_height_opt: None,
                glyphs: Vec::new(),
            });
        fonts.used.extend(line.glyphs.iter().map(|g| g.font_id));
        let line = Rc::new(line);
        self.current.insert(key, line.clone());
        line
    }
}

fn floor_boundary(text: &str, mut i: usize) -> usize {
    i = i.min(text.len());
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn to_color(rgb: u32) -> Color {
    Color::rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// X offset of byte `index` within a laid-out line.
pub fn x_for_byte(line: &LayoutLine, index: usize) -> f32 {
    for glyph in &line.glyphs {
        if index <= glyph.start {
            return glyph.x;
        }
        if index < glyph.end {
            // Inside a ligature: interpolate across the characters it covers.
            let t = (index - glyph.start) as f32 / (glyph.end - glyph.start) as f32;
            return glyph.x + glyph.w * t;
        }
    }
    line.w
}

/// Byte index of the character boundary closest to `x`.
pub fn byte_for_x(line: &LayoutLine, text: &str, x: f32) -> usize {
    for glyph in &line.glyphs {
        if x < glyph.x + glyph.w {
            let index = if x < glyph.x + glyph.w / 2.0 { glyph.start } else { glyph.end };
            return floor_boundary(text, index);
        }
    }
    text.len()
}

/// A window's pixel buffer in 0x00RRGGBB.
pub struct Canvas<'a> {
    pub pixels: &'a mut [u32],
    pub width: usize,
    pub height: usize,
}

impl Canvas<'_> {
    pub fn fill(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, color: u32) {
        let x0 = (x0.round().max(0.0) as usize).min(self.width);
        let x1 = (x1.round().max(0.0) as usize).min(self.width);
        let y0 = (y0.round().max(0.0) as usize).min(self.height);
        let y1 = (y1.round().max(0.0) as usize).min(self.height);
        for y in y0..y1 {
            self.pixels[y * self.width + x0..y * self.width + x1].fill(color);
        }
    }

    /// A rectangle outline `thickness` pixels wide, inside the given bounds.
    pub fn outline(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, thickness: f32, color: u32) {
        self.fill(x0, y0, x1, y0 + thickness, color);
        self.fill(x0, y1 - thickness, x1, y1, color);
        self.fill(x0, y0, x0 + thickness, y1, color);
        self.fill(x1 - thickness, y0, x1, y1, color);
    }

    /// Draws `line` with its left edge at `x` and baseline at `y`, clipped to `clip_x0..clip_x1`.
    pub fn text(&mut self, fonts: &mut Fonts, line: &LayoutLine, x: f32, y: f32, clip_x0: f32, clip_x1: f32) {
        // Whole-pixel origins plus hinted layout put every glyph on the pixel grid, so
        // each copy of a letter rasterises identically.
        let (x, y) = (x.round(), y.round());
        for glyph in &line.glyphs {
            if x + glyph.x + glyph.w < clip_x0 {
                continue;
            }
            if x + glyph.x > clip_x1 {
                break;
            }
            let color = glyph
                .color_opt
                .map_or(0xffffff, |c| (c.r() as u32) << 16 | (c.g() as u32) << 8 | c.b() as u32);
            let physical = glyph.physical((x, y), 1.0);
            let Some(image) = fonts.swash.get_image(&mut fonts.system, physical.cache_key) else {
                continue;
            };
            let left = physical.x + image.placement.left;
            let top = physical.y - image.placement.top;
            let (w, h) = (image.placement.width as i32, image.placement.height as i32);
            let (cx0, cx1) = (clip_x0.max(0.0) as i32, (clip_x1 as i32).min(self.width as i32));
            for row in 0..h {
                let py = top + row;
                if py < 0 || py >= self.height as i32 {
                    continue;
                }
                for col in 0..w {
                    let px = left + col;
                    if px < cx0 || px >= cx1 {
                        continue;
                    }
                    let i = (row * w + col) as usize;
                    let (src, alpha) = match image.content {
                        SwashContent::Mask => (color, image.data[i] as u32),
                        SwashContent::SubpixelMask => {
                            let d = &image.data[i * 4..i * 4 + 3];
                            (color, (d[0] as u32 + d[1] as u32 + d[2] as u32) / 3)
                        }
                        SwashContent::Color => {
                            let d = &image.data[i * 4..i * 4 + 4];
                            ((d[0] as u32) << 16 | (d[1] as u32) << 8 | d[2] as u32, d[3] as u32)
                        }
                    };
                    if alpha == 0 {
                        continue;
                    }
                    let dst = &mut self.pixels[py as usize * self.width + px as usize];
                    *dst = blend(*dst, src, alpha);
                }
            }
        }
    }
}

pub fn blend(dst: u32, src: u32, alpha: u32) -> u32 {
    let inv = 255 - alpha;
    let channel = |shift: u32| {
        let s = (src >> shift) & 0xff;
        let d = (dst >> shift) & 0xff;
        ((s * alpha + d * inv + 127) / 255) << shift
    };
    channel(16) | channel(8) | channel(0)
}

/// Writes pixels as a 32-bit BMP. Used by `OXIDE_SNAPSHOT=<path>` to capture frames.
pub fn write_bmp(path: &std::path::Path, pixels: &[u32], width: usize, height: usize) -> std::io::Result<()> {
    let size = 54 + pixels.len() * 4;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&(-(height as i32)).to_le_bytes()); // negative: rows run top-down
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for pixel in pixels {
        out.extend_from_slice(&pixel.to_le_bytes());
    }
    std::fs::write(path, out)
}
