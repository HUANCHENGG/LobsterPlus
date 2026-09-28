//! 账号快照清单与目录交换引擎（LobsterPlus 核心新模块）。
//!
//! LobsterAI 的对话库（cowork_sessions/cowork_messages）没有任何账号字段
//! （单账号设计），凭据（kv.auth_tokens）、Cookies、openclaw 状态又都在同
//! 一个数据目录里——唯一安全的账号隔离方式是「清单式目录交换」：
//! 按固定 manifest 把账号私有状态在 live 数据目录与账号快照目录之间搬移。
//!
//! manifest 包含 = 账号私有（对话库、凭据、会话、登录缓存）；
//! manifest 之外 = 可重建缓存/静态运行时（Cache、logs、updates 298MB、
//! python-win 运行时等），永远留在 live 目录，不进快照。
//!
//! 交换语义（切换账号时）：
//!   live 的 manifest 路径 → 移入源账号快照（保鲜：对话/token 全量带走）
//!   目标账号快照的 manifest 路径 → 拷回 live（目录先移到临时名再 rename，
//!   尽量原子；Windows 上 rename 目录要求目标不存在，所以 live 侧先移走）
//!
//! Windows 文件占用：客户端退出后仍可能短暂持有句柄（杀毒扫描等），
//! 拷贝/删除带重试。

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 账号私有状态清单（相对 live 数据目录的路径）。
/// 目录以自身为整体交换（递归），文件单文件交换（+wal/-shm 兄弟文件随主库）。
pub const MANIFEST_PATHS: &[&str] = &[
    // 主数据库：登录态 kv + 对话库 cowork_* + 收藏 + 计划任务
    "lobsterai.sqlite",
    "lobsterai.sqlite-wal",
    "lobsterai.sqlite-shm",
    // Chromium 系登录缓存/偏好（Cookies 在 Network\）
    "Network",
    "Local Storage",
    "Session Storage",
    "Shared Dictionary",
    "Shared Storage",
    "Partitions",
    "Preferences",
    "Local State",
    "DIPS",
    "DIPS-wal",
    "DIPS-shm",
    // 内嵌 OpenClaw 状态：openclaw.json + gateway 凭据 + agent 会话库
    "openclaw\\state",
    "openclaw\\.openclaw\\exec-approvals.json",
    "openclaw\\plugin-skills",
    // 客户端 skill 目录（33 个内置 skill，账号可自装）
    "SKILLs",
];

/// live 数据目录下要显式排除的路径（可重建缓存，永远不进快照，也不许误拷）。
/// manifest 之外的目录本来就不交换，这里用于 dry-run 展示与防御性校验。
pub const EXCLUDED_PATHS: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "blob_storage",
    "logs",
    "updates",
    "runtimes",
    "cowork",
    "openclaw\\bin",
    "openclaw\\logs",
    "openclaw\\cache",
    "openclaw\\media",
    "openclaw\\.compile-cache",
    "lockfile",
];

/// manifest 内某路径是否落在排除清单（防御性：manifest 与 EXCLUDE 不得重叠）。
pub fn manifest_conflicts() -> Vec<&'static str> {
    MANIFEST_PATHS
        .iter()
        .filter(|m| {
            EXCLUDED_PATHS.iter().any(|x| {
                let m = m.replace('\\', "/");
                let x = x.replace('\\', "/");
                m == x || m.starts_with(&format!("{x}/")) || x.starts_with(&format!("{m}/"))
            })
        })
        .copied()
        .collect()
}

/// 解析 manifest 路径的规范分隔符（manifest 用 '\\' 声明，跨平台 join 用）。
fn rel_segments(rel: &str) -> Vec<&str> {
    rel.split(['\\', '/']).filter(|s| !s.is_empty()).collect()
}

fn join_rel(base: &Path, rel: &str) -> PathBuf {
    let mut p = base.to_path_buf();
    for seg in rel_segments(rel) {
        p.push(seg);
    }
    p
}

/// 快照目录里放 manifest 内容的根（账号快照目录/snapshot/）。
pub fn snapshot_root(account_dir: &Path) -> PathBuf {
    account_dir.join("snapshot")
}

/// 统计目录+文件总大小（字节）。目录不存在返回 0。
pub fn path_size(p: &Path) -> u64 {
    if !p.exists() {
        return 0;
    }
    let meta = match fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    let mut total = 0;
    if let Ok(rd) = fs::read_dir(p) {
        for e in rd.flatten() {
            total += path_size(&e.path());
        }
    }
    total
}

