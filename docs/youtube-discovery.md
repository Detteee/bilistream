# YouTube discovery and fallback / YouTube 发现与回退

## Single server / 单机

Keep multi-server mode off. Configure a YouTube Data API key for confirmed live/upcoming discovery. RSS is on by default; WebSub is optional and needs only a reachable callback URL, its listener port and the key. A public status page is not required.

基础设置的 **YouTube RSS** 只控制频道 RSS 请求。关闭后仍处理 WebSub，并在配额允许时用上传列表兜底。**显示优先频道** 只控制界面，不是监控开关；需要停止优先监控时使用卡片里的开关。

| Condition | Behavior |
| --- | --- |
| RSS enabled | Read feeds approximately every 3 minutes, respecting per-feed backoff and outages. |
| RSS off | No feed requests. Uploads polling remains eligible within budget; WebSub classification continues. UI says 已关闭. |
| RSS rate limited/down | Backoff or rotate outage probes. Uploads polling covers the gap. UI reports failure, separately from manual off. |
| WebSub healthy | Slow roster uploads polling by half; target cadence is retained. |
| WebSub missing, failed or silent | Normal budgeted uploads cadence; enabled RSS remains available. |
| No callback or no YouTube key | No active WebSub subscriptions. A missing key also disables local discovery classification; Holodex can still supply metadata. |
| No usable quota | API calls obey the exhausted/rejected key state and reserved budget. Direct source checks and configured Holodex remain separate paths. |
| yt-dlp 兜底 on | Direct YouTube checks each monitor cycle. Off uses the index when available, with a periodic safety probe. |

RSS and pushes carry video IDs, not authoritative live status. `videos.list` classifies them; yt-dlp/streamlink still verifies a playable source before an automatic priority switch. Uploads polling reserves the actual target deterministically when a priority channel is monitored too.

Each usable key has a 9,000-unit application budget per Pacific day. Protected work and target checks reserve quota before roster polling. Polling cadence also adapts to learned go-live hours and remaining quota. RSS has no API request quota cost, but confirming discovered IDs does.

## Shared index / 多服务器共享索引

When the cluster has a selected public-status node with usable keys, that node supplies the YouTube index. Other nodes fetch its answers instead of making duplicate discovery calls. Configure RSS, keys and WebSub on the index node; these settings are node-local.

Only the selected index node subscribes to WebSub. Startup checks both the current configuration and the settled role; a stale previous role cannot grant permission after reassignment. The subscriber verifies its callback listener is ready before contacting the hub and retries a failed/exited listener.

When the index is stale, unavailable or cannot spend keys, nodes fall back to local answers using their own configuration. Clustered fallback nodes remain unsubscribed to WebSub to avoid duplicate push confirmation costs; RSS/uploads/Holodex cover the gap. With the cluster disabled, WebSub works locally again. With no selected public-status node, each node answers locally and clustered WebSub remains off under this policy.

Turning WebSub off retires subscriptions and keeps the callback available for unsubscribe verification for up to 10 minutes. See [tunnel setup](websub-tunnel.md). Local automated tests cover these decisions; actual public callback reachability must be verified in the deployed network.
