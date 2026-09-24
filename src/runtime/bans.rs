//! Persistent content bans and the gate that serializes them with wallpaper applies.

use super::*;

pub(super) const BANS_FILENAME: &str = "bans.txt";

#[derive(Default)]
pub(super) struct BanCoordinator {
    pub(super) updates: Mutex<()>,
    pub(super) hashes: RwLock<HashSet<String>>,
}

/// Shared synchronization point for ban persistence and the final wallpaper apply.
#[derive(Clone, Default)]
pub struct BanGate(pub(super) Arc<BanCoordinator>);

impl BanGate {
    pub(super) fn new(hashes: HashSet<String>) -> Self {
        Self(Arc::new(BanCoordinator {
            updates: Mutex::new(()),
            hashes: RwLock::new(hashes),
        }))
    }

    pub(super) fn is_banned(&self, hash: &str) -> bool {
        self.0.hashes.read().contains(hash)
    }

    pub(super) fn run_if_allowed<T>(
        &self,
        hash: &str,
        apply: impl FnOnce() -> Result<T>,
    ) -> Result<Option<T>> {
        let hashes = self.0.hashes.read();
        if hashes.contains(hash) {
            return Ok(None);
        }
        let result = apply().map(Some);
        drop(hashes);
        result
    }
}

pub(super) fn bans_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name(BANS_FILENAME)
}

pub(super) fn normalize_ban_hash(hash: &str) -> Result<String> {
    let hash = hash.trim();
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("ban hash must be exactly 64 hexadecimal characters");
    }
    Ok(hash.to_ascii_lowercase())
}

pub(super) fn load_bans(path: &Path) -> Result<HashSet<String>> {
    if !path.exists() {
        return Ok(HashSet::new());
    }
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("read bans sidecar {}", path.display()))?;
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            normalize_ban_hash(line)
                .with_context(|| format!("bans sidecar {} line {}", path.display(), index + 1))
        })
        .collect()
}

pub(super) fn persist_bans(path: &Path, bans: &HashSet<String>) -> Result<()> {
    let mut hashes: Vec<&str> = bans.iter().map(String::as_str).collect();
    hashes.sort_unstable();
    let mut content = hashes.join("\n");
    if !content.is_empty() {
        content.push('\n');
    }
    let tmp = path.with_extension("txt.tmp");
    write_synced(&tmp, content.as_bytes())?;
    crate::playlist::replace_file(&tmp, path)
}
