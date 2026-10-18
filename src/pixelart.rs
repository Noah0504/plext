//! Turns an album cover into small pixel art for the now-playing area.
//!
//! The cover is cropped square, shrunk to 16×16 by averaging each block of
//! pixels (in linear light, so dark and bright areas mix the way the eye
//! mixes them), and its colors are cut down to a small palette with median
//! cut. The UI then draws two pixels per terminal cell with the `▀` glyph.

use anyhow::{Context, Result};
use std::path::Path;

/// Pixels per side.
pub const SIZE: usize = 16;
/// Colors in the reduced palette.
const COLORS: usize = 16;

/// A `SIZE`×`SIZE` image, row by row, as sRGB colors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PixelArt {
    pub pixels: Vec<[u8; 3]>,
}

impl PixelArt {
    pub fn at(&self, x: usize, y: usize) -> [u8; 3] {
        self.pixels[y * SIZE + x]
    }
}

/// Reads an image file (JPEG or PNG) and makes pixel art from it.
pub fn from_file(path: &Path) -> Result<PixelArt> {
    let img = image::ImageReader::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .with_guessed_format()?
        .decode()
        .with_context(|| format!("decoding {}", path.display()))?
        .to_rgb8();
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        anyhow::bail!("empty image");
    }
    Ok(from_rgb(w as usize, h as usize, img.as_raw()))
}

/// Makes pixel art from raw RGB bytes (`w`×`h`, 3 bytes per pixel).
pub fn from_rgb(w: usize, h: usize, rgb: &[u8]) -> PixelArt {
    let small = shrink(w, h, rgb);
    PixelArt {
        pixels: median_cut(&small, COLORS),
    }
}

// ---------- shrinking ----------

fn to_linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn to_srgb(l: f32) -> u8 {
    let l = l.clamp(0.0, 1.0);
    let c = if l <= 0.003_130_8 { l * 12.92 } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 };
    (c * 255.0).round() as u8
}

/// Crops the centre square and averages it down to `SIZE`×`SIZE`.
fn shrink(w: usize, h: usize, rgb: &[u8]) -> Vec<[u8; 3]> {
    let lut: Vec<f32> = (0..=255u8).map(to_linear).collect();
    let side = w.min(h);
    let (ox, oy) = ((w - side) / 2, (h - side) / 2);
    let mut out = Vec::with_capacity(SIZE * SIZE);
    for py in 0..SIZE {
        let (y0, y1) = (oy + py * side / SIZE, oy + ((py + 1) * side / SIZE).max(py * side / SIZE + 1));
        for px in 0..SIZE {
            let (x0, x1) = (ox + px * side / SIZE, ox + ((px + 1) * side / SIZE).max(px * side / SIZE + 1));
            let mut sum = [0f32; 3];
            let mut n = 0f32;
            for y in y0..y1.min(h) {
                let row = &rgb[y * w * 3..(y + 1) * w * 3];
                for x in x0..x1.min(w) {
                    for c in 0..3 {
                        sum[c] += lut[row[x * 3 + c] as usize];
                    }
                    n += 1.0;
                }
            }
            let n = n.max(1.0);
            out.push([to_srgb(sum[0] / n), to_srgb(sum[1] / n), to_srgb(sum[2] / n)]);
        }
    }
    out
}

// ---------- palette ----------

/// Reduces `pixels` to at most `k` colors: repeatedly split the group of
/// colors that spans the widest range at the median of that channel, then
/// give each pixel its group's average color.
fn median_cut(pixels: &[[u8; 3]], k: usize) -> Vec<[u8; 3]> {
    let mut groups: Vec<Vec<usize>> = vec![(0..pixels.len()).collect()];
    let range = |g: &[usize]| -> (u8, usize) {
        let mut best = (0u8, 0usize);
        for c in 0..3 {
            let (lo, hi) = g.iter().fold((255u8, 0u8), |(lo, hi), &i| {
                (lo.min(pixels[i][c]), hi.max(pixels[i][c]))
            });
            if hi - lo > best.0 {
                best = (hi - lo, c);
            }
        }
        best
    };
    while groups.len() < k {
        // The group to split: widest range, ties broken by size.
        let Some((gi, (spread, ch))) = groups
            .iter()
            .enumerate()
            .filter(|(_, g)| g.len() > 1)
            .map(|(i, g)| (i, range(g)))
            .max_by_key(|(i, (spread, _))| (*spread, groups[*i].len()))
        else {
            break;
        };
        if spread == 0 {
            break;
        }
        let mut g = groups.swap_remove(gi);
        g.sort_by_key(|&i| pixels[i][ch]);
        let upper = g.split_off(g.len() / 2);
        groups.push(g);
        groups.push(upper);
    }
    let mut out = vec![[0u8; 3]; pixels.len()];
    for g in &groups {
        let mut sum = [0u32; 3];
        for &i in g {
            for c in 0..3 {
                sum[c] += pixels[i][c] as u32;
            }
        }
        let n = g.len().max(1) as u32;
        let avg = [
            ((sum[0] + n / 2) / n) as u8,
            ((sum[1] + n / 2) / n) as u8,
            ((sum[2] + n / 2) / n) as u8,
        ];
        for &i in g {
            out[i] = avg;
        }
    }
    out
}

