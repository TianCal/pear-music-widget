//! Generates the app icon and the menu-bar icons from scratch — no image
//! libraries beyond a PNG encoder, and no binary assets checked in.
//!
//!   icons/icon.png        1024px master, used by the bundler
//!   icons/icon.icns       Big Sur style rounded square with a play glyph
//!   $OUT_DIR/tray-*.png   menu-bar template images, solid + hollow, @1x and @2x
//!
//! The tray images land in OUT_DIR and are pulled in with `include_bytes!`, so
//! they are never written back into the source tree.

use std::path::{Path, PathBuf};
use std::process::Command;

const SIZE: u32 = 1024;
const MARGIN: f64 = 100.0; // Big Sur icons float inside a transparent margin
const RADIUS: f64 = 185.0; // ~22.4% of the visible square
const SS: u32 = 3; // supersampling factor for anti-aliasing

// A vinyl record on a dark tile, with a pear-shaped coral label.
const BACKGROUND: [f64; 3] = [44.0, 44.0, 42.0];
const DISC: [f64; 3] = [68.0, 68.0, 65.0];
const GROOVE: [f64; 3] = [95.0, 94.0, 90.0];
const LABEL: [f64; 3] = [240.0, 153.0, 123.0];
const DISC_RADIUS: f64 = 318.0;
const GROOVES: [f64; 3] = [286.0, 250.0, 214.0];
const GROOVE_WIDTH: f64 = 5.0;
/// The spindle hole sits in the pear's wide lower half, as on a real label.
const HOLE_DROP: f64 = 30.0;
const HOLE_RADIUS: f64 = 16.0;

fn write_png(path: &Path, width: u32, height: u32, pixels: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create icon directory");
    }
    let file = std::fs::File::create(path).expect("create png");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .expect("png header")
        .write_image_data(pixels)
        .expect("png data");
}

/// Signed distance to a rounded rectangle; negative inside.
fn rounded_rect_sdf(px: f64, py: f64, c: f64, half: f64, r: f64) -> f64 {
    let qx = (px - c).abs() - (half - r);
    let qy = (py - c).abs() - (half - r);
    let outside = qx.max(0.0).hypot(qy.max(0.0));
    outside + qx.max(qy).min(0.0) - r
}

fn render_app_icon() -> Vec<u8> {
    let mut pixels = vec![0u8; (SIZE * SIZE * 4) as usize];
    let c = SIZE as f64 / 2.0;
    let half = (SIZE as f64 - MARGIN * 2.0) / 2.0;

    // Painted back to front; each layer is (signed distance at a point, colour).
    let layers = |x: f64, y: f64| -> [(f64, [f64; 3]); 4] {
        let r = (x - c).hypot(y - c);
        // Grooves: thin rings across the record, between label and rim.
        let groove = GROOVES
            .iter()
            .map(|g| (r - g).abs() - GROOVE_WIDTH / 2.0)
            .fold(f64::INFINITY, f64::min);
        [
            (r - DISC_RADIUS, DISC),
            (groove.max(r - DISC_RADIUS), GROOVE),
            (pear_sdf(x - c, y - c), LABEL),
            ((x - c).hypot(y - c - HOLE_DROP) - HOLE_RADIUS, BACKGROUND),
        ]
    };

    let samples = (SS * SS) as f64;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let mut alpha = 0.0;
            let mut colour = [0.0f64; 3];
            for sy in 0..SS {
                for sx in 0..SS {
                    let fx = x as f64 + (sx as f64 + 0.5) / SS as f64;
                    let fy = y as f64 + (sy as f64 + 0.5) / SS as f64;
                    if rounded_rect_sdf(fx, fy, c, half, RADIUS) > 0.0 {
                        continue;
                    }
                    let mut here = BACKGROUND;
                    for (distance, paint) in layers(fx, fy) {
                        if distance <= 0.0 {
                            here = paint;
                        }
                    }
                    alpha += 1.0;
                    for ch in 0..3 {
                        colour[ch] += here[ch];
                    }
                }
            }
            if alpha == 0.0 {
                continue;
            }
            let i = ((y * SIZE + x) * 4) as usize;
            for ch in 0..3 {
                pixels[i + ch] = (colour[ch] / alpha).round() as u8;
            }
            pixels[i + 3] = (alpha / samples * 255.0).round() as u8;
        }
    }
    pixels
}

/// A pear standing upright around the origin: a small circle on a large one,
/// blended with a smooth minimum so the waist curves in rather than pinching.
fn pear_sdf(x: f64, y: f64) -> f64 {
    let top = x.hypot(y + 62.0) - 64.0;
    let bottom = x.hypot(y - 42.0) - 98.0;
    let k = 46.0;
    let h = (0.5 + 0.5 * (bottom - top) / k).clamp(0.0, 1.0);
    bottom * (1.0 - h) + top * h - k * h * (1.0 - h)
}

