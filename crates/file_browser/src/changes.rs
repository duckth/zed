use std::sync::Arc;

use anyhow::{Result, bail};
use collections::{HashMap, HashSet};
use project::ProjectEntryId;
use similar::{Algorithm, DiffOp, capture_diff_slices};
use util::rel_path::RelPath;

use crate::BrowserEntry;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Change {
    CreateDirectory {
        path: Arc<RelPath>,
    },
    CreateFile {
        path: Arc<RelPath>,
    },
    Rename {
        entry_id: ProjectEntryId,
        from: Arc<RelPath>,
        to: Arc<RelPath>,
    },
    Trash {
        entry_id: ProjectEntryId,
        path: Arc<RelPath>,
        is_dir: bool,
    },
}

impl Change {
    pub(crate) fn is_trash(&self) -> bool {
        matches!(self, Change::Trash { .. })
    }

    pub(crate) fn description(&self) -> String {
        match self {
            Change::CreateDirectory { path } => format!("Create {}/", path.as_unix_str()),
            Change::CreateFile { path } => format!("Create {}", path.as_unix_str()),
            Change::Rename { from, to, .. } => {
                format!("Rename {} → {}", from.as_unix_str(), to.as_unix_str())
            }
            Change::Trash { path, is_dir, .. } => {
                let separator = if *is_dir { "/" } else { "" };
                format!("Trash {}{separator}", path.as_unix_str())
            }
        }
    }
}

/// Works out the file system changes that turn the listing of `entries` in `directory` into
/// `text`, in the order they must be applied.
///
/// Lines are matched up by name: a line edited in place is a rename, a removed line is trashed and
/// a new line is created. Lines that only moved within the listing are left alone.
pub(crate) fn plan_changes(
    directory: &RelPath,
    entries: &[BrowserEntry],
    text: &str,
    path_exists: impl Fn(&RelPath) -> bool,
) -> Result<Vec<Change>> {
    let old_lines = entries.iter().map(BrowserEntry::line).collect::<Vec<_>>();
    let old_lines = old_lines.iter().map(String::as_str).collect::<Vec<_>>();
    let new_lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();

    let mut hunks = Vec::new();
    for operation in capture_diff_slices(Algorithm::Myers, &old_lines, &new_lines) {
        let (old_range, new_range) = match operation {
            DiffOp::Equal { .. } => continue,
            DiffOp::Delete {
                old_index, old_len, ..
            } => (old_index..old_index + old_len, 0..0),
            DiffOp::Insert {
                new_index, new_len, ..
            } => (0..0, new_index..new_index + new_len),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => (
                old_index..old_index + old_len,
                new_index..new_index + new_len,
            ),
        };
        hunks.push((old_range.collect::<Vec<_>>(), new_lines[new_range].to_vec()));
    }

    // A line that was cut and pasted elsewhere in the listing shows up as both removed and
    // added, but the entry itself is unchanged.
    let mut added_counts = HashMap::<&str, usize>::default();
    for (_, added) in &hunks {
        for line in added {
            *added_counts.entry(line).or_default() += 1;
        }
    }
    let mut moved_counts = HashMap::<&str, usize>::default();
    for (removed, _) in &mut hunks {
        removed.retain(|&index| {
            let line = old_lines[index];
            match added_counts.get_mut(line) {
                Some(count) if *count > 0 => {
                    *count -= 1;
                    *moved_counts.entry(line).or_default() += 1;
                    false
                }
                _ => true,
            }
        });
    }
    for (_, added) in &mut hunks {
        added.retain(|line| match moved_counts.get_mut(line) {
            Some(count) if *count > 0 => {
                *count -= 1;
                false
            }
            _ => true,
        });
    }

    let mut trashes = Vec::new();
    let mut renames = Vec::new();
    let mut creates = Vec::new();
    for (removed, added) in hunks {
        if removed.len() == added.len() {
            for (index, line) in removed.into_iter().zip(added) {
                let entry = &entries[index];
                let (to, _) = resolve_path(directory, line)?;
                if to != entry.path {
                    renames.push((entry, to));
                }
            }
        } else {
            trashes.extend(removed.into_iter().map(|index| &entries[index]));
            for line in added {
                creates.push(resolve_path(directory, line)?);
            }
        }
    }

    // Renames aren't counted as freeing their old path, as that would make the order of renames
    // matter.
    let trashed_paths = trashes
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<HashSet<_>>();
    let mut targets = HashSet::default();
    for target in renames
        .iter()
        .map(|(_, to)| to)
        .chain(creates.iter().map(|(path, _)| path))
    {
        if !targets.insert(target.clone()) {
            bail!("{} is listed more than once.", target.as_unix_str());
        }
        if path_exists(target) && !trashed_paths.contains(target) {
            bail!("{} already exists.", target.as_unix_str());
        }
    }

    let mut changes = Vec::new();
    let mut created_directories = HashSet::default();
    for target in &targets {
        let Some(parent) = target.parent() else {
            continue;
        };
        for ancestor in parent
            .ancestors()
            .take_while(|ancestor| !ancestor.is_empty() && !path_exists(ancestor))
        {
            created_directories.insert(ancestor.into_arc());
        }
    }
    // Sorting by path puts each directory before its subdirectories.
    let mut created_directories = created_directories.into_iter().collect::<Vec<_>>();
    created_directories.sort();
    changes.extend(
        created_directories
            .iter()
            .map(|path| Change::CreateDirectory { path: path.clone() }),
    );

    changes.extend(trashes.into_iter().map(|entry| Change::Trash {
        entry_id: entry.id,
        path: entry.path.clone(),
        is_dir: entry.is_dir,
    }));

    changes.extend(renames.into_iter().map(|(entry, to)| Change::Rename {
        entry_id: entry.id,
        from: entry.path.clone(),
        to,
    }));

    for (path, is_dir) in creates {
        if is_dir {
            if !created_directories.contains(&path) {
                changes.push(Change::CreateDirectory { path });
            }
        } else {
            changes.push(Change::CreateFile { path });
        }
    }
    Ok(changes)
}

