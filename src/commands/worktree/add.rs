use anyhow::Result;
use colored::*;
use meta_core::display::{escape, escape_path};

use meta_cli::worktree::discover_worktree_repos;
use meta_git_lib::worktree::git_ops::git_worktree_add;
use meta_git_lib::worktree::helpers::*;
use meta_git_lib::worktree::record;
use meta_git_lib::worktree::safety::linked_worktree_admin_dir;
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

    // What is added joins the set's record, if meta made the set; one it
    // didn't make stays unrecorded, so `remove` keeps refusing it.
    let set_record = record::find(&wt_dir, existing.iter().map(|r| &r.path));

    // Check every repo before adding any: one refused after others were
    // added would leave a set the caller didn't ask for
    let mut planned = Vec::new();
    for (i, spec) in repo_specs.iter().enumerate() {
        // As in create: a repeat is one worktree, a repeat on another branch
        // is refused
        if let Some(first) = repo_specs.iter().take(i).find(|o| o.alias == spec.alias) {
            if first.branch != spec.branch {
                anyhow::bail!(
                    "Repo '{}' is given twice, on branches '{}' and '{}'",
                    escape(&spec.alias),
                    escape(first.branch.as_deref().unwrap_or("(default)")),
                    escape(spec.branch.as_deref().unwrap_or("(default)"))
                );
            }
            continue;
        }
        if existing.iter().any(|r| r.alias == spec.alias) {
            anyhow::bail!(
                "Repo '{}' already exists in worktree '{name}'",
                escape(&spec.alias)
            );
        }

        // Use recursive lookup for nested paths (containing '/')
        let (source, _project) = lookup_nested_project(&meta_dir, &spec.alias)?;

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
        planned.push((spec, source, dest));
    }

    let mut added = Vec::new();
    let mut failed = None;
    for (spec, source, dest) in planned {
        let branch = resolve_branch(name, None, spec.branch.as_deref());
        // Again now: a checkout added above (a parent repo's) may have put a
        // symlink where this one goes
        if let Err(why) = meta_core::config::resolve_inside(&wt_dir, &spec.alias) {
            failed = Some(anyhow::anyhow!(
                "Refusing to add the worktree for '{}': {why}",
                escape(&spec.alias)
            ));
            break;
        }

        // Ensure parent directories exist for nested paths (e.g., "vendor/")
        if let Some(parent) = dest.parent() {
            if parent != wt_dir {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    failed = Some(
                        anyhow::Error::from(e)
                            .context(format!("Cannot create {}", escape_path(parent))),
                    );
                    break;
                }
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

        // A failed add stops here; what was added is reported
        let created_branch = match git_worktree_add(&source, &dest, &branch, None) {
            Ok(created) => created.is_some(),
            Err(e) => {
                failed = Some(e);
                break;
            }
        };
        // A worktree that can't be recorded is reported as failed, not added
        if let Some(set_record) = &set_record {
            if let Err(e) =
                linked_worktree_admin_dir(&dest).and_then(|admin| record::write(&admin, set_record))
            {
                failed = Some(e.context(format!(
                    "Added '{}', but cannot record it in worktree '{name}'",
                    escape(&spec.alias)
                )));
                break;
            }
        }
        added.push(CreateRepoEntry {
            alias: spec.alias.clone(),
            path: dest.display().to_string(),
            branch,
            created_branch,
        });
    }

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

    failed.map_or(Ok(()), Err)
}
