//! 拉流播放控制器: 移植自 Python 版 Player (双线程 + 条件变量 + generation 代际号)。
//!
//! - fetch 线程: 播放中持续预取, 缓冲不足 PREFETCH 时 GET /tts; 处理跳章 (GET ?index&pos);
//!   204 -> 全书结束; 请求结果回来时校验代际号, 作废在途拉取
//! - play 线程: 从缓冲弹出块 -> 先发 ChunkReady (文本随块开始播放同步, 与 web playNext 一致)
//!   -> rodio Sink 播放; 轮询 stop/paused/jump (20ms), 不需要 Python 版
//!   "暂停块塞回队首重播" 的绕路 (rodio 可原地暂停/恢复)
//! - 关键: 弹出块后必须 notify_all 唤醒预取线程, 否则初始 PREFETCH 块播完后
//!   双线程互等死锁 (Python 版踩过的坑)
//! - 暂停/恢复与服务端进度的对齐: 暂停时上报当前块并记 pause_pos;
//!   恢复时清空缓冲从 pause_pos 重新拉流, 避免预取块相对服务端超前导致复播重复

use percent_encoding::percent_decode_str;
use rodio::{Decoder, OutputStream, Sink};
use serde_json::json;
use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::reporter::Reporter;

const PREFETCH: usize = 3;

#[derive(Clone, Debug)]
pub struct Chunk {
    pub chapter: u32,
    pub pos: u64,
    #[allow(dead_code)]
    pub title: String,
    pub text: String,
    pub total: u32,
    pub is_end: bool,
    pub duration: f64,
    pub audio: Vec<u8>,
}

pub enum PlayerEvent {
    ChunkReady { chapter: u32, #[allow(dead_code)] pos: u64, text: String, total: u32, duration: f64 },
    BookEnded,
    Error(String),
}

struct Shared {
    buffer: VecDeque<Chunk>,
    paused: bool,
    jump: Option<(u32, u64)>,
    generation: u64,
    book_ended: bool,
    stop: bool,
    /// 最后一个非 is_end 块的位置 (恢复播放兜底)
    cur: (u32, u64),
    /// 当前(最近开始)播放中的块, 用于进度上报
    playing_cur: Option<(u32, u64)>,
    /// 暂停时上报过的位置, 恢复时据此重新对齐服务端
    pause_pos: Option<(u32, u64)>,
}

pub struct Player {
    shared: Arc<(Mutex<Shared>, Condvar)>,
    reporter: Reporter,
    sink: Option<Arc<Sink>>,
    _stream: Option<OutputStream>,
    #[allow(dead_code)] // 供未来扩展; 当前通过克隆传给线程
    events_tx: mpsc::Sender<PlayerEvent>,
    pub events_rx: mpsc::Receiver<PlayerEvent>,
}

impl Player {
    pub fn new(base_url: &str) -> Self {
        let (events_tx, events_rx) = mpsc::channel();
        let (_stream, sink) = match OutputStream::try_default() {
            Ok((stream, handle)) => (Some(stream), Sink::try_new(&handle).ok().map(Arc::new)),
            Err(e) => {
                eprintln!("[audio] 初始化失败: {e}, 将按时长模拟播放");
                (None, None)
            }
        };
        let this = Self {
            shared: Arc::new((
                Mutex::new(Shared {
                    buffer: VecDeque::new(),
                    paused: false, // 启动即播放 (对应 Python 版 main 里的 _toggle_play)
                    jump: None,
                    generation: 0,
                    book_ended: false,
                    stop: false,
                    cur: (0, 0),
                    playing_cur: None,
                    pause_pos: None,
                }),
                Condvar::new(),
            )),
            reporter: Reporter::new(base_url.trim_end_matches('/').to_owned()),
            sink,
            _stream,
            events_tx: events_tx.clone(),
            events_rx,
        };
        let shared = this.shared.clone();
        let http = Arc::new(
            reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
        );
        let base = base_url.trim_end_matches('/').to_owned();
        let tx = events_tx.clone();
        std::thread::Builder::new()
            .name("tts-fetch".into())
            .spawn(move || fetch_loop(shared, http, base, tx))
            .ok();
        let shared = this.shared.clone();
        let sink = this.sink.clone();
        let tx = events_tx;
        std::thread::Builder::new()
            .name("tts-play".into())
            .spawn(move || play_loop(shared, sink, tx))
            .ok();
        this
    }

    fn lock(&self) -> (std::sync::MutexGuard<'_, Shared>, &Condvar) {
        let (lock, cvar) = &*self.shared;
        (lock.lock().unwrap(), cvar)
    }

