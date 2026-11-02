# Boubou

一条命令，在本机与语音模型聊天。

```sh
boubou run liquid
```

Boubou 自动下载匹配的模型与推理后端，使用麦克风接收语音、流式播放回答。当前是面向 **Apple Silicon Mac 的开发原型**，模型为 Liquid AI LFM3-Audio-2B。**自动免提对话尚未通过真实验收，当前不能视为可用产品。**

提供与官方示例录音/提交顺序对应的手动对照入口：

```sh
boubou run liquid --offline --manual
```

按 Enter 开始录音，再按 Enter 提交。回答期间按 Enter 会停止回答并开始新录音；闲置和播放期间不会自动提交麦克风内容。`Esc` 暂停，`Enter` 恢复，`Ctrl+C` 退出。这个入口用于先确认完整问答，不能代表自动语音打断已经完成。

## 从源码安装

开发者需要 Rust 1.88+ 和 Xcode Command Line Tools。最终可执行文件运行时不需要 Python、Rust、编译器或手动安装 llama.cpp。

在仓库目录执行：

```sh
cargo install --path . --locked
boubou run liquid
```

首次使用会下载约 **1.42 GB** 的固定版本权重与音频组件，以及约 16 MB 的推理后端。macOS 第一次使用麦克风时会请求授权。允许 Boubou 或启动它的终端访问麦克风。

目前提供源码构建；签名、公证过的公开安装包尚未发布。其他操作系统和芯片尚未支持。

## 常用命令

```sh
# 提前下载；run 也会自动完成这一步
boubou pull liquid

# 已缓存的模型完全在本机运行，不检查更新或下载
boubou run liquid --offline

# 无网络地核验本地模型及后端
boubou doctor

# 自定义缓存目录；也可设置 BOUBOU_HOME
boubou --cache-dir /path/to/cache run liquid
```

默认缓存位于 macOS 的 `~/Library/Caches/boubou`。下载中断后重新执行命令即可续传；文件必须通过固定大小和 SHA-256 检查才能使用。多个进程不能同时修改安装缓存。当前固定的模型配置不会自动更新或切换到云服务。

## 对话方式

- 直接说话，停顿约 600 ms 后提交一轮。
- 回答期间再次说话，会停止当前播放并取消旧回答，随后处理你的新话语。
- `Esc` 停止播放、取消生成、清空本地排队输入并暂停监听。暂停期间不会提交麦克风内容，按 `Enter` 恢复；`Ctrl+C` 退出并释放音频设备、关闭本次启动的推理进程。
- 一轮输入最长 30 秒，待播语音队列最多 30 秒。队列积压、设备断开或后端无响应时明确报错。
- 优先使用耳机进行首次体验。免提回声消除已接入系统 VoiceProcessingIO，真实设备效果仍需人工验收。

**对话语言先用英语。** 官方模型卡把语音聊天限定为英语；模型的多语言 ASR/TTS 能力不代表中文连续对话已经验证。

**打断历史的限制：** 固定版本的 llama.cpp 支持取消生成，但不支持按实际播放时间截断模型历史。Boubou 会清空当前与待播音频、屏蔽旧响应的迟到输出，并在下一轮告诉模型上一条回答被打断、用户可能没有听到剩余内容。模型内部仍保留已生成的内容，因此当前版本不提供精确的已播放历史回滚，也不宣称模型原生全双工。

对话保留在本次推理会话内，退出后不恢复。音频默认不保存到磁盘，模型推理仅连接本次启动的 `127.0.0.1` 后端。后端诊断日志位于缓存的 `logs` 目录。

## 不使用麦克风的文件模式

输入要求为 PCM16、单声道、16 kHz WAV，每个文件不超过 30 秒。重复 `--input` 会在同一会话中连续发送多轮。文件模式输出文字，并可将语音合并保存为 PCM16、单声道、24 kHz WAV；不会打开麦克风或扬声器。

```sh
boubou run liquid --offline \
  --input question.wav --input follow-up.wav --output replies.wav
```

如果需要转换已有录音，可以使用：

```sh
ffmpeg -i recording.wav -ar 16000 -ac 1 -c:a pcm_s16le question.wav
```

