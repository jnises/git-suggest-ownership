use anyhow::{anyhow, Context, Result};
use git2::{
    BlameOptions, Diff, DiffFindOptions, DiffOptions, FileMode, ObjectType, Oid, Patch, Repository,
    TreeWalkMode, TreeWalkResult,
};
use indicatif::{ProgressBar, ProgressStyle};
use log::{debug, info, warn};
use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};
use std::{
    cell::RefCell,
    cmp,
    collections::HashMap,
    path::{Path, PathBuf},
};
use structopt::StructOpt;
use thread_local::ThreadLocal;

#[derive(Debug, StructOpt)]
#[structopt(
    about = r#"List the files that currently have files that were changed by you.
    Sorted by percentage of lines you changed for each file."#
)]
struct Opt {
    /// Start with the files with the smallest percentage
    #[structopt(short, long)]
    reverse: bool,

    /// Verbose mode (-v, -vv, -vvv, etc), disables progress bar
    #[structopt(short, long, parse(from_occurrences))]
    verbose: usize,

    /// Don't display a progress bar
    #[structopt(long)]
    no_progress: bool,

    /// Include all files, even the ones with no lines changed by you
    #[structopt(short, long)]
    all: bool,

    /// Your email address. You can specify multiple. Defaults to your configured `config.email`
    #[structopt(long)]
    email: Vec<String>,
    // TODO add option to limit the search to only the current directory
}

fn get_repo() -> Result<Repository> {
    Ok(Repository::discover(".")?)
}

/// returns (lines by user with email, total lines) for the file at path
fn get_lines_in_file(
    repo: &Repository,
    path: &Path,
    emails: &Vec<String>,
) -> Result<(usize, usize)> {
    let blame = repo.blame_file(path, Some(BlameOptions::new().use_mailmap(true)))?;
    Ok(blame.iter().fold((0, 0), |acc, hunk| {
        let lines = hunk.lines_in_hunk();
        let by_user = hunk
            .final_signature()
            .email()
            .map(|e| emails.iter().any(|x| x == e))
            .unwrap_or(false);
        (acc.0 + lines * by_user as usize, acc.1 + lines)
    }))
}

fn main() -> Result<()> {
    let opt = Opt::from_args();
    stderrlog::new().verbosity(opt.verbose).init()?;
    let repo = get_repo()?;
    let emails = if !opt.email.is_empty() {
        opt.email.clone()
    } else {
        vec![repo
            .signature()?
            .email()
            .ok_or_else(|| anyhow!("bad email configured"))?
            .to_string()]
    };
    let head = repo.head()?.peel_to_tree()?;
    let progress = if opt.no_progress || opt.verbose > 0 {
        ProgressBar::hidden()
    } else {
        ProgressBar::new_spinner()
    };
    let mut paths = vec![];
    head.walk(TreeWalkMode::PreOrder, |root, entry| {
        if let Some(ObjectType::Blob) = entry.kind() {
            let path = PathBuf::from(format!("{root}{}", entry.name().unwrap()));
            paths.push(path);
        }
        TreeWalkResult::Ok
    })?;
    progress.set_style(ProgressStyle::default_bar());
    progress.set_length(paths.len() as u64);
    let repo_tls: ThreadLocal<Repository> = ThreadLocal::new();
    // TODO limit max number of threads?
    let mut files: Vec<_> = paths
        .par_iter()
        .filter_map(|path| {
            let repo = repo_tls.get_or_try(get_repo).expect("unable to get repo");
            let (lines_by_user, total_lines) =
                get_lines_in_file(&repo, &path, &emails).expect("error blaming file");
            progress.inc(1);
            if (opt.all || lines_by_user > 0) && total_lines > 0 {
                Some((path, lines_by_user as f64 / total_lines as f64))
            } else {
                None
            }
        })
        .collect();

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
            "{:>5.1}% - {}",
            percentage_authored * 100.0,
            path.to_string_lossy()
        );
    }

    Ok(())
}
