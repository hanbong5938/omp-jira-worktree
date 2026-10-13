//! Private on-disk state: mkdir locks (never waited on or stolen), atomic JSON
//! records, and exclusively created private files. Directories are 0700, files
//! 0600; symlinks, foreign owners and group/other permission bits are refused.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
const OWNER_FILE: &str = "owner";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateError {
    /// A lock directory already exists; it is never waited on or stolen.
    LockBusy(PathBuf),
    Io(String),
    Invalid(String),
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StateError::LockBusy(path) => write!(
                f,
                "state lock is busy: {}. Another jira-tern operation holds it, or a crashed process left it behind. \
                 If no such operation is running, inspect {} (pid/attempt) and remove the lock directory manually.",
                path.display(),
                path.join(OWNER_FILE).display()
            ),
            StateError::Io(message) | StateError::Invalid(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for StateError {}

fn io_error(context: &Path, error: io::Error) -> StateError {
    StateError::Io(format!("{}: {error}", context.display()))
}

/// 32 lowercase hex characters from `/dev/urandom`.
pub fn random_id() -> String {
    let mut bytes = [0u8; 16];
    let filled = File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes));
    if filled.is_err() {
        // /dev/urandom is always present on supported platforms; fall back to time+pid entropy.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seed = sha256(format!("{nanos}:{}", std::process::id()).as_bytes());
        bytes.copy_from_slice(&seed[..16]);
    }
    hex(&bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Lowercase hex SHA-256 of `data`.
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    hex(&sha256(data))
}

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = Vec::with_capacity(data.len() + 72);
    message.extend_from_slice(data);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_be_bytes());
    for chunk in message.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// RFC 3339 UTC timestamp (second precision) for lock owner files.
fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn assert_private(meta: &fs::Metadata, target: &Path, directory: bool) -> Result<(), StateError> {
    let kind = if directory { "directory" } else { "file" };
    if meta.file_type().is_symlink() {
        return Err(StateError::Invalid(format!(
            "state {kind} {} is a symlink; refusing to use it (inspect manually)",
            target.display()
        )));
    }
    if if directory {
        !meta.is_dir()
    } else {
        !meta.is_file()
    } {
        return Err(StateError::Invalid(format!(
            "state path {} is not a regular {kind}; inspect manually",
            target.display()
        )));
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        let expected = if directory { "0700" } else { "0600" };
        return Err(StateError::Invalid(format!(
            "state {kind} {} has group/other permissions (mode {mode:04o}, expected {expected}); inspect manually",
            target.display()
        )));
    }
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if meta.uid() != uid {
        return Err(StateError::Invalid(format!(
            "state {kind} {} is owned by uid {}, not the current user; inspect manually",
            target.display(),
            meta.uid()
        )));
    }
    Ok(())
}

fn lstat_opt(target: &Path) -> Result<Option<fs::Metadata>, StateError> {
    match fs::symlink_metadata(target) {
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(target, error)),
    }
}

fn fsync_dir(dir: &Path) -> Result<(), StateError> {
    File::open(dir)
        .and_then(|f| f.sync_all())
        .map_err(|e| io_error(dir, e))
}

/// Open a private regular file without following symlinks; `None` when absent.
fn read_private_file(target: &Path) -> Result<Option<String>, StateError> {
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(target);
    let mut file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return Err(StateError::Invalid(format!(
                "state file {} is a symlink; refusing to use it (inspect manually)",
                target.display()
            )));
        }
        Err(error) => return Err(io_error(target, error)),
    };
    let meta = file.metadata().map_err(|e| io_error(target, e))?;
    assert_private(&meta, target, false)?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|e| io_error(target, e))?;
    Ok(Some(text))
}

fn create_exclusive(target: &Path, text: &str) -> Result<(), StateError> {
    let opened = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW)
        .open(target);
    let mut file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            return Err(StateError::Invalid(format!(
                "state file {} already exists; refusing to overwrite it",
                target.display()
            )));
        }
        Err(error) => return Err(io_error(target, error)),
    };
    if let Err(error) = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
    {
        drop(file);
        let _ = fs::remove_file(target);
        return Err(io_error(target, error));
    }
    Ok(())
}

/// Locks taken by one `with_locks` attempt; released on drop (including unwinding).
struct HeldLocks {
    paths: Vec<PathBuf>,
    attempt: String,
}