FFmpeg 仅是转换文件的可选工具，Boubou 本身不依赖它。

## 当前实现

| 部分 | 实现 |
| --- | --- |
| CLI、下载、会话与进程管理 | Rust |
| 麦克风、扬声器与回声消除 | macOS AVAudioEngine / VoiceProcessingIO，原生 helper 内嵌在可执行文件中 |
| 语音活动检测 | macOS HAL 回声感知近端语音检测，20 ms 帧，含起始预留音频 |
| 推理 | llama.cpp `b11960`，通过 `/v1/realtime` 流式收发音频 |
| 模型 | `LiquidAI/LFM3-Audio-2B-GGUF`，revision `02355c1a141e464a064f2ab2c276638dbcb7f38a` |
| 权重 | `Q4_K_M` 主模型 + `Q8_0` 音频 projector |

macOS 音频 helper 当前使用本地 ad-hoc 签名。重新构建改变 helper 后，系统可能重新请求麦克风授权。

验证命令：

```sh
cargo test --locked
cargo clippy --locked -- -D warnings
cargo build --release --locked
```

模型缓存就绪后，可另行运行真实后端的取消与下一轮接续验证（不会打开音频设备）：

```sh
cargo test --locked liquid_backend_cancels_and_accepts_the_next_turn -- --ignored --nocapture
```

普通测试覆盖下载恢复与校验、离线缓存、实时协议取消与旧响应过滤、音频播放代次以及语音检测输入边界，不会打开音频设备。捕获帧携带实际采集时间和系统近端语音标记；音量大或普通语音分类阳性不会直接触发打断。麦克风需要支持 macOS HAL 回声感知语音检测，不支持时明确报错。

真实扬声器回放回归测试需显式运行，并在测试期间保持安静。它播放指定的模型回答，检查播放期间及尾音是否产生假用户回合，不保存麦克风音频：

```sh
BOUBOU_SPEAKER_TEST_WAV=/path/to/model-reply-24khz.wav \
  cargo test --locked speaker_echo_probe -- --ignored --nocapture
```

真人插话、不同音频设备、端到端延迟及长时间稳定性需要在目标设备上另外验收。

只用扬声器输出静音、验证打断不会让过期播放通知挤满收音队列，并检查下一段播放接续（不打开麦克风）：

```sh
cargo test --locked cancelled_playback_does_not_overflow_capture_events -- --ignored --nocapture
```

## Conversation diagnostics

For a coordinated investigation, enable control metadata in the terminal:

```sh
BOUBOU_TRACE=1 boubou run liquid --offline
```

Trace lines contain relative times, input turn numbers, response IDs, playback epochs,
sample counts, pause/resume events, and cancellation/filtering decisions. They do not
contain PCM, transcripts, or generated text. Tracing is off by default and does not
save audio or create a trace file. Normal conversation text remains visible as usual.
`playback.clear.applied` acknowledges the native player's stop/clear operation; it
does not measure the remaining hardware or acoustic tail.

`Esc` pauses input submission until `Enter`. Microphone capture remains open and
captured frames are discarded while paused. Spoken “stop” or “goodbye” is currently
ordinary input to Liquid, not an explicit pause control. Passing an Esc test does
not establish that spoken stopping, speaker echo, or human barge-in is working.

## 上游与许可证

模型使用自定义 **LFM Open License v1.0**，包含商业使用及再分发条件。模型许可证随权重下载至缓存目录。llama.cpp 使用 MIT 许可证，其完整上游归档与许可证随推理后端保留。

- [Liquid 官方模型卡与许可证](https://huggingface.co/LiquidAI/LFM3-Audio-2B)
- [固定模型制品](https://huggingface.co/LiquidAI/LFM3-Audio-2B-GGUF/tree/02355c1a141e464a064f2ab2c276638dbcb7f38a)
- [llama.cpp 固定版本](https://github.com/ggml-org/llama.cpp/releases/tag/b11960)
- [Apple 语音处理与 HAL 语音检测说明](https://developer.apple.com/videos/play/wwdc2023/10235/)

后续模型与 Hugging Face / LiveKit 级联方案将在这个 Liquid 样板完成验证后接入。
