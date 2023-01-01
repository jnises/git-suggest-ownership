use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::atomic::AtomicUsize,
};

use anyhow::{anyhow, Result};
use chrono::{DateTime, FixedOffset, NaiveDateTime, Utc};
use git2::{BlameOptions, DiffOptions, Repository};
use log::warn;
use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};
use thread_local::ThreadLocal;

#[derive(Default, Debug)]
pub(crate) struct Contributions {
    pub(crate) authors: BTreeMap<String, usize>,
    pub(crate) total_lines: usize,
}

impl Contributions {
    pub(crate) fn try_from_path(
        repo: &Repository,
        path: &Path,
        max_age: &Option<chrono::Duration>,
    ) -> Result<Self> {
        let blame = repo.blame_file(path, Some(BlameOptions::new().use_mailmap(true)))?;
        let mut s = Self::default();
        for hunk in blame.iter() {
            let lines = hunk.lines_in_hunk();
            let signature = hunk.final_signature();
            let when = signature.when();
            let commit_time = git_time_to_utc_datetime(when)?;
            let age = Utc::now() - commit_time;
            if let Some(max_age) = max_age {
                if age > *max_age {
                    continue;
                }
            }
            if let Some(email) = signature.email() {
                s.add_lines(email.to_owned(), lines);
            } else {
                // TODO keep track of unauthored hunks somehow?
                warn!("hunk without email found in {}", path.display());
            }
        }
        Ok(s)
    }

