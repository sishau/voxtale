# Heartale 客户端 Rust 迁移方案

目标: 把 `client/reader.py` (PySide6) 迁移为单一原生 exe, 功能与 web 端 `templates/index.html` 一致。

## 1. 技术选型

| 模块 | Python 现状 | Rust 选型 | 说明 |
|---|---|---|---|
| GUI | PySide6 (Qt) | **egui + eframe** | 单 exe 无外部依赖; 原生支持无边框+透明+置顶 |
| 音频播放 | `winsound.PlaySound(SND_MEMORY)` | **rodio** | 直接从内存 `Cursor<Vec<u8>>` 解码 WAV 播放, 支持 pause/resume |
| HTTP 拉流 | `requests` | **reqwest** (blocking) | 预取线程内阻塞式 GET /tts, 与现有逻辑一一对应 |
| 配置 | json 读写 | **serde + serde_json** | 沿用现有 config.json 格式, 原子写 (tmp + rename) |
| 全局热键 | Win32 RegisterHotKey (ctypes) | **global-hotkey** crate | Ctrl+Alt+H |
| 背景亮度检测 | Qt grabWindow + 采样求亮度 | **windows** crate (GDI BitBlt) | GetDC(0) + BitBlt 截屏, 同款 0.299/0.587/0.114 加权 |
| 进度上报 | socket.io `chunk_played` (python-socketio) | **rust-socketio** | web 端已确认用 socket.io 事件 `chunk_played {chapterIndex, position}`; 已确认服务端无独立 HTTP 端点, 走同一路径 |

备选方案: **Tauri v2** — 可近乎原样复用 index.html 的播放逻辑, 但打包体积大 (依赖系统 WebView2)、无边框透明窗口细节在 Windows 上坑多。本客户端 UI 极简 (一行按钮 + 一行文字), 用 egui 重写成本反而更低, 推荐 egui。

## 2. 工程结构

```
heartale-client/
├── Cargo.toml
└── src/
    ├── main.rs        # 入口: 加载配置 → 建窗口 → 启动 player/hotkey
    ├── config.rs      # load/save config.json (server + window x/y/w/h)
    ├── player.rs      # 移植 Player 类: 预取/播放双线程 + generation 代际号
    ├── ui.rs          # egui 绘制: 圆角半透明面板、5 个按钮、文本、光标/缩放逻辑
    ├── bg_detect.rs   # 截屏测亮度, 返回 light_bg: bool
    └── hotkey.rs      # global-hotkey 线程 → 发送显隐事件
```

### Cargo.toml 依赖草案

```toml
[dependencies]
eframe = "0.29"
egui = "0.29"
rodio = "0.20"
reqwest = { version = "0.12", features = ["blocking"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
global-hotkey = "0.6"
windows = { version = "0.58", features = [
  "Win32_Graphics_Gdi", "Win32_Foundation", "Win32_UI_WindowsAndMessaging",
] }
```

## 3. 逐条需求映射

### 需求 1: 无边框透明窗口 + 内容随窗口缩放

egui/eframe 原生支持:

```rust
let mut vp = eframe::NativeOptions::default();
vp.viewport = egui::ViewportBuilder::new()
    .with_decorations(false)      // 无边框
    .with_transparent(true)       // 透明背景
    .with_always_on_top()         // 置顶
    .with_resizable(true)
    .with_inner_size([620.0, 110.0])
    .with_min_inner_size([340.0, 56.0]);
```

"文字和图标一起缩放" 用 `pixels_per_point` 实现——这是 egui 的全局缩放系数, 每帧根据窗口高度回写:

```rust
ctx.set_pixels_per_point((height / 110.0).clamp(0.8, 2.5));
```

Python 版里 `apply_style()` 按 `h * 0.26` 算字号的逻辑可以整段删除, egui 一个系数搞定所有字体/图标/间距。

### 需求 2: 低调界面 + 重新采背景按钮

- 绘制: `egui::Painter` 画圆角矩形 `Rounding::same(12.0)` + 半透明填充 `Color32::from_rgba_unmultiplied(12,12,16,150)`。
- 边缘光标 (Python 版 bug 2): egui 0.29 对可调大小窗口有内建的边缘 hit-test, 若 `with_resizable(true)` 下光标仍不变, 用 `ViewportCommand::StartResize(direction)` 手动触发 OS 级缩放循环 (还自带 Windows 的贴边分屏体验); 拖动移动用 `ViewportCommand::StartDrag`。
- 背景 A 按钮: `bg_detect.rs` 用 GDI 截屏:

```rust
let hdc_screen = GetDC(None);
let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
// CreateCompatibleBitmap + BitBlt 到内存 DC, GetDIBits 读像素
// 采样后 0.299R + 0.587G + 0.114B, 均值 >= 150 → 浅色背景
```

注意: 截屏前要把窗口 `with_visible(false)` 或先移出屏幕 (对应 Python 版 `setWindowOpacity(0)` + processEvents), 截完恢复。

### 需求 3: 播放与 web 端一致

`player.rs` 直接照搬 `reader.py::Player` 的线程模型 (已被验证可用的设计):

