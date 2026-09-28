//! Voxtale 悬浮朗读窗 (Rust 版, 移植自 client/reader.py)
//!
//! - 无边框 + 透明 + 置顶, 窗口内文字/图标随高度等比缩放 (pixels_per_point = h/110)
//! - 界面低调: 半透明圆角面板, 中性灰字色 (不与背景完全相反), A 按钮重新采背景调字色
//! - 播放与 web 端一致: 预取 3 段 / 文本随块播放同步 / 长文本分页轮播 / 关闭时上报进度
//! - 服务器地址与窗口位置存 exe 同目录 config.json
//! - 全局热键 Ctrl+Alt+H 隐藏/显示; 边缘拖拽缩放 (winit 无边框可缩放原生支持),
//!   空白处拖动移动 (StartDrag), 关闭即刻销毁窗口 + 后台收尾

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod bg_detect;
mod config;
mod player;
mod reporter;

use eframe::egui;
use egui::epaint::Shape;
use egui::{
    Align2, Color32, CursorIcon, FontData, FontDefinitions, FontId, Pos2, Rect, Sense, Stroke,
    Vec2, ViewportCommand,
};
use egui::viewport::ResizeDirection;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWA_BORDER_COLOR,
    DWMWA_NCRENDERING_POLICY, DWMWA_WINDOW_CORNER_PREFERENCE, DWMNCRP_DISABLED,
    DWMWCP_DONOTROUND, DWMWA_COLOR_NONE,
};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetSystemMetrics, IsWindowVisible, SetForegroundWindow, ShowWindow, SM_CXSCREEN,
    SM_CYSCREEN, SW_HIDE, SW_SHOW,
};

use config::Config;
use player::{Player, PlayerEvent};

// 逻辑尺寸基准 (pixels_per_point = 物理高 / BASE_H, 所有逻辑尺寸随窗口整体缩放)
const BASE_H: f32 = 110.0;
const TEXT_SIZE: f32 = 40.0; // h=110 时 40 物理px; 提大以收紧文字与上下边框的留白 (h=26 时约 9.5px)
const BTN_SIZE: f32 = 39.0; // 按钮 39, 图标 29 (Python: fs_btn+pad / fs_btn)
const ICON_SIZE: f32 = 29.0;
const MARGIN_X: f32 = 12.0; // 左右边距 (上下边距由面板内垂直居中自然达成, 约等效 2px)
const GAP: f32 = 4.0;
const MIN_W: f32 = 340.0;
const MIN_H: f32 = 18.0;
const CORNER_PX: f32 = 12.0; // 面板圆角, 物理 px 固定 (Python 版 CORNER=12)
const RESIZE_EDGE_PX: f32 = 6.0; // 边缘缩放命中带宽度, 物理 px (无边框窗口需自行命中检测)

