//! Evaluation has an allowlist filesystem, never a read-only view of the host.
use std::path::Path;
use std::process::Command;

pub(super) fn command(twig: &Path, config: &Path, workspace: &Path, runs: &Path) -> Result<Command, String> {
    let absolute = |path: &Path| path.canonicalize().map_err(|e| format!("sandbox {}: {e}", path.display()));
    let twig = absolute(twig)?;
    let config = absolute(config)?;
    let workspace = absolute(workspace)?;
    let runs = absolute(runs)?;
    let mut cmd = Command::new("bwrap");
    cmd.env_clear().args(["--die-with-parent", "--new-session", "--unshare-user", "--unshare-pid", "--unshare-ipc", "--unshare-uts", "--share-net",
        "--clearenv", "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--dir", "/home",
        "--dir", "/home/agent", "--dir", "/etc"]);
    // System runtimes only. No /home, /run, host /tmp, source checkout or checks.
    for path in ["/usr", "/bin", "/sbin", "/lib", "/lib64"] {
        if Path::new(path).exists() { cmd.args(["--ro-bind", path, path]); }
    }
    for path in ["/etc/resolv.conf", "/etc/hosts", "/etc/ssl", "/etc/ld.so.cache"] {
        if Path::new(path).exists() { cmd.args(["--ro-bind", path, path]); }
    }
    cmd.arg("--ro-bind").arg(twig).arg("/twig")
        .arg("--ro-bind").arg(config).arg("/harness.json")
        .arg("--bind").arg(workspace).arg("/workspace")
        .arg("--bind").arg(runs).arg("/runs")
        .args(["--chdir", "/workspace", "--setenv", "HOME", "/home/agent", "--setenv", "PATH", "/usr/bin:/bin",
            "--setenv", "TWIG_STATE_DIR", "/runs/state", "--setenv", "TWIG_CONFIG", "/harness.json"]);
    // Network is explicitly shared for the model gateway. No credential files or
    // ambient environment are inherited; only gateway-specific settings cross.
    for name in ["TWIG_BASE_URL", "TWIG_API_KEY", "TWIG_ROLES"] {
        if let Ok(value) = std::env::var(name) { cmd.args(["--setenv", name, &value]); }
    }
    cmd.args(["--", "/twig", "serve", "/workspace", "/runs"]);
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sandbox_does_not_mount_corpus_or_inherit_environment() {
        let root = std::env::temp_dir().join(format!("twig-sandbox-{}", std::process::id()));
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        std::fs::create_dir_all(root.join("runs")).unwrap();
        std::fs::write(root.join("config"), "{}").unwrap();
        let cmd = command(Path::new("/bin/true"), &root.join("config"), &root.join("workspace"), &root.join("runs")).unwrap();
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(args.contains(&"--clearenv".to_string()));
        assert!(!args.windows(3).any(|a| a[0] == "--ro-bind" && a[1] == "/"));
        assert!(args.contains(&"--unshare-pid".to_string()));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn solver_cannot_read_checks_or_source_prompts_outside_workspace() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("twig-hidden-{}", std::process::id()));
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        std::fs::create_dir_all(root.join("runs")).unwrap();
        std::fs::write(root.join("config"), "{}").unwrap();
        std::fs::write(root.join("check.sh"), "secret judge").unwrap();
        std::fs::write(root.join("task.json"), "secret source prompt").unwrap();
        let executable = root.join("solver");
        std::fs::write(&executable, format!("#!/bin/sh\nset -eu\ntest ! -e '{}/check.sh'\ntest ! -e '{}/task.json'\ntest ! -e /proc/1/root{}/check.sh\ntest -d /workspace\necho isolated\n", root.display(), root.display(), root.display())).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = command(&executable, &root.join("config"), &root.join("workspace"), &root.join("runs")).unwrap().output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "isolated");
        std::fs::remove_dir_all(root).unwrap();
    }
}
