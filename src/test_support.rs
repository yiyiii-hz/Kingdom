use std::ffi::OsString;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

pub fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

pub struct PathGuard {
    old_path: Option<OsString>,
}

impl PathGuard {
    pub fn prepend(dir: &Path) -> Self {
        let old_path = std::env::var_os("PATH");
        let mut next = OsString::from(dir.as_os_str());
        if let Some(old) = &old_path {
            next.push(OsString::from(":"));
            next.push(old);
        }
        std::env::set_var("PATH", &next);
        Self { old_path }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        if let Some(old_path) = &self.old_path {
            std::env::set_var("PATH", old_path);
        } else {
            std::env::remove_var("PATH");
        }
    }
}