/// 按窗口标题查找主窗口 HWND (返回裸指针值以便跨线程传递)
/// 注意: FindWindowW(类名, 窗口名) — 第一个参数是窗口类名, 标题必须传第二个参数
fn find_hwnd() -> Option<isize> {
    unsafe {
        FindWindowW(PCWSTR::null(), w!("voxtale"))
            .ok()
            .map(|h| h.0 as isize)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Icon {
    Prev,
    Play,
    Pause,
    Next,
    Detect,
    Close,
}

#[derive(Clone, Copy, Debug)]
struct Palette {
    fg: Color32,
    fill: Color32,
}

impl Palette {
    fn new(light_bg: bool) -> Self {
        if light_bg {
            Self {
                fg: Color32::from_rgb(0x6b, 0x6b, 0x73), // 浅背景: 中灰, 比黑柔和 (对比度 ~4.6:1)
                fill: Color32::from_rgba_unmultiplied(252, 252, 252, 150),
            }
        } else {
            Self {
                fg: Color32::from_rgb(0x98, 0x98, 0x9f), // 深背景: 暗灰, 不刺眼 (~5.1:1)
                fill: Color32::from_rgba_unmultiplied(12, 12, 16, 150),
            }
        }
    }
}

struct App {
    cfg: Config,
    player: Player,
    hotkey_rx: mpsc::Receiver<()>,
    palette: Palette,
    playing: bool,
    chapter: u32,
    total: u32,
    full_text: String,
    chunk_dur: f64,
    pages: Vec<String>,
    page_idx: usize,
    page_started: Instant,
    dirty_pages: bool,
    last_avail_w: f32,
    applied_ppp: f32, // 我们上次设定的 ppp; set_pixels_per_point 下一帧才生效
    stable_frames: u32,
    measured_phy_h: f32, // 最近一次可信的窗口物理高
    visible: bool,
    /// A 按钮背景重检测结果 (亮度), 由后台线程经 channel 送回
    bg_rx: mpsc::Receiver<Option<f32>>,
    #[allow(dead_code)]
    bg_tx: mpsc::Sender<Option<f32>>,
    /// 边缘缩放命中时本帧应显示的光标 (CentralPanel 闭合后统一写入 output, 覆盖拖拽 widget 的 Grab)
    resize_cursor: Option<CursorIcon>,
}

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        cfg: Config,
        light_bg: bool,
        hotkey_rx: mpsc::Receiver<()>,
        bg_rx: mpsc::Receiver<Option<f32>>,
        bg_tx: mpsc::Sender<Option<f32>>,
    ) -> Self {
        set_fonts(&cc.egui_ctx);
        let server = cfg.server.clone();
        Self {
            cfg,
            player: Player::new(&server),
            hotkey_rx,
            palette: Palette::new(light_bg),
            playing: true, // Player 启动即播放, 与 Python 版一致
            chapter: 0,
            total: 0,
            full_text: "连接服务中…".to_owned(),
            chunk_dur: 0.0,
            pages: Vec::new(),
            page_idx: 0,
            page_started: Instant::now(),
            dirty_pages: true,
            last_avail_w: 0.0,
            applied_ppp: 0.0,
            stable_frames: 0,
            measured_phy_h: 0.0,
            visible: true,
            bg_rx,
            resize_cursor: None,
            bg_tx,
        }
    }

    fn jump_to(&mut self, chapter: u32) {
        self.player.jump(chapter, 0);
        self.chapter = chapter;
        self.playing = true;
        self.full_text = "…".to_owned();
        self.pages = Vec::new();
        self.dirty_pages = true;
    }
}

/// 按显示宽度把长文本拆成多页: 标点优先断句, 超宽句按字硬切, 贪心装页 (与 Python 版 _split_pages 一致)
fn split_pages(ctx: &egui::Context, text: &str, avail_w: f32, font_id: FontId) -> Vec<String> {
    let width =
        |s: &str| ctx.fonts(|f| f.layout_no_wrap(s.to_owned(), font_id.clone(), Color32::WHITE).rect.width());
    let is_punct =
        |c: char| matches!(c, '，' | '。' | '！' | '？' | '；' | '：' | '、' | ',' | '.' | '!' | '?' | ';' | ':' | '\n');
    let mut units: Vec<String> = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        cur.push(c);
        if is_punct(c) {
            units.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        units.push(cur);
    }
    // 超宽句按字硬切 (保证至少 1 字/页)
    let mut fine: Vec<String> = Vec::new();
    for u in units {
        if width(&u) <= avail_w {
            fine.push(u);
            continue;
        }
        let mut acc = String::new();
        let mut acc_w = 0.0f32;
        for c in u.chars() {
            let w = width(&c.to_string());
            if !acc.is_empty() && acc_w + w > avail_w {
                fine.push(std::mem::take(&mut acc));
                acc_w = 0.0;
            }
            acc.push(c);
            acc_w += w;
        }
        if !acc.is_empty() {
            fine.push(acc);
        }
    }
    // 贪心装页
    let mut pages: Vec<String> = Vec::new();
    let mut page = String::new();
    let mut page_w = 0.0f32;
    for u in fine {
        let w = width(&u);
        if !page.is_empty() && page_w + w > avail_w {
            pages.push(std::mem::take(&mut page));
            page_w = 0.0;
        }
        page.push_str(&u);
        page_w += w;
    }
    if !page.is_empty() {
        pages.push(page);
    }
    if pages.is_empty() {
        pages.push(text.to_owned());
    }
    pages
}

