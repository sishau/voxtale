//! socket.io 进度上报 (对应 web 端 `chunk_played` 事件 / Python 版 ProgressReporter)。
//!
//! 背景: 预取会让服务端 position 超前实际播放位置, 暂停/退出时需回报
//! "当前正在播放的块" 的 (chapterIndex, position), 服务端据此回拨进度。
//! 独立线程维持长连接; 服务端不可达时静默重试, 不影响播放。

use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Reporter {
    client: Arc<Mutex<Option<rust_socketio::client::Client>>>,
    connected: Arc<AtomicBool>,
}

impl Reporter {
    pub fn new(base_url: String) -> Self {
        let client: Arc<Mutex<Option<rust_socketio::client::Client>>> = Arc::new(Mutex::new(None));
        let connected = Arc::new(AtomicBool::new(false));
        let c = client.clone();
        let st = connected.clone();
        let _ = std::thread::Builder::new()
            .name("progress-reporter".into())
            .spawn(move || loop {
                match rust_socketio::ClientBuilder::new(&base_url)
                    .on("connect", {
                        let st = st.clone();
                        move |_, _| st.store(true, Ordering::Relaxed)
                    })
                    .on("disconnect", {
                        let st = st.clone();
                        move |_, _| st.store(false, Ordering::Relaxed)
                    })
                    .on("error", |e, _| {
                        eprintln!("[socketio] error: {e:?}");
                    })
                    .connect()
                {
                    Ok(cl) => {
                        st.store(true, Ordering::Relaxed);
                        *c.lock().unwrap() = Some(cl);
                        // 客户端内部线程维持连接; 轮询断开标记, 断开后重连
                        while st.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(500));
                        }
                        *c.lock().unwrap() = None;
                    }
                    Err(_) => { /* 服务端不可达, 稍后重试 */ }
                }
                std::thread::sleep(Duration::from_secs(5));
            });
        Self { client, connected }
    }

    /// 上报播放进度; 未连接时静默失败
    pub fn report(&self, chapter: u32, pos: u64) -> bool {
        if !self.connected.load(Ordering::Relaxed) {
            return false;
        }
        let guard = self.client.lock().unwrap();
        match guard.as_ref() {
            Some(cl) => cl
                .emit("chunk_played", json!({"chapterIndex": chapter, "position": pos}))
                .is_ok(),
            None => false,
        }
    }
}
