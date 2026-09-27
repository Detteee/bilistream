# WebSub over a Cloudflare tunnel

WebSub needs its own hostname in the tunnel you already run, pointed at the
callback port on localhost. The WebUI and public-status routes stay as they are.

## 1. Add an ingress rule to cloudflared

If the tunnel uses a local config file (`~/.cloudflared/config.yml` or
`/etc/cloudflared/config.yml`), add a rule **above** the final catch-all:

```yaml
tunnel: <your-tunnel-id>
credentials-file: /root/.cloudflared/<id>.json
ingress:
  # existing rules, unchanged
  - hostname: admin.example.com
    service: http://localhost:3150
  - hostname: status.example.com
    service: http://localhost:<public-status-port>
  # new: WebSub callback only
  - hostname: yt.example.com
    path: ^/websub/youtube$
    service: http://localhost:3151
  - service: http_status:404
```

Then create the DNS record and restart the tunnel:

```bash
cloudflared tunnel route dns <tunnel-name> yt.example.com
sudo systemctl restart cloudflared
```

If the tunnel is managed in the Cloudflare dashboard (Zero Trust → Networks →
Tunnels → your tunnel → Public Hostname → Add):

- Subdomain: `yt`
- Path: `websub/youtube`
- Service: HTTP → `localhost:3151`

The dashboard creates the DNS record for you.

The `path` rule means cloudflared only forwards `/websub/youtube`, on top of
the listener answering nothing else.

## 2. Use its own hostname, not the WebUI's or the status page's

- **WebUI hostname:** if it's behind Cloudflare Access, Access would block
  Google's hub the same way it blocks anyone who isn't logged in.
- **Status page hostname:** you could add a path rule for `/websub/youtube` on
  it, placed above that hostname's existing rule. Since cloudflared uses the
  first match, the order matters. A separate hostname is simpler and keeps the
  WebSub traffic separate.

## 3. Don't let Cloudflare challenge the hub

Google's hub is a bot and can't solve challenges. For `yt.example.com`, make
sure none of these apply:

- Bot Fight Mode or Under Attack mode
- WAF rules that challenge or block POSTs
- Access policies

If any of these are on for the whole zone, add a WAF custom rule: Hostname
equals `yt.example.com` → Skip (all remaining custom rules, plus Super Bot
Fight Mode if you're on a plan that has it).

## 4. Configure bilistream

In Settings → API 密钥:

- **YouTube Data API Key:** must be set, because WebSub is off without a key.
- **WebSub 回调地址:** `https://yt.example.com/websub/youtube`
- **WebSub 回调端口:** `3151`, or any free port that isn't the WebUI's. If you
  change it, change the port in the cloudflared rule too.

Save. It takes effect within 30s, with no restart needed. The callback binds
the same address as the WebUI, so a default `127.0.0.1` bind is fine:
cloudflared connects over localhost.

## 5. Verify

```bash
curl -i https://yt.example.com/websub/youtube
```

| Result | Meaning |
|---|---|
| 404 | Good: the request reached bilistream, which rejects any request it didn't ask for. |
| 502 / 1033 | cloudflared can't reach port 3151. Check the port and that WebSub is on. |
| 403 or a challenge page | A Cloudflare security feature is blocking it; go back to step 3. |

Then check the log and the settings page:

- The log shows `WebSub 回调监听: 127.0.0.1:3151/websub/youtube`, then
  `WebSub 订阅: 已验证 N / 等待 0 / 失败 0` within a minute or two.
- In settings, the WebSub tile turns yellow (`N 已验证`) and goes green once
  the first push arrives.
- If subscriptions stay 等待 and then turn 失败 after about 10 min, the hub
  can't reach the callback. It's almost always step 3.

Before a push has arrived, the WebSub tile stays yellow, and the roster
playlist poll isn't slowed until the first real push lands. That's expected,
since channels only push when they post or edit a video.
