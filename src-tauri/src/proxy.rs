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

#[derive(Serialize, Clone, Debug, Default)]
pub struct ProxyStatus {
    pub running: bool,
    pub port: u16,
    /// true = 端口上有 /health 响应，但返回的不是本代理
    pub foreign: bool,
    /// 上游（LobsterAI 本地模型代理）当前是否探测成功
    pub upstream_alive: Option<bool>,
    pub upstream: Option<String>,
    /// 正在运行的代理进程报告的脚本版本（/health 的 script_version）
    pub script_version: Option<String>,
    /// 本程序内嵌的脚本版本（应为 CARGO_PKG_VERSION）
    pub expected_version: Option<String>,
    /// 运行中的是旧脚本：需要重启代理才能生效
    pub stale: bool,
    /// 代理当前能提供的模型数（/health 的 models 长度），用于自检清单是否完整
    pub model_count: Option<usize>,
    /// 账号风控状态（/health 的 account.state）：OK / TEMP_RESTRICTED / ...
    pub account_state: Option<String>,
    /// 账号被限制时的解禁时间（人类可读，来自 /health restricted_until_text）
    pub account_restricted_until: Option<String>,
    /// 账号被限制时的原始说明（不含凭据）
    pub account_message: Option<String>,
    /// 账号当前是否处于限制窗口（/health 的 account_restricted）
    pub account_restricted: bool,
}

/// 代理脚本版本（与 lobster_proxy.py 的 PROXY_SCRIPT_VERSION 保持一致）。
///
/// 升级安装后旧的 Python 进程仍在跑旧脚本，`start()` 原先看到 /health
/// 通就直接返回，**从不重启**——用户升级了也拿不到新行为。v0.3.3 实测：
/// 另一台机器装完新版，CC-Switch 获取模型仍是旧的 5 个。现在靠版本比对
/// 触发自动重启。
pub const SCRIPT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 释放内嵌脚本与默认配置。脚本带版本标记随程序更新；config.json 只在缺失时写。
pub fn ensure_released(paths: &Paths) -> Result<PathBuf, String> {
    let dir = paths.proxy_dir();
    fs::create_dir_all(&dir).map_err(|e| trf("err.mkdir", &[("e", &e.to_string())]))?;
    let script = paths.proxy_script();
    let tag_file = dir.join(".released-tag");
    let tag = &format!("{SCRIPT_TAG}:{}", script_tag());
    let need_update = fs::read_to_string(&tag_file).ok().as_deref() != Some(tag);
    if need_update {
        fs::write(&script, PROXY_SCRIPT).map_err(|e| {
            trf(
                "err.write_file",
                &[
                    ("path", &script.display().to_string()),
                    ("e", &e.to_string()),
                ],
            )
        })?;
        let _ = fs::write(&tag_file, tag);
    }
    let cfg = paths.proxy_config();
    if !cfg.exists() {
        fs::write(&cfg, PROXY_CONFIG_TEMPLATE).map_err(|e| {
            trf(
                "err.write_file",
                &[("path", &cfg.display().to_string()), ("e", &e.to_string())],
            )
        })?;
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
    let key = v
        .get("api_key")
        .and_then(|p| p.as_str())?
        .trim()
        .to_string();
    (!key.is_empty()).then_some(key)
}

/// GET /health 探测。running=true 要求返回 JSON 且 status=="ok"。
pub fn health(paths: &Paths) -> Result<ProxyStatus, String> {
    if in_sandbox() {
        return Ok(ProxyStatus {
            port: DEFAULT_PORT,
            ..Default::default()
        });
    }
    let port = listen_port(paths);
    let url = format!("http://127.0.0.1:{port}/health");
    let resp = ureq::get(&url).timeout(HEALTH_TIMEOUT).call();
    let Some(resp) = resp.ok() else {
        return Ok(ProxyStatus {
            port,
            ..Default::default()
        });
    };
    let body = resp.into_string().unwrap_or_default();
    match serde_json::from_str::<Value>(&body) {
        Ok(v) if v.get("status").and_then(|s| s.as_str()) == Some("ok") => {
            let upstream_alive = v.get("upstream_alive").and_then(|x| x.as_bool());
            let upstream = v.get("upstream").and_then(|x| x.as_str()).map(String::from);
            let script_version = v
                .get("script_version")
                .and_then(|x| x.as_str())
                .map(String::from);
            // 老脚本不返回 script_version，也视为 stale（必须重启换新脚本）
            let stale = script_version.as_deref() != Some(SCRIPT_VERSION);
            let model_count = v
                .get("models")
                .and_then(|m| m.as_array())
                .map(|a| a.len());
            // 账号风控状态：老脚本没有该字段，按 OK 处理
            let account = v.get("account");
            let account_state = account
                .and_then(|a| a.get("state"))
                .and_then(|x| x.as_str())
                .map(String::from);
            let account_restricted_until = account
                .and_then(|a| a.get("restricted_until_text"))
                .and_then(|x| x.as_str())
                .map(String::from);
            let account_message = account
                .and_then(|a| a.get("message"))
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
            let account_restricted = v
                .get("account_restricted")
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            Ok(ProxyStatus {
                running: true,
                port,
                foreign: false,
                upstream_alive,
                upstream,
                script_version,
                expected_version: Some(SCRIPT_VERSION.to_string()),
                stale,
                model_count,
                account_state,
                account_restricted_until,
                account_message,
                account_restricted,
            })
        }
        _ => Ok(ProxyStatus {
            running: false,
            port,
            foreign: true,
            ..Default::default()
        }),
    }
}

/// 三级 Python 探测：
/// PATH python.exe → LobsterAI 自带 Python（runtimes\python-win）→ py launcher
///
/// 每个候选都必须「实际启动」验证：本机 PATH 里的 python.exe/python3.exe 是
/// Windows Store App Execution Alias（重定向到 AppInstallerPythonRedirector.exe），
/// 未从商店装过 Python 时启动即静默退出，退出码 49（ERROR_NOT_FOUND），
/// 无任何 stdout/stderr。只靠文件存在判断会把 alias 当成可用解释器，
/// 代理进程瞬间消失，start() 只能报一句「/health 未就绪」的超时，
/// 而 proxy.log 是空的——问题完全不可诊断。
pub fn find_python() -> Option<PathBuf> {
    for cand in python_candidates() {
        if python_works(&cand) {
            return Some(cand);
        }
    }
    None
}

/// 候选顺序即优先级。同名 python.exe 在不同 PATH 目录下都可能是 alias，
/// 所以 PATH 里出现的每一个 python.exe 都要单独验证。
fn python_candidates() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(';').filter(|d| !d.is_empty()) {
            for exe in ["python.exe", "python3.exe"] {
                push(Path::new(dir).join(exe));
            }
        }
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        let runtime = PathBuf::from(&appdata)
            .join("LobsterAI")
            .join("runtimes")
            .join("python-win");
        // 目录下可能有版本子目录或直接放 python.exe
        if runtime.join("python.exe").is_file() {
            push(runtime.join("python.exe"));
        }
        if let Ok(rd) = fs::read_dir(&runtime) {
            for e in rd.flatten() {
                push(e.path().join("python.exe"));
            }
        }
    }
    // py launcher 不在固定目录，交给 PATH 解析
    push(PathBuf::from("py.exe"));
    out
}

