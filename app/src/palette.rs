//! THE CHARTER — one meaning per color, checkable.
//!
//! The ONLY place in this crate where `Color32::from_rgb` may appear. Adapted
//! from the Retro Receipts "Arena Card System" method: one meaning per token, in
//! a table, with an explicit NEVER column.
//!
//! The rule that matters most here is the local form of "losses are never red":
//!
//! > **Your finger gap is never styled as failure.**
//!
//! Everything NOBD measures *about the user or their hardware* is data ink. No
//! function anywhere may take a measured value and return a `Color32`. NOBD
//! colours its own contribution and nothing else — a 14 ms recommendation used
//! to render red, which reads as "your hands are broken".
//!
//! Every hue below clears WCAG AA (4.5:1) on all three grounds. The colours it
//! replaces did not: the old RED was 4.22:1 and the old DARK_GRAY 2.97:1, and
//! the latter was carrying real content.

use egui::Color32;

// --- ground: never ink ---
/// Window and panel fill.
pub const BASE: Color32 = Color32::from_rgb(18, 18, 24);
/// The one card fill. There is exactly one.
pub const SURFACE: Color32 = Color32::from_rgb(24, 24, 31);
/// Recessed scroll containers (the tape).
pub const WELL: Color32 = Color32::from_rgb(14, 14, 19);
/// Stateless 1px borders, and the unlit status dot. Never text.
pub const HAIRLINE: Color32 = Color32::from_rgb(48, 48, 60);

// --- ink: never a state ---
/// Every measured number, and the headline of whatever is currently true.
pub const INK: Color32 = Color32::from_rgb(232, 234, 242);
/// Prose, labels, units, section headings. The default label ink.
pub const INK_DIM: Color32 = Color32::from_rgb(160, 165, 182);
/// Caveats and footnotes. Never an instruction, never a number.
pub const INK_FAINT: Color32 = Color32::from_rgb(126, 131, 150);

// --- state: never data ---
/// NOBD's own mechanism is running and doing its job right now.
/// NEVER a grade on a measured number — LIVE describes NOBD, never you.
pub const LIVE: Color32 = Color32::from_rgb(64, 208, 126);
/// This is clickable, or it reveals more. An affordance, not a highlighter.
pub const ACTION: Color32 = Color32::from_rgb(0, 180, 216);
/// There is one specific thing only you can do, and the way to do it is on this
/// screen. NEVER a state the user chose (sync off), never a measured number, and
/// never without a control or an imperative sentence beside it.
pub const NEEDS_YOU: Color32 = Color32::from_rgb(240, 176, 64);
/// NOBD failed at something it promised, and you did not cause it. NEVER a
/// percentage, never a stray or bounce, never a destructive control at rest.
pub const BROKEN: Color32 = Color32::from_rgb(240, 92, 92);
