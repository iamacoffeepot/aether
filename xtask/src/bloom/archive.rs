//! `cargo xtask bloom archive` — move records onto the archive tier, or list it.

use anyhow::Result;
use clap::Args;

use super::client::Client;
use super::dto::{ArchiveFailureView, ArchiveListView, ArchivePassView, ArchiveRecordView};

/// Move aged evidence directories and resolved session trees onto the archive
/// tier. Refuses unless the coordinator is between blooms. Nothing is ever
/// deleted.
#[derive(Args, Debug)]
pub struct ArchiveArgs {
    /// List the tier instead of running a pass.
    #[arg(long)]
    list: bool,
}

/// Run the pass or list the tier through the coordinator REST client.
pub fn run(client: &Client<'_>, args: &ArchiveArgs) -> Result<String> {
    if args.list {
        return Ok(render_list(&client.list_archive()?));
    }
    Ok(render_pass(&client.archive_pass()?))
}

fn render_pass(view: &ArchivePassView) -> String {
    let mut lines: Vec<String> = view.records.iter().map(render_record).collect();
    lines.extend(view.failures.iter().map(render_failure));
    lines.join("\n")
}

fn render_list(view: &ArchiveListView) -> String {
    view.records.iter().map(render_record).collect::<Vec<_>>().join("\n")
}

fn render_record(record: &ArchiveRecordView) -> String {
    format!("{} {} {}", record.class, record.name, record.path)
}

fn render_failure(failure: &ArchiveFailureView) -> String {
    format!("failed {} {}: {}", failure.class, failure.name, failure.error)
}
