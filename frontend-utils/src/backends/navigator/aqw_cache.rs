//! AQW asset cache.
//!
//! Flash Player in a browser (or Artix's Electron launcher) sits behind an HTTP
//! cache, so the hundreds of item/armor/hair SWFs AQW loads while you move
//! between rooms are downloaded once and then reused. Ruffle's desktop
//! navigator has no cache at all, so every room change re-downloads every
//! player's gear (a diagnostics log showed 464 loads for 325 distinct files in
//! a few minutes), and the art pops in late.
//!
//! This keeps static game files (SWF/images/sounds under `/gamefiles/` on
//! `*.aq.com`) in memory for the session, and versioned ones (URL has a query
//! such as `?v=4.372`) on disk across sessions. Disk entries are refreshed
//! after `DISK_MAX_AGE` so an asset Artix replaces without bumping the
//! version still gets picked up eventually.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};
use url::Url;

const MEMORY_BUDGET_BYTES: usize = 384 * 1024 * 1024;
const DISK_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DISK_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_ENTRY_BYTES: usize = 64 * 1024 * 1024;
const CACHEABLE_EXTENSIONS: [&str; 6] = ["swf", "png", "jpg", "jpeg", "gif", "mp3"];

static DISK_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
static MEMORY: OnceLock<Mutex<MemoryCache>> = OnceLock::new();

/// Sets the on-disk cache folder. Call once at startup; without it only the
/// in-memory cache is used. Also starts a background trim of old entries.
pub fn set_disk_dir(dir: PathBuf) {
    if disabled() {
        return;
    }
    if fs::create_dir_all(&dir).is_err() {
        let _ = DISK_DIR.set(None);
        return;
    }
    let trim_dir = dir.clone();
    if DISK_DIR.set(Some(dir)).is_ok() {
        std::thread::spawn(move || trim_disk(&trim_dir));
    }
}

fn disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| {
        std::env::var("RUFFLE_AQW_NO_ASSET_CACHE")
            .map(|v| !matches!(v.trim(), "" | "0" | "false" | "off"))
            .unwrap_or(false)
    })
}

fn disk_dir() -> Option<&'static Path> {
    DISK_DIR.get().and_then(|dir| dir.as_deref())
}

#[derive(Default)]
struct MemoryCache {
    entries: HashMap<String, Arc<Vec<u8>>>,
    order: VecDeque<String>,
    bytes: usize,
}

fn memory() -> &'static Mutex<MemoryCache> {
    MEMORY.get_or_init(|| Mutex::new(MemoryCache::default()))
}

/// What can be cached for a request, if anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheKey {
    pub url: String,
    /// Versioned URLs (with a query string) are also kept on disk.
    pub on_disk: bool,
}

/// Only plain GETs of static AQW game files are cached.
pub fn cache_key(url: &Url, is_get: bool, has_body: bool) -> Option<CacheKey> {
    if disabled() || !is_get || has_body {
        return None;
    }
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "aq.com" && !host.ends_with(".aq.com") {
        return None;
    }
    let path = url.path().to_ascii_lowercase();
    if !path.contains("/gamefiles/") {
        return None;
    }
    let extension = path.rsplit_once('.')?.1;
    if !CACHEABLE_EXTENSIONS.contains(&extension) {
        return None;
    }
    // Ignore the scheme so http:// and https:// share entries.
    let mut normalized = url.clone();
    let _ = normalized.set_scheme("https");
    normalized.set_fragment(None);
    Some(CacheKey {
        url: normalized.to_string(),
        on_disk: url.query().is_some_and(|q| !q.is_empty()),
    })
}

/// Bytes currently held by the in-memory copy (for the memory panel).
pub fn memory_bytes() -> usize {
    memory().lock().map(|cache| cache.bytes).unwrap_or(0)
}

/// Drops the in-memory copy (the disk copy stays). Returns bytes released.
pub fn clear_memory() -> usize {
    let Ok(mut cache) = memory().lock() else {
        return 0;
    };
    let released = cache.bytes;
    *cache = MemoryCache::default();
    released
}

pub fn get_memory(key: &CacheKey) -> Option<Vec<u8>> {
    let cache = memory().lock().ok()?;
    cache.entries.get(&key.url).map(|bytes| bytes.as_ref().clone())
}

pub fn put_memory(key: &CacheKey, bytes: &[u8]) {
    if bytes.is_empty() || bytes.len() > MAX_ENTRY_BYTES {
        return;
    }
    let Ok(mut cache) = memory().lock() else {
        return;
    };
    if let Some(old) = cache.entries.insert(key.url.clone(), Arc::new(bytes.to_vec())) {
        cache.bytes -= old.len();
    } else {
        cache.order.push_back(key.url.clone());
    }
    cache.bytes += bytes.len();
    while cache.bytes > MEMORY_BUDGET_BYTES {
        let Some(oldest) = cache.order.pop_front() else {
            break;
        };
        if let Some(evicted) = cache.entries.remove(&oldest) {
            cache.bytes -= evicted.len();
        }
    }
}

