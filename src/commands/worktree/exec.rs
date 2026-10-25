use anyhow::Result;
use colored::*;
use std::path::Path;

use meta_cli::repo_filter;
use meta_cli::worktree::{discover_worktree_repos, WorktreeRepoInfo};
use meta_git_lib::worktree::helpers::*;

use super::cli_types::{CreateArgs, DestroyArgs, ExecArgs};
use crate::git_env;

/// Runs in each of `repos` that `--include`/`--exclude` select (a repo's
/// `.meta` name or its path from `wt_dir`), naming them by that path.
fn build_loop_config(
    wt_dir: &Path,
    repos: &[WorktreeRepoInfo],
    include_filters: Vec<String>,
    exclude_filters: Vec<String>,
    parallel: bool,
    verbose: bool,
    json: bool,
) -> Result<loop_lib::LoopConfig> {
    let directories: Vec<String> = repos.iter().map(|r| r.path.display().to_string()).collect();
    let directories = if include_filters.is_empty() && exclude_filters.is_empty() {
        directories
    } else {
        let tree = find_meta_dir()
            .and_then(|meta_dir| meta_core::config::walk_meta_tree(&meta_dir, None).ok())
            .unwrap_or_default();
        let names = repo_filter::repo_names(&[], &tree);
        repo_filter::select(
            &include_filters,
            &exclude_filters,
            &directories,
            wt_dir,
            &names,
        )?
    };
    Ok(loop_lib::LoopConfig {
        directories,
        ignore: vec![],
        verbose,
        silent: false,
        parallel,
        dry_run: false,
        json_output: json,
        add_aliases_to_global_looprc: false,
        spawn_stagger_ms: 0,
        env: Some(git_env::git_env()),
        max_parallel: None,
        root_dir: Some(wt_dir.to_path_buf()),
    })
}

/// RAII guard that removes an ephemeral worktree on drop.
/// Ensures cleanup even if the exec command panics.
struct EphemeralGuard {
    name: String,
    verbose: bool,
    json: bool,
}

impl Drop for EphemeralGuard {
    fn drop(&mut self) {
        if self.verbose {
            eprintln!("Removing ephemeral worktree '{}'...", self.name);
        }
        let destroy_args = DestroyArgs {
            name: self.name.clone(),
            force: true,
        };
        // Ephemeral cleanup uses strict=false to ensure best-effort cleanup
        if let Err(e) = super::remove::handle_remove(destroy_args, self.verbose, self.json, false) {
            eprintln!(
                "{} Failed to remove ephemeral worktree '{}': {e}",
                "warning:".yellow().bold(),
                self.name
            );
            eprintln!(
                "  Run 'meta worktree remove {} --force' or 'meta worktree prune' to clean up.",
                self.name
            );
        }
    }
}

pub(crate) fn handle_exec(args: ExecArgs, verbose: bool, json: bool) -> Result<()> {
    if args.ephemeral {
        return handle_ephemeral_exec(args, verbose, json);
    }

    let name = &args.name;
    let repos = discover_and_validate_worktree(name)?;

    let config = build_loop_config(
        &resolve_existing_worktree(name)?.wt_dir,
        &repos,
        args.include,
        args.exclude,
        args.parallel,
        verbose,
        json,
    )?;

    loop_lib::run(&config, &args.command)?;
    Ok(())
}

fn handle_ephemeral_exec(args: ExecArgs, verbose: bool, json: bool) -> Result<()> {
    let name = args.name.clone();
    validate_worktree_name(&name)?;

    let cmd_parts = args.command;
    if cmd_parts.is_empty() {
        anyhow::bail!("No command specified after --");
    }

    // Extract filters and parallel flag before moving remaining args into CreateArgs
    let include_filters = args.include;
    let exclude_filters = args.exclude;
    let parallel = args.parallel;

    // Build CreateArgs from the exec args
    let create_args = CreateArgs {
        name: name.clone(),
        commit_ish: args.commit_ish,
        branch: args.branch,
        repos: args.repos,
        all: args.all,
        dry_run: false,
        from_ref: args.from_ref,
        from_pr: args.from_pr,
        ephemeral: true,
        ttl: None,
        custom_meta: args.custom_meta,
        strict: false,  // Exec creates ephemeral worktrees, don't fail on skipped repos
        no_deps: false, // Include dependencies by default
        recursive: false, // Ephemeral exec uses current meta level
    };

    if verbose {
        eprintln!("Creating ephemeral worktree '{name}'...");
    }
    // Ephemeral worktrees use global_strict=false to ensure creation succeeds
    super::create::handle_create(create_args, verbose, json, false)?;

    // Guard ensures cleanup even on panic
    let guard = EphemeralGuard {
        name: name.clone(),
        verbose,
        json,
    };

    // Resolve worktree path for exec
    let meta_dir = find_meta_dir();
    let worktree_root = resolve_worktree_root(meta_dir.as_deref())?;
    let wt_dir = worktree_root.join(&name);

    // Run the command
    let repos = discover_worktree_repos(&wt_dir)?;

    let config = build_loop_config(
        &wt_dir,
        &repos,
        include_filters,
        exclude_filters,
        parallel,
        verbose,
        json,
    );

    let exec_result = config.and_then(|config| loop_lib::run(&config, &cmd_parts));

    // Explicitly drop guard to trigger cleanup before propagating result
    drop(guard);

    // Propagate exec result
    exec_result?;
    Ok(())
}