fn retry_op<T>(op: impl Fn() -> Result<T, std::io::Error>, what: &str) -> Result<T, String> {
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut last: Option<std::io::Error> = None;
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(32) || e.raw_os_error() == Some(5) => {
                // ERROR_SHARING_VIOLATION(32) / ACCESS_DENIED(5)：Windows 句柄占用，重试
                last = Some(e);
                if Instant::now() >= deadline {
                    return Err(format!("{what} 失败（文件被占用，重试耗尽）：{}", last.unwrap()));
                }
                std::thread::sleep(Duration::from_millis(400));
            }
            Err(e) => {
                // 目录/文件不存在视为幂等成功由调用方处理；这里直接报错
                return Err(format!("{what} 失败：{e}"));
            }
        }
    }
}

/// 递归拷贝目录（覆盖目标已存在内容）。目标父目录自动创建。
pub fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("创建目录失败 {}：{e}", dst.display()))?;
    for entry in fs::read_dir(src).map_err(|e| format!("读取目录失败 {}：{e}", src.display()))? {
        let entry = entry.map_err(|e| format!("读取目录失败 {e}"))?;
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&from, &to)?;
        } else if ty.is_symlink() {
            // 数据目录内不应有符号链接；跳过并记录（防御性）
            eprintln!("manifest: 跳过符号链接 {}", from.display());
        } else {
            retry_op(|| fs::copy(&from, &to), &format!("拷贝 {}", from.display()))?;
        }
    }
    Ok(())
}

/// 删除目录/文件（幂等：不存在 = 成功）。
pub fn remove_path(p: &Path) -> Result<(), String> {
    if !p.exists() && fs::symlink_metadata(p).is_err() {
        return Ok(());
    }
    let meta = fs::symlink_metadata(p).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        retry_op(|| fs::remove_dir_all(p), &format!("删除目录 {}", p.display()))
    } else {
        retry_op(|| fs::remove_file(p), &format!("删除文件 {}", p.display()))
    }
}

/// 把 live 的一个 manifest 路径「移动」到快照目录（同盘 move 优先，失败回退 copy+delete）。
fn move_live_to_snapshot(live: &Path, snap: &Path) -> Result<(), String> {
    if !live.exists() {
        return Ok(());
    }
    if let Some(parent) = snap.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目录失败 {}：{e}", parent.display()))?;
    }
    // 快照侧已有旧内容：先清掉（sync_back 全量覆盖语义）
    remove_path(snap)?;
    if fs::rename(live, snap).is_ok() {
        return Ok(());
    }
    // 跨设备/被占用兜底：copy + delete
    let meta = fs::symlink_metadata(live).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        copy_dir_all(live, snap)?;
        remove_path(live)?;
    } else {
        retry_op(|| fs::copy(live, snap), &format!("拷贝 {}", live.display()))?;
        remove_path(live)?;
    }
    Ok(())
}

/// 把快照的一个 manifest 路径拷回 live（live 侧目标已先移走，这里纯 copy）。
fn restore_one(snap: &Path, live: &Path) -> Result<(), String> {
    if !snap.exists() {
        return Ok(());
    }
    if let Some(parent) = live.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目录失败 {}：{e}", parent.display()))?;
    }
    let meta = fs::symlink_metadata(snap).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        // live 残留（理论不应存在）清掉再拷
        if live.exists() {
            remove_path(live)?;
        }
        if fs::rename(snap, live).is_ok() {
            return Ok(());
        }
        copy_dir_all(snap, live)?;
    } else {
        if live.exists() {
            remove_path(live)?;
        }
        retry_op(|| fs::copy(snap, live), &format!("拷贝 {}", snap.display()))?;
    }
    Ok(())
}

/// manifest 交换结果（切换流程消费）。
#[derive(Default, serde::Serialize)]
pub struct ExchangeReport {
    /// 移入源账号快照的路径
    pub preserved: Vec<String>,
    /// 从目标快照恢复到 live 的路径
    pub restored: Vec<String>,
    /// live 侧存在但快照缺失（目标账号从未有过该项）的路径
    pub skipped: Vec<String>,
}

/// dry-run：列出 live 数据目录中实际存在的 manifest 路径。
pub fn live_manifest_paths(live_dir: &Path) -> Vec<String> {
    MANIFEST_PATHS
        .iter()
        .filter(|m| join_rel(live_dir, m).exists())
        .map(|m| m.to_string())
        .collect()
}

