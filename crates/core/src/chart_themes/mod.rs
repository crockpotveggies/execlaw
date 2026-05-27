//! Chart theme presets — JSON `config` blocks merged onto Vega-Lite
//! specs by the ReplyRouter chart renderer.
//!
//! The two presets (`execlaw_dark` and `execlaw_light`) are derived
//! by hand from the SCSS tokens in `web/src/styles/theme.scss`. A
//! follow-up TODO is to generate these at build time from the
//! actual tokens so a token rename in CSS can't silently desync the
//! chart theme.

use serde::Deserialize;

const DARK_JSON: &str = include_str!("./execlaw_dark.json");
const LIGHT_JSON: &str = include_str!("./execlaw_light.json");

/// One preset slice — strips the wrapping `{ "$schema": ..., "config": {...} }`
/// and returns the `config` block ready for merging into a spec.
pub fn dark_config() -> serde_json::Value {
    parse_config(DARK_JSON)
}

pub fn light_config() -> serde_json::Value {
    parse_config(LIGHT_JSON)
}

fn parse_config(s: &str) -> serde_json::Value {
    #[derive(Deserialize)]
    struct Wrapper {
        config: serde_json::Value,
    }
    let w: Wrapper = serde_json::from_str(s).expect("baked theme JSON must parse");
    w.config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_config_includes_axis_label_color() {
        let cfg = dark_config();
        let label_color = cfg
            .get("axis")
            .and_then(|a| a.get("labelColor"))
            .and_then(|v| v.as_str())
            .unwrap();
        // $text-muted from theme.scss
        assert_eq!(label_color, "#7d8590");
    }

    #[test]
    fn light_config_includes_background_white() {
        let cfg = light_config();
        let bg = cfg
            .get("background")
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(bg, "#ffffff");
    }

    #[test]
    fn category_palettes_have_8_colors_each() {
        let dark = dark_config();
        let light = light_config();
        for cfg in [&dark, &light] {
            let cats = cfg
                .get("range")
                .and_then(|r| r.get("category"))
                .and_then(|c| c.as_array())
                .unwrap();
            assert_eq!(cats.len(), 8, "category palette must have 8 colors");
        }
    }
}
