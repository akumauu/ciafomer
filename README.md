# Ciallo — 语音唤醒 + 多模式翻译桌面助手

> 本地部署、低延迟、高性能的桌面翻译工具。语音唤醒后即时响应，支持选中文本翻译、区域 OCR 翻译、实时增量翻译三种模式。

---\n\n你是 Principal Desktop AI Engineer + Perf Engineer + Reliability Engineer。\n请在“不新增任何产品功能”的前提下，实现一个“语音唤醒 + 多模式翻译”桌面助手，并达到所有性能验收指标。\n\n## 0) 任务边界（硬约束）\n- 仅实现以下既有功能，不得新增产品功能：\n  1) 后台低功耗语音唤醒（命中后即时 UI/音效反馈）\n  2) 三种翻译模式：选中文本翻译、区域 OCR 翻译（矩形/套索多边形/四点透视）、实时增量翻译\n  3) DeepSeek API 翻译（重试、限流、缓存、可选流式）\n  4) 术语表注入、历史记录持久化、复制译文、原文/译文悬浮窗\n- 禁止引入与需求无关的新功能、新服务、新协议。\n- 若遇到实现不确定项，采用最保守默认值并在“Assumptions”中记录，不中断开发。\n\n## 1) 技术栈（固定）\n- 主进程：Rust + Tauri v2\n- 前端：Tauri WebView + TypeScript/CSS（禁止额外 UI 框架）\n- OCR：Python 独立 Worker（PaddleOCR + OpenCV）\n- IPC：主进程↔Worker 使用 Named Pipe(Windows)/Unix Socket(Linux/macOS) + MessagePack\n- 翻译：DeepSeek chat/completions（reqwest 连接池）\n- 存储：SQLite + 内存 LRU\n\n## 2) 平台优先级\n- P0：Windows 11 x64（必须先满足全部 KPI）\n- P1：Linux/macOS（接口保持一致，可后续适配）\n- 涉及平台差异处必须提供 trait/adapter 设计，避免平台代码污染核心逻辑。\n\n## 3) 性能与资源 KPI（阻塞验收）\n- 唤醒反馈（Wake→UI/音效事件发出）：p95 < 250ms，p99 < 400ms\n- 模式面板出现：p95 < 300ms，p99 < 500ms\n- 选中翻译首条译文：p95 < 800ms，p99 < 1.2s\n- OCR 翻译首条译文（1080p 局部 ROI）：p95 < 1.2s，p99 < 2.0s\n- 高优任务排队等待：p95 < 80ms，p99 < 120ms\n- 待机 CPU < 2%，待机内存 < 200MB\n- Token 节省（实时增量 vs 全量朴素）：>= 40%\n\n### Wake 路径禁止项（硬规则）\n- 禁止网络 I/O、OCR、大模型推理、磁盘同步写\n- P0 通道仅允许：事件通知、窗口可见切换、音效触发\n- P0 处理不得包含 >1ms 计算\n\n## 4) 架构强制要求\n### 4.1 状态机\nSleep -> WakeConfirm -> ModeSelect -> Capture -> OCR?(optional) -> Translate -> Render -> Idle/Sleep\n- 两阶段唤醒确认：\n  - 阶段1：th_low 命中即刻发 UI/音效反馈\n  - 阶段2：150ms 内累计确认，失败回退 Sleep + 轻提示\n- 目的：降低误唤醒导致的 OCR/翻译白跑，不增加用户体感延迟\n\n### 4.2 三队列调度\n- P0: Wake/UI Channel（独立，最高优先级）\n- P1: Capture/Translate Queue（Tokio async）\n- P2: OCR Heavy Queue（spawn_blocking/独立线程池 + 独立 Python 进程）\n- 抢占规则：\n  - 新 Wake 到来：取消 P1/P2 所有可取消任务，仅保留必要清理\n  - 同模式新请求：取消旧请求，request_id + generation 防止旧结果回写 UI\n  - 用户退出/超时：全链路取消并回 Sleep\n\n### 4.3 端到端取消\n- Rust：CancellationToken + generation guard\n- Python：Job.cancelled Event，推理循环中频繁检查\n- 验收：连续触发 10 次新请求，UI 只能出现最后一次结果\n\n## 5) 链路实现约束\n### 5.1 音频/唤醒\n- Ring Buffer 固定预分配（3s PCM），禁止动态扩容\n- 能量门控（RMS）-> VAD -> Wake 推理 级联过滤\n- 连续 VAD=false 降频唤醒推理（如 1/4）\n- 唤醒命中后：先发 P0，再异步投递 P1（顺序不可反）\n\n### 5.2 选中文本采集\n- 优先系统可访问性 API（超时 50-80ms）\n- 失败回退剪贴板方案：\n  - 备份原剪贴板 -> Ctrl+C -> 等待变更(<=200ms) -> 读新值 -> finally 恢复原剪贴板\n- TextPacket 统一输入，后续链路不关心来源\n\n### 5.3 区域 OCR\n- 全帧仅用于定位，OpenCV 预处理与 OCR 必须 ROI-only\n- 支持：矩形 crop、多边形 mask、四点透视 warpPerspective\n- 预处理：灰度、自适应二值化、降噪、可选 deskew\n\n### 5.4 实时增量翻译\n- 默认 500ms 周期采样 + 像素差分（MAE/SSIM）变化检测\n- 无变化帧跳过 OCR\n- OCR 行级 diff：line-hash(text + y_bucket)，y_bucket 默认 8px\n- 仅 added lines 进翻译；不变行复用缓存\n- 验收：字幕 60s 不变，API 请求 <=1 次\n\n## 6) 翻译与 Token 成本控制\n- 本地规范化先行：语言检测、占位符保护（数字/单位/URL/邮箱/代码）\n- 术语表仅注入命中项\n- Prompt：\n  - system: 极短固定模板（<=60 tokens）\n  - user: 紧凑 JSON 字段（t/g）\n- max_tokens 动态估算：(input_tokens*1.15 + 32)，上限 768\n- 缓存：\n  - L1: 内存 LRU(512, TTL 10m)\n  - L2: SQLite(TTL 7d)\n  - key = blake3(src|tgt|glossary_ver|normalized_text)\n- 行级缓存必须启用（实时增量主收益）\n\n## 7) 网络、限流、重试\n- reqwest 长连接池 keep-alive，连接数 2~8\n- 本地令牌桶限流（先限流，后请求）\n- 重试策略：\n  - 429：优先 Retry-After，否则 1s/2s/4s（最多3次）\n  - 5xx：指数退避最多2次\n  - timeout：立即重试1次\n  - 其他错误：不重试\n- API Key 仅从环境变量读取，日志禁止泄露密钥/原文敏感信息\n\n## 8) UI 与持久化\n- Mode Panel 启动预创建 hidden，唤醒时仅 show + focus\n- 流式渲染必须增量 append，禁止全量重排\n- 历史记录异步批量写（如 300ms flush），不得阻塞渲染路径\n\n## 9) Python OCR Worker 规约\n- Worker 进程常驻，模型延迟加载，idle>=60s 卸载模型\n- IPC 使用 MessagePack + raw bytes（禁 base64 JSON）\n- 健康检查：每 30s ping，500ms 内 pong；连续3次失败则重启\n- Worker 崩溃后：pending OCR 任务失败并上报“服务重启中”\n\n## 10) 可观测性（必须实现）\n每个 request 必须有：trace_id, request_id, generation\n必须埋点并输出直方图统计（p50/p95/p99）：\n- t_wake_detected\n- t_wake_ui_emitted\n- t_mode_panel_visible\n- t_capture_done\n- t_ocr_done\n- t_translate_first_chunk\n- t_translate_done\n- t_render_done\n- queue_wait_p0 / p1 / p2\n- cancel_latency\n禁止“无埋点宣称达标”。\n\n## 11) 目录结构（必须匹配）\n使用以下目录：\n- src-tauri/src/{main.rs,state_machine.rs,scheduler.rs,cancellation.rs,...}\n- src-tauri/src/audio/*\n- src-tauri/src/capture/*\n- src-tauri/src/ocr/*\n- src-tauri/src/translate/*\n- src/*\n- python-worker/*\n- scripts/*\n- glossary/default.json\n- README.md\n\n## 12) 开发顺序（强制 Phase Gate）\nPhase 1: Wake/UI 路径 + 三队列 + 取消框架\nPhase 2: 选中文本翻译链路（采集/翻译/缓存/渲染）\nPhase 3: OCR Worker + ROI 预处理 + 区域翻译\nPhase 4: 实时增量（变化检测 + line-hash diff + 行级缓存）\nPhase 5: 限流/连接池/持久缓存/历史批量写/稳定性与性能打磨\n规则：前一 Phase 未达 KPI，不得进入下一 Phase。\n\n## 13) 输出协议（必须遵守）\n每次输出必须包含以下 7 段：\n1) Assumptions（阻塞假设）\n2) Plan（本轮计划）\n3) Code（按文件给完整可运行代码，禁止伪代码/TODO）\n4) Runbook（本地启动与验证命令）\n5) Metrics（本轮可观测数据与 KPI 对比）\n6) Acceptance Mapping（映射到 20 条验收项：PASS/FAIL/UNRUN）\n7) Handoff（给下一模型）\n   - changed_files\n   - completed_acceptance_ids\n   - open_risks\n   - next_actions\n   - commands_executed + summary\n禁止伪造测试结果；未执行必须标注 UNRUN，并给出复现步骤。\n\n## 14) 20 条验收标准\n严格使用 A1~A20（与需求文档一致）：\nA1-A7 功能正确性\nA8-A14 性能\nA15-A20 稳定性与取消正确性\n交付时必须逐条给证据（日志片段/指标摘要/脚本结果）。\n\n## 15) 代码质量红线\n- 不得阻塞 Wake 路径\n- 不得在热路径频繁堆分配\n- 不得让旧 request 回写 UI\n- 不得全帧 OCR（必须 ROI-only）\n- 不得遗漏剪贴板 finally 恢复\n- 不得输出解释性翻译（只输出译文）\n- 不得泄露密钥或敏感原文到日志\n\n现在开始执行。先输出 Phase 1 的 Assumptions + Plan + 项目骨架代码。\n


