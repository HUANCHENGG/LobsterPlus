# LobsterPlus

有道 LobsterAI（龙虾）桌面客户端的多账号管理器：**多账号切换保留对话记录 + 全库每日自动签到 + 本地 API 稳定代理给 CC-Switch 用**。

## 功能

### 1. 多账号切换（对话记录完整保留）

LobsterAI 的对话库（`cowork_sessions`/`cowork_messages`）没有账号字段——整个数据目录按单账号设计。LobsterPlus 采用 **manifest 清单式目录快照交换**：

- 收编账号 = 把 `%APPDATA%\LobsterAI` 里的账号私有路径（对话库、登录态、openclaw 状态、Cookies）整体存入该账号专属快照；
- 切换账号 = 杀客户端 → 当前数据保鲜回源账号快照 → 目标账号快照恢复到 live → **读回校验**（live `auth_user.id` 必须等于目标账号）→ 可选重启；校验失败自动从首次切换前的全量备份回滚；
- 可重建的缓存（Cache、logs、updates 298MB、python 运行时等）永远留在 live 目录，不进快照，切换开销最小化。

### 2. 全库每日自动签到

移植 youdaocheckin 已逆向的三步接口（`lobsterai-server.youdao.com`）：查活动位 → 查上下文（claimedToday 预检）→ check_in（幂等键）。签到只需要 token，**不依赖客户端运行**：

- 非活跃账号用账号库加密 blob 独立续期（`/api/auth/refresh`），续期后回写 blob 与快照；
- 活跃账号优先读 live sqlite 最新鲜的 token；
- GUI 启动补签 + 每小时循环；`--cli checkin` 可挂 Windows 计划任务（`schedule install` 一条命令装好每日 09:00 任务）。

### 3. 本地 API 稳定代理（转 API 给 CC-Switch）

LobsterAI 自带本地 OpenAI 兼容代理，但**端口和 apiKey 随每次重启轮换**（3248→3858→2880→1316…），接 CC-Switch 一配就失效。LobsterPlus 监听**固定端口 19260**：

- 自动探测当前轮换的本地端口/key（models.json + openclaw.json 双候选 + TCP/`/v1/models` 探活），轮换对调用方彻底透明；
- 双协议：`/v1/chat/completions`（OpenAI 纯透传）+ `/v1/messages`（Anthropic 协议转换，Claude Code 可直连）+ `/v1/models` + `/health`；
- **一键注册进 CC-Switch**（备份 db 后 SQL upsert provider，非破坏性），Base URL 填 `http://127.0.0.1:19260` 永不漂移。

## 架构蓝本

- **RaccoonPlus / TraePlus**：Tauri 2 + Rust 家族架构（快照+原子替换、enc:v1 账号库、ProxyLock、CLI 全功能镜像）；Anthropic↔OpenAI 协议转换层 fork 自 `raccoon_proxy.py`。
- **youdaocheckin**：签到三步 HTTP 流程与 token 续期。
- **youdaoAPI**：上游探测候选（models.json/openclaw.json）与 CC-Switch 数据库注册逻辑。

## CLI

```
lobster-plus.exe --cli <子命令>
  state          全量状态 JSON
  list           账号列表
  capture        收编当前登录 [--name]
  switch --id <id> [--force] [--dry-run]
  rename / delete / launch / kill / setpath / behavior
  checkin [--id]  全库（或单账号）签到
  proxy start|stop|status|log
  register [--switch]  注册进 CC-Switch
  schedule install|uninstall|status  每日签到计划任务
  doctor         一键诊断
  manifest       查看快照清单
```

## 安全设计

- 账号凭据 AES-256-GCM 加密（`enc:v1:`，密钥 = `LOBSTER_PLUS_CREDENTIAL_SECRET` 或机器指纹派生）；
- live `lobsterai.sqlite` 只读（红线：仅客户端退出后的 token 续期回写除外）；
- 首次切换前自动全量备份；切换读回校验失败自动回滚；删除账号仅删快照不动 live；
- CLI JSON 输出经 `account_public` 过滤，不含任何密钥。

## 构建

```
scripts\build-release.cmd   （前端 + release exe，GNU 工具链）
npm install && npm run tauri dev   （开发模式）
cargo test --manifest-path src-tauri\Cargo.toml   （单元测试）
```

详见 `docs/architecture.md`。
