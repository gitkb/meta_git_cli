use crate::helpers::get_all_repo_directories;
use chrono::Utc;
use console::style;
use dialoguer::Confirm;
use meta_git_lib::snapshot::{self, RepoState, Snapshot};
use meta_plugin_protocol::CommandResult;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::Path;

/// Show snapshot help text
pub(crate) fn execute_snapshot_help() -> anyhow::Result<CommandResult> {
    println!(
        r#"{}

{}

Usage: meta git snapshot <command> [args]

Commands:
  {}      Create a snapshot of all repos' git state
  {}        List all available snapshots
  {}        Show details of a snapshot
  {}     Restore all repos to a snapshot state
  {}      Delete a snapshot

Examples:
  meta git snapshot create before-upgrade
  meta git snapshot list
  meta git snapshot show before-upgrade
  meta git snapshot restore before-upgrade --dry-run
  meta git snapshot restore before-upgrade --force
  meta git snapshot delete before-upgrade

Snapshots capture the entire workspace state (recursive by default).
Use --force to skip confirmation on restore, --dry-run to preview."#,
        style("meta git snapshot - Workspace State Management").bold(),
        style("[EXPERIMENTAL] File format is subject to change.").yellow(),
        style("create <name>").cyan(),
        style("list").cyan(),
        style("show <name>").cyan(),
        style("restore <name>").cyan(),
        style("delete <name>").cyan(),
    );
    Ok(CommandResult::Message(String::new()))
}

pub(crate) fn execute_snapshot_command_help(command: &str) -> CommandResult {
    let subcommand = command
        .split_whitespace()
        .filter(|word| *word != "--help" && *word != "-h")
        .nth(2);

    let Some(subcommand) = subcommand else {
        return execute_snapshot_help().unwrap_or_else(|e| {
            CommandResult::Error(format!("Failed to show snapshot help: {e}"))
        });
    };

    let help = match subcommand {
        "create" => {
            r#"meta git snapshot create - Save workspace git state

Usage: meta git snapshot create <NAME>

Records each repo's current SHA, branch, and dirty status.

Examples:
  meta git snapshot create before-refactor
  meta git snapshot create before-upgrade"#
        }
        "list" => {
            r#"meta git snapshot list - List saved workspace snapshots

Usage: meta git snapshot list

Shows snapshot names, creation times, repo counts, and dirty repo counts.

Examples:
  meta git snapshot list"#
        }
        "show" => {
            r#"meta git snapshot show - Display one snapshot

Usage: meta git snapshot show <NAME>

Shows per-repo branch, SHA, and dirty state recorded in a snapshot.

Examples:
  meta git snapshot show before-refactor"#
        }
        "restore" => {
            r#"meta git snapshot restore - Restore workspace git state

Usage: meta git snapshot restore <NAME> [--force] [--dry-run]

Restores repos to the recorded branches/SHAs. Dirty repos are stashed before restore.

Options:
  --force     Skip confirmation
  --dry-run   Preview restore actions without changing repos

Examples:
  meta git snapshot restore before-refactor --dry-run
  meta git snapshot restore before-refactor --force"#
        }
        "delete" => {
            r#"meta git snapshot delete - Delete a saved snapshot

Usage: meta git snapshot delete <NAME>

Examples:
  meta git snapshot delete before-refactor"#
        }
        _ => {
            return CommandResult::ShowHelp(Some(format!(
                "unknown snapshot command '{subcommand}'"
            )));
        }
    };

    CommandResult::Message(help.to_string())
}

