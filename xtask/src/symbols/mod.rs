//! `cargo xtask symbols` — workspace symbol inventory for lanes and gates.
//!
//! Token clone-detection cannot see a private `fn digest` redefined under
//! the same name in another crate's `#[cfg(test)]` module. This command
//! walks every workspace crate with `syn`, including test modules and
//! `tests/` trees, and answers "does the workspace already have this?"
//! over the shell.

mod diff;
mod extract;
mod query;
mod table;
mod walk;

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use crate::cargo::write_json_pretty;
use crate::symbols::table::Table;

/// Default cap on `find` rows so a lane can read the answer in one call.
const DEFAULT_FIND_LIMIT: usize = 64;

#[derive(Args, Debug)]
pub struct SymbolsArgs {
    #[command(subcommand)]
    command: SymbolsCommand,
}

#[derive(Subcommand, Debug)]
enum SymbolsCommand {
    /// Emit a JSON symbol table for every workspace crate.
    Build(BuildArgs),
    /// Find symbols by name substring or normalized-name similarity.
    Find(FindArgs),
    /// Report symbols the working tree introduces relative to a stored table.
    Diff(DiffArgs),
}

#[derive(Args, Debug)]
struct BuildArgs {
    /// Write the table to this path instead of stdout.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct FindArgs {
    /// Name fragment to search; case and underscores fold for similarity.
    query: String,
    /// Previously stored table from `symbols build`. Rebuilds when omitted.
    #[arg(long)]
    table: Option<PathBuf>,
    /// Maximum rows to print. `0` prints every match. Defaults to 64.
    #[arg(long, default_value_t = DEFAULT_FIND_LIMIT)]
    limit: usize,
}

#[derive(Args, Debug)]
struct DiffArgs {
    /// Previously stored table from `symbols build`.
    base_table: PathBuf,
}

pub fn run(args: &SymbolsArgs) -> Result<()> {
    match &args.command {
        SymbolsCommand::Build(args) => run_build(args),
        SymbolsCommand::Find(args) => run_find(args),
        SymbolsCommand::Diff(args) => run_diff(args),
    }
}

fn run_build(args: &BuildArgs) -> Result<()> {
    let table = walk::build_workspace_table()?;
    emit_table(&table, args.out.as_deref())
}

fn run_find(args: &FindArgs) -> Result<()> {
    let table = load_or_build(args.table.as_deref())?;
    let found = query::find(&table, &args.query, args.limit);
    print_json(&found)
}

fn run_diff(args: &DiffArgs) -> Result<()> {
    let base = Table::load(&args.base_table)?;
    let current = walk::build_workspace_table()?;
    print_json(&diff::diff(&base, &current))
}

fn load_or_build(table: Option<&Path>) -> Result<Table> {
    table.map_or_else(walk::build_workspace_table, Table::load)
}

fn emit_table(table: &Table, out: Option<&Path>) -> Result<()> {
    out.map_or_else(
        || {
            io::stdout().write_all(table.to_json()?.as_bytes()).context("write symbol output")?;
            Ok(())
        },
        |path| write_json_pretty(path, table),
    )
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    let mut json = serde_json::to_string_pretty(value).context("serialize json")?;
    json.push('\n');
    io::stdout().write_all(json.as_bytes()).context("write symbol output")?;
    Ok(())
}