impl HeldLocks {
    fn release(&mut self) -> Option<String> {
        let mut failures = Vec::new();
        for lock in self.paths.drain(..).rev() {
            let owner = lock.join(OWNER_FILE);
            let outcome = (|| -> Result<Option<String>, StateError> {
                if let Some(text) = read_private_file(&owner)? {
                    let value: serde_json::Value = serde_json::from_str(&text)
                        .map_err(|e| StateError::Invalid(format!("owner file is not JSON: {e}")))?;
                    if value.get("attempt").and_then(|a| a.as_str()) != Some(self.attempt.as_str())
                    {
                        return Ok(Some(format!(
                            "{} is not owned by this attempt; left in place",
                            lock.display()
                        )));
                    }
                    fs::remove_file(&owner).map_err(|e| io_error(&owner, e))?;
                }
                fs::remove_dir(&lock).map_err(|e| io_error(&lock, e))?;
                Ok(None)
            })();
            match outcome {
                Ok(None) => {}
                Ok(Some(failure)) => failures.push(failure),
                Err(error) => failures.push(format!("{}: {error}", lock.display())),
            }
        }
        if failures.is_empty() {
            None
        } else {
            Some(format!(
                "could not release state locks (inspect and remove manually): {}",
                failures.join("; ")
            ))
        }
    }
}

