use anyhow::{anyhow, Result};
use chrono::{DateTime, FixedOffset, NaiveDateTime, Utc};
use clap::{command, Parser};
use git2::{BlameOptions, ObjectType, Repository, TreeWalkMode, TreeWalkResult};
use indicatif::{ProgressBar, ProgressStyle};
use log::{debug, error, info, trace, warn};
use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};
use std::{
    cmp::Ordering,
    collections::BTreeMap,
    ffi::OsStr,
    path::{Path, PathBuf},
    str::FromStr,
};
use thread_local::ThreadLocal;

/// List the files and directories that currently have lines that were changed by you.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
struct Opt {
    /// Start with the files with the smallest percentage
    #[arg(long, conflicts_with_all = &["show_authors", "max_authors"])]
    reverse: bool,

    /// Verbose mode (-v, -vv, -vvv, etc), disables progress bar
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Don't display a progress bar
    #[arg(long)]
    no_progress: bool,

    /// Include all files, even the ones with no lines changed by you
    #[arg(long, conflicts_with_all = &["show_authors", "max_authors"])]
    all: bool,

    /// Your email address. You can specify multiple. Defaults to your configured `config.email`
    #[arg(long, conflicts_with_all = &["show_authors", "max_authors"])]
    email: Vec<String>,

    /// Show percentage changed per file
    #[arg(long)]
    flat: bool,

    /// Show the top authors of each file or directory
    #[arg(long, conflicts_with_all = &["email", "all", "reverse"])]
    show_authors: bool,

    #[arg(long, default_value_t = 3, conflicts_with_all = &["email", "all", "reverse"], requires = "show_authors")]
    max_authors: u32,

    /// Limit to the specified directory. Defaults to the entire repo.
    #[arg(short, long)]
    dir: Option<PathBuf>,

    /// Email of users to ignore. You can specify multiple. Useful if someone has stopped working on the project.
    #[arg(long)]
    ignore_user: Vec<String>,

    /// Don't count commits older than this. Example: 6M
    #[arg(long)]
    max_age: Option<humantime::Duration>,
    // TODO add option to limit the depth of tree printed
}

#[derive(Default)]
struct Contributions {
    authors: BTreeMap<String, usize>,
    total_lines: usize,
}

impl Contributions {
    // TODO max commit age arg?
    fn try_from_path(
        repo: &Repository,
        path: &Path,
        max_age: &Option<chrono::Duration>,
    ) -> Result<Self> {
        let blame = repo.blame_file(path, Some(BlameOptions::new().use_mailmap(true)))?;
        Ok(blame.iter().fold(Self::default(), |mut acc, hunk| {
            let lines = hunk.lines_in_hunk();
            let signature = hunk.final_signature();
            let when = signature.when();
            let commit_time = DateTime::<FixedOffset>::from_local(
                NaiveDateTime::from_timestamp_opt(when.seconds(), 0).expect("valid timestamp"),
                FixedOffset::east_opt(when.offset_minutes() * 60).unwrap_or_else(|| {
                    // TODO handle error better?
                    warn!(
                        "Invalid timezone offset: {}. Defaulting to 0.",
                        when.offset_minutes()
                    );
                    FixedOffset::east_opt(0).unwrap()
                }),
            )
            .with_timezone(&Utc);
            let age = Utc::now() - commit_time;
            if let Some(max_age) = max_age {
                if age > *max_age {
                    return acc;
                }
            }
            if let Some(email) = signature.email() {
                acc.total_lines += lines;
                *acc.authors.entry(email.to_owned()).or_default() += lines;
            } else {
                // TODO keep track of unauthored hunks somehow?
                warn!("hunk without email found in {}", path.display());
            }
            acc
        }))
    }

    // TODO `ignored_users` will probably not get large enough to warrant a HashSet?
    fn filter_ignored(&mut self, ignored_users: &[impl AsRef<str>]) {
        self.authors.retain(|k, v| {
            if ignored_users.iter().any(|ignored| k == ignored.as_ref()) {
                self.total_lines -= *v;
                false
            } else {
                true
            }
        });
    }

    fn lines_by_user<S: AsRef<str>>(&self, author: &[S]) -> usize {
        self.authors
            .iter()
            .filter_map(|(key, value)| {
                author
                    .iter()
                    .any(|email| email.as_ref() == key)
                    .then_some(value)
            })
            .sum()
    }

    fn ratio_changed_by_user<S: AsRef<str>>(&self, author: &[S]) -> f64 {
        let lines_by_user = self.lines_by_user(author);
        lines_by_user as f64 / self.total_lines as f64
    }

