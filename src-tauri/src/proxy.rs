use crate::i18n::{tr, trf};
use crate::store::{in_sandbox, Paths};
use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// lobster_proxy.py 及配置模板在编译期内嵌，运行时释放到用户库
/// （~/.lobster-plus/proxy/）。config.json 只在缺失时落盘，绝不覆盖
/// 用户手动改过的配置（模型映射等低频配置直接改文件 + 重启代理）。
const PROXY_SCRIPT: &str = include_str!("../proxy/lobster_proxy.py");
const PROXY_CONFIG_TEMPLATE: &str = include_str!("../proxy/config.template.json");

pub const DEFAULT_PORT: u16 = 19260;
const HEALTH_TIMEOUT: Duration = Duration::from_millis(1500);
const START_WAIT: Duration = Duration::from_secs(5);
const STOP_WAIT: Duration = Duration::from_secs(10);

/// 释放目录里脚本的版本标记：编译期内嵌脚本变化（sha256 前 8 位）时自动覆盖更新。
const SCRIPT_TAG: &str = "released-by-lobsterplus";

#[derive(Serialize, Clone, Debug)]
pub struct ProxyStatus {
    pub running: bool,
    pub port: u16,
    /// true = 端口上有 /health 响应，但返回的不是本代理
    pub foreign: bool,
    /// 上游（LobsterAI 本地模型代理）当前是否探测成功
    pub upstream_alive: Option<bool>,
    pub upstream: Option<String>,
}

/// 释放内嵌脚本与默认配置。脚本带版本标记随程序更新；config.json 只在缺失时写。
pub fn ensure_released(paths: &Paths) -> Result<PathBuf, String> {
    let dir = paths.proxy_dir();
    fs::create_dir_all(&dir).map_err(|e| trf("err.mkdir", &[("e", &e.to_string())]))?;
    let script = paths.proxy_script();
    let tag_file = dir.join(".released-tag");
    let tag = &format!("{SCRIPT_TAG}:{}", script_tag());
    let need_update = fs::read_to_string(&tag_file).ok().as_deref() != Some(tag);
    if need_update {
        fs::write(&script, PROXY_SCRIPT)
            .map_err(|e| trf("err.write_file", &[("path", &script.display().to_string()), ("e", &e.to_string())]))?;
        let _ = fs::write(&tag_file, tag);
    }
    let cfg = paths.proxy_config();
    if !cfg.exists() {
        fs::write(&cfg, PROXY_CONFIG_TEMPLATE)
            .map_err(|e| trf("err.write_file", &[("path", &cfg.display().to_string()), ("e", &e.to_string())]))?;
    }
    Ok(dir)
}

fn script_tag() -> String {
    sha2_short(PROXY_SCRIPT.as_bytes())
}

fn sha2_short(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    let d = h.finalize();
    d.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// 读取释放目录 config.json 的 listen_port（缺失/损坏回退默认端口）。
pub fn listen_port(paths: &Paths) -> u16 {
    let cfg = paths.proxy_config();
    let port = fs::read_to_string(&cfg)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("listen_port").and_then(|p| p.as_u64()));
    match port {
        Some(p) if (1..=65535).contains(&p) => p as u16,
        _ => DEFAULT_PORT,
    }
}

/// 读取 config.json 的 api_key（代理鉴权，注册 CC-Switch 时带上）。
pub fn api_key(paths: &Paths) -> Option<String> {
    let cfg = paths.proxy_config();
    let raw = fs::read_to_string(&cfg).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let key = v.get("api_key").and_then(|p| p.as_str())?.trim().to_string();
    (!key.is_empty()).then_some(key)
}

/// GET /health 探测。running=true 要求返回 JSON 且 status=="ok"。
pub fn health(paths: &Paths) -> Result<ProxyStatus, String> {
    if in_sandbox() {
        return Ok(ProxyStatus { running: false, port: DEFAULT_PORT, foreign: false, upstream_alive: None, upstream: None });
    }
    let port = listen_port(paths);
    let url = format!("http://127.0.0.1:{port}/health");
    let resp = ureq::get(&url).timeout(HEALTH_TIMEOUT).call();
    let Some(resp) = resp.ok() else {
        return Ok(ProxyStatus { running: false, port, foreign: false, upstream_alive: None, upstream: None });
    };
    let body = resp.into_string().unwrap_or_default();
    match serde_json::from_str::<Value>(&body) {
        Ok(v) if v.get("status").and_then(|s| s.as_str()) == Some("ok") => {
            let upstream_alive = v.get("upstream_alive").and_then(|x| x.as_bool());
            let upstream = v
                .get("upstream")
                .and_then(|x| x.as_str())
                .map(String::from);
            Ok(ProxyStatus { running: true, port, foreign: false, upstream_alive, upstream })
        }
        _ => Ok(ProxyStatus { running: false, port, foreign: true, upstream_alive: None, upstream: None }),
    }
}

