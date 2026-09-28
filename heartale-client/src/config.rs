//! config.json 读写 (与 Python 版格式一致): server + window {x,y,w,h}
//!
//! 路径: exe 同目录 config.json, 可用环境变量 HEARTALE_CONFIG 覆盖;
//! 保存采用 tmp + rename 原子写。

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

fn default_server() -> String {
    "http://10.0.1.125:28081".to_owned()
}
fn default_w() -> f32 {
    620.0
}
fn default_h() -> f32 {
    110.0
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct WindowCfg {
    pub x: Option<i32>,
    pub y: Option<i32>,
    #[serde(default = "default_w")]
    pub w: f32,
    #[serde(default = "default_h")]
    pub h: f32,
}

impl Default for WindowCfg {
    fn default() -> Self {
        Self { x: None, y: None, w: default_w(), h: default_h() }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    #[serde(default = "default_server")]
    pub server: String,
    #[serde(default)]
    pub window: WindowCfg,
}

impl Default for Config {
    fn default() -> Self {
        Self { server: default_server(), window: WindowCfg::default() }
    }
}

pub fn config_path() -> PathBuf {
    match std::env::var("HEARTALE_CONFIG") {
        Ok(p) => PathBuf::from(p),
        Err(_) => std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("config.json"),
    }
}

pub fn load() -> Config {
    let path = config_path();
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<Config>(&bytes).unwrap_or_default(),
        Err(_) => Config::default(),
    }
}

pub fn save(cfg: &Config) {
    let path = config_path();
    let Ok(json) = serde_json::to_vec_pretty(cfg) else { return };
    let tmp = path.with_extension("json.tmp");
    if fs::write(&tmp, json).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
}
