use anyhow::{Context, Result};
use colored::*;
use meta_core::display::{escape, escape_path};

use meta_cli::worktree::discover_worktree_repos;
use meta_git_lib::worktree::git_ops::git_worktree_add;
use meta_git_lib::worktree::helpers::*;
use meta_git_lib::worktree::store::store_extend_repos;
use meta_git_lib::worktree::types::*;

use super::cli_types::AddArgs;

pub(crate) fn handle_add(args: AddArgs, verbose: bool, json: bool) -> Result<()> {
    let name = &args.name;
    validate_worktree_name(name)?;

    let repo_specs = &args.repos;

    // Check for "." alias
    if repo_specs.iter().any(|r| r.alias == ".") {
        anyhow::bail!(
            "Cannot add '.' to an existing worktree. The meta repo root can only be established at create time.\n\
             Use 'meta worktree remove {name}' then 'meta worktree create {name} --repo . ...' instead."
        );
    }

    let meta_dir = require_meta_dir()?;
    let worktree_root = resolve_worktree_root(Some(&meta_dir))?;
    let wt_dir = worktree_set_dir(&worktree_root, name)?;

    if !wt_dir.exists() {
        anyhow::bail!("Worktree '{}' not found at {}", name, escape_path(&wt_dir));
    }

    // Check existing repos in the worktree
    let existing = discover_worktree_repos(&wt_dir)?;

    // A repo given twice can't be added twice: refuse before adding anything,
    // or the first add would be left out of the store
    for (i, spec) in repo_specs.iter().enumerate() {
        if repo_specs.iter().take(i).any(|o| o.alias == spec.alias) {
            anyhow::bail!("Repo '{}' is given twice", escape(&spec.alias));
        }
        if existing.iter().any(|r| r.alias == spec.alias) {
            anyhow::bail!(
                "Repo '{}' already exists in worktree '{name}'",
                escape(&spec.alias)
            );
        }
    }

    let mut added = Vec::new();
    for spec in repo_specs {
        // Use recursive lookup for nested paths (containing '/')
        let (source, _project) = lookup_nested_project(&meta_dir, &spec.alias)?;

        let branch = resolve_branch(name, None, spec.branch.as_deref());
        // Preserve the full alias as the destination path so that relative
        // references (e.g., Cargo.toml workspace members) remain valid.
        let dest = wt_dir.join(&spec.alias);
        // A checkout already in the worktree (such as the root repo's) may
        // hold a symlink where this one goes; git must not follow it out
        meta_core::config::resolve_inside(&wt_dir, &spec.alias).map_err(|why| {
            anyhow::anyhow!(
                "Refusing to add the worktree for '{}': {why}",
                escape(&spec.alias)
            )
        })?;

        // Ensure parent directories exist for nested paths (e.g., "vendor/")
        if let Some(parent) = dest.parent() {
            if parent != wt_dir {
                std::fs::create_dir_all(parent)?;
            }
        }

        if verbose {
            eprintln!(
                "Adding worktree for '{}' at {} (branch: {})",
                escape(&spec.alias),
                escape_path(&dest),
                escape(&branch)
            );
        }

        let created_branch = git_worktree_add(&source, &dest, &branch, None)?.is_some();
        added.push(CreateRepoEntry {
            alias: spec.alias.clone(),
            path: dest.display().to_string(),
            branch,
            created_branch,
        });
    }

    // Update centralized store
    let new_repos: Vec<StoreRepoEntry> = added.iter().map(StoreRepoEntry::from).collect();
    store_extend_repos(&wt_dir, new_repos).with_context(|| {
        format!("Added repos to worktree '{name}', but could not update the worktree store")
    })?;

    if json {
        let output = AddOutput {
            name: name.to_string(),
            repos: added,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        for r in &added {
            let branch_note = if r.created_branch { " (new)" } else { "" };
            println!(
                "{} Added '{}' to worktree '{}' (branch: {}{})",
                "✓".green(),
                escape(&r.alias),
                name,
                escape(&r.branch),
                branch_note
            );
        }
    }

    Ok(())
}