/// 三级 Python 探测：
/// PATH python.exe → LobsterAI 自带 Python（runtimes\python-win）→ py launcher
pub fn find_python() -> Option<PathBuf> {
    if let Some(p) = find_in_path("python.exe") {
        return Some(p);
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        let runtime = PathBuf::from(&appdata)
            .join("LobsterAI")
            .join("runtimes")
            .join("python-win");
        // 目录下可能有版本子目录或直接放 python.exe
        if runtime.join("python.exe").is_file() {
            return Some(runtime.join("python.exe"));
        }
        if let Ok(rd) = fs::read_dir(&runtime) {
            for e in rd.flatten() {
                let cand = e.path().join("python.exe");
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    find_in_path("py.exe")
}

fn find_in_path(exe: &str) -> Option<PathBuf> {
    let path_var = std::env::var("PATH").ok()?;
    for dir in path_var.split(';').filter(|d| !d.is_empty()) {
        let cand = Path::new(dir).join(exe);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// 启动代理。幂等：/health 已通则直接返回已在运行。
pub fn start(paths: &Paths) -> Result<ProxyStatus, String> {
    if in_sandbox() {
        return Err(tr("err.proxy.sandbox"));
    }
    let st = health(paths)?;
    if st.running {
        return Ok(st);
    }
    let dir = ensure_released(paths)?;
    let script = dir.join("lobster_proxy.py");
    let Some(py) = find_python() else {
        return Err(tr("err.proxy.no_python"));
    };
    let log = paths.proxy_log();
    let log_file = fs::File::create(&log)
        .map_err(|e| trf("err.write_file", &[("path", &log.display().to_string()), ("e", &e.to_string())]))?;

    let mut cmd = std::process::Command::new(&py);
    cmd.args([script.as_os_str()]);
    cmd.current_dir(&dir);
    // 三个标准流全部显式重定向：继承父进程管道会让「调用方（CLI/python 测试）
    // 的 subprocess 挂到代理进程退出」——CLI proxy start 永不返回的根因。
    cmd.stdout(std::process::Stdio::from(log_file));
    cmd.stderr({
        let e = fs::OpenOptions::new().append(true).open(&log)
            .or_else(|_| fs::File::create(&log));
        match e {
            Ok(f) => std::process::Stdio::from(f),
            Err(_) => std::process::Stdio::null(),
        }
    });
    cmd.stdin(std::process::Stdio::null());
    detach(&mut cmd);
    cmd.spawn()
        .map_err(|e| trf("err.proxy.launch", &[("e", &e.to_string())]))?;

    let deadline = Instant::now() + START_WAIT;
    loop {
        let st = health(paths)?;
        if st.running {
            return Ok(st);
        }
        if Instant::now() >= deadline {
            let tail = log_tail(paths, 15);
            let extra = if tail.is_empty() { String::new() } else { format!("\n{}", trf("err.proxy.log_tail", &[("log", &tail)])) };
            return Err(tr("err.proxy.start_timeout") + &extra);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// 停止代理：杀任何命令行含 lobster_proxy.py 的 python 进程，
/// 扑空但 /health 仍通时按端口找 LISTENING PID 兜底。
pub fn stop(paths: &Paths) -> Result<bool, String> {
    if in_sandbox() {
        return Err(tr("err.proxy.sandbox"));
    }
    let st = health(paths)?;
    if !st.running {
        return Ok(true);
    }
    let port = st.port;
    {
        let mut sys = sysinfo::System::new();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        for (_pid, p) in sys.processes() {
            let cmd = p.cmd().iter().map(|s| s.to_string_lossy()).collect::<Vec<_>>().join(" ");
            if crate::guard::is_proxy_cmdline(&cmd) {
                let _ = p.kill();
            }
        }
    }
    let brief = Instant::now() + Duration::from_millis(800);
    while Instant::now() < brief {
        if !health(paths)?.running {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if health(paths)?.running {
        if let Some(pid) = listener_pid(port) {
            let mut sys = sysinfo::System::new();
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            if let Some(p) = sys.process(sysinfo::Pid::from_u32(pid)) {
                let _ = p.kill();
            }
        }
    }
    let deadline = Instant::now() + STOP_WAIT;
    loop {
        if !health(paths)?.running {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// 读 proxy.log 尾部。
pub fn log_tail(paths: &Paths, lines: usize) -> String {
    let f = paths.proxy_log();
    if !f.exists() {
        return String::new();
    }
    let mut buf = vec![0u8; 64 * 1024];
    let Ok(mut fh) = fs::File::open(&f) else {
        return String::new();
    };
    let size = fh.metadata().map(|m| m.len()).unwrap_or(0);
    let read_len = size.min(buf.len() as u64) as usize;
    if read_len == 0 {
        return String::new();
    }
    use std::io::Seek;
    let start = size - read_len as u64;
    if fh.seek(std::io::SeekFrom::Start(start)).is_err() {
        return String::from_utf8_lossy(&buf).to_string();
    }
    if fh.read_exact(&mut buf[..read_len]).is_err() {
        return String::from_utf8_lossy(&buf).to_string();
    }
    let text = String::from_utf8_lossy(&buf[..read_len]).to_string();
    let tail: Vec<&str> = text.lines().rev().take(lines).collect::<Vec<_>>().into_iter().rev().collect();
    tail.join("\n")
}

#[cfg(windows)]
fn detach(c: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    c.creation_flags(0x0000_0008 | 0x0000_0200); // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
}

#[cfg(not(windows))]
fn detach(c: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    c.process_group(0);
}

/// 找监听 127.0.0.1:port 的 PID（netstat 解析）。
fn listener_pid(port: u16) -> Option<u32> {
    let out = std::process::Command::new("netstat")
        .args(["-ano", "-p", "TCP"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let needle = format!(":{port}");
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 5 && cols[0].eq_ignore_ascii_case("TCP") && cols[3].eq_ignore_ascii_case("LISTENING") {
            if cols[1].ends_with(&needle) {
                return cols[4].parse().ok();
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_files_present() {
        assert!(PROXY_SCRIPT.contains("ThreadingHTTPServer"), "内嵌脚本损坏");
        assert!(PROXY_SCRIPT.contains("UpstreamManager"), "稳定器缺失");
        assert!(PROXY_CONFIG_TEMPLATE.contains("listen_port"), "内嵌配置模板损坏");
    }

    #[test]
    fn script_tag_changes_with_content() {
        assert_eq!(sha2_short(b"abc"), sha2_short(b"abc"));
        assert_ne!(sha2_short(b"abc"), sha2_short(b"abd"));
    }
}
