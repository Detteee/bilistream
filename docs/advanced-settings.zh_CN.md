# 高级设置

[返回首页](../README.zh_CN.md) · [English](advanced-settings.md)

普通 YouTube/Twitch 转播按首页配置即可，以下功能按需启用。

## 控制面板

YouTube、Twitch 卡片支持按平台配置标题、分区、画质、裁剪与 HLS 缓存。转播控制包括 Bilibili 开关播、重启转播、码率与缓存仪表、标题/分区/封面更新和防撞车。配置 Holodex API Key 后可浏览直播与预告、切换转播目标、查看推荐分区；可选登录以使用收藏夹。

优先监控和多服务器默认关闭；显示设置不改变监控状态。已有配置键保持兼容。

## 命令行选项

以控制台或托盘模式启动，也可设置单个选项：

```bash
./bilistream --webui
./bilistream --tray
./bilistream --port 3150
./bilistream --bind 127.0.0.1
./bilistream --ffmpeg-log-level error
```

Windows 默认使用系统托盘，Linux/macOS 默认使用控制台 Web UI。以上命令使用 `./bilistream`，Windows 请改用 `bilistream.exe`。

| 选项 | 环境变量 | 用途 |
| --- | --- | --- |
| `--bind ADDR` | `BILISTREAM_BIND` | 管理端监听地址，默认 `127.0.0.1`。 |
| `-p`、`--port PORT` | `BILISTREAM_PORT` | 管理端口，默认 `3150`。 |
| `--password PASSWORD`、`--password-file PATH` | `BILISTREAM_PASSWORD` | 仅首次导入面板密码；已有或明确清除的选择优先。见[远程访问与密码](remote-access.zh_CN.md)。 |
| `--cluster-token-file PATH` | `BILISTREAM_CLUSTER_TOKEN_FILE` | 节点共享密钥，见[多服务器](#多服务器)。 |
| `--ffmpeg-log-level LEVEL` | `BILISTREAM_FFMPEG_LOG_LEVEL` | `error`、`info` 或 `debug`，默认 `error`。 |
| `--reset-panel-password` | — | 停止服务后离线清除面板密码并退出，见[忘记密码](remote-access.zh_CN.md#忘记密码)。 |
| `--export-legacy DIR` | — | 导出供旧版使用的明文数据后退出，见[回退旧版](data-and-upgrades.zh_CN.md#回退旧版)。 |
| `-h`、`--help` / `-V`、`--version` | — | 显示帮助或版本后退出。 |
| — | `BILISTREAM_DATA_DIR`、`BILISTREAM_KEY_FILE` | 数据目录与受限密钥位置，见[数据与备份](data-and-upgrades.zh_CN.md)。 |

## 优先频道

**优先频道**：可选的 YouTube/Twitch 优先频道卡片。基础设置中的 **显示优先频道** 只控制卡片显示；卡片内监控开关启用优先选择，**自动重启流** 允许在转播其他频道时，待优先频道确认可播放后自动切换。隐藏卡片不会停止监控。

1. 系统设置 → 基础设置，勾选「显示优先频道」并保存。
2. 在仪表盘卡片中选择频道与默认分区，打开监控开关。
3. 需要打断当前转播并切换时，再打开「自动重启流」。

显示和监控是两个独立设置。只隐藏卡片会保留已启用的监控；停止优先调度应关闭卡片里的监控开关。

## YouTube 发现设置

| 设置或来源 | 作用 |
| --- | --- |
| YouTube Data API Key | 确认直播/预告状态，并提供上传列表轮询额度；不同 Google 项目的多个 key 可提供独立额度。 |
| 基础设置 → **YouTube RSS** | 读取频道订阅源，默认开启。关闭后仍处理 WebSub，并按可用配额轮询上传列表。 |
| WebSub 回调地址与端口 | 接收推送线索，单机可用；需要 YouTube key 和可达的公网回调。地址留空关闭。 |
| Holodex API Key | 补充元数据、分类、预告和外部直播；可选登录用于收藏夹。 |
| 基础设置 → **yt-dlp 兜底** | 每轮直接探测 YouTube。关闭时，索引模式仍保留定期 yt-dlp 安全探测。 |

RSS 和 WebSub 用于发现视频 ID，本身不证明直播可播放。Data API 确认状态，实际转播还需要可用流地址。RSS 关闭或故障时，由上传列表在可用配额内兜底；WebSub 失效时恢复正常轮询速度。详见 [发现与回退](youtube-discovery.md) 和 [WebSub 隧道配置](websub-tunnel.md)。

多服务器使用共享索引时，由 **YouTube 索引节点** 负责发现，请在该节点设置 key、RSS 和回调。它与 **转播活跃节点** 可以是不同机器，不要把这两个角色混为一谈。

## 公开状态页

单台电脑或服务器即可使用。在**系统设置 → 公开状态页**选择**本机**并保存，多服务器保持关闭。默认访问 `http://127.0.0.1:23234`。[单机配置与分享方式](public-status.md)。

## 多服务器

**可选多服务器**：活跃/备用节点管理、健康检查、配置同步、自动转移。公开状态页由指定节点通过独立端口提供。

- 默认关闭。启用时填写每个节点唯一的 ID、可达的管理 API 地址和同一组成员；节点共用一份通信密钥，Web UI 密码可以各自设置。
- 转播活跃节点负责推流，备用节点等待交接；自动转移依赖心跳、多数节点确认和来源停止检查，无法确认安全接管时保持限制。
- 指定公开状态页节点、独立端口和公网 URL。状态页不提供管理接口；请将其与管理端区分。
- 公开状态页节点有可用 YouTube key 时，也是 YouTube 索引节点；可以与转播活跃节点不同。
- key、RSS、WebSub 回调和卡片显示设置属于各节点本地配置。共享索引模式请在索引节点配置发现来源。

详见 [发现回退](youtube-discovery.md) 和 [WebSub 隧道](websub-tunnel.md)。

先在各节点本机的「系统设置 → 安全」设置面板密码；无桌面服务器可按[远程访问说明](remote-access.zh_CN.md)首次导入。

通信密钥**只生成一次**，通过 SSH 将同一文件复制到各节点：

```bash
umask 077
mkdir -p ~/.config/bilistream
openssl rand -hex 32 > ~/.config/bilistream/cluster-token
chmod 600 ~/.config/bilistream/cluster-token
./bilistream --bind 0.0.0.0 \
  --cluster-token-file ~/.config/bilistream/cluster-token
```

也可用 `BILISTREAM_CLUSTER_TOKEN_FILE` 指定文件。通信密钥不能与 Web UI 密码相同，以明文保存并由文件权限保护。节点 API 地址使用 HTTPS 或私有网络；缺少密钥或密钥不一致时，节点认证失败。

从共用 Web UI 密码的旧版升级时，先停止集群，为**所有节点**配置密钥并升级，再恢复运行。首次升级后需重新登录网页；后续正常重启保留登录状态。注销、30 天到期会使会话失效；更改或清除密码立即注销全部会话。备份不包含面板密码和网页登录会话。更换通信密钥时，各节点一起更换并重启。

## Niconico Live

配置 Niconico 频道及 streamlink，在系统设置中直接填写 `user_session` 的值。无需整份 Cookie 导出；旧 Netscape 文件路径仍可兼容。每日检测只验证登录是否可用，不会续期；网络异常与明确失效分开显示。[可用性检测说明](niconico-session.md)。

基础设置的「显示 Twitch」「显示 Niconico」「显示优先频道」只控制卡片显示，隐藏不停止监控。多服务器关闭时隐藏其面板。Niconico 使用管道输入；优先频道目前支持 YouTube/Twitch。

## 弹幕、分区规则与英雄联盟检查

弹幕切换使用频道管理中保存的频道名，例如：

```text
%转播%YT%示例频道1%英雄联盟
%转播%TW%示例频道1%无畏契约
%查询
```

分区关键词规则可能调整请求的分区。英雄联盟玩家名称检查需要 Riot key，`lol_monitor_interval` 的单位是分钟。玩家名称过滤关键词位于 Riot API Key 下方，仅在启用英雄联盟玩家 ID 监控时显示。

## 设置预览

![使用模拟数据的高级设置](images/settings.png)

截图来自真实界面，使用模拟数据。
