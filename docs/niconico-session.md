# Niconico user_session

在系统设置 → 平台设置中填写 `user_session` Cookie 的值，保存后可点击「检查已保存会话」。输入框不会回显已保存的值；留空保留。清除选项会同时清除直接配置和旧文件路径。

Only `user_session` is passed to streamlink as `--niconico-user-session`. Other cookies in an export (`nicosid`, `_dd_s`, `user_session_secure`, etc.) are not needed for this authentication path. A directly configured value takes precedence over the legacy Netscape file, including Cookie-Editor HttpOnly records.

![Mock Niconico session settings](images/niconico-session.png)

## Validity checks / 有效性预警

Daily checking is on by default for configured sessions, even when the source monitor is idle or its card is hidden. It uses a read-only request to `https://nvapi.nicovideo.jp/v1/users/me`, sending only the session cookie and required frontend headers. Responses and account details are not stored. Redirects are not followed, and response cookies are not saved.

- **Accepted / 会话仍被接受:** the authenticated endpoint returns its successful JSON response. Recheck after 24 hours.
- **Invalid / 登录会话已失效:** explicit HTTP 401. Log a warning and ask the operator to log in again and update the value.
- **Unavailable / 暂时无法验证:** timeout, blocked request, rate limit, unexpected response or service error. This does not prove expiry; retry after one hour.
- **Unconfigured / 未配置:** no session value or legacy file; no remote check.

Changing the credential triggers a new check; restarting the application checks again. The manual button checks the saved credential immediately. Turning daily checks off does not affect streamlink authentication.

**Checks do not refresh or extend `user_session`.** They provide early warning for infrequent rebroadcasts; successful validation does not guarantee the next stream is accessible or that the session cannot expire later. Login elsewhere or logout can invalidate it independently.

The session is node-local. Config responses expose only `user_session_configured`, never the value; it is excluded from public/cluster payloads. Keep the local runtime configuration private, as with other credentials.
