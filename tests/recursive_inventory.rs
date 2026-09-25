use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

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

fn request(root: &Path, command: &str, recursive: bool, explicit: bool) -> std::process::Output {
    let mut args = if command == "status" {
        vec!["example"]
    } else {
        vec![]
    };
    if explicit {
        args.push("--recursive");
    }
    let request = json!({"command": format!("git worktree {command}"), "args": args,
        "cwd": root, "options": {"json_output": true, "recursive": recursive}});
    let mut child = Command::new(env!("CARGO_BIN_EXE_meta-git"))
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
fn public_list_status_recursive_protocol_and_explicit_flags() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let set = root.join("sets/example");
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
    for command in ["list", "status"] {
        let out = request(root, command, true, false);
        assert!(!out.status.success());
        assert!(
            out.stdout.is_empty(),
            "must not emit a partial successful inventory"
        );
    }
}
