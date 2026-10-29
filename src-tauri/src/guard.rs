use crate::store::{in_sandbox, Paths};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use sysinfo::{ProcessesToUpdate, System};

/// 进程名包含 lobsterai 即视为 LobsterAI（Electron 多进程形态，主/子进程同名）。
/// 排除 lobster-plus 自身。运行中共观测到 6 个进程，切换时必须全部退出，
/// 否则残留进程会继续持有 sqlite/文件句柄导致交换失败。
pub fn is_lobster_name(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("lobsterai") && !n.contains("lobster-plus")
}

/// 命令行含 lobster_proxy.py → 我们的 Python 代理进程（proxy.rs stop 用）。
pub fn is_proxy_cmdline(cmd: &str) -> bool {
    cmd.contains("lobster_proxy.py")
}

/// LobsterAI 的派生进程：SKILL 会派生独立 node/python 服务（命令行含 LobsterAI
/// 数据目录，实测 SKILLs\web-search\...\index.js 占住 SKILLs 目录导致交换失败）。
/// 这些进程名不含 lobsterai，必须按命令行识别并一并杀。
pub fn is_lobster_child_cmdline(cmd: &str) -> bool {
    let c = cmd.to_lowercase();
    c.contains("\\lobsterai\\skills")
        || c.contains("\\lobsterai\\openclaw")
        || c.contains("/lobsterai/skills")
        || c.contains("/lobsterai/openclaw")
}

/// 杀掉全部 LobsterAI 主进程 + 派生进程。返回是否全部退出。
fn kill_all_impl(paths: &Paths) -> bool {
    let mut sys = refresh_system();
    for (_pid, p) in sys.processes() {
        let name = p.name().to_string_lossy();
        if is_lobster_name(&name) {
            let _ = p.kill();
            continue;
        }
        // Electron 主进程之外，node/python 派生服务按命令行识别
        let exe_lc = p
            .exe()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if exe_lc.contains("lobsterai") {
            let _ = p.kill();
            continue;
        }
        let cmd = p
            .cmd()
            .iter()
            .map(|s| s.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        if is_lobster_child_cmdline(&cmd) {
            let _ = p.kill();
        }
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(400));
        if !any_lobster_left(paths) {
            return true;
        }
        sys.refresh_processes(ProcessesToUpdate::All, true);
    }
    !any_lobster_left(paths)
}

/// 是否还有 LobsterAI 主进程或派生进程存活。
fn any_lobster_left(paths: &Paths) -> bool {
    let sys = refresh_system();
    for (_pid, p) in sys.processes() {
        let name = p.name().to_string_lossy();
        if is_lobster_name(&name) {
            return true;
        }
        let cmd = p
            .cmd()
            .iter()
            .map(|s| s.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        if is_lobster_child_cmdline(&cmd) {
            return true;
        }
    }
    let _ = paths;
    false
}

fn refresh_system() -> System {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    sys
}

pub struct LobsterProc {
    pub running: bool,
    pub exe: Option<PathBuf>,
}

pub fn lobster_process(paths: &Paths) -> LobsterProc {
    if in_sandbox() {
        return LobsterProc {
            running: false,
            exe: None,
        };
    }
    let sys = refresh_system();
    let mut running = false;
    let mut exe: Option<PathBuf> = None;
    for (_pid, p) in sys.processes() {
        let name = p.name().to_string_lossy();
        if is_lobster_name(&name) {
            running = true;
            if exe.is_none() {
                exe = p.exe().map(|e| e.to_path_buf());
            }
        }
    }
    let _ = paths;
    LobsterProc { running, exe }
}

pub fn lobster_running(paths: &Paths) -> bool {
    lobster_process(paths).running
}

pub fn kill_lobster(paths: &Paths) -> Result<bool, String> {
    if in_sandbox() {
        return Ok(true);
    }
    if !lobster_running(paths) {
        return Ok(true);
    }
    Ok(kill_all_impl(paths))
}

/// 优雅关闭 LobsterAI：`taskkill /PID`（不带 /F）投递 WM_CLOSE，让客户端
/// 走正常退出流程、把 WAL 落盘；等待退出，超时回退强杀。
/// SKILL 派生的 node/python 服务不响应 WM_CLOSE，在强杀阶段统一清掉。
/// 切换/收编前必须走这条路径——强杀没有 flush 机会。
pub fn close_lobster_graceful(paths: &Paths) -> Result<bool, String> {
    if in_sandbox() || !lobster_running(paths) {
        return Ok(true);
    }
    #[cfg(windows)]
    {
        let sys = refresh_system();
        for (_pid, p) in sys.processes() {
            if is_lobster_name(&p.name().to_string_lossy()) {
                let pid = p.pid().as_u32();
                use std::os::windows::process::CommandExt;
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string()])
                    .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                    .output();
            }
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(500));
            if !lobster_running(paths) {
                // 主进程已退；派生 node/python 可能还活着，交给强杀阶段
                return kill_lobster(paths);
            }
        }
    }
    kill_lobster(paths)
}

