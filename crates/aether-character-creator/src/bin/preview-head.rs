#![allow(clippy::print_stdout, reason = "the preview CLI reports its output artifact")]

use std::{env, error::Error, fs, io, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    let asset = PathBuf::from("crates/aether-character-creator/assets/aether-head.glb");
    let checked_in = fs::read(&asset)?;
    let regenerated = aether_character_creator::generate_head_glb()?;
    if checked_in != regenerated {
        return Err(io::Error::other("checked-in GLB does not match the generator").into());
    }

    let output = env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("crates/aether-character-creator/assets/aether-head-preview.png"), PathBuf::from);
    let png = aether_character_creator::render_head_preview_png(900, 900)?;
    fs::write(&output, &png)?;
    println!("wrote {} bytes to {}", png.len(), output.display());
    Ok(())
}
