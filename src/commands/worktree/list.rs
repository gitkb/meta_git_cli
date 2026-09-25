use anyhow::Result;
use chrono::Utc;
use colored::*;
use rayon::prelude::*;

use meta_cli::worktree::discover_worktree_repos;
use meta_git_lib::worktree::git_ops::git_status_summary;
use meta_git_lib::worktree::helpers::*;
use meta_git_lib::worktree::store::{entry_ttl_remaining, store_list};
use meta_git_lib::worktree::types::*;

use super::cli_types::ListArgs;

/// Render worktree sets, failing recursive inventory atomically on observation errors.
pub(crate) fn handle_list(args: ListArgs, _verbose: bool, json: bool) -> Result<()> {
    let meta_dir = find_meta_dir();
    let worktree_root = resolve_worktree_root(meta_dir.as_deref())?;

    if !worktree_root.exists() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&ListOutput { worktrees: vec![] })?
            );
        } else {
            println!("No worktrees found.");
        }
        return Ok(());
    }

    // Load store data for metadata enrichment
    let store_data = store_list().unwrap_or_default();
    let now = Utc::now().timestamp();

    // Collect directory entries, then process in parallel
    let mut dir_entries = Vec::new();
    for entry in std::fs::read_dir(&worktree_root)? {
        if args.recursive {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                dir_entries.push(entry);
            }
        } else if let Ok(entry) = entry {
            if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                dir_entries.push(entry);
            }
        }
    }

    let mut entries: Vec<ListEntry> = dir_entries
        .par_iter()
        .map(|entry| -> Result<Option<ListEntry>> {
            let name = entry.file_name().to_string_lossy().to_string();
            let wt_dir = entry.path();

            let repos = if args.recursive {
                meta_cli::worktree::discover_worktree_repos_recursive(&wt_dir)?
            } else {
                discover_worktree_repos(&wt_dir).unwrap_or_default()
            };
            if repos.is_empty() {
                return Ok(None); // Not a valid worktree set
            }

            let has_meta_root = repos.iter().any(|r| r.alias == ".");
            let repo_entries: Vec<ListRepoEntry> = repos
                .par_iter()
                .map(|r| {
                    let status = git_status_summary(&r.path);
                    let dirty = if args.recursive {
                        status?.dirty
                    } else {
                        status.map(|s| s.dirty).unwrap_or(false)
                    };
                    Ok(ListRepoEntry {
                        alias: r.alias.clone(),
                        branch: r.branch.clone(),
                        dirty,
                    })
                })
                .collect::<Result<Vec<_>>>()?;

            // Merge store metadata if available
            let wt_key = wt_dir.to_string_lossy().to_string();
            let (ephemeral, ttl_remaining, custom) =
                if let Some(store_entry) = store_data.worktrees.get(&wt_key) {
                    (
                        Some(store_entry.ephemeral),
                        entry_ttl_remaining(store_entry, now),
                        (!store_entry.custom.is_empty()).then(|| store_entry.custom.clone()),
                    )
                } else {
                    (None, None, None)
                };

            Ok(Some(ListEntry {
                name,
                root: wt_dir.display().to_string(),
                has_meta_root,
                repos: repo_entries,
                ephemeral,
                ttl_remaining_seconds: ttl_remaining,
                custom,
            }))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();

    // Sort by name for deterministic output
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&ListOutput { worktrees: entries })?
        );
    } else if entries.is_empty() {
        println!("No worktrees found.");
    } else {
        for e in &entries {
            let mut header = e.name.bold().to_string();
            if e.ephemeral == Some(true) {
                header.push_str(&format!(" {}", "[ephemeral]".dimmed()));
            }
            if let Some(ttl) = e.ttl_remaining_seconds {
                if ttl > 0 {
                    header.push_str(&format!(
                        " {}",
                        format!("[TTL: {}]", format_duration(ttl)).dimmed()
                    ));
                } else {
                    header.push_str(&format!(" {}", "[expired]".red()));
                }
            }
            println!("{header}");
            for r in &e.repos {
                let status = if r.dirty {
                    "modified".yellow().to_string()
                } else {
                    "clean".green().to_string()
                };
                println!("  {:12} -> {:20} ({})", r.alias, r.branch, status);
            }
            println!();
        }
    }

    Ok(())
}
