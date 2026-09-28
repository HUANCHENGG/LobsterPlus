use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 跨进程凭证锁（家族约定）：
/// 在数据目录侧创建 `<marker>.lobster-plus.lock`（O_CREAT|O_EXCL），
/// 内容为 pid，超过 stale_after 视为陈旧锁可清理。
/// 切换期间持锁，避免代理/签到与切换动作并发操作同一份数据。
pub struct ProxyLock {
    path: PathBuf,
    held: bool,
}

impl ProxyLock {
    pub fn path_of(marker: &Path) -> PathBuf {
        let mut s = marker.as_os_str().to_os_string();
        s.push(".lobster-plus.lock");
        PathBuf::from(s)
    }

    pub fn acquire(marker: &Path, timeout: Duration, stale_after: Duration) -> Result<ProxyLock, String> {
        let path = Self::path_of(marker);
        let deadline = Instant::now() + timeout;
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    let _ = f.write_all(std::process::id().to_string().as_bytes());
                    return Ok(ProxyLock { path, held: true });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // 清理崩溃进程遗留的陈旧锁
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .map(|age| age > stale_after)
                        .unwrap_or(false);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if Instant::now() >= deadline {
                        return Err(format!("lock busy: {}", path.display()));
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(e) => return Err(e.to_string()),
            }
        }
    }

    fn drop_impl(&mut self) {
        if self.held {
            self.held = false;
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl Drop for ProxyLock {
    fn drop(&mut self) {
        self.drop_impl();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_path_and_relock() {
        let dir = std::env::temp_dir().join(format!("lp-lock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("sqlite.db");
        std::fs::write(&marker, b"x").unwrap();
        {
            let _l = ProxyLock::acquire(&marker, Duration::from_secs(1), Duration::from_secs(1)).unwrap();
            assert!(ProxyLock::path_of(&marker).exists());
        }
        // 释放后可重新获取
        let _ = ProxyLock::acquire(&marker, Duration::from_secs(1), Duration::from_secs(1)).unwrap();
        assert!(!ProxyLock::path_of(&marker).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
