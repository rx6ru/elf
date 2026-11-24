//! Where the overlay goes and how big it is, from the monitor it appears on.
//!
//! Hyprland reports each monitor's pixel size, scale, rotation and place in the global layout
//! (`hyprctl -j monitors`). Everything here is in logical pixels, the unit of the compositor's
//! layout and of Rune's points.

use serde::Deserialize;
use std::process::Command;

use crate::settings::{Anchor, DEFAULT_EXPONENT, FOCUSED, HudSettings};

/// The overlay's design size in points, at scale 1 (a 1080p screen).
pub const BASE_SIZE: (f32, f32) = (225.0, 120.0);
/// By default size grows with the shorter side of the screen as a square root: 1:1 growth makes
/// the HUD huge on big screens, while the eye wants it only modestly larger.
pub const SIZE_EXPONENT: f32 = DEFAULT_EXPONENT;
pub const SCALE_MIN: f32 = 0.8;
pub const SCALE_MAX: f32 = 1.45;
/// The shorter side of the screen at which the scale is 1.
const REFERENCE_SIDE: f32 = 1080.0;
/// In portrait the overlay may take at most this share of the screen width.
const PORTRAIT_MAX_WIDTH: f32 = 0.6;
/// The default gap from the anchor edge is a share of the screen height: it has to clear bars
/// and widgets, which do not shrink with the screen.
const MARGIN_FRACTION: f32 = 0.065;
const MARGIN_MIN: f32 = 24.0;
const MARGIN_MAX: f32 = 160.0;
/// The colour sample is this tall, in design points, and sits right next to the overlay.
const STRIP_HEIGHT: f32 = 24.0;

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Monitor {
    pub name: String,
    /// Mode size in pixels, before rotation.
    pub width: u32,
    pub height: u32,
    #[serde(default = "one")]
    pub scale: f32,
    /// 0..7; 1, 3, 5 and 7 turn the screen a quarter, which swaps width and height.
    #[serde(default)]
    pub transform: u32,
    pub x: i32,
    pub y: i32,
    #[serde(default)]
    pub focused: bool,
    /// Space taken by bars: left, top, right, bottom.
    #[serde(default)]
    pub reserved: [i32; 4],
}

fn one() -> f32 {
    1.0
}

impl Monitor {
    /// Width and height in logical pixels, after rotation.
    pub fn logical_size(&self) -> (f32, f32) {
        let (w, h) = (self.width as f32 / self.scale, self.height as f32 / self.scale);
        if self.transform % 2 == 1 { (h, w) } else { (w, h) }
    }
}

/// Where the overlay goes: its size, its edge, and where that puts it in the layout.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub size: (f32, f32),
    /// The shape is drawn this many times its design size.
    pub zoom: f32,
    pub anchor: Anchor,
    /// From the anchor edge of the area bars leave free (Rune's layer surface stays inside it).
    pub margin: f32,
    /// `None`: centred in the free area. `Some`: this far from its left edge.
    pub inset: Option<f32>,
    /// The overlay's rectangle in the global layout: x, y, width, height.
    pub rect: [f32; 4],
    /// The screen strip right next to it (above it when anchored at the bottom, below it when
    /// anchored at the top), whose colour decides the ink: x, y, width, height.
    pub strip: [f32; 4],
}

/// The size multiplier for a screen whose logical size is `logical`.
pub fn scale_for(logical: (f32, f32), exponent: f32) -> f32 {
    (logical.0.min(logical.1) / REFERENCE_SIDE).powf(exponent).clamp(SCALE_MIN, SCALE_MAX)
}

/// The default distance from the anchor edge: 6.5% of the height, within 24..160 pixels.
pub fn margin_for(logical_height: f32) -> f32 {
    (MARGIN_FRACTION * logical_height).round().clamp(MARGIN_MIN, MARGIN_MAX)
}

