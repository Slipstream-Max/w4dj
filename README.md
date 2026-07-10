# W4DJ 网易云曲库增量同步工具

W4DJ 用于把一个或多个下载目录、NCM 文件或普通音频增量同步到个人曲库。它可以解密 NCM、保留歌曲元数据和封面，并通过 FFmpeg 将输出统一转换为 MP3 或 WAV。

## 功能

- 同时接收多个文件和目录
- 支持包含空格和中文的路径
- 支持把多个文件或目录直接拖到程序上运行
- 无参数启动时提供 GPUI 桌面界面
- 使用稳定歌曲 ID 和 manifest 做增量同步
- 使用 Rayon 并行 dump 和转码
- 保留标题、歌手、专辑、曲号、流派和封面等元数据
- 识别用户移动到输出子目录中的文件
- 目标文件被删除后，在源文件再次出现时自动恢复
- 同 ID 的更高音质源自动升级已有输出
- 不同 ID 的同名歌曲使用 ID 后缀，绝不随机覆盖
- 所有结果先写临时文件，验证成功后再原子替换

当前支持 NCM、MP3、FLAC 和 WAV 输入。

## 图形界面

直接双击 `w4dj.exe`，或在终端中不带参数运行 `w4dj`，会打开 GPUI 桌面界面。界面自动加载默认 `.config.toml`：

- `Add folder` 添加的目录会保存到配置文件
- 拖入列表的文件和目录只在当前窗口有效，不写入配置
- 点击输出目录可修改并保存同步目标
- MP3、WAV、AS IS 模式切换会保存到配置
- `Sync` 在后台执行扫描、增量比较和并行转换，界面显示逐文件进度

带 `--input`、`--config` 等参数运行时仍使用命令行模式。

## 命令行用法

通过 `--input` 传入一个或多个文件、目录：

```powershell
w4dj --input "D:\Cloud Music" "E:\Downloads\song.ncm"
w4dj --input "D:\Cloud Music" --output "D:\Music" --mode mp3
w4dj --input "D:\song.flac" --mode wav
```

也可以使用位置参数。这也是 Windows 把文件拖到 exe 上时采用的形式：

```powershell
w4dj "D:\Cloud Music" "E:\Downloads\song.ncm"
```

未指定 `--output` 时，程序会在 `w4dj.exe` 同级目录自动创建 `w4djdump`。

### 输出模式

| 模式 | 行为 | FFmpeg |
| --- | --- | --- |
| `original` | NCM 解密为内部真实 MP3/FLAC；普通音频保持原格式 | 不需要 |
| `mp3` | 所有源统一使用 `libmp3lame -q:a 2` 编码 | 需要 |
| `wav` | 所有源统一输出为 16 位 PCM WAV | 需要 |

WAV 可以写入包含封面的 ID3 数据块，但具体播放器是否显示 WAV 封面取决于播放器兼容性。

## 配置文件

未指定 `--config` 时，W4DJ 会先读取 exe 同级的 `.config.toml`，没有找到时再读取当前目录的 `.config.toml`。因此可以把 exe 和配置文件放在同一目录，编辑配置后直接双击运行。

```toml
inputs = [
    'D:\CloudMusic',
    'E:\Downloads\song.ncm',
]

# 可省略；默认是 exe 同级的 w4djdump。
output = 'D:\Music'

mode = "original" # original | mp3 | wav
```

### 跨平台路径

配置文件使用 TOML 字符串。Windows 路径推荐使用单引号，反斜杠会保持原样，无需手动替换为 `/`：

```toml
inputs = [
    'C:\Users\name\Cloud Music',       # 普通路径
    '\\server\share\Cloud Music',     # UNC 网络路径
    '\\?\C:\very long path\Music',   # Windows 扩展长度路径
    "D:\\escaped\\Cloud Music",      # 双引号写法：反斜杠需要转义
]
output = 'D:\Music\w4djdump'
```

Linux 和 macOS 路径直接使用各自的绝对路径：

```toml
# Linux
inputs = ["/home/name/Music", "/mnt/media/Cloud Music"]

# macOS
output = "/Users/name/Music/w4djdump"
```

相对路径也受支持，并以配置文件所在目录为基准。空格、中文以及 TOML 双引号字符串中的 `\\`、`\uXXXX` 等标准转义均会正确解析。不要把未转义的 Windows 路径写在双引号中，例如 `"C:\Users\name"`；应改用单引号，或写成 `"C:\\Users\\name"`。

也可以指定其他配置文件：

```powershell
w4dj --config "D:\Configs\music.toml"
```

命令行出现输入路径时，会整体替换配置文件的 `inputs`。其他命令行选项分别覆盖对应配置项。

## 增量同步

W4DJ 在输出目录保存 `.w4dj-state.json`，每首歌只记录：

- 稳定歌曲 ID
- 当前输出相对路径
- 输出 profile 版本
- 源格式、码率和文件大小

NCM 优先使用网易云 `musicId`。普通音频依次使用已写入的 `W4DJ_ID`、MusicBrainz ID、ISRC，最后使用规范化后的标题、歌手、专辑和时长生成 ID。

同步规则如下：

- 记录路径存在并且 `W4DJ_ID` 一致：跳过
- 文件被移动到其他输出子目录：更新 manifest 路径并跳过
- 目标文件被删除且本次仍有源文件：重新生成
- 源文件本次没有出现：保留 manifest 和现有输出，不执行删除
- 同 ID 出现更高格式或更高码率源：升级现有输出
- 同 ID 仅修改时间变化：不重复处理
- 不同 ID 映射到同一文件名：为后来的文件添加稳定 ID 后缀
- mode 或编码 profile 变化：重新生成当前输入涉及的输出

输入文件大小永远不会与转码后的输出文件大小比较。源大小只用于判断同一歌曲的源版本是否变化。

## FFmpeg

`mp3` 和 `wav` 模式需要 FFmpeg。程序按以下顺序查找：

1. `w4dj.exe` 同级的 `ffmpeg.exe`
2. 系统 `PATH` 中的 `ffmpeg`

使用 doctor 检查 FFmpeg 版本以及 W4DJ 需要的 MP3、WAV 编码器：

```powershell
w4dj doctor
```

普通 doctor 只检查，不修改系统。需要自动安装时显式执行：

```powershell
w4dj doctor --install
```

自动安装支持以下系统包管理器：

- Windows：winget、Scoop、Chocolatey
- macOS：Homebrew、MacPorts
- Linux：APT、DNF、pacman、Zypper、APK

程序会选择当前系统找到的第一个受支持包管理器，执行官方 FFmpeg 包安装，然后重新检查版本、`libmp3lame` 和 `pcm_s16le`。需要系统权限的包管理器可能会请求 sudo 或管理员权限。

Windows 可以安装：

```powershell
winget install --id Gyan.FFmpeg --exact
```

每个 FFmpeg 进程限制为一个线程，外层由 Rayon 控制并发数量，避免多个转码任务造成 CPU 过度竞争。

## 构建

安装当前 Rust 工具链后执行：

```powershell
cargo build --release
```

Windows 可执行文件位于 `target/release/w4dj.exe`。

## 致谢

- [anonymous5l/ncmdump](https://github.com/anonymous5l/ncmdump)
- [iqiziqi/ncmdump.rs](https://github.com/iqiziqi/ncmdump.rs)

## 免责声明

本工具仅用于个人学习和合法管理有权使用的音乐文件。请遵守所在地法律法规及相关服务条款。
