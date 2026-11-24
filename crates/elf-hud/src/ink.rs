//! The ink colour: white on dark screens, near-black on light ones. The decision is made from
//! the mean luminance of the screen strip above the overlay, sampled with `grim`.

use serde::Deserialize;
use std::process::Command;

/// How the ink is chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ink {
    #[default]
    Auto,
    White,
    Dark,
}

impl Ink {
    pub fn parse(s: &str) -> Option<Ink> {
        Some(match s {
            "auto" => Ink::Auto,
            "white" => Ink::White,
            "dark" => Ink::Dark,
            _ => return None,
        })
    }
}

/// Switch to dark ink above this mean luminance, back to white below the lower one. The gap
/// keeps the colour from flickering on a background near the middle.
pub const DARK_ABOVE: f32 = 0.45;
pub const WHITE_BELOW: f32 = 0.35;

/// Whether the ink should be dark, given the strip's luminance and the current ink.
pub fn decide(dark: bool, luminance: f32) -> bool {
    if luminance > DARK_ABOVE {
        true
    } else if luminance < WHITE_BELOW {
        false
    } else {
        dark
    }
}

fn linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// Relative luminance (Rec. 709 weights) of an sRGB colour, 0..1 per channel.
pub fn luminance(rgb: [f32; 3]) -> f32 {
    0.2126 * linear(rgb[0]) + 0.7152 * linear(rgb[1]) + 0.0722 * linear(rgb[2])
}

/// Mean luminance of 8-bit RGB pixels.
pub fn mean_luminance(rgb: &[u8]) -> Option<f32> {
    let mut table = [0.0f32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        *t = linear(i as f32 / 255.0);
    }
    let pixels = rgb.chunks_exact(3);
    let n = pixels.len();
    if n == 0 {
        return None;
    }
    let sum: f32 = pixels
        .map(|p| 0.2126 * table[p[0] as usize] + 0.7152 * table[p[1] as usize] + 0.0722 * table[p[2] as usize])
        .sum();
    Some(sum / n as f32)
}

/// The pixel bytes of a binary PPM (`P6`, 8 bits per channel): width, height, RGB data.
pub fn parse_ppm(data: &[u8]) -> Option<(usize, usize, &[u8])> {
    let mut pos = 0;
    let mut token = || -> Option<&[u8]> {
        loop {
            while data.get(pos)?.is_ascii_whitespace() {
                pos += 1;
            }
            if data[pos] != b'#' {
                break;
            }
            while *data.get(pos)? != b'\n' {
                pos += 1;
            }
        }
        let start = pos;
        while data.get(pos).is_some_and(|b| !b.is_ascii_whitespace()) {
            pos += 1;
        }
        Some(&data[start..pos])
    };
    let number = |t: &[u8]| std::str::from_utf8(t).ok()?.parse::<usize>().ok();
    if token()? != b"P6" {
        return None;
    }
    let (w, h, max) = (number(token()?)?, number(token()?)?, number(token()?)?);
    if max != 255 || w == 0 || h == 0 {
        return None;
    }
    // Exactly one whitespace byte separates the header from the pixels.
    let body = data.get(pos + 1..)?;
    (body.len() >= w * h * 3).then(|| (w, h, &body[..w * h * 3]))
}