全程在wsl环境下开发！ciallo

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
| **OCR** | Python Worker (PaddleOCR + OpenCV) | Phase 3: stdin/stdout IPC + MessagePack 帧协议，ROI 预处理；Phase 4: 实时像素差分(MAE) |
| **IPC** | stdin/stdout + MessagePack (4字节大端长度前缀) | 主进程↔Python Worker 通信 |
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
│  │                           result-panel      │    │
│  │                           capture-overlay   │    │
│  └────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────┘
│         │ IPC (stdin/stdout + MessagePack)
│   ┌─────▼─────┐
│   │ Python    │
│   │ OCR       │
│   │ Worker    │
│   └───────────┘
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
| 可观测性框架 | `metrics.rs` | SampleRing(1024) histogram, p50/p95/p99, 13+ 个命名指标 |
| Timing Span | `metrics.rs` → `TimingSpan` | 自动计时并记录到 histogram |
| trace_id/request_id | `metrics.rs` → `RequestIds` | 每请求唯一标识 (UUID v4) |
| Tauri 命令 | `lib.rs` | get_state / get_metrics_summary / select_mode / cancel_current / dismiss / get_screenshot_base64 / submit_ocr_selection / cancel_ocr_capture / stop_realtime / get_history |
| Python OCR Worker | `python-worker/worker.py` | PaddleOCR 延迟加载，idle≥60s 卸载，msgpack 帧协议 |
| 术语表模板 | `glossary/default.json` | JSON 格式 source→target 映射 |

### 已实现 (Phase 2)

| 功能 | 文件 | 说明 |
|------|------|------|
| OCR 引擎 trait | `ocr/mod.rs` | `OcrEngine` trait + `StubOcrEngine` (Phase 2 占位) |
| 剪贴板文本采集 | `capture/mod.rs` | `ClipboardCapture`: xdotool Ctrl+C → xclip 读取，启动时探测工具可用性 |
| 剪贴板安全恢复 | `capture/mod.rs` → `ClipboardGuard` | RAII Drop 模式保证 finally 恢复原剪贴板内容 |
| 语言检测 | `translate/normalize.rs` | whatlang crate，ISO 639-1 映射 |
| 占位符保护 | `translate/normalize.rs` → `PlaceholderProtector` | URL/邮箱/数字+单位/独立数字/内联代码 正则保护与恢复 |
| 术语表匹配 | `translate/glossary.rs` | 大小写不敏感匹配，仅注入命中项 |
| 翻译缓存 L1 | `translate/cache.rs` | LRU(512) + blake3 hash key + TTL 10min |
| DeepSeek API 客户端 | `translate/deepseek.rs` | reqwest 连接池(4 idle, 90s timeout)，手动 SSE 解析 |
| 批量流式输出 | `translate/deepseek.rs` | 40ms batched chunk flush，非逐 token |
| 令牌桶限流 | `translate/deepseek.rs` | 100ms 最小间隔 (10 req/s) |
| 重试策略 | `translate/deepseek.rs` → `send_with_retry` | 429→1s/2s/4s(3次)，5xx→500ms/1s(2次)，timeout→立即1次 |
| max_tokens 动态估算 | `translate/deepseek.rs` | (input_tokens*1.15 + 32).min(768).max(64) |
| 翻译服务编排 | `translate/mod.rs` → `TranslationService` | normalize→glossary→cache→API→restore placeholders→cache insert |
| P1 Worker 循环 | `scheduler.rs` → `run_p1_loop` | CaptureSelection→Translate(streaming)→RenderResult 完整链路 |
| 翻译结果悬浮窗 | `result-panel.html/ts` + `tauri.conf.json` | hidden + transparent + alwaysOnTop，480x320 |
| 流式渲染 | `result-panel.ts` | 增量 append `textContent +=`，禁止全量重排 |
| 复制译文 | `result-panel.ts` | `navigator.clipboard.writeText()` |
| Phase 2 事件体系 | `main.ts` | capture-complete/error, translate-chunk/complete/error |
| 优雅降级 | `lib.rs` | DEEPSEEK_API_KEY 缺失时跳过翻译服务，仅 warn |