/// Create a snapshot of the current workspace state
pub(crate) fn execute_snapshot_create(
    args: &[String],
    projects: &[String],
    cwd: &Path,
) -> anyhow::Result<CommandResult> {
    // Parse snapshot name from args
    let name = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .ok_or_else(|| anyhow::anyhow!("Usage: meta git snapshot create <name>"))?;
    snapshot::validate_snapshot_name(name)?;

    // Get all repos (recursive by default)
    let dirs = get_all_repo_directories(projects, cwd)?;

    println!(
        "Creating snapshot '{}' of {} repos...",
        style(name).cyan(),
        dirs.len()
    );

    // Capture repo states in parallel
    let results: Vec<_> = dirs
        .par_iter()
        .map(|dir| {
            let path = if dir == "." {
                cwd.to_path_buf()
            } else {
                cwd.join(dir)
            };

            if !path.exists() || !snapshot::is_git_repo(&path) {
                return (dir.clone(), None);
            }

            let state = snapshot::capture_repo_state(&path);
            (dir.clone(), Some(state))
        })
        .collect();

    // Process results sequentially for display
    let mut repos = HashMap::new();
    let mut dirty_count = 0;

    for (dir, result) in &results {
        match result {
            None => {
                println!(
                    "  {} {} (not a git repo, skipping)",
                    style("⚠").yellow(),
                    dir
                );
            }
            Some(Ok(state)) => {
                if state.dirty {
                    dirty_count += 1;
                    println!("  {} {} (dirty)", style("○").yellow(), dir);
                } else {
                    println!("  {} {}", style("✓").green(), dir);
                }
                repos.insert(dir.clone(), state.clone());
            }
            Some(Err(e)) => {
                println!("  {} {} (error: {})", style("✗").red(), dir, e);
            }
        }
    }

    if repos.is_empty() {
        anyhow::bail!("No repos captured");
    }

    let snap = Snapshot {
        name: name.clone(),
        created: Utc::now(),
        repos,
    };

    snapshot::save_snapshot(cwd, &snap)?;

    println!();
    println!(
        "{} Captured state of {} repos",
        style("✓").green(),
        snap.repos.len()
    );
    if dirty_count > 0 {
        println!(
            "{} {} repo(s) have uncommitted changes (recorded as dirty)",
            style("⚠").yellow(),
            dirty_count
        );
    }
    println!(
        "Snapshot saved: {}",
        style(format!(".meta-snapshots/{name}.json")).dim()
    );

    Ok(CommandResult::Message(String::new()))
}

/// List all snapshots
pub(crate) fn execute_snapshot_list(cwd: &Path) -> anyhow::Result<CommandResult> {
    let snapshots = snapshot::list_snapshots(cwd)?;

    if snapshots.is_empty() {
        println!("No snapshots found.");
        println!(
            "Create one with: {}",
            style("meta git snapshot create <name>").cyan()
        );
        return Ok(CommandResult::Message(String::new()));
    }

    println!("Snapshots:\n");
    for info in snapshots {
        let dirty_note = if info.dirty_count > 0 {
            format!(" ({} dirty)", info.dirty_count)
        } else {
            String::new()
        };
        println!(
            "  {} - {} repos{} - {}",
            style(&info.name).cyan().bold(),
            info.repo_count,
            style(dirty_note).yellow(),
            style(info.created.format("%Y-%m-%d %H:%M:%S")).dim()
        );
    }
    println!();

    Ok(CommandResult::Message(String::new()))
}

/// Show details of a snapshot
pub(crate) fn execute_snapshot_show(args: &[String], cwd: &Path) -> anyhow::Result<CommandResult> {
    let name = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .ok_or_else(|| anyhow::anyhow!("Usage: meta git snapshot show <name>"))?;

    let snap = snapshot::load_snapshot(cwd, name)?;

    println!("Snapshot: {}", style(&snap.name).cyan().bold());
    println!("Created:  {}", snap.created.format("%Y-%m-%d %H:%M:%S UTC"));
    println!("Repos:    {}", snap.repos.len());
    println!();

    // Sort repos by name for consistent output
    let mut repos: Vec<_> = snap.repos.iter().collect();
    repos.sort_by(|a, b| a.0.cmp(b.0));

    for (name, state) in repos {
        let branch_info = state
            .branch
            .as_ref()
            .map(|b| format!(" -> {b}"))
            .unwrap_or_else(|| " (detached)".to_string());

        let dirty_marker = if state.dirty {
            format!(" {}", style("(dirty)").yellow())
        } else {
            String::new()
        };

        println!(
            "  {} {}{}{}",
            style(snapshot::short_sha(&state.sha)).dim(),
            name,
            style(branch_info).cyan(),
            dirty_marker
        );
    }

    Ok(CommandResult::Message(String::new()))
}

