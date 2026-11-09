# 播放诊断日志

设置 → 播放诊断 → **播放诊断日志**。默认关闭，开关在本机持久化，发布版也可使用。

## 复现与取日志

1. 开启日志。如需排查启动时的后台播放，开启后退出并重新启动应用。
2. 依次复现：播放首页视频、切到其他页面播放、点击进度条、拖动进度条。点击/松手后等待画面恢复；若一直卡住，至少等待 10 秒。
3. 回到设置关闭日志，点击 **复制播放诊断日志**。关闭后已有日志保留。

macOS 文件位于 `~/Library/Application Support/com.hghub.huangguo/`：

- `playback.log`：当前诊断日志，JSON Lines 格式。
- `playback.previous.log`：轮转前的日志，若存在，手动取文件时请一并提供。
- `playback-diagnostics.json`：开关配置，与 `security.json` 独立。

复制按钮包含当前与上一份日志。每份上限 2 MiB，总计约 4 MiB，自动轮转。日志只存本机；媒体地址用跨前后端一致的资源编号替代，不记录播放直链、查询参数、密钥内容、账号凭据或响应正文。原有 `debug.log` 仍用于应用异常与旧自检，不受此开关控制。

## 如何读记录

每行包含 UTC `ts`、进程 `pid`、递增 `seq`、当时生效的安全模式 `strict`、`event` 与 `data`。前端记录还包含采集时刻 `data.at`、单调时钟 `mono_ms`、页面会话 `session`、播放器 `player`、页面来源、视频/集数及激活状态。前端约每 250ms 批量写入，分析事件先后顺序使用 `data.at/mono_ms`，不只看落盘时间。

`debug.log` 新写入的所有记录也使用 RFC 3339 UTC（`Z` 结尾），格式为 `[2026-11-08T14:30:00.123Z] [pid=12345] [api] ...`。时间由后端统一在写入时生成，覆盖 API 错误、运行异常、未处理的 Promise 拒绝、网络自检和播放自检；多行正文/堆栈的每一行带同一个时间和进程编号，同一条记录的各行连续写入。`playback.log` 的后端 `ts` 与前端 `data.at` 同样都是 RFC 3339 UTC，精度可能不同；`mono_ms`、`elapsed_ms` 等为相对耗时，继续保留毫秒数值。已有无时间戳的历史记录保留原样，无法补出真实发生时间。

点击、拖动和键盘定位会记录：

| 事件 | 含义 |
| --- | --- |
| `web.seek.input.down/change/up/cancel` | 输入操作，包括 click/drag/keyboard、目标时间；点击同一位置即使没有 change，也保留按下/松开 |
| `web.seek.request` | 真正发起定位，包含播放器内递增的 `seek` 编号、目标时间和来源 |
| `web.seek.assigned` / `web.seek.rejected` | 写入 currentTime 后的状态，或写入失败 |
| `web.media.seeking` / `web.media.seeked` | 媒体元素确认正在定位/定位结束及耗时 |
| `web.seek.resumed` | 播放中定位后的首帧回调；不支持首帧 API 时标记为 playing 事件回退 |
| `web.seek.ready` | 暂停定位完成、数据已就绪；保持暂停，不把它记作恢复播放 |
| `web.seek.superseded` | 之前的定位被下一次拖动/点击替代 |
| `web.seek.noop` | 目标与当前时间相同，无需等待 seeking 事件 |
| `web.seek.timeout` | 8 秒后仍未完成，包含目标、实际时间、缓冲/可定位范围、readyState、paused、seeking 和 HLS 加载/缓冲开关 |

使用 `(pid, session, player, seek)` 关联一次操作。拖动期间的 `seek.input.change` 记录预览位置，松手后才发起一次 `seek.request` 和 currentTime 写入；点击在松开时定位，键盘定位立即执行。取消拖动或切到其他页面会丢弃预览，不发起跳转。播放器每 5 秒记录状态快照，非激活页面仍播放时可看到 `active: false` 与 `paused: false`。

播放器在页面失活时暂停并停止 HLS 分片加载；清单稍后返回时也必须重新检查活动状态。重新进入页面时，之前正在播放的内容可继续播放；手动暂停的内容保持暂停。共用播放页与吃瓜详情的独立播放器均遵循此规则。