/// The strip `[x, y, w, h]` (global layout pixels) as seen on screen. `None` when `grim` is
/// missing or fails, or the output is not a PPM; nothing is written to disk.
pub fn sample(strip: [f32; 4]) -> Option<f32> {
    let [x, y, w, h] = strip;
    let region = format!("{},{} {}x{}", x.round() as i32, y.round() as i32, w.ceil() as u32, h.ceil() as u32);
    let out = Command::new("grim").args(["-t", "ppm", "-g", &region, "-"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let (_, _, pixels) = parse_ppm(&out.stdout)?;
    mean_luminance(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ppm(w: usize, h: usize, rgb: [u8; 3]) -> Vec<u8> {
        let mut v = format!("P6\n{w} {h}\n255\n").into_bytes();
        for _ in 0..w * h {
            v.extend_from_slice(&rgb);
        }
        v
    }

    #[test]
    fn ppm_header_and_pixels() {
        let data = ppm(3, 2, [10, 20, 30]);
        let (w, h, px) = parse_ppm(&data).unwrap();
        assert_eq!((w, h, px.len()), (3, 2, 18));
        assert_eq!(&px[..3], &[10, 20, 30]);
    }

    #[test]
    fn ppm_with_comments_and_odd_whitespace() {
        let mut data = b"P6\n# made by grim\n2  1\r\n255\n".to_vec();
        data.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(parse_ppm(&data).unwrap(), (2, 1, &[1u8, 2, 3, 4, 5, 6][..]));
    }

    #[test]
    fn ppm_pixel_bytes_may_start_with_whitespace_values() {
        let mut data = b"P6\n1 1\n255\n".to_vec();
        data.extend_from_slice(&[10, 10, 10]);
        assert_eq!(parse_ppm(&data).unwrap().2, &[10, 10, 10]);
    }

    #[test]
    fn bad_ppm_is_rejected() {
        assert!(parse_ppm(b"").is_none());
        assert!(parse_ppm(b"P5\n1 1\n255\nxxx").is_none());
        assert!(parse_ppm(b"P6\n2 2\n255\nshort").is_none());
        assert!(parse_ppm(b"P6\n1 1\n65535\nxxxxxx").is_none());
        assert!(parse_ppm(b"P6\n0 1\n255\n").is_none());
        assert!(parse_ppm(b"P6\n1 1").is_none());
    }

    #[test]
    fn luminance_of_known_colours() {
        assert!((luminance([0.0; 3])).abs() < 1e-6);
        assert!((luminance([1.0; 3]) - 1.0).abs() < 1e-4);
        assert!((luminance([1.0, 0.0, 0.0]) - 0.2126).abs() < 1e-4);
        assert!((luminance([0.0, 1.0, 0.0]) - 0.7152).abs() < 1e-4);
        // Mid grey in sRGB is much darker than 0.5 in linear light.
        assert!((luminance([0.5; 3]) - 0.214).abs() < 0.002);
    }

    #[test]
    fn mean_luminance_averages_pixels() {
        let black_white = [0u8, 0, 0, 255, 255, 255];
        assert!((mean_luminance(&black_white).unwrap() - 0.5).abs() < 1e-4);
        assert_eq!(mean_luminance(&[]), None);
        let white = ppm(4, 4, [255, 255, 255]);
        let (_, _, px) = parse_ppm(&white).unwrap();
        assert!((mean_luminance(px).unwrap() - 1.0).abs() < 1e-4);
    }

    #[test]
    fn hysteresis_holds_between_the_thresholds() {
        assert!(decide(false, 0.46));
        assert!(!decide(true, 0.34));
        // In the gap the current ink stays.
        assert!(decide(true, 0.40));
        assert!(!decide(false, 0.40));
        assert!(decide(true, 0.45) && !decide(false, 0.45));
        assert!(!decide(true, 0.0) && decide(false, 1.0));
    }

    #[test]
    fn a_background_drifting_through_the_gap_does_not_flicker() {
        let mut dark = false;
        let mut flips = 0;
        for l in [0.30, 0.38, 0.44, 0.46, 0.42, 0.37, 0.40, 0.44, 0.36, 0.34, 0.40] {
            let next = decide(dark, l);
            flips += usize::from(next != dark);
            dark = next;
        }
        assert_eq!(flips, 2);
    }

    #[test]
    fn typical_screens() {
        // The light backdrop used for the strips is "light", the dark one is "dark".
        assert!(luminance([0.86, 0.85, 0.82]) > DARK_ABOVE);
        assert!(luminance([0.02, 0.022, 0.026]) < WHITE_BELOW);
        assert!(luminance([0.50, 0.55, 0.62]) < WHITE_BELOW);
    }

    #[test]
    fn ink_names() {
        assert_eq!(Ink::parse("auto"), Some(Ink::Auto));
        assert_eq!(Ink::parse("dark"), Some(Ink::Dark));
        assert_eq!(Ink::parse("red"), None);
    }
}