// ---------- terminal colors ----------

/// Whether the terminal says it can show 24-bit color.
pub fn truecolor() -> bool {
    std::env::var("COLORTERM")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "truecolor" | "24bit"))
        .unwrap_or(false)
}

/// The nearest of the xterm 256-color palette's colors (its 6×6×6 cube or
/// its gray ramp) for terminals without 24-bit color. "Nearest" is measured
/// in Oklab, which tracks how different colors look, with hue weighted up:
/// plain RGB distance turns dark blues and purples into grays.
pub fn to_256(rgb: [u8; 3]) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let target = oklab(rgb);
    let dist = |c: [u8; 3]| -> f32 {
        let o = oklab(c);
        // Hue counts double: the palette is coarse in the darks, and a
        // slightly-off lightness looks better than a color gone gray.
        (o[0] - target[0]).powi(2) + 4.0 * ((o[1] - target[1]).powi(2) + (o[2] - target[2]).powi(2))
    };
    let cube = (0..216).map(|i| (16 + i as u8, [LEVELS[i / 36], LEVELS[(i / 6) % 6], LEVELS[i % 6]]));
    let grays = (0..24).map(|i| {
        let v = 8 + i as u8 * 10;
        (232 + i as u8, [v, v, v])
    });
    cube.chain(grays)
        .map(|(idx, c)| (idx, dist(c)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(idx, _)| idx)
        .unwrap_or(16)
}

fn oklab(rgb: [u8; 3]) -> [f32; 3] {
    let [r, g, b] = rgb.map(to_linear);
    let l = (0.412_221_47 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(w: usize, h: usize, f: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        let mut v = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                v.extend_from_slice(&f(x, y));
            }
        }
        v
    }

    #[test]
    fn a_flat_color_stays_that_color() {
        let rgb = image(512, 512, |_, _| [200, 40, 90]);
        let art = from_rgb(512, 512, &rgb);
        assert_eq!(art.pixels.len(), SIZE * SIZE);
        assert!(art.pixels.iter().all(|p| *p == [200, 40, 90]));
    }

    #[test]
    fn halves_land_in_the_right_pixels() {
        // Left half red, right half blue.
        let rgb = image(320, 320, |x, _| if x < 160 { [255, 0, 0] } else { [0, 0, 255] });
        let art = from_rgb(320, 320, &rgb);
        assert_eq!(art.at(0, 0), [255, 0, 0]);
        assert_eq!(art.at(7, 15), [255, 0, 0]);
        assert_eq!(art.at(8, 0), [0, 0, 255]);
        assert_eq!(art.at(15, 15), [0, 0, 255]);
    }

    #[test]
    fn averaging_is_done_in_linear_light() {
        // A fine black/white checkerboard averages to sRGB ~188, not 128.
        let rgb = image(64, 64, |x, y| if (x + y) % 2 == 0 { [0, 0, 0] } else { [255, 255, 255] });
        let art = from_rgb(64, 64, &rgb);
        let v = art.at(3, 3)[0];
        assert!((180..=195).contains(&v), "got {}", v);
    }

    #[test]
    fn wide_images_are_cropped_to_the_centre() {
        // 3:1 image: only the middle third (green) should show.
        let rgb = image(300, 100, |x, _| match x {
            0..=99 => [255, 0, 0],
            100..=199 => [0, 255, 0],
            _ => [0, 0, 255],
        });
        let art = from_rgb(300, 100, &rgb);
        assert!(art.pixels.iter().all(|p| *p == [0, 255, 0]));
    }

    #[test]
    fn a_gradient_is_cut_to_at_most_the_palette_size() {
        let rgb = image(256, 256, |x, y| [x as u8, y as u8, ((x + y) / 2) as u8]);
        let art = from_rgb(256, 256, &rgb);
        let mut colors: Vec<[u8; 3]> = art.pixels.clone();
        colors.sort();
        colors.dedup();
        assert!(colors.len() <= COLORS && colors.len() >= COLORS / 2, "{} colors", colors.len());
    }

    #[test]
    fn decodes_png_files() {
        let dir = std::env::temp_dir().join(format!("plext-art-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cover.png");
        let img = image::RgbImage::from_pixel(40, 40, image::Rgb([10, 120, 230]));
        img.save(&path).unwrap();
        let art = from_file(&path).unwrap();
        assert!(art.pixels.iter().all(|p| *p == [10, 120, 230]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn maps_to_the_256_color_palette() {
        assert_eq!(to_256([0, 0, 0]), 16);
        assert_eq!(to_256([255, 255, 255]), 231);
        assert_eq!(to_256([255, 0, 0]), 196);
        assert_eq!(to_256([128, 128, 128]), 244);
        assert_eq!(to_256([0, 95, 135]), 16 + 6 + 2);
        // A dark navy stays blue rather than going gray.
        let navy = to_256([30, 40, 90]);
        assert!((16..232).contains(&navy) && !(navy - 16).is_multiple_of(6), "got {}", navy);
    }
}
