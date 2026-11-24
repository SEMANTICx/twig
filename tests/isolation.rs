use std::time::Duration;
#[test]
fn sandbox_writes_workspace_but_not_sibling_host_files(){
 let root=std::env::temp_dir().join(format!("twig-isolation-{}",std::process::id()));
 let workspace=root.join("repo");std::fs::create_dir_all(&workspace).unwrap();
 let secret=root.join("host-secret");std::fs::write(&secret,"unchanged").unwrap();
 let command=format!("printf ok > inside; test ! -e '{}'",secret.display());
 let wrapped=twig::isolation::shell(&workspace,&command,false,false).unwrap();
 let result=twig::process::run(&workspace,&wrapped,Duration::from_secs(5)).unwrap();
 assert_eq!(result.exit_code,Some(0),"{}",result.text);
 assert_eq!(std::fs::read_to_string(workspace.join("inside")).unwrap(),"ok");
 assert_eq!(std::fs::read_to_string(secret).unwrap(),"unchanged");
}
