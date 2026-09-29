# Niconico user_session

在系统设置 → 平台设置中填写 `user_session` Cookie 的值，保存后可点击「检查已保存会话」。输入框不会回显已保存的值；留空保留。清除选项会同时清除直接配置和旧文件路径。

Enter only the `user_session` value; a full cookie export is unnecessary. If you previously configured a Netscape cookie file, it remains supported. A directly entered value takes precedence.

![Mock Niconico session settings](images/niconico-session.png)

## Validity checks / 有效性预警

Daily checking is on by default after you configure a session, even when monitoring is off or the card is hidden.

- **Accepted / 会话仍被接受:** your login is currently accepted; the next check is in 24 hours.
- **Invalid / 登录会话已失效:** log in to Niconico again and update the saved value.
- **Unavailable / 暂时无法验证:** a network or service problem prevented verification. This does not mean the session expired; the next attempt is in one hour.
- **Unconfigured / 未配置:** enter a session value to use login checks.

Changing the credential triggers a new check; restarting the application checks again. The manual button checks the saved credential immediately. Turning daily checks off does not affect streamlink authentication.

**Checks do not refresh or extend `user_session`.** They provide early warning for infrequent rebroadcasts; successful validation does not guarantee the next stream is accessible or that the session cannot expire later. Login elsewhere or logout can invalidate it independently.

In multi-server mode, configure the session separately on each computer that restreams Niconico. Keep the saved configuration private, as with other login credentials.
