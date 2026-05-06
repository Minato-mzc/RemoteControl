# RemoteControl Wire Protocol

版本：**0.6** （M8 阶段，新增剪贴板手动同步；M1-M5 全部向下兼容）

## 传输层

| 通道 | 用途 | 实现 |
|---|---|---|
| 控制平面 | 配对、握手、心跳、流控制信令 | WebSocket **Text** frame，UTF-8 JSON |
| 数据平面 | H.264 视频帧（M2）、未来音频/控制输入 | WebSocket **Binary** frame，自定义二进制头 + 负载 |

> M2 把控制和数据都复用同一个 `/ws` WebSocket 连接。M4 性能优化阶段如果发现
> TCP 头部阻塞导致延迟问题，再考虑把视频流挪到独立 QUIC 通道。

M2 仍用明文 `ws://`；M1.5 升 `wss://` + 自签证书 + 指纹校验的计划不变（独立推进）。

## 二维码 Payload

URL 形式（v=2 同时兼容 v=1 客户端进行老版协商）：

```
rc://<host>:<port>/?v=2&c=<code>&k=<key_b64url>
```

字段含义和 v0.1 一致。`v` 字段含义升级：

| v | 含义 |
|---|---|
| 1 | M1：仅握手（`hello` / `welcome` / `ping` / `pong` / `error`） |
| 2 | M2：在 v1 基础上新增 stream 控制消息 + binary 视频帧通道 |
| 3 | M3：在 v2 基础上新增鼠标输入消息（C→S：`mouse_move` / `mouse_button` / `mouse_scroll`） |
| 4 | M3.5：在 v3 基础上新增键盘输入消息（C→S：`key_text` / `key_event`） |
| 5 | M5：在 v4 基础上新增音频串流（`stream_started.audio` + binary frame_type=0x02） |
| 6 | M8：在 v5 基础上新增手动剪贴板同步（C↔S：`clipboard_set` / `clipboard_get` / `clipboard_text`） |

向下兼容：v=2 服务器接受 v=1 客户端 hello（不会有 stream 能力，但不报 version_mismatch）。v=1 服务器拒绝 v=2 客户端（返回 version_mismatch）。

---

## 控制平面消息（JSON）

### M1 已有消息（保持不变）

| 方向 | type | 用途 |
|---|---|---|
| C→S | `hello` | 握手请求 |
| S→C | `welcome` | 握手成功 |
| S→C | `error` | 各种错误 |
| C→S | `ping` | 心跳 |
| S→C | `pong` | 心跳应答 |

