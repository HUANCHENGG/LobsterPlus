# LobsterPlus 架构

```
┌────────────────────────────────────────────────────────────┐
│ LobsterPlus (Tauri 2 GUI + CLI 同一 exe，--cli 分流)        │
│                                                            │
│  src-tauri/src/                                            │
│  ├ store.rs     账号库 CRUD + 切换编排（读回校验/回滚）      │
│  ├ manifest.rs  ★清单式目录快照交换引擎                     │
│  ├ kvdb.rs      ★lobsterai.sqlite kv 表读写（rusqlite）     │
│  ├ checkin.rs   三步签到引擎 + token 独立续期               │
│  ├ guard.rs     LobsterAI.exe 探测/优雅关闭/启动            │
│  ├ proxy.rs     Python 代理生命周期 + 三级 Python 发现       │
│  ├ usage.rs     Token usage 只读查询 + 日/模型聚合           │
│  ├ app.rs       Tauri GUI command 薄封装                     │
│  ├ register.rs  ★CC-Switch 数据库注册（备份→upsert→还原）   │
│  ├ zcrypto.rs   enc:v1 AES-256-GCM 账号凭据加密（家族复用）  │
│  ├ lockfile.rs  ProxyLock 跨进程锁（家族复用）              │
│  └ cli.rs       CLI 全功能镜像 + doctor + schtasks          │
│                                                            │
│  src-tauri/proxy/lobster_proxy.py                          │
│    ★上游稳定器（探测轮换端口/key）                          │
│    + Anthropic↔OpenAI 转换层（fork raccoon_proxy.py）       │
│    + OpenAI 转发 + SSE 流式 + Token usage 日聚合            │
└────────────────────────────────────────────────────────────┘
        │                              │
        ▼                              ▼
 %APPDATA%\LobsterAI (live)    ~\.lobster-plus\ (自有库)
   lobsterai.sqlite ←manifest→   accounts\<uuid>\account.json
   openclaw\state     交换        accounts\<uuid>\snapshot\
   Network/Cookies/…              backups\ proxy\ settings.json
        │
        ▼
 https://lobsterai-server.youdao.com  (签到/token 续期)
 http://127.0.0.1:<轮换端口>          (模型上游，代理探测转发)
 http://127.0.0.1:19260               (LobsterPlus 固定端口)
        → CC-Switch / Claude Code
```

## 一、切换引擎（manifest 清单式目录交换）

### 为什么是目录交换而不是键交换

探测结论（2026-09-28 实机）：
- LobsterAI 凭据在 `lobsterai.sqlite` kv 表（`auth_tokens` + `auth_user` 两行）；
- **对话库 `cowork_sessions`/`cowork_messages` 无任何账号/user_id 列**（全库扫描确认）——单账号设计，不能像 TraePlus 那样只换登录键靠数据库天然隔离；
- app.asar 内有 Electron 单例锁（`app.requestSingleInstanceLock`），无法双开。

所以切换时的账号隔离路径是账号私有状态在 live 与快照间整体交换；首次收编只创建副本，不参与交换。

### manifest（store 的账号私有路径，相对 `%APPDATA%\LobsterAI`）

**包含**：`lobsterai.sqlite(+wal/-shm)`、`Network`、`Local Storage`、`Session Storage`、`Shared Dictionary`、`Shared Storage`、`Partitions`、`Preferences`、`Local State`、`DIPS*`、`openclaw\state`（openclaw.json + gateway 凭据 + agents 整树含 openclaw-agent.sqlite）、`openclaw\.openclaw\exec-approvals.json`、`openclaw\plugin-skills`、`SKILLs`。

**排除**（可重建/静态，永远留在 live）：`Cache`、`Code Cache`、`GPUCache`、`Dawn*Cache`、`blob_storage`、`logs`、`updates`（298MB）、`runtimes`、`cowork`、`openclaw\{bin,logs,cache,media,.compile-cache}`、`lockfile`。

### 在线首次收编（store.rs::capture_current）

收编不关闭或重启 LobsterAI。持有 ProxyLock 后先验证 live 登录和重复账号，再在 staging 中用 SQLite Backup API 复制主库（包含当时已提交及活跃 WAL 中的数据，并做 `PRAGMA quick_check`）；其他 manifest 项逐项在线复制，跳过主库 `-wal/-shm`。这些文件不是一个跨文件事务，因此只保证 SQLite 副本自身一致，不承诺其他文件与数据库处于同一时刻。当前存在的 manifest 项除瞬态 `DIPS-wal`/`DIPS-shm` 外均为 required：复制失败会清理 staging 和新账号目录并使收编整体失败；两个 optional sidecar 失败只记 warnings。全部完成后将 staging 一次 rename 为 `snapshot`，元数据、版本及 refresh payload 均从已提交的快照数据库读取；live 全程只读、不移动。

