//! Reproduce the original runtime ICO decode and Lanczos3 resize with image.
//! Run through generate_tray_icons.ps1 so image matches the workspace lockfile.

use std::{error::Error, fs, path::PathBuf};

// Keep this algorithm identical to the former tray::decode_icon implementation.
fn decode_icon(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Ico).ok()?;
    let rgba = image.to_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    const TARGET: u32 = 32;
    if width.min(height) > TARGET {
        let scaled =
            image::imageops::resize(&rgba, TARGET, TARGET, image::imageops::FilterType::Lanczos3);
        Some((scaled.into_raw(), TARGET, TARGET))
    } else {
        Some((rgba.into_raw(), width, height))
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let root = PathBuf::from(args.next().ok_or("missing repository root")?);
    let check = match args.next() {
        None => false,
        Some(arg) if arg == "--check" => true,
        _ => return Err("expected only an optional --check argument".into()),
    };
    if args.next().is_some() {
        return Err("unexpected extra argument".into());
    }

    let assets = root.join("crates/snug-app/assets");
    for name in ["idle", "connected"] {
        let source = assets.join(format!("snugwin-{name}.ico"));
        let output = assets.join(format!("snugwin-{name}-32x32.rgba"));
        let (raw, width, height) =
            decode_icon(&fs::read(&source)?).ok_or("cannot decode source ICO")?;
        assert_eq!((width, height), (32, 32), "{}", source.display());
        assert_eq!(raw.len(), 32 * 32 * 4, "{}", source.display());
        if check {
            assert_eq!(fs::read(&output)?, raw, "{} differs", output.display());
            println!("verified all 1024 RGBA pixels: {}", output.display());
        } else {
            fs::write(&output, raw)?;
            println!("generated 32x32 RGBA: {}", output.display());
        }
    }
    Ok(())
}
