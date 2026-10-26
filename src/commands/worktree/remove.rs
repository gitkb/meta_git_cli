use anyhow::{Context, Result};
use colored::*;
use meta_core::display::{escape, escape_path};

use meta_git_lib::worktree::helpers::*;
use meta_git_lib::worktree::hooks::fire_post_destroy;
use meta_git_lib::worktree::safety::{check_worktree_removal, delete_checked_set};
use meta_git_lib::worktree::store::{store_get, store_remove_batch, store_sets_inside};
use meta_git_lib::worktree::types::*;

use super::cli_types::DestroyArgs;

pub(crate) fn handle_remove(args: DestroyArgs, verbose: bool, json: bool) -> Result<()> {
    let name = &args.name;
    validate_worktree_name(name)?;
    let force = args.force;

    let meta_dir = find_meta_dir();
    let worktree_root = resolve_worktree_root(meta_dir.as_deref())?;
    let wt_dir = worktree_set_dir(&worktree_root, name)?;

    if !wt_dir.exists() {
        anyhow::bail!("Worktree '{}' not found at {}", name, escape_path(&wt_dir));
    }

    // Only delete what meta created: the store records every worktree set it made.
    let Some((record_key, record)) = store_get(&wt_dir)? else {
        anyhow::bail!(
            "Refusing to remove {}: meta has no record of creating a worktree there.\n\
             If it is a worktree whose record was lost, delete it yourself and run \
             'git worktree prune' in its repositories.",
            escape_path(&wt_dir)
        );
    };

    let check = check_worktree_removal(&wt_dir)?;
    let nested = store_sets_inside(&wt_dir)?;
    let blockers: Vec<String> = check
        .blockers
        .iter()
        .cloned()
        .chain(
            nested
                .iter()
                .map(|k| format!("{}: another worktree set; remove it first", escape(k))),
        )
        .collect();
    if !blockers.is_empty() {
        anyhow::bail!(
            "Refusing to remove worktree '{name}':\n  {}\n\
             meta only deletes linked worktrees it can check; move or delete these yourself first.",
            blockers.join("\n  ")
        );
    }
    if !check.unsaved.is_empty() {
        let found = check.unsaved.join("\n  ");
        if !force {
            anyhow::bail!(
                "Worktree '{name}' has work that removing it would lose:\n  {found}\n\
                 Commit and push it, or use --force to discard it."
            );
        }
        eprintln!(
            "{} discarding work in worktree '{name}':\n  {found}",
            "warning:".yellow().bold()
        );
    }

    // Remove exactly what was checked, nested worktrees first
    delete_checked_set(&wt_dir, &check, force, verbose)?;

    // Remove from centralized store
    // By the key read before deleting: the deleted path can't be resolved again.
    let record = [(record_key, record.created_at)];
    // A failure is reported after the hook and output: the worktree is gone
    let stored = store_remove_batch(&record).with_context(|| {
        format!("Removed worktree '{name}', but could not update the worktree store")
    });

    // Fire post-destroy hook
    fire_post_destroy(name, &wt_dir, force, meta_dir.as_deref());

    if json {
        let output = DestroyOutput {
            name: name.to_string(),
            path: wt_dir.display().to_string(),
            repos_removed: check.members.len(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("{} Removed worktree '{}'", "✓".green(), name.bold());
    }
    stored
}