/// 把 live 的全部 manifest 路径保存进账号快照目录（收编 / sync_back / 交换第一步）。
/// 全量覆盖：快照侧旧内容先删。
pub fn save_live_to_snapshot(live_dir: &Path, account_dir: &Path) -> Result<ExchangeReport, String> {
    let snap_root = snapshot_root(account_dir);
    fs::create_dir_all(&snap_root).map_err(|e| format!("创建快照目录失败：{e}"))?;
    let mut report = ExchangeReport::default();
    for m in MANIFEST_PATHS {
        let live = join_rel(live_dir, m);
        if !live.exists() {
            continue;
        }
        let snap = join_rel(&snap_root, m);
        move_live_to_snapshot(&live, &snap)?;
        report.preserved.push(m.to_string());
    }
    Ok(report)
}

/// 把账号快照目录的 manifest 路径恢复到 live（切换第二步）。
/// live 侧当前内容应已被 save_live_to_snapshot 移走；残留（快照没有的项）会被清除
/// （防止上个账号的对话库残留串号——这是对话隔离的关键防线）。
pub fn restore_snapshot_to_live(account_dir: &Path, live_dir: &Path) -> Result<ExchangeReport, String> {
    restore_tree_to_live(&snapshot_root(account_dir), live_dir)
}

/// 备份目录回滚（读回校验失败安全网）。备份目录与快照目录同构。
pub fn restore_backup_to_live(backup_dir: &Path, live_dir: &Path) -> Result<ExchangeReport, String> {
    restore_tree_to_live(backup_dir, live_dir)
}

fn restore_tree_to_live(tree_root: &Path, live_dir: &Path) -> Result<ExchangeReport, String> {
    let snap_root = tree_root;
    if !snap_root.is_dir() {
        return Err(format!("快照目录不存在：{}", snap_root.display()));
    }
    let mut report = ExchangeReport::default();
    for m in MANIFEST_PATHS {
        let live = join_rel(live_dir, m);
        let snap = join_rel(&snap_root, m);
        if snap.exists() {
            restore_one(&snap, &live)?;
            report.restored.push(m.to_string());
        } else {
            // 目标账号快照缺这项 → live 残留必须清除（串号防线）
            if live.exists() {
                remove_path(&live)?;
            }
            report.skipped.push(m.to_string());
        }
    }
    Ok(report)
}

