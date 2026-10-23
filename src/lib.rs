//! meta-git library
//!
//! Provides git operations optimized for meta repositories.

mod clone;
mod clone_worker;
mod commit;
mod git_env;
mod helpers;
mod snapshot;
mod ssh;
mod ssh_setup;
mod status;
mod update;

mod commands;

use log::debug;
use meta_plugin_protocol::{CommandResult, PlannedCommand, PluginRequestOptions};
use std::path::Path;

use helpers::get_project_directories_with_fallback;

/// Execute a command for meta repositories.
///
/// The `projects` parameter is the list of project directories passed from meta_cli.
/// When meta_cli runs with `--recursive`, it discovers nested .meta files and passes
/// all project directories here. If `projects` is empty, we fall back to reading
/// the local .meta file via `get_project_directories()`.
pub fn execute_command(
    command: &str,
    args: &[String],
    projects: &[String],
    options: &PluginRequestOptions,
    cwd: &Path,
) -> CommandResult {
    debug!("[meta_git_cli] Plugin invoked with command: '{command}'");
    debug!("[meta_git_cli] Args: {args:?}");
    debug!(
        "[meta_git_cli] Options: parallel={}, dry_run={}, verbose={}, recursive={}, depth={:?}",
        options.parallel, options.dry_run, options.verbose, options.recursive, options.depth
    );
    debug!("[meta_git_cli] Projects from meta_cli: {projects:?}");

    let help_requested = has_help_token(command, args);

    // Worktree commands (meta worktree * and meta git worktree *)
    // Checked before --help intercept so worktree subcommands can handle their own --help via clap
    if command.starts_with("worktree") || command.starts_with("git worktree") {
        return commands::worktree::execute_worktree_command(command, args, options, cwd);
    }

    if command.starts_with("git snapshot") && help_requested {
        return snapshot::execute_snapshot_command_help(command);
    }

    if help_requested {
        if let Some(help) = build_passthrough_git_help(command, args, projects, cwd) {
            return CommandResult::Message(help);
        }
        return CommandResult::ShowHelp(None);
    }

    let result = match command {
        "git status" => status::execute_git_status(projects, options, cwd),
        "git clone" => clone::execute_git_clone(args, options, cwd),
        "git update" => update::execute_git_update(projects, options.dry_run, cwd),
        "git commit" => commit::execute_git_commit(args, projects, options, cwd),
        "git snapshot" => snapshot::execute_snapshot_help(),
        "git snapshot create" => snapshot::execute_snapshot_create(args, projects, cwd),
        "git snapshot list" => snapshot::execute_snapshot_list(cwd),
        "git snapshot show" => snapshot::execute_snapshot_show(args, cwd),
        "git snapshot restore" => {
            snapshot::execute_snapshot_restore(args, projects, options.dry_run, cwd)
        }
        "git snapshot delete" => snapshot::execute_snapshot_delete(args, cwd),
        // Fallback: run raw git command across all repos
        _ => return execute_raw_git_command(command, args, projects, options, cwd),
    };

    match result {
        Ok(cmd_result) => cmd_result,
        Err(e) => CommandResult::Error(format!("{e}")),
    }
}

/// Check if a git command accesses remote repositories
fn is_remote_command(command: &str) -> bool {
    let remote_commands = ["push", "pull", "fetch", "clone", "ls-remote"];
    let cmd_lower = command.to_lowercase();
    remote_commands.iter().any(|rc| cmd_lower.contains(rc))
}

fn has_help_token(command: &str, args: &[String]) -> bool {
    command
        .split_whitespace()
        .chain(args.iter().map(String::as_str))
        .any(|arg| arg == "--help" || arg == "-h")
}