fn file_name(url: &str) -> String {
    // FNV-1a, 64-bit: stable across runs and Rust versions.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in url.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}.bin")
}

/// Blocking disk read; call from a worker thread.
pub fn read_disk(key: &CacheKey) -> Option<Vec<u8>> {
    if !key.on_disk {
        return None;
    }
    let path = disk_dir()?.join(file_name(&key.url));
    let modified = fs::metadata(&path).ok()?.modified().ok()?;
    if SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age > DISK_MAX_AGE)
    {
        return None;
    }
    let data = fs::read(&path).ok()?;
    // Layout: "<url>\n<bytes>". The URL guards against hash collisions.
    let split = data.iter().position(|b| *b == b'\n')?;
    if &data[..split] != key.url.as_bytes() {
        return None;
    }
    let body = data[split + 1..].to_vec();
    (!body.is_empty()).then_some(body)
}

/// Writes in the background so the game never waits on the disk.
pub fn write_disk(key: &CacheKey, bytes: &[u8]) {
    if !key.on_disk || bytes.is_empty() || bytes.len() > MAX_ENTRY_BYTES {
        return;
    }
    let Some(dir) = disk_dir() else {
        return;
    };
    let path = dir.join(file_name(&key.url));
    let temp = dir.join(format!("{}.tmp", file_name(&key.url)));
    let url = key.url.clone();
    let bytes = bytes.to_vec();
    std::thread::spawn(move || {
        let result = (|| -> std::io::Result<()> {
            let mut file = fs::File::create(&temp)?;
            file.write_all(url.as_bytes())?;
            file.write_all(b"\n")?;
            file.write_all(&bytes)?;
            file.sync_all().ok();
            drop(file);
            fs::rename(&temp, &path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
    });
}

/// Keeps the disk cache under its budget by deleting the oldest files.
fn trim_disk(dir: &Path) {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = read_dir
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some((meta.modified().ok()?, meta.len(), entry.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
    if total <= DISK_BUDGET_BYTES {
        return;
    }
    files.sort_by_key(|(modified, _, _)| *modified);
    for (_, len, path) in files {
        if total <= DISK_BUDGET_BYTES * 9 / 10 {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(url: &str) -> Option<CacheKey> {
        cache_key(&Url::parse(url).unwrap(), true, false)
    }

    #[test]
    fn only_static_aqw_game_files() {
        assert!(key("https://game.aq.com/game/gamefiles/items/swords/unarmed.swf?v=4.372").is_some());
        assert!(key("https://game.aq.com/game/gamefiles/hair/M/Blank.swf?v=1").unwrap().on_disk);
        assert!(!key("https://game.aq.com/game/gamefiles/title/Generic2.swf").unwrap().on_disk);
        assert_eq!(key("https://game.aq.com/game/api/data/servers"), None);
        assert_eq!(key("https://game.aq.com/game/gamefiles/foo.xml"), None);
        assert_eq!(key("https://example.com/gamefiles/a.swf"), None);
        assert_eq!(key("https://evilaq.com/gamefiles/a.swf"), None);
        let url = Url::parse("https://game.aq.com/game/gamefiles/a.swf").unwrap();
        assert_eq!(cache_key(&url, false, false), None); // POST
        assert_eq!(cache_key(&url, true, true), None); // has body
    }

    #[test]
    fn http_and_https_share_a_key() {
        assert_eq!(
            key("http://game.aq.com/game/gamefiles/a.swf?v=1"),
            key("https://game.aq.com/game/gamefiles/a.swf?v=1#x")
        );
    }

    #[test]
    fn memory_round_trip() {
        let k = key("https://game.aq.com/game/gamefiles/test/roundtrip.swf").unwrap();
        assert_eq!(get_memory(&k), None);
        put_memory(&k, b"FWS123");
        assert_eq!(get_memory(&k).as_deref(), Some(&b"FWS123"[..]));
    }

    #[test]
    fn disk_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let _ = DISK_DIR.set(Some(dir.path().to_path_buf()));
        let Some(active) = disk_dir() else { return };
        let k = key("https://game.aq.com/game/gamefiles/test/disk.swf?v=9").unwrap();
        write_disk(&k, b"CWSdata");
        let path = active.join(file_name(&k.url));
        for _ in 0..200 {
            if path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(read_disk(&k).as_deref(), Some(&b"CWSdata"[..]));
        let other = key("https://game.aq.com/game/gamefiles/test/disk.swf?v=10").unwrap();
        assert_eq!(read_disk(&other), None);
    }
}
