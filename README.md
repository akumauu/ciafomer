# Ciallo — 语音唤醒 + 多模式翻译桌面助手

> 本地部署、低延迟、高性能的桌面翻译工具。语音唤醒后即时响应，支持选中文本翻译、区域 OCR 翻译、实时增量翻译三种模式。

---

## 目录

- [项目概述](#项目概述)
- [技术架构](#技术架构)
- [功能清单](#功能清单)
- [目录结构](#目录结构)
- [核心模块详解](#核心模块详解)
- [性能指标 (KPI)](#性能指标-kpi)
- [开发环境搭建](#开发环境搭建)
- [构建与运行](#构建与运行)
- [开发阶段记录](#开发阶段记录)
- [实现与未实现清单](#实现与未实现清单)
- [配置说明](#配置说明)
- [技术决策与假设](#技术决策与假设)

---

## 项目概述

Ciallo 是一个纯本地部署的桌面翻译助手，目标是以最低的资源占用和最快的响应速度提供翻译服务。

**核心特性：**
- 后台低功耗语音唤醒，命中后即时 UI/音效反馈
- 三种翻译模式：选中文本翻译、区域 OCR 翻译（可选）、实时增量翻译
- DeepSeek API 翻译（带重试、限流、缓存、可选流式）
- 术语表注入、历史记录持久化、复制译文、原文/译文悬浮窗

**设计原则：**
- 不引入与需求无关的新功能、新服务、新协议
- 平台 P0: Windows 11 x64，P1: Linux/macOS
- 所有平台差异通过 trait/adapter 隔离，不污染核心逻辑

---

## 技术架构

### 技术栈

| 层级 | 技术 | 说明 |
|------|------|------|
| **主进程** | Rust + Tauri v2 | 状态机、调度器、取消框架、音频管道 |
| **前端** | TypeScript + CSS | Tauri WebView，禁止额外 UI 框架 |
| **OCR Worker** | Python (PaddleOCR + OpenCV) | 独立进程，延迟加载，idle 自动卸载 |
| **IPC** | Named Pipe / Unix Socket + MessagePack | 主进程 ↔ Python Worker |
| **翻译** | DeepSeek chat/completions | reqwest 连接池 |
| **存储** | SQLite + 内存 LRU | 二级缓存 |
| **构建** | esbuild (前端) + cargo (后端) | 最小构建链 |

### 架构总览

```
┌─────────────────────────────────────────────────────┐
│                    Tauri v2 App                      │
│                                                     │
│  ┌──────────┐   ┌──────────────┐   ┌─────────────┐ │
│  │ Audio    │   │ State Machine│   │ Scheduler   │ │
│  │ Pipeline │──>│ (8 states)   │──>│ P0/P1/P2    │ │
│  └──────────┘   └──────────────┘   └─────────────┘ │
│       │              │                   │          │
│  ┌────▼────┐   ┌─────▼─────┐     ┌──────▼──────┐  │
│  │RingBuf  │   │Cancel     │     │ P0: Wake/UI │  │
│  │VAD→Wake │   │Coordinator│     │ P1: Cap/Trl │  │
│  └─────────┘   └───────────┘     │ P2: OCR     │  │
│                                  └─────────────┘  │
│  ┌────────────────────────────────────────────┐    │
│  │              WebView (TS/CSS)               │    │
│  │  main window ◄──events──► mode-panel       │    │
│  └────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────┘
          │ IPC (MessagePack)
    ┌─────▼─────┐
    │ Python    │
    │ OCR       │
    │ Worker    │
    └───────────┘
```

### 状态机

```
Sleep ──► WakeConfirm ──► ModeSelect ──► Capture ──► OCR? ──► Translate ──► Render ──► Idle
  ▲          │ (fail)        │                                                         │
  └──────────┘               └─────────────────── cancel/timeout ─────────────────────►┘
```

**两阶段唤醒确认：**
1. **阶段 1**：`th_low` 命中 → 即刻发 UI/音效反馈（用户无感知延迟）
2. **阶段 2**：150ms 内累计确认达到 `th_high`，失败回退 Sleep + 轻提示

### 三队列调度

| 队列 | 优先级 | 实现 | 用途 |
|------|--------|------|------|
| **P0** | 最高 | crossbeam 无界 channel + 专用 OS 线程 | Wake/UI/音效，<1ms 处理 |
| **P1** | 中 | tokio mpsc(64) | Capture/Translate/Render |
| **P2** | 低 | tokio mpsc(16) + spawn_blocking | OCR 重计算 |

**抢占规则：**
- 新 Wake 到来：取消 P1/P2 所有可取消任务
- 同模式新请求：取消旧请求（generation 防止旧结果回写 UI）
- 用户退出/超时：全链路取消回 Sleep

### Wake 路径禁止项

P0 通道处理**严禁**：
- 网络 I/O
- OCR / 大模型推理
- 磁盘同步写
- 任何 >1ms 的计算

---

## 功能清单

### 已实现 (Phase 1)

| 功能 | 文件 | 说明 |
|------|------|------|
| 状态机 (8 状态) | `state_machine.rs` | Sleep→WakeConfirm→ModeSelect→Capture→OCR→Translate→Render→Idle，带验证的状态转移 |
| 两阶段唤醒确认 | `state_machine.rs` → `WakeConfirmer` | th_low/th_high 双阈值，150ms 确认窗口 |
| 三队列调度器 | `scheduler.rs` | P0(crossbeam unbounded) / P1(tokio mpsc 64) / P2(tokio mpsc 16) |
| P0 专用线程 | `scheduler.rs` → `run_p0_loop` | 独立 OS 线程处理 wake/UI/sound，不受 Tokio 调度影响 |
| 取消框架 | `cancellation.rs` | TaskGeneration + GenerationGuard + CancelCoordinator |
| 端到端取消 | `cancellation.rs` → `cancel_and_advance()` | cancel 当前 token → 创建新 root → 推进 generation |
| Generation Guard | `cancellation.rs` → `GenerationGuard` | `is_current()` / `should_continue()` 防止旧结果回写 UI |
| 音频环形缓冲区 | `audio/ring_buffer.rs` | 固定预分配 3s PCM(16kHz)，无动态扩容 |
| 能量门控 VAD | `audio/vad.rs` | RMS 能量检测 + 连续静音计数 + 降频推理(1/4) |
| 唤醒检测管道 | `audio/mod.rs` + `audio/wake.rs` | cpal capture → RingBuffer → VAD → Wake → P0 channel |
| Wake→UI 反馈 | `scheduler.rs` P0 handler | emit("wake-detected") → emit("wake-confirmed") → show mode-panel |
| 模式面板 (预创建) | `tauri.conf.json` + `mode-panel.html` | hidden + transparent + alwaysOnTop，唤醒时 show + focus |
| 前端状态同步 | `main.ts` | listen wake-detected/confirmed/rejected 事件，更新 UI |
| WebAudio 音效 | `main.ts` | wake: 880Hz sine 150ms / reject: 330Hz sine 100ms |
| 可观测性框架 | `metrics.rs` | SampleRing(1024) histogram, p50/p95/p99, 12 个命名指标 |
| Timing Span | `metrics.rs` → `TimingSpan` | 自动计时并记录到 histogram |
| trace_id/request_id | `metrics.rs` → `RequestIds` | 每请求唯一标识 (UUID v4) |
| Tauri 命令 | `lib.rs` | get_state / get_metrics_summary / select_mode / cancel_current / dismiss |
| Python OCR Worker | `python-worker/worker.py` | PaddleOCR 延迟加载，idle≥60s 卸载，msgpack 帧协议 |
| 术语表模板 | `glossary/default.json` | JSON 格式 source→target 映射 |

### 未实现（Phase 2-5 计划）

| 功能 | Phase | 说明 |
|------|-------|------|
| 选中文本采集 | Phase 2 | accessibility API(超时 50-80ms) → clipboard fallback(backup→Ctrl+C→read→finally restore) |
| DeepSeek API 翻译 | Phase 2 | reqwest 连接池(2-8)，system prompt ≤60 tokens，紧凑 JSON(t/g) |
| 本地令牌桶限流 | Phase 2 | 先限流后请求 |
| 重试策略 | Phase 2 | 429: Retry-After/1s/2s/4s(3次)，5xx: 指数退避(2次)，timeout: 立即1次 |
| 翻译缓存 L1 | Phase 2 | 内存 LRU(512, TTL 10m)，key=blake3(src\|tgt\|glossary_ver\|normalized_text) |
| 翻译缓存 L2 | Phase 5 | SQLite(TTL 7d) |
| 术语表注入 | Phase 2 | 仅注入命中项，不全量发送 |
| 语言检测 | Phase 2 | 本地规范化先行 |
| 占位符保护 | Phase 2 | 数字/单位/URL/邮箱/代码 |
| max_tokens 动态估算 | Phase 2 | (input_tokens*1.15 + 32)，上限 768 |
| 流式渲染 | Phase 2 | 增量 append，禁止全量重排 |
| 原文/译文悬浮窗 | Phase 2 | 浮动窗口显示翻译结果 |
| 复制译文 | Phase 2 | 一键复制到剪贴板 |
| OCR Worker IPC | Phase 3 | Named Pipe(Win) / Unix Socket + MessagePack |
| ROI 预处理 | Phase 3 | 灰度、自适应二值化、降噪、可选 deskew |
| 矩形/多边形/透视 OCR | Phase 3 | rect crop / polygon mask / warpPerspective |
| Worker 健康检查 | Phase 3 | 30s ping，500ms pong，3次失败重启 |
| 实时增量翻译 | Phase 4 | 500ms 采样 + 像素差分(MAE/SSIM) 变化检测 |
| 行级 diff | Phase 4 | line-hash(text + y_bucket 8px)，仅 added lines 进翻译 |
| 行级缓存 | Phase 4 | 不变行复用缓存，字幕 60s 不变 API≤1 次 |
| 历史记录批量写 | Phase 5 | 异步 300ms flush，不阻塞渲染 |
| 连接池优化 | Phase 5 | reqwest keep-alive 长连接 |
| 稳定性打磨 | Phase 5 | 全面性能测试，KPI 达标验证 |

---

## 目录结构

```
ciallo/
├── src-tauri/                     # Rust 后端 (Tauri v2)
│   ├── Cargo.toml                 # Rust 依赖定义
│   ├── build.rs                   # Tauri 构建钩子
│   ├── tauri.conf.json            # Tauri 配置 (窗口/权限/构建)
│   ├── capabilities/
│   │   └── default.json           # Tauri v2 权限声明
│   ├── icons/                     # 应用图标 (RGBA PNG)
│   │   ├── icon.png
│   │   ├── 32x32.png
│   │   ├── 128x128.png
│   │   └── 128x128@2x.png
│   └── src/
│       ├── main.rs                # 入口点
│       ├── lib.rs                 # App 初始化 + Tauri 命令注册
│       ├── state_machine.rs       # 8 状态 FSM + 两阶段唤醒确认
│       ├── scheduler.rs           # 三队列调度 (P0/P1/P2) + P0 handler
│       ├── cancellation.rs        # CancellationToken + GenerationGuard
│       ├── metrics.rs             # 可观测性: histogram, timing span
│       ├── audio/
│       │   ├── mod.rs             # 音频管道: cpal → RingBuffer → VAD → Wake
│       │   ├── ring_buffer.rs     # 固定预分配环形缓冲区 (3s)
│       │   ├── vad.rs             # RMS 能量 VAD + 降频推理
│       │   └── wake.rs            # WakeDetector trait + 能量模式检测
│       ├── capture/
│       │   └── mod.rs             # TextCapture trait + TextPacket (Phase 2)
│       ├── ocr/
│       │   └── mod.rs             # OcrWorkerClient trait + 类型定义 (Phase 3)
│       └── translate/
│           └── mod.rs             # Translator trait + 类型定义 (Phase 2)
├── src/                           # 前端 (TypeScript + CSS)
│   ├── index.html                 # 主窗口 HTML
│   ├── mode-panel.html            # 模式选择面板 HTML
│   ├── main.ts                    # 主窗口逻辑 (事件监听/状态/音效)
│   ├── mode-panel.ts              # 模式面板逻辑 (invoke 命令)
│   └── style.css                  # 暗色主题样式
├── python-worker/                 # Python OCR Worker (可选)
│   ├── worker.py                  # PaddleOCR 进程 (lazy load, msgpack)
│   ├── requirements.txt           # Python 依赖
│   └── .venv/                     # Python 虚拟环境
├── glossary/
│   └── default.json               # 术语表
├── scripts/
│   └── dev.sh                     # 开发环境一键搭建
├── build.mjs                      # esbuild 前端构建脚本
├── package.json                   # Node 依赖 + 构建命令
├── tsconfig.json                  # TypeScript 配置
└── .gitignore
```

---

## 核心模块详解

### 1. 状态机 (`state_machine.rs`)

管理应用的全局状态流转。所有状态转移都经过 `can_transition_to()` 验证，非法转移会被拒绝并记录日志。

**关键类型：**
- `AppState` — 8 个枚举值：Sleep, WakeConfirm, ModeSelect, Capture, Ocr, Translate, Render, Idle
- `TranslateMode` — 3 种翻译模式：Selection, OcrRegion, RealtimeIncremental
- `StateMachine` — 线程安全 FSM，使用 `parking_lot::RwLock` + `tokio::sync::watch` channel
- `WakeConfirmer` — 两阶段唤醒参数：`th_low=0.02, th_high=0.04, confirm_window=150ms, confirm_frames_needed=2`

**状态订阅：** 任何模块可通过 `state_machine.subscribe()` 获取 `watch::Receiver<AppState>` 响应式监听状态变化。

### 2. 调度器 (`scheduler.rs`)

三优先级队列调度器，确保 Wake/UI 事件的绝对优先。

**P0 通道设计要点：**
- crossbeam 无界 channel — 发送永不阻塞
- 专用 OS 线程 (`p0-handler`) — 不受 Tokio 运行时调度延迟影响
- 处理逻辑严格 <1ms：仅做事件通知、窗口 show/hide、音效触发

**抢占实现：** `preempt_for_wake()` → `CancelCoordinator.cancel_all_and_advance()` → 所有 P1/P2 持有的 CancellationToken 被取消。

### 3. 取消框架 (`cancellation.rs`)

端到端取消机制，确保旧任务不会污染新结果。

**核心概念：**
- `TaskGeneration` — 每次 `cancel_and_advance()` 原子递增 generation，取消当前 token，发放新 token
- `GenerationGuard` — 任务持有，执行过程中调用 `should_continue()` 检查是否被取消或过期
- `CancelCoordinator` — 分别管理 P1 和 P2 的 TaskGeneration，支持 `cancel_all_and_advance()` 全局取消

**验收标准：** 连续触发 10 次新请求，UI 只能出现最后一次结果。

### 4. 音频管道 (`audio/`)

三级级联过滤：RMS 能量门控 → VAD → 唤醒推理。

**ring_buffer.rs：**
- 固定预分配 `Box<[i16]>`（16kHz × 3s = 48000 samples ≈ 96KB）
- 写入在 cpal 音频回调中，仅一次 `lock()` + memcpy，无堆分配

**vad.rs：**
- `compute_rms()` — O(n) 能量计算
- `EnergyVad` — 连续静音帧计数，8 帧后判定无声
- **降频优化**：VAD=false 时唤醒推理降至 1/4 频率

**wake.rs：**
- `WakeDetector` trait — 可插拔替换为真实 keyword-spotting 模型
- `EnergyPatternDetector` — 当前实现：检测能量突升（spike_ratio > 3x）

**mod.rs（管道协调）：**
- cpal 回调线程 → `SharedAudioState.ring_buffer` (Mutex)
- 处理线程 50Hz 循环：read frame → VAD → Wake → P0 channel

### 5. 可观测性 (`metrics.rs`)

每个请求必须携带 `trace_id, request_id, generation`。所有关键路径有埋点。

**Histogram 实现：**
- `SampleRing(1024)` — 固定容量环形缓冲，无动态扩容
- 按需计算 percentile：排序后取 `p * (n-1)` 位置

**12 个命名指标：**
```
t_wake_detected, t_wake_ui_emitted, t_mode_panel_visible,
t_capture_done, t_ocr_done, t_translate_first_chunk,
t_translate_done, t_render_done,
queue_wait_p0, queue_wait_p1, queue_wait_p2,
cancel_latency
```

**Tauri 命令 `get_metrics_summary`** 可随时查询所有指标的 p50/p95/p99。

### 6. 前端 (`src/`)

纯 TypeScript + CSS，无框架依赖。通过 `window.__TAURI__` 全局对象与后端通信。

**main.ts：** 监听 `wake-detected/confirmed/rejected/force-cancel/play-sound` 事件，驱动状态指示器和音效。

**mode-panel.ts：** 三个模式按钮（Selection / OCR Region / Realtime）+ Cancel，通过 `invoke('select_mode', { mode })` 通知后端。

**style.css：** 暗色主题 (`#1a1a2e`)，脉冲动画反馈，按钮悬停/按下状态。

### 7. Python OCR Worker (`python-worker/`)

独立进程，当前为 Phase 3 预留。

**设计要点：**
- PaddleOCR 延迟加载，idle ≥ 60s 自动卸载模型释放内存
- MessagePack 帧协议（4字节大端长度前缀 + msgpack payload），禁止 base64 JSON
- 支持 ping/pong 健康检查、ocr 任务、shutdown 命令

---

## 性能指标 (KPI)

| 指标 | 目标 | 当前状态 |
|------|------|---------|
| 唤醒反馈 (Wake→UI) | p95 < 250ms, p99 < 400ms | 框架就绪，埋点已实现 |
| 模式面板出现 | p95 < 300ms, p99 < 500ms | 预创建 hidden + show，埋点就绪 |
| 选中翻译首条译文 | p95 < 800ms, p99 < 1.2s | Phase 2 |
| OCR 翻译首条译文 | p95 < 1.2s, p99 < 2.0s | Phase 3 (OCR 可选) |
| 高优任务排队等待 | p95 < 80ms, p99 < 120ms | crossbeam 无界 + 专用线程 |
| 待机 CPU | < 2% | 预期达标 (sleep loop) |
| 待机内存 | < 200MB | 预期达标 (~96KB ring buffer) |
| Token 节省 (增量 vs 全量) | >= 40% | Phase 4 |

---

## 开发环境搭建

### 前置要求

- **WSL2 (Ubuntu 24.04)** 或 Linux x64
- **Rust** >= 1.70 (通过 rustup 安装)
- **Node.js** >= 18 (推荐 22)
- **Python** >= 3.10 (用于 OCR Worker，可选)

### 一键搭建

```bash
# 1. 安装系统依赖 (Ubuntu/Debian)
sudo apt-get install -y \
  build-essential pkg-config libssl-dev \
  libwebkit2gtk-4.1-dev libayatana-appindicator3-dev \
  librsvg2-dev libgtk-3-dev libsoup-3.0-dev \
  libjavascriptcoregtk-4.1-dev libasound2-dev

# 2. 安装 Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env

# 3. 安装 Node.js (NodeSource)
curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash -
sudo apt-get install -y nodejs

# 4. 项目依赖
cd /path/to/ciallo
npm install

# 5. Python 虚拟环境 (可选，OCR 用)
cd python-worker
python3 -m venv .venv
source .venv/bin/activate
pip install msgpack  # Phase 1 仅需 msgpack
```

或直接运行：
```bash
bash scripts/dev.sh
```

---

## 构建与运行

### 构建前端
```bash
npm run build
# 输出到 dist/ 目录
```

### 构建后端
```bash
cd src-tauri
cargo build          # Debug 构建
cargo build --release  # Release 构建 (启用 LTO)
```

### 运行应用
```bash
cd src-tauri
cargo run
```

### 环境变量

| 变量 | 说明 | 默认值 |
|------|------|--------|
| `DEEPSEEK_API_KEY` | DeepSeek API 密钥 | 无（Phase 2 需要） |
| `RUST_LOG` | 日志级别 | `ciallo=debug,tauri=info` |

---

## 开发阶段记录

### Phase 1: Wake/UI 路径 + 三队列 + 取消框架 [已完成]

**目标：** 建立应用骨架，实现 Wake→UI 的完整路径。

**完成内容：**
1. Tauri v2 项目骨架，双窗口配置
2. 8 状态 FSM，两阶段唤醒确认
3. 三优先级队列调度器 (P0/P1/P2)
4. CancellationToken + Generation Guard 取消框架
5. 音频管道：cpal capture → RingBuffer(3s) → VAD → Wake
6. P0 专用 OS 线程处理 Wake/UI 事件
7. 前端：状态指示器 + 模式面板 + WebAudio 音效
8. 可观测性：12 指标 histogram + timing span
9. Python OCR Worker 骨架 (msgpack 帧协议)

**编译状态：** `cargo build` 通过

### Phase 2: 选中文本翻译链路 [计划中]

**目标：** 实现选中文本 → 翻译 → 渲染的完整链路。

**计划内容：**
1. 选中文本采集 (accessibility API + clipboard fallback)
2. DeepSeek API 翻译 (reqwest 连接池)
3. 本地规范化 (语言检测、占位符保护)
4. 术语表注入 (仅命中项)
5. 翻译缓存 L1 (内存 LRU 512, TTL 10m)
6. 流式渲染 (增量 append)
7. 翻译结果悬浮窗

### Phase 3: OCR Worker + 区域翻译 [计划中]

**目标：** 实现 OCR 区域选择 → 预处理 → OCR → 翻译链路。

### Phase 4: 实时增量翻译 [计划中]

**目标：** 实现变化检测 + 行级 diff + 行级缓存的增量翻译。

### Phase 5: 稳定性与性能 [计划中]

**目标：** 限流/连接池/持久缓存/历史批量写/全面 KPI 验证。

---

## 实现与未实现清单

### 已实现

- [x] Tauri v2 项目骨架 + 双窗口 (main + mode-panel)
- [x] 状态机 (8 状态，验证转移)
- [x] 两阶段唤醒确认 (th_low → UI反馈 → th_high 确认/回退)
- [x] 三队列调度器 (P0 crossbeam unbounded / P1 tokio mpsc / P2 tokio mpsc)
- [x] P0 专用 OS 线程 (不受 Tokio 调度影响)
- [x] CancellationToken + GenerationGuard 取消框架
- [x] 全局取消协调 (CancelCoordinator)
- [x] 音频环形缓冲区 (固定预分配 3s，无动态扩容)
- [x] RMS 能量门控 VAD (降频推理 1/4)
- [x] WakeDetector trait (可插拔唤醒模型)
- [x] 音频管道协调 (cpal → RingBuffer → VAD → Wake → P0)
- [x] 前端状态同步 (wake-detected/confirmed/rejected 事件)
- [x] WebAudio 音效 (wake 880Hz / reject 330Hz)
- [x] 模式面板 (预创建 hidden，唤醒时 show + focus)
- [x] 可观测性框架 (SampleRing histogram, 12 metric, TimingSpan)
- [x] trace_id / request_id / generation 追踪
- [x] Tauri 命令 (get_state / get_metrics_summary / select_mode / cancel_current / dismiss)
- [x] TextCapture trait 接口定义
- [x] OcrWorkerClient trait 接口定义
- [x] Translator trait 接口定义
- [x] ROI 类型定义 (Rect / Polygon / Perspective)
- [x] OCR 预处理配置类型 (grayscale, threshold, denoise, deskew)
- [x] Python OCR Worker (PaddleOCR, lazy load, idle 卸载, msgpack)
- [x] 术语表 JSON 模板
- [x] 开发脚本 (scripts/dev.sh)
- [x] Python 虚拟环境
- [x] .gitignore

### 未实现

- [ ] 真实唤醒词模型 (当前用能量尖峰检测代替)
- [ ] 选中文本采集 (accessibility API + clipboard fallback)
- [ ] 剪贴板备份/恢复 (finally guarantee)
- [ ] DeepSeek API 翻译调用
- [ ] reqwest 连接池 (keep-alive, 2-8 连接)
- [ ] 本地令牌桶限流
- [ ] 重试策略 (429/5xx/timeout 分别处理)
- [ ] 翻译缓存 L1 (内存 LRU)
- [ ] 翻译缓存 L2 (SQLite)
- [ ] blake3 缓存 key 计算
- [ ] 语言检测
- [ ] 占位符保护 (数字/URL/邮箱/代码)
- [ ] 术语表匹配与注入
- [ ] Prompt 模板 (system ≤60 tokens, user 紧凑 JSON)
- [ ] max_tokens 动态估算
- [ ] 流式翻译渲染 (增量 append)
- [ ] 原文/译文悬浮窗
- [ ] 复制译文到剪贴板
- [ ] OCR Worker IPC (Named Pipe / Unix Socket)
- [ ] Worker 健康检查 (ping/pong)
- [ ] Worker 崩溃恢复
- [ ] 区域选择 UI (矩形/多边形/四点透视)
- [ ] ROI 预处理 (灰度、二值化、降噪、deskew)
- [ ] 实时增量翻译
- [ ] 像素差分变化检测 (MAE/SSIM)
- [ ] 行级 diff (line-hash + y_bucket)
- [ ] 行级缓存
- [ ] 历史记录 SQLite 持久化
- [ ] 异步批量写 (300ms flush)
- [ ] API Key 安全处理 (仅环境变量)
- [ ] 全面 KPI 性能验证

---

## 配置说明

### tauri.conf.json 关键配置

```json
{
  "app": {
    "withGlobalTauri": true,    // 前端通过 window.__TAURI__ 访问 API
    "windows": [
      {
        "label": "main",
        "visible": false         // 启动时隐藏，按需显示
      },
      {
        "label": "mode-panel",
        "visible": false,        // 预创建但隐藏
        "decorations": false,    // 无标题栏
        "transparent": true,     // 透明背景
        "alwaysOnTop": true      // 始终置顶
      }
    ]
  }
}
```

### 唤醒参数调优

在 `state_machine.rs` 的 `WakeConfirmer::new()` 中：

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `th_low` | 0.02 | 初始触发阈值（越低越灵敏，误唤醒越多） |
| `th_high` | 0.04 | 确认阈值（越高越严格） |
| `confirm_window` | 150ms | 确认窗口时长 |
| `confirm_frames_needed` | 2 | 窗口内需要的确认帧数 |

### VAD 参数

在 `audio/vad.rs` 的 `EnergyVad::new()` 中：

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `silence_threshold` | 300.0 | i16 RMS 静音阈值 |
| `silence_frames_needed` | 8 | 连续静音帧数判定无声 |

---

## 技术决策与假设

| 决策 | 理由 |
|------|------|
| P0 用 crossbeam unbounded 而非 tokio channel | 避免 Tokio 运行时调度抖动影响 Wake 延迟 |
| P0 用独立 OS 线程 | 与 async 运行时解耦，保证 <1ms 处理 |
| RingBuffer 用 `Box<[i16]>` 而非 `Vec` | 明确固定大小，避免意外 resize |
| 前端用 `withGlobalTauri` 而非 bundler 集成 | 避免模块解析问题，保持最简构建链 |
| esbuild 而非 webpack/vite | 构建速度快，不违反"禁止 UI 框架"约束 |
| WakeDetector 用 trait | 方便后续替换为真实 keyword-spotting 模型 |
| TextCapture/Translator/OcrWorkerClient 用 trait | 平台适配器模式，隔离平台代码 |
| 音效用 WebAudio 合成 | 无需外部音频文件，零额外依赖 |
| metrics 用自实现 SampleRing | 无需引入 prometheus/metrics 等重依赖 |

---

## License

MIT
