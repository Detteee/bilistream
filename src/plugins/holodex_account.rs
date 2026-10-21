//! Config-backed Holodex account and JWT synchronization.

pub(crate) async fn ensure_holodex_username(cfg: &mut crate::config::Config, jwt: &str) {
    if cfg
        .holodex_username
        .as_ref()
        .is_some_and(|name| !name.is_empty())
    {
        return;
    }

    let Some(api_key) = cfg.holodex_api_key.as_deref().filter(|key| !key.is_empty()) else {
        return;
    };

    let Ok(Some(refresh)) = crate::plugins::holodex::refresh_holodex_jwt(api_key, jwt).await else {
        return;
    };

    let mut should_save = false;
    if let Some(name) = refresh.username.filter(|name| !name.is_empty()) {
        cfg.holodex_username = Some(name);
        should_save = true;
    }
    if let Some(new_jwt) = refresh.jwt.filter(|token| !token.is_empty()) {
        cfg.holodex_jwt = Some(new_jwt);
        cfg.holodex_jwt_refreshed_at = Some(crate::plugins::holodex::holodex_unix_now());
        should_save = true;
    }
    if should_save {
        let _ = crate::config::save_config(cfg).await;
    }
}

pub(crate) async fn apply_holodex_jwt_sync(
    cfg: &mut crate::config::Config,
) -> Result<(String, bool), String> {
    let jwt = cfg
        .holodex_jwt
        .as_ref()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "Holodex JWT not configured".to_string())?
        .clone();

    ensure_holodex_username(cfg, &jwt).await;

    if cfg.holodex_skip_jwt_verify {
        return Ok((jwt, false));
    }

    let api_key = cfg
        .holodex_api_key
        .as_ref()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| "尚未配置 Holodex API Key".to_string())?
        .clone();

    let sync = crate::plugins::holodex::sync_holodex_jwt_if_needed(
        &api_key,
        &jwt,
        cfg.holodex_jwt_refreshed_at,
        cfg.holodex_username.clone(),
    )
    .await?;

    let mut should_save = false;
    if sync.jwt != jwt {
        cfg.holodex_jwt = Some(sync.jwt.clone());
        should_save = true;
    }
    if sync.refreshed_at != cfg.holodex_jwt_refreshed_at {
        cfg.holodex_jwt_refreshed_at = sync.refreshed_at;
        should_save = true;
    }
    if sync.username.is_some() && sync.username != cfg.holodex_username {
        cfg.holodex_username = sync.username.clone();
        should_save = true;
    }
    if should_save {
        if let Err(e) = crate::config::save_config(cfg).await {
            tracing::warn!(
                "Holodex 登录状态保存失败：{}",
                crate::logging::safe_diagnostic(&e.to_string())
            );
        }
    }

    Ok((sync.jwt, sync.token_rotated))
}
