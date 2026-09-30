//! Installation-local data. Secrets are encrypted before SQLite sees them.
mod crypto;
mod legacy;
pub mod paths;
pub(crate) use legacy::validate as validate_document;

use crypto::Key;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, OnceLock, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;
const MAX_BACKUP_BYTES: usize = 128 * 1024 * 1024;
pub const NAMES: &[&str] = &[
    "config.json",
    "cookies.json",
    "channels.json",
    "areas.json",
    "cookies.txt",
    "niconico_cookies.txt",
    "invalid_words.txt",
    "youtube_quota.json",
    "youtube_golive_hours.json",
];

#[derive(Clone, Serialize, Deserialize)]
pub struct Document {
    pub value: Value,
    pub revision: u64,
    pub updated_at: u64,
}

type Cache = Arc<RwLock<HashMap<String, Document>>>;
type Job = Box<dyn FnOnce(&mut Connection, &Key, &str, &Cache) + Send>;

pub struct Store {
    data_dir: PathBuf,
    runtime_dir: PathBuf,
    cache: Cache,
    sender: Mutex<Option<mpsc::SyncSender<Job>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    _instance: InstanceLock,
}

struct InstanceLock(File);
impl Drop for InstanceLock {
    fn drop(&mut self) {
        // A concurrent fork can temporarily inherit the descriptor before exec.
        // Explicitly release the lock on initialization failure and shutdown.
        let _ = self.0.unlock();
    }
}

fn sql_error(error: rusqlite::Error) -> io::Error {
    // SQL errors never include bound parameters/secret payloads.
    io::Error::other(format!("本地数据库操作失败: {error}"))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn sensitive(name: &str) -> bool {
    !matches!(
        name,
        "channels.json"
            | "areas.json"
            | "invalid_words.txt"
            | "youtube_quota.json"
            | "youtube_golive_hours.json"
    )
}

fn context(id: &str, name: &str, revision: u64) -> Vec<u8> {
    format!("bilistream/v1/{id}/{name}/{revision}").into_bytes()
}

fn read_row(
    connection: &Connection,
    key: &Key,
    id: &str,
    name: &str,
) -> io::Result<Option<Document>> {
    let row: Option<(Vec<u8>, bool, u64, u64)> = connection
        .query_row(
            "SELECT payload, encrypted, revision, updated_at FROM documents WHERE name=?1",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(sql_error)?;
    let Some((bytes, encrypted, revision, updated_at)) = row else {
        return Ok(None);
    };
    if encrypted != sensitive(name) {
        return Err(invalid("数据库加密标记不匹配"));
    }
    let bytes = if encrypted {
        crypto::open(key, &context(id, name, revision), &bytes)?
    } else {
        bytes
    };
    let value = serde_json::from_slice(&bytes).map_err(|_| invalid("数据库记录格式无效"))?;
    Ok(Some(Document {
        value,
        revision,
        updated_at,
    }))
}

fn load_rows(
    connection: &Connection,
    key: &Key,
    id: &str,
) -> io::Result<HashMap<String, Document>> {
    let mut stmt = connection
        .prepare("SELECT name FROM documents")
        .map_err(sql_error)?;
    let names = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    let mut rows = HashMap::new();
    for name in names {
        if let Some(doc) = read_row(connection, key, id, &name)? {
            rows.insert(name, doc);
        }
    }
    Ok(rows)
}

pub struct Transaction<'a> {
    sql: rusqlite::Transaction<'a>,
    key: &'a Key,
    id: &'a str,
    changed: HashMap<String, Document>,
}

impl Transaction<'_> {
    pub fn read(&self, name: &str) -> io::Result<Option<Document>> {
        read_row(&self.sql, self.key, self.id, name)
    }

    pub fn write(&mut self, name: &str, value: Value) -> io::Result<u64> {
        self.write_at(name, value, now())
    }

    pub fn write_at(&mut self, name: &str, value: Value, updated_at: u64) -> io::Result<u64> {
        if name.len() > 128 || name.contains(['/', '\\', '\0']) {
            return Err(invalid("数据记录名称无效"));
        }
        let bytes = serde_json::to_vec(&value).map_err(|_| invalid("数据记录格式无效"))?;
        if bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(invalid("数据记录过大"));
        }
        let revision: u64 = self.sql.query_row("UPDATE meta SET value=CAST(value AS INTEGER)+1 WHERE name='revision' RETURNING CAST(value AS INTEGER)", [], |r| r.get(0)).map_err(sql_error)?;
        let encrypted = sensitive(name);
        let payload = if encrypted {
            crypto::seal(self.key, &context(self.id, name, revision), &bytes)?
        } else {
            bytes
        };
        self.sql.execute("INSERT INTO documents(name,payload,encrypted,revision,updated_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(name) DO UPDATE SET payload=excluded.payload, encrypted=excluded.encrypted, revision=excluded.revision, updated_at=excluded.updated_at", params![name, payload, encrypted, revision, updated_at]).map_err(sql_error)?;
        self.changed.insert(
            name.to_owned(),
            Document {
                value,
                revision,
                updated_at,
            },
        );
        Ok(revision)
    }

    pub fn compare_exchange(&mut self, name: &str, expected: u64, value: Value) -> io::Result<u64> {
        let current = self.read(name)?.map(|d| d.revision).unwrap_or(0);
        if current != expected {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "内容已更新，请刷新后重试",
            ));
        }
        self.write(name, value)
    }
}

