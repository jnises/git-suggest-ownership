use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{atomic::AtomicUsize, Arc},
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
        let commits: Vec<_> = {
            let mut v: Vec<_> = walker
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
            // oldest first
            v.reverse();
            v
        };
        let num_commits = commits.len();
        log::debug!("calculating contributions");
        let root = repo.workdir().unwrap_or_else(|| repo.path());
        let get_repo = || -> Result<_> { Ok(Repository::discover(root)?) };
        let repo_tls: ThreadLocal<Repository> = ThreadLocal::new();
        let contributions = commits
            //.par_iter()
            .iter()
            .map(|oid| -> Result<_> {
                let repo = repo_tls.get_or_try(get_repo).expect("unable to get repo");
                let mailmap = repo.mailmap()?;
                let c = repo.find_commit(*oid).unwrap();
                log::debug!("processing commit {}", c.id());
                debug_assert!(c.parents().count() == 1);
                let parent = c.parents().next().unwrap();
                let mut contributions: HashMap<PathBuf, Self> = HashMap::new();
                // TODO rename handling is broken. what happens if a file is renamed and then the original file is recreated later?
                let mut renames = HashMap::new();

                let signature = c.author_with_mailmap(&mailmap)?;
                if let Some(author) = signature.email() {
                    let mut diff = repo.diff_tree_to_tree(
                        Some(&parent.tree()?),
                        Some(&c.tree()?),
                        // TODO use some other diff options? patience?
                        Some(DiffOptions::new().context_lines(0)),
                    )?;
                    // TODO we get more sensible numbers if we don't use find_similar, but then we don't get renames
                    diff.find_similar(None)?;
                    diff.foreach(
                        &mut |delta, _diff_progress| {
                            // TODO keep track of added files as well
                            log::debug!("processing delta {:?}", delta);
                            if delta.old_file().exists()
                                && delta.old_file().path() != delta.new_file().path()
                            {
                                let new = delta.new_file().path().map(|p| p.to_path_buf());
                                let old = delta.old_file().path().unwrap().to_path_buf();
                                renames.insert(old, new);
                            }
                            true
                        },
                        None,
                        Some(&mut |delta, hunk| {
                            log::debug!("processing hunk {:?}", hunk);
                            if !delta.new_file().exists()
                                || !matches!(
                                    delta.status(),
                                    git2::Delta::Added | git2::Delta::Modified
                                )
                            {
                                return true;
                            }
                            let new = delta.new_file().path().unwrap().to_path_buf();
                            // TODO is this a sensible way to calculate it? better to count lines added, removed, and changed properly?
                            let lines_changed = hunk.old_lines().max(hunk.new_lines());
                            contributions
                                .entry(new)
                                .or_default()
                                .add_lines(author.to_string(), lines_changed as usize);
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
                //|| Ok((HashMap::new(), HashMap::new())),
                |older, newer| {
                    let (older_contributions, mut older_renames) = older?;
                    let (newer_contributions, newer_renames) = newer?;
                    let mut mapped = HashMap::new();
                    // TODO is the rename handling done correctly?
                    // TODO if a file is added in `newer` what should happen to stuff from `older`? I guess it should be discarded?
                    // update older contributions using the newer renames
                    for (old_path, contributions) in older_contributions {
                        match newer_renames.get(&old_path) {
                            Some(Some(new_path)) => {
                                mapped.insert(new_path.clone(), contributions);
                            }
                            Some(None) => {
                                // the file was removed. don't add it to the map
                            }
                            _ => {
                                // not renamed, so just add it to the map
                                mapped.insert(old_path, contributions);
                            }
                        }
                    }
                    // merge the contributions
                    for (path, contributions) in newer_contributions {
                        mapped.entry(path).or_default().merge(contributions);
                    }
                    // merge the rename mappings
                    'outer: for (new_from_path, new_to_path) in newer_renames {
                        // TODO do this with a better data structure
                        for older_to_path in older_renames.values_mut() {
                            match older_to_path {
                                Some(path) if path == &new_from_path => {
                                    *older_to_path = new_to_path;
                                    // TODO or can there be multiple renames to the same path?
                                    break 'outer;
                                }
                                _ => {}
                            }
                        }
                        older_renames.insert(new_from_path, new_to_path);
                    }
                    log::debug!("older_renames: {:?}", older_renames);
                    Ok((mapped, older_renames))
                },
            )
            .unwrap()?
            //?
            .0
            .into_iter()
            .filter(|(p, _)| paths.contains(p))
            .collect();
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