impl Drop for HeldLocks {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

#[derive(Debug, Clone)]
pub struct StateStore {
    base: PathBuf,
}

impl StateStore {
    pub fn new(root: PathBuf) -> Result<Self, StateError> {
        if !root.is_absolute() {
            return Err(StateError::Invalid(format!(
                "state root must be an absolute path: {}",
                root.display()
            )));
        }
        let mut base = PathBuf::new();
        for component in root.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    base.pop();
                }
                other => base.push(other),
            }
        }
        Ok(Self { base })
    }

    pub fn root(&self) -> &Path {
        &self.base
    }

    fn resolve<'a>(&self, relative: &'a str) -> Result<(PathBuf, Vec<&'a str>), StateError> {
        let invalid = || StateError::Invalid(format!("invalid state path: {relative:?}"));
        if relative.is_empty()
            || relative.starts_with('/')
            || relative.contains('\\')
            || relative.contains('\0')
        {
            return Err(invalid());
        }
        let parts: Vec<&str> = relative.split('/').collect();
        if parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == "..")
        {
            return Err(invalid());
        }
        let mut absolute = self.base.clone();
        absolute.extend(&parts);
        Ok((absolute, parts))
    }

    /// Create missing private directories (0700) from the root down; validate existing ones.
    fn ensure_directories(&self, parts: &[&str]) -> Result<PathBuf, StateError> {
        if lstat_opt(&self.base)?.is_none() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(DIR_MODE)
                .create(&self.base)
                .map_err(|e| io_error(&self.base, e))?;
        }
        let meta = fs::symlink_metadata(&self.base).map_err(|e| io_error(&self.base, e))?;
        assert_private(&meta, &self.base, true)?;
        let mut current = self.base.clone();
        for part in parts {
            let parent = current.clone();
            current.push(part);
            if lstat_opt(&current)?.is_none() {
                match fs::DirBuilder::new().mode(DIR_MODE).create(&current) {
                    Ok(()) => fsync_dir(&parent)?,
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(io_error(&current, error)),
                }
            }
            let meta = fs::symlink_metadata(&current).map_err(|e| io_error(&current, e))?;
            assert_private(&meta, &current, true)?;
        }
        Ok(current)
    }

    /// Validate existing directories for reads; false when any component is absent.
    fn directories_exist(&self, parts: &[&str]) -> Result<bool, StateError> {
        let mut current = self.base.clone();
        for part in std::iter::once(None).chain(parts.iter().map(Some)) {
            if let Some(part) = part {
                current.push(part);
            }
            match lstat_opt(&current)? {
                None => return Ok(false),
                Some(meta) => assert_private(&meta, &current, true)?,
            }
        }
        Ok(true)
    }

    /// Hold every key's lock (sorted, deduplicated) while `f` runs. A busy lock fails
    /// immediately with `LockBusy`; locks are never waited on or stolen.
    pub fn with_locks<T>(&self, keys: &[String], f: impl FnOnce() -> T) -> Result<T, StateError> {
        let mut hashes: Vec<String> = keys.iter().map(|k| sha256_hex(k.as_bytes())).collect();
        hashes.sort();
        hashes.dedup();
        let locks_dir = self.ensure_directories(&["locks"])?;
        let mut held = HeldLocks {
            paths: Vec::new(),
            attempt: random_id(),
        };
        let acquired = (|| -> Result<(), StateError> {
            for hash in &hashes {
                let lock = locks_dir.join(hash);
                match fs::DirBuilder::new().mode(DIR_MODE).create(&lock) {
                    Ok(()) => {}
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                        return Err(StateError::LockBusy(lock));
                    }
                    Err(error) => return Err(io_error(&lock, error)),
                }
                held.paths.push(lock.clone());
                let owner = serde_json::json!({
                    "pid": std::process::id(),
                    "attempt": held.attempt,
                    "createdAt": now_rfc3339(),
                });
                create_exclusive(&lock.join(OWNER_FILE), &format!("{owner}\n"))?;
            }
            Ok(())
        })();
        if let Err(error) = acquired {
            return Err(match held.release() {
                None => error,
                Some(release) => match error {
                    StateError::LockBusy(_) => error,
                    other => StateError::Io(format!("{other}; {release}")),
                },
            });
        }
        let value = f();
        match held.release() {
            None => Ok(value),
            Some(release) => Err(StateError::Io(release)),
        }
    }

    pub fn read_record<T: serde::de::DeserializeOwned>(
        &self,
        rel: &str,
    ) -> Result<Option<T>, StateError> {
        let (absolute, parts) = self.resolve(rel)?;
        if !self.directories_exist(&parts[..parts.len() - 1])? {
            return Ok(None);
        }
        let Some(text) = read_private_file(&absolute)? else {
            return Ok(None);
        };
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            StateError::Invalid(format!(
                "corrupt state record {}: {e}; it is never overwritten — inspect it manually",
                absolute.display()
            ))
        })?;
        serde_json::from_value(value).map(Some).map_err(|e| {
            StateError::Invalid(format!(
                "invalid state record {}: {e}; it is never overwritten — inspect it manually",
                absolute.display()
            ))
        })
    }

    pub fn write_record<T: serde::Serialize>(
        &self,
        rel: &str,
        value: &T,
    ) -> Result<(), StateError> {
        let (absolute, parts) = self.resolve(rel)?;
        let text = serde_json::to_string(value).map_err(|e| {
            StateError::Invalid(format!(
                "state record {} has no JSON representation: {e}",
                absolute.display()
            ))
        })?;
        let dir = self.ensure_directories(&parts[..parts.len() - 1])?;
        if let Some(existing) = read_private_file(&absolute)?
            && let Err(error) = serde_json::from_str::<serde_json::Value>(&existing)
        {
            return Err(StateError::Invalid(format!(
                "corrupt state record {}: {error}; refusing to overwrite it — inspect it manually",
                absolute.display()
            )));
        }
        let name = parts[parts.len() - 1];
        let temp = dir.join(format!(".{name}.{}.tmp", random_id()));
        create_exclusive(&temp, &format!("{text}\n"))?;
        if let Err(error) = fs::rename(&temp, &absolute) {
            let _ = fs::remove_file(&temp);
            return Err(io_error(&absolute, error));
        }
        fsync_dir(&dir)
    }

    pub fn write_private_file(&self, rel: &str, text: &str) -> Result<PathBuf, StateError> {
        let (absolute, parts) = self.resolve(rel)?;
        let dir = self.ensure_directories(&parts[..parts.len() - 1])?;
        create_exclusive(&absolute, text)?;
        fsync_dir(&dir)?;
        Ok(absolute)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;

    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("jt-state-{}", random_id()));
            fs::create_dir_all(&dir).unwrap();
            Temp(fs::canonicalize(dir).unwrap())
        }
        fn root(&self) -> PathBuf {
            self.0.join("state")
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().mode() & 0o777
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn lock_entries(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(root.join("locks"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    type Obj = serde_json::Value;

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(random_id().len(), 32);
    }

    #[test]
    fn round_trips_a_record_in_private_directories_and_files() {
        let temp = Temp::new();
        let root = temp.root();
        let store = StateStore::new(root.clone()).unwrap();
        assert_eq!(store.read_record::<Obj>("receipts/a.json").unwrap(), None);
        store
            .write_record("receipts/a.json", &serde_json::json!({"state": "pending"}))
            .unwrap();
        store
            .write_record("receipts/a.json", &serde_json::json!({"state": "ready"}))
            .unwrap();
        assert_eq!(
            store.read_record::<Obj>("receipts/a.json").unwrap(),
            Some(serde_json::json!({"state": "ready"}))
        );
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&root.join("receipts")), 0o700);
        assert_eq!(mode(&root.join("receipts/a.json")), 0o600);
        let entries: Vec<_> = fs::read_dir(root.join("receipts")).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        let temp = Temp::new();
        let store = StateStore::new(temp.root()).unwrap();
        for bad in ["../x.json", "/etc/passwd", "a/../b.json", "", "a//b"] {
            let w = store.write_record(bad, &serde_json::json!({})).unwrap_err();
            assert!(w.to_string().contains("invalid state path"), "{w}");
            let r = store.read_record::<Obj>(bad).unwrap_err();
            assert!(r.to_string().contains("invalid state path"), "{r}");
        }
        assert!(StateStore::new(PathBuf::from("relative")).is_err());
    }

    #[test]
    fn refuses_permissive_record_without_overwriting() {
        let temp = Temp::new();
        let store = StateStore::new(temp.root()).unwrap();
        store
            .write_record("r.json", &serde_json::json!({"v": 1}))
            .unwrap();
        let file = temp.root().join("r.json");
        chmod(&file, 0o644);
        assert!(
            store
                .read_record::<Obj>("r.json")
                .unwrap_err()
                .to_string()
                .contains("group/other permissions")
        );
        assert!(
            store
                .write_record("r.json", &serde_json::json!({"v": 2}))
                .unwrap_err()
                .to_string()
                .contains("group/other permissions")
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), "{\"v\":1}\n");
    }

    #[test]
    fn refuses_permissive_state_directory() {
        let temp = Temp::new();
        let store = StateStore::new(temp.root()).unwrap();
        store
            .write_record("r.json", &serde_json::json!({"v": 1}))
            .unwrap();
        chmod(&temp.root(), 0o755);
        assert!(
            store
                .read_record::<Obj>("r.json")
                .unwrap_err()
                .to_string()
                .contains("group/other permissions")
        );
        assert!(
            store
                .write_record("r.json", &serde_json::json!({"v": 2}))
                .unwrap_err()
                .to_string()
                .contains("group/other permissions")
        );
    }

    #[test]
    fn refuses_symlinked_record_without_touching_target() {
        let temp = Temp::new();
        let store = StateStore::new(temp.root()).unwrap();
        store
            .write_record("seed.json", &serde_json::json!({}))
            .unwrap();
        let target = temp.0.join("elsewhere.json");
        fs::write(&target, "{\"outside\":true}").unwrap();
        chmod(&target, 0o600);
        std::os::unix::fs::symlink(&target, temp.root().join("r.json")).unwrap();
        assert!(
            store
                .read_record::<Obj>("r.json")
                .unwrap_err()
                .to_string()
                .contains("symlink")
        );
        assert!(
            store
                .write_record("r.json", &serde_json::json!({"v": 2}))
                .unwrap_err()
                .to_string()
                .contains("symlink")
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "{\"outside\":true}");
    }

    #[test]
    fn reports_corrupt_record_and_never_overwrites_it() {
        let temp = Temp::new();
        let store = StateStore::new(temp.root()).unwrap();
        store
            .write_record("r.json", &serde_json::json!({"v": 1}))
            .unwrap();
        let file = temp.root().join("r.json");
        fs::write(&file, "{not json").unwrap();
        assert!(
            store
                .read_record::<Obj>("r.json")
                .unwrap_err()
                .to_string()
                .contains("corrupt state record")
        );
        assert!(
            store
                .write_record("r.json", &serde_json::json!({"v": 2}))
                .unwrap_err()
                .to_string()
                .contains("corrupt state record")
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), "{not json");
    }

    #[test]
    fn surfaces_shape_mismatch_as_invalid_record() {
        let temp = Temp::new();
        let store = StateStore::new(temp.root()).unwrap();
        store
            .write_record("r.json", &serde_json::json!({"v": 1}))
            .unwrap();
        let error = store.read_record::<Vec<String>>("r.json").unwrap_err();
        assert!(
            error.to_string().contains("invalid state record"),
            "{error}"
        );
    }

    #[test]
    fn private_files_are_exclusive_and_private() {
        let temp = Temp::new();
        let store = StateStore::new(temp.root()).unwrap();
        let written = store.write_private_file("handoffs/x.md", "hello").unwrap();
        assert_eq!(written, temp.root().join("handoffs/x.md"));
        assert_eq!(fs::read_to_string(&written).unwrap(), "hello");
        assert_eq!(mode(&written), 0o600);
        assert_eq!(mode(written.parent().unwrap()), 0o700);
        let error = store
            .write_private_file("handoffs/x.md", "other")
            .unwrap_err();
        assert!(error.to_string().contains("already exists"));
        assert_eq!(fs::read_to_string(&written).unwrap(), "hello");
    }

    #[test]
    fn concurrent_holder_gets_busy_error_naming_the_lock() {
        let temp = Temp::new();
        let root = temp.root();
        let first = StateStore::new(root.clone()).unwrap();
        let second = StateStore::new(root.clone()).unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            first
                .with_locks(&["dest:/a".to_string(), "name:x".to_string()], || {
                    entered_tx.send(()).unwrap();
                    gate_rx.recv().unwrap();
                    "first"
                })
                .unwrap()
        });
        entered_rx.recv().unwrap();
        let mut ran = false;
        let busy = second
            .with_locks(&["name:x".to_string()], || ran = true)
            .unwrap_err();
        let StateError::LockBusy(lock) = &busy else {
            panic!("expected LockBusy, got {busy}")
        };
        assert!(busy.to_string().contains(&lock.display().to_string()));
        assert!(busy.to_string().contains("manually"));
        assert!(!ran);
        gate_tx.send(()).unwrap();
        assert_eq!(holder.join().unwrap(), "first");
        assert_eq!(
            second
                .with_locks(&["name:x".to_string()], || "second")
                .unwrap(),
            "second"
        );
        assert!(lock_entries(&root).is_empty());
    }

    #[test]
    fn partial_acquisition_releases_only_its_own_locks() {
        let temp = Temp::new();
        let root = temp.root();
        let store = StateStore::new(root.clone()).unwrap();
        let rival = store.clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            rival
                .with_locks(&["b".to_string()], || {
                    entered_tx.send(()).unwrap();
                    gate_rx.recv().unwrap();
                })
                .unwrap()
        });
        entered_rx.recv().unwrap();
        let before = lock_entries(&root);
        let keys = ["a", "b", "c"].map(String::from);
        assert!(matches!(
            store.with_locks(&keys, || ()),
            Err(StateError::LockBusy(_))
        ));
        assert_eq!(lock_entries(&root), before);
        gate_tx.send(()).unwrap();
        holder.join().unwrap();
        assert!(lock_entries(&root).is_empty());
    }

    #[test]
    fn crashed_process_lock_is_never_stolen() {
        let temp = Temp::new();
        let root = temp.root();
        let store = StateStore::new(root.clone()).unwrap();
        let lock = store
            .with_locks(&["k".to_string()], || {
                let lock = root.join("locks").join(&lock_entries(&root)[0]);
                let owner: Obj =
                    serde_json::from_str(&fs::read_to_string(lock.join("owner")).unwrap()).unwrap();
                assert_eq!(owner["pid"], serde_json::json!(std::process::id()));
                assert!(owner["attempt"].is_string());
                lock
            })
            .unwrap();
        fs::DirBuilder::new().mode(0o700).create(&lock).unwrap();
        fs::write(lock.join("owner"), "{\"pid\":999999,\"attempt\":\"dead\"}").unwrap();
        chmod(&lock.join("owner"), 0o600);
        let error = store.with_locks(&["k".to_string()], || ()).unwrap_err();
        assert!(error.to_string().contains(&lock.display().to_string()));
        assert!(
            fs::read_to_string(lock.join("owner"))
                .unwrap()
                .contains("dead")
        );
    }

    #[test]
    fn releases_locks_when_the_action_fails() {
        let temp = Temp::new();
        let root = temp.root();
        let store = StateStore::new(root.clone()).unwrap();
        let result: Result<(), String> = store
            .with_locks(&["k".to_string()], || Err("boom".to_string()))
            .unwrap();
        assert_eq!(result, Err("boom".to_string()));
        assert!(lock_entries(&root).is_empty());
    }
}