// ------------------------------------------------------------ menu-bar icons

// Template images: black with an alpha mask, which macOS re-tints for light and
// dark menu bars. Drawn at 18pt (and @2x) with the glyph occupying ~41% of the
// canvas radius, a touch smaller than the system Now Playing icon.
const TRAY_RADIUS_RATIO: f64 = 0.414;
const TRAY_STROKE_RATIO: f64 = 0.108;

/// Play triangle inscribed in a circle of radius `r` centred on the canvas.
fn in_play_glyph(dx: f64, dy: f64, r: f64) -> bool {
    let tx = dx / r;
    let ty = dy / r;
    tx >= -0.34 && tx <= 0.46 && ty.abs() <= (0.46 - tx) * 0.55
}

fn render_tray(s: u32, hollow: bool) -> Vec<u8> {
    let mut pixels = vec![0u8; (s * s * 4) as usize];
    let c = (s as f64 - 1.0) / 2.0;
    let outer = s as f64 * TRAY_RADIUS_RATIO;
    let inner = outer - s as f64 * TRAY_STROKE_RATIO;
    let ss = 4u32;

    for y in 0..s {
        for x in 0..s {
            let mut covered = 0u32;
            for sy in 0..ss {
                for sx in 0..ss {
                    let dx = x as f64 + (sx as f64 + 0.5) / ss as f64 - 0.5 - c;
                    let dy = y as f64 + (sy as f64 + 0.5) / ss as f64 - 0.5 - c;
                    let dist = dx.hypot(dy);
                    if dist > outer {
                        continue;
                    }

                    if hollow {
                        // Ring plus a solid play triangle sitting inside it.
                        if dist >= inner && !in_play_glyph(dx, dy, inner) {
                            covered += 1;
                        } else if dist < inner && in_play_glyph(dx, dy, inner * 0.92) {
                            covered += 1;
                        }
                    } else if !in_play_glyph(dx, dy, outer) {
                        // Solid disc with the triangle knocked out.
                        covered += 1;
                    }
                }
            }
            if covered == 0 {
                continue;
            }
            let i = ((y * s + x) * 4) as usize;
            pixels[i + 3] = ((covered as f64 / (ss * ss) as f64) * 255.0).round() as u8;
        }
    }
    pixels
}

// ---------------------------------------------------------------------- icns

/// Fold the 1024px master down into an .icns. `sips` and `iconutil` ship with
/// macOS, so this needs nothing installed.
fn build_icns(icons: &Path, master: &Path) {
    let iconset = icons.join("icon.iconset");
    let _ = std::fs::remove_dir_all(&iconset);
    std::fs::create_dir_all(&iconset).expect("create iconset");

    for size in [16u32, 32, 128, 256, 512] {
        for scale in [1u32, 2] {
            let px = size * scale;
            let name = if scale == 1 {
                format!("icon_{size}x{size}.png")
            } else {
                format!("icon_{size}x{size}@2x.png")
            };
            let ok = Command::new("sips")
                .args(["-z", &px.to_string(), &px.to_string()])
                .arg(master)
                .arg("--out")
                .arg(iconset.join(name))
                .output()
                .map(|out| out.status.success())
                .unwrap_or(false);
            if !ok {
                println!("cargo:warning=sips failed; skipping icon.icns");
                let _ = std::fs::remove_dir_all(&iconset);
                return;
            }
        }
    }

    let ok = Command::new("iconutil")
        .args(["-c", "icns"])
        .arg(&iconset)
        .arg("-o")
        .arg(icons.join("icon.icns"))
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    if !ok {
        println!("cargo:warning=iconutil failed; skipping icon.icns");
    }
    let _ = std::fs::remove_dir_all(&iconset);
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    for (name, size, hollow) in [
        ("tray-solid.png", 18, false),
        ("tray-solid@2x.png", 36, false),
        ("tray-hollow.png", 18, true),
        ("tray-hollow@2x.png", 36, true),
    ] {
        write_png(&out_dir.join(name), size, size, &render_tray(size, hollow));
    }

    let icons = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir")).join("icons");
    let master = icons.join("icon.png");
    write_png(&master, SIZE, SIZE, &render_app_icon());

    // The bundler wants these exact names alongside the master.
    for (name, px) in [("32x32.png", 32u32), ("128x128.png", 128), ("128x128@2x.png", 256)] {
        let _ = Command::new("sips")
            .args(["-z", &px.to_string(), &px.to_string()])
            .arg(&master)
            .arg("--out")
            .arg(icons.join(name))
            .output();
    }

    if cfg!(target_os = "macos") {
        build_icns(&icons, &master);
    }

    tauri_build::build();
}
