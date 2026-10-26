use anyhow::Result;
use chrono::Utc;
use colored::*;
use meta_core::display::escape;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use meta_cli::worktree::discover_worktree_members;
use meta_git_lib::worktree::helpers::{require_meta_dir, resolve_worktree_root};
use meta_git_lib::worktree::hooks::fire_post_prune;
use meta_git_lib::worktree::record::{self, SetRecord};
use meta_git_lib::worktree::safety::{
    check_worktree_removal, delete_checked_set, linked_worktree_dirs, member_record,
};
use meta_git_lib::worktree::types::*;

use super::cli_types::PruneArgs;

/// Helper to create a PruneEntry with consistent structure.
fn create_prune_entry(
    name: String,
    path: String,
    reason: impl Into<String>,
    age_seconds: Option<u64>,
) -> PruneEntry {
    PruneEntry {
        name,
        path,
        reason: reason.into(),
        age_seconds,
    }
}

/// Check if a set is orphaned because `project` no longer has any of the
/// repos its worktrees (`aliases`, their paths in the set) belong to.
/// Uses a config cache to avoid redundant file I/O when multiple worktrees
/// share the same source project.
fn check_repo_orphaned(
    project: &Path,
    aliases: &[String],
    config_cache: &mut std::collections::HashMap<
        PathBuf,
        Option<Vec<meta_core::config::ProjectInfo>>,
    >,
) -> Option<String> {
    let config = config_cache
        .entry(project.to_path_buf())
        .or_insert_with(|| {
            meta_core::config::find_meta_config_in(project)
                .ok()
                .flatten()
                .and_then(|(meta_path, _)| {
                    meta_core::config::parse_meta_config(&meta_path)
                        .ok()
                        .map(|(projects, _)| projects)
                })
        });

    let Some(projects) = config else {
        return None; // Can't check without config
    };

    // "." is the project's own repo and always exists, so a set holding it is
    // never orphaned. Nested aliases ("sub/leaf") live in child .meta files,
    // not the top-level project list.
    let missing_count = aliases
        .iter()
        .map(String::as_str)
        .filter(|alias| {
            *alias != "."
                && !projects.iter().any(|p| p.name == *alias)
                && !(alias.contains('/') && nested_alias_present(project, alias))
        })
        .count();

    if missing_count > 0 && missing_count == aliases.len() {
        Some("orphaned (all source repos removed from project)".to_string())
    } else {
        None
    }
}

/// Whether nested alias `alias` still names a repo of `project`. A nested
/// config that can't be read or is refused says nothing about the repo, so
/// it counts as present: only a tree that reads cleanly without the alias
/// makes the repo removed.
fn nested_alias_present(project: &Path, alias: &str) -> bool {
    match meta_core::config::walk_meta_tree(project, None) {
        Ok(tree) => meta_core::config::build_project_map(&tree, project, "").contains_key(alias),
        Err(_) => true,
    }
}