/// 矢量图标 (替代 Python 版的 Segoe MDL2 字体字形, 不依赖系统字体)
fn icon_shapes(icon: Icon, r: Rect, color: Color32) -> Vec<Shape> {
    let c = r.center();
    let s = r.height() * 0.30; // 半尺寸
    let stroke = Stroke::new(2.2_f32, color);
    let mut v = Vec::new();
    match icon {
        Icon::Prev => {
            // |◀ : 竖条在最左, 三角形指向左 (朝起点)
            v.push(Shape::rect_filled(
                Rect::from_min_max(Pos2::new(c.x - s, c.y - s), Pos2::new(c.x - s * 0.6, c.y + s)),
                1.0,
                color,
            ));
            v.push(Shape::convex_polygon(
                vec![
                    Pos2::new(c.x + s * 0.75, c.y - s),
                    Pos2::new(c.x + s * 0.75, c.y + s),
                    Pos2::new(c.x - s * 0.35, c.y),
                ],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Next => {
            // ▶| : 三角形指向右 (朝终点), 竖条在最右
            v.push(Shape::convex_polygon(
                vec![
                    Pos2::new(c.x - s * 0.75, c.y - s),
                    Pos2::new(c.x - s * 0.75, c.y + s),
                    Pos2::new(c.x + s * 0.35, c.y),
                ],
                color,
                Stroke::NONE,
            ));
            v.push(Shape::rect_filled(
                Rect::from_min_max(Pos2::new(c.x + s * 0.6, c.y - s), Pos2::new(c.x + s, c.y + s)),
                1.0,
                color,
            ));
        }
        Icon::Play => {
            v.push(Shape::convex_polygon(
                vec![
                    Pos2::new(c.x - s * 0.65, c.y - s * 0.9),
                    Pos2::new(c.x - s * 0.65, c.y + s * 0.9),
                    Pos2::new(c.x + s * 0.85, c.y),
                ],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Pause => {
            for dx in [-0.65f32, 0.15] {
                v.push(Shape::rect_filled(
                    Rect::from_min_max(
                        Pos2::new(c.x + s * dx, c.y - s * 0.9),
                        Pos2::new(c.x + s * (dx + 0.5), c.y + s * 0.9),
                    ),
                    1.0,
                    color,
                ));
            }
        }
        Icon::Detect => {} // Detect 在按钮循环里用 painter.text 绘制, 不会走到这里
        Icon::Close => {
            let k = s * 0.75;
            v.push(Shape::line_segment(
                [Pos2::new(c.x - k, c.y - k), Pos2::new(c.x + k, c.y + k)],
                stroke,
            ));
            v.push(Shape::line_segment(
                [Pos2::new(c.x - k, c.y + k), Pos2::new(c.x + k, c.y - k)],
                stroke,
            ));
        }
    }
    v
}

fn set_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    // 优先级从高到低; 按反向 insert(0) 使最终顺序 = 列表顺序
    // 本机可用的更好选择: Noto Sans SC (思源黑体), 小字号下比微软雅黑更清晰
    let candidates: &[(&str, &str)] = &[
        ("noto-medium", r"C:\Windows\Fonts\Noto Sans SC Medium (TrueType).otf"),
        ("noto-regular", r"C:\Windows\Fonts\Noto Sans SC (TrueType).otf"),
        ("cjk", r"C:\Windows\Fonts\msyh.ttc"),
        ("cjk-fallback", r"C:\Windows\Fonts\simhei.ttf"),
    ];
    for (name, path) in candidates.iter().rev() {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert((*name).into(), FontData::from_owned(bytes));
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, (*name).into());
        }
    }
    ctx.set_fonts(fonts);
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ---- 播放器事件 ----
        while let Ok(ev) = self.player.events_rx.try_recv() {
            match ev {
                PlayerEvent::ChunkReady { chapter, text, total, duration, .. } => {
                    self.chapter = chapter;
                    self.total = total;
                    self.full_text = if text.is_empty() { "…".to_owned() } else { text };
                    self.chunk_dur = duration;
                    self.page_idx = 0;
                    self.page_started = Instant::now();
                    self.dirty_pages = true;
                    self.playing = true;
                }
                PlayerEvent::BookEnded => {
                    self.playing = false;
                    self.full_text = "全书播放完毕".to_owned();
                    self.pages = Vec::new();
                    self.dirty_pages = true;
                }
                PlayerEvent::Error(msg) => {
                    self.full_text = msg;
                    self.pages = Vec::new();
                    self.dirty_pages = true;
                }
            }
        }
        // ---- 全局热键 Ctrl+Alt+H ----
        // 窗口的实际隐藏/显示已由 hotkey 线程直接用 Win32 ShowWindow 完成,
        // 这里仅镜像状态 (隐藏后 update() 可能不再运行, 不能依赖这里做切换)
        while self.hotkey_rx.try_recv().is_ok() {
            self.visible = !self.visible;
        }
        // ---- 内容随窗口高度缩放: ppp = 物理高 / 110 ----
        // 关键: set_pixels_per_point 下一帧才生效, 且生效当帧 viewport 坐标仍按旧系数换算。
        // 若每帧用 rect.points * ctx.ppp 反推物理高, 会混用新旧两套系数 -> ppp 振荡 (字号闪烁)。
        // 因此只在 ppp 连续数帧稳定等于我们上次设定值时, 才信任物理高测量。
        let vp = ctx.input(|i| i.viewport().clone());
        let ppp_cur = ctx.pixels_per_point();
        let mut phy_ok = false;
        if let Some(rect) = vp.inner_rect {
            if self.applied_ppp > 0.0
                && (ppp_cur - self.applied_ppp).abs() / self.applied_ppp < 0.005
            {
                self.stable_frames = self.stable_frames.saturating_add(1);
            } else {
                self.stable_frames = 0;
            }
            if self.applied_ppp <= 0.0 || self.stable_frames >= 3 {
                // 首帧 zoom=1, rect.points 即物理 px; 之后仅在 ppp 稳定后测量
                self.measured_phy_h = rect.height() * ppp_cur;
            }
            let want = (self.measured_phy_h / BASE_H).clamp(0.15, 6.0);
            if self.applied_ppp <= 0.0 || (want - self.applied_ppp).abs() / self.applied_ppp > 0.01
            {
                ctx.set_pixels_per_point(want);
                self.applied_ppp = want;
                self.stable_frames = 0;
            }
            phy_ok = self.stable_frames >= 3;
        }
        // ---- 记录窗口位置 (物理 px, 退出时存盘; 仅在 ppp 一致时测量, 避免存入错误坐标) ----
        if phy_ok {
            if let Some(outer) = vp.outer_rect {
                self.cfg.window.x = Some((outer.min.x * ppp_cur) as i32);
                self.cfg.window.y = Some((outer.min.y * ppp_cur) as i32);
                self.cfg.window.w = outer.width() * ppp_cur;
                self.cfg.window.h = outer.height() * ppp_cur;
            }
        }
        // ---- A 按钮背景重检测结果应用 ----
        if let Ok(mut last) = self.bg_rx.try_recv() {
            while let Ok(v) = self.bg_rx.try_recv() {
                last = v;
            }
            if let Some(l) = last {
                // bg_detect: 亮度 >= 150 视为浅色背景
                self.palette = Palette::new(l >= 150.0);
            }
        }
        // ---- 分页轮播计时 ----
        if self.playing && self.pages.len() > 1 {
            let n = self.pages.len();
            let per = if self.chunk_dur > 0.0 {
                (self.chunk_dur / n as f64).max(1.5)
            } else {
                5.0
            };
            if self.page_started.elapsed().as_secs_f64() >= per {
                self.page_idx = (self.page_idx + 1) % n;
                self.page_started = Instant::now();
            }
        }

        // ================= UI =================
        let fg = self.palette.fg;
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::TRANSPARENT))
            .show(ctx, |ui| {
                let size = ui.available_size();
                let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
                // 半透明圆角面板: 填充+描边用单个 RectShape (同一光栅化路径)。
                // 之前 rect_filled + rect_stroke 两个形状在圆角处的 AA 接缝会叠出第二根细线
                let ppp_ui = ctx.pixels_per_point();
                let corner = CORNER_PX / ppp_ui;
                let panel = rect.shrink(0.5 / ppp_ui);
                ui.painter().add(Shape::Rect(egui::epaint::RectShape::new(
                    panel,
                    egui::Rounding::same(corner),
                    self.palette.fill,
                    Stroke::NONE,
                )));
                // ---- 边缘缩放命中检测 (无边框窗口 winit 不做 hit-test, 必须自己来) ----
                // 命中带 = 窗口四边 6 物理 px; 命中时显示系统缩放光标, 按下即进入 Windows
                // 模态缩放循环 (BeginResize -> winit drag_resize_window -> WM_NCLBUTTONDOWN),
                // 光标在模态循环内由系统接管, 缩放过程中字号随 ppp 自动跟随
                let edge = RESIZE_EDGE_PX / ppp_ui;
                self.resize_cursor = None;
                let mut in_edge = false;
                if let Some(p) = ctx.input(|i| i.pointer.latest_pos()) {
                    let north = p.y - rect.top() <= edge;
                    let south = rect.bottom() - p.y <= edge;
                    let west = p.x - rect.left() <= edge;
                    let east = rect.right() - p.x <= edge;
                    if north || south || west || east {
                        in_edge = true;
                        let (dir, icon) = match (north, south, west, east) {
                            (true, false, true, false) => (ResizeDirection::NorthWest, CursorIcon::ResizeNorthWest),
                            (true, false, false, true) => (ResizeDirection::NorthEast, CursorIcon::ResizeNorthEast),
                            (false, true, true, false) => (ResizeDirection::SouthWest, CursorIcon::ResizeSouthWest),
                            (false, true, false, true) => (ResizeDirection::SouthEast, CursorIcon::ResizeSouthEast),
                            (true, _, false, false) => (ResizeDirection::North, CursorIcon::ResizeNorth),
                            (false, true, false, false) => (ResizeDirection::South, CursorIcon::ResizeSouth),
                            (false, false, true, _) => (ResizeDirection::West, CursorIcon::ResizeWest),
                            _ => (ResizeDirection::East, CursorIcon::ResizeEast),
                        };
                        self.resize_cursor = Some(icon);
                        if ctx.input(|i| i.pointer.primary_pressed()) {
                            ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
                        }
                    }
                }
                // 空白处拖动移动窗口 (边缘命中带内让位给缩放)
                let drag = ui.interact(rect, egui::Id::new("panel_drag"), Sense::drag());
                if drag.drag_started() && !in_edge {
                    ctx.send_viewport_cmd(ViewportCommand::StartDrag);
                }
                // 垂直居中行: 文本 + 5 按钮
                let cy = rect.center().y;
                let bx0 = rect.right() - MARGIN_X - 5.0 * BTN_SIZE - 4.0 * GAP;
                let avail_w = (bx0 - GAP - (rect.left() + MARGIN_X)).max(60.0);
                let tx0 = rect.left() + MARGIN_X;
                // 分页 (宽度变化或新文本时重算)
                if self.dirty_pages || (self.last_avail_w - avail_w).abs() > 0.5 {
                    self.pages = split_pages(
                        ctx,
                        &self.full_text,
                        avail_w,
                        FontId::proportional(TEXT_SIZE),
                    );
                    if self.page_idx >= self.pages.len() {
                        self.page_idx = 0;
                    }
                    self.last_avail_w = avail_w;
                    self.dirty_pages = false;
                }
                let page = self
                    .pages
                    .get(self.page_idx)
                    .cloned()
                    .unwrap_or_else(|| self.full_text.clone());
                let galley = ui.painter().layout_no_wrap(
                    page,
                    FontId::proportional(TEXT_SIZE),
                    fg,
                );
                let ty = cy - galley.rect.height() * 0.5;
                ui.painter().galley(Pos2::new(tx0, ty), galley, fg);
                // 按钮 (与 Python 版顺序一致: 上一章 播/停 下一章 A 关闭)
                let mut bx = rect.right() - MARGIN_X - BTN_SIZE;
                let icons = [
                    Icon::Close,
                    Icon::Detect,
                    Icon::Next,
                    if self.playing { Icon::Pause } else { Icon::Play },
                    Icon::Prev,
                ]; // 从右往左布, 视觉左->右: 上一章 播/停 下一章 A 关闭
                let hover_fill = Color32::from_rgba_unmultiplied(fg.r(), fg.g(), fg.b(), 60);
                for icon in icons {
                    let br = Rect::from_min_size(Pos2::new(bx, cy - BTN_SIZE * 0.5), Vec2::splat(BTN_SIZE));
                    let resp = ui.allocate_rect(br, Sense::click());
                    if resp.hovered() {
                        ui.painter().rect_filled(br, BTN_SIZE * 0.5, hover_fill);
                    }
                    match icon {
                        Icon::Detect => {
                            ui.painter().text(
                                Pos2::new(br.center().x, br.center().y + BTN_SIZE * 0.05),
                                Align2::CENTER_CENTER,
                                "A",
                                FontId::proportional(ICON_SIZE),
                                fg,
                            );
                        }
                        other => {
                            for sh in icon_shapes(other, br, fg) {
                                ui.painter().add(sh);
                            }
                        }
                    }
                    if resp.clicked() {
                        match icon {
                            Icon::Close => {
                                // 即刻销毁: 上报进度 (同步 emit) + 存盘 + 硬退出, 不等音频/线程
                                self.player.shutdown();
                                config::save(&self.cfg);
                                std::process::exit(0);
                            }
                            Icon::Detect => {
                                // 必须用 Win32 ShowWindow 直接隐藏/显示:
                                // ViewportCommand::Visible(false) 隐藏后 winit 不再派发重绘,
                                // update() 停止运行, 无人恢复显示 -> 窗口回不来 (踩过的坑)
                                let (x, y, w, h) = (
                                    self.cfg.window.x.unwrap_or(0),
                                    self.cfg.window.y.unwrap_or(0),
                                    self.cfg.window.w.max(1.0) as i32,
                                    self.cfg.window.h.max(1.0) as i32,
                                );
                                let ctx2 = ctx.clone();
                                let tx = self.bg_tx.clone();
                                if let Some(hwnd) = find_hwnd() {
                                    let hwnd_raw = hwnd; // isize, 可跨线程; HWND 不是 Send
                                    unsafe {
                                        let _ = ShowWindow(
                                            windows::Win32::Foundation::HWND(hwnd_raw as *mut _),
                                            SW_HIDE,
                                        );
                                    }
                                    std::thread::spawn(move || {
                                        std::thread::sleep(Duration::from_millis(250));
                                        let lum = bg_detect::luminance(x, y, w, h);
                                        let _ = tx.send(lum);
                                        unsafe {
                                            let _ = ShowWindow(
                                                windows::Win32::Foundation::HWND(hwnd_raw as *mut _),
                                                SW_SHOW,
                                            );
                                            let _ = SetForegroundWindow(
                                                windows::Win32::Foundation::HWND(hwnd_raw as *mut _),
                                            );
                                        }
                                        ctx2.request_repaint();
                                    });
                                }
                            }
                            Icon::Play => {
                                self.player.play();
                                self.playing = true;
                                self.page_started = Instant::now();
                            }
                            Icon::Pause => {
                                self.player.pause();
                                self.playing = false;
                            }
                            Icon::Prev => {
                                self.jump_to(self.chapter.saturating_sub(1));
                            }
                            Icon::Next => {
                                let nxt = if self.total > 0 {
                                    (self.chapter + 1).min(self.total - 1)
                                } else {
                                    self.chapter + 1
                                };
                                self.jump_to(nxt);
                            }
                        }
                    }
                    bx -= BTN_SIZE + GAP;
                }
            });
        // 边缘缩放光标: 在 show() 之后写入, 是本帧对 cursor_icon 的最终写入,
        // 覆盖 panel_drag 拖拽 widget 的 Grab 光标
        if let Some(icon) = self.resize_cursor.take() {
            ctx.output_mut(|o| o.cursor_icon = icon);
        }
        // 轮播计时 + 事件轮询需要持续重绘
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // 必须全透明: eframe 默认清屏色是不透明深灰, 会在面板圆角外露出黑色/深灰的四个角
        egui::Color32::TRANSPARENT.to_normalized_gamma_f32()
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // 兜底: 走到这说明是系统级退出 (非 Close 按钮), 同样上报进度并存盘
        self.player.shutdown();
        config::save(&self.cfg);
    }
}