    /// 恢复播放; 暂停时上报过进度, 恢复时清空缓冲从上报位置重新拉流
    pub fn play(&self) {
        let (mut sh, cvar) = self.lock();
        if !sh.paused {
            return;
        }
        sh.paused = false;
        if sh.book_ended {
            sh.book_ended = false;
        }
        if let Some(p) = sh.pause_pos.take() {
            sh.generation += 1;
            sh.jump = Some(p);
            sh.buffer.clear();
        } else if sh.buffer.is_empty() && sh.cur != (0, 0) {
            sh.generation += 1;
            sh.jump = Some(sh.cur);
        }
        cvar.notify_all();
    }

    /// 暂停: 记录当前块并上报进度 (服务端回拨), 播放线程 20ms 内暂停 Sink
    pub fn pause(&self) {
        let cur;
        {
            let (mut sh, cvar) = self.lock();
            if sh.paused {
                return;
            }
            sh.pause_pos = sh.playing_cur;
            sh.paused = true;
            cvar.notify_all();
            cur = sh.playing_cur;
        }
        if let Some((ch, pos)) = cur {
            self.reporter.report(ch, pos);
        }
    }

    /// 跳章: 代际号递增作废在途拉取; playing_cur 设为目标 (关闭时上报用)
    pub fn jump(&self, chapter: u32, pos: u64) {
        let (mut sh, cvar) = self.lock();
        sh.generation += 1;
        sh.jump = Some((chapter, pos));
        sh.book_ended = false;
        sh.pause_pos = None;
        sh.playing_cur = Some((chapter, pos));
        sh.buffer.clear();
        sh.paused = false;
        cvar.notify_all();
    }

    /// 关闭/退出: 上报当前进度 + 停止播放; 线程随进程退出, 不 join
    pub fn shutdown(&self) {
        self.report_current();
        {
            let (mut sh, cvar) = self.lock();
            sh.stop = true;
            cvar.notify_all();
        }
        if let Some(s) = &self.sink {
            s.stop();
        }
    }