pub fn layout(m: &Monitor, cfg: &HudSettings) -> Layout {
    let (lw, lh) = m.logical_size();
    let mut zoom = scale_for((lw, lh), cfg.scale_exponent) * cfg.size;
    if lh > lw {
        zoom = zoom.min(PORTRAIT_MAX_WIDTH * lw / BASE_SIZE.0);
    }
    let size = (BASE_SIZE.0 * zoom, BASE_SIZE.1 * zoom);
    let margin = cfg.y_offset.map_or_else(|| margin_for(lh), |f| (f * lh).round());
    let [left, top, right, bottom] = m.reserved.map(|r| r as f32);
    let free_w = lw - left - right;

    let (inset, rel_x) = if cfg.x_offset == 0.0 {
        (None, (free_w - size.0) / 2.0)
    } else {
        // The offset is measured from the centre of the whole monitor, then kept on screen.
        let from_left = ((lw - size.0) / 2.0 + cfg.x_offset * lw - left).clamp(0.0, (free_w - size.0).max(0.0));
        (Some(from_left), from_left)
    };
    let x = m.x as f32 + left + rel_x;
    let height = STRIP_HEIGHT * zoom;
    let (y, strip_y, strip_h) = match cfg.anchor {
        Anchor::Bottom => {
            let y = m.y as f32 + lh - bottom - margin - size.1;
            // Never reaches above the screen (or under a top bar) on a tiny screen.
            let strip_y = (y - height).max(m.y as f32 + top);
            (y, strip_y, (y - strip_y).max(1.0))
        }
        Anchor::Top => {
            let y = m.y as f32 + top + margin;
            let below = y + size.1;
            (y, below, height.min((m.y as f32 + lh - below).max(1.0)))
        }
    };
    Layout { size, zoom, anchor: cfg.anchor, margin, inset, rect: [x, y, size.0, size.1], strip: [x, strip_y, size.0, strip_h] }
}

pub fn parse_monitors(json: &str) -> Vec<Monitor> {
    serde_json::from_str(json).unwrap_or_default()
}

/// The monitor to use: the one named (unless `which` is "focused"), else the focused one, else
/// the first.
pub fn pick<'a>(monitors: &'a [Monitor], which: &str) -> Option<&'a Monitor> {
    monitors
        .iter()
        .find(|m| which != FOCUSED && m.name == which)
        .or_else(|| monitors.iter().find(|m| m.focused))
        .or(monitors.first())
}

