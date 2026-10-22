//! Membership-only public projection for background workspace discovery.
use anyhow::Result;
use meta_cli::worktree::{discover_worktree_members_recursive, WorktreeMemberInfo};
use meta_git_lib::worktree::helpers::{resolve_existing_worktree, validate_worktree_name};
use serde::Serialize;

use super::cli_types::MembersArgs;

#[derive(Serialize)]
struct MembersOutput {
    name: String,
    repos: Vec<WorktreeMemberInfo>,
}

pub(crate) fn handle_members(args: MembersArgs, json: bool) -> Result<()> {
    validate_worktree_name(&args.name)?;
    let ctx = resolve_existing_worktree(&args.name)?;
    let repos = discover_worktree_members_recursive(&ctx.wt_dir)?;
    anyhow::ensure!(
        !repos.is_empty(),
        "No repos found in worktree '{}'",
        args.name
    );
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&MembersOutput {
                name: args.name,
                repos
            })?
        );
    } else {
        for repo in repos {
            println!("{}  {}", repo.alias, repo.path.display());
        }
    }
    Ok(())
}