### 已实现 (Phase 3)

| 功能 | 文件 | 说明 |
|------|------|------|
| PythonOcrEngine 实现 | `ocr/python_engine.rs` | 实现 OcrEngine trait，通过 stdin/stdout IPC 与 Python Worker 通信 |
| MessagePack 帧协议 | `ocr/python_engine.rs` | 4 字节大端长度前缀 + msgpack payload，rmp-serde 编解码 |
| Worker 进程管理 | `ocr/python_engine.rs` | spawn/restart/shutdown，健康检查(30s ping/pong)，连续 3 次失败自动重启 |
| OCR 健康检查循环 | `ocr/python_engine.rs` → `start_health_loop` | 专用线程 30s 间隔 ping，500ms 超时 |
| 屏幕截图采集 | `capture/screen.rs` | ScreenCapture: grim(Wayland)/maim(X11)/scrot(X11) 后端自动检测 |
| 截图缓存与传输 | `lib.rs` → `screenshot_cache` | parking_lot::Mutex 缓存 PNG bytes，base64 传输到前端 |
| 区域选择覆盖层 | `capture-overlay.html/ts` | 全屏透明窗口，canvas 绘制，三种选区工具 |
| 矩形选区 | `capture-overlay.ts` | click-drag 绘制矩形 ROI |
| 多边形选区 | `capture-overlay.ts` | 点击添加顶点，双击闭合多边形 ROI |
| 四点透视选区 | `capture-overlay.ts` | 4 次点击定义透视变换角点 |
| ROI 裁剪 (矩形) | `worker.py` → `ImagePreprocessor` | OpenCV 直接 crop |
| ROI 裁剪 (多边形) | `worker.py` → `ImagePreprocessor` | fillPoly mask + boundingRect crop |
| ROI 裁剪 (透视) | `worker.py` → `ImagePreprocessor` | cv2.warpPerspective 透视变换 |
| OCR 预处理管道 | `worker.py` → `ImagePreprocessor.preprocess` | 灰度 → fastNlMeansDenoising → 自适应阈值 → 可选 deskew |
| Deskew 校正 | `worker.py` → `ImagePreprocessor._deskew` | minAreaRect 角度检测 + warpAffine 旋转 |
| P2 Worker 循环 | `scheduler.rs` → `run_p2_loop` | OCR 任务处理：spawn_blocking → OCR → emit ocr-complete → 提交 P1 翻译 |
| OCR Region 流程编排 | `lib.rs` → `select_mode` | 截屏 → 缓存 → 显示 overlay → 用户选区 → P2 OCR → P1 翻译 |
| OCR 取消 | `lib.rs` → `cancel_ocr_capture` | Escape 取消选区，隐藏 overlay，清除缓存，回退 Sleep |
| Phase 3 事件体系 | `main.ts` | ocr-started/ocr-complete/ocr-error 事件监听 |
| OCR 结果显示 | `result-panel.ts` | 监听 ocr-complete 显示 OCR 原文 |

### 已实现 (Phase 4)

| 功能 | 文件 | 说明 |
|------|------|------|
| 实时增量翻译模块 | `realtime.rs` | 500ms 周期采样 + 像素差分 + 行级 diff + 行级缓存 |
| 像素差分 (MAE) | `worker.py` → `RealtimeState` | 连续帧 ROI 图像 MAE 对比，MAE < 5.0 跳过 OCR |
| line-hash 行级 diff | `realtime.rs` → `diff_lines()` | blake3(text \| y_bucket 8px)，仅 added lines 进翻译 |
| 行级翻译缓存 | `realtime.rs` → `RealtimeSession.line_cache` | 不变行复用缓存，避免重复 API 调用 |
| Token 节省统计 | `realtime.rs` → `RealtimeSession.token_saving_pct()` | 实时计算 lines_from_cache / total 比例 |
| realtime_ocr IPC | `python_engine.rs` + `worker.py` | 新增 realtime_ocr 消息类型：diff + OCR 一体 |
| reset_realtime IPC | `python_engine.rs` + `worker.py` | 清除 Python Worker 中的前帧缓存 |
| 实时循环控制 | `lib.rs` → `submit_ocr_selection` | 复用 capture-overlay 选区 UI，自动检测 RealtimeIncremental 模式 |
| stop_realtime 命令 | `lib.rs` | Tauri 命令：取消实时循环 |
| 实时取消集成 | `lib.rs` → `cancel_current` / `dismiss` | cancel/dismiss 自动终止实时循环 |
| 实时事件体系 | `main.ts` + `result-panel.ts` | realtime-started/update/error/stopped 事件 |
| 实时渲染 | `result-panel.ts` | 每次 cycle 更新 source + translated 全文 |
| 实时指标 | `metrics.rs` → `REALTIME_CYCLE` | t_realtime_cycle 周期耗时 histogram |

### 已实现 (Phase 5)

| 功能 | 文件 | 说明 |
|------|------|------|
| 翻译缓存 L2 (SQLite) | `translate/sqlite_cache.rs` | SQLite 持久缓存，TTL 7 天，WAL 模式，自动过期清理 |
| L2 缓存集成 | `translate/mod.rs` | L1 miss → L2 lookup → API；命中 L2 时自动 promote 到 L1 |
| L2 缓存清理循环 | `translate/sqlite_cache.rs` → `start_cleanup_loop` | 专用线程每小时清理过期条目 |
| 历史记录 SQLite 持久化 | `history.rs` | HistoryStore：翻译记录写入 SQLite，支持按时间倒序查询 |
| 异步批量写 (300ms flush) | `history.rs` → `flush_loop` | Tokio task 300ms 间隔批量 INSERT (事务)，不阻塞渲染路径 |
| 历史记录录入 (Selection) | `scheduler.rs` → `run_p1_loop` | P1 RenderResult 完成时异步写入历史 |
| 历史记录录入 (Realtime) | `realtime.rs` → `run_realtime_loop` | 实时会话结束时写入历史摘要 |
| get_history Tauri 命令 | `lib.rs` | 查询最近 N 条历史记录 |
| 数据目录管理 | `lib.rs` → `dirs_data_path` | XDG_DATA_HOME/ciallo 自动创建 |
| t_history_batch_write 指标 | `metrics.rs` | 历史批量写耗时 histogram |

