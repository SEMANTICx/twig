//! OS-owned lock releases on crash; two daemons cannot append one session.
use std::fs::{File,OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;
pub struct Lease(File);
impl Lease {
    pub fn acquire(path:&Path)->std::io::Result<Self>{
        if let Some(p)=path.parent(){std::fs::create_dir_all(p)?;}
        let f=OpenOptions::new().create(true).append(true).open(path)?;
        if unsafe{libc::flock(f.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}!=0 {
            return Err(std::io::Error::other("session is already owned by another process"));
        }
        Ok(Self(f))
    }
}
impl Drop for Lease {fn drop(&mut self){unsafe{libc::flock(self.0.as_raw_fd(),libc::LOCK_UN);}}}
