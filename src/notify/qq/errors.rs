//! QQ OpenAPI error classification (design §7, research §2–§3).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    /// Drop the token, fetch once, retry once.
    NeedsReauth,
    /// Retry with backoff.
    Transient,
    /// Wait at least 60 s and spend the shared budget for now.
    RateLimited,
    /// This delivery mode is closed; try the next one.
    ModeUnavailable,
    /// Warn once, count as undelivered, never reroute.
    Undeliverable,
    /// Re-render once as minimal text.
    ContentRejected,
    /// Downgrade markdown to plain text and remember it.
    Markdown,
}

/// Classify by business code first (QQ often answers HTTP 200/4xx with a code
/// in the body), then by HTTP status.
pub fn classify(http_status: u16, code: Option<i64>) -> ErrorClass {
    if let Some(code) = code {
        if let Some(class) = classify_code(code) {
            return class;
        }
    }
    match http_status {
        401 => ErrorClass::NeedsReauth,
        429 => ErrorClass::RateLimited,
        500..=599 | 201 | 202 => ErrorClass::Transient,
        400 | 403 | 404 => ErrorClass::Undeliverable,
        _ => ErrorClass::Transient,
    }
}

pub fn classify_code(code: i64) -> Option<ErrorClass> {
    use ErrorClass::*;
    Some(match code {
        11241 | 11243 | 11251 => NeedsReauth,
        // 11242 is listed both as a token problem and as transient; a refetch
        // costs nothing, so reauth first (it retries once anyway).
        11242 => NeedsReauth,
        50055002 | 100001 | 11252 | 11263 => Transient,
        40034100 => RateLimited,
        40034105 | 40034122 | 40034123 | 40034128 | 40034005 | 304103 => ModeUnavailable,
        40054004 | 40054013 | 40054016 | 11253 | 11254 | 11265 | 100007 | 100016 | 10004 => {
            Undeliverable
        }
        40034006 | 40054007 | 40054018 | 304003 | 22006 => ContentRejected,
        40034127 | 50056 => Markdown,
        _ => return None,
    })
}

/// Short Chinese reason for logs, status and the "未送达" summary.
pub fn describe(code: Option<i64>, http_status: u16) -> String {
    let text = match code {
        Some(100016) => "AppID 或 AppSecret 无效",
        Some(100007) => "AppID 无效或机器人已封禁",
        Some(10004) => "机器人不存在",
        Some(100001) => "获取令牌过于频繁",
        Some(40034100) => "主动消息频率超限",
        Some(40034105) => "主动消息无权限",
        Some(40034122) | Some(40034123) => "召回额度已用完",
        Some(40034128) | Some(40034005) | Some(304103) => "被动回复窗口已过期",
        Some(40054004) => "管理员尚未添加机器人",
        Some(40054013) => "管理员已关闭主动消息",
        Some(40054016) => "机器人离线",
        Some(11253) | Some(11254) => "接口无权限",
        Some(11265) => "机器人已封禁",
        Some(304003) => "消息含未报备链接",
        Some(40034006) => "消息内容违规",
        Some(40034127) | Some(50056) => "无 Markdown 权限",
        Some(11241) | Some(11242) | Some(11243) => "令牌无效",
        _ => match http_status {
            0 => "网络错误",
            401 => "鉴权失败",
            429 => "请求过多",
            _ => "发送失败",
        },
    };
    match code {
        Some(code) => format!("{text} ({code})"),
        None if http_status > 0 => format!("{text} (HTTP {http_status})"),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_design_code_has_its_class() {
        use ErrorClass::*;
        let table: &[(&[i64], ErrorClass)] = &[
            (&[11241, 11242, 11243, 11251], NeedsReauth),
            (&[50055002, 100001, 11252, 11263], Transient),
            (&[40034100], RateLimited),
            (
                &[40034105, 40034122, 40034123, 40034128, 40034005, 304103],
                ModeUnavailable,
            ),
            (
                &[
                    40054004, 40054013, 40054016, 11253, 11254, 11265, 100007, 100016, 10004,
                ],
                Undeliverable,
            ),
            (
                &[40034006, 40054007, 40054018, 304003, 22006],
                ContentRejected,
            ),
            (&[40034127, 50056], Markdown),
        ];
        for (codes, class) in table {
            for code in *codes {
                assert_eq!(classify(200, Some(*code)), *class, "code {code}");
                assert!(!describe(Some(*code), 200).is_empty());
            }
        }
    }

    #[test]
    fn http_status_is_the_fallback() {
        assert_eq!(classify(401, None), ErrorClass::NeedsReauth);
        assert_eq!(classify(429, None), ErrorClass::RateLimited);
        assert_eq!(classify(500, None), ErrorClass::Transient);
        assert_eq!(classify(504, None), ErrorClass::Transient);
        assert_eq!(classify(202, None), ErrorClass::Transient);
        assert_eq!(classify(0, None), ErrorClass::Transient);
        // An unknown body code falls back to the status.
        assert_eq!(classify(429, Some(1)), ErrorClass::RateLimited);
        assert_eq!(describe(None, 0), "网络错误");
        assert_eq!(
            describe(Some(100016), 200),
            "AppID 或 AppSecret 无效 (100016)"
        );
    }
}
