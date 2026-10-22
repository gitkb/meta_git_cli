use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

/// Run a Git fixture command and surface diagnostics on failure.
fn git(path: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Invoke the public plugin protocol with isolated worktree and data roots.
fn request(root: &Path, command: &str, recursive: bool, explicit: bool) -> std::process::Output {
    let mut args = if matches!(command, "status" | "members") {
        vec!["example"]
    } else {
        vec![]
    };
    if explicit {
        args.push("--recursive");
    }
    let request = json!({"command": format!("git worktree {command}"), "args": args,
        "cwd": root, "options": {"json_output": true, "recursive": recursive}});
    let mut executable = Command::new(env!("CARGO_BIN_EXE_meta-git"));
    if root.join("bin").is_dir() {
        executable.env("PATH", root.join("bin"));
    }
    let mut child = executable
        .arg("--meta-plugin-exec")
        .current_dir(root)
        .env("META_WORKTREES", root.join("sets"))
        .env("META_DATA_DIR", root.join("data"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
/// Verify real nested members, option forwarding and command-specific error output.
fn public_list_status_recursive_protocol_and_explicit_flags() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let set = root.join("sets").join("example");
    for (index, alias) in [".", "open-source", "open-source/atc", "other/atc"]
        .iter()
        .enumerate()
    {
        let source = root.join(format!("source{index}"));
        fs::create_dir(&source).unwrap();
        git(&source, &["init", "-q"]);
        git(
            &source,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-qm",
                "initial",
            ],
        );
        let destination = set.join(alias);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        git(
            &source,
            &[
                "worktree",
                "add",
                "-qb",
                "topic",
                destination.to_str().unwrap(),
            ],
        );
    }
    fs::write(set.join("open-source/atc/untracked.txt"), "dirty").unwrap();
    for command in ["list", "status"] {
        for (recursive, explicit) in [(false, false), (true, false), (false, true), (true, true)] {
            let out = request(root, command, recursive, explicit);
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let value: Value = serde_json::from_slice(&out.stdout).unwrap();
            let rows = if command == "list" {
                &value["worktrees"][0]["repos"]
            } else {
                &value["repos"]
            };
            let aliases: Vec<_> = rows
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["alias"].as_str().unwrap().replace('\\', "/"))
                .collect();
            if recursive || explicit {
                assert_eq!(
                    aliases,
                    [".", "open-source", "open-source/atc", "other/atc"]
                );
                let nested = rows
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| {
                        row["alias"].as_str().unwrap().replace('\\', "/") == "open-source/atc"
                    })
                    .unwrap();
                assert_eq!(nested["branch"], "topic");
                assert_eq!(nested["dirty"], true);
            } else {
                assert_eq!(aliases, [".", "open-source", "other/atc"]);
            }
        }
    }
    fs::write(set.join("open-source/atc/.git"), "broken").unwrap();
    let healthy_set = root.join("sets").join("healthy");
    git(
        &root.join("source0"),
        &[
            "worktree",
            "add",
            "-qb",
            "healthy",
            healthy_set.to_str().unwrap(),
        ],
    );
    let out = request(root, "list", true, false);
    assert!(out.status.success());
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    let worktrees = value["worktrees"].as_array().unwrap();
    assert_eq!(worktrees.len(), 1);
    assert_eq!(worktrees[0]["name"], "healthy");
    assert_eq!(worktrees[0]["root"], healthy_set.to_str().unwrap());
    assert_eq!(
        worktrees[0]["repos"],
        json!([{"alias": ".", "branch": "healthy", "dirty": false}])
    );
    assert_eq!(value["warnings"].as_array().unwrap().len(), 1);
    assert_eq!(value["warnings"][0]["name"], "example");
    assert_eq!(value["warnings"][0]["root"], set.to_str().unwrap());
    assert!(value["warnings"][0]["message"]
        .as_str()
        .unwrap()
        .contains("Invalid .git file format"));

    let out = request(root, "status", true, false);
    assert!(!out.status.success());
    assert!(
        out.stdout.is_empty(),
        "named status must not emit an incomplete successful inventory"
    );
}

