use super::{
    crypto, invalid, paths, read_row, sql_error, Key, Transaction, MAX_DOCUMENT_BYTES, NAMES,
};
use base64::Engine;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Serialize, Deserialize)]
struct Source {
    path: PathBuf,
    bytes: String,
    digest: String,
    retire: bool,
}

pub(crate) fn validate(name: &str, value: &Value) -> io::Result<()> {
    if !NAMES.contains(&name) {
        return Err(invalid("备份包含未知数据类型"));
    }
    let bad = || invalid(&format!("{name} 格式无效，原文件已保留"));
    match name {
        "config.json" => {
            serde_json::from_value::<crate::config::Config>(value.clone()).map_err(|_| bad())?;
        }
        "channels.json" => {
            let channels = value
                .get("channels")
                .and_then(Value::as_array)
                .ok_or_else(bad)?;
            for channel in channels {
                if channel.get("name").and_then(Value::as_str).is_none()
                    || !channel.get("platforms").is_some_and(Value::is_object)
                    || !channel.get("aliases").is_some_and(Value::is_array)
                {
                    return Err(bad());
                }
            }
        }
        "areas.json" => {
            if !value.get("areas").is_some_and(Value::is_array) {
                return Err(bad());
            }
        }
        "cookies.json" => {
            if !value
                .pointer("/cookie_info/cookies")
                .is_some_and(Value::is_array)
                || !value.get("token_info").is_some_and(Value::is_object)
            {
                return Err(bad());
            }
        }
        "cookies.txt" | "niconico_cookies.txt" => {
            let text = value.as_str().ok_or_else(bad)?;
            if !text.is_empty() {
                crate::plugins::youtube_cookies::validate_netscape(text).map_err(|_| bad())?;
            }
        }
        "invalid_words.txt" => {
            if !value.is_string() {
                return Err(bad());
            }
        }
        "youtube_quota.json" => {
            if !value.get("day").is_some_and(Value::is_i64)
                || !value.get("keys").is_some_and(Value::is_object)
            {
                return Err(bad());
            }
        }
        "youtube_golive_hours.json" => {
            let channels = value
                .get("channels")
                .and_then(Value::as_object)
                .ok_or_else(bad)?;
            if !value.get("counted").is_some_and(Value::is_object)
                || !value.get("decayed_at").is_some_and(Value::is_i64)
            {
                return Err(bad());
            }
            for buckets in channels.values() {
                if !buckets
                    .as_array()
                    .is_some_and(|v| v.len() == 24 && v.iter().all(Value::is_number))
                {
                    return Err(bad());
                }
            }
        }
        _ => return Err(bad()),
    }
    Ok(())
}

fn read_source(path: &Path, name: &str, retire: bool) -> io::Result<Option<(Value, u64, Source)>> {
    let metadata = match path.symlink_metadata() {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if metadata.len() > MAX_DOCUMENT_BYTES as u64 {
        return Err(invalid(&format!("{name} 文件过大，未迁移")));
    }
    let bytes = fs::read(path)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| invalid(&format!("{name} 编码无效，原文件已保留")))?;
    let value = if name.ends_with(".txt") {
        Value::String(text.to_owned())
    } else {
        serde_json::from_str(text)
            .map_err(|_| invalid(&format!("{name} 格式无效，原文件已保留")))?
    };
    validate(name, &value)?;
    let at = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let source = Source {
        path: path.to_path_buf(),
        bytes: base64::engine::general_purpose::STANDARD.encode(&bytes),
        digest: paths::hex(&Sha256::digest(&bytes)),
        retire: retire && metadata.is_file() && !metadata.file_type().is_symlink(),
    };
    Ok(Some((value, at, source)))
}