    fn authors_str(&self, num_authors: usize) -> String {
        let mut authors = self
            .authors
            .iter()
            .map(|(email, lines)| (email.clone(), *lines as f64 / self.total_lines as f64))
            .collect::<Vec<_>>();
        authors.sort_by(|(_, a), (_, b)| b.partial_cmp(a).unwrap_or(Ordering::Equal));
        authors.truncate(num_authors);
        let author_str = authors
            .into_iter()
            .map(|(email, contribution)| format!("{email}: {:.1}%", contribution * 100.0))
            .collect::<Vec<_>>()
            .join(", ");
        format!("({author_str})")
    }
}

struct File<'a> {
    path: &'a Path,
    contributions: Contributions,
}

fn print_files_sorted_percentage<S: AsRef<str>>(
    files: &[File],
    author: &[S],
    reverse: bool,
    all: bool,
) {
    let mut contributions_by_author = files
        .iter()
        .map(|f| (f.path, f.contributions.ratio_changed_by_user(author)))
        .collect::<Vec<_>>();
    contributions_by_author.sort_by(|(_, a), (_, b)| {
        let x = b.partial_cmp(a).unwrap_or(Ordering::Equal);
        if reverse {
            x.reverse()
        } else {
            x
        }
    });
    for (path, ratio) in contributions_by_author {
        if all || ratio > 0.0 {
            println!("{:>5.1}% - {}", ratio * 100.0, path.display());
        }
    }
}

fn print_file_authors(files: &[File], num_authors: usize) {
    for f in files {
        println!(
            "{} - {}",
            f.path.display(),
            f.contributions.authors_str(num_authors)
        );
    }
}

fn print_tree_sorted_percentage<S: AsRef<str>>(
    files: &[File],
    author: &[S],
    reverse: bool,
    all: bool,
) {
    #[derive(Default)]
    struct Node<'a> {
        name: &'a OsStr,
        lines_by_user: usize,
        total_lines: usize,
        children: BTreeMap<&'a OsStr, Box<Node<'a>>>,
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
        node.lines_by_user += f.contributions.lines_by_user(author);
        node.total_lines += f.contributions.total_lines;
        for p in f.path.iter() {
            node = node
                .children
                .entry(p)
                .or_insert_with(|| Box::new(Node::new(p)));
            node.lines_by_user += f.contributions.lines_by_user(author);
            node.total_lines += f.contributions.total_lines;
        }
    }

    fn print_node(node: &Node, reverse: bool, all: bool, prefix: &str) {
        println!(
            "{} - {:.1}%",
            node.name.to_string_lossy(),
            node.ratio_changed() * 100.0
        );
        let sorted_children = {
            let mut children: Vec<_> = node
                .children
                .values()
                .filter(|c| all || c.lines_by_user > 0)
                .collect();
            children.sort_by(|a, b| {
                let x = (b.ratio_changed())
                    .partial_cmp(&a.ratio_changed())
                    .unwrap_or(Ordering::Equal);
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
                print!("└── ");
                print_node(child, reverse, all, &format!("{prefix}    "));
            } else {
                print!("├── ");
                print_node(child, reverse, all, &format!("{prefix}│   "));
            }
        }
    }
    print_node(&root, reverse, all, "");
}

fn print_tree_authors(files: &[File], num_authors: usize) {
    #[derive(Default)]
    struct Node<'a> {
        // TODO is this needed?
        name: &'a OsStr,
        contributions: Contributions,
        children: BTreeMap<&'a OsStr, Box<Node<'a>>>,
    }

    impl<'a> Node<'a> {
        fn new(name: &'a OsStr) -> Self {
            Self {
                name,
                ..Default::default()
            }
        }

        fn add_contribution(&mut self, contributions: &Contributions) {
            self.contributions.total_lines += contributions.total_lines;
            for (author, lines) in &contributions.authors {
                *self
                    .contributions
                    .authors
                    .entry(author.clone())
                    .or_default() += lines;
            }
        }
    }

    let mut root = Node::new(OsStr::new("/"));
    for f in files {
        let mut node = &mut root;
        node.add_contribution(&f.contributions);
        for p in f.path.iter() {
            node = node
                .children
                .entry(p)
                .or_insert_with(|| Box::new(Node::new(p)));
            node.add_contribution(&f.contributions);
        }
    }

    fn print_node<'a>(node: &Node<'a>, prefix: &str, num_authors: usize) {
        println!(
            "{} - {}",
            node.name.to_string_lossy(),
            node.contributions.authors_str(num_authors)
        );
        let mut it = node.children.iter().peekable();
        while let Some((_, child)) = it.next() {
            print!("{prefix}");
            if it.peek().is_none() {
                print!("└── ");
                print_node(child, &format!("{prefix}    "), num_authors);
            } else {
                print!("├── ");
                print_node(child, &format!("{prefix}│   "), num_authors);
            }
        }
    }
    print_node(&root, "", num_authors);
}

