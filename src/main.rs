use anyhow::{anyhow, Context, Result};
use git2::{
    BlameOptions, Diff, DiffFindOptions, DiffOptions, FileMode, ObjectType, Oid, Patch, Repository,
    TreeWalkMode, TreeWalkResult,
};
use log::{debug, info, warn};
use std::{
    cell::RefCell,
    cmp,
    collections::HashMap,
    path::{Path, PathBuf},
};
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
#[structopt(
    about = "List the files that currently have files that were changed by you. Sorted by percentage of lines you changed for each file."
)]
struct Opt {
    /// Verbose mode (-v, -vv, -vvv, etc), disables progress bar
    #[structopt(short, long, parse(from_occurrences))]
    verbose: usize,

    #[structopt(short, long)]
    reverse: bool,
}

fn get_repo() -> Result<Repository> {
    Ok(Repository::discover(".")?)
}

/// returns (lines by user with email, total lines) for the file at path
fn get_lines_in_file(repo: &Repository, path: &Path, email: &str) -> Result<(usize, usize)> {
    let blame = repo.blame_file(path, None)?;
    Ok(blame.iter().fold((0, 0), |acc, hunk| {
        let lines = hunk.lines_in_hunk();
        let by_user = hunk
            .final_signature()
            .email()
            .map(|e| e == email)
            .unwrap_or(false);
        (acc.0 + lines * by_user as usize, acc.1 + lines)
    }))
}

fn main() -> Result<()> {
    let opt = Opt::from_args();
    stderrlog::new().verbosity(opt.verbose).init()?;
    let repo = get_repo()?;
    let email = repo
        .signature()?
        .email()
        .ok_or_else(|| anyhow!("bad email configured"))?
        .to_string();
    let head = repo.head()?.peel_to_tree()?;
    let mut files: Vec<(PathBuf, f64)> = vec![];
    head.walk(TreeWalkMode::PreOrder, |root, entry| {
        //println!("{s}");
        if let Some(ObjectType::Blob) = entry.kind() {
            let path = PathBuf::from(format!("{root}{}", entry.name().unwrap()));
            let (lines_by_user, total_lines) = get_lines_in_file(&repo, &path, &email).unwrap();
            if lines_by_user > 0 && total_lines > 0 {
                files.push((path, lines_by_user as f64 / total_lines as f64));
            }
            //entry.
            //repo.blame_file(entry., opts)
            //println!("{s} - {}", entry.name().unwrap_or_default());
        }
        TreeWalkResult::Ok
    })?;

    files.sort_unstable_by(|(_, a), (_, b)| {
        let x = b.partial_cmp(a).unwrap();
        if opt.reverse {
            x.reverse()
        } else {
            x
        }
    });
    for (path, percentage_authored) in files {
        println!(
            "{:>6.1}% - {}",
            percentage_authored * 100.0,
            path.to_string_lossy()
        );
    }

    Ok(())
}