/// A missing configured root is empty for list and an error for named status.
#[test]
fn missing_worktree_root_preserves_list_and_status_contracts() {
    let temp = tempfile::tempdir().unwrap();
    for recursive in [false, true] {
        let out = request(temp.path(), "list", recursive, false);
        assert!(out.status.success());
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value, json!({"worktrees": []}));
        let out = request(temp.path(), "status", recursive, false);
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn membership_only_protocol_runs_without_git_and_rejects_partial_membership() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let source = root.join("source");
    fs::create_dir(&source).unwrap();
    git(&source, &["init", "-q"]);
    git(
        &source,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-qm",
            "initial",
        ],
    );
    let set = root.join("sets/example");
    fs::create_dir_all(set.parent().unwrap()).unwrap();
    git(
        &source,
        &["worktree", "add", "-qb", "topic", set.to_str().unwrap()],
    );
    fs::write(set.join("untracked"), "dirty, deliberately unobserved").unwrap();
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    let fake_git = bin.join("git");
    fs::write(
        &fake_git,
        "#!/bin/sh\nprintf called >> \"$0.calls\"\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o700)).unwrap();
    for (recursive, explicit) in [(true, false), (false, true)] {
        let out = request(&root, "members", recursive, explicit);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value["name"], "example");
        assert_eq!(value["repos"].as_array().unwrap().len(), 1);
        assert_eq!(value["repos"][0]["alias"], ".");
        assert_eq!(value["repos"][0]["path"], set.to_str().unwrap());
        assert!(value["repos"][0].get("branch").is_none());
        assert!(value["repos"][0].get("dirty").is_none());
    }
    assert!(!bin.join("git.calls").exists(), "membership launched Git");
    fs::create_dir(set.join("broken")).unwrap();
    fs::write(set.join("broken/.git"), "broken").unwrap();
    let out = request(&root, "members", true, false);
    assert!(!out.status.success());
    assert!(
        out.stdout.is_empty(),
        "must not report a partial successful inventory"
    );
    assert!(!bin.join("git.calls").exists());
}

/// Inference selects the enclosing physical set, including from deep descendants,
/// without consulting a different configured worktree root or launching Git.
#[cfg(unix)]
#[test]
fn membership_infers_enclosing_set_without_git_and_requires_context() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let source = root.join("source");
    fs::create_dir(&source).unwrap();
    git(&source, &["init", "-q"]);
    git(
        &source,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-qm",
            "initial",
        ],
    );
    let set = root.join(".worktrees/example");
    fs::create_dir_all(set.parent().unwrap()).unwrap();
    git(
        &source,
        &["worktree", "add", "-qb", "topic", set.to_str().unwrap()],
    );
    let nested = set.join("deep/directory");
    fs::create_dir_all(&nested).unwrap();
    let other = root.join("configured/other");
    fs::create_dir_all(other.parent().unwrap()).unwrap();
    git(
        &source,
        &["worktree", "add", "-qb", "other", other.to_str().unwrap()],
    );
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    let fake_git = bin.join("git");
    fs::write(
        &fake_git,
        "#!/bin/sh\nprintf called >> \"$0.calls\"\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o700)).unwrap();
    let invoke = |cwd: &Path, args: Vec<&str>, recursive: bool| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_meta-git"))
            .arg("--meta-plugin-exec")
            .current_dir(cwd)
            .env("PATH", &bin)
            .env("META_WORKTREES", root.join("configured"))
            .env("META_DATA_DIR", root.join("data"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let request = json!({"command": "git worktree members", "args": args,
            "cwd": cwd, "options": {"json_output": true, "recursive": recursive}});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    for cwd in [&set, &nested] {
        for (args, recursive) in [(vec![], true), (vec!["--recursive"], false)] {
            let out = invoke(cwd, args, recursive);
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let value: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(value["name"], "example");
            assert_eq!(value["repos"][0]["path"], set.to_str().unwrap());
        }
    }
    let explicit = invoke(&nested, vec!["other"], true);
    assert!(explicit.status.success());
    let value: Value = serde_json::from_slice(&explicit.stdout).unwrap();
    assert_eq!(value["name"], "other");
    assert_eq!(value["repos"][0]["path"], other.to_str().unwrap());

    let outside = invoke(&source, vec![], true);
    assert!(!outside.status.success());
    assert!(outside.stdout.is_empty());
    assert!(String::from_utf8_lossy(&outside.stderr).contains("Worktree name required"));
    assert!(
        !invoke(&set, vec![], false).status.success(),
        "recursive remains required"
    );

    fs::create_dir(set.join("broken")).unwrap();
    fs::write(set.join("broken/.git"), "broken").unwrap();
    let malformed = invoke(&nested, vec![], true);
    assert!(!malformed.status.success());
    assert!(
        malformed.stdout.is_empty(),
        "inference must not mask invalid membership"
    );
    assert!(!bin.join("git.calls").exists(), "inference launched Git");
}