hls.js 固定为 **1.6.19**，包含 [macOS/iOS 27 ManagedMediaSource 跳转恢复缓冲的上游修复](https://github.com/video-dev/hls.js/pull/7984)。两个播放器均显式配置 `preferManagedMediaSource: false`，在标准 MediaSource 可用时优先使用它；只有 ManagedMediaSource 的环境仍可回退，并由升级后的库在 seeking 时恢复缓冲。诊断日志中的 `hls_version`、`engine` 和 `prefer_managed` 可确认实际运行版本和路径。

`web.hls.attached`/状态快照记录 MediaSource 类型与偏好；`web.mms.startstreaming/endstreaming`、分片加载/缓冲、非致命和致命 HLS 错误也会记录。`protocol.request` 包含原请求的 Range 和 Origin 分类；`proxy.begin/end` 与 `network.begin/end` 通过 `resource` 和请求编号关联代理耗时、HTTP 状态、字节数与每次重试。`network.target_failed`、`security.origin_denied`、`web.security.csp_violation` 用于识别安全拒绝，不需要关闭安全保护。

业务接口也受同一个日志开关控制：`api.begin/end` 记录请求编号 `request`、每次尝试 `attempt`、操作类型 `operation`（首页/分类/播放地址等）、逻辑方法、所选线路 `line_host`、重定向后目标 `target_host`、HTTP 状态、响应头/总耗时、响应类型与网关类别。`business_status` 是 HTTP 200 后返回的业务状态；`retryable` 表示当前逻辑请求及错误是否允许重试。通过 `(pid, request, attempt)` 关联一次尝试，结合 `line.changed` 和每行的 `strict` 判断是否与切换线路或安全模式有关。只记录主机名和固定操作名，不记录搜索词、账号、内容 ID、请求体、凭据或响应正文。旧版只有 `[api]` 错误的 `debug.log` 无法事后补出请求时间和线路。

HTTP 404/其他不可重试的 4xx、权限和安全拒绝直接结束；超时、408/425/429 和 5xx 的逻辑只读请求仍最多尝试三次。每次请求保留原有 20 秒超时，因此持续 5xx/超时时仍可能等待较久；日志会逐次记录。首页失败后显示重试入口；分类首次打开才请求预览，各频道和帖子独立更新、独立报错及重试，避免一个慢请求拖住整页。分片计时尚未就绪时记录 `null`，不再出现负耗时。

可运行 `pnpm test:api-loading` 注入首页 404、频道 503、慢频道和空结果。测试使用真实页面及 API 错误处理，替换 Tauri IPC，阻止所有外部请求；检查首页停止转圈并可重试、成功频道与帖子提前显示、失败频道单独重试，以及未打开分类时不预取。结果保存到 `test-results/api-loading/<UTC时间>/`。Rust 网络层另行测试 404/权限/安全拒绝只尝试一次，503/超时可以重试恢复。

## 本地验证

使用 [进度条回归的媒体样本与服务器](playback-seek.md)，打开 `http://127.0.0.1:1420/tests/player-seek.html?diagnostics=1`。该模式使用真实 Feed、PlayerControls、VideoSurface 和 hls.js，日志传输替换为内存；“诊断设置”按钮提供真实设置界面。控制台的 `window.playerSeekFixture.logs` 可查看记录，`flushLogs()` 可立即刷新批次。

验证默认关闭时点击不记录；通过设置开启后，实际鼠标点击和拖动应分别产生 `desktop_click`、`desktop_drag`，并串联 request、seeking、seeked、首帧恢复。拖动时有多个预览记录但没有实际定位，松手后恰好定位一次。暂停定位和键盘定位应产生 ready 且保持暂停；关闭后不再增加记录；保存失败保持原状态。前后端类型检查、Rust 日志脱敏/轮转/保存失败测试也随构建检查。

追加 `&legacy=1` 可以检查吃瓜详情页使用的独立播放器。

本地 Chromium 验证不替代 macOS WKWebView 实机复现。

## 无头服务器自动采集

需要 Node.js 22+、pnpm、FFmpeg（含 libx264/AAC）及 Chromium。首次准备：

```sh
pnpm install --frozen-lockfile
pnpm exec playwright-core install --with-deps chromium
```

Ubuntu 上可用 `sudo apt-get install ffmpeg` 安装媒体样本生成器，然后运行：

```sh
pnpm test:playback
```

命令自动生成 90 秒 HLS 样本、启动仅监听本机随机端口的 Vite/媒体服务、操作无头浏览器，并在结束后关闭服务、浏览器及删除临时媒体。不需要显示器、桌面会话、账号或播放直链。测试中的外部网络请求会被拦截并导致失败。

每次运行保存到新的 `test-results/playback/<UTC时间>/` 目录，包含：

- `report.json`：平台、浏览器版本、每个检查的结果、点击耗时、拖动请求数及失败原因。
- 各场景的 `playback.jsonl`：边操作边落盘的播放器诊断日志，采集不依赖手动复制或测试成功结束。
- `network.jsonl`、`console.jsonl`：浏览器请求状态、控制台和脚本错误。
- `final.png`、`trace.zip`：结束/失败时的截图，以及可回放的操作追踪。
- `media-server.jsonl`：本地分片服务器的耗时、字节数与取消情况。

可用 `pnpm exec playwright-core show-trace <trace.zip路径>` 打开追踪。输出目录已经加入 gitignore；文件保存在执行命令的服务器，不会自动上传。

默认检查六个场景：

| 场景 | 检查内容 |
| --- | --- |
| `seek` | 共用播放器的真实鼠标点击/拖动、拖动松手后仅定位一次、首帧恢复、暂停/键盘定位、失活取消拖动、开关关闭和保存失败 |
| `retry` | 首次获取播放地址返回 404，点击重试重新取址并成功播放 |
| `legacy` | 吃瓜详情独立播放器的相同检查 |
| `stall` | 人为停止 HLS 缓冲后点击未缓冲位置，检查 8 秒超时日志以及恢复缓冲后的首帧；属于故障注入 |
| `lifecycle` | 两个播放器延迟清单后失活、后台禁止自动播放和分片加载、重新激活后恢复、手动暂停状态保留，以及视频延迟挂载时保持失活 |
| `mms` | 两个播放器在两种 API 同时存在时优先使用标准 MSE；在模拟 MMS-only 环境中，endstreaming 停止缓冲后点击未缓冲位置，即使没有 startstreaming 事件也能自动恢复并输出首帧 |

任何检查失败，命令退出码为 1，仍保留日志和报告。不要把日志采集通过理解成所有播放问题已修复。只运行诊断回归可以选择：

```sh
pnpm test:playback --scenarios seek,legacy,stall
```

仅验证 hls.js 的 MMS 兼容修复可运行 `pnpm test:playback --scenarios mms`。该场景由 Chromium 的真实 MSE 完成解码和缓冲，仅模拟 MMS 构造器与 streaming 事件；报告标记 `simulated_mms: true`，不能替代 macOS 27 实机确认。

其他参数：`--media-dir DIR` 复用已有 `list.m3u8` 与 `seg*.ts`；`--browser-executable PATH` 使用已安装的匹配版本 Chromium；`--output NEW_DIR` 指定新的产物目录。`FFMPEG_PATH` 可指定 FFmpeg 可执行文件。命令拒绝覆盖已经存在的输出目录。

仓库附带手动触发的 GitHub Actions 工作流 **Playback diagnostics**。它会在检查失败时仍上传日志、截图与追踪，保留 7 天；不会随推送自动运行。

**覆盖边界：** 这些测试使用真实 React 控件、video、hls.js 与本地 HTTP 分片，但业务 API 和 Tauri 日志 IPC 使用替身。它们不执行真实 Rust 媒体代理，不验证真实 CDN、代理/TUN、Tauri 的 CSP/Origin/DNS 策略，也不能证明 macOS 27 的 ManagedMediaSource 问题已复现或修复。Rust 日志持久化/轮转/脱敏仍由本地 Rust 回归检查；macOS 特有问题使用实机 `playback.log` 确认。
