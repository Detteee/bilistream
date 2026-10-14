//! Completion of a credential save already committed by the gateway holder.
//! Local preferences stay local; only their unchanged base may be replaced.

use super::{check_update, merge, reply, settings_view, store_error, UpdateError, UpdateNotify};
use crate::notify::{credentials, openid_sync::PeerLink, store, NotifyConfig, Refusal};
use crate::storage::{Store, Transaction};
use axum::{http::StatusCode, response::Response};
use serde_json::{json, Value};
use std::{io, sync::Arc, time::Duration};

/// A slow holder must not make cache repair keep a browser save open.
const COPY_TIMEOUT: Duration = Duration::from_secs(2);
const LOCAL_ATTEMPTS: usize = 2;

#[derive(Clone)]
pub(crate) struct LocalSettings {
    base: Value,
    desired: Value,
}

fn preferences(mut config: Value) -> Value {
    if let Some(qq) = config.get_mut("qq").and_then(Value::as_object_mut) {
        qq.remove("app_id");
    }
    config
}

fn config_value(config: &NotifyConfig) -> io::Result<Value> {
    serde_json::to_value(config).map_err(|_| io::Error::other("通知设置无法读取"))
}

/// Validate everything before forwarding. A peer holder is the only openid
/// writer, so a local openid edit cannot be left behind as a cache retry.
pub(crate) fn prepare(
    store: &Store,
    update: &UpdateNotify,
    local: &str,
    holder: &str,
) -> Result<Option<LocalSettings>, UpdateError> {
    let (base, _) = store::load_config(store).map_err(store_error)?;
    check_update(store, update, local, &|| {
        Err(Refusal::not_owner(holder, "修改管理员 openid"))
    })?;
    let Some(patch) = update.config.as_ref() else {
        return Ok(None);
    };
    let base = config_value(&base).map_err(store_error)?;
    let mut desired = base.clone();
    merge(&mut desired, patch);
    let desired: NotifyConfig = serde_json::from_value(desired)
        .map_err(|_| store_error(io::Error::other("通知设置无法读取")))?;
    let desired = config_value(&desired).map_err(store_error)?;
    let base = preferences(base);
    if preferences(desired.clone()) == base {
        return Ok(None);
    }
    Ok(Some(LocalSettings { base, desired }))
}

impl LocalSettings {
    fn apply(&self, tx: &mut Transaction<'_>) -> io::Result<()> {
        let current = tx.read(store::CONFIG_RECORD)?;
        let revision = current.as_ref().map(|doc| doc.revision).unwrap_or(0);
        let config: NotifyConfig = match current {
            Some(doc) => serde_json::from_value(doc.value)
                .map_err(|_| io::Error::other("通知设置无法读取"))?,
            None => NotifyConfig::default(),
        };
        let current = config_value(&config)?;
        let current_preferences = preferences(current.clone());
        if current_preferences == preferences(self.desired.clone()) {
            return Ok(());
        }
        if current_preferences != self.base {
            // An actual local preference edit wins. A credential push only
            // changes AppID and is safe to rebase onto, even if it bumped the
            // config record revision while the holder handled the request.
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                store::RevisionConflict {
                    record: "config",
                    current: revision,
                },
            ));
        }
        let mut desired = self.desired.clone();
        desired["qq"]["app_id"] = current["qq"]["app_id"].clone();
        tx.write(store::CONFIG_RECORD, desired)?;
        Ok(())
    }
}

pub(crate) struct Committed {
    pub view: Value,
    pub cache_ready: bool,
    pub local_settings_pending: bool,
}

impl Committed {
    pub(crate) fn into_response(self, cleared: bool) -> Response {
        let message = if self.local_settings_pending {
            "机器人凭据已保存，本节点设置暂未保存，请重试。"
        } else if !self.cache_ready {
            "机器人凭据已保存，正在同步到本节点。"
        } else if cleared {
            "AppSecret 已清除"
        } else {
            "通知设置已保存"
        };
        reply(StatusCode::OK, Some(self.view), message)
    }
}

/// Rule 18: completion cannot turn a committed holder save into a failure.
/// The copy and local preferences commit together, with a bounded local
/// retry. The caller schedules the normal pull if this repair cannot finish.
/// `after` runs after all writes, allowing a real rollback regression.
pub(crate) async fn complete<F>(
    store: Arc<Store>,
    link: &dyn PeerLink,
    holder: &str,
    outcome: credentials::SaveOutcome,
    settings: Option<LocalSettings>,
    after: F,
) -> Committed
where
    F: Fn(&mut Transaction<'_>) -> io::Result<()> + Send + Sync + 'static,
{
    let after = Arc::new(after);
    let mut cache_ready = false;
    if let Ok((revision, members)) = link.members() {
        if members.iter().any(|node| node == holder) {
            if let Ok(Ok(copy)) =
                tokio::time::timeout(COPY_TIMEOUT, link.pull(revision, holder)).await
            {
                let current_member = link.members().is_ok_and(|(current, members)| {
                    current == revision && members.iter().any(|node| node == holder)
                });
                let committed_copy = copy
                    .credentials
                    .as_ref()
                    .is_some_and(|set| set.generation >= outcome.generation);
                if current_member && committed_copy {
                    for _ in 0..LOCAL_ATTEMPTS {
                        let store = Arc::clone(&store);
                        let copy = copy.clone();
                        let settings = settings.clone();
                        let after = Arc::clone(&after);
                        let result = tokio::task::spawn_blocking(move || {
                            store::accept_copy_as_with(
                                &store,
                                &copy,
                                None,
                                crate::storage::now(),
                                move |tx| {
                                    if credentials::load_meta_tx(tx)?.generation
                                        < outcome.generation
                                    {
                                        return Err(io::Error::other("通知凭据副本尚未更新"));
                                    }
                                    if let Some(settings) = settings {
                                        settings.apply(tx)?;
                                    }
                                    after(tx)
                                },
                            )
                        })
                        .await;
                        match result {
                            Ok(Ok(_)) => {
                                cache_ready = true;
                                break;
                            }
                            // Replaying over a newer operator edit would
                            // lose it. Keep the draft pending in the panel.
                            Ok(Err(error)) if error.kind() == io::ErrorKind::WouldBlock => break,
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    let local_settings_pending = settings.is_some() && !cache_ready;
    let mut view = settings_view(&store).unwrap_or_else(|_| json!({}));
    // The local cache may still be old, but the save acknowledgement always
    // names the generation the holder actually committed.
    view["credentials_generation"] = json!(outcome.generation);
    view["local_settings_pending"] = json!(local_settings_pending);
    Committed {
        view,
        cache_ready,
        local_settings_pending,
    }
}