impl Store {
    pub fn open(
        data_dir: PathBuf,
        key_file: PathBuf,
        legacy_dir: Option<PathBuf>,
    ) -> io::Result<Arc<Self>> {
        if std::path::absolute(&key_file)?.starts_with(std::path::absolute(&data_dir)?) {
            return Err(invalid("密钥文件必须在数据库目录之外"));
        }
        paths::private_dir(&data_dir)?;
        let lock_path = data_dir.join("instance.lock");
        let instance = match paths::create_private(&lock_path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path)?,
            Err(e) => return Err(e),
        };
        instance
            .try_lock()
            .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "此数据目录已有程序在运行"))?;
        let instance = InstanceLock(instance);
        let db_path = data_dir.join("bilistream.db");
        let existing = db_path.try_exists()?;
        let key = paths::load_key(&key_file, existing)?;
        if !existing {
            drop(paths::create_private(&db_path)?);
        }
        let mut connection = Connection::open(&db_path).map_err(sql_error)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(sql_error)?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA temp_store=MEMORY;").map_err(sql_error)?;
        let version: u32 = connection
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(sql_error)?;
        if version > 1 {
            return Err(invalid("数据库由更新版本创建，请升级程序"));
        }
        connection.execute_batch("CREATE TABLE IF NOT EXISTS meta(name TEXT PRIMARY KEY,value TEXT NOT NULL); CREATE TABLE IF NOT EXISTS documents(name TEXT PRIMARY KEY,payload BLOB NOT NULL,encrypted INTEGER NOT NULL,revision INTEGER NOT NULL,updated_at INTEGER NOT NULL); INSERT OR IGNORE INTO meta VALUES('revision','0'); PRAGMA user_version=1;").map_err(sql_error)?;
        let stored_id: Option<String> = connection
            .query_row("SELECT value FROM meta WHERE name='id'", [], |r| r.get(0))
            .optional()
            .map_err(sql_error)?;
        let id = match stored_id {
            Some(id) => id,
            None => paths::hex(&crypto::random::<16>()?),
        };
        connection
            .execute("INSERT OR IGNORE INTO meta VALUES('id',?1)", [&id])
            .map_err(sql_error)?;
        let check: Option<String> = connection
            .query_row("SELECT value FROM meta WHERE name='key_check'", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(sql_error)?;
        use base64::Engine;
        if let Some(check) = check {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(check)
                .map_err(|_| invalid("密钥校验记录无效"))?;
            if crypto::open(&key, id.as_bytes(), &bytes)? != b"bilistream-storage-key" {
                return Err(invalid("数据库密钥不匹配"));
            }
        } else {
            let check = crypto::seal(&key, id.as_bytes(), b"bilistream-storage-key")?;
            connection
                .execute(
                    "INSERT INTO meta VALUES('key_check',?1)",
                    [base64::engine::general_purpose::STANDARD.encode(check)],
                )
                .map_err(sql_error)?;
        }
        legacy::initialize(&mut connection, &key, &id, &data_dir, legacy_dir.as_deref())?;
        let cache = Arc::new(RwLock::new(load_rows(&connection, &key, &id)?));
        let runtime_dir = data_dir.join("runtime");
        paths::private_dir(&runtime_dir)?;
        crate::plugins::youtube_cookies::cleanup_stale(&runtime_dir);
        let (sender, receiver) = mpsc::sync_channel::<Job>(32);
        let worker_cache = Arc::clone(&cache);
        let worker = std::thread::Builder::new()
            .name("bilistream-storage".into())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    job(&mut connection, &key, &id, &worker_cache);
                }
                let _ = connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
            })?;
        Ok(Arc::new(Self {
            data_dir,
            runtime_dir,
            cache,
            sender: Mutex::new(Some(sender)),
            worker: Mutex::new(Some(worker)),
            _instance: instance,
        }))
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    pub fn read(&self, name: &str) -> io::Result<Option<Document>> {
        let cache = self
            .cache
            .read()
            .map_err(|_| io::Error::other("数据库缓存不可用"))?;
        Ok(cache.get(name).cloned())
    }

    pub fn revision(&self, name: &str) -> io::Result<Option<u64>> {
        Ok(self
            .cache
            .read()
            .map_err(|_| io::Error::other("数据库缓存不可用"))?
            .get(name)
            .map(|doc| doc.revision))
    }

    pub fn read_many(&self, names: &[&str]) -> io::Result<HashMap<String, Document>> {
        let cache = self
            .cache
            .read()
            .map_err(|_| io::Error::other("数据库缓存不可用"))?;
        Ok(names
            .iter()
            .filter_map(|name| cache.get(*name).map(|doc| (name.to_string(), doc.clone())))
            .collect())
    }

    pub fn snapshot(&self) -> io::Result<HashMap<String, Document>> {
        Ok(self
            .cache
            .read()
            .map_err(|_| io::Error::other("数据库缓存不可用"))?
            .clone())
    }

    pub fn transaction<T, F>(&self, edit: F) -> io::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Transaction<'_>) -> io::Result<T> + Send + 'static,
    {
        let (reply, answer) = mpsc::sync_channel(1);
        let job: Job = Box::new(move |connection, key, id, cache| {
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
                let value = edit(&mut tx)?;
                let changed = std::mem::take(&mut tx.changed);
                // Acquire publication lock before commit; never leave a committed
                // database with a stale cache because of a poisoned lock.
                let mut published = cache
                    .write()
                    .map_err(|_| io::Error::other("数据库缓存不可用"))?;
                tx.sql.commit().map_err(sql_error)?;
                published.extend(changed);
                Ok(value)
            })();
            let _ = reply.send(result);
        });
        self.sender
            .lock()
            .map_err(|_| io::Error::other("数据库工作线程不可用"))?
            .as_ref()
            .ok_or_else(|| io::Error::other("数据库已关闭"))?
            .send(job)
            .map_err(|_| io::Error::other("数据库工作线程已停止"))?;
        answer
            .recv()
            .map_err(|_| io::Error::other("数据库事务未完成"))?
    }

    pub fn write(&self, name: &str, value: Value) -> io::Result<u64> {
        let name = name.to_owned();
        self.transaction(move |tx| tx.write(&name, value))
    }

    pub fn compare_exchange(&self, name: &str, expected: u64, value: Value) -> io::Result<u64> {
        let name = name.to_owned();
        self.transaction(move |tx| tx.compare_exchange(&name, expected, value))
    }

    pub fn export_backup(&self, password: &str) -> io::Result<Vec<u8>> {
        let mut snapshot = self.snapshot()?;
        // Login sessions are installation-local and must not be restored elsewhere.
        snapshot.retain(|name, _| NAMES.contains(&name.as_str()));
        let bytes = serde_json::to_vec(&snapshot).map_err(|_| invalid("无法生成备份"))?;
        crypto::export(password, &bytes)
    }

    /// Explicit offline downgrade only. Never overwrite an existing directory.
    pub fn export_legacy(&self, destination: &Path) -> io::Result<()> {
        let mut docs = self.snapshot()?;
        if !docs.contains_key("config.json") {
            return Err(invalid("尚未配置，无法导出旧版数据"));
        }
        for (platform, name) in [
            ("youtube", "cookies.txt"),
            ("niconico", "niconico_cookies.txt"),
        ] {
            let has_cookie = docs
                .get(name)
                .is_some_and(|d| d.value.as_str().is_some_and(|s| !s.is_empty()));
            if let Some(config) = docs.get_mut("config.json") {
                if let Some(settings) = config
                    .value
                    .get_mut(platform)
                    .and_then(Value::as_object_mut)
                {
                    settings.insert(
                        "cookies_file".into(),
                        if has_cookie {
                            Value::String(name.into())
                        } else {
                            Value::Null
                        },
                    );
                }
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().mode(0o700).create(destination)?;
        }
        #[cfg(not(unix))]
        fs::create_dir(destination)?;
        let result = (|| {
            paths::private_dir(destination)?;
            for &name in NAMES {
                let Some(doc) = docs.get(name) else { continue };
                legacy::validate(name, &doc.value)?;
                let bytes = if name.ends_with(".txt") {
                    doc.value
                        .as_str()
                        .ok_or_else(|| invalid("文本记录无效"))?
                        .as_bytes()
                        .to_vec()
                } else {
                    serde_json::to_vec_pretty(&doc.value).map_err(|_| invalid("无法导出数据"))?
                };
                let path = destination.join(name);
                paths::write_private(&path, &bytes)?;
                let file = File::options().write(true).open(&path)?;
                file.set_times(
                    fs::FileTimes::new()
                        .set_modified(UNIX_EPOCH + Duration::from_secs(doc.updated_at)),
                )?;
                file.sync_all()?;
                if fs::read(path)? != bytes {
                    return Err(invalid("旧版数据导出验证失败"));
                }
            }
            #[cfg(unix)]
            File::open(destination)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(destination);
        }
        result
    }

    pub fn restore_backup(&self, password: &str, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > MAX_BACKUP_BYTES {
            return Err(invalid("备份文件过大"));
        }
        let plain = crypto::restore(password, bytes)?;
        let docs: HashMap<String, Document> =
            serde_json::from_slice(&plain).map_err(|_| invalid("备份内容无效"))?;
        for (name, doc) in &docs {
            legacy::validate(name, &doc.value)?;
        }
        self.transaction(move |tx| {
            // Restore only into an unused installation. Live replacement would
            // race active refreshes/streams and is deliberately not exposed.
            if tx.read("config.json")?.is_some() || tx.read("cookies.json")?.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "请在空的数据目录中恢复备份",
                ));
            }
            for (name, doc) in docs {
                tx.write_at(&name, doc.value, doc.updated_at)?;
            }
            Ok(())
        })
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        self.sender
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = worker.join();
        }
    }
}