### 切换流程（store.rs::switch_to）

切换与 sync_back 保留原有离线移动语义，仍必须先停止 LobsterAI；在线收编不改变这条隔离红线。

1. 目标 == 当前登录 → 仅 sync_back 保鲜；
2. LobsterAI 运行中 → 优雅关闭（`taskkill /PID` 不带 /F 给 WAL 落盘机会，15s 超时回退强杀，等全部 Electron 进程退出）；
3. ProxyLock 跨进程锁（数据目录侧 `.lobster-plus.lock`，30s 超时/180s 陈旧清理——目录拷贝耗时长于家族前作）；
4. **first_backup**：首次切换前把 live manifest 子集复制到 `backups\pre-first-switch-*\`（只做一次）；
5. **sync_back**：live manifest 路径全部移入当前登录所属账号的快照（对话/token 全量保鲜）；未入库登录自动收编（auto_preserve）；
6. **restore**：目标账号快照恢复到 live；快照缺项时 live 残留必须清除（串号防线）；
7. **读回校验**：live kv `auth_user.id` == 目标账号 user_id，失败 → 自动从备份回滚 live；
8. 可选重启客户端（settings.launch_after_switch，默认开）。

### 身份识别

`kv.auth_user.id`（用户 id）为主键；`auth_ref`（refreshToken SHA-256）兜底。

### 账号库结构 `~\.lobster-plus\`

```
accounts\<uuid>\account.json   元数据 + auth_blob（enc:v1 加密的
                               accessToken/refreshToken/keyfrom/uuid/version，
                               供签到引擎独立续期）
accounts\<uuid>\snapshot\      manifest 目录快照（对话记录副本）
backups\                       首次切换前全量备份（安全网，永不自动删）
proxy\                         lobster_proxy.py + config.json + proxy.log
                               + usage.sqlite（运行时可能有 -wal/-shm）
settings.json
```

## 二、签到引擎（checkin.rs）

三步接口（Bearer accessToken）：

```
GET  /api/client-activities/slot?placement=desktop_sidebar&clientVersion=…&containerApiVersion=2&platform=win32
     → data.activity.activityCode + configRevision（code 51102 = 登录失效）
GET  /api/client-activities/{code}/context?configRevision={rev}
     → data.state.claimedToday 预检
POST /api/client-activities/{code}/actions/check_in
     body {configRevision, idempotencyKey: uuid4, payload:{}}
     → code 0 成功（creditsGranted/claimedDays/claimedCredits），51104 已签（幂等）
```

token 策略：
- 活跃账号优先读 live sqlite（客户端自己续期的最新值），并回写 blob；
- blob 临期（<60s）→ `POST /api/auth/refresh`（带 firstKeyfrom/latestKeyfrom/uuid/userId/version 辅助字段）→ 回写 blob + 快照 sqlite（仅客户端退出时）；
- 单账号 1 小时节流防重试风暴。

调度：GUI 启动 catchup（对今天未签账号）+ 每小时循环；`schedule install` 装 Windows 计划任务每日 09:00 跑 `--cli checkin`。

## 三、API 稳定代理（proxy/lobster_proxy.py + proxy.rs）

**端口/key 轮换问题**：LobsterAI 本地模型代理（`lobsterai-model-compat`）的 baseUrl/apiKey 写在 openclaw 两处配置里，且实测互相过期（models.json 记 1316 时实际监听 2880）。历史观测端口：3248→3858→2880→1316。

**稳定器**（UpstreamManager，替换 raccoon 的 AuthManager）：
- 候选 = models.json 与 openclaw.json 两处 `(baseUrl, apiKey)`；
- 探活 = TCP 可连 + 最小 `POST /v1/chat/completions` 确认端点存在（上游无 `/v1/models`，HTTP 错误响应也表示服务在线）；
- 缓存 60s；连接失败自动 force 重探测并重试一次；全挂返回 503（LobsterAI 未运行）。

**Token usage**：Python 代理写 `~\.lobster-plus\proxy\usage.sqlite`（即脚本同目录；`LOBSTER_PLUS_HOME` 沙箱下随根目录变化），schema `user_version=2`，`usage_daily` 以 `(day, protocol, model)` 为主键，累计 `input/output/request/success/error/accurate/estimated/missing`，并可迁移旧 `daily_usage` 表。仅保留本地日聚合、协议和模型名，不保存请求/响应内容或密钥；统计写入失败不影响转发。`accurate` 表示上游响应 usage 同时给出输入/输出 token，缺任一字段时整次请求按本地规则记为 `estimated`；`missing` 是迁移未知旧来源或显式缺失记录的保留口径，正常采集主要使用前两类。

Rust `proxy_usage(range)` 只读该库并校验 `usage_daily` 列结构，数据库不存在时返回空报告；范围仅支持 `today`、`7d`、`30d`、`all`，返回总计、按日和按模型汇总。前端入口在代理卡片的“今日用量”摘要及“用量详情”按钮，详情还展示准确/估算/缺失请求数和日趋势。

**端点**（监听 127.0.0.1:19260，config 可改）：

| 路由 | 协议 | 处理 |
|---|---|---|
| POST /v1/chat/completions | OpenAI | 纯透传（模型名归一化） |
| GET /v1/models | OpenAI | 上游清单 + config 映射合并 |
| POST /v1/messages | Anthropic | → OpenAI 转换（fork raccoon 转换层：system/messages/tools/tool_result 互转 + SSE 事件发射器 + thinking） |
| POST /v1/messages/count_tokens | Anthropic | 估算 |
| GET /health | — | 状态 + 上游探测结果 |

模型映射：上游存在的 ID 直通；`claude-*` 前缀按 config models 映射（默认 opus→glm-5.3、sonnet/haiku→deepseek-flash）；兜底 default_model。

**CC-Switch 注册**（register.rs，移植 add_provider.py）：
备份 `~\.cc-switch\cc-switch.db` → upsert provider「LobsterPlus(本地)」（apiFormat=anthropic native、`ANTHROPIC_BASE_URL=http://127.0.0.1:19260`、模型全映射）→ 还原写入前激活项（`--switch` 才切换）。Python 发现三级：PATH → `%APPDATA%\LobsterAI\runtimes\python-win\` → py.exe；候选必须能实际启动，不能把 Windows Store App Execution Alias 当成真实解释器。