/// 用 `python -c "pass"` 验证候选真的能跑起来（区分真解释器与 Store alias）。
fn python_works(py: &Path) -> bool {
    std::process::Command::new(py)
        .args(["-c", "pass"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 启动代理。幂等：/health 已通且脚本版本一致则直接返回已在运行；
/// 若运行中的是旧脚本（升级安装后旧 Python 进程未退出），先停再起。
pub fn start(paths: &Paths) -> Result<ProxyStatus, String> {
    if in_sandbox() {
        return Err(tr("err.proxy.sandbox"));
    }
    let st = health(paths)?;
    if st.running && !st.stale {
        return Ok(st);
    }
    let stale_running = st.running && st.stale;
    // 先释放内嵌脚本：即使后面不重启，磁盘上的脚本也要是最新的
    let dir = ensure_released(paths)?;
    if stale_running {
        // 旧进程持有旧代码；不重启的话所有修复都不生效
        stop(paths)?;
        std::thread::sleep(Duration::from_millis(300));
    }
    let script = dir.join("lobster_proxy.py");
    let Some(py) = find_python() else {
        return Err(tr("err.proxy.no_python"));
    };
    let log = paths.proxy_log();
    let log_file = fs::File::create(&log).map_err(|e| {
        trf(
            "err.write_file",
            &[("path", &log.display().to_string()), ("e", &e.to_string())],
        )
    })?;

    let mut cmd = std::process::Command::new(&py);
    cmd.args([script.as_os_str()]);
    cmd.current_dir(&dir);
    // 三个标准流全部显式重定向：继承父进程管道会让「调用方（CLI/python 测试）
    // 的 subprocess 挂到代理进程退出」——CLI proxy start 永不返回的根因。
    cmd.stdout(std::process::Stdio::from(log_file));
    cmd.stderr({
        let e = fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .or_else(|_| fs::File::create(&log));
        match e {
            Ok(f) => std::process::Stdio::from(f),
            Err(_) => std::process::Stdio::null(),
        }
    });
    cmd.stdin(std::process::Stdio::null());
    detach(&mut cmd);
    // 必须在 spawn 前临时清掉 CLI 自身 std 句柄的可继承位：Rust std 在 Windows
    // 上用 bInheritHandles=TRUE 启子进程，shell 给 CLI 的管道写端会被长驻的
    // python 一并继承；lobster-plus.exe 退出后 grep/cat 永远等不到 EOF，
    // `proxy start | grep` 就此挂死。spawn 后 guard drop 自动恢复。
    let _inherit_guard = crate::guard::suspend_std_handle_inheritance();
    cmd.spawn()
        .map_err(|e| trf("err.proxy.launch", &[("e", &e.to_string())]))?;
    drop(_inherit_guard);

    let deadline = Instant::now() + START_WAIT;
    loop {
        let st = health(paths)?;
        if st.running {
            return Ok(st);
        }
        if Instant::now() >= deadline {
            let tail = log_tail(paths, 15);
            let extra = if tail.is_empty() {
                // 空日志最常见的原因是子进程根本没起来（例如 Windows Store
                // python alias 静默退出）。带上解释器路径便于排查。
                trf("err.proxy.log_empty", &[("py", &py.display().to_string())])
            } else {
                format!("\n{}", trf("err.proxy.log_tail", &[("log", &tail)]))
            };
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
            let cmd = p
                .cmd()
                .iter()
                .map(|s| s.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
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
    let tail: Vec<&str> = text
        .lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
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
        if cols.len() >= 5
            && cols[0].eq_ignore_ascii_case("TCP")
            && cols[3].eq_ignore_ascii_case("LISTENING")
        {
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
        assert!(
            PROXY_CONFIG_TEMPLATE.contains("listen_port"),
            "内嵌配置模板损坏"
        );
    }

    #[test]
    fn script_tag_changes_with_content() {
        assert_eq!(sha2_short(b"abc"), sha2_short(b"abc"));
        assert_ne!(sha2_short(b"abc"), sha2_short(b"abd"));
    }

    /// 回归：内嵌脚本的 PROXY_SCRIPT_VERSION 必须与 CARGO_PKG_VERSION 一致，
    /// 否则升级后 stale 判断永远为真，每次 start 都会多一次无谓重启。
    #[test]
    fn embedded_script_version_matches_crate_version() {
        let needle = format!("PROXY_SCRIPT_VERSION = \"{}\"", SCRIPT_VERSION);
        assert!(
            PROXY_SCRIPT.contains(&needle),
            "lobster_proxy.py 的 PROXY_SCRIPT_VERSION 必须等于 {SCRIPT_VERSION}"
        );
    }

    /// 候选列表必须去重且保序：PATH 里同一目录可能出现多次（本机 mingw64/bin
    /// 就重复了），重复项会让探测白跑一次 `python -c`。
    #[test]
    fn candidates_dedup_and_order() {
        let mut out: Vec<PathBuf> = Vec::new();
        let mut push = |p: PathBuf| {
            if !out.contains(&p) {
                out.push(p);
            }
        };
        push(PathBuf::from("a\\python.exe"));
        push(PathBuf::from("a\\python.exe"));
        push(PathBuf::from("b\\python.exe"));
        assert_eq!(
            out,
            vec![
                PathBuf::from("a\\python.exe"),
                PathBuf::from("b\\python.exe")
            ]
        );
    }

    /// 不存在的路径必须判为不可用（不能当作有效解释器）。
    #[test]
    fn python_works_rejects_missing() {
        assert!(!python_works(Path::new(
            "Z:\\definitely-not-a-real-python-xyz.exe"
        )));
    }

    /// 真解释器要能被 `python -c "pass"` 验证通过。
    #[test]
    fn python_works_accepts_real_interpreter() {
        let Some(py) = find_python() else {
            // CI/无 Python 环境：跳过而非失败
            return;
        };
        assert!(python_works(&py), "{} 实际不可用", py.display());
    }

    /// 回归：PATH 首位是 Windows Store alias（启动即退出码 49）时，
    /// find_python 必须跳过它找到真正的解释器，而不是把 alias 交出去。
    #[test]
    fn find_python_skips_store_alias() {
        let alias = PathBuf::from(std::env::var("APPDATA").unwrap_or_default())
            .join("Local")
            .join("Microsoft")
            .join("WindowsApps")
            .join("python.exe");
        if !alias.is_file() {
            return;
        }
        // alias 存在但不可用；若 find_python 返回它，说明校验失效
        if let Some(py) = find_python() {
            assert_ne!(
                py, alias,
                "find_python 返回了 Windows Store alias（未做实际启动校验）"
            );
        }
    }
}