/// Only a path that is really gone counts as missing. A dangling link or a
/// path prune can't inspect (permission denied) exists, so it gets checked
/// and kept, never forgotten.
fn is_missing(path: &Path) -> bool {
    matches!(path.symlink_metadata(), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// Every record naming each set path, with the admin directory it is in.
type Records = BTreeMap<PathBuf, Vec<(PathBuf, SetRecord)>>;

/// The sets meta created that prune can find from the current workspace:
/// the sets under the worktree root, and any other set with a record in a
/// repository of the workspace (one under `META_WORKTREES`, or one whose
/// directory is gone). There is no global list, so sets recorded only in
/// other workspaces' repositories are not seen. Records that can't be used
/// are returned as kept entries.
fn find_sets(meta_dir: &Path) -> Result<(Records, Vec<PruneEntry>)> {
    let mut found: Vec<(PathBuf, SetRecord)> = Vec::new();
    let mut unusable: Vec<PruneEntry> = Vec::new();
    let mut not_used = |name: String, path: &Path, e: anyhow::Error| {
        unusable.push(create_prune_entry(
            name,
            path.display().to_string(),
            format!("meta's records there can't be read: {e:#}"),
            None,
        ))
    };
    let worktree_root = resolve_worktree_root(Some(meta_dir))?;
    // A root that can't be read is reported; its sets are still found
    // through their repositories, and checked one by one
    let entries = match std::fs::read_dir(&worktree_root) {
        Ok(entries) => entries.collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => vec![Err(e)],
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                let name = worktree_root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned());
                not_used(name.unwrap_or_default(), &worktree_root, e.into());
                continue;
            }
        };
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        for member in discover_worktree_members(&entry.path()).unwrap_or_default() {
            match member_record(&member.path) {
                Ok(Some(found_here)) => found.push(found_here),
                Ok(None) => {}
                Err(e) => not_used(name.clone(), &member.path, e),
            }
        }
    }
    let root = meta_core::config::find_root_meta_dir(meta_dir);
    let mut repos = vec![root.clone()];
    match meta_core::config::walk_meta_tree(&root, None) {
        Ok(tree) => repos.extend(
            meta_core::config::build_project_map(&tree, &root, "")
                .into_values()
                .map(|(path, _)| path),
        ),
        // Only some repositories can be searched: that is reported
        Err(e) => {
            let name = root.file_name().map(|n| n.to_string_lossy().into_owned());
            not_used(name.unwrap_or_default(), &root, e)
        }
    }
    for repo in repos {
        let git_dir = match repo.join(".git").symlink_metadata() {
            Ok(meta) if meta.is_dir() => repo.join(".git"),
            Ok(meta) if meta.is_file() => match linked_worktree_dirs(&repo) {
                Ok((_, common)) => common,
                Err(_) => continue,
            },
            _ => continue,
        };
        for (admin, record) in record::in_repo(&git_dir) {
            match record {
                Ok(record) => found.push((admin, record)),
                Err(e) => {
                    let name = admin.file_name().map(|n| n.to_string_lossy().into_owned());
                    not_used(name.unwrap_or_default(), &admin, e)
                }
            }
        }
    }
    let mut sets = Records::new();
    let mut seen = BTreeSet::new();
    for (admin, record) in found {
        if seen.insert(admin.clone()) {
            sets.entry(record.set.clone())
                .or_default()
                .push((admin, record));
        }
    }
    let mut paths = BTreeSet::new();
    unusable.retain(|e| paths.insert(e.path.clone()));
    Ok((sets, unusable))
}

/// Why an existing set with `record`, whose worktrees are at `aliases` in
/// it, is due for pruning, if it is: the reason and, for an expired TTL,
/// its age.
fn eligible(
    record: &SetRecord,
    aliases: &[String],
    config_cache: &mut std::collections::HashMap<
        PathBuf,
        Option<Vec<meta_core::config::ProjectInfo>>,
    >,
    now: i64,
) -> Option<(String, Option<u64>)> {
    // Check if source project directory still exists
    if !record.project.exists() {
        return Some(("orphaned (source project missing)".to_string(), None));
    }
    // Check if source repos still exist in project (with config caching)
    if let Some(reason) = check_repo_orphaned(&record.project, aliases, config_cache) {
        return Some((reason, None));
    }
    // Check TTL expiration
    let remaining = record.ttl_remaining(now).filter(|r| *r <= 0)?;
    // Total age = configured TTL + seconds past expiry
    let age = record
        .ttl_seconds
        .unwrap_or(0)
        .saturating_add(remaining.unsigned_abs());
    Some(("ttl_expired".to_string(), Some(age)))
}

/// The paths of `members` from `set`, as repo aliases.
fn aliases<'a>(set: &Path, members: impl IntoIterator<Item = &'a Path>) -> Vec<String> {
    members
        .into_iter()
        .map(|path| match path.strip_prefix(set) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_) => path.to_string_lossy().into_owned(),
        })
        .collect()
}