pub fn launch_lobster(path: &str) -> Result<(), String> {
    if in_sandbox() {
        return Ok(());
    }
    let p = PathBuf::from(path);
    if !p.exists() {
        return Err(crate::i18n::trf("err.lobster.missing", &[("path", path)]));
    }
    // 三个标准流全部显式置 null：继承父进程管道会让 CLI 调用方（python
    // subprocess 等）挂到 LobsterAI 退出——launch 命令永不返回的根因。
    // stdin/stdout/stderr 返回 &mut Command，flags 直接在最终引用上设。
    #[allow(unused_mut)]
    let mut c = std::process::Command::new(&p);
    c.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    set_detach_flags(&mut c);
    // 同 proxy::start：别让长驻的 LobsterAI 继承 CLI 调用方的管道句柄。
    let _inherit_guard = suspend_std_handle_inheritance();
    c.spawn()
        .map_err(|e| crate::i18n::trf("err.lobster.launch", &[("e", &e.to_string())]))?;
    drop(_inherit_guard);
    Ok(())
}

#[cfg(windows)]
fn set_detach_flags(c: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    c.creation_flags(0x0000_0008 | 0x0000_0200); // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
}

/// RAII：临时清除本进程标准流句柄的 HANDLE_FLAG_INHERIT，drop 时恢复原值。
///
/// 为什么需要：Rust std 在 Windows 上以 bInheritHandles=TRUE 创建子进程
/// （为把重定向后的 stdio 传进去）。于是父进程里所有「可继承」句柄都会漏给
/// 子进程。CLI 被 shell 用管道调用时（`lobster-plus.exe --cli proxy start |
/// grep ...`），shell 创建的管道写端就是可继承的；长驻的 Python 代理会把它
/// 一起继承，导致 lobster-plus.exe 早已退出、grep/cat 却等不到 EOF，
/// 整条命令永久挂死。
///
/// 做法：spawn 前把 CLI 自己的 std 句柄改成不可继承，spawn 完立刻恢复。
/// 子进程需要的 stdio 是 std 自己复制出的可继承副本，不依赖这里的标志，
/// 所以不受影响。必须在 spawn **之前**清除——子进程一旦继承，事后无法收回。
#[cfg(windows)]
pub struct StdInheritGuard {
    saved: Vec<(windows_sys::Win32::Foundation::HANDLE, u32)>,
}

#[cfg(windows)]
pub fn suspend_std_handle_inheritance() -> StdInheritGuard {
    use windows_sys::Win32::Foundation::{
        GetHandleInformation, SetHandleInformation, HANDLE_FLAG_INHERIT,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    let mut saved = Vec::new();
    for std_id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        unsafe {
            let h = GetStdHandle(std_id);
            if h == 0 || h == -1 {
                continue;
            }
            let mut flags: u32 = 0;
            if GetHandleInformation(h, &mut flags) != 0 && flags & HANDLE_FLAG_INHERIT != 0 {
                if SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) != 0 {
                    saved.push((h, flags));
                }
            }
        }
    }
    StdInheritGuard { saved }
}

#[cfg(windows)]
impl Drop for StdInheritGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
        for (h, flags) in self.saved.drain(..) {
            unsafe {
                let _ = SetHandleInformation(h, HANDLE_FLAG_INHERIT, flags & HANDLE_FLAG_INHERIT);
            }
        }
    }
}

#[cfg(not(windows))]
pub struct StdInheritGuard;

#[cfg(not(windows))]
pub fn suspend_std_handle_inheritance() -> StdInheritGuard {
    StdInheritGuard
}

#[cfg(not(windows))]
fn set_detach_flags(c: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    c.process_group(0);
}

/// LobsterAI 主程序候选路径。
/// 本机实测安装在 D:\Install\LobsterAI\LobsterAI.exe；常规位置是 LOCALAPPDATA。
pub fn lobster_path_candidates() -> Vec<String> {
    let mut out = vec![];
    if let Ok(d) = std::env::var("LOCALAPPDATA") {
        out.push(format!(r"{d}\Programs\LobsterAI\LobsterAI.exe"));
    }
    for d in ["C:", "D:", "E:"] {
        out.push(format!(r"{d}\Install\LobsterAI\LobsterAI.exe"));
        out.push(format!(r"{d}\Program Files\LobsterAI\LobsterAI.exe"));
        out.push(format!(r"{d}\Apps\LobsterAI\LobsterAI.exe"));
    }
    out
}

pub fn effective_lobster_path(paths: &Paths) -> (String, bool) {
    let settings = crate::store::load_settings(paths);
    if let Some(p) = settings
        .lobster_path
        .as_ref()
        .filter(|p| std::path::PathBuf::from(p).exists())
    {
        return (p.clone(), true);
    }
    // 运行中的 LobsterAI 自己暴露安装路径
    if !in_sandbox() {
        let sys = refresh_system();
        for (_pid, p) in sys.processes() {
            if is_lobster_name(&p.name().to_string_lossy()) {
                if let Some(exe) = p.exe() {
                    return (exe.to_string_lossy().to_string(), true);
                }
            }
        }
    }
    for c in lobster_path_candidates() {
        if !c.is_empty() && std::path::PathBuf::from(&c).exists() {
            return (c, true);
        }
    }
    if let Some(p) = settings.lobster_path {
        return (p, false);
    }
    (String::new(), false)
}

pub fn kill_lobster_checked(paths: &Paths) -> Result<bool, String> {
    if !kill_lobster(paths)? {
        return Err(crate::i18n::tr("err.lobster.kill_timeout"));
    }
    Ok(true)
}
