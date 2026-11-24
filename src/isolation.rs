//! OS isolation is established by bwrap, not a list of guessed shell strings.
use std::path::Path;

pub fn quote(text: &str) -> String { format!("'{}'", text.replace('\'', "'\\''")) }
pub fn shell(workspace: &Path, command: &str, network: bool, read_only: bool) -> Result<String,String> {
    let workspace=workspace.canonicalize().map_err(|e|e.to_string())?;
    let mut args=vec!["bwrap".to_string(),"--die-with-parent".into(),"--new-session".into(),"--unshare-pid".into(),
        "--unshare-ipc".into(),"--unshare-uts".into(),"--clearenv".into()];
    if !network {args.push("--unshare-net".into());}
    for path in ["/usr","/bin","/lib","/lib64","/etc/ssl","/etc/alternatives","/etc/resolv.conf","/etc/hosts"] {
        if Path::new(path).exists() { args.extend(["--ro-bind".into(),path.into(),path.into()]); }
    }
    args.extend(["--proc".into(),"/proc".into(),"--dev".into(),"/dev".into(),"--tmpfs".into(),"/tmp".into()]);
    args.extend([if read_only {"--ro-bind"}else{"--bind"}.into(),workspace.display().to_string(),workspace.display().to_string(),
        "--chdir".into(),workspace.display().to_string(),"--setenv".into(),"HOME".into(),"/tmp".into(),
        "--setenv".into(),"PATH".into(),"/usr/local/bin:/usr/bin:/bin".into(),"--".into(),"sh".into(),"-c".into(),command.into()]);
    Ok(args.iter().map(|a|quote(a)).collect::<Vec<_>>().join(" "))
}