/// Resolves a line of the listing to a path in the worktree, relative to the listed `directory`.
/// A trailing `/` marks a directory.
fn resolve_path(directory: &RelPath, line: &str) -> Result<(Arc<RelPath>, bool)> {
    if line.starts_with('/') {
        bail!("{line}: absolute paths aren't supported.");
    }
    let is_dir = line.ends_with('/');
    let mut path = directory.to_rel_path_buf();
    for component in line.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if !path.pop() {
                    bail!("{line}: paths can't go above the project root.");
                }
            }
            component => path.push_component(component)?,
        }
    }
    if path.as_rel_path() == directory || path.as_rel_path().is_empty() {
        bail!("{line}: not a valid name.");
    }
    Ok((path.as_rel_path().into_arc(), is_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use util::rel_path::rel_path;

    fn entry(id: usize, path: &str, is_dir: bool) -> BrowserEntry {
        let path: Arc<RelPath> = rel_path(path).into();
        BrowserEntry {
            id: ProjectEntryId::from_usize(id),
            name: path.file_name().unwrap_or_default().to_string(),
            path,
            is_dir,
            is_ignored: false,
        }
    }

    /// `src/` containing `a.rs`, `b.rs` and `old/`, in a project that also has `docs/`.
    fn plan(text: &str) -> Result<Vec<String>> {
        let entries = [
            entry(1, "src/old", true),
            entry(2, "src/a.rs", false),
            entry(3, "src/b.rs", false),
        ];
        let existing = ["src", "src/old", "src/a.rs", "src/b.rs", "docs"];
        let changes = plan_changes(rel_path("src"), &entries, text, |path| {
            existing.contains(&path.as_unix_str())
        })?;
        Ok(changes.iter().map(Change::description).collect())
    }

    #[test]
    fn test_unchanged_listing() {
        assert_eq!(plan("old/\na.rs\nb.rs").unwrap(), Vec::<String>::new());
        // Blank lines, indentation and order don't matter.
        assert_eq!(
            plan("\n  b.rs\nold/\n\na.rs  \n").unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_rename_in_place() {
        assert_eq!(
            plan("old/\nc.rs\nb.rs").unwrap(),
            ["Rename src/a.rs → src/c.rs"]
        );
        assert_eq!(
            plan("new/\na.rs\nb.rs").unwrap(),
            ["Rename src/old → src/new"]
        );
    }

    #[test]
    fn test_rename_to_a_path_moves() {
        assert_eq!(
            plan("old/\n../docs/a.rs\nb.rs").unwrap(),
            ["Rename src/a.rs → docs/a.rs"]
        );
        assert_eq!(
            plan("old/\nold/a.rs\nb.rs").unwrap(),
            ["Rename src/a.rs → src/old/a.rs"]
        );
    }

    #[test]
    fn test_removed_lines_are_trashed() {
        assert_eq!(plan("a.rs").unwrap(), ["Trash src/old/", "Trash src/b.rs"]);
    }

    #[test]
    fn test_new_lines_are_created() {
        assert_eq!(
            plan("old/\na.rs\nb.rs\nc.rs\nnew/").unwrap(),
            ["Create src/c.rs", "Create src/new/"]
        );
    }

    #[test]
    fn test_missing_parent_directories_are_created_first() {
        assert_eq!(
            plan("old/\na.rs\nb.rs\nx/y/z.rs\nold/q/r.rs").unwrap(),
            [
                "Create src/old/q/",
                "Create src/x/",
                "Create src/x/y/",
                "Create src/x/y/z.rs",
                "Create src/old/q/r.rs",
            ]
        );
    }

    #[test]
    fn test_adjacent_renames() {
        assert_eq!(
            plan("old/\nc.rs\nd.rs").unwrap(),
            ["Rename src/a.rs → src/c.rs", "Rename src/b.rs → src/d.rs"]
        );
    }

    #[test]
    fn test_renames_onto_another_entry_are_ambiguous() {
        // Renaming a.rs to b.rs and b.rs to c.rs leaves the same text as removing a.rs and adding
        // c.rs, so that's how it's read. The confirmation lists it, so the user can cancel.
        assert_eq!(
            plan("old/\nb.rs\nc.rs").unwrap(),
            ["Trash src/a.rs", "Create src/c.rs"]
        );
    }

    #[test]
    fn test_invalid_edits() {
        let error = |text| plan(text).unwrap_err().to_string();
        assert_eq!(error("old/\nb.rs\nb.rs"), "src/b.rs already exists.");
        assert_eq!(
            error("old/\na.rs\nb.rs\nc.rs\nc.rs"),
            "src/c.rs is listed more than once."
        );
        assert_eq!(error("old/\na.rs\nb.rs\n../docs/"), "docs already exists.");
        assert_eq!(
            error("old/\na.rs\nb.rs\n/etc/hosts"),
            "/etc/hosts: absolute paths aren't supported."
        );
        assert_eq!(
            error("old/\na.rs\nb.rs\n../../x"),
            "../../x: paths can't go above the project root."
        );
    }
}