pub(crate) fn handle_prune(
    args: PruneArgs,
    _verbose: bool,
    json: bool,
    strict: bool,
) -> Result<()> {
    let dry_run = args.dry_run;

    let meta_dir = require_meta_dir()?;
    let (sets, mut kept) = find_sets(&meta_dir)?;
    if sets.is_empty() && kept.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&PruneOutput {
                    removed: vec![],
                    kept: vec![],
                    dry_run,
                })?
            );
        } else {
            println!("No worktree sets that meta created were found. Nothing to prune.");
        }
        return Ok(());
    }

    let now = Utc::now().timestamp();
    let mut to_remove: Vec<(PruneEntry, SetRecord)> = Vec::new();
    let mut config_cache = std::collections::HashMap::new();

    for (set, records) in &sets {
        let path_key = set.display().to_string();
        let Some((_, found)) = records.first() else {
            continue;
        };
        let name = found.name();

        // Check if path exists (orphaned detection)
        if is_missing(set) {
            to_remove.push((
                create_prune_entry(name, path_key, "orphaned (missing directory)", None),
                found.clone(),
            ));
            continue;
        }

        // The record the set's own worktrees hold decides, not an older one
        // left in a repository; the removal check must see this one again
        let members = discover_worktree_members(set).unwrap_or_default();
        let record =
            record::find(set, members.iter().map(|m| &m.path)).unwrap_or_else(|| found.clone());

        let found_aliases = aliases(set, members.iter().map(|m| m.path.as_path()));
        if let Some((reason, age)) = eligible(&record, &found_aliases, &mut config_cache, now) {
            to_remove.push((create_prune_entry(name, path_key, reason, age), record));
        }
    }

    // Prune never discards work: a candidate whose directory holds anything
    // that deleting it would lose is kept, with the reason.
    let mut candidates = Vec::new();
    for (entry, record) in to_remove {
        let wt_path = Path::new(&entry.path);
        if is_missing(wt_path) {
            candidates.push((entry, None));
            continue;
        }
        if let Some(why) = path_moved(wt_path) {
            kept.push(create_prune_entry(
                entry.name,
                entry.path,
                format!("{}, but {why}", entry.reason),
                entry.age_seconds,
            ));
            continue;
        }
        // A set holding another set is blocked by the check.
        let problems = match check_worktree_removal(wt_path) {
            Ok(check) if check.is_clear() && check.record() == Some(&record) => {
                // Judged again with every repo the full scan found: only it
                // finds repos nested in other repos
                let all = aliases(wt_path, check.members.iter().map(|m| m.path.as_path()));
                if let Some((reason, age)) = eligible(&record, &all, &mut config_cache, now) {
                    let entry = create_prune_entry(entry.name, entry.path, reason, age);
                    candidates.push((entry, Some(check)));
                }
                continue;
            }
            Ok(check) if check.is_clear() && check.record().is_some() => {
                "its record changed after prune read it".to_string()
            }
            Ok(check) if check.is_clear() => "meta's record of it is gone".to_string(),
            Ok(check) => [check.blockers, check.unsaved].concat().join("; "),
            Err(e) => format!("cannot inspect it: {e:#}"),
        };
        kept.push(create_prune_entry(
            entry.name,
            entry.path,
            format!("{}, but {problems}", entry.reason),
            entry.age_seconds,
        ));
    }

    if dry_run || candidates.is_empty() {
        let would_remove: Vec<PruneEntry> = candidates.into_iter().map(|(e, _)| e).collect();
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&PruneOutput {
                    removed: would_remove,
                    kept: kept.clone(),
                    dry_run,
                })?
            );
        } else {
            if would_remove.is_empty() {
                println!("Nothing to prune.");
            } else {
                println!("Would prune {} worktree(s):", would_remove.len());
                for entry in &would_remove {
                    println!(
                        "  {} ({}) — {}",
                        escape(&entry.name),
                        escape(&entry.reason),
                        escape(&entry.path)
                    );
                }
            }
            print_kept(&kept);
        }
        if !dry_run {
            warn_kept(&kept, strict)?;
        }
        return Ok(());
    }

    // Actually remove: each checked set, or the records of a missing one.
    let mut removed = Vec::new();
    let mut failed = Vec::new();
    for (prune_entry, check) in candidates {
        let wt_path = Path::new(&prune_entry.path);
        let removal = match &check {
            // Checked again: a directory above may have become a link since
            Some(check) => match path_moved(wt_path) {
                None => delete_checked_set(wt_path, check, false, false),
                Some(why) => {
                    kept.push(create_prune_entry(
                        prune_entry.name,
                        prune_entry.path,
                        format!("{}, but {why}", prune_entry.reason),
                        prune_entry.age_seconds,
                    ));
                    continue;
                }
            },
            // A missing directory only loses its records. One that has
            // reappeared since was never checked: keep it, nothing failed.
            None if !is_missing(wt_path) => {
                kept.push(create_prune_entry(
                    prune_entry.name,
                    prune_entry.path,
                    format!("{}, but it reappeared", prune_entry.reason),
                    prune_entry.age_seconds,
                ));
                continue;
            }
            None => forget(Path::new(&prune_entry.path), &sets),
        };
        match removal {
            Ok(()) => removed.push(prune_entry),
            Err(e) => {
                failed.push(prune_entry.name.clone());
                kept.push(create_prune_entry(
                    prune_entry.name,
                    prune_entry.path,
                    format!("{}, but removing it failed: {e:#}", prune_entry.reason),
                    prune_entry.age_seconds,
                ));
            }
        }
    }

    // Fire post-prune hook
    fire_post_prune(&removed, Some(&meta_dir));

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&PruneOutput {
                removed,
                kept: kept.clone(),
                dry_run: false,
            })?
        );
    } else {
        println!("{} Pruned {} worktree(s):", "✓".green(), removed.len());
        for entry in &removed {
            println!(
                "  {} ({}) — {}",
                escape(&entry.name),
                escape(&entry.reason),
                escape(&entry.path)
            );
        }
        print_kept(&kept);
    }

    if !failed.is_empty() {
        anyhow::bail!("Failed to prune: {}", escape(&failed.join(", ")));
    }
    warn_kept(&kept, strict)
}