### 未实现（超出范围）

| 功能 | 说明 |
|------|------|
| 真实唤醒词模型 | 当前用能量尖峰检测代替，需接入 keyword-spotting 模型 |

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
│       ├── lib.rs                 # App 初始化 + Tauri 命令注册 + SQLite/历史初始化
│       ├── state_machine.rs       # 8 状态 FSM + 两阶段唤醒确认
│       ├── scheduler.rs           # 三队列调度 (P0/P1/P2) + P0 handler + 历史录入
│       ├── cancellation.rs        # CancellationToken + GenerationGuard
│       ├── metrics.rs             # 可观测性: histogram, timing span
│       ├── realtime.rs            # Phase 4: 实时增量翻译 (像素差分+行级diff+行级缓存)
│       ├── history.rs             # Phase 5: 历史记录 SQLite 持久化 + 异步 300ms 批量写
│       ├── audio/
│       │   ├── mod.rs             # 音频管道: cpal → RingBuffer → VAD → Wake
│       │   ├── ring_buffer.rs     # 固定预分配环形缓冲区 (3s)
│       │   ├── vad.rs             # RMS 能量 VAD + 降频推理
│       │   └── wake.rs            # WakeDetector trait + 能量模式检测
│       ├── capture/
│       │   ├── mod.rs             # ClipboardCapture: xdotool+xclip 采集 + ClipboardGuard RAII
│       │   └── screen.rs          # ScreenCapture: grim/maim/scrot 后端自动检测
│       ├── ocr/
│       │   ├── mod.rs             # OcrEngine trait + StubOcrEngine
│       │   └── python_engine.rs   # PythonOcrEngine: stdin/stdout IPC + msgpack 帧协议
│       └── translate/
│           ├── mod.rs             # TranslationService 编排层
│           ├── normalize.rs       # 语言检测 + 占位符保护/恢复
│           ├── glossary.rs        # 术语表加载与匹配
│           ├── cache.rs           # L1: LRU(512) + blake3 key + TTL 缓存
│           ├── sqlite_cache.rs    # L2: SQLite 持久缓存 (TTL 7d, WAL 模式)
│           └── deepseek.rs        # DeepSeek API 客户端 (SSE streaming + retry)
├── src/                           # 前端 (TypeScript + CSS)
│   ├── index.html                 # 主窗口 HTML
│   ├── mode-panel.html            # 模式选择面板 HTML
│   ├── result-panel.html          # 翻译结果悬浮窗 HTML
│   ├── capture-overlay.html       # 区域选择覆盖层 HTML
│   ├── main.ts                    # 主窗口逻辑 (事件监听/状态/音效)
│   ├── mode-panel.ts              # 模式面板逻辑 (invoke 命令)
│   ├── result-panel.ts            # 结果面板逻辑 (流式渲染/复制)
│   ├── capture-overlay.ts         # 区域选择覆盖层逻辑 (canvas 选区/提交)
│   └── style.css                  # 暗色主题样式
├── python-worker/                 # Python OCR Worker
│   ├── worker.py                  # PaddleOCR + OpenCV 预处理 (ROI crop/warp, 降噪/二值化/deskew)
│   ├── requirements.txt           # Python 依赖 (paddleocr, opencv, msgpack, numpy)
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

**13 个命名指标：**
```
t_wake_detected, t_wake_ui_emitted, t_mode_panel_visible,
t_capture_done, t_ocr_done, t_translate_first_chunk,
t_translate_done, t_render_done,
queue_wait_p0, queue_wait_p1, queue_wait_p2,
cancel_latency, t_realtime_cycle, t_history_batch_write
```

**Tauri 命令 `get_metrics_summary`** 可随时查询所有指标的 p50/p95/p99。

### 6. 前端 (`src/`)

纯 TypeScript + CSS，无框架依赖。通过 `window.__TAURI__` 全局对象与后端通信。

**main.ts：** 监听 `wake-detected/confirmed/rejected/force-cancel/play-sound` 事件，驱动状态指示器和音效。监听 `ocr-started/ocr-complete/ocr-error` 事件，更新 OCR 状态显示。

**mode-panel.ts：** 三个模式按钮（Selection / OCR Region / Realtime）+ Cancel，通过 `invoke('select_mode', { mode })` 通知后端。

**result-panel.ts：** 翻译结果悬浮窗。监听 `capture-complete`, `ocr-complete`, `translate-chunk`, `translate-complete`, `force-cancel` 事件。流式渲染采用增量 `textContent +=` append，避免全量重排。提供复制译文和关闭按钮。

**capture-overlay.ts：** 区域选择覆盖层。加载截图（base64）并绘制到 canvas，提供三种选区工具：矩形（click-drag）、多边形（点击添加顶点，双击闭合）、四点透视（4 次点击）。选区完成后调用 `invoke('submit_ocr_selection')` 提交到 P2 队列。

**style.css：** 暗色主题 (`#1a1a2e`)，脉冲动画反馈，按钮悬停/按下状态。

### 7. Python OCR Worker (`python-worker/`)

Phase 3 完整实现。PaddleOCR + OpenCV 预处理管道，通过 stdin/stdout 与 Rust 主进程通信。

**设计要点：**
- PaddleOCR 延迟加载，idle ≥ 60s 自动卸载模型释放内存
- MessagePack 帧协议（4字节大端长度前缀 + msgpack payload），禁止 base64 JSON
- 支持 ping/pong 健康检查、ocr 任务、shutdown 命令

**ImagePreprocessor (Phase 3)：**
- `crop_roi()` — 三种 ROI 裁剪：矩形直接 crop，多边形 fillPoly mask + boundingRect，透视 warpPerspective
- `preprocess()` — 灰度 → fastNlMeansDenoising → 自适应阈值(GAUSSIAN, blockSize=11) → 可选 deskew
- `_deskew()` — minAreaRect 角度检测，±45° 范围内 warpAffine 旋转校正

### 8. PythonOcrEngine (`ocr/python_engine.rs`)

Rust 端的 Python Worker IPC 封装，实现 `OcrEngine` trait。

**关键类型：**
- `WorkerProcess` — 管理 `Child` 进程 + `BufReader<ChildStdout>` + `ChildStdin`
- `PythonOcrEngine` — 线程安全封装：`Mutex<Option<WorkerProcess>>` + `AtomicBool` 可用性标志
- `WorkerMessage` — serde tagged enum：`ping`、`ocr`(image_data + roi)、`shutdown`
- `WorkerResponse` — 响应枚举：`pong`、`ocr_result`(text + lines + elapsed_ms)、`error`

**IPC 协议：**
- `send_recv()` — 序列化 msgpack → 4字节 BE 长度前缀 → write stdin → read stdout → 反序列化
- 自动 spawn worker（首次调用时启动进程）
- 调用失败时标记 `available = false`，健康检查恢复后重新标记