fn main() -> eframe::Result<()> {
    let cfg = config::load();
    // 初始窗口矩形 (config w/h 按逻辑点处理, 与 Python 物理px 在 100% 缩放下一致)
    let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) } as f32;
    let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) } as f32;
    let (w, h) = (cfg.window.w.min(screen_w), cfg.window.h.min(screen_h));
    let (x, y) = match (cfg.window.x, cfg.window.y) {
        (Some(x), Some(y)) => (x as f32, y as f32),
        _ => ((screen_w - w) * 0.5, (screen_h - h) * 0.5),
    };
    // 启动时按窗口位置采背景亮度定字色 (亮度 >= 150 视为浅色背景)
    let light = bg_detect::luminance(x as i32, y as i32, w as i32, h as i32)
        .map(|l| l >= 150.0)
        .unwrap_or(true);
    // 全局热键 Ctrl+Alt+H: manager 必须活到进程结束
    let manager = global_hotkey::GlobalHotKeyManager::new()
        .expect("GlobalHotKeyManager 初始化失败");
    let hk = HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyH);
    manager.register(hk).ok();
    std::mem::forget(manager); // 泄漏保持注册有效
    let (hotkey_tx, hotkey_rx) = mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || {
            use global_hotkey::GlobalHotKeyEvent;
            for ev in GlobalHotKeyEvent::receiver() {
                if ev.state == global_hotkey::HotKeyState::Pressed {
                    // 必须在本线程直接用 Win32 切换显示, 不能发给 update() 处理:
                    // 窗口隐藏后 winit 停止派发重绘, update() 不再运行,
                    // 发过去的事件永远没机会被消费 -> 窗口唤不回 (踩过的坑)
                    if let Some(raw) = find_hwnd() {
                        let hwnd = windows::Win32::Foundation::HWND(raw as *mut _);
                        unsafe {
                            if IsWindowVisible(hwnd).as_bool() {
                                let _ = ShowWindow(hwnd, SW_HIDE);
                            } else {
                                let _ = ShowWindow(hwnd, SW_SHOW);
                                let _ = SetForegroundWindow(hwnd);
                            }
                        }
                    }
                    let _ = hotkey_tx.send(());
                }
            }
        })
        .ok();
    // Win11 DWM 会自动给顶层窗口加圆角裁剪, 与我们自绘的圆角叠加很丑 — 关掉 (对齐 Python 版观感);
    // 同时去掉 DWM 的 1px 窗口描边和阴影: winit 无边框窗口默认带 DWM 阴影 + 非客户区一圈,
    // 叠在半透明面板外就是"不透明"的观感 (Python/Qt 版没有这层)
    std::thread::Builder::new()
        .name("dwm-corners".into())
        .spawn(|| {
            for _ in 0..100 {
                std::thread::sleep(Duration::from_millis(100));
                if let Some(hwnd) = find_hwnd() {
                    let hwnd = windows::Win32::Foundation::HWND(hwnd as *mut _);
                    let pref = DWMWCP_DONOTROUND.0;
                    let no_border = DWMWA_COLOR_NONE;
                    let no_nc = DWMNCRP_DISABLED.0;
                    let ok1 = unsafe {
                        DwmSetWindowAttribute(
                            hwnd,
                            DWMWA_WINDOW_CORNER_PREFERENCE,
                            &pref as *const i32 as *const core::ffi::c_void,
                            4,
                        )
                    };
                    let ok2 = unsafe {
                        DwmSetWindowAttribute(
                            hwnd,
                            DWMWA_BORDER_COLOR,
                            &no_border as *const u32 as *const core::ffi::c_void,
                            4,
                        )
                    };
                    let ok3 = unsafe {
                        DwmSetWindowAttribute(
                            hwnd,
                            DWMWA_NCRENDERING_POLICY,
                            &no_nc as *const i32 as *const core::ffi::c_void,
                            4,
                        )
                    };
                    // 关键: 撤掉 winit 无边框阴影 hack 扩展出的非客户区框架。
                    // winit 在 WM_NCCALCSIZE 把客户区顶部下移 1px 给 DWM 画阴影,
                    // DWM 就在窗口第 0 行画出一条 1px 的框线 (NCRENDERING_DISABLED 关不掉它);
                    // MARGINS 全 0 让 DWM 不再延伸框架, 第 0 行变为透明, 那条线随之消失
                    let margins = MARGINS { cxLeftWidth: 0, cxRightWidth: 0, cyTopHeight: 0, cyBottomHeight: 0 };
                    let ok4 = unsafe { DwmExtendFrameIntoClientArea(hwnd, &margins) };
                    if ok1.is_ok() && ok2.is_ok() && ok3.is_ok() && ok4.is_ok() {
                        return;
                    }
                }
            }
        })
        .ok();
    let (bg_tx, bg_rx) = mpsc::channel::<Option<f32>>();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_resizable(true) // winit 无边框窗口在 Windows 上原生支持边缘拖拽缩放+光标提示
            .with_min_inner_size([MIN_W, MIN_H])
            .with_inner_size([w, h])
            .with_position([x, y]),
        ..Default::default()
    };
    eframe::run_native(
        "voxtale",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, cfg, light, hotkey_rx, bg_rx, bg_tx)))),
    )
}
