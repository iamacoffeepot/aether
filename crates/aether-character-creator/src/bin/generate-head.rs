#![allow(clippy::print_stdout, reason = "the generator CLI reports its output artifact")]

use std::{env, error::Error, fs, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    let output = env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("crates/aether-character-creator/assets/aether-head.glb"), PathBuf::from);
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let glb = aether_character_creator::generate_head_glb()?;
    fs::write(&output, &glb)?;
    println!("wrote {} bytes to {}", glb.len(), output.display());
    Ok(())
}