**健康检查循环：**
- `start_health_loop()` 启动专用线程，30s 间隔发送 ping
- 连续 3 次失败 → 重启 worker 进程
- 成功响应 → 重置失败计数

### 9. 屏幕截图采集 (`capture/screen.rs`)

支持多种 Linux 截图工具的屏幕截图采集器。

**ScreenCapture：**
- 启动时 `probe_backends()` 按优先级探测可用后端：grim(Wayland) > maim(X11) > scrot(X11)
- `capture()` — 调用选中后端执行截图 → 保存 `/tmp/ciallo_capture.png` → 读取字节
- `is_available()` — 至少有一个后端可用即返回 true

### 10. 区域选择覆盖层 (`capture-overlay.html/ts`)

全屏透明 Tauri 窗口，用于 OCR Region 模式的区域选择。

**工作流：**
1. 用户选择 OCR Region 模式 → 后端截屏并缓存 → 显示 overlay 窗口
2. Overlay 通过 `invoke('get_screenshot_base64')` 获取截图并绘制到 canvas
3. 用户使用工具选区（矩形/多边形/四点透视）
4. 选区完成 → `invoke('submit_ocr_selection', { roiType, roiParams })` → 隐藏 overlay
5. P2 队列接收 OCR 任务 → Python Worker 执行 → 结果送 P1 翻译

**工具：**
- **矩形**：click-drag 绘制，实时预览矩形框
- **多边形**：点击添加顶点，双击闭合多边形
- **四点透视**：依次点击 4 个角点，适用于倾斜/透视文档
- Escape/Cancel 取消并回退到 Sleep 状态

### 11. 翻译管道 (`translate/`)

Phase 2 完整翻译管道，由 `TranslationService` 编排。

**normalize.rs：**
- `detect_language()` — whatlang 检测 + ISO 639-1 映射 (en/zh/ja/ko/fr/de/es/ru/pt/ar/...)
- `PlaceholderProtector` — 5 种正则模式：URL、邮箱、数字+单位、独立数字、内联代码 `` `...` ``
- `normalize()` — 返回 `NormalizeResult { normalized_text, detected_lang, placeholders }`

**glossary.rs：**
- `Glossary` — 从 JSON 加载术语表，大小写不敏感匹配
- `match_entries()` — 仅返回 source 出现在输入文本中的条目

**cache.rs：**
- `TranslationCache` — Mutex 保护的 `LruCache<[u8;32], CacheEntry>`
- key = `blake3(src_lang | tgt_lang | glossary_ver | normalized_text)`
- 容量 512，TTL 10 分钟

**deepseek.rs：**
- `DeepSeekClient` — reqwest 连接池 (4 idle, 90s timeout)
- 手动 SSE 解析 (`data: {...}` → `serde_json`)
- 40ms batched chunk flush (非逐 token)
- 简单令牌桶限流 (100ms min interval = 10 req/s)
- `send_with_retry()` — 429→1s/2s/4s(3次)，5xx→500ms/1s(2次)，timeout→立即1次
- 紧凑 prompt：system ≤60 tokens，user `{"t":"text","l":"lang","g":{"src":"tgt"}}`

**mod.rs — TranslationService：**
1. normalize (语言检测 + 占位符保护)
2. glossary match (仅命中项)
3. L1 cache lookup (blake3 key)
4. L2 cache lookup (SQLite, promote to L1 on hit)
5. API call (SSE streaming + on_chunk callback)
6. restore placeholders
7. cache insert (L1 + L2)

### 15. 翻译缓存 L2 (`translate/sqlite_cache.rs`) — Phase 5

SQLite 持久化的二级翻译缓存，跨会话复用翻译结果。

**设计要点：**
- WAL (Write-Ahead Logging) 模式：读写可并发
- Key = blake3 hash blob (与 L1 相同)
- TTL 7 天：`created_at` 字段 + 查询时过滤
- 自动清理：专用线程每小时 `DELETE WHERE created_at <= cutoff`
- Mutex 保护的单连接（写入量低，无需连接池）

**缓存层级：**
```
L1 miss → L2 lookup → API call
                ↓ hit
          promote to L1
```

### 16. 历史记录 (`history.rs`) — Phase 5

异步批量写的翻译历史持久化模块。

**核心类型：**
- `HistoryRecord` — 单条记录：request_id, source/translated, mode, tokens, cached, created_at
- `HistoryStore` — unbounded channel 接收 + 300ms flush 批量写

**异步批量写：**
- `record()` — 非阻塞发送到 unbounded channel
- `flush_loop()` — Tokio task，300ms interval，drain all pending → 事务 batch INSERT
- 读写分离：独立的 `read_conn` (查询) 和 `write_conn` (写入)

**查询：**
- `query_recent(limit)` — 按时间倒序返回最近 N 条记录
- `cleanup_older_than_days(days)` — 清理过旧记录

### 12. 文本采集 (`capture/`)

**ClipboardCapture：**
- 启动时 `probe_command()` 检测 xdotool + xclip 可用性
- 采集流程：backup clipboard → xdotool Ctrl+C → wait 60ms → read clipboard → compare with backup
- `ClipboardGuard` RAII Drop 模式保证 finally 恢复原剪贴板内容
- 工具不可用时快速失败 `CaptureError::ToolNotAvailable`

### 13. 实时增量翻译 (`realtime.rs`) — Phase 4

500ms 周期采样 + 像素差分(MAE) + 行级 diff + 行级缓存的增量翻译模块。

**核心类型：**
- `RealtimeSession` — 会话状态：previous_lines, line_cache(text→translation), 统计计数器
- `LineDiff` — 行级 diff 结果：added (需翻译) + unchanged (复用缓存)

**关键函数：**
- `line_hash(text, y_center) -> [u8;32]` — blake3(text | y_bucket)，y_bucket = (y_center / 8) * 8
- `diff_lines(old, new) -> LineDiff` — 通过 line_hash 集合差集区分 added/unchanged
- `run_realtime_loop()` — 主循环 (tokio task)：
  1. `ScreenCapture::capture()` → PNG bytes
  2. `PythonOcrEngine::realtime_ocr()` → 像素差分 + OCR (spawn_blocking)
  3. 如无变化 → 跳过，等待下一周期
  4. `diff_lines()` → 行级 diff
  5. 仅 added 行调用 `TranslationService::translate()`
  6. `build_merged()` → 合并缓存 + 新翻译
  7. emit `realtime-update` → result-panel 更新

**像素差分 (Python Worker)：**
- `RealtimeState` 存储前帧 ROI 图像
- `compute_mae()` — numpy 计算 Mean Absolute Error
- MAE < 5.0 → 返回 `no_change`，跳过 OCR
- MAE >= 5.0 → 预处理 + OCR，更新前帧

