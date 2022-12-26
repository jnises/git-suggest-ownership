use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Result};
use chrono::{DateTime, FixedOffset, NaiveDateTime, Utc};
use git2::{BlameOptions, Commit, DiffOptions, Repository};
use log::warn;

#[derive(Default)]
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
            let commit_time = time_to_utc_datetime(when)?;
            let age = Utc::now() - commit_time;
            if let Some(max_age) = max_age {
                if age > *max_age {
                    continue;
                }
            }
            if let Some(email) = signature.email() {
                s.total_lines += lines;
                *s.authors.entry(email.to_owned()).or_default() += lines;
            } else {
                // TODO keep track of unauthored hunks somehow?
                warn!("hunk without email found in {}", path.display());
            }
        }
        Ok(s)
    }

    pub(crate) fn calculate_overwritten_from_paths(
        repo: &Repository,
        paths: &HashSet<PathBuf>,
        max_age: &Option<chrono::Duration>,
    ) -> Result<HashMap<PathBuf, Self>> {
        fn calculate(
            repo: &Repository,
            paths: &HashSet<PathBuf>,
            max_age: &Option<chrono::Duration>,
            root: &Commit,
            contributions: &mut HashMap<PathBuf, Contributions>,
            mut renames: HashMap<PathBuf, PathBuf>,
        ) -> Result<()> {
            let root_time = time_to_utc_datetime(root.time())?;
            let age = Utc::now() - root_time;
            if let Some(max_age) = max_age {
                if age > *max_age {
                    return Ok(());
                }
            }
            for parent in root.parents() {
                // TODO mailmap
                if let Some(author) = root.author().email() {
                    let mut diff = repo.diff_tree_to_tree(
                        Some(&parent.tree()?),
                        Some(&root.tree()?),
                        // TODO use some other diff options? patience?
                        Some(DiffOptions::new().context_lines(0)),
                    )?;
                    // TODO different options here?
                    diff.find_similar(None)?;
                    diff.foreach(
                        &mut |_, _| true,
                        None,
                        Some(&mut |delta, hunk| {
                            if !delta.new_file().exists() {
                                return true;
                            }
                            let mut mapped_new = delta.new_file().path().unwrap().to_path_buf();
                            if let Some(m) = renames.get(&mapped_new) {
                                mapped_new = m.clone();
                            }
                            if delta.old_file().exists()
                                && delta.old_file().path() != delta.new_file().path()
                            {
                                renames.insert(
                                    delta.old_file().path().unwrap().to_path_buf(),
                                    mapped_new.clone(),
                                );
                            }
                            // TODO make sure these paths match
                            if paths.contains(&mapped_new) {
                                // TODO is this a sensible way to calculate it?
                                let lines_changed = hunk.old_lines().max(hunk.new_lines());
                                *contributions
                                    .entry(mapped_new)
                                    .or_default()
                                    .authors
                                    .entry(author.to_string())
                                    .or_default() += lines_changed as usize;
                            }
                            true
                        }),
                        None,
                    )?;
                } else {
                    log::warn!("Commit {} has no valid author email", root.id());
                }
                calculate(
                    repo,
                    paths,
                    max_age,
                    &parent,
                    contributions,
                    renames.clone(),
                )?;
            }
            Ok(())
        }
        let head = repo.head()?.peel_to_commit()?;
        let mut contributions = HashMap::new();
        calculate(
            repo,
            paths,
            max_age,
            &head,
            &mut contributions,
            HashMap::new(),
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

fn time_to_utc_datetime(time: git2::Time) -> Result<DateTime<Utc>> {
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