static GLOBAL: OnceLock<Arc<Store>> = OnceLock::new();
static INITIALIZING: Mutex<()> = Mutex::new(());

pub fn global() -> io::Result<Arc<Store>> {
    if let Some(store) = GLOBAL.get() {
        return Ok(Arc::clone(store));
    }
    let _guard = INITIALIZING
        .lock()
        .map_err(|_| io::Error::other("数据库初始化不可用"))?;
    if let Some(store) = GLOBAL.get() {
        return Ok(Arc::clone(store));
    }
    #[cfg(not(test))]
    let (data, key, legacy) = paths::global_paths()?;
    #[cfg(test)]
    let (data, key, legacy) = {
        let root =
            std::env::temp_dir().join(format!("bilistream-unit-store-{}", std::process::id()));
        (
            root.join("data"),
            root.join("keys/master.key"),
            root.join("legacy"),
        )
    };
    let store = Store::open(data, key, Some(legacy))?;
    let _ = GLOBAL.set(Arc::clone(&store));
    Ok(store)
}

pub fn cache_file(name: &str) -> io::Result<PathBuf> {
    if !matches!(name, "cover.jpg" | "pic_for_crop.jpg") {
        return Err(invalid("缓存文件名称无效"));
    }
    let directory = global()?.data_dir().join("cache");
    paths::private_dir(&directory)?;
    Ok(directory.join(name))
}

pub fn read_json<T: serde::de::DeserializeOwned>(name: &str) -> io::Result<T> {
    let doc = global()?
        .read(name)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("尚未配置 {name}")))?;
    serde_json::from_value(doc.value).map_err(|_| invalid(&format!("{name} 数据格式无效")))
}

pub fn read_text(name: &str) -> io::Result<String> {
    read_json(name)
}
pub fn contains(name: &str) -> io::Result<bool> {
    Ok(global()?.revision(name)?.is_some())
}
pub fn write_json<T: Serialize>(name: &str, value: &T) -> io::Result<u64> {
    global()?.write(
        name,
        serde_json::to_value(value).map_err(|_| invalid("数据格式无效"))?,
    )
}

#[cfg(test)]
mod tests;