**Token 节省机制：**
- 行级缓存：session-local HashMap(line_text → translated_text)
- 相同文本出现在同一 y_bucket → line_hash 匹配 → 标记为 unchanged → 复用缓存
- 统计：token_saving_pct = lines_from_cache / (lines_from_cache + lines_translated_via_api) × 100%
- 验收目标：字幕 60s 不变，API 请求 ≤1 次

---

## 性能指标 (KPI)

| 指标 | 目标 | 当前状态 |
|------|------|---------|
| 唤醒反馈 (Wake→UI) | p95 < 250ms, p99 < 400ms | 框架就绪，埋点已实现 |
| 模式面板出现 | p95 < 300ms, p99 < 500ms | 预创建 hidden + show，埋点就绪 |
| 选中翻译首条译文 | p95 < 800ms, p99 < 1.2s | Phase 2 链路完成 + Phase 5 L2 缓存加速，待实测 |
| OCR 翻译首条译文 | p95 < 1.2s, p99 < 2.0s | Phase 3 链路完成 + Phase 5 L2 缓存加速，待实测 |
| 高优任务排队等待 | p95 < 80ms, p99 < 120ms | crossbeam 无界 + 专用线程 |
| 待机 CPU | < 2% | 预期达标 (sleep loop) |
| 待机内存 | < 200MB | 预期达标 (~96KB ring buffer) |
| Token 节省 (增量 vs 全量) | >= 40% | Phase 4 行级缓存 + Phase 5 L2 持久缓存，待实测验证 |

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

# 5. Python 虚拟环境 (OCR Worker)
cd python-worker
python3 -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt  # paddleocr, opencv-python-headless, msgpack, numpy
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

### Phase 2: 选中文本翻译链路 [已完成]

**目标：** 实现选中文本 → 翻译 → 渲染的完整链路。

**完成内容：**
1. OCR 模块重构：移除 Python Worker IPC 架构，改为轻量 Rust-native `OcrEngine` trait（Phase 3 接入真实引擎）
2. 剪贴板文本采集：`ClipboardCapture` (xdotool Ctrl+C + xclip 读取)，启动时探测工具可用性，`ClipboardGuard` RAII 保证恢复
3. 语言检测 + 占位符保护：`whatlang` 检测语种，正则保护 URL/邮箱/数字+单位/代码
4. 术语表匹配：大小写不敏感匹配，仅注入命中项到 prompt
5. 翻译缓存 L1：LRU(512) + blake3 hash key + TTL 10min
6. DeepSeek API 客户端：reqwest 连接池(4 idle)，手动 SSE 解析，40ms batched chunk flush
7. 令牌桶限流 + 重试策略：429/5xx/timeout 分级重试
8. `TranslationService` 编排：normalize → glossary → cache → API → restore → cache insert
9. P1 Worker 循环：CaptureSelection → Translate(streaming) → RenderResult 三阶段
10. 翻译结果悬浮窗：result-panel (hidden + transparent + alwaysOnTop)，流式增量渲染 + 复制译文

**编译状态：** `cargo check` 通过 (WSL rustc 1.93.1 + Windows rustc 1.90.0，零错误零警告)

**新增文件：**
- `src-tauri/src/translate/normalize.rs` — 语言检测 + 占位符保护
- `src-tauri/src/translate/glossary.rs` — 术语表加载与匹配
- `src-tauri/src/translate/cache.rs` — LRU + blake3 翻译缓存
- `src-tauri/src/translate/deepseek.rs` — DeepSeek API 客户端
- `src/result-panel.html` — 翻译结果悬浮窗
- `src/result-panel.ts` — 结果面板前端逻辑

**重构文件：**
- `src-tauri/src/ocr/mod.rs` — `OcrWorkerClient` → `OcrEngine` trait
- `src-tauri/src/capture/mod.rs` — 实现 `ClipboardCapture`
- `src-tauri/src/translate/mod.rs` — 新增 `TranslationService`
- `src-tauri/src/scheduler.rs` — P1 worker loop + Mutex 重构
- `src-tauri/src/lib.rs` — AppContext 扩展 + 翻译服务初始化

### Phase 3: OCR Worker + ROI 预处理 + 区域翻译 [已完成]

**目标：** 实现 Python OCR Worker IPC、ROI 预处理管道、区域选择 UI、OCR→翻译完整链路。

**完成内容：**
1. `PythonOcrEngine` 实现 `OcrEngine` trait，通过 stdin/stdout + MessagePack 帧协议与 Python Worker 通信
2. Worker 进程生命周期管理：自动 spawn、健康检查(30s ping/pong)、连续 3 次失败自动重启
3. Python Worker 升级：`ImagePreprocessor` 类，支持矩形/多边形/透视 ROI 裁剪 + 灰度/降噪/自适应阈值/deskew 预处理
4. `ScreenCapture` 屏幕截图采集器：grim(Wayland)/maim(X11)/scrot(X11) 后端自动检测
5. 区域选择覆盖层 (`capture-overlay`)：全屏透明窗口，canvas 绘制，三种选区工具（矩形/多边形/四点透视）
6. 截图缓存与 base64 传输：后端缓存 PNG bytes → 前端通过 Tauri command 获取
7. `run_p2_loop()` P2 Worker 循环：spawn_blocking OCR → emit ocr-complete → 提交 P1 翻译
8. OCR Region 流程编排：select_mode → 截屏 → 缓存 → overlay → 选区 → P2 OCR → P1 翻译
9. 前端 OCR 事件体系：ocr-started/ocr-complete/ocr-error 状态更新
10. Tauri 配置：capture-overlay 窗口(fullscreen + transparent + alwaysOnTop)，CSP 允许 data: 图片

**编译状态：** `cargo check` 通过 (零错误零警告)，`npm run build` 通过

**新增文件：**
- `src-tauri/src/ocr/python_engine.rs` — PythonOcrEngine IPC 实现
- `src-tauri/src/capture/screen.rs` — ScreenCapture 屏幕截图采集
- `src/capture-overlay.html` — 区域选择覆盖层 HTML
- `src/capture-overlay.ts` — 区域选择覆盖层前端逻辑

**修改文件：**
- `src-tauri/Cargo.toml` — 新增 rmp-serde, base64 依赖
- `src-tauri/src/ocr/mod.rs` — 添加 python_engine 模块导出
- `src-tauri/src/capture/mod.rs` — 添加 screen 模块导出
- `src-tauri/src/scheduler.rs` — 新增 `run_p2_loop()` P2 Worker 循环
- `src-tauri/src/lib.rs` — OCR engine 初始化、P2 loop 启动、新增 get_screenshot_base64/submit_ocr_selection/cancel_ocr_capture 命令
- `src-tauri/tauri.conf.json` — 添加 capture-overlay 窗口配置，更新 CSP
- `src-tauri/capabilities/default.json` — 添加 capture-overlay 到 windows 列表
- `src/main.ts` — 添加 ocr-started/ocr-complete/ocr-error 事件监听
- `src/result-panel.ts` — 添加 ocr-complete 事件监听显示 OCR 原文
- `src/style.css` — 添加 capture overlay 样式
- `build.mjs` — 添加 capture-overlay.ts 构建和 HTML 拷贝
- `python-worker/worker.py` — 全面升级：ImagePreprocessor 类 + ROI 裁剪 + 预处理管道

