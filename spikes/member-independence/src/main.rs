mod diff;
mod eval;
mod extract;
mod git;
mod independent;
mod refs;
mod resolve;
mod tokens;

use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Result, bail};

fn main() {
    if let Err(err) = run() {
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        print_usage();
        bail!("missing command");
    }
    let cmd = args.remove(0);
    let repo = git::repo_root()?;
    match cmd.as_str() {
        "diff" => cmd_diff(&repo, &args),
        "refs" => cmd_refs(&repo, &args),
        "independent" => cmd_independent(&repo, &args),
        "eval" => cmd_eval(&repo, &args),
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(())
        }
        other => {
            print_usage();
            bail!("unknown command {other}");
        }
    }
}

fn print_usage() {
    eprintln!(
        "\
symdiff diff <base> <head> [--paths <prefix>...]
symdiff refs <rev> <symbol-name>
symdiff independent <base> <headA> <headB>
symdiff independent --parents <headA> <headB>
symdiff eval [--base <rev>] [--prepare <rev>]... [--candidates <rev,rev,...>]
             [--oracle-dir <path>] [--skip-compile]

`eval` replays every candidate's parent-relative patch onto one shared base and
refuses to report precision unless that base compiles. `--prepare` names the
repair patches that make it compile, in the order they replay."
    );
}

fn cmd_diff(repo: &Path, args: &[String]) -> Result<()> {
    let (base, head, prefixes) = parse_diff_args(args)?;
    let start = Instant::now();
    let changes = diff::diff_revs(repo, &base, &head, &prefixes)?;
    let mut stdout = io::stdout().lock();
    for change in &changes {
        writeln!(stdout, "{}", change.render())?;
    }
    eprintln!("{} items, {}ms", changes.len(), start.elapsed().as_millis());
    Ok(())
}

fn parse_diff_args(args: &[String]) -> Result<(String, String, Vec<String>)> {
    let mut base = None;
    let mut head = None;
    let mut prefixes = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--paths" {
            i += 1;
            while i < args.len() && !args[i].starts_with('-') {
                prefixes.push(args[i].clone());
                i += 1;
            }
            continue;
        }
        if base.is_none() {
            base = Some(args[i].clone());
        } else if head.is_none() {
            head = Some(args[i].clone());
        } else {
            bail!("unexpected argument {}", args[i]);
        }
        i += 1;
    }
    match (base, head) {
        (Some(b), Some(h)) => Ok((b, h, prefixes)),
        _ => bail!("diff requires <base> <head>"),
    }
}

fn cmd_refs(repo: &Path, args: &[String]) -> Result<()> {
    if args.len() < 2 {
        bail!("refs requires <rev> <symbol-name>");
    }
    let rev = &args[0];
    let name = &args[1];
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        bail!("symbol-name must be a bare identifier");
    }
    let hits = refs::find_refs(repo, rev, name)?;
    let mut stdout = io::stdout().lock();
    for hit in &hits {
        writeln!(stdout, "{}", hit.render())?;
    }
    Ok(())
}

fn cmd_independent(repo: &Path, args: &[String]) -> Result<()> {
    let mut parents = false;
    let pos: Vec<&str> = args
        .iter()
        .filter(|a| {
            if *a == "--parents" {
                parents = true;
                false
            } else {
                true
            }
        })
        .map(String::as_str)
        .collect();
    let report = if parents {
        if pos.len() != 2 {
            bail!("independent --parents requires <headA> <headB>");
        }
        independent::independent_parents(repo, pos[0], pos[1])?
    } else {
        if pos.len() != 3 {
            bail!("independent requires <base> <headA> <headB>");
        }
        independent::independent(repo, pos[0], pos[1], pos[2])?
    };
    print!("{}", independent::render(&report));
    eprintln!(
        "{}ms (A {} items, B {} items)",
        report.elapsed_millis, report.changes_a, report.changes_b
    );
    Ok(())
}

fn cmd_eval(repo: &Path, args: &[String]) -> Result<()> {
    let options = parse_eval_args(args)?;
    let bundle = eval::run_eval(repo, &options)?;
    let report = repo.join("spikes/member-independence/REPORT-round-2.md");
    eval::write_report(&bundle, &report)?;
    eprintln!("wrote {}", report.display());
    Ok(())
}

fn parse_eval_args(args: &[String]) -> Result<eval::EvalOptions> {
    let mut options = eval::EvalOptions::default();
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        match flag {
            "--skip-compile" => index += 1,
            "--base" | "--prepare" | "--candidates" | "--oracle-dir" => {
                let Some(value) = args.get(index + 1) else {
                    bail!("{flag} requires a value");
                };
                match flag {
                    "--base" => options.base = value.clone(),
                    "--prepare" => options.prepare.push(value.clone()),
                    "--candidates" => {
                        options.candidates = value
                            .split(',')
                            .map(str::trim)
                            .filter(|rev| !rev.is_empty())
                            .map(str::to_string)
                            .collect();
                    }
                    _ => options.oracle_dir = PathBuf::from(value),
                }
                index += 2;
            }
            other => bail!("unknown eval argument {other}"),
        }
    }
    options.skip_compile = args.iter().any(|arg| arg == "--skip-compile");
    if options.candidates.len() < 2 {
        bail!("eval needs at least two candidates");
    }
    Ok(options)
}