fn main() -> Result<()> {
    let opt = Opt::parse();
    stderrlog::new().verbosity(opt.verbose as usize).init()?;
    let limit_dir = opt.dir.is_some();
    let root = opt
        .dir
        .clone()
        .unwrap_or_else(|| PathBuf::from_str(".").unwrap());
    let canonical_root = root.canonicalize()?;
    info!("dir: {}", canonical_root.display());
    let get_repo = || -> Result<_> { Ok(Repository::discover(&root)?) };

    let repo = get_repo()?;
    let repo_root = repo.workdir().unwrap_or_else(|| repo.path());
    info!("repo: {}", repo_root.display());
    let emails = if !opt.email.is_empty() {
        opt.email.clone()
    } else {
        vec![repo
            .signature()?
            .email()
            .ok_or_else(|| anyhow!("bad email configured"))?
            .to_string()]
    };
    let max_age = opt
        .max_age
        .map(|d| {
            info!("max age: {d}");
            chrono::Duration::from_std(d.into()).expect("bad duration")
        });
    info!("Looking for lines made by email(s) {emails:?}");
    let head = repo.head()?.peel_to_tree()?;
    let progress = if opt.no_progress || opt.verbose > 0 {
        ProgressBar::hidden()
    } else {
        ProgressBar::new_spinner()
    };
    let mut paths = vec![];
    head.walk(TreeWalkMode::PreOrder, |dir, entry| {
        if let Some(ObjectType::Blob) = entry.kind() {
            if let Some(name) = entry.name() {
                let path = PathBuf::from(format!("{dir}{name}"));
                if limit_dir {
                    let canonical_path = if let Ok(cpath) = repo_root.join(&path).canonicalize() {
                        cpath
                    } else {
                        error!("unable to get canonical version of {}", path.display());
                        return TreeWalkResult::Ok;
                    };
                    if canonical_path.starts_with(&canonical_root) {
                        paths.push(path);
                    } else {
                        debug!(
                            "{} not in {} skipping.",
                            path.display(),
                            canonical_root.display()
                        );
                    }
                } else {
                    paths.push(path);
                }
            } else {
                warn!("no name for entry in {dir}");
            }
        }
        TreeWalkResult::Ok
    })?;
    if opt.dir.is_some() {
        info!("blaming limited to: {paths:?}");
    } else {
        info!("blaming all paths");
    }
    progress.set_style(ProgressStyle::default_bar());
    progress.set_length(paths.len() as u64);
    let repo_tls: ThreadLocal<Repository> = ThreadLocal::new();
    // TODO limit max number of threads? the user can set it using RAYON_NUM_THREADS by default
    let mut files: Vec<_> = paths
        .par_iter()
        .filter_map(|path| {
            debug!("blaming {}", path.display());
            let repo = repo_tls.get_or_try(get_repo).expect("unable to get repo");
            let contributions = Contributions::try_from_path(repo, path, &max_age);
            progress.inc(1);
            let contributions = match contributions {
                Ok(c) => c,
                Err(e) => {
                    warn!("Error blaming file {} ({e})", path.display());
                    return None;
                }
            };
            if contributions.total_lines > 0 {
                Some(File {
                    path,
                    contributions,
                })
            } else {
                None
            }
        })
        .collect();
    progress.finish_and_clear();
    trace!("done blaming");
    files
        .iter_mut()
        .for_each(|f| f.contributions.filter_ignored(&opt.ignore_user));
    files.retain(|f| f.contributions.total_lines > 0);
    if opt.flat {
        if opt.show_authors {
            print_file_authors(&files, opt.max_authors as usize);
        } else {
            print_files_sorted_percentage(&files, &emails, opt.reverse, opt.all);
        }
    } else {
        #[allow(clippy::collapsible_else_if)]
        if opt.show_authors {
            print_tree_authors(&files, opt.max_authors as usize);
        } else {
            print_tree_sorted_percentage(&files, &emails, opt.reverse, opt.all);
        }
    }

    Ok(())
}