详见 [v0.1 历史](#历史--v01-握手详细规范)。

### M2 新增消息

#### `C→S: stream_request` —— 客户端请求开启视频流

```jsonc
{
  "type": "stream_request",
  "codec": "h264",                 // 当前仅支持 "h264"
  "max_bitrate_kbps": 12000,       // 客户端建议的码率上限；服务器可低于此值
  "max_fps": 60,                    // 客户端建议的帧率上限
  "prefer_keyframe_interval_ms": 1000  // 建议 IDR 间隔（M2 服务器固定 1000）
}
```

服务器收到后，启动抓屏+编码 pipeline。第一帧推出去前先发 `stream_started`。

#### `S→C: stream_started` —— 视频流已开启，告知解码参数

```jsonc
{
  "type": "stream_started",
  "stream_id": "<uuid>",          // 该 stream 实例 ID（断流/重连时区分）
  "codec": "h264",
  "profile": "high",              // baseline / main / high
  "level": "4.2",                 // H.264 level（信息性，解码端通常不强校验）
  "width": 1920,
  "height": 1080,
  "fps": 60,
  "bitrate_kbps": 8000,           // 实际目标码率
  "keyframe_interval_frames": 60, // IDR 间隔
  "pixel_format": "yuv420p",
  "started_at_unix_ms": 1713000000000,

  // M5: optional audio sub-stream metadata. Absent on video-only streams.
  "audio": {
    "codec": "opus",
    "sample_rate": 48000,
    "channels": 2,
    "frame_size_ms": 20,
    "bitrate_kbps": 64,
    "csd_0_b64": "<base64 of Opus ID Header>",
    "csd_1_b64": "<base64 of pre-skip ns, LE i64>",
    "csd_2_b64": "<base64 of seek pre-roll ns, LE i64>"
  }
}
```

`csd_*` 是 Android `MediaCodec` 配置 Opus 解码器需要的 codec-specific data（[doc](https://developer.android.com/reference/android/media/MediaCodec#initialization)）。服务端 hardcode 这些值（来自 audiopus 默认参数）。

#### `S→C: stream_stopped` —— 服务器侧停止流

```jsonc
{
  "type": "stream_stopped",
  "stream_id": "<uuid>",
  "reason": "client_requested" | "encoder_error" | "capture_error" | "server_shutdown",
  "msg": "<可选 detail>"
}
```

#### `C→S: stream_stop` —— 客户端要求停止流

```jsonc
{ "type": "stream_stop", "stream_id": "<uuid>" }
```

服务器收到后停 pipeline 并发 `stream_stopped { reason: "client_requested" }` 确认。

#### `C→S: keyframe_request` —— 客户端要求服务器立刻发 IDR

用于客户端 App 切回前台、丢包后画面花屏等场景。

```jsonc
{ "type": "keyframe_request", "stream_id": "<uuid>" }
```

服务器尽力下一帧编 IDR，但不保证立即（编码器可能正在处理 GOP 中间）。

### M3 新增消息（鼠标输入，C→S）

**只在 `authenticated=true` 时接受。** 不需要 stream 处于活跃状态。

#### `C→S: mouse_move` —— 鼠标绝对位置

坐标归一化到 `[0.0, 1.0]`，由服务器按主显示器分辨率缩放。`x=0,y=0` 屏幕左上，`x=1,y=1` 屏幕右下。

```jsonc
{ "type": "mouse_move", "x": 0.5234, "y": 0.4187 }
```

服务器使用 `SendInput` 配合 `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE` 注入。

#### `C→S: mouse_button` —— 鼠标按键按下/抬起

```jsonc
{ "type": "mouse_button", "button": "left" | "right" | "middle", "down": true }
```

一次"点击"由两条消息组成：`down: true` 然后 `down: false`。"拖拽"是 `down: true` + 多个 `mouse_move` + `down: false`。

#### `C→S: mouse_scroll` —— 鼠标滚轮

```jsonc
{ "type": "mouse_scroll", "dx": 0, "dy": -3 }
```

`dx`/`dy` 单位是 **wheel notch**（标准滚轮一格）。正值：右 / 上滚；负值：左 / 下滚。服务器内部乘以 `WHEEL_DELTA`（120）。

### M3.5 新增消息（键盘输入，C→S）

#### `C→S: key_text` —— Unicode 文本注入

```jsonc
{ "type": "key_text", "text": "你好 hello" }
```

服务器对字符串中**每个 Unicode 标量**调用 `SendInput` + `KEYEVENTF_UNICODE`（down + up），相当于按一个键盘上不存在的键直接产生该字符。BMP 之外的字符（emoji 等）会被拆成 surrogate pair，多数 Windows 应用接受。**不走 PC 端 IME**——汉字直接注入，不会触发拼音候选窗。

#### `C→S: key_event` —— 虚拟键 down / up

```jsonc
{ "type": "key_event", "vk": 27, "down": true }     // VK_ESCAPE
{ "type": "key_event", "vk": 27, "down": false }
```

`vk` 是 Win32 [Virtual-Key Code](https://learn.microsoft.com/en-us/windows/win32/inputdev/virtual-key-codes)。一次"按下抬起"必须发两条消息（`down=true` 然后 `down=false`）。M3.5 主要用于：`VK_ESCAPE` `VK_TAB` `VK_BACK` `VK_RETURN` `VK_LEFT/UP/RIGHT/DOWN` `VK_HOME/END/PRIOR/NEXT` `VK_F1..F12` `VK_LWIN` `VK_SNAPSHOT`。

> 组合键（Ctrl+C 等）M3.5 暂不支持，留到 M7 快捷命令做。

### M8 新增消息（剪贴板手动同步，双向）

只支持纯文本。每次同步由用户手动触发（无自动监听），避免循环、隐私问题。

#### `C→S: clipboard_set` —— 把手机剪贴板**推送**到 PC

```jsonc
{ "type": "clipboard_set", "text": "https://example.com/..." }
```

服务器收到后调用 Win32 `SetClipboardData(CF_UNICODETEXT, ...)` 写入 PC 系统剪贴板。

#### `C→S: clipboard_get` —— **拉取** PC 当前剪贴板

```jsonc
{ "type": "clipboard_get" }
```

服务器读取 PC 剪贴板（`GetClipboardData(CF_UNICODETEXT)`）并以 `clipboard_text` 回应。

#### `S→C: clipboard_text` —— 服务器返回 PC 剪贴板内容

```jsonc
{ "type": "clipboard_text", "text": "..." }
```

仅在客户端 `clipboard_get` 之后发送，不会主动推送。

---

## 数据平面：视频帧二进制格式

每个 WebSocket **binary** frame = **1 个完整的视频帧**（一组带同一 PTS 的 NAL units）。

### 帧头（12 bytes，固定，所有字段网络序之外的多字节都是 little-endian）

| Offset | Size | 字段 | 说明 |
|---|---|---|---|
| 0 | 1 | `frame_type` | `0x01` = video（H.264）· `0x02` = audio（Opus，M5） |
| 1 | 1 | `flags` | video: bit0=`keyframe`（IDR） · bit1=`config`（payload 包含 SPS/PPS 在前）。audio: 保留为 0 |
| 2 | 2 | `reserved` | 保留位，必须为 0 |
| 4 | 8 | `pts_us` | LE u64，相对 stream_started 的呈现时间戳，单位微秒。**视频和音频共用同一个时钟原点** |

### Payload（变长）

**Video（frame_type=0x01）**：H.264 **Annex-B** 格式 NAL units，连续拼接，NAL 之间用 `00 00 00 01` 起始码分隔：

- **IDR 帧**（flags bit0=1, bit1=1）：`[startcode][SPS][startcode][PPS][startcode][IDR slice]`
- **P 帧**：`[startcode][P slice]`

服务器**每个 IDR 都会内联重新发送 SPS/PPS**，不依赖客户端缓存。客户端可直接把整个 payload 喂给 `MediaCodec`。

**Audio（frame_type=0x02，M5）**：一个 Opus 数据包（self-delimited 不需要外层 framing）。每包对应 20ms (frame_size_ms 默认 20) 的 48kHz stereo PCM。客户端把 payload 直接喂给 Opus `MediaCodec`，输出 PCM 转给 `AudioTrack`。

### 帧大小

- M2 目标 1080p60 8Mbps → 平均每帧 ~17 KB，IDR 可能 50-100 KB
- WebSocket 单 frame 大小限制由实现定，OkHttp 默认 16 MB，足够。tokio-tungstenite 默认 64 MB，也足够。

---

## 流生命周期

```
M1 握手成功
  ↓
客户端 → stream_request
  ↓
服务器：启动 capture + encoder pipeline
  ↓
服务器 → stream_started { stream_id, width, height, ... }
  ↓
服务器 → [binary frame: IDR + SPS/PPS] (frame_type=video, flags=keyframe|config)
  ↓
服务器 → [binary frame: P 帧] × N
  ↓
（每 60 帧 / 1 秒重发 IDR）
  ↓
... 持续
  ↓
客户端 → stream_stop  或  WebSocket 断开
  ↓
服务器：停 pipeline，→ stream_stopped
```

---

## 错误码（v0.1 + 新增）

| code | 含义 |
|---|---|
| `bad_pairing_code` | 配对码错误 |
| `code_expired` | 配对码已超时 |
| `code_used` | 配对码已被使用 |
| `version_mismatch` | 协议版本不兼容 |
| `malformed` | 消息格式错误 |
| **`stream_unavailable`** | M2: 未知 codec、硬件不支持等 |
| **`stream_already_running`** | M2: 当前会话已有活跃流，需先 stop |
| **`not_authenticated`** | 未握手前发了控制消息 |
| **`input_unavailable`** | M3: 系统输入注入失败（驱动错误/权限受限等） |
| **`key_event_invalid`** | M3.5: vk 越界或字符串不可注入 |

---

## 历史 — v0.1 握手详细规范

握手流程、`hello` / `welcome` / `error` 消息字段定义、HMAC 验证方式见 git 历史的 v0.1 PROTOCOL.md（commit before M2）。M2 完全保持兼容。