### Phase 4: 实时增量翻译 [已完成]

**目标：** 实现变化检测 + 行级 diff + 行级缓存的增量翻译。

**完成内容：**
1. `RealtimeState` 像素差分：Python Worker 存储前帧 ROI 图像，MAE (Mean Absolute Error) 对比，阈值 5.0 以下跳过 OCR
2. `realtime_ocr` IPC 消息：一次调用完成 diff + OCR，减少 IPC 往返；`reset_realtime` 清除前帧缓存
3. `LineDiffer` 行级 diff：`blake3(text | y_bucket)` 哈希，y_bucket = 8px 粒度，区分 added/unchanged 行
4. `RealtimeSession` 行级缓存：per-session HashMap (line_text → translated_text)，不变行直接复用
5. `run_realtime_loop()` 500ms 周期循环：截屏 → realtime_ocr(diff+OCR) → line diff → 仅翻译 added → merge → render
6. Token 节省统计：实时计算 lines_from_cache / (lines_from_cache + lines_translated_via_api)
7. 复用 capture-overlay 选区 UI：Realtime 模式与 OCR Region 共享区域选择流程
8. `stop_realtime` Tauri 命令：独立停止实时循环
9. cancel_current/dismiss 自动终止：取消令牌集成到全局取消协调
10. 前端事件：realtime-started/update/error/stopped，result-panel 每周期更新
11. 可观测性：t_realtime_cycle 指标 histogram

**编译状态：** `cargo check` 通过 + `npm run build` 通过 (零错误零警告)

**新增文件：**
- `src-tauri/src/realtime.rs` — 实时增量翻译核心模块 (LineDiffer, RealtimeSession, run_realtime_loop)

**修改文件：**
- `python-worker/worker.py` — 新增 RealtimeState 类 (MAE 像素差分)、realtime_ocr/reset_realtime 消息处理
- `src-tauri/src/ocr/mod.rs` — 新增 RealtimeOcrResult 类型
- `src-tauri/src/ocr/python_engine.rs` — 新增 realtime_ocr() / reset_realtime() 方法、RealtimeOcr/ResetRealtime 消息类型
- `src-tauri/src/lib.rs` — 新增 python_ocr / realtime_cancel 字段、RealtimeIncremental 模式处理、stop_realtime 命令
- `src-tauri/src/metrics.rs` — 新增 REALTIME_CYCLE 指标
- `src/main.ts` — 新增 realtime-started/update/error/stopped 事件监听
- `src/result-panel.ts` — 新增 realtime-update / realtime-stopped 事件处理

### Phase 5: 限流/连接池/持久缓存/历史批量写/稳定性与性能打磨 [已完成]

**目标：** 持久缓存 L2 (SQLite)、历史记录异步批量写、全链路稳定性打磨。

**完成内容：**
1. `SqliteCache` 翻译缓存 L2：SQLite WAL 模式，TTL 7 天，blake3 key，自动过期清理（每小时，专用线程）
2. L2 缓存集成到 `TranslationService`：L1 miss → L2 lookup → API call；L2 命中时 promote 到 L1；API 成功后同时写入 L1 + L2
3. `HistoryStore` 历史记录持久化：SQLite 存储翻译记录（request_id, source, translated, mode, tokens, cached, timestamp）
4. 异步批量写 (300ms flush)：unbounded channel + Tokio task，300ms 间隔批量 INSERT 事务，绝不阻塞渲染路径
5. 读写分离：history 使用独立的 read/write Connection（两个 SQLite 连接，WAL 模式允许并行）
6. P1 RenderResult 历史录入：翻译完成后 selection 模式自动写入历史
7. 实时会话历史录入：realtime loop 结束时写入最终翻译摘要
8. `get_history` Tauri 命令：查询最近 N 条历史记录（默认 50，按时间倒序）
9. 数据目录管理：`dirs_data_path()` 使用 XDG_DATA_HOME，自动创建 `~/.local/share/ciallo/`
10. `t_history_batch_write` 可观测性指标

**编译状态：** `cargo check` 通过 + `npm run build` 通过 (零错误零警告)

**新增文件：**
- `src-tauri/src/translate/sqlite_cache.rs` — L2 SQLite 翻译缓存 (TTL 7d, WAL, 自动清理)
- `src-tauri/src/history.rs` — 历史记录 SQLite 持久化 + 异步 300ms 批量写

**修改文件：**
- `src-tauri/Cargo.toml` — 新增 rusqlite (bundled) 依赖
- `src-tauri/src/translate/mod.rs` — L2 缓存集成 (TranslationService 新增 l2_cache 字段)
- `src-tauri/src/lib.rs` — SQLite/HistoryStore 初始化、get_history 命令、AppContext 扩展
- `src-tauri/src/scheduler.rs` — run_p1_loop 接受 history_store 参数，RenderResult 写入历史
- `src-tauri/src/realtime.rs` — run_realtime_loop 接受 history_store 参数，会话结束写入历史
- `src-tauri/src/metrics.rs` — 新增 HISTORY_BATCH_WRITE 指标

---

## 实现与未实现清单

### 已实现