/// Restore workspace to a snapshot state
pub(crate) fn execute_snapshot_restore(
    args: &[String],
    _projects: &[String],
    dry_run: bool,
    cwd: &Path,
) -> anyhow::Result<CommandResult> {
    // Parse args
    let mut name: Option<&str> = None;
    let mut force = false;
    let mut dry_run = dry_run;

    for arg in args {
        match arg.as_str() {
            "--force" | "-f" => force = true,
            "--dry-run" => dry_run = true,
            s if !s.starts_with('-') => name = Some(s),
            _ => {}
        }
    }

    let name = name.ok_or_else(|| {
        anyhow::anyhow!("Usage: meta git snapshot restore <name> [--force] [--dry-run]")
    })?;

    let snap = snapshot::load_snapshot(cwd, name)?;

    // Analyze what would change
    let mut repos_to_restore: Vec<(&str, &RepoState, bool)> = Vec::new();
    let mut missing_repos = Vec::new();
    let mut moved_back = Vec::new();

    for (repo_name, state) in &snap.repos {
        let path = if repo_name == "." {
            cwd.to_path_buf()
        } else {
            cwd.join(repo_name)
        };

        if !path.exists() || !snapshot::is_git_repo(&path) {
            missing_repos.push(repo_name.as_str());
            continue;
        }

        // Check if current state is dirty
        let current_state = snapshot::capture_repo_state(&path)?;
        repos_to_restore.push((repo_name, state, current_state.dirty));
        match snapshot::backups_needed(&path, state, name) {
            Ok(backups) => moved_back.extend(backups.into_iter().map(|backup| {
                format!(
                    "{repo_name}: {} commit(s) on {} -> kept on {}",
                    backup.commits, backup.from, backup.name
                )
            })),
            // The restore itself refuses this repo; say so before confirming.
            Err(e) => moved_back.push(format!(
                "{repo_name}: cannot check for newer commits ({e:#})"
            )),
        }
    }

    let dirty_count = repos_to_restore.iter().filter(|(_, _, d)| *d).count();

    // Show preview
    println!(
        "Restore {} repos to snapshot '{}':",
        repos_to_restore.len(),
        style(name).cyan()
    );
    println!(
        "  - {} repos will checkout to their recorded SHA",
        repos_to_restore.len() - dirty_count
    );
    if dirty_count > 0 {
        println!(
            "  - {} repos have uncommitted changes (will be stashed)",
            style(dirty_count).yellow()
        );
    }
    if !missing_repos.is_empty() {
        println!(
            "  - {} repos missing (will be skipped): {}",
            style(missing_repos.len()).red(),
            missing_repos.join(", ")
        );
    }
    if !moved_back.is_empty() {
        println!(
            "  - {} commit sets would end up on no branch; they are kept on backup branches:",
            style(moved_back.len()).yellow()
        );
        for line in &moved_back {
            println!("      {line}");
        }
    }
    println!();

    if dry_run {
        println!("{} Dry run - no changes made", style("[DRY RUN]").cyan());
        return Ok(CommandResult::Message(String::new()));
    }

    // Confirm unless --force
    if !force {
        let proceed = Confirm::new()
            .with_prompt("Proceed?")
            .default(false)
            .interact()?;

        if !proceed {
            println!("Aborted.");
            return Ok(CommandResult::Message(String::new()));
        }
    }

    // Execute restore
    println!("Restoring...");
    let mut success_count = 0;
    let mut fail_count = 0;

    for (repo_name, state, _is_dirty) in &repos_to_restore {
        let path = if *repo_name == "." {
            cwd.to_path_buf()
        } else {
            cwd.join(repo_name)
        };

        let result = snapshot::restore_repo_state(&path, state, name)?;

        let stash_note = if result.stashed {
            format!(
                " {}",
                style("(changes stashed: see git stash list)").yellow()
            )
        } else {
            String::new()
        };
        let mark = if result.success {
            success_count += 1;
            style("✓").green()
        } else {
            fail_count += 1;
            style("✗").red()
        };
        println!("  {mark} {repo_name} {}{stash_note}", result.message);
    }

    println!();
    if fail_count > 0 {
        return Ok(CommandResult::Error(format!(
            "Restored {success_count} repo(s), {fail_count} failed"
        )));
    }
    println!("{} Restored {} repo(s)", style("✓").green(), success_count);

    Ok(CommandResult::Message(String::new()))
}

/// Delete a snapshot
pub(crate) fn execute_snapshot_delete(
    args: &[String],
    cwd: &Path,
) -> anyhow::Result<CommandResult> {
    let name = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .ok_or_else(|| anyhow::anyhow!("Usage: meta git snapshot delete <name>"))?;

    snapshot::delete_snapshot(cwd, name)?;

    println!(
        "{} Deleted snapshot '{}'",
        style("✓").green(),
        style(name).cyan()
    );

    Ok(CommandResult::Message(String::new()))
}
