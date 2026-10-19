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

#[derive(serde::Serialize)]
struct ListWarning {
    name: String,
    root: String,
    message: String,
}

#[derive(serde::Serialize)]
struct ListWithWarnings {
    #[serde(flatten)]
    listing: ListOutput,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<ListWarning>,
}

/// Render healthy worktree sets and report unavailable sets without aborting the list.
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
            dir_entries.push(entry?);
        } else if let Ok(entry) = entry {
            if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                dir_entries.push(entry);
            }
        }
    }

    let results: Vec<Result<Option<ListEntry>>> = dir_entries
        .par_iter()
        .map(|entry| -> Result<Option<ListEntry>> {
            if args.recursive && !entry.file_type()?.is_dir() {
                return Ok(None);
            }
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
        .collect();

    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    for (entry, result) in dir_entries.iter().zip(results) {
        match result {
            Ok(Some(entry)) => entries.push(entry),
            Ok(None) => {}
            Err(error) if args.recursive => warnings.push(ListWarning {
                name: entry.file_name().to_string_lossy().into_owned(),
                root: entry.path().display().to_string(),
                message: format!("{error:#}"),
            }),
            Err(error) => return Err(error),
        }
    }

    // Sort both collections by name for deterministic output.
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    warnings.sort_by(|a, b| a.name.cmp(&b.name));

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&ListWithWarnings {
                listing: ListOutput { worktrees: entries },
                warnings,
            })?
        );
        return Ok(());
    }

    if entries.is_empty() {
        if warnings.is_empty() {
            println!("No worktrees found.");
        } else {
            println!("No readable worktree sets found.");
        }
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
    for warning in &warnings {
        eprintln!("Warning: {}: {}", warning.name, warning.message);
    }

    Ok(())
}