- [x] Tauri v2 项目骨架 + 四窗口 (main + mode-panel + result-panel + capture-overlay)
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
- [x] TextCapture trait + ClipboardCapture 实现
- [x] OcrEngine trait + StubOcrEngine + PythonOcrEngine (Python Worker IPC)
- [x] ROI 类型定义 (Rect / Polygon / Perspective)
- [x] OCR 预处理配置类型 (grayscale, threshold, denoise, deskew)
- [x] Python OCR Worker (PaddleOCR, lazy load, idle 卸载, msgpack, ROI 预处理)
- [x] 术语表 JSON 模板 + 匹配注入
- [x] 开发脚本 (scripts/dev.sh)
- [x] .gitignore
- [x] 剪贴板安全恢复 (ClipboardGuard RAII Drop)
- [x] DeepSeek API 翻译 (SSE streaming + 手动解析)
- [x] reqwest 连接池 (keep-alive, 4 idle, 90s timeout)
- [x] 本地令牌桶限流 (100ms 间隔)
- [x] 重试策略 (429/5xx/timeout 分级处理)
- [x] 翻译缓存 L1 (内存 LRU 512, TTL 10min, blake3 key)
- [x] 语言检测 (whatlang + ISO 639-1)
- [x] 占位符保护 (URL/邮箱/数字+单位/代码)
- [x] Prompt 模板 (system ≤60 tokens, user 紧凑 JSON t/l/g)
- [x] max_tokens 动态估算
- [x] 流式翻译渲染 (增量 append，40ms batch)
- [x] 翻译结果悬浮窗 (result-panel)
- [x] 复制译文到剪贴板
- [x] TranslationService 编排层 (normalize→glossary→cache→API→restore→cache)
- [x] P1 Worker 循环 (Capture→Translate→Render 完整链路)
- [x] Phase 2 事件体系 (capture/translate 事件)
- [x] DEEPSEEK_API_KEY 优雅降级
- [x] API Key 安全处理 (仅环境变量，日志不泄露)
- [x] PythonOcrEngine (stdin/stdout IPC + msgpack 帧协议)
- [x] Worker 进程生命周期管理 (spawn/restart/健康检查)
- [x] 屏幕截图采集 (ScreenCapture: grim/maim/scrot 后端检测)
- [x] 区域选择覆盖层 (capture-overlay: 全屏透明 canvas)
- [x] 矩形选区 (click-drag)
- [x] 多边形选区 (点击添加顶点，双击闭合)
- [x] 四点透视选区 (4 次点击定义角点)
- [x] ROI 裁剪 (矩形 crop / 多边形 mask+crop / 透视 warpPerspective)
- [x] OCR 预处理管道 (灰度 → 降噪 → 自适应阈值 → deskew)
- [x] P2 Worker 循环 (run_p2_loop: OCR → emit → P1 翻译)
- [x] OCR Region 完整流程 (截屏 → 缓存 → overlay → 选区 → P2 OCR → P1 翻译)
- [x] Phase 3 事件体系 (ocr-started/ocr-complete/ocr-error)
- [x] OCR 结果显示 (result-panel 监听 ocr-complete)
- [x] 实时增量翻译 (500ms 采样 + 像素差分 + line-hash diff + 行级缓存)
- [x] 像素差分变化检测 (MAE, 阈值 5.0)
- [x] 行级 diff (blake3 line-hash + y_bucket 8px)
- [x] 行级翻译缓存 (session-local text→translation HashMap)
- [x] Token 节省统计 (lines_from_cache / total)
- [x] realtime_ocr IPC (diff + OCR 一体)
- [x] 实时循环控制 (stop_realtime + cancel 集成)
- [x] 实时事件体系 (realtime-started/update/error/stopped)
- [x] t_realtime_cycle 指标
- [x] 翻译缓存 L2 (SQLite, TTL 7d, WAL 模式, 自动过期清理)
- [x] L2 缓存集成 (L1 miss → L2 → API, promote on hit)
- [x] 历史记录 SQLite 持久化 (HistoryStore)
- [x] 异步批量写 (300ms flush, unbounded channel + Tokio task)
- [x] get_history Tauri 命令
- [x] t_history_batch_write 指标

### 未实现

- [ ] 真实唤醒词模型 (当前用能量尖峰检测代替)
- [ ] 全面 KPI 性能验证 (需真实硬件环境实测)

---

## 配置说明

### tauri.conf.json 关键配置

```json
{
  "app": {
    "withGlobalTauri": true,
    "windows": [
      {
        "label": "main",
        "visible": false
      },
      {
        "label": "mode-panel",
        "visible": false,
        "decorations": false,
        "transparent": true,
        "alwaysOnTop": true
      },
      {
        "label": "result-panel",
        "visible": false,
        "decorations": false,
        "transparent": true,
        "alwaysOnTop": true,
        "width": 480,
        "height": 320
      },
      {
        "label": "capture-overlay",
        "visible": false,
        "fullscreen": true,
        "decorations": false,
        "transparent": true,
        "alwaysOnTop": true,
        "resizable": false
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
| TextCapture/OcrEngine 用 trait | 平台适配器模式，隔离平台代码 |
| 音效用 WebAudio 合成 | 无需外部音频文件，零额外依赖 |
| metrics 用自实现 SampleRing | 无需引入 prometheus/metrics 等重依赖 |
| 手动 SSE 解析而非 eventsource-stream crate | 减少依赖，对 DeepSeek 响应格式更可控 |
| 40ms batched chunk flush | 平衡渲染频率与性能，避免逐 token DOM 更新 |
| blake3 而非 sha256 做缓存 key | 更快的哈希，无密码学需求 |
| on_chunk callback 用 `&(dyn Fn + Send + Sync)` | tokio::spawn 要求 future 为 Send |
| ClipboardGuard RAII Drop | 保证剪贴板恢复，即使发生 panic |
| Python Worker IPC (stdin/stdout) | 跨平台兼容，无需 Named Pipe/Unix Socket 差异处理 |
| rmp-serde 做 IPC 序列化 | 与 Python msgpack 互操作，二进制紧凑 |
| base64 传输截图到前端 | Tauri command 不支持直接传 binary，base64 是最简方案 |
| ScreenCapture 后端探测链 | grim > maim > scrot 优先级，覆盖 Wayland 和 X11 |
| Canvas 绘制选区而非 DOM | 性能好，支持自由绘制多边形和实时预览 |
| 截图缓存在 Mutex 中 | 单次使用(take)，OCR 提交后立即释放内存 |
| P2 用 spawn_blocking | OCR 是 CPU 密集型，不阻塞 Tokio 异步运行时 |
| OCR 结果直接提交 P1 翻译 | P2→P1 自动衔接，用户无需二次操作 |
| 像素差分在 Python Worker 做 | 避免 Rust 添加 image crate 依赖，复用 OpenCV/numpy |
| MAE 阈值 5.0 | 保守默认值，平衡灵敏度与误触发 |
| y_bucket = 8px | 容忍 OCR 对同一行的 y_center 微小漂移 |
| 行级缓存用 session HashMap | 无 TTL 需求(session 生命周期=缓存生命周期)，比全局 LRU 更快 |
| realtime_ocr 一次 IPC 完成 diff+OCR | 减少 IPC 往返，避免发送两次帧数据 |
| 复用 capture-overlay 选区 UI | 实时模式和 OCR 模式共享选区流程，减少代码重复 |
| 实时循环独立于 P1/P2 队列 | 循环需要同步获取 OCR 结果做 diff，不适合队列异步模式 |
| CancellationToken 独立管理 | 实时循环有独立 cancel token，不受 P1/P2 generation 影响 |
| rusqlite bundled | 自带 SQLite 编译，无需系统 libsqlite3，跨平台一致 |
| WAL 模式 | 读写可并发，不阻塞查询 |
| 读写分离 (history) | 独立的 read/write Connection，WAL 允许并行读写 |
| unbounded channel (history) | 历史写入绝不阻塞渲染路径，即使 flush 暂时延迟 |
| 300ms flush interval | 平衡写入延迟与 I/O 频率，批量事务提升吞吐 |
| XDG_DATA_HOME | 遵循 Linux 标准目录规范，数据存储在 ~/.local/share/ciallo/ |

---

## License

MIT