/// 首次切换前把 live manifest 子集复制（不是移动）到备份目录（zip 语义，这里用目录快照）。
pub fn backup_live(live_dir: &Path, backup_dir: &Path) -> Result<Vec<String>, String> {
    fs::create_dir_all(backup_dir).map_err(|e| format!("创建备份目录失败：{e}"))?;
    let mut out = vec![];
    for m in MANIFEST_PATHS {
        let live = join_rel(live_dir, m);
        if !live.exists() {
            continue;
        }
        let dst = join_rel(backup_dir, m);
        let meta = fs::symlink_metadata(&live).map_err(|e| e.to_string())?;
        if meta.is_dir() {
            copy_dir_all(&live, &dst)?;
        } else {
            if let Some(p) = dst.parent() {
                fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            retry_op(|| fs::copy(&live, &dst), &format!("备份 {}", live.display()))?;
        }
        out.push(m.to_string());
    }
    Ok(out)
}

/// 快照目录占用的总大小（doctor / 删账号提示用）。
pub fn snapshot_size(account_dir: &Path) -> u64 {
    path_size(&snapshot_root(account_dir))
}

/// JSON 展示用（dry-run / doctor）。
pub fn manifest_json() -> Value {
    serde_json::json!({
        "included": MANIFEST_PATHS,
        "excluded": EXCLUDED_PATHS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path, content: &str) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, content).unwrap();
    }

    fn make_live() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lp-mani-{}", uuid::Uuid::new_v4()));
        touch(&dir.join("lobsterai.sqlite"), "db-A");
        touch(&dir.join("lobsterai.sqlite-wal"), "wal-A");
        touch(&dir.join("Network").join("Cookies"), "cookies-A");
        touch(&dir.join("openclaw").join("state").join("openclaw.json"), "oc-A");
        touch(&dir.join("openclaw").join("state").join("agents").join("main").join("agent").join("openclaw-agent.sqlite"), "agent-A");
        touch(&dir.join("SKILLs").join("checkin").join("SKILL.md"), "skill-A");
        // 排除项（不该进快照）
        touch(&dir.join("Cache").join("x.bin"), "cache-A");
        touch(&dir.join("logs").join("main.log"), "log-A");
        touch(&dir.join("updates").join("update.exe"), "update-A");
        touch(&dir.join("lockfile"), "");
        dir
    }

    #[test]
    fn manifest_has_no_conflicts() {
        assert!(manifest_conflicts().is_empty(), "manifest 与排除清单重叠：{:?}", manifest_conflicts());
    }

    #[test]
    fn save_and_restore_roundtrip() {
        let live = make_live();
        let acc_a = std::env::temp_dir().join(format!("lp-accA-{}", uuid::Uuid::new_v4()));
        let acc_b = std::env::temp_dir().join(format!("lp-accB-{}", uuid::Uuid::new_v4()));

        // A 收编：live → A 快照
        let r = save_live_to_snapshot(&live, &acc_a).unwrap();
        assert!(r.preserved.contains(&"lobsterai.sqlite".to_string()));
        assert!(r.preserved.contains(&"openclaw\\state".to_string()));
        assert!(!r.preserved.iter().any(|p| p.contains("Cache")));
        // live 的 manifest 路径已移走，但排除项留在原地
        assert!(!live.join("lobsterai.sqlite").exists());
        assert!(live.join("Cache").join("x.bin").exists());
        assert!(live.join("updates").join("update.exe").exists());

        // live 变成 B（模拟客户端生成了 B 的数据）
        touch(&live.join("lobsterai.sqlite"), "db-B");
        touch(&live.join("openclaw").join("state").join("openclaw.json"), "oc-B");
        touch(&live.join("Network").join("Cookies"), "cookies-B");

        // B 收编：live → B 快照
        save_live_to_snapshot(&live, &acc_b).unwrap();

        // 切回 A：A 快照 → live
        let r = restore_snapshot_to_live(&acc_a, &live).unwrap();
        assert!(r.restored.contains(&"lobsterai.sqlite".to_string()));
        assert_eq!(fs::read_to_string(live.join("lobsterai.sqlite")).unwrap(), "db-A");
        assert_eq!(fs::read_to_string(live.join("openclaw").join("state").join("openclaw.json")).unwrap(), "oc-A");
        assert_eq!(
            fs::read_to_string(live.join("openclaw").join("state").join("agents").join("main").join("agent").join("openclaw-agent.sqlite")).unwrap(),
            "agent-A"
        );

        // 切到 B：先把 live 存回 A，再恢复 B
        save_live_to_snapshot(&live, &acc_a).unwrap();
        restore_snapshot_to_live(&acc_b, &live).unwrap();
        assert_eq!(fs::read_to_string(live.join("lobsterai.sqlite")).unwrap(), "db-B");
        // B 快照没有 SKILLs（make_live 未重建）→ live 的 SKILLs 被清除（串号防线）
        assert!(!live.join("SKILLs").exists());
        // A 快照里 SKILLs 完好
        assert!(acc_a.join("snapshot").join("SKILLs").join("checkin").join("SKILL.md").exists());

        let _ = fs::remove_dir_all(&live);
        let _ = fs::remove_dir_all(&acc_a);
        let _ = fs::remove_dir_all(&acc_b);
    }

    #[test]
    fn backup_is_copy_not_move() {
        let live = make_live();
        let bak = std::env::temp_dir().join(format!("lp-bak-{}", uuid::Uuid::new_v4()));
        let backed = backup_live(&live, &bak).unwrap();
        assert!(backed.contains(&"lobsterai.sqlite".to_string()));
        // live 原样保留
        assert_eq!(fs::read_to_string(live.join("lobsterai.sqlite")).unwrap(), "db-A");
        assert_eq!(fs::read_to_string(bak.join("lobsterai.sqlite")).unwrap(), "db-A");
        let _ = fs::remove_dir_all(&live);
        let _ = fs::remove_dir_all(&bak);
    }

    #[test]
    fn dry_run_lists_existing_only() {
        let live = make_live();
        let paths = live_manifest_paths(&live);
        assert!(paths.contains(&"lobsterai.sqlite".to_string()));
        assert!(!paths.iter().any(|p| p == "DIPS"));
        let _ = fs::remove_dir_all(&live);
    }
}
