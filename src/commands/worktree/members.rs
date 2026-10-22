//! Membership-only public projection for background workspace discovery.
use anyhow::Result;
use meta_cli::worktree::{
    detect_worktree_context, discover_worktree_members_recursive, WorktreeMemberInfo,
};
use meta_git_lib::worktree::helpers::{resolve_existing_worktree, validate_worktree_name};
use serde::Serialize;

use super::cli_types::MembersArgs;

#[derive(Serialize)]
struct MembersOutput {
    name: String,
    repos: Vec<WorktreeMemberInfo>,
}

pub(crate) fn handle_members(args: MembersArgs, json: bool) -> Result<()> {
    let (name, wt_dir) = match args.name {
        Some(name) => {
            validate_worktree_name(&name)?;
            let ctx = resolve_existing_worktree(&name)?;
            (name, ctx.wt_dir)
        }
        None => {
            let cwd = std::env::current_dir()?;
            let (name, wt_dir, _) = detect_worktree_context(&cwd).ok_or_else(|| {
                anyhow::anyhow!(
                    "Worktree name required outside a realized .worktrees/<name> directory; \
                     specify NAME or run from inside the worktree"
                )
            })?;
            validate_worktree_name(&name)?;
            (name, wt_dir)
        }
    };
    let repos = discover_worktree_members_recursive(&wt_dir)?;
    anyhow::ensure!(!repos.is_empty(), "No repos found in worktree '{}'", name);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&MembersOutput { name, repos })?
        );
    } else {
        for repo in repos {
            println!("{}  {}", repo.alias, repo.path.display());
        }
    }
    Ok(())
}
