# LobsterPlus

有道 LobsterAI（龙虾）桌面客户端的多账号管理器：**多账号切换保留对话记录 + 全库每日自动签到 + 本地 API 稳定代理给 CC-Switch 用**。

## 功能

### 1. 多账号切换（对话记录完整保留）

LobsterAI 的对话库（`cowork_sessions`/`cowork_messages`）没有账号字段——整个数据目录按单账号设计。LobsterPlus 采用 **manifest 清单式目录快照交换**：

- 首次收编账号 = LobsterAI 不会被关闭或重启；主数据库用 SQLite Backup API 从活跃 WAL 创建数据库一致副本，其余账号私有路径逐项在线复制到 staging，成功后一次提交快照，live 全程不移动；SQLite 之外的文件不承诺跨文件同一时刻一致；
- 切换账号仍需停机 = 杀客户端 → 当前数据保鲜回源账号快照 → 目标账号快照恢复到 live → **读回校验**（live `auth_user.id` 必须等于目标账号）→ 可选重启；校验失败自动从首次切换前的全量备份回滚；
- 可重建的缓存（Cache、logs、updates 298MB、python 运行时等）永远留在 live 目录，不进快照，切换开销最小化。

### 2. 全库每日自动签到

移植 youdaocheckin 已逆向的三步接口（`lobsterai-server.youdao.com`）：查活动位 → 查上下文（claimedToday 预检）→ check_in（幂等键）。签到只需要 token，**不依赖客户端运行**：

- 非活跃账号用账号库加密 blob 独立续期（`/api/auth/refresh`），续期后回写 blob 与快照；
- 活跃账号优先读 live sqlite 最新鲜的 token；
- GUI 启动补签 + 每小时循环；`--cli checkin` 可挂 Windows 计划任务（`schedule install` 一条命令装好每日 09:00 任务）。

### 3. 本地 API 稳定代理（转 API 给 CC-Switch）

LobsterAI 自带本地 OpenAI 兼容代理，但**端口和 apiKey 随每次重启轮换**（3248→3858→2880→1316…），接 CC-Switch 一配就失效。LobsterPlus 监听**固定端口 19260**：

- 自动探测当前轮换的本地端口/key（models.json + openclaw.json 双候选 + TCP/最小 chat 请求探活），轮换对调用方彻底透明；
- 双协议：`/v1/chat/completions`（OpenAI）+ `/v1/messages`（Anthropic 协议转换，Claude Code 可直连）+ `/v1/models` + `/health`；
- 本地按日统计请求与输入/输出 Token，区分上游准确 usage 和本地估算，支持今日、7 天、30 天、全部范围及按模型汇总；只保存聚合数据，不保存请求/响应正文或密钥；
- **一键注册进 CC-Switch**（备份 db 后 SQL upsert provider，非破坏性），Base URL 填 `http://127.0.0.1:19260` 永不漂移。

模型清单来自 LobsterAI 服务端 `/api/models/available`（客户端自己用的权威来源，带真实显示名与上下文窗口），本地 openclaw 配置与 `config.json` 依次兜底。`claude-*` 映射别名只用于协议转换，不会出现在模型列表里。

升级后代理会自己发现并重启旧脚本（`/health` 的 `script_version` 与程序版本比对），不用手动结束进程。标题栏显示当前版本号；代理仍在跑旧脚本时胶囊变黄提示重启。

## 快速开始

1. 从 [Releases](https://github.com/HUANCHENGG/LobsterPlus/releases) 下载最新的 `LobsterPlus_<版本>_x64-setup.exe` 安装；
2. 打开 LobsterPlus，点「收编当前登录」把当前 LobsterAI 账号入库；
3. 需要把本地 API 给 CC-Switch 用时，点代理卡片的「注册 CC-Switch」；
4. 需要每日自动签到，执行一次 `lobster-plus.exe --cli schedule install`。

## 架构蓝本

- **RaccoonPlus / TraePlus**：Tauri 2 + Rust 家族架构（快照+原子替换、enc:v1 账号库、ProxyLock、CLI 全功能镜像）；Anthropic↔OpenAI 协议转换层 fork 自 `raccoon_proxy.py`。
- **youdaocheckin**：签到三步 HTTP 流程与 token 续期。
- **youdaoAPI**：上游探测候选（models.json/openclaw.json）与 CC-Switch 数据库注册逻辑。

## CLI

```
lobster-plus.exe --cli <子命令>
  state          全量状态 JSON
  list           账号列表
  capture        在线收编当前登录 [--name] [--restart]
  switch --id <id> [--force] [--dry-run] [--restart|--no-restart]
  rename / delete / launch / kill / setpath / behavior
  checkin [--id]  全库（或单账号）签到
  proxy start|stop|status|log
  register [--switch]  注册进 CC-Switch
  schedule install|uninstall|status  每日签到计划任务
  doctor         一键诊断
  manifest       查看快照清单
```

## 配置

代理配置在 `~\.lobster-plus\proxy\config.json`（首次启动时生成，之后不会被程序覆盖），常用项：

| 键 | 默认 | 说明 |
|---|---|---|
| `listen_port` | 19260 | 固定监听端口 |
| `default_model` | `deepseek-flash` | 未匹配到模型时的兜底上游模型 |
| `api_key` | 空 | 代理自身 Bearer 鉴权；留空=不校验（仅监听 127.0.0.1） |
| `models` | 见模板 | 模型名映射，`claude-*` 别名 → 上游真实 ID |
| `probe_recheck_seconds` | 60 | 上游端口/key 重探测间隔 |

改完配置需要重启代理（GUI 的「停止代理 / 启动代理」）。账号库路径可用环境变量 `LOBSTER_PLUS_HOME` 重定向，凭据加密密钥用 `LOBSTER_PLUS_CREDENTIAL_SECRET`。

## 安全设计

- 账号凭据 AES-256-GCM 加密（`enc:v1:`，密钥 = `LOBSTER_PLUS_CREDENTIAL_SECRET` 或机器指纹派生）；
- live `lobsterai.sqlite` 只读（红线：仅客户端退出后的 token 续期回写除外）；
- 首次切换前自动全量备份；切换读回校验失败自动回滚；删除账号仅删快照不动 live；
- CLI JSON 输出经 `account_public` 过滤，不含任何密钥。

## 构建

```
scripts\build-release.cmd   （前端 + release exe，GNU 工具链）
npm ci && npm run tauri dev   （开发模式）
cargo test --manifest-path src-tauri\Cargo.toml --lib --no-default-features   （Rust 单元测试）
"%APPDATA%\LobsterAI\runtimes\python-win\python.exe" src-tauri\proxy\lobster_proxy.py --selftest-storage   （代理三项自检：用量存储/连接容错/模型目录）
```

发布安装包（需本机已装 NSIS）：

```
npx tauri build --target x86_64-pc-windows-gnu
```

产物在 `src-tauri\target\x86_64-pc-windows-gnu\release\bundle\nsis\`。必须发安装包而不是裸 exe——裸 exe 依赖同目录的 `WebView2Loader.dll`，单独分发会启动失败。

改版本号时四处一起改：`package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`、`src-tauri/proxy/lobster_proxy.py` 的 `PROXY_SCRIPT_VERSION`（后者不一致会让代理每次启动都误判为旧脚本并重启）。

## 许可证

MIT，见 `package.json` 的 `license` 字段。

详见 [架构说明](docs/architecture.md)。