fn build_passthrough_git_help(
    command: &str,
    args: &[String],
    projects: &[String],
    cwd: &Path,
) -> Option<String> {
    let command_words: Vec<&str> = command
        .split_whitespace()
        .filter(|word| *word != "--help" && *word != "-h")
        .collect();

    if command_words.first().copied() != Some("git") {
        return None;
    }

    let adapted_commands = [
        "clone", "status", "update", "commit", "snapshot", "worktree",
    ];
    let git_args: Vec<String> = command_words
        .iter()
        .skip(1)
        .map(|s| s.to_string())
        .chain(
            args.iter()
                .filter(|arg| arg.as_str() != "--help" && arg.as_str() != "-h")
                .cloned(),
        )
        .collect();
    let subcommand = git_args.first()?;

    if adapted_commands.contains(&subcommand.as_str()) {
        return None;
    }

    let display_command = format!("git {}", git_args.join(" "));
    let scope = match get_project_directories_with_fallback(projects, cwd) {
        Ok(dirs) => {
            if dirs.is_empty() {
                "Current scope: no repos were discovered.".to_string()
            } else {
                format!(
                    "Current scope: {} repo(s): {}",
                    dirs.len(),
                    dirs.iter()
                        .map(|d| d.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        }
        Err(_) => "Current scope: unable to resolve repos from the current directory.".to_string(),
    };

    Some(format!(
        "meta git {subcommand} - Pass-through git command\n\n\
Usage: meta git {subcommand} [GIT_ARGS...] [META_OPTIONS]\n\n\
Runs `{display_command}` in each repo in the current meta workspace.\n\n\
{scope}\n\n\
Meta options:\n\
  --dry-run             Show the command that would run in each repo\n\
  --recursive           Include repos from nested meta workspaces\n\
  --depth <N>           Limit recursive traversal depth\n\
  --include <REPOS>     Only run in specified repos/directories\n\
  --exclude <REPOS>     Skip specified repos/directories\n\
  --tag <TAGS>          Filter by project tag(s), comma-separated\n\
  --parallel            Run repo commands concurrently\n\
  --sequential          Run repo commands one at a time\n\n\
Examples:\n\
  meta git {subcommand}\n\
  meta git {subcommand} --dry-run\n\
  meta git {subcommand} --recursive --dry-run\n\n\
Git help: run `git {subcommand} --help` directly for upstream git options."
    ))
}

/// Execute a raw git command across all repos (fallback for unrecognized subcommands)
fn execute_raw_git_command(
    command: &str,
    args: &[String],
    projects: &[String],
    options: &PluginRequestOptions,
    cwd: &Path,
) -> CommandResult {
    use meta_plugin_protocol::ExecutionPlan;

    // Get project directories
    let dirs = match get_project_directories_with_fallback(projects, cwd) {
        Ok(d) => d,
        Err(e) => return CommandResult::Error(format!("Failed to get project directories: {e}")),
    };

    // Build the full git command string
    // command is e.g. "git add ." and args contains any additional arguments
    let full_cmd = if args.is_empty() {
        command.to_string()
    } else {
        format!("{} {}", command, args.join(" "))
    };

    // Set git-specific environment variables (pager, colors, prompts)
    let git_env = Some(git_env::git_env());

    let commands: Vec<PlannedCommand> = dirs
        .into_iter()
        .map(|dir| PlannedCommand {
            dir,
            cmd: full_cmd.clone(),
            env: git_env.clone(),
        })
        .collect();

    // For remote commands running in parallel, establish SSH ControlMaster
    // connections and inject GIT_SSH_COMMAND into each planned command.
    if options.parallel && is_remote_command(command) {
        let urls = ssh::discover_ssh_urls(cwd);

        // HTTPS-only workspaces don't need SSH multiplexing
        if urls.is_empty() {
            return CommandResult::Plan(commands, Some(true));
        }

        let url_refs: Vec<&str> = urls.iter().map(|s| s.as_str()).collect();

        let (ssh_env, parallel_ok) = match ssh_setup::establish_ssh_masters(&url_refs) {
            ssh_setup::SshMasters::OurSockets(dir) => {
                (Some(ssh_setup::git_ssh_command(&dir)), true)
            }
            ssh_setup::SshMasters::UserManaged => (None, true),
            ssh_setup::SshMasters::Failed => (None, false),
        };

        // Inject GIT_SSH_COMMAND into every planned command's env
        let commands: Vec<PlannedCommand> = commands
            .into_iter()
            .map(|mut cmd| {
                if let Some(ref ssh) = ssh_env {
                    let env = cmd.env.get_or_insert_with(Default::default);
                    env.insert("GIT_SSH_COMMAND".to_string(), ssh.clone());
                }
                cmd
            })
            .collect();

        // SSH ControlMaster has a default MaxSessions limit of 10 (server-side).
        const SSH_MAX_SESSIONS: usize = 10;

        // Stagger spawns by 25ms to prevent SSH socket saturation.
        const SSH_SPAWN_STAGGER_MS: u64 = 25;

        if parallel_ok {
            CommandResult::FullPlan(ExecutionPlan {
                pre_commands: vec![],
                commands,
                post_commands: vec![],
                parallel: Some(true),
                max_parallel: Some(SSH_MAX_SESSIONS),
                spawn_stagger_ms: Some(SSH_SPAWN_STAGGER_MS),
            })
        } else {
            // SSH setup failed — fall back to sequential
            CommandResult::Plan(commands, Some(false))
        }
    } else {
        CommandResult::Plan(commands, Some(options.parallel))
    }
}

/// Get help text for the plugin
pub fn get_help_text() -> &'static str {
    r#"meta git - Meta CLI Git Plugin

SPECIAL COMMANDS:
  These commands have meta-specific implementations:

  meta git clone <meta-repo-url> [options]
    Clones the meta repository and all child repositories defined in its manifest.

    Options:
      --recursive       Clone nested meta repositories recursively
      --meta-depth N    Limit recursive meta-repo discovery to N levels deep
      --parallel N      Clone up to N repositories in parallel
      --depth N         Create a shallow clone with truncated history

  meta git update
    Updates all repositories by cloning any missing repos and pulling the latest
    changes. Runs in parallel for efficiency.

  meta git commit --edit
    Opens an editor to create different commit messages for each repo.

SNAPSHOT COMMANDS (EXPERIMENTAL - file format subject to change):
  Capture and restore workspace state for safe batch operations:

  meta git snapshot create <name>
    Record the current git state (SHA, branch, dirty status) of ALL repos.
    Snapshots are recursive by default - they capture the entire workspace.

  meta git snapshot list
    List all available snapshots with creation date and repo count.

  meta git snapshot show <name>
    Display details of a snapshot including per-repo state.

  meta git snapshot restore <name> [--force] [--dry-run]
    Restore all repos to the recorded snapshot state. Prompts for confirmation.
    Dirty repos are automatically stashed before restore.
    Use --force to skip confirmation, --dry-run to preview changes.

  meta git snapshot delete <name>
    Delete a snapshot file.

PASS-THROUGH COMMANDS:
  All other git commands are passed through to each repository:

    meta git status      - Run 'git status' in all repos
    meta git pull        - Run 'git pull' in all repos
    meta git push        - Run 'git push' in all repos
    meta git checkout    - Run 'git checkout' in all repos
    meta git <any>       - Run 'git <any>' in all repos

FILTERING OPTIONS:
  These meta/loop options work with all pass-through commands:

    --tag <tags>        Filter by project tag(s), comma-separated
    --include-only      Only run in specified directories
    --exclude           Skip specified directories
    --parallel          Run commands in parallel

Examples:
  meta git clone https://github.com/example/meta-repo.git
  meta git status
  meta git pull --rebase
  meta git pull --tag backend
  meta git commit --edit
  meta git checkout -b feature/new --include-only api,frontend
  meta git snapshot create before-upgrade
  meta git snapshot restore before-upgrade
"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use commit::parse_multi_commit_file;
    use helpers::get_project_directories;
    use meta_plugin_protocol::{ExecutionPlan, PlanResponse, PlannedCommand};
    use tempfile::TempDir;

    #[test]
    fn test_execute_git_status_no_meta_file() {
        let temp_dir = TempDir::new().unwrap();
        let options = PluginRequestOptions::default();

        let result = execute_command("git status", &[], &[], &options, temp_dir.path());

        // Should succeed (returns a Plan, not an Error)
        assert!(!matches!(result, CommandResult::Error(_)));
    }

    #[test]
    fn test_meta_config_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let meta_path = dir.path().join(".meta");
        std::fs::write(
            &meta_path,
            r#"{"projects": {"foo": "git@github.com:org/foo.git", "bar": "git@github.com:org/bar.git"}}"#,
        )
        .unwrap();
        let (projects, _) = meta_core::config::parse_meta_config(&meta_path).unwrap();
        assert_eq!(projects.len(), 2);
        let names: std::collections::HashSet<_> =
            projects.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains("foo"));
        assert!(names.contains("bar"));
    }

    #[test]
    fn test_unknown_command_falls_through_to_raw_git() {
        // Unknown git subcommands should fall through to raw git execution
        let options = PluginRequestOptions::default();
        let result = execute_command("git unknown", &[], &[], &options, Path::new("."));
        // Should return a Plan (to run `git unknown` in all repos), not ShowHelp
        assert!(matches!(result, CommandResult::Plan(_, _)));
    }

    #[test]
    fn test_get_help_text() {
        let help = get_help_text();
        assert!(help.contains("meta git clone"));
        assert!(help.contains("meta git update"));
        assert!(!help.contains("meta git setup-ssh"));
    }

    #[test]
    fn test_parse_multi_commit_file() {
        let content = r#"# Meta Multi-Commit
# Each section represents one repository.

========== meta_cli ==========
# 3 file(s) staged: src/main.rs, src/lib.rs, Cargo.toml

feat: add new feature

========== meta_mcp ==========
# 2 file(s) staged: src/main.rs, Cargo.toml

fix: fix bug in MCP server

========== empty_repo ==========
# 1 file(s) staged: file.rs

# No message here - should be skipped

"#;

        let commits = parse_multi_commit_file(content);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].0, "meta_cli");
        assert_eq!(commits[0].1, "feat: add new feature");
        assert_eq!(commits[1].0, "meta_mcp");
        assert_eq!(commits[1].1, "fix: fix bug in MCP server");
    }

    #[test]
    fn test_parse_multi_commit_file_multiline_message() {
        let content = r#"========== repo1 ==========
# 1 file staged

feat: add feature

This is a longer description
that spans multiple lines.

- bullet point 1
- bullet point 2
"#;

        let commits = parse_multi_commit_file(content);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].0, "repo1");
        assert!(commits[0].1.contains("feat: add feature"));
        assert!(commits[0].1.contains("bullet point 1"));
    }

    #[test]
    fn test_parse_multi_commit_file_empty() {
        let content = "";
        let commits = parse_multi_commit_file(content);
        assert!(commits.is_empty());
    }

    #[test]
    fn test_parse_multi_commit_file_only_comments() {
        let content = r#"# Meta Multi-Commit
# Each section represents one repository.
# This file has only comments
"#;
        let commits = parse_multi_commit_file(content);
        assert!(commits.is_empty());
    }

    #[test]
    fn test_parse_multi_commit_file_whitespace_only_message() {
        let content = r#"========== repo1 ==========
# 1 file staged



========== repo2 ==========
# 1 file staged

valid message
"#;
        let commits = parse_multi_commit_file(content);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].0, "repo2");
        assert_eq!(commits[0].1, "valid message");
    }

    #[test]
    fn test_parse_multi_commit_file_special_characters_in_repo_name() {
        let content = r#"========== my-repo_v2.0 ==========
# 1 file staged

fix: handle special chars
"#;
        let commits = parse_multi_commit_file(content);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].0, "my-repo_v2.0");
    }

    #[test]
    fn test_parse_multi_commit_file_preserves_message_whitespace() {
        let content = r#"========== repo ==========
# staged files

first line
  indented line
    more indent

last line
"#;
        let commits = parse_multi_commit_file(content);
        assert_eq!(commits.len(), 1);
        assert!(commits[0].1.contains("  indented line"));
        assert!(commits[0].1.contains("    more indent"));
    }

    #[test]
    fn test_parse_multi_commit_file_deleted_section() {
        // Simulates user deleting a section entirely
        let content = r#"========== repo1 ==========
# 1 file staged

first commit

========== repo3 ==========
# 1 file staged

third commit
"#;
        let commits = parse_multi_commit_file(content);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].0, "repo1");
        assert_eq!(commits[1].0, "repo3");
    }

    #[test]
    fn test_parse_multi_commit_file_single_repo() {
        let content = r#"========== only_repo ==========
# 5 files staged

the only commit message
"#;
        let commits = parse_multi_commit_file(content);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].0, "only_repo");
        assert_eq!(commits[0].1, "the only commit message");
    }

    #[test]
    fn test_help_text_contains_commit_edit() {
        let help = get_help_text();
        assert!(help.contains("meta git commit --edit"));
        assert!(help.contains("PASS-THROUGH COMMANDS"));
        assert!(help.contains("FILTERING OPTIONS"));
    }

    // ============ Execution Plan Tests ============

    #[test]
    fn test_execution_plan_serialization() {
        let plan = ExecutionPlan {
            pre_commands: vec![],
            commands: vec![
                PlannedCommand {
                    dir: "./repo1".to_string(),
                    cmd: "git status".to_string(),
                    env: None,
                },
                PlannedCommand {
                    dir: "./repo2".to_string(),
                    cmd: "git status".to_string(),
                    env: None,
                },
            ],
            post_commands: vec![],
            parallel: Some(false),
            max_parallel: None,
            spawn_stagger_ms: None,
        };

        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"dir\":\"./repo1\""));
        assert!(json.contains("\"cmd\":\"git status\""));
        assert!(json.contains("\"parallel\":false"));
    }

    #[test]
    fn test_execution_plan_without_parallel() {
        let plan = ExecutionPlan {
            pre_commands: vec![],
            commands: vec![PlannedCommand {
                dir: ".".to_string(),
                cmd: "ls".to_string(),
                env: None,
            }],
            post_commands: vec![],
            parallel: None,
            max_parallel: None,
            spawn_stagger_ms: None,
        };

        let json = serde_json::to_string(&plan).unwrap();
        // parallel and max_parallel should be omitted when None due to skip_serializing_if
        assert!(!json.contains("parallel"));
        assert!(!json.contains("max_parallel"));
        assert!(!json.contains("spawn_stagger_ms"));
    }

    #[test]
    fn test_planned_command_serialization() {
        let cmd = PlannedCommand {
            dir: "/absolute/path".to_string(),
            cmd: "git pull --rebase".to_string(),
            env: None,
        };

        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"dir\":\"/absolute/path\""));
        assert!(json.contains("\"cmd\":\"git pull --rebase\""));
    }

    #[test]
    fn test_plan_response_serialization() {
        let response = PlanResponse {
            plan: ExecutionPlan {
                pre_commands: vec![],
                commands: vec![PlannedCommand {
                    dir: "project".to_string(),
                    cmd: "make build".to_string(),
                    env: None,
                }],
                post_commands: vec![],
                parallel: Some(true),
                max_parallel: None,
                spawn_stagger_ms: None,
            },
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"plan\":"));
        assert!(json.contains("\"commands\":"));
        assert!(json.contains("\"dir\":\"project\""));
        assert!(json.contains("\"cmd\":\"make build\""));
        assert!(json.contains("\"parallel\":true"));
    }

    #[test]
    fn test_plan_response_structure() {
        // Test that the JSON structure matches what subprocess_plugins expects
        let response = PlanResponse {
            plan: ExecutionPlan {
                pre_commands: vec![],
                commands: vec![
                    PlannedCommand {
                        dir: "a".to_string(),
                        cmd: "cmd1".to_string(),
                        env: None,
                    },
                    PlannedCommand {
                        dir: "b".to_string(),
                        cmd: "cmd2".to_string(),
                        env: None,
                    },
                ],
                post_commands: vec![],
                parallel: Some(false),
                max_parallel: None,
                spawn_stagger_ms: None,
            },
        };

        let json = serde_json::to_string(&response).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        // Verify structure matches what shim expects
        assert!(parsed.get("plan").is_some());
        let plan = parsed.get("plan").unwrap();
        assert!(plan.get("commands").is_some());
        let commands = plan.get("commands").unwrap().as_array().unwrap();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].get("dir").unwrap().as_str().unwrap(), "a");
        assert_eq!(commands[0].get("cmd").unwrap().as_str().unwrap(), "cmd1");
        assert!(!plan.get("parallel").unwrap().as_bool().unwrap());
    }

    #[test]
    fn test_execution_plan_empty_commands() {
        let plan = ExecutionPlan {
            pre_commands: vec![],
            commands: vec![],
            post_commands: vec![],
            parallel: None,
            max_parallel: None,
            spawn_stagger_ms: None,
        };

        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"commands\":[]"));
    }

    #[test]
    fn test_planned_command_with_special_chars() {
        let cmd = PlannedCommand {
            dir: "./path with spaces".to_string(),
            cmd: "git commit -m \"feat: add feature\"".to_string(),
            env: None,
        };

        let json = serde_json::to_string(&cmd).unwrap();
        // JSON should properly escape the string
        assert!(json.contains("path with spaces"));
        assert!(json.contains("\\\"feat: add feature\\\""));
    }

    #[test]
    fn test_planned_command_git_clone() {
        let cmd = PlannedCommand {
            dir: "/home/user/workspace".to_string(),
            cmd: "git clone git@github.com:org/repo.git my-repo".to_string(),
            env: None,
        };

        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("git clone"));
        assert!(json.contains("git@github.com:org/repo.git"));
    }

    #[test]
    fn test_execution_plan_many_commands() {
        let commands: Vec<PlannedCommand> = (0..100)
            .map(|i| PlannedCommand {
                dir: format!("./repo_{i}"),
                cmd: "git status".to_string(),
                env: None,
            })
            .collect();

        let plan = ExecutionPlan {
            pre_commands: vec![],
            commands,
            post_commands: vec![],
            parallel: Some(true),
            max_parallel: None,
            spawn_stagger_ms: None,
        };

        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("./repo_0"));
        assert!(json.contains("./repo_99"));
        assert!(json.contains("\"parallel\":true"));
    }

    // ============ get_project_directories Tests ============

    #[test]
    fn test_get_project_directories_no_meta_file() {
        let temp_dir = TempDir::new().unwrap();

        let dirs = get_project_directories(temp_dir.path()).unwrap();

        // Should return just "." when no .meta file
        assert_eq!(dirs, vec!["."]);
    }

    #[test]
    fn test_get_project_directories_with_meta_file() {
        let temp_dir = TempDir::new().unwrap();

        // Create .meta file
        let meta_content = r#"{"projects": {"alpha": "url1", "beta": "url2", "gamma": "url3"}}"#;
        std::fs::write(temp_dir.path().join(".meta"), meta_content).unwrap();

        let dirs = get_project_directories(temp_dir.path()).unwrap();

        // Should return "." plus sorted project names
        assert_eq!(dirs.len(), 4);
        assert_eq!(dirs[0], ".");
        assert_eq!(dirs[1], "alpha");
        assert_eq!(dirs[2], "beta");
        assert_eq!(dirs[3], "gamma");
    }

    #[test]
    fn test_get_project_directories_sorted() {
        let temp_dir = TempDir::new().unwrap();

        // Create .meta file with unsorted projects
        let meta_content = r#"{"projects": {"zebra": "url1", "alpha": "url2", "middle": "url3"}}"#;
        std::fs::write(temp_dir.path().join(".meta"), meta_content).unwrap();

        let dirs = get_project_directories(temp_dir.path()).unwrap();

        // Projects should be sorted alphabetically
        assert_eq!(dirs[1], "alpha");
        assert_eq!(dirs[2], "middle");
        assert_eq!(dirs[3], "zebra");
    }

    #[test]
    fn test_get_project_directories_empty_projects() {
        let temp_dir = TempDir::new().unwrap();

        // Create .meta file with no projects
        let meta_content = r#"{"projects": {}}"#;
        std::fs::write(temp_dir.path().join(".meta"), meta_content).unwrap();

        let dirs = get_project_directories(temp_dir.path()).unwrap();

        // Should return just "."
        assert_eq!(dirs, vec!["."]);
    }

    // ============ Integration-like Tests ============

    #[test]
    fn test_git_status_returns_execution_plan_format() {
        // We can't easily test the actual output, but we can verify
        // the plan structure by creating one and checking its JSON format
        let dirs = vec![".".to_string(), "repo1".to_string(), "repo2".to_string()];

        let commands: Vec<PlannedCommand> = dirs
            .into_iter()
            .map(|dir| PlannedCommand {
                dir,
                cmd: "git status".to_string(),
                env: None,
            })
            .collect();

        let response = PlanResponse {
            plan: ExecutionPlan {
                pre_commands: vec![],
                commands,
                post_commands: vec![],
                parallel: Some(false),
                max_parallel: None,
                spawn_stagger_ms: None,
            },
        };

        let json = serde_json::to_string(&response).unwrap();

        // Verify it can be parsed as expected by the shim
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["plan"]["commands"].is_array());
        assert_eq!(parsed["plan"]["commands"].as_array().unwrap().len(), 3);
        assert!(!parsed["plan"]["parallel"].as_bool().unwrap());
    }

    #[test]
    fn test_git_update_plan_for_missing_repos() {
        // Simulate what execute_git_update would return for missing repos
        let missing_repos = [
            ("repo1", "git@github.com:org/repo1.git"),
            ("repo2", "git@github.com:org/repo2.git"),
        ];

        let cwd = "/home/user/workspace";
        let commands: Vec<PlannedCommand> = missing_repos
            .iter()
            .map(|(name, url)| PlannedCommand {
                dir: cwd.to_string(),
                cmd: format!("git clone {url} {name}"),
                env: None,
            })
            .collect();

        let response = PlanResponse {
            plan: ExecutionPlan {
                pre_commands: vec![],
                commands,
                post_commands: vec![],
                parallel: Some(false),
                max_parallel: None,
                spawn_stagger_ms: None,
            },
        };

        let json = serde_json::to_string(&response).unwrap();

        // Verify clone commands are in the plan
        assert!(json.contains("git clone"));
        assert!(json.contains("repo1"));
        assert!(json.contains("repo2"));
    }

    // ============ Remote Command Detection Tests ============

    #[test]
    fn test_is_remote_command_push() {
        assert!(super::is_remote_command("git push"));
        assert!(super::is_remote_command("git push origin main"));
    }

    #[test]
    fn test_is_remote_command_pull() {
        assert!(super::is_remote_command("git pull"));
        assert!(super::is_remote_command("git pull --rebase"));
    }

    #[test]
    fn test_is_remote_command_fetch() {
        assert!(super::is_remote_command("git fetch"));
        assert!(super::is_remote_command("git fetch --all"));
    }

    #[test]
    fn test_is_remote_command_clone() {
        assert!(super::is_remote_command("git clone"));
        assert!(super::is_remote_command(
            "git clone https://github.com/org/repo.git"
        ));
    }

    #[test]
    fn test_is_remote_command_ls_remote() {
        assert!(super::is_remote_command("git ls-remote"));
    }

    #[test]
    fn test_is_remote_command_local_commands() {
        assert!(!super::is_remote_command("git status"));
        assert!(!super::is_remote_command("git add ."));
        assert!(!super::is_remote_command("git commit -m \"msg\""));
        assert!(!super::is_remote_command("git log"));
        assert!(!super::is_remote_command("git diff"));
        assert!(!super::is_remote_command("git branch"));
        assert!(!super::is_remote_command("git checkout main"));
    }

    #[test]
    fn test_execution_plan_max_parallel_serialization() {
        let plan = ExecutionPlan {
            pre_commands: vec![],
            commands: vec![PlannedCommand {
                dir: ".".to_string(),
                cmd: "git push".to_string(),
                env: None,
            }],
            post_commands: vec![],
            parallel: Some(true),
            max_parallel: Some(10),
            spawn_stagger_ms: Some(25),
        };

        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"max_parallel\":10"));
        assert!(json.contains("\"spawn_stagger_ms\":25"));

        // Verify round-trip
        let restored: ExecutionPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.max_parallel, Some(10));
        assert_eq!(restored.spawn_stagger_ms, Some(25));
    }

    #[test]
    fn test_execution_plan_max_parallel_omitted_when_none() {
        let plan = ExecutionPlan {
            pre_commands: vec![],
            commands: vec![PlannedCommand {
                dir: ".".to_string(),
                cmd: "git status".to_string(),
                env: None,
            }],
            post_commands: vec![],
            parallel: Some(true),
            max_parallel: None,
            spawn_stagger_ms: None,
        };

        let json = serde_json::to_string(&plan).unwrap();
        // max_parallel and spawn_stagger_ms should be omitted when None due to skip_serializing_if
        assert!(!json.contains("max_parallel"));
        assert!(!json.contains("spawn_stagger_ms"));
    }
}
