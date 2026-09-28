# Heartale

一个自托管的 TTS 有声书 Web 服务：读取本地 txt 小说，通过 [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx)（Matcha-TTS 中文语音）合成语音，用浏览器在线听书。

## 功能特性

- **txt 文本源**：本地指定一个 txt 文件，自动解析「第X卷 / 第X章」章节结构
- **sherpa-onnx TTS**：Matcha-TTS 中文模型离线合成，不依赖任何在线 API
- **客户端驱动流式播放**：浏览器主动按需拉取音频块，服务端边合成边推送（二进制 WAV）
- **播放进度自动保存**：按「实际播放完成」回报进度，断线/退出自动落盘；磁盘写入节流（默认 10 分钟）
- **预取缓冲**：客户端最多预取 3 个音频块，保证连续播放、避免卡顿
- **文本同步显示**：可选边听边看当前文本
- **多端并发**：每个浏览器连接独立的阅读游标，互不干扰

## 架构

```
浏览器 (templates/index.html)
   │  Socket.IO (binary audio)
   ▼
app.py  ── Flask + Flask-SocketIO
   ├── text.py     # 文本源：章节解析、流式文本生成、进度管理
   └── sherpa.py   # TTS：sherpa-onnx Matcha 模型，文本→WAV bytes
```

- `request_next_audio`（客户端→服务端）：请求下一个音频块
- `audio_data`（服务端→客户端）：返回 `{chapterIndex, position, audio(binary)}`
- `chunk_played`（客户端→服务端）：一块播完后回报进度
- `audio_end` / `text_data` / `text_sync`：结束标记与文本同步

## 目录结构

```
heartale/
├── app.py                  # 入口：Flask + SocketIO 路由与事件
├── text.py                 # txt 文本源：章节解析、流式生成、进度保存
├── sherpa.py               # sherpa-onnx Matcha TTS 引擎
├── logger.py               # 日志初始化
├── config/
│   ├── config.yaml         # 服务与 TTS 配置
│   └── logger.yaml         # 日志格式与输出配置
├── templates/
│   └── index.html          # 播放器页面
├── static/vendor/
│   └── socket.io.js        # 本地化的 socket.io 客户端（离线可用）
├── deploy/
│   └── heartale.service    # systemd 单元文件
├── requirements.txt        # Python 依赖（版本固定）
├── models/                 # TTS 模型目录（不入库，手动放置）
├── storage/                # 书籍 txt 文件（不入库）
└── logs/                   # 日志与进度文件（不入库）
```

## 环境要求

- Python 3.12+（建议 3.12）
- Linux / macOS / Windows 均可；内网自用推荐 Linux + systemd

## 安装

```bash
# 1. 创建虚拟环境（本项目约定放项目内 venv/）
python3 -m venv venv
source venv/bin/activate

# 2. 安装依赖（如网络受限，可加 -i https://pypi.tuna.tsinghua.edu.cn/simple）
pip install -r requirements.txt

# 3. 放置 TTS 模型
#    从 sherpa-onnx 发布页下载 matcha-icefall-zh-baker 模型，解压到 models/ 下
#    models/matcha-icefall-zh-baker/ 需包含 model-steps-3.onnx / hifigan_v2.onnx 等文件

# 4. 放置书籍
#    把你的 txt 小说放到 storage/text/ 下，然后在 config/config.yaml 指定路径
```

## 配置

编辑 `config/config.yaml`：

```yaml
server:
  path: ~/workspace/heartale/storage/text/temp.txt  # 待阅读的 txt 文件路径（~ 会自动展开）
  encoding: utf-8
tts:
  model_folder: ./models/matcha-icefall-zh-baker  # 模型目录（相对项目根）
  model: model-steps-3.onnx
  vocoder: hifigan_v2.onnx
  lexicon: lexicon.txt
  dict_dir: dict
  tokens: tokens.txt
  rule_fsts: phone.fst, number.fst, date.fst
  sid: 0
  speed: 1.3               # 语速
```

## 运行

```bash
python app.py
```

- 服务监听 `0.0.0.0:28081`
- 浏览器访问 `http://<服务器IP>:28081/index`（页面由 `/index` 提供）

## 部署（systemd）

内网长期运行推荐注册为 systemd 服务。先将 `deploy/heartale.service` 中的 `User=your_username` 改为你的实际用户名（`%h` 会自动展开为对应家目录），然后：

```bash
sudo cp deploy/heartale.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now heartale
```

如果启用了 ufw 防火墙，放行端口：

```bash
sudo ufw allow 28081/tcp
```

## HTTP 接口（补充）

- `GET /index` — 播放器页面
- `GET /tts` — 独立音频流（每次返回下一个 WAV 块，供非浏览器客户端使用）
- `GET|POST /save?index=N&pos=M` — 手动保存进度

## 说明

- 进度文件保存在 `logs/text_<md5>.json`，按书籍 md5 区分，记录章/节位置
- 进度磁盘写入默认节流 10 分钟；断连或进程退出时强制落盘
- 每个浏览器会话拥有独立阅读游标，刷新页面从各自上次位置继续

## 致谢

本项目最初 fork 自 [yuhldr/heartale](https://github.com/yuhldr/heartale)，感谢原作者 [yuhldr](https://github.com/yuhldr) 的开源工作。当前仓库在其基础上进行了较大重构与扩展（TTS 流程改进、Rust 桌面悬浮客户端等），已作为独立项目发展，与上游仓库不再保持同步。