    fn report_current(&self) {
        let (sh, _) = self.lock();
        if let Some((ch, pos)) = sh.playing_cur {
            if !sh.book_ended {
                self.reporter.report(ch, pos);
            }
        }
    }
}

fn fetch_loop(
    shared: Arc<(Mutex<Shared>, Condvar)>,
    http: Arc<reqwest::blocking::Client>,
    base: String,
    tx: mpsc::Sender<PlayerEvent>,
) {
    let (lock, cvar) = &*shared;
    let mut sh = lock.lock().unwrap();
    loop {
        if sh.stop {
            return;
        }
        // 等待需要预取: 未暂停且 (有 jump 或缓冲不足) 且未到全书末尾
        while !sh.stop {
            if sh.paused {
                sh = cvar.wait(sh).unwrap();
            } else if sh.jump.is_some() || sh.buffer.len() < PREFETCH && !sh.book_ended {
                break;
            } else {
                sh = cvar.wait(sh).unwrap();
            }
        }
        if sh.stop {
            return;
        }
        let jump = sh.jump.take();
        let gen = sh.generation;
        drop(sh);

        let result = fetch_chunk(&http, &base, jump);

        sh = lock.lock().unwrap();
        // 作废在途拉取: 已暂停 / 代际号变化
        if sh.stop || sh.paused || gen != sh.generation {
            continue;
        }
        match result {
            Ok(Fetched::Ended) => {
                sh.book_ended = true;
                cvar.notify_all();
            }
            Ok(Fetched::Chunk(c)) => {
                if !c.is_end {
                    sh.cur = (c.chapter, c.pos);
                }
                sh.buffer.push_back(c);
                cvar.notify_all();
            }
            Err(e) => {
                drop(sh);
                let _ = tx.send(PlayerEvent::Error(format!("连接失败: {e}")));
                std::thread::sleep(Duration::from_secs(2));
                sh = lock.lock().unwrap();
            }
        }
    }
}

fn play_loop(
    shared: Arc<(Mutex<Shared>, Condvar)>,
    sink: Option<Arc<Sink>>,
    tx: mpsc::Sender<PlayerEvent>,
) {
    let (lock, cvar) = &*shared;
    loop {
        let mut sh = lock.lock().unwrap();
        if sh.stop {
            if let Some(s) = &sink {
                s.stop();
            }
            return;
        }
        while !sh.stop && sh.paused && sh.jump.is_none() {
            sh = cvar.wait(sh).unwrap();
        }
        if sh.stop {
            if let Some(s) = &sink {
                s.stop();
            }
            return;
        }
        let chunk = if sh.buffer.is_empty() && sh.book_ended {
            // 全书播完: 只报一次结束, 随后暂停等待重启
            sh.book_ended = false;
            sh.paused = true;
            sh.playing_cur = None;
            cvar.notify_all();
            drop(sh);
            let _ = tx.send(PlayerEvent::BookEnded);
            continue;
        } else if let Some(c) = sh.buffer.pop_front() {
            // 关键: 弹出后唤醒预取线程, 否则它沉睡在 "缓冲已满" 的 wait 里 (死锁教训)
            cvar.notify_all();
            sh.playing_cur = Some((c.chapter, c.pos));
            drop(sh);
            c
        } else {
            sh = cvar.wait(sh).unwrap();
            drop(sh);
            continue;
        };
        // 文本在块开始播放时同步 (与 web 端 playNext 一致)
        let _ = tx.send(PlayerEvent::ChunkReady {
            chapter: chunk.chapter,
            pos: chunk.pos,
            text: chunk.text.clone(),
            total: chunk.total,
            duration: chunk.duration,
        });
        if playback(&shared, &sink, chunk) {
            // 播放中发现 stop: 线程退出
            return;
        }
        // 回到持锁段; chunk 播完后自然丢弃 (无需回队, 恢复走 jump 重新拉流)
    }
}

/// 播放一块音频, 内部 20ms 轮询 stop/paused/jump。
/// 返回 true 表示因 stop 退出 (线程应结束)。
fn playback(
    shared: &Arc<(Mutex<Shared>, Condvar)>,
    sink: &Option<Arc<Sink>>,
    chunk: Chunk,
) -> bool {
    let (lock, _) = &**shared;
    match sink {
        Some(s) => {
            s.clear();
            let dec = match Decoder::new(Cursor::new(chunk.audio)) {
                Ok(d) => d,
                Err(e) => {
                    // 解码失败: 跳过该块 (不回报进度, 避免跳读)
                    eprintln!("[audio] 解码失败: {e}");
                    return false;
                }
            };
            s.append(dec);
            std::thread::sleep(Duration::from_millis(30)); // 等 Sink 装载, 避免 is_empty 误判
            loop {
                let sh = lock.lock().unwrap();
                if sh.stop {
                    s.stop();
                    return true;
                }
                if sh.jump.is_some() {
                    s.clear();
                    return false;
                }
                if sh.paused {
                    s.pause();
                } else {
                    s.play();
                }
                let empty = s.empty();
                drop(sh);
                if empty {
                    return false; // 播完
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        None => {
            // 无音频设备: 按时长模拟, 保持文本同步节奏
            let mut slept = 0.0f64;
            let dur = if chunk.duration > 0.0 { chunk.duration } else { 0.1 };
            while slept < dur {
                {
                    let sh = lock.lock().unwrap();
                    if sh.stop || sh.paused || sh.jump.is_some() {
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
                slept += 0.05;
            }
            false
        }
    }
}

enum Fetched {
    Ended,
    Chunk(Chunk),
}

fn fetch_chunk(
    http: &reqwest::blocking::Client,
    base: &str,
    jump: Option<(u32, u64)>,
) -> Result<Fetched, String> {
    let mut req = http.get(format!("{base}/tts"));
    if let Some((ch, pos)) = jump {
        req = req.query(&[("index", ch.to_string()), ("pos", pos.to_string())]);
    }
    let resp = req.send().map_err(|e| e.to_string())?;
    if resp.status() == reqwest::StatusCode::NO_CONTENT {
        return Ok(Fetched::Ended);
    }
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    let headers = resp.headers().clone();
    let audio = resp.bytes().map_err(|e| e.to_string())?.to_vec();
    let header = |k: &str| -> String {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    let chapter: u32 = header("X-Chapter-Index").parse().unwrap_or(0);
    let pos: u64 = header("X-Position").parse().unwrap_or(0);
    let total: u32 = header("X-Total").parse().unwrap_or(0);
    let title = pct(&header("X-Chapter-Title"));
    let text = pct(&header("X-Text"));
    let is_end = header("X-Is-End") == "1";
    Ok(Fetched::Chunk(Chunk {
        chapter,
        pos,
        title,
        text,
        total,
        is_end,
        duration: wav_duration(&audio),
        audio,
    }))
}

fn pct(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().to_string()
}

/// 解析 WAV 头返回音频时长(秒); 非法/解析失败返回 0.0 (用于长文本分页轮播)
pub fn wav_duration(data: &[u8]) -> f64 {
    if data.len() < 44 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return 0.0;
    }
    let mut pos = 12usize;
    let (mut byte_rate, mut data_size) = (0u32, 0u32);
    while pos + 8 <= data.len() {
        let cid = &data[pos..pos + 4];
        let size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap());
        if cid == b"fmt " {
            if pos + 20 <= data.len() {
                byte_rate = u32::from_le_bytes(data[pos + 16..pos + 20].try_into().unwrap());
            }
        } else if cid == b"data" {
            data_size = if size > 0 { size } else { (data.len() - pos - 8) as u32 };
            break;
        }
        pos += 8 + size as usize + (size & 1) as usize; // chunk 按 2 字节对齐
    }
    if byte_rate > 0 && data_size > 0 {
        data_size as f64 / byte_rate as f64
    } else {
        0.0
    }
}

// json! 仅在 reporter 使用; 这里保留依赖提示
#[allow(dead_code)]
fn _json_marker() -> serde_json::Value {
    json!(null)
}