    pub(crate) fn calculate_with_overwritten_lines_from_paths(
        repo: &Repository,
        paths: &HashSet<PathBuf>,
        max_age: &Option<chrono::Duration>,
        progress: impl Fn(usize, usize) + Sync,
    ) -> Result<HashMap<PathBuf, Self>> {
        let completed = AtomicUsize::new(0);
        let mut walker = repo.revwalk()?;
        walker.push_head()?;
        log::debug!("counting commits");
        let commits: Vec<_> = walker
            .filter_map(|oid_res| {
                match oid_res {
                    Ok(oid) => {
                        let c = repo.find_commit(oid).unwrap();
                        if let Some(max_age) = max_age {
                            let Ok(time) = git_time_to_utc_datetime(c.time()) else {
                                log::warn!("Commit {} has no valid time. Ignoring.", c.id());
                                return None;
                            };
                            let age = Utc::now() - time;
                            if age > *max_age {
                                return None;
                            }
                        }
                        // we don't want to count merge commits. but perhaps we should somehow?
                        if c.parents().count() == 1 {
                            Some(oid)
                        } else {
                            None
                        }
                    }
                    Err(e) => {
                        log::warn!("error while walking commits: {e}");
                        None
                    }
                }
            })
            .collect();
        let num_commits = commits.len();
        log::debug!("calculating contributions");
        let root = repo.workdir().unwrap_or_else(|| repo.path());
        let get_repo = || -> Result<_> { Ok(Repository::discover(root)?) };
        let repo_tls: ThreadLocal<Repository> = ThreadLocal::new();
        let (contributions, _renames) = commits
            .par_iter()
            .map(|oid| -> Result<_> {
                let repo = repo_tls.get_or_try(get_repo).expect("unable to get repo");
                let mailmap = repo.mailmap().ok();
                let c = repo.find_commit(*oid).unwrap();
                debug_assert!(c.parents().count() == 1);
                let parent = c.parents().next().unwrap();
                let mut contributions: HashMap<PathBuf, Self> = HashMap::new();
                let mut renames = HashMap::new();

                let signature = if let Some(mm) = mailmap {
                    mm.resolve_signature(&c.author())?
                } else {
                    c.author()
                };
                if let Some(author) = signature.email() {
                    let mut diff = repo.diff_tree_to_tree(
                        Some(&parent.tree()?),
                        Some(&c.tree()?),
                        // TODO use some other diff options? patience?
                        Some(DiffOptions::new().context_lines(0)),
                    )?;
                    // TODO different options here?
                    diff.find_similar(None)?;
                    diff.foreach(
                        &mut |delta, _diff_progress| {
                            if delta.new_file().exists()
                                && delta.old_file().exists()
                                && delta.old_file().path() != delta.new_file().path()
                            {
                                let new = delta.new_file().path().unwrap().to_path_buf();
                                renames.insert(delta.old_file().path().unwrap().to_path_buf(), new);
                            }
                            true
                        },
                        None,
                        Some(&mut |delta, hunk| {
                            if !delta.new_file().exists()
                                || !matches!(
                                    delta.status(),
                                    git2::Delta::Added | git2::Delta::Modified
                                )
                            {
                                return true;
                            }
                            let new = delta.new_file().path().unwrap().to_path_buf();
                            if paths.contains(&new) {
                                // TODO is this a sensible way to calculate it? better to count lines added, removed, and changed properly?
                                let lines_changed = hunk.old_lines().max(hunk.new_lines());
                                contributions
                                    .entry(new)
                                    .or_default()
                                    .add_lines(author.to_string(), lines_changed as usize);
                            }
                            true
                        }),
                        None,
                    )?;
                } else {
                    log::warn!("Commit {} has no valid author email", c.id());
                }

                progress(
                    completed.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1,
                    num_commits,
                );
                Ok((contributions, renames))
            })
            .reduce(
                || Ok((HashMap::new(), HashMap::new())),
                |acc, other| {
                    let (acc_contributions, mut acc_renames) = acc?;
                    let (other_contributions, other_renames) = other?;
                    let mut mapped = HashMap::new();
                    // TODO is the rename handling done correctly?
                    for (path, contributions) in acc_contributions {
                        if let Some(new_path) = other_renames.get(&path) {
                            mapped.insert(new_path.clone(), contributions);
                        } else {
                            mapped.insert(path, contributions);
                        }
                    }
                    for (path, contributions) in other_contributions {
                        mapped.entry(path).or_default().merge(contributions);
                    }
                    'outer: for (other_old_path, other_new_path) in other_renames {
                        // TODO do this with a better data structure
                        for acc_new in acc_renames.values_mut() {
                            if *acc_new == other_old_path {
                                *acc_new = other_new_path;
                                // TODO or can there be multiple renames to the same path?
                                break 'outer;
                            }
                        }
                        acc_renames.insert(other_old_path, other_new_path);
                    }
                    Ok((mapped, acc_renames))
                },
            )?;
        Ok(contributions)
    }

    // TODO `ignored_users` will probably not get large enough to warrant a HashSet?
    pub(crate) fn filter_ignored(&mut self, ignored_users: &[impl AsRef<str>]) {
        self.authors.retain(|k, v| {
            if ignored_users.iter().any(|ignored| k == ignored.as_ref()) {
                self.total_lines -= *v;
                false
            } else {
                true
            }
        });
    }

    pub(crate) fn lines_by_user<S: AsRef<str>>(&self, author: &[S]) -> usize {
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

    pub(crate) fn ratio_changed_by_user<S: AsRef<str>>(&self, author: &[S]) -> f64 {
        let lines_by_user = self.lines_by_user(author);
        lines_by_user as f64 / self.total_lines as f64
    }

    pub(crate) fn authors_str(&self, num_authors: usize) -> String {
        let mut authors = self
            .authors
            .iter()
            .map(|(email, lines)| (email.clone(), *lines as f64 / self.total_lines as f64))
            .collect::<Vec<_>>();
        authors.sort_by(|(_, a), (_, b)| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        authors.truncate(num_authors);
        let author_str = authors
            .into_iter()
            .map(|(email, contribution)| format!("{email}: {:.1}%", contribution * 100.0))
            .collect::<Vec<_>>()
            .join(", ");
        format!("({author_str})")
    }

    fn add_lines(&mut self, author: String, lines: usize) {
        self.total_lines += lines;
        *self.authors.entry(author).or_default() += lines;
    }

    fn merge(&mut self, other: Self) {
        self.total_lines += other.total_lines;
        for (author, lines) in other.authors {
            *self.authors.entry(author).or_default() += lines;
        }
    }
}

fn git_time_to_utc_datetime(time: git2::Time) -> Result<DateTime<Utc>> {
    Ok(DateTime::<FixedOffset>::from_local(
        NaiveDateTime::from_timestamp_opt(time.seconds(), 0)
            .ok_or_else(|| anyhow!("Unable to convert commit time"))?,
        FixedOffset::east_opt(time.offset_minutes() * 60).unwrap_or_else(|| {
            // TODO handle error better?
            warn!(
                "Invalid timezone offset: {}. Defaulting to 0.",
                time.offset_minutes()
            );
            FixedOffset::east_opt(0).unwrap()
        }),
    )
    .with_timezone(&Utc))
}