/// Delete the records of a set whose directory is gone, each only if it is
/// still the record prune read. Git's own entries for its worktrees stay,
/// for `git worktree prune`.
fn forget(set: &Path, sets: &Records) -> Result<()> {
    for (admin, record) in sets.get(set).into_iter().flatten() {
        if record::read(admin)?.as_ref() == Some(record) {
            record::delete(admin)?;
        }
    }
    Ok(())
}

/// Why the recorded path no longer resolves to itself, if it doesn't: a
/// directory above it became a symlink, which could lead into another set,
/// or it can't be resolved at all.
/// Each record holds its set's resolved path from when it was created. A link at
/// the set directory itself is left to the removal check, which refuses it.
fn path_moved(path: &Path) -> Option<String> {
    match path.symlink_metadata() {
        Ok(meta) if !meta.file_type().is_symlink() => match path.canonicalize() {
            Ok(resolved) if resolved == path => None,
            Ok(_) => Some("a directory above it now leads elsewhere through a symlink".to_string()),
            Err(e) => Some(format!("its path cannot be resolved ({e})")),
        },
        _ => None,
    }
}

fn print_kept(kept: &[PruneEntry]) {
    if kept.is_empty() {
        return;
    }
    println!("Kept {} worktree(s):", kept.len());
    for entry in kept {
        println!(
            "  {} ({}) — {}",
            escape(&entry.name),
            escape(&entry.reason),
            escape(&entry.path)
        );
    }
}

