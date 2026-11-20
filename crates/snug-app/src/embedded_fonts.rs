//! Losslessly compressed copies of eframe 0.32.3's default font data.
//!
//! XPRESS_HUFF is expanded before font parsing and checked against the upstream
//! source hashes. Font bytes, face indices, tweaks and fallback order stay intact.

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

use eframe::egui::{FontData, FontDefinitions, FontFamily, FontTweak};

const HACK: &[u8] = include_bytes!("../assets/fonts/hack.xpress");
const UBUNTU: &[u8] = include_bytes!("../assets/fonts/ubuntu-light.xpress");
const NOTO_EMOJI: &[u8] = include_bytes!("../assets/fonts/noto-emoji.xpress");
const EMOJI_ICON: &[u8] = include_bytes!("../assets/fonts/emoji-icon.xpress");

struct FontAsset {
    name: &'static str,
    compressed: &'static [u8],
    original_len: usize,
    sha256: &'static str,
    scale: f32,
}

const ASSETS: [FontAsset; 4] = [
    FontAsset {
        name: "Hack",
        compressed: HACK,
        original_len: 309_408,
        sha256: "15f55cc0c85a2988d2b4b3a8cdb5d77fdfbaf319e1bb5309d725db9818fb7125",
        scale: 1.0,
    },
    FontAsset {
        name: "Ubuntu-Light",
        compressed: UBUNTU,
        original_len: 361_676,
        sha256: "80307b8da7649aa4ee4d484b232140e3ce1ec0ca093073d3c53c8f5a5ced7a70",
        scale: 1.0,
    },
    FontAsset {
        name: "NotoEmoji-Regular",
        compressed: NOTO_EMOJI,
        original_len: 418_804,
        sha256: "415dc6290378574135b64c808dc640c1df7531973290c4970c51fdeb849cb0c5",
        scale: 0.81,
    },
    FontAsset {
        name: "emoji-icon-font",
        compressed: EMOJI_ICON,
        original_len: 317_864,
        sha256: "3a706840d2f888b83c184f7d41900dbaa0151a806c4d130cc88539d53fedef0a",
        scale: 0.90,
    },
];

pub fn definitions(cjk_font: Option<Vec<u8>>) -> io::Result<FontDefinitions> {
    let mut font_data = BTreeMap::new();
    for asset in ASSETS {
        let bytes =
            snug_win::compression::decompress_xpress_huffman(asset.compressed, asset.original_len)?;
        let actual = snug_win::update::sha256_hex(&bytes);
        if !actual.eq_ignore_ascii_case(asset.sha256) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("内置字体 {} 的 SHA-256 不匹配", asset.name),
            ));
        }
        let data = FontData::from_owned(bytes);
        let data = if asset.scale == 1.0 {
            data
        } else {
            data.tweak(FontTweak {
                scale: asset.scale,
                ..Default::default()
            })
        };
        font_data.insert(asset.name.to_owned(), Arc::new(data));
    }

    let mut proportional = vec![
        "Ubuntu-Light".to_owned(),
        "NotoEmoji-Regular".to_owned(),
        "emoji-icon-font".to_owned(),
    ];
    let mut monospace = vec![
        "Hack".to_owned(),
        "Ubuntu-Light".to_owned(),
        "NotoEmoji-Regular".to_owned(),
        "emoji-icon-font".to_owned(),
    ];
    if let Some(bytes) = cjk_font {
        font_data.insert("cjk".to_owned(), Arc::new(FontData::from_owned(bytes)));
        proportional.insert(0, "cjk".to_owned());
        monospace.insert(0, "cjk".to_owned());
    }
    Ok(FontDefinitions {
        font_data,
        families: BTreeMap::from([
            (FontFamily::Proportional, proportional),
            (FontFamily::Monospace, monospace),
        ]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_exact_eframe_default_font_metadata_and_fallback_order() {
        let definitions = definitions(None).unwrap();
        assert_eq!(definitions.font_data.len(), 4);
        assert_eq!(
            definitions.families[&FontFamily::Proportional],
            ["Ubuntu-Light", "NotoEmoji-Regular", "emoji-icon-font"]
        );
        assert_eq!(
            definitions.families[&FontFamily::Monospace],
            [
                "Hack",
                "Ubuntu-Light",
                "NotoEmoji-Regular",
                "emoji-icon-font"
            ]
        );
        assert_eq!(definitions.font_data["Hack"].index, 0);
        assert_eq!(definitions.font_data["Ubuntu-Light"].index, 0);
        assert_eq!(definitions.font_data["NotoEmoji-Regular"].index, 0);
        assert_eq!(definitions.font_data["emoji-icon-font"].index, 0);
        assert_eq!(definitions.font_data["Hack"].tweak, FontTweak::default());
        assert_eq!(
            definitions.font_data["Ubuntu-Light"].tweak,
            FontTweak::default()
        );
        assert_eq!(
            definitions.font_data["NotoEmoji-Regular"].tweak,
            FontTweak {
                scale: 0.81,
                ..Default::default()
            }
        );
        assert_eq!(
            definitions.font_data["emoji-icon-font"].tweak,
            FontTweak {
                scale: 0.90,
                ..Default::default()
            }
        );
    }

    #[test]
    fn embedded_fonts_render_ascii_and_original_symbol_fallback_without_system_fonts() {
        let context = eframe::egui::Context::default();
        context.set_fonts(definitions(None).unwrap());
        let _ = context.run(eframe::egui::RawInput::default(), |context| {
            context.fonts(|fonts| {
                for family in [FontFamily::Proportional, FontFamily::Monospace] {
                    let font = eframe::egui::FontId::new(16.0, family);
                    assert!(fonts.has_glyphs(&font, "Snug 0123 ABC xyz"));
                    assert!(fonts.has_glyphs(&font, "· × — • … ‹ › −"));
                }
                let proportional = eframe::egui::FontId::proportional(16.0);
                assert!(fonts.has_glyphs(&proportional, "⏏ ⚙ 📱 📷 🔍"));
                let monospace = eframe::egui::FontId::monospace(16.0);
                assert!(fonts.has_glyphs(&monospace, "→ ● ⏏ ⚙ 📱 📷 🔍"));
            });
        });
    }

    #[test]
    fn cjk_font_is_inserted_first_but_never_required_for_embedded_fallbacks() {
        let bundled = definitions(None).unwrap();
        let ubuntu_bytes = bundled.font_data["Ubuntu-Light"].as_ref().as_ref().to_vec();
        let with_cjk = definitions(Some(ubuntu_bytes)).unwrap();
        assert_eq!(with_cjk.families[&FontFamily::Proportional][0], "cjk");
        assert_eq!(with_cjk.families[&FontFamily::Monospace][0], "cjk");
        assert_eq!(with_cjk.font_data.len(), 5);
        assert_eq!(bundled.font_data.len(), 4);
    }
}
