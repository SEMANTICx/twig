//! OS isolation is established by bwrap, never by guessing shell strings.
//!
//! Covers both single-command tool sandboxing and allowlist evaluation sandboxes
//! that hide host paths, credentials, checks and source prompts from the solver.

use std::path::Path;
use std::process::Command;

pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

pub fn shell(
    workspace: &Path,
    command: &str,
    network: bool,
    read_only: bool,
) -> Result<String, String> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (workspace, command, network, read_only);
        return Err("bwrap OS isolation requires Linux (disable `isolation` in harness.json on macOS/Windows)".into());
    }
    #[cfg(target_os = "linux")]
    {
    let workspace = workspace.canonicalize().map_err(|e| e.to_string())?;
    let mut args = vec![
        "bwrap".to_string(),
        "--die-with-parent".into(),
        "--new-session".into(),
        "--unshare-pid".into(),
        "--unshare-ipc".into(),
        "--unshare-uts".into(),
        "--clearenv".into(),
    ];
    if !network {
        args.push("--unshare-net".into());
    }
    for path in [
        "/usr",
        "/bin",
        "/lib",
        "/lib64",
        "/etc/ssl",
        "/etc/alternatives",
        "/etc/resolv.conf",
        "/etc/hosts",
    ] {
        if Path::new(path).exists() {
            args.extend(["--ro-bind".into(), path.into(), path.into()]);
        }
    }
    args.extend([
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        "--tmpfs".into(),
        "/tmp".into(),
    ]);
    args.extend([
        if read_only { "--ro-bind" } else { "--bind" }.into(),
        workspace.display().to_string(),
        workspace.display().to_string(),
        "--chdir".into(),
        workspace.display().to_string(),
        "--setenv".into(),
        "HOME".into(),
        "/tmp".into(),
        "--setenv".into(),
        "PATH".into(),
        "/usr/local/bin:/usr/bin:/bin".into(),
        "--".into(),
        "sh".into(),
        "-c".into(),
        command.into(),
    ]);
    Ok(args
        .iter()
        .map(|a| quote(a))
        .collect::<Vec<_>>()
        .join(" "))
    }
}

/// Cached probe returning `true` when `bwrap` user namespaces are available in the
/// current Linux kernel/container environment.
pub fn bwrap_userns_usable() -> bool {
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
    #[cfg(target_os = "linux")]
    {
        static USABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *USABLE.get_or_init(|| {
            Command::new("bwrap")
                .args(["--unshare-user", "--ro-bind", "/", "/", "--", "/bin/true"])
                .output()
                .is_ok_and(|o| o.status.success())
        })
    }
}

/// Evaluation sandbox with an allowlist filesystem: never mounts `/home`,
/// checks, or source task metadata. Caller supplies explicit `(key, value)` env pairs.
pub fn eval_command(
    twig: &Path,
    config: &Path,
    workspace: &Path,
    runs: &Path,
    envs: &[(&str, &str)],
) -> Result<Command, String> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (twig, config, workspace, runs, envs);
        return Err("bwrap evaluation sandbox requires Linux".into());
    }
    #[cfg(target_os = "linux")]
    {
    let absolute = |path: &Path| {
        path.canonicalize()
            .map_err(|e| format!("sandbox {}: {e}", path.display()))
    };
    let twig = absolute(twig)?;
    let config = absolute(config)?;
    let workspace = absolute(workspace)?;
    let runs = absolute(runs)?;
    let mut cmd = Command::new("bwrap");
    cmd.env_clear().args([
        "--die-with-parent",
        "--new-session",
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--share-net",
        "--clearenv",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
        "--dir",
        "/home",
        "--dir",
        "/home/agent",
        "--dir",
        "/etc",
    ]);
    for path in ["/usr", "/bin", "/sbin", "/lib", "/lib64"] {
        if Path::new(path).exists() {
            cmd.args(["--ro-bind", path, path]);
        }
    }
    for path in [
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/ssl",
        "/etc/ld.so.cache",
    ] {
        if Path::new(path).exists() {
            cmd.args(["--ro-bind", path, path]);
        }
    }
    cmd.arg("--ro-bind")
        .arg(twig)
        .arg("/twig")
        .arg("--ro-bind")
        .arg(config)
        .arg("/harness.json")
        .arg("--bind")
        .arg(workspace)
        .arg("/workspace")
        .arg("--bind")
        .arg(runs)
        .arg("/runs")
        .args([
            "--chdir",
            "/workspace",
            "--setenv",
            "HOME",
            "/home/agent",
            "--setenv",
            "PATH",
            "/usr/bin:/bin",
            "--setenv",
            "TWIG_STATE_DIR",
            "/runs/state",
            "--setenv",
            "TWIG_CONFIG",
            "/harness.json",
        ]);
    for (name, value) in envs {
        cmd.args(["--setenv", name, value]);
    }
    cmd.args(["--", "/twig", "serve", "/workspace", "/runs"]);
    Ok(cmd)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn sandbox_does_not_mount_corpus_or_inherit_environment() {
        let root = std::env::temp_dir().join(format!("twig-sandbox-{}", std::process::id()));
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        std::fs::create_dir_all(root.join("runs")).unwrap();
        std::fs::write(root.join("config"), "{}").unwrap();
        let cmd = eval_command(
            Path::new("/bin/true"),
            &root.join("config"),
            &root.join("workspace"),
            &root.join("runs"),
            &[("TWIG_MODEL", "stub")],
        )
        .unwrap();
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--clearenv".to_string()));
        assert!(!args.windows(3).any(|a| a[0] == "--ro-bind" && a[1] == "/"));
        assert!(args.contains(&"--unshare-pid".to_string()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn solver_cannot_read_checks_or_source_prompts_outside_workspace() {
        if !bwrap_userns_usable() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("twig-hidden-{}", std::process::id()));
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        std::fs::create_dir_all(root.join("runs")).unwrap();
        std::fs::write(root.join("config"), "{}").unwrap();
        std::fs::write(root.join("check.sh"), "secret judge").unwrap();
        std::fs::write(root.join("task.json"), "secret source prompt").unwrap();
        let executable = root.join("solver");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nset -eu\ntest ! -e '{}/check.sh'\ntest ! -e '{}/task.json'\ntest ! -e /proc/1/root{}/check.sh\ntest -d /workspace\necho isolated\n",
                root.display(),
                root.display(),
                root.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = eval_command(
            &executable,
            &root.join("config"),
            &root.join("workspace"),
            &root.join("runs"),
            &[],
        )
        .unwrap()
        .output()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "isolated");
        std::fs::remove_dir_all(root).unwrap();
    }
}