pub fn query() -> Vec<Monitor> {
    Command::new("hyprctl")
        .args(["-j", "monitors"])
        .output()
        .map(|o| parse_monitors(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(w: u32, h: u32, scale: f32, transform: u32) -> Monitor {
        Monitor { name: "M".into(), width: w, height: h, scale, transform, x: 0, y: 0, focused: true, reserved: [0; 4] }
    }

    fn auto() -> HudSettings {
        HudSettings::default()
    }

    fn lay(m: &Monitor) -> Layout {
        layout(m, &auto())
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn logical_size_divides_by_scale_and_swaps_on_odd_transforms() {
        assert_eq!(mon(1920, 1080, 1.0, 0).logical_size(), (1920.0, 1080.0));
        assert_eq!(mon(3840, 2160, 2.0, 0).logical_size(), (1920.0, 1080.0));
        assert_eq!(mon(1920, 1080, 1.0, 2).logical_size(), (1920.0, 1080.0));
        for t in [1, 3, 5, 7] {
            assert_eq!(mon(1920, 1080, 1.0, t).logical_size(), (1080.0, 1920.0), "transform {t}");
        }
        assert_eq!(mon(2560, 1440, 1.5, 1).logical_size(), (960.0, 1706.6666));
    }

    #[test]
    fn scale_is_the_square_root_of_the_ratio_within_bounds() {
        let s = |w, h| scale_for((w, h), SIZE_EXPONENT);
        assert!(close(s(1920.0, 1080.0), 1.0));
        assert!(close(s(2560.0, 1440.0), 1.1547));
        assert!(close(s(1280.0, 720.0), 0.8165));
        assert!(close(s(3840.0, 2160.0), 1.4142));
        // The shorter side counts, so a portrait 1080x1920 screen is 1.0.
        assert!(close(s(1080.0, 1920.0), 1.0));
        assert_eq!(s(300.0, 200.0), SCALE_MIN);
        assert_eq!(s(7680.0, 4320.0), SCALE_MAX);
    }

    #[test]
    fn the_exponent_bends_the_curve() {
        assert!(close(scale_for((2160.0, 3840.0), 1.0), 1.45)); // linear: 2.0, clamped
        assert!(close(scale_for((1440.0, 2560.0), 1.0), 1.3333));
        assert_eq!(scale_for((3840.0, 2160.0), 0.0), 1.0); // fixed size
        assert_eq!(scale_for((1280.0, 720.0), 0.0), 1.0);
    }

    #[test]
    fn margin_follows_the_height_within_bounds() {
        assert_eq!(margin_for(1080.0), 70.0);
        assert_eq!(margin_for(1440.0), 94.0);
        assert_eq!(margin_for(1920.0), 125.0);
        assert_eq!(margin_for(200.0), 24.0);
        assert_eq!(margin_for(4000.0), 160.0);
    }

    #[test]
    fn layout_1080p_is_the_design_size_centred() {
        let l = lay(&mon(1920, 1080, 1.0, 0));
        assert_eq!(l.size, (225.0, 120.0));
        assert_eq!((l.zoom, l.margin, l.inset, l.anchor), (1.0, 70.0, None, Anchor::Bottom));
        assert_eq!(l.rect, [847.5, 1080.0 - 70.0 - 120.0, 225.0, 120.0]);
        // The strip is 24 px tall, right above the overlay, as wide as it.
        assert_eq!(l.strip, [847.5, 890.0 - 24.0, 225.0, 24.0]);
    }

    #[test]
    fn layout_1440p_grows_modestly() {
        let l = lay(&mon(2560, 1440, 1.0, 0));
        assert!(close(l.size.0, 259.81) && close(l.size.1, 138.56), "{:?}", l.size);
        assert_eq!(l.margin, 94.0);
        assert!(close(l.rect[0], (2560.0 - l.size.0) / 2.0));
        assert!(close(l.rect[1], 1440.0 - 94.0 - l.size.1));
    }

    #[test]
    fn layout_4k_at_scale_2_matches_1080p() {
        assert_eq!(lay(&mon(3840, 2160, 2.0, 0)), lay(&mon(1920, 1080, 1.0, 0)));
    }

    #[test]
    fn layout_portrait_centres_on_the_narrow_width() {
        let l = lay(&mon(1080, 1920, 1.0, 0));
        assert_eq!(l.size, (225.0, 120.0));
        assert_eq!(l.margin, 125.0);
        assert_eq!(l.rect, [427.5, 1920.0 - 125.0 - 120.0, 225.0, 120.0]);
    }

    #[test]
    fn rotated_monitors_lay_out_like_portrait_ones() {
        for t in [1, 3] {
            assert_eq!(lay(&mon(1920, 1080, 1.0, t)), lay(&mon(1080, 1920, 1.0, 0)), "transform {t}");
        }
    }

    #[test]
    fn a_narrow_portrait_screen_caps_the_width() {
        let l = lay(&mon(250, 900, 1.0, 0));
        assert!(close(l.size.0, 150.0) && close(l.zoom, 150.0 / 225.0));
        assert!(close(l.size.1 / l.size.0, 120.0 / 225.0), "proportions kept");
    }

    #[test]
    fn position_is_in_the_global_layout_and_clears_bars() {
        let mut m = mon(1920, 1080, 1.0, 0);
        m.x = 7180;
        m.y = 360;
        m.reserved = [0, 32, 0, 32];
        let l = lay(&m);
        assert_eq!(l.rect, [7180.0 + 847.5, 360.0 + 1080.0 - 32.0 - 70.0 - 120.0, 225.0, 120.0]);
        assert_eq!(l.strip[1] + l.strip[3], l.rect[1]);
    }

    #[test]
    fn x_offset_is_a_fraction_of_the_logical_width_on_landscape() {
        let cfg = HudSettings { x_offset: 0.1, ..auto() };
        let l = layout(&mon(1920, 1080, 1.0, 0), &cfg);
        // 0.1 of 1920 right of the centre.
        assert!(close(l.rect[0], 847.5 + 192.0), "{:?}", l.rect);
        assert!(close(l.inset.unwrap(), 847.5 + 192.0));
        let l = layout(&mon(1920, 1080, 1.0, 0), &HudSettings { x_offset: -0.25, ..auto() });
        assert!(close(l.rect[0], 847.5 - 480.0));
        // The same fraction on a 4K screen at scale 2 lands in the same logical place.
        let l4 = layout(&mon(3840, 2160, 2.0, 0), &cfg);
        assert!(close(l4.rect[0], l.rect[0] + 480.0 + 192.0));
    }

    #[test]
    fn x_offset_follows_the_rotated_width_on_portrait() {
        let cfg = HudSettings { x_offset: 0.2, ..auto() };
        for t in [0, 1, 3] {
            let m = if t == 0 { mon(1080, 1920, 1.0, 0) } else { mon(1920, 1080, 1.0, t) };
            let l = layout(&m, &cfg);
            // 0.2 of the narrow 1080 width, not of 1920.
            assert!(close(l.rect[0], 427.5 + 216.0), "transform {t}: {:?}", l.rect);
        }
    }

    #[test]
    fn x_offset_cannot_push_the_overlay_off_screen() {
        let l = layout(&mon(1920, 1080, 1.0, 0), &HudSettings { x_offset: 0.5, ..auto() });
        assert_eq!(l.rect[0] + l.rect[2], 1920.0);
        let l = layout(&mon(1920, 1080, 1.0, 0), &HudSettings { x_offset: -0.5, ..auto() });
        assert_eq!(l.rect[0], 0.0);
    }

    #[test]
    fn y_offset_is_a_fraction_of_the_logical_height() {
        let cfg = HudSettings { y_offset: Some(0.2), ..auto() };
        let l = layout(&mon(1920, 1080, 1.0, 0), &cfg);
        assert_eq!(l.margin, 216.0);
        assert_eq!(l.rect[1], 1080.0 - 216.0 - 120.0);
        // On portrait the fraction is of the tall side.
        let p = layout(&mon(1080, 1920, 1.0, 0), &cfg);
        assert_eq!(p.margin, 384.0);
        // An explicit fraction is not held to the 24..160 pixel range.
        let tiny = layout(&mon(1920, 1080, 1.0, 0), &HudSettings { y_offset: Some(0.0), ..auto() });
        assert_eq!(tiny.margin, 0.0);
    }

    #[test]
    fn anchor_top_sits_under_the_top_bar_and_samples_below() {
        let mut m = mon(1920, 1080, 1.0, 0);
        m.reserved = [0, 32, 0, 0];
        let l = layout(&m, &HudSettings { anchor: Anchor::Top, ..auto() });
        assert_eq!((l.anchor, l.margin), (Anchor::Top, 70.0));
        assert_eq!(l.rect[1], 32.0 + 70.0);
        assert_eq!(l.strip[1], l.rect[1] + l.rect[3]);
        assert_eq!(l.strip[3], 24.0);
    }

    #[test]
    fn size_multiplies_the_curved_scale() {
        let l = layout(&mon(1920, 1080, 1.0, 0), &HudSettings { size: 1.5, ..auto() });
        assert_eq!((l.zoom, l.size), (1.5, (337.5, 180.0)));
        let fixed = layout(&mon(3840, 2160, 1.0, 0), &HudSettings { scale_exponent: 0.0, ..auto() });
        assert_eq!(fixed.size, (225.0, 120.0));
    }

    #[test]
    fn a_named_monitor_is_chosen_else_the_focused_one() {
        let json = r#"[{"name":"eDP-1","width":1920,"height":1080,"x":0,"y":0,"focused":true},
            {"name":"HDMI-A-1","width":2560,"height":1440,"x":1920,"y":0}]"#;
        let ms = parse_monitors(json);
        assert_eq!(ms.len(), 2);
        assert_eq!(pick(&ms, "focused").unwrap().name, "eDP-1");
        assert_eq!(pick(&ms, "HDMI-A-1").unwrap().name, "HDMI-A-1");
        assert_eq!(pick(&ms, "DP-9").unwrap().name, "eDP-1");
        assert_eq!(pick(&ms[1..], "focused").unwrap().name, "HDMI-A-1");
        assert!(pick(&[], "focused").is_none());
    }

    #[test]
    fn monitors_parse_from_hyprctl_json() {
        let json = r#"[{"id":0,"name":"eDP-1","width":1920,"height":1080,"refreshRate":60.0,"x":7180,"y":360,
            "scale":1.00,"transform":0,"focused":true,"reserved":[0,32,0,32]},
            {"name":"HDMI-A-1","width":2560,"height":1440,"x":4620,"y":0,"scale":1.5,"transform":1}]"#;
        let ms = parse_monitors(json);
        assert_eq!(ms[0].reserved, [0, 32, 0, 32]);
        assert_eq!((ms[1].scale, ms[1].transform, ms[1].focused), (1.5, 1, false));
        assert!(parse_monitors("nope").is_empty());
    }
}
