use anyhow::{Context, Result};
use chrono::Utc;
use colored::*;
use meta_core::display::escape;
use std::path::Path;

use meta_git_lib::worktree::helpers::find_meta_dir;
use meta_git_lib::worktree::hooks::fire_post_prune;
use meta_git_lib::worktree::safety::{check_worktree_removal, delete_checked_set};
use meta_git_lib::worktree::store::{entry_ttl_remaining, store_list, store_remove_batch};
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

/// Check if a worktree entry is orphaned due to missing source repos.
/// Uses a config cache to avoid redundant file I/O when multiple worktrees
/// share the same source project.
fn check_repo_orphaned(
    entry: &WorktreeStoreEntry,
    config_cache: &mut std::collections::HashMap<
        String,
        Option<Vec<meta_core::config::ProjectInfo>>,
    >,
) -> Option<String> {
    let config = config_cache
        .entry(entry.project.clone())
        .or_insert_with(|| {
            let project_path = Path::new(&entry.project);
            meta_core::config::find_meta_config_in(project_path)
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
    let missing_count = entry
        .repos
        .iter()
        .map(|r| r.alias.as_str())
        .filter(|alias| {
            *alias != "."
                && !projects.iter().any(|p| p.name == *alias)
                && !(alias.contains('/') && nested_alias_present(Path::new(&entry.project), alias))
        })
        .count();

    if missing_count > 0 && missing_count == entry.repos.len() {
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

pub(crate) fn handle_prune(
    args: PruneArgs,
    _verbose: bool,
    json: bool,
    strict: bool,
) -> Result<()> {
    let dry_run = args.dry_run;

    let store: WorktreeStoreData = store_list()?;
    if store.worktrees.is_empty() {
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
            println!("No worktrees in store. Nothing to prune.");
        }
        return Ok(());
    }

    let now = Utc::now().timestamp();
    let mut to_remove: Vec<PruneEntry> = Vec::new();
    let mut config_cache: std::collections::HashMap<
        String,
        Option<Vec<meta_core::config::ProjectInfo>>,
    > = std::collections::HashMap::new();

    for (path_key, entry) in &store.worktrees {
        let wt_path = Path::new(path_key);

        // Check if path exists (orphaned detection)
        if is_missing(wt_path) {
            to_remove.push(create_prune_entry(
                entry.name.clone(),
                path_key.clone(),
                "orphaned (missing directory)",
                None,
            ));
            continue;
        }

        // Check if source project directory still exists
        let project_path = Path::new(&entry.project);
        if !project_path.exists() {
            to_remove.push(create_prune_entry(
                entry.name.clone(),
                path_key.clone(),
                "orphaned (source project missing)",
                None,
            ));
            continue;
        }

        // Check if source repos still exist in project (with config caching)
        if let Some(reason) = check_repo_orphaned(entry, &mut config_cache) {
            to_remove.push(create_prune_entry(
                entry.name.clone(),
                path_key.clone(),
                reason,
                None,
            ));
            continue;
        }

        // Check TTL expiration
        if let Some(remaining) = entry_ttl_remaining(entry, now) {
            if remaining <= 0 {
                // Total age = configured TTL + seconds past expiry
                let overdue = remaining.unsigned_abs();
                let age = entry.ttl_seconds.unwrap_or(0).saturating_add(overdue);
                to_remove.push(create_prune_entry(
                    entry.name.clone(),
                    path_key.clone(),
                    "ttl_expired",
                    Some(age),
                ));
            }
        }
    }

    // Prune never discards work: a candidate whose directory holds anything
    // that deleting it would lose is kept, with the reason.
    let mut kept = Vec::new();
    let mut candidates = Vec::new();
    for entry in to_remove {
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
        // Deleting a set would delete any recorded set inside it.
        let nested: Vec<&str> = store
            .worktrees
            .keys()
            .map(String::as_str)
            .filter(|k| Path::new(k) != wt_path && Path::new(k).starts_with(wt_path))
            .collect();
        let problems = match check_worktree_removal(wt_path) {
            _ if !nested.is_empty() => {
                format!("it holds other worktree sets: {}", nested.join(", "))
            }
            Ok(check) if check.is_clear() => {
                candidates.push((entry, Some(check)));
                continue;
            }
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

    // Actually remove: physical cleanup first, then batch store update.
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
            // A missing directory only loses its record. One that has
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
            None => Ok(()),
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

    // Batch-remove the pruned records in a single lock cycle, each only if it
    // is still the record this run read.
    let records: Vec<(String, String)> = removed
        .iter()
        .filter_map(|e| {
            let entry = store.worktrees.get(&e.path)?;
            Some((e.path.clone(), entry.created_at.clone()))
        })
        .collect();
    // A failure is reported after the hook and output: the sets are gone
    let stored = store_remove_batch(&records)
        .context("Pruned worktrees, but could not update the worktree store");

    // Fire post-prune hook
    let meta_dir = find_meta_dir();
    fire_post_prune(&removed, meta_dir.as_deref());

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

    // Every failure is reported, the store's and the sets' alike
    let mut problems = Vec::new();
    if let Err(e) = stored {
        problems.push(format!("{e:#}"));
    }
    if !failed.is_empty() {
        problems.push(format!("Failed to prune: {}", escape(&failed.join(", "))));
    }
    if !problems.is_empty() {
        anyhow::bail!("{}", problems.join("\n"));
    }
    warn_kept(&kept, strict)
}

/// Why the recorded path no longer resolves to itself, if it doesn't: a
/// directory above it became a symlink, which could lead into another set,
/// or it can't be resolved at all.
/// The store records each set's resolved path when it is created. A link at
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

    fn make_store_entry(name: &str, project: &str, repos: Vec<(&str, &str)>) -> WorktreeStoreEntry {
        WorktreeStoreEntry {
            name: name.to_string(),
            project: project.to_string(),
            created_at: "2025-01-01T00:00:00Z".to_string(),
            ephemeral: false,
            ttl_seconds: None,
            repos: repos
                .into_iter()
                .map(|(alias, branch)| StoreRepoEntry {
                    alias: alias.to_string(),
                    branch: branch.to_string(),
                    created_branch: false,
                })
                .collect(),
            custom: HashMap::new(),
        }
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

        let entry = make_store_entry(
            "test-wt",
            project_path.to_str().unwrap(),
            vec![("repo1", "main"), ("repo2", "feat-x")],
        );

        let mut cache = HashMap::new();
        let result = check_repo_orphaned(&entry, &mut cache);

        assert!(result.is_none());
    }

    #[test]
    fn check_repo_orphaned_detects_when_all_repos_missing() {
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path();

        // Create .meta config with only repo3 (repos 1 and 2 are missing)
        let meta_config = r#"{"projects":{"repo3":"git@github.com:org/repo3.git"}}"#;
        std::fs::write(project_path.join(".meta"), meta_config).unwrap();

        let entry = make_store_entry(
            "test-wt",
            project_path.to_str().unwrap(),
            vec![("repo1", "main"), ("repo2", "feat-x")],
        );

        let mut cache = HashMap::new();
        let result = check_repo_orphaned(&entry, &mut cache);

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

        let entry = make_store_entry(
            "test-wt",
            project_path.to_str().unwrap(),
            vec![("repo1", "main"), ("repo2", "feat-x")],
        );

        let mut cache = HashMap::new();
        let result = check_repo_orphaned(&entry, &mut cache);

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

        let root_only = make_store_entry("root-only", project, vec![(".", "main")]);
        assert!(check_repo_orphaned(&root_only, &mut cache).is_none());

        let root_and_removed =
            make_store_entry("removed", project, vec![(".", "main"), ("repo1", "main")]);
        assert!(check_repo_orphaned(&root_and_removed, &mut cache).is_none());
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

        let entry = make_store_entry(
            "nested",
            project_path.to_str().unwrap(),
            vec![("sub/leaf", "main")],
        );
        assert!(check_repo_orphaned(&entry, &mut HashMap::new()).is_none());
    }

    #[test]
    fn check_repo_orphaned_returns_none_when_no_config() {
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path();

        // No .meta config file
        let entry = make_store_entry(
            "test-wt",
            project_path.to_str().unwrap(),
            vec![("repo1", "main")],
        );

        let mut cache = HashMap::new();
        let result = check_repo_orphaned(&entry, &mut cache);

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

        let entry1 = make_store_entry(
            "test-wt-1",
            project_path.to_str().unwrap(),
            vec![("repo1", "main")],
        );

        let entry2 = make_store_entry(
            "test-wt-2",
            project_path.to_str().unwrap(),
            vec![("repo1", "feat-x")],
        );

        let mut cache = HashMap::new();

        // First call should populate cache
        let result1 = check_repo_orphaned(&entry1, &mut cache);
        assert!(result1.is_none());
        assert_eq!(cache.len(), 1);

        // Second call with same project should use cache
        let result2 = check_repo_orphaned(&entry2, &mut cache);
        assert!(result2.is_none());
        assert_eq!(cache.len(), 1); // Still 1 entry (cached)

        // Verify cache contains the correct project
        assert!(cache.contains_key(project_path.to_str().unwrap()));
    }

    #[test]
    fn check_repo_orphaned_handles_invalid_json() {
        let temp_dir = TempDir::new().unwrap();
        let project_path = temp_dir.path();

        // Create invalid .meta config
        std::fs::write(project_path.join(".meta"), "invalid json").unwrap();

        let entry = make_store_entry(
            "test-wt",
            project_path.to_str().unwrap(),
            vec![("repo1", "main")],
        );

        let mut cache = HashMap::new();
        let result = check_repo_orphaned(&entry, &mut cache);

        // Should return None (can't parse config)
        assert!(result.is_none());
    }
}
