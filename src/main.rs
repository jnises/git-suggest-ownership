use anyhow::{anyhow, Result};
use git2::{BlameOptions, ObjectType, Repository, TreeWalkMode, TreeWalkResult};
use indicatif::{ProgressBar, ProgressStyle};
use log::{debug, info, warn};
use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    path::{Path, PathBuf},
};
use structopt::StructOpt;
use thread_local::ThreadLocal;

#[derive(Debug, StructOpt)]
#[structopt(
    about = r#"List the files that currently have lines that were changed by you.
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

    /// Show percentage changed per directory
    #[structopt(long)]
    tree: bool,
}

fn get_repo() -> Result<Repository> {
    Ok(Repository::discover(".")?)
}

/// returns (lines by user with email, total lines) for the file at path
fn get_lines_in_file<T: AsRef<str>>(
    repo: &Repository,
    path: &Path,
    emails: &[T],
) -> Result<(usize, usize)> {
    let blame = repo.blame_file(path, Some(BlameOptions::new().use_mailmap(true)))?;
    Ok(blame.iter().fold((0, 0), |acc, hunk| {
        let lines = hunk.lines_in_hunk();
        let by_user = hunk
            .final_signature()
            .email()
            .map(|e| emails.iter().any(|x| x.as_ref() == e))
            .unwrap_or(false);
        (acc.0 + lines * by_user as usize, acc.1 + lines)
    }))
}

struct File<'a> {
    path: &'a PathBuf,
    lines_by_user: usize,
    total_lines: usize,
}

impl<'a> File<'a> {
    fn ratio_changed(&self) -> f64 {
        self.lines_by_user as f64 / self.total_lines as f64
    }
}

fn print_files_sorted_percentage(mut files: Vec<File>, reverse: bool, all: bool) {
    files.sort_by(|a, b| {
        let x = (b.ratio_changed()).partial_cmp(&a.ratio_changed()).unwrap();
        if reverse {
            x.reverse()
        } else {
            x
        }
    });
    for file in files {
        let ratio = file.ratio_changed();
        if all || ratio > 0.0 {
            println!("{:>5.1}% - {}", ratio * 100.0, file.path.to_string_lossy());
        }
    }
}

fn print_tree_sorted_percentage(files: &Vec<File>, reverse: bool, all: bool) {
    #[derive(Default)]
    struct Node<'a> {
        name: &'a OsStr,
        lines_by_user: usize,
        total_lines: usize,
        children: BTreeMap<&'a str, Box<Node<'a>>>,
    }

    impl<'a> Node<'a> {
        fn ratio_changed(&self) -> f64 {
            self.lines_by_user as f64 / self.total_lines as f64
        }
    }

    impl<'a> Node<'a> {
        fn new(name: &'a OsStr) -> Self {
            Self {
                name,
                ..Default::default()
            }
        }
    }

    let mut root = Node::new(OsStr::new("/"));
    for f in files {
        let mut node = &mut root;
        node.lines_by_user += f.lines_by_user;
        node.total_lines += f.total_lines;
        for p in f.path.iter() {
            node = node
                .children
                .entry(p.to_str().unwrap())
                .or_insert_with(|| Box::new(Node::new(p)));
            node.lines_by_user += f.lines_by_user;
            node.total_lines += f.total_lines;
        }
    }

    fn print_node<'a>(node: &Node<'a>, reverse: bool, all: bool, prefix: &str) {
        println!(
            "{} - {:.1}%",
            node.name.to_string_lossy(),
            node.lines_by_user as f64 / node.total_lines as f64 * 100.0
        );
        let sorted_children = {
            let mut children: Vec<_> = node
                .children
                .values()
                .filter(|c| all || c.lines_by_user > 0)
                .collect();
            children.sort_by(|a, b| {
                let x = (b.ratio_changed()).partial_cmp(&a.ratio_changed()).unwrap();
                if reverse {
                    x.reverse()
                } else {
                    x
                }
            });
            children
        };
        let mut it = sorted_children.into_iter().peekable();
        while let Some(child) = it.next() {
            print!("{prefix}");
            if it.peek().is_none() {
                print!("╰── ");
                print_node(child, reverse, all, &format!("{prefix}    "));
            } else {
                print!("├── ");
                print_node(child, reverse, all, &format!("{prefix}│   "));
            }
        }
    }
    print_node(&root, reverse, all, "");
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
    info!("Looking for lines made by email(s) {emails:?}");
    let head = repo.head()?.peel_to_tree()?;
    let progress = if opt.no_progress || opt.verbose > 0 {
        ProgressBar::hidden()
    } else {
        ProgressBar::new_spinner()
    };
    let mut paths = vec![];
    head.walk(TreeWalkMode::PreOrder, |root, entry| {
        if let Some(ObjectType::Blob) = entry.kind() {
            if let Some(name) = entry.name() {
                let path = PathBuf::from(format!("{root}{name}"));
                paths.push(path);
            } else {
                warn!("no name for entry in {root}");
            }
        }
        TreeWalkResult::Ok
    })?;
    progress.set_style(ProgressStyle::default_bar());
    progress.set_length(paths.len() as u64);
    let repo_tls: ThreadLocal<Repository> = ThreadLocal::new();
    // TODO limit max number of threads? the user can set it using RAYON_NUM_THREADS by default
    let files: Vec<_> = paths
        .par_iter()
        .filter_map(|path| {
            debug!("{}", path.to_string_lossy());
            let repo = repo_tls.get_or_try(get_repo).expect("unable to get repo");
            let (lines_by_user, total_lines) =
                get_lines_in_file(repo, path, &emails).expect("error blaming file");
            progress.inc(1);
            if total_lines > 0 {
                Some(File {
                    path,
                    lines_by_user,
                    total_lines,
                })
            } else {
                None
            }
        })
        .collect();

    if opt.tree {
        print_tree_sorted_percentage(&files, opt.reverse, opt.all);
    } else {
        print_files_sorted_percentage(files, opt.reverse, opt.all);
    }

    Ok(())
}