fn retire_sources(sources: &[Source]) {
    for source in sources.iter().filter(|s| s.retire) {
        let Ok(metadata) = source.path.symlink_metadata() else {
            continue;
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        let Ok(bytes) = fs::read(&source.path) else {
            continue;
        };
        if paths::hex(&Sha256::digest(&bytes)) != source.digest {
            tracing::warn!(
                "旧数据文件在迁移后发生变化，已保留: {}",
                source
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            );
            continue;
        }
        if fs::remove_file(&source.path).is_err() {
            tracing::warn!(
                "旧数据文件无法移除，请检查权限: {}",
                source
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            );
        }
    }
}

pub(super) fn initialize(
    connection: &mut Connection,
    key: &Key,
    id: &str,
    data_dir: &Path,
    legacy: Option<&Path>,
) -> io::Result<()> {
    let complete: Option<String> = connection
        .query_row(
            "SELECT value FROM meta WHERE name='legacy_archive'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    let aad = format!("bilistream/{id}/legacy-recovery/v1");
    if let Some(archive) = complete {
        // A failed plaintext cleanup can resume, but never re-import stale files.
        let path = data_dir.join(&archive);
        if let Ok(bytes) = fs::read(path) {
            let raw = crypto::open(key, aad.as_bytes(), &bytes)?;
            let sources: Vec<Source> =
                serde_json::from_slice(&raw).map_err(|_| invalid("迁移恢复记录损坏"))?;
            retire_sources(&sources);
        }
        return Ok(());
    }
    let mut docs = HashMap::<String, (Value, u64)>::new();
    let mut sources = Vec::new();
    if let Some(root) = legacy {
        for &name in NAMES {
            if let Some((value, at, source)) = read_source(&root.join(name), name, true)? {
                docs.insert(name.to_owned(), (value, at));
                sources.push(source);
            }
        }
        // Import explicitly configured jars even when outside the installation,
        // but never delete external/shared files. They override default filenames.
        let config = docs.get("config.json").map(|d| d.0.clone());
        if let Some(config) = config {
            for (pointer, name) in [
                ("/youtube/cookies_file", "cookies.txt"),
                ("/niconico/cookies_file", "niconico_cookies.txt"),
            ] {
                if let Some(path) = config
                    .pointer(pointer)
                    .and_then(Value::as_str)
                    .filter(|p| !p.trim().is_empty())
                {
                    let path = PathBuf::from(path);
                    let path = if path.is_absolute() {
                        path
                    } else {
                        root.join(path)
                    };
                    if path != root.join(name) {
                        if let Some((value, at, source)) = read_source(&path, name, false)? {
                            docs.insert(name.to_owned(), (value, at));
                            sources.push(source);
                        } else {
                            return Err(invalid(&format!(
                                "配置的 {name} 文件不存在，原配置已保留"
                            )));
                        }
                    }
                }
            }
        }
    }
    // Verify an independent encrypted recovery copy before retiring any source.
    let raw = serde_json::to_vec(&sources).map_err(|_| invalid("无法创建迁移恢复记录"))?;
    let archive = format!(
        "legacy-recovery-{}.backup",
        paths::hex(&crypto::random::<12>()?)
    );
    let archive_path = data_dir.join(&archive);
    paths::write_private(&archive_path, &crypto::seal(key, aad.as_bytes(), &raw)?)?;
    if crypto::open(key, aad.as_bytes(), &fs::read(&archive_path)?)? != raw {
        return Err(invalid("迁移恢复副本验证失败"));
    }
    let result = (|| {
        let sql = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let mut tx = Transaction {
            sql,
            key,
            id,
            changed: HashMap::new(),
        };
        for (name, (value, at)) in &docs {
            tx.write_at(name, value.clone(), *at)?;
        }
        for (name, default) in [
            (
                "channels.json",
                include_str!("../../assets/defaults/channels.json"),
            ),
            (
                "areas.json",
                include_str!("../../assets/defaults/areas.json"),
            ),
        ] {
            if tx.read(name)?.is_none() {
                tx.write(
                    name,
                    serde_json::from_str(default).map_err(|_| invalid("内置默认数据无效"))?,
                )?;
            }
        }
        if tx.read("invalid_words.txt")?.is_none() {
            tx.write("invalid_words.txt", Value::String(String::new()))?;
        }
        for (name, (value, _)) in &docs {
            if read_row(&tx.sql, key, id, name)?.is_none_or(|d| &d.value != value) {
                return Err(invalid("迁移数据回读验证失败"));
            }
        }
        tx.sql
            .execute("INSERT INTO meta VALUES('legacy_archive',?1)", [&archive])
            .map_err(sql_error)?;
        tx.sql.commit().map_err(sql_error)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(archive_path);
        return result;
    }
    retire_sources(&sources);
    Ok(())
}
