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
│  ├ register.rs  ★CC-Switch 数据库注册（备份→upsert→还原）   │
│  ├ zcrypto.rs   enc:v1 AES-256-GCM 账号凭据加密（家族复用）  │
│  ├ lockfile.rs  ProxyLock 跨进程锁（家族复用）              │
│  └ cli.rs       CLI 全功能镜像 + doctor + schtasks          │
│                                                            │
│  src-tauri/proxy/lobster_proxy.py                          │
│    ★上游稳定器（探测轮换端口/key）                          │
│    + Anthropic↔OpenAI 转换层（fork raccoon_proxy.py）       │
│    + OpenAI 纯透传 + SSE 流式                               │
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

所以账号隔离的唯一安全路径 = 账号私有路径在 live 与快照间整体搬移。

### manifest（store 的账号私有路径，相对 `%APPDATA%\LobsterAI`）

**包含**：`lobsterai.sqlite(+wal/-shm)`、`Network`、`Local Storage`、`Session Storage`、`Shared Dictionary`、`Shared Storage`、`Partitions`、`Preferences`、`Local State`、`DIPS*`、`openclaw\state`（openclaw.json + gateway 凭据 + agents 整树含 openclaw-agent.sqlite）、`openclaw\.openclaw\exec-approvals.json`、`openclaw\plugin-skills`、`SKILLs`。

**排除**（可重建/静态，永远留在 live）：`Cache`、`Code Cache`、`GPUCache`、`Dawn*Cache`、`blob_storage`、`logs`、`updates`（298MB）、`runtimes`、`cowork`、`openclaw\{bin,logs,cache,media,.compile-cache}`、`lockfile`。

### 切换流程（store.rs::switch_to）

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
- 探活 = TCP 可连 + `GET /v1/models` 鉴权通过；
- 缓存 60s；请求失败自动 force 重探测；全挂返回 503（LobsterAI 未运行）。

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
备份 `~\.cc-switch\cc-switch.db` → upsert provider「LobsterPlus(本地)」（apiFormat=anthropic native、`ANTHROPIC_BASE_URL=http://127.0.0.1:19260`、模型全映射）→ 还原写入前激活项（`--switch` 才切换）。Python 发现三级：PATH → `%APPDATA%\LobsterAI\runtimes\python-win\` → py.exe。

## 四、安全红线

1. live `lobsterai.sqlite` 只读；唯一写路径 = 客户端已退出时的 token 续期回写（且走快照 sqlite，不碰 live）。
2. 切换必过读回校验，失败自动回滚（backups 安全网）。
3. 删账号仅删 account.json + snapshot/，不动 live；活跃账号禁止删除。
4. CLI/JSON 输出经 `account_public` 过滤，无任何密钥。
5. 沙箱：`LOBSTER_PLUS_HOME` 重定向账号库且不杀进程（单测/E2E 用）。

## 五、构建

- 本机固定 GNU 工具链（MSVC 残缺）：`scripts\build-release.cmd`（PATH 前置 `D:\mingw64\bin` + `--features tauri/custom-protocol` 嵌入前端）。
- `[lib]` 只留 `rlib`（cdylib 超 MinGW ld 65535 符号上限）。
- 单测：`cargo test --manifest-path src-tauri\Cargo.toml`（manifest 交换/kvdb/zcrypto/lockfile/proxy 内嵌）。

## 六、E2E 验收清单

1. `capture` 收编账号 A → 切换往返 A↔B → 两边对话记录（cowork_sessions 行数）完好；
2. 全库签到（每个账号 today 状态 ok/done）；
3. proxy start → `/health` 上游探测成功 → `/v1/messages` 与 `/v1/chat/completions` 各 curl 一次（含 SSE）；
4. register → CC-Switch 出现 provider → Claude Code 实际对话一次；
5. `schedule install` → schtasks 查询确认 → 次日 09:00 日志有签到记录。
