// Pure-std rasterizer for the NOBD app icon.
//
// Mirrors `docs/branding/icon-square.svg` exactly (512×512 viewBox): the dark
// rounded square, the two frame brackets, and the two overlapping purple/yellow
// press-dots with the bright lens where they meet — the same-frame moment.
//
// Renders at any size with 4×4 supersampled anti-aliasing. Used for the
// window/taskbar icon (main.rs), the system-tray icon (tray.rs), AND the
// embedded .exe icon (build.rs `include!`s this file). No image crate, no SVG
// rasterizer — the geometry is simple enough to draw directly.

const VB: f32 = 512.0; // SVG viewBox edge
const SS: u32 = 4; // supersamples per axis (4×4 = 16 per pixel)

// Palette (must match the SVG fills).
const BG: [u8; 3] = [18, 18, 24]; // #121218
const BRACKET: [u8; 3] = [232, 236, 243]; // #E8ECF3
const PURPLE: [u8; 3] = [139, 92, 246]; // #8B5CF6
const YELLOW: [u8; 3] = [255, 214, 10]; // #FFD60A
const LENS: [u8; 3] = [244, 247, 250]; // #F4F7FA

// Two press-dots.
const DOT_R: f32 = 82.0;
const PURPLE_CX: f32 = 196.0;
const YELLOW_CX: f32 = 316.0;
const DOT_CY: f32 = 256.0;

/// Straight-alpha RGBA8 buffer, `size`×`size`. `with_bg` draws the dark rounded
/// square behind everything (the full app icon); when false the dots + brackets
/// float on transparency.
pub fn rgba(size: u32, with_bg: bool) -> Vec<u8> {
    let n = size as f32;
    let scale = VB / n;
    let mut out = Vec::with_capacity((size * size * 4) as usize);

    for y in 0..size {
        for x in 0..size {
            let (mut ar, mut ag, mut ab, mut covered) = (0f32, 0f32, 0f32, 0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let fx = (x as f32 + (sx as f32 + 0.5) / SS as f32) * scale;
                    let fy = (y as f32 + (sy as f32 + 0.5) / SS as f32) * scale;
                    if let Some(c) = sample(fx, fy, with_bg) {
                        ar += c[0] as f32;
                        ag += c[1] as f32;
                        ab += c[2] as f32;
                        covered += 1.0;
                    }
                }
            }
            let tot = (SS * SS) as f32;
            if covered > 0.0 {
                out.push((ar / covered).round() as u8);
                out.push((ag / covered).round() as u8);
                out.push((ab / covered).round() as u8);
            } else {
                out.extend_from_slice(&[0, 0, 0]);
            }
            out.push(((covered / tot) * 255.0).round() as u8);
        }
    }
    out
}

/// Color at a sub-sample point, or `None` if the pixel is transparent there.
/// Painter's order: bg → brackets → purple dot → yellow dot → lens (topmost).
fn sample(x: f32, y: f32, with_bg: bool) -> Option<[u8; 3]> {
    let in_purple = dist2(x, y, PURPLE_CX, DOT_CY) <= DOT_R * DOT_R;
    let in_yellow = dist2(x, y, YELLOW_CX, DOT_CY) <= DOT_R * DOT_R;
    if in_purple && in_yellow {
        return Some(LENS);
    }
    if in_yellow {
        return Some(YELLOW);
    }
    if in_purple {
        return Some(PURPLE);
    }
    if on_brackets(x, y) {
        return Some(BRACKET);
    }
    if with_bg && in_rrect(x, y) {
        return Some(BG);
    }
    None
}

fn dist2(x: f32, y: f32, cx: f32, cy: f32) -> f32 {
    let (dx, dy) = (x - cx, y - cy);
    dx * dx + dy * dy
}

/// 512×512 rounded square, corner radius 112 (SVG `rx="112"`).
fn in_rrect(x: f32, y: f32) -> bool {
    let dx = ((x - 256.0).abs() - 144.0).max(0.0); // 144 = 256 - 112
    let dy = ((y - 256.0).abs() - 144.0).max(0.0);
    dx * dx + dy * dy <= 112.0 * 112.0
}

/// The two bracket strokes (width 28 → half 14, round caps/joins). Distance to
/// the polyline ≤ half-width reproduces the rounded SVG stroke.
fn on_brackets(x: f32, y: f32) -> bool {
    const HW2: f32 = 14.0 * 14.0;
    // Each bracket is a 3-segment polyline (top arm, spine, bottom arm).
    const SEGS: [(f32, f32, f32, f32); 6] = [
        // left  — M150 148 H96 V364 H150
        (150.0, 148.0, 96.0, 148.0),
        (96.0, 148.0, 96.0, 364.0),
        (96.0, 364.0, 150.0, 364.0),
        // right — M362 148 H416 V364 H362
        (362.0, 148.0, 416.0, 148.0),
        (416.0, 148.0, 416.0, 364.0),
        (416.0, 364.0, 362.0, 364.0),
    ];
    SEGS.iter().any(|&(ax, ay, bx, by)| dist_seg2(x, y, ax, ay, bx, by) <= HW2)
}

fn dist_seg2(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (abx, aby) = (bx - ax, by - ay);
    let (apx, apy) = (px - ax, py - ay);
    let ab2 = abx * abx + aby * aby;
    let t = if ab2 > 0.0 {
        ((apx * abx + apy * aby) / ab2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (cx, cy) = (ax + abx * t, ay + aby * t);
    let (dx, dy) = (px - cx, py - cy);
    dx * dx + dy * dy
}