- `Mutex<State>` + `Condvar`: state 含 `buffer: VecDeque<Chunk>`、`paused: bool`、`jump: Option<(u32,u32)>`、`generation: u64`、`book_ended: bool`。
- 预取线程: buffer 不足 PREFETCH=3 时 `GET /tts` (跳章时带 `?index=&pos=`), 从响应头解 `X-Chapter-Index / X-Position / X-Chapter-Title / X-Text / X-Total / X-Is-End`; 204 → 全书结束。
- 播放线程: 弹出块 → 先发 UI 事件 (文本随块开始播放同步, 与 web `playNext` 一致) → `sink.append(Decoder::new(Cursor::new(audio)))` → `sink.sleep_until_end()` 等播完。
- **比 Python 版更简单的一点**: winsound 不支持原地暂停 (暂停块要塞回队首重播), rodio 的 `Sink::pause()/play()` 可原地暂停恢复, 可以删掉"被暂停的块放回队首"那段逻辑; 但要注意暂停时预取线程是否继续 (建议继续预取, 与 web 端一致)。
- 进度上报 (**已确认**, 与 Python 版 2026-09-23 实现一致): 预取会让服务端 position 超前实际播放位置。暂停和退出时, 用 `rust-socketio` 发 `chunk_played {chapterIndex, position}`, 内容为"当前正在播放的块"的位置 (不是预取块)。对应 Python 版 `ProgressReporter` + `Player._report_progress()`。
  - **恢复播放时的对齐语义**: 暂停上报后, 本地缓冲里的预取块相对服务端已超前; 恢复时清空缓冲、从上报位置重新 GET 拉流 (`Player.play()` 里 `_pause_pos -> _jump`), 否则缓冲播完后服务端会把当前块再发一遍造成重复。Rust 版用 tokio channel 重写时保留此语义。
  - 全书播完 (playback_end) 后不再上报, 避免进度回拨一格。
- 按钮图标: Python 版已改用 Windows 系统图标字体 Segoe MDL2 Assets (E892/E768/E769/E893/E711), 单色跟随前景色。Rust 版更彻底——egui 里用 `Painter` 直接画矢量图标 (三角形/双竖线/叉), 不依赖系统字体, 缩放无损。

### 需求 4: 配置文件

`config.rs` 沿用现有格式与路径 (`exe 同目录 config.json`), serde 定义:

```rust
#[derive(Serialize, Deserialize)]
struct Config { server: String, window: WindowCfg }
#[derive(Serialize, Deserialize)]
struct WindowCfg { x: Option<i32>, y: Option<i32>, w: f32, h: f32 }
```

保存: 写 `config.json.tmp` 再 `fs::rename` (对应 Python 版原子写)。启动时校验 x/y 在任一屏幕内, 否则放右下角。

## 4. 已知坑 (提前列出来)

1. **中文字体**: egui 默认字体不含 CJK。启动时加载 `C:\Windows\Fonts\msyh.ttc` (微软雅黑) 注入 `FontDefinitions`, 否则中文全是方框。
2. **WAV 同步问题**: `Decoder::new` 对 WAV 用 `WavDecoder`, 从内存 Cursor 解码零拷贝, 无坑; 若服务端将来换 mp3 需开 `minimp3` feature。
3. **Windows 11 圆角**: 无边框窗口系统仍可能套 DWM 圆角/阴影, 如需完全自绘可 `DwmSetWindowAttribute(DWMWA_WINDOW_CORNER_PREFERENCE = DWMWCP_DONOTROUND)`。
4. **透明+置顶+点击穿透混合**: 本客户端不需要点击穿透, 不要开 `WS_EX_TRANSPARENT`, 否则没法交互。
5. **msvc 工具链**: 需要先装 Visual Studio Build Tools (勾选 "Desktop development with C++") 或 `rustup` 检测到的 MSVC 组件, 再 `cargo build --release`; 也可以改用 `x86_64-pc-windows-gnu` 工具链绕开 MSVC (MinGW), 但 msvc 是 Windows 上的首选, 调试符号更友好。

## 5. 迁移步骤 (建议顺序)

1. 装 MSVC → `cargo new` → 跑通 eframe 空窗口 (透明/无边框/置顶/缩放光标先验证)。
2. `config.rs` 读写 + 窗口位置记忆。
3. `player.rs` 移植 (先不接 UI, 用 println 验证拉流/预取/跳章/204 结束)。
4. `ui.rs` 接上: 按钮行 + 文本 + 事件通道 (`std::sync::mpsc` 把 player 事件投递给 egui 帧循环, `ctx.request_repaint()` 唤醒)。
5. `bg_detect.rs` + A 按钮。
6. `hotkey.rs` + 关闭时进度上报。
7. `cargo build --release` 产出单 exe。

## 6. 待确认事项 (等服务器可连后)

- `GET /tts` 是否每次调用服务端自动推进 position (Python 版隐含此假设) — 已口头确认, 待连上后实测。
- `chunk_played` 事件服务端收到后如何处理 position (回拨到该块起点?), 需实测暂停→恢复→与 web 端进度一致性。
- 音频格式确认为 WAV (响应头未带 Content-Type 说明, web 端按 audio/wav 处理)。
- python-socketio 与 flask-socketio 的握手兼容性 (transports 默认 polling→websocket 升级)。