## 四、安全红线

1. live `lobsterai.sqlite` 只读；唯一写路径 = 客户端已退出时的 token 续期回写（且走快照 sqlite，不碰 live）。
2. 切换必过读回校验，失败自动回滚（backups 安全网）。
3. 删账号仅删 account.json + snapshot/，不动 live；活跃账号禁止删除。
4. CLI/JSON 输出经 `account_public` 过滤，无任何密钥。
5. 沙箱：`LOBSTER_PLUS_HOME` 重定向账号库且不杀进程（单测/E2E 用）。

## 五、构建

- 本机固定 GNU 工具链（无 VS，且 `C:\Program Files\Git\usr\bin\link.exe` 会抢占 MSVC 链接器、让 build script 全部链接失败）：`scripts\build-release.cmd`（`RUSTUP_TOOLCHAIN=stable-x86_64-pc-windows-gnu` + `PATH=D:\mingw64\bin` + `--features tauri/custom-protocol` 嵌入前端）。
- `custom-protocol` 是 release 必需项：tauri build.rs 以 `dev = !has_feature("custom-protocol")` 判定模式，缺它则不嵌入 `frontendDist`，WebView 改连 `devUrl`（127.0.0.1:5177），vite 未启动即 `chrome-error://chromewebdata/` 白屏；Vite 另需 `base: "./"`。
- `[lib]` 只留 `rlib`（cdylib 超 MinGW ld 65535 符号上限）。
- 单测：`cargo test --manifest-path src-tauri\Cargo.toml --lib --no-default-features`（含 Rust usage 只读查询；gui 层不进测试二进制）。
- Python 用量存储自检（不访问真实上游）：`"%APPDATA%\LobsterAI\runtimes\python-win\python.exe" src-tauri\proxy\lobster_proxy.py --selftest-storage`（schema/v1 迁移/WAL/聚合/失败/并发写入；本机 PATH 的 `python` 是 Windows Store alias，不可用）。
- 发布：`npx tauri build --target x86_64-pc-windows-gnu`（需 NSIS）→ `bundle\nsis\LobsterPlus_<版本>_x64-setup.exe`。**只能发安装包**：exe 依赖同目录 `WebView2Loader.dll`，裸发会在未装 WebView2 运行时的机器上启动失败（v0.2.0 教训）。
- 版本号需同步四处：`package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`、`src-tauri/Cargo.lock`。

## 六、E2E 验收清单

1. `capture` 收编账号 A → 切换往返 A↔B → 两边对话记录（cowork_sessions 行数）完好；
2. 全库签到（每个账号 today 状态 ok/done）；
3. proxy start → `/health` 上游探测成功 → `/v1/messages` 与 `/v1/chat/completions` 各 curl 一次（含 SSE）；
4. 上述请求后 `usage.sqlite` 按协议/模型只累计一次，GUI 今日摘要与 7d/30d/all 详情一致，数据库不含请求正文或密钥；
5. register → CC-Switch 出现 provider → Claude Code 实际对话一次；
6. `schedule install` → schtasks 查询确认 → 次日 09:00 日志有签到记录。
