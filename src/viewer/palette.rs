//! Dynamic wallpaper palette (`~/.theme/palette.json`) and system dark-mode
//! (`~/.theme/mode` + GTK `gtk-application-prefer-dark-theme`) resolution.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WallpaperPalette {
    pub primary_hex: String,
    pub secondary_hex: String,
    pub primary_rgb: String,
    pub secondary_rgb: String,
    pub on_primary_hex: String,
}

/// Parse `$HOME/.theme/palette.json` (`primary_rgb` / `secondary_rgb` with hex
/// fallback) and compute WCAG 2.1 relative luminance for `on_primary_hex`.
pub fn parse_palette_json(raw: &str) -> Option<WallpaperPalette> {
    let val: Value = serde_json::from_str(raw).ok()?;
    let obj = val.as_object()?;

    let p_hex_raw = obj
        .get("primary_hex")
        .or_else(|| obj.get("primaryHex"))
        .and_then(Value::as_str);
    let s_hex_raw = obj
        .get("secondary_hex")
        .or_else(|| obj.get("secondaryHex"))
        .and_then(Value::as_str);
    let p_rgb_raw = obj
        .get("primary_rgb")
        .or_else(|| obj.get("primaryRgb"))
        .and_then(Value::as_str);
    let s_rgb_raw = obj
        .get("secondary_rgb")
        .or_else(|| obj.get("secondaryRgb"))
        .and_then(Value::as_str);

    let (pr, pg, pb) = p_rgb_raw
        .and_then(parse_rgb_triplet)
        .or_else(|| p_hex_raw.and_then(parse_hex_rgb))?;
    let (sr, sg, sb) = s_rgb_raw
        .and_then(parse_rgb_triplet)
        .or_else(|| s_hex_raw.and_then(parse_hex_rgb))?;

    let primary_hex = p_hex_raw
        .and_then(normalize_hex_color)
        .unwrap_or_else(|| format!("#{pr:02x}{pg:02x}{pb:02x}"));
    let secondary_hex = s_hex_raw
        .and_then(normalize_hex_color)
        .unwrap_or_else(|| format!("#{sr:02x}{sg:02x}{sb:02x}"));

    let lum = relative_luminance(pr, pg, pb);
    // WCAG 2.1 relative-luminance crossover against Flexoki ink `#100F0F` vs `#FFFCF0`.
    let on_primary_hex = if lum > 0.179 {
        "#100F0F".to_string()
    } else {
        "#FFFCF0".to_string()
    };

    Some(WallpaperPalette {
        primary_hex,
        secondary_hex,
        primary_rgb: format!("{pr}, {pg}, {pb}"),
        secondary_rgb: format!("{sr}, {sg}, {sb}"),
        on_primary_hex,
    })
}

pub fn load_wallpaper_palette() -> Option<WallpaperPalette> {
    let home = dirs::home_dir()?;
    let raw = std::fs::read_to_string(home.join(".theme/palette.json")).ok()?;
    parse_palette_json(&raw)
}

pub fn detect_system_dark() -> bool {
    if let Some(home) = dirs::home_dir()
        && let Ok(mode) = std::fs::read_to_string(home.join(".theme/mode"))
    {
        let m = mode.trim();
        if m.eq_ignore_ascii_case("dark") {
            return true;
        }
        if m.eq_ignore_ascii_case("light") {
            return false;
        }
    }
    gtk4::Settings::default().is_some_and(|s| s.is_gtk_application_prefer_dark_theme())
}

fn parse_rgb_triplet(s: &str) -> Option<(u8, u8, u8)> {
    let mut parts = s.split(',');
    let r = parts.next()?.trim().parse::<u8>().ok()?;
    let g = parts.next()?.trim().parse::<u8>().ok()?;
    let b = parts.next()?.trim().parse::<u8>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((r, g, b))
}

fn parse_hex_rgb(s: &str) -> Option<(u8, u8, u8)> {
    let hex = s.trim().strip_prefix('#').unwrap_or(s.trim());
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

fn normalize_hex_color(s: &str) -> Option<String> {
    let (r, g, b) = parse_hex_rgb(s)?;
    Some(format!("#{r:02x}{g:02x}{b:02x}"))
}

fn srgb_channel_linear(c: u8) -> f64 {
    let v = f64::from(c) / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn relative_luminance(r: u8, g: u8, b: u8) -> f64 {
    0.2126 * srgb_channel_linear(r)
        + 0.7152 * srgb_channel_linear(g)
        + 0.0722 * srgb_channel_linear(b)
}