/// Kept worktrees are a warning, and an error in strict mode.
fn warn_kept(kept: &[PruneEntry], strict: bool) -> Result<()> {
    if kept.is_empty() {
        return Ok(());
    }
    let names: Vec<&str> = kept.iter().map(|e| e.name.as_str()).collect();
    super::warn_or_bail(strict, format!("Kept worktrees: {}", names.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    /// A set's project and aliases, as check_repo_orphaned takes them.
    fn make_set(project: &str, repos: Vec<(&str, &str)>) -> (PathBuf, Vec<String>) {
        let aliases = repos
            .into_iter()
            .map(|(alias, _)| alias.to_string())
            .collect();
        (PathBuf::from(project), aliases)
    }

    // ── create_prune_entry ──────────────────────────────

    #[test]
    fn create_prune_entry_with_age() {
        let entry = create_prune_entry(
            "test-wt".to_string(),
            "/path/to/wt".to_string(),
            "ttl_expired",
            Some(3600),
        );
        assert_eq!(entry.name, "test-wt");
        assert_eq!(entry.path, "/path/to/wt");
        assert_eq!(entry.reason, "ttl_expired");
        assert_eq!(entry.age_seconds, Some(3600));
    }

    #[test]
    fn create_prune_entry_without_age() {
        let entry = create_prune_entry(
            "test-wt".to_string(),
            "/path/to/wt".to_string(),
            "orphaned (missing directory)",
            None,
        );
        assert_eq!(entry.name, "test-wt");
        assert_eq!(entry.reason, "orphaned (missing directory)");
        assert_eq!(entry.age_seconds, None);
    }

    #[test]
    fn create_prune_entry_converts_into_string() {
        let entry = create_prune_entry(
            "test-wt".to_string(),
            "/path/to/wt".to_string(),
            String::from("test reason"),
            None,
        );
        assert_eq!(entry.reason, "test reason");
    }

    // ── check_repo_orphaned ─────────────────────────────

    #[test]
    fn check_repo_orphaned_returns_none_when_all_repos_exist() {
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path();

        // Create .meta config
        let meta_config = r#"{"projects":{"repo1":"git@github.com:org/repo1.git","repo2":"git@github.com:org/repo2.git"}}"#;
        std::fs::write(project_path.join(".meta"), meta_config).unwrap();

        let entry = make_set(
            project_path.to_str().unwrap(),
            vec![("repo1", "main"), ("repo2", "feat-x")],
        );

        let mut cache = HashMap::new();
        let result = check_repo_orphaned(&entry.0, &entry.1, &mut cache);

        assert!(result.is_none());
    }

    #[test]
    fn check_repo_orphaned_detects_when_all_repos_missing() {
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path();

        // Create .meta config with only repo3 (repos 1 and 2 are missing)
        let meta_config = r#"{"projects":{"repo3":"git@github.com:org/repo3.git"}}"#;
        std::fs::write(project_path.join(".meta"), meta_config).unwrap();

        let entry = make_set(
            project_path.to_str().unwrap(),
            vec![("repo1", "main"), ("repo2", "feat-x")],
        );

        let mut cache = HashMap::new();
        let result = check_repo_orphaned(&entry.0, &entry.1, &mut cache);

        assert_eq!(
            result,
            Some("orphaned (all source repos removed from project)".to_string())
        );
    }

    #[test]
    fn check_repo_orphaned_returns_none_when_some_repos_missing() {
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path();

        // Create .meta config with repo1 but not repo2
        let meta_config = r#"{"projects":{"repo1":"git@github.com:org/repo1.git"}}"#;
        std::fs::write(project_path.join(".meta"), meta_config).unwrap();

        let entry = make_set(
            project_path.to_str().unwrap(),
            vec![("repo1", "main"), ("repo2", "feat-x")],
        );

        let mut cache = HashMap::new();
        let result = check_repo_orphaned(&entry.0, &entry.1, &mut cache);

        // Only partial orphan - should return None (worktree is still partially valid)
        assert!(result.is_none());
    }

    #[test]
    fn check_repo_orphaned_never_counts_the_root_repo_as_missing() {
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path();
        let meta_config = r#"{"projects":{"repo3":"git@github.com:org/repo3.git"}}"#;
        std::fs::write(project_path.join(".meta"), meta_config).unwrap();
        let project = project_path.to_str().unwrap();
        let mut cache = HashMap::new();

        let root_only = make_set(project, vec![(".", "main")]);
        assert!(check_repo_orphaned(&root_only.0, &root_only.1, &mut cache).is_none());

        let root_and_removed = make_set(project, vec![(".", "main"), ("repo1", "main")]);
        assert!(
            check_repo_orphaned(&root_and_removed.0, &root_and_removed.1, &mut cache).is_none()
        );
    }

    #[test]
    fn check_repo_orphaned_finds_nested_aliases() {
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path();
        let meta_config =
            r#"{"projects":{"sub":{"repo":"git@github.com:org/sub.git","meta":true}}}"#;
        std::fs::write(project_path.join(".meta"), meta_config).unwrap();
        std::fs::create_dir(project_path.join("sub")).unwrap();
        let sub_config = r#"{"projects":{"leaf":"git@github.com:org/leaf.git"}}"#;
        std::fs::write(project_path.join("sub/.meta"), sub_config).unwrap();

        let entry = make_set(project_path.to_str().unwrap(), vec![("sub/leaf", "main")]);
        assert!(check_repo_orphaned(&entry.0, &entry.1, &mut HashMap::new()).is_none());
    }

    #[test]
    fn check_repo_orphaned_returns_none_when_no_config() {
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path();

        // No .meta config file
        let entry = make_set(project_path.to_str().unwrap(), vec![("repo1", "main")]);

        let mut cache = HashMap::new();
        let result = check_repo_orphaned(&entry.0, &entry.1, &mut cache);

        // Can't determine without config
        assert!(result.is_none());
    }

    #[test]
    fn check_repo_orphaned_uses_cache() {
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path();

        // Create .meta config
        let meta_config = r#"{"projects":{"repo1":"git@github.com:org/repo1.git"}}"#;
        std::fs::write(project_path.join(".meta"), meta_config).unwrap();

        let entry1 = make_set(project_path.to_str().unwrap(), vec![("repo1", "main")]);

        let entry2 = make_set(project_path.to_str().unwrap(), vec![("repo1", "feat-x")]);

        let mut cache = HashMap::new();

        // First call should populate cache
        let result1 = check_repo_orphaned(&entry1.0, &entry1.1, &mut cache);
        assert!(result1.is_none());
        assert_eq!(cache.len(), 1);

        // Second call with same project should use cache
        let result2 = check_repo_orphaned(&entry2.0, &entry2.1, &mut cache);
        assert!(result2.is_none());
        assert_eq!(cache.len(), 1); // Still 1 entry (cached)

        // Verify cache contains the correct project
        assert!(cache.contains_key(project_path));
    }

    #[test]
    fn check_repo_orphaned_handles_invalid_json() {
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path();

        // Create invalid .meta config
        std::fs::write(project_path.join(".meta"), "invalid json").unwrap();

        let entry = make_set(project_path.to_str().unwrap(), vec![("repo1", "main")]);

        let mut cache = HashMap::new();
        let result = check_repo_orphaned(&entry.0, &entry.1, &mut cache);

        // Should return None (can't parse config)
        assert!(result.is_none());
    }
}
