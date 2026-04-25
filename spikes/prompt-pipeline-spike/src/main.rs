use anyhow::{Context, Result, bail};
use std::path::PathBuf;

mod cache;
mod claude;
mod compose;
mod distills;
mod facts;
mod frontmatter;
mod gemini;
mod grader;
mod lenses;
mod observers;
mod pipeline;
mod profiles;
mod recipes;
mod transforms;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut recipe_path: Option<PathBuf> = None;
    let mut profile_path: Option<PathBuf> = None;
    let mut do_generate: bool = false;
    let mut do_grade: bool = false;
    let mut grade_model: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--profile" => {
                profile_path = Some(
                    args.next()
                        .context("--profile requires a path argument")?
                        .into(),
                );
            }
            "--generate" => {
                do_generate = true;
            }
            "--grade" => {
                do_grade = true;
            }
            "--grade-model" => {
                grade_model = Some(
                    args.next()
                        .context("--grade-model requires a model name argument")?,
                );
            }
            other if other.starts_with("--") => {
                bail!("unknown flag: {other}");
            }
            other => {
                if recipe_path.is_some() {
                    bail!("unexpected positional argument: {other}");
                }
                recipe_path = Some(other.into());
            }
        }
    }

    if do_grade && !do_generate {
        bail!("--grade requires --generate (we need an image to grade)");
    }

    let recipe_path = recipe_path
        .context("usage: prompt-spike <recipe.toml> [--profile <profile.yaml>] [--generate] [--grade] [--grade-model <model>]")?;
    let profile = match &profile_path {
        Some(p) => Some(profiles::load(p)?),
        None => None,
    };

    let root = std::env::current_dir().context("getting current dir")?;
    let result = pipeline::run(&root, &recipe_path, profile.as_ref(), do_generate)?;

    if let Some(p) = &profile {
        eprintln!(
            "profile: {}",
            p.name.as_deref().unwrap_or("(unnamed)")
        );
    }

    println!("=== Per-fact framed blocks ===\n");
    for (id, block) in &result.blocks {
        println!("[{id}]\n{block}\n");
    }
    println!("=== Composed prompt ===\n");
    println!("{}", result.composed_prompt);

    if let Some(img) = &result.image {
        println!("\n=== Generated image ===\n");
        println!("model:       {}", img.model);
        println!("bytes:       {}", img.byte_len);
        println!("cache path:  {}", img.cache_path.display());
        if let Some(p) = &img.copied_to {
            println!("copied to:   {}", p.display());
        }

        if do_grade {
            let grader_refs: Vec<grader::GraderReference> = img
                .references
                .iter()
                .map(|r| grader::GraderReference {
                    bytes: r.bytes.clone(),
                    mime_type: r.mime_type.clone(),
                    label: r.label.clone().unwrap_or_else(|| r.path.clone()),
                })
                .collect();

            let report = grader::grade(
                &img.bytes,
                &img.mime_type,
                &grader_refs,
                &result.recipe,
                &result.composed_prompt,
                &result.facts,
                &result.environmentals,
                grade_model.as_deref(),
            )?;

            println!("\n=== Grading report ===\n");
            println!("{report}");
        }
    }

    Ok(())
}
