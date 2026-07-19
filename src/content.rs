use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

const CONTENT_SCHEMA_VERSION: u32 = 3;
const LEGACY_RECONCILIATION_SCHEMA_VERSION: u32 = 3;
const CONTENT_FILENAME: &str = "content.json";
const MAX_AUTOTAG_RAW_BYTES: usize = 256 * 1024;
const MAX_AUTOTAG_ENDPOINT_BYTES: usize = 2 * 1024;
const MAX_AUTOTAG_LABEL_BYTES: usize = 256;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutoTagProvenance {
    pub model: String,
    pub confidence: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tagged_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContentMetadata {
    /// Known path spellings for this exact byte sequence.
    pub aliases: Vec<String>,
    /// Shared tag kind -> normalized tags.
    pub tag_groups: BTreeMap<String, Vec<String>>,
    /// Default star rating. A playlist-local legacy rating remains an override.
    pub rating: Option<u8>,
    /// Legacy playlists disagreed, so their local ratings remain authoritative
    /// until the user explicitly assigns a shared content rating.
    #[serde(skip_serializing_if = "is_false")]
    pub rating_conflicted: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autotag: Option<AutoTagProvenance>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TagFilters {
    /// Every kind must match at least one listed tag.
    pub include: BTreeMap<String, Vec<String>>,
    /// Any matching tag rejects the image.
    pub exclude: BTreeMap<String, Vec<String>>,
}

impl TagFilters {
    pub fn new(
        include: BTreeMap<String, Vec<String>>,
        exclude: BTreeMap<String, Vec<String>>,
    ) -> Result<Self> {
        Ok(Self {
            include: normalize_filter_groups(include)?,
            exclude: normalize_filter_groups(exclude)?,
        })
    }

    pub fn accepts(&self, groups: Option<&BTreeMap<String, Vec<String>>>) -> bool {
        let matches = |kind: &str, wanted: &[String]| {
            groups
                .and_then(|groups| groups.get(kind))
                .is_some_and(|actual| wanted.iter().any(|tag| actual.contains(tag)))
        };
        self.include
            .iter()
            .all(|(kind, wanted)| matches(kind, wanted))
            && !self
                .exclude
                .iter()
                .any(|(kind, unwanted)| matches(kind, unwanted))
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContentStore {
    entries: BTreeMap<String, ContentMetadata>,
    legacy_migrated: bool,
    legacy_reconciliation: bool,
    pending_legacy: BTreeSet<LegacyMetadata>,
    dynamic_playlists: BTreeSet<String>,
    playlist_filters: BTreeMap<String, TagFilters>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyMetadata {
    playlist: String,
    path: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredContent {
    schema: u32,
    #[serde(default)]
    legacy_migrated: bool,
    #[serde(default)]
    pending_legacy: BTreeSet<LegacyMetadata>,
    #[serde(default)]
    dynamic_playlists: BTreeSet<String>,
    #[serde(default)]
    playlist_filters: BTreeMap<String, TagFilters>,
    entries: BTreeMap<String, ContentMetadata>,
}

pub fn content_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name(CONTENT_FILENAME)
}

pub fn load_content(path: &Path) -> Result<ContentStore> {
    if !path.exists() {
        return Ok(ContentStore::default());
    }
    let bytes =
        std::fs::read(path).with_context(|| format!("read content metadata {}", path.display()))?;
    parse_content(&bytes)
        .map_err(|error| anyhow::anyhow!("parse content metadata {}: {error:#}", path.display()))
}

pub(crate) fn parse_content(bytes: &[u8]) -> Result<ContentStore> {
    let stored: StoredContent =
        serde_json::from_slice(bytes).context("parse content metadata JSON")?;
    if !(1..=CONTENT_SCHEMA_VERSION).contains(&stored.schema) {
        anyhow::bail!(
            "unsupported content metadata schema {}; supported versions are 1 through {}",
            stored.schema,
            CONTENT_SCHEMA_VERSION
        );
    }
    let mut store = ContentStore {
        entries: stored.entries,
        legacy_migrated: stored.legacy_migrated,
        legacy_reconciliation: stored.schema < LEGACY_RECONCILIATION_SCHEMA_VERSION
            && stored.legacy_migrated,
        pending_legacy: stored.pending_legacy,
        dynamic_playlists: stored.dynamic_playlists,
        playlist_filters: stored.playlist_filters,
    };
    store.normalize()?;
    Ok(store)
}

pub(crate) fn serialize_content(store: &ContentStore) -> Result<Vec<u8>> {
    let mut normalized = store.clone();
    normalized.normalize()?;
    let stored = StoredContent {
        schema: CONTENT_SCHEMA_VERSION,
        legacy_migrated: normalized.legacy_migrated,
        pending_legacy: normalized.pending_legacy,
        dynamic_playlists: normalized.dynamic_playlists,
        playlist_filters: normalized.playlist_filters,
        entries: normalized.entries,
    };
    serde_json::to_vec_pretty(&stored).context("serialize content metadata")
}

pub fn persist_content(store: &ContentStore, path: &Path) -> Result<()> {
    let bytes = serialize_content(store)?;
    let tmp = path.with_extension("json.tmp");
    crate::playlist::write_synced(&tmp, &bytes)?;
    crate::playlist::replace_file(&tmp, path)
}

impl ContentStore {
    pub fn needs_legacy_migration(&self) -> bool {
        !self.legacy_migrated
    }

    pub fn finish_legacy_migration(&mut self) -> bool {
        let changed = !self.legacy_migrated;
        self.legacy_migrated = true;
        changed
    }

    pub fn needs_legacy_reconciliation(&self) -> bool {
        self.legacy_reconciliation
    }

    pub fn finish_legacy_reconciliation(&mut self) -> bool {
        std::mem::take(&mut self.legacy_reconciliation)
    }

    pub fn pending_legacy(&self) -> impl Iterator<Item = (&str, &str)> {
        self.pending_legacy
            .iter()
            .map(|item| (item.playlist.as_str(), item.path.as_str()))
    }

    pub fn is_legacy_pending(&self, playlist: &str, path: &str) -> bool {
        self.pending_legacy
            .iter()
            .any(|item| item.playlist == playlist && item.path == path)
    }

    pub fn replace_pending_legacy(
        &mut self,
        pending: impl IntoIterator<Item = (String, String)>,
    ) -> Result<bool> {
        let pending: BTreeSet<LegacyMetadata> = pending
            .into_iter()
            .map(|(playlist, path)| LegacyMetadata { playlist, path })
            .collect();
        for item in &pending {
            validate_legacy_metadata(item)?;
        }
        let changed = self.pending_legacy != pending;
        self.pending_legacy = pending;
        Ok(changed)
    }

    pub fn remove_pending_playlist(&mut self, name: &str) {
        self.pending_legacy
            .retain(|item| item.playlist.as_str() != name);
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &ContentMetadata)> {
        self.entries
            .iter()
            .map(|(hash, metadata)| (hash.as_str(), metadata))
    }

    pub fn get(&self, hash: &str) -> Option<&ContentMetadata> {
        self.entries.get(hash)
    }

    pub fn set_dynamic_playlist(&mut self, name: &str, dynamic: bool) -> Result<()> {
        validate_playlist_name(name)?;
        if dynamic {
            self.dynamic_playlists.insert(name.to_string());
        } else {
            self.dynamic_playlists.remove(name);
        }
        Ok(())
    }

    pub fn is_dynamic_playlist(&self, name: &str) -> bool {
        self.dynamic_playlists.contains(name)
    }

    pub fn dynamic_playlists(&self) -> impl Iterator<Item = &str> {
        self.dynamic_playlists.iter().map(String::as_str)
    }

    pub fn remove_dynamic_playlist(&mut self, name: &str) {
        self.dynamic_playlists.remove(name);
    }

    pub fn playlist_filters(&self, name: &str) -> Option<&TagFilters> {
        self.playlist_filters.get(name)
    }

    pub fn playlist_filter_names(&self) -> impl Iterator<Item = &str> {
        self.playlist_filters.keys().map(String::as_str)
    }

    pub fn set_playlist_filters(
        &mut self,
        name: &str,
        include: BTreeMap<String, Vec<String>>,
        exclude: BTreeMap<String, Vec<String>>,
    ) -> Result<()> {
        validate_playlist_name(name)?;
        let filters = TagFilters::new(include, exclude)?;
        if filters.include.is_empty() && filters.exclude.is_empty() {
            self.playlist_filters.remove(name);
        } else {
            self.playlist_filters.insert(name.to_string(), filters);
        }
        Ok(())
    }

    pub fn remove_playlist_filters(&mut self, name: &str) {
        self.playlist_filters.remove(name);
    }

    pub fn playlist_accepts(
        &self,
        name: &str,
        groups: Option<&BTreeMap<String, Vec<String>>>,
    ) -> bool {
        let Some(filters) = self.playlist_filters(name) else {
            return true;
        };
        filters.accepts(groups)
    }

    pub fn hash_for_alias(&self, alias: &str) -> Result<Option<&str>> {
        let mut matches = self
            .entries
            .iter()
            .filter(|(_, metadata)| metadata.aliases.iter().any(|known| known == alias))
            .map(|(hash, _)| hash.as_str());
        let first = matches.next();
        if first.is_some() && matches.next().is_some() {
            anyhow::bail!("path alias {alias:?} belongs to multiple content IDs");
        }
        Ok(first)
    }

    pub fn has_autotag_metadata(&self, hash: &str) -> bool {
        self.get(hash).is_some_and(|metadata| {
            metadata.autotag.is_some()
                || metadata.rating.is_some()
                || metadata.tag_groups.values().any(|tags| !tags.is_empty())
        })
    }

    /// Merge legacy path metadata without guessing across rating conflicts.
    pub fn merge_legacy(
        &mut self,
        hash: &str,
        aliases: &[String],
        groups: &BTreeMap<String, Vec<String>>,
        rating: Option<u8>,
        dimensions: (Option<u32>, Option<u32>),
    ) -> Result<bool> {
        validate_hash(hash)?;
        validate_rating(rating)?;
        let before = self.entries.get(hash).cloned();
        let metadata = self.entries.entry(hash.to_string()).or_default();
        merge_aliases(&mut metadata.aliases, aliases);
        for (kind, tags) in groups {
            merge_tags(metadata, kind, tags.clone())?;
        }
        if !metadata.rating_conflicted {
            match (metadata.rating, rating) {
                (None, Some(rating)) => metadata.rating = Some(rating),
                (Some(current), Some(rating)) if current != rating => {
                    metadata.rating = None;
                    metadata.rating_conflicted = true;
                }
                _ => {}
            }
        }
        if metadata.width.is_none() {
            metadata.width = dimensions.0;
        }
        if metadata.height.is_none() {
            metadata.height = dimensions.1;
        }
        Ok(before.as_ref() != Some(metadata))
    }

    pub fn set_tag_group(
        &mut self,
        hash: &str,
        aliases: &[String],
        kind: &str,
        tags: Vec<String>,
        dimensions: (Option<u32>, Option<u32>),
    ) -> Result<()> {
        validate_hash(hash)?;
        let kind = normalize_kind(kind)?;
        let tags = normalize_tags(tags);
        let metadata = self.entries.entry(hash.to_string()).or_default();
        merge_aliases(&mut metadata.aliases, aliases);
        if tags.is_empty() {
            metadata.tag_groups.remove(&kind);
        } else {
            metadata.tag_groups.insert(kind, tags);
        }
        update_dimensions(metadata, dimensions);
        Ok(())
    }

    pub fn set_rating(
        &mut self,
        hash: &str,
        aliases: &[String],
        rating: u8,
        dimensions: (Option<u32>, Option<u32>),
    ) -> Result<()> {
        validate_hash(hash)?;
        validate_rating(Some(rating))?;
        let metadata = self.entries.entry(hash.to_string()).or_default();
        merge_aliases(&mut metadata.aliases, aliases);
        metadata.rating = Some(rating);
        metadata.rating_conflicted = false;
        update_dimensions(metadata, dimensions);
        Ok(())
    }

    pub fn set_autotag(
        &mut self,
        hash: &str,
        aliases: &[String],
        mut provenance: AutoTagProvenance,
        dimensions: (Option<u32>, Option<u32>),
    ) -> Result<()> {
        validate_hash(hash)?;
        provenance.normalize()?;
        let metadata = self.entries.entry(hash.to_string()).or_default();
        merge_aliases(&mut metadata.aliases, aliases);
        metadata.autotag = Some(provenance);
        update_dimensions(metadata, dimensions);
        Ok(())
    }

    pub fn clear_metadata(&mut self, hash: &str) -> Result<()> {
        validate_hash(hash)?;
        if let Some(metadata) = self.entries.get_mut(hash) {
            metadata.tag_groups.clear();
            metadata.rating = None;
            metadata.rating_conflicted = false;
            metadata.autotag = None;
        }
        Ok(())
    }

    pub fn remember_aliases(
        &mut self,
        hash: &str,
        aliases: &[String],
        dimensions: (Option<u32>, Option<u32>),
    ) -> Result<bool> {
        validate_hash(hash)?;
        let before = self.entries.get(hash).cloned();
        let metadata = self.entries.entry(hash.to_string()).or_default();
        merge_aliases(&mut metadata.aliases, aliases);
        update_dimensions(metadata, dimensions);
        Ok(before.as_ref() != Some(metadata))
    }

    fn normalize(&mut self) -> Result<()> {
        for (hash, metadata) in &mut self.entries {
            validate_hash(hash)?;
            validate_rating(metadata.rating)?;
            metadata.aliases.retain(|alias| !alias.is_empty());
            dedupe(&mut metadata.aliases);
            let groups = std::mem::take(&mut metadata.tag_groups);
            for (kind, tags) in groups {
                let kind = normalize_kind(&kind)?;
                let tags = normalize_tags(tags);
                if !tags.is_empty() {
                    metadata.tag_groups.insert(kind, tags);
                }
            }
            match (metadata.width, metadata.height) {
                (None, None) => {}
                (Some(width), Some(height))
                    if width > 0
                        && height > 0
                        && u64::from(width) * u64::from(height)
                            <= crate::decode::MAX_IMAGE_PIXELS => {}
                _ => anyhow::bail!("content {hash} has invalid image dimensions"),
            }
            if metadata.rating_conflicted && metadata.rating.is_some() {
                anyhow::bail!("content {hash} has both a shared and conflicted rating");
            }
            if let Some(provenance) = &mut metadata.autotag {
                provenance.normalize()?;
            }
        }
        for name in &self.dynamic_playlists {
            validate_playlist_name(name)?;
        }
        let filters = std::mem::take(&mut self.playlist_filters);
        for (name, filters) in filters {
            validate_playlist_name(&name)?;
            let filters = TagFilters::new(filters.include, filters.exclude)?;
            if !filters.include.is_empty() || !filters.exclude.is_empty() {
                self.playlist_filters.insert(name, filters);
            }
        }
        for item in &self.pending_legacy {
            validate_legacy_metadata(item)?;
        }
        Ok(())
    }
}

fn validate_legacy_metadata(item: &LegacyMetadata) -> Result<()> {
    if item.playlist.trim().is_empty() || item.path.trim().is_empty() {
        anyhow::bail!("pending legacy metadata requires a playlist name and path");
    }
    Ok(())
}

fn validate_playlist_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        anyhow::bail!("playlist name must not be empty");
    }
    Ok(())
}

impl AutoTagProvenance {
    pub(crate) fn validate(&self) -> Result<()> {
        let mut normalized = self.clone();
        normalized.normalize()
    }

    fn normalize(&mut self) -> Result<()> {
        self.model = self.model.trim().to_string();
        if self.model.is_empty()
            || self.model.len() > 256
            || self.model.chars().any(char::is_control)
        {
            anyhow::bail!("autotag model must be 1 to 256 printable characters");
        }
        if self
            .confidence
            .is_some_and(|confidence| !confidence.is_finite() || !(0.0..=1.0).contains(&confidence))
        {
            anyhow::bail!("autotag confidence must be between 0 and 1");
        }
        normalize_optional_provenance_text(
            &mut self.endpoint,
            "endpoint",
            MAX_AUTOTAG_ENDPOINT_BYTES,
        )?;
        normalize_optional_provenance_text(
            &mut self.prompt_version,
            "prompt version",
            MAX_AUTOTAG_LABEL_BYTES,
        )?;
        normalize_optional_provenance_text(&mut self.run_id, "run ID", MAX_AUTOTAG_LABEL_BYTES)?;
        let raw_len = serde_json::to_vec(&self.raw)
            .context("serialize raw autotag provenance")?
            .len();
        if raw_len > MAX_AUTOTAG_RAW_BYTES {
            anyhow::bail!(
                "raw autotag provenance is {raw_len} bytes; maximum is {MAX_AUTOTAG_RAW_BYTES}"
            );
        }
        Ok(())
    }
}

fn normalize_optional_provenance_text(
    value: &mut Option<String>,
    label: &str,
    max_bytes: usize,
) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    *value = value.trim().to_string();
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        anyhow::bail!("autotag {label} must be 1 to {max_bytes} printable bytes");
    }
    Ok(())
}

fn validate_hash(hash: &str) -> Result<()> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!("content hash must be 64 lowercase hexadecimal characters");
    }
    Ok(())
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn validate_rating(rating: Option<u8>) -> Result<()> {
    if rating.is_some_and(|rating| rating > 5) {
        anyhow::bail!("content rating must be between 0 and 5");
    }
    Ok(())
}

fn normalize_kind(kind: &str) -> Result<String> {
    crate::playlist::canonical_tag_kind(kind)
        .context("content tag kind must not be empty or invalid")
}

fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut tags: Vec<String> = tags
        .into_iter()
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty())
        .collect();
    dedupe(&mut tags);
    tags
}

fn normalize_filter_groups(
    groups: BTreeMap<String, Vec<String>>,
) -> Result<BTreeMap<String, Vec<String>>> {
    let mut normalized = BTreeMap::new();
    for (kind, tags) in groups {
        let kind = normalize_kind(&kind)?;
        let tags = normalize_tags(tags);
        if !tags.is_empty() {
            normalized.entry(kind).or_insert_with(Vec::new).extend(tags);
        }
    }
    for tags in normalized.values_mut() {
        dedupe(tags);
    }
    Ok(normalized)
}

fn merge_tags(metadata: &mut ContentMetadata, kind: &str, tags: Vec<String>) -> Result<()> {
    let kind = normalize_kind(kind)?;
    let mut tags = normalize_tags(tags);
    tags.extend(metadata.tag_groups.remove(&kind).unwrap_or_default());
    dedupe(&mut tags);
    if !tags.is_empty() {
        metadata.tag_groups.insert(kind, tags);
    }
    Ok(())
}

fn merge_aliases(existing: &mut Vec<String>, aliases: &[String]) {
    existing.extend(aliases.iter().filter(|alias| !alias.is_empty()).cloned());
    dedupe(existing);
}

fn update_dimensions(metadata: &mut ContentMetadata, dimensions: (Option<u32>, Option<u32>)) {
    if dimensions.0.is_some() {
        metadata.width = dimensions.0;
    }
    if dimensions.1.is_some() {
        metadata.height = dimensions.1;
    }
}

fn dedupe(values: &mut Vec<String>) {
    let mut seen = HashSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: char) -> String {
        std::iter::repeat_n(byte, 64).collect()
    }

    #[test]
    fn content_roundtrips_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("content.json");
        let mut store = ContentStore::default();
        store
            .set_tag_group(
                &hash('a'),
                &["old.jpg".to_string()],
                "theme",
                vec![" night ".to_string(), "night".to_string()],
                (Some(3840), Some(2160)),
            )
            .unwrap();
        store
            .set_rating(&hash('a'), &["new.jpg".to_string()], 4, (None, None))
            .unwrap();
        store
            .set_autotag(
                &hash('a'),
                &[],
                AutoTagProvenance {
                    model: " vision-model ".to_string(),
                    confidence: Some(0.9),
                    tagged_at_ms: Some(1_700_000_000_000),
                    endpoint: Some(" https://example.test/v1/chat/completions ".to_string()),
                    prompt_version: Some(" aurora-autotag-v1 ".to_string()),
                    run_id: Some(" run-123 ".to_string()),
                    raw: serde_json::json!({"identity": {"theme": ["night"]}}),
                },
                (None, None),
            )
            .unwrap();
        store
            .set_playlist_filters(
                "focus",
                BTreeMap::from([("themes".to_string(), vec![" night ".to_string()])]),
                BTreeMap::new(),
            )
            .unwrap();
        store
            .replace_pending_legacy([("focus".to_string(), "offline.jpg".to_string())])
            .unwrap();
        store.set_dynamic_playlist("focus", true).unwrap();

        persist_content(&store, &path).unwrap();
        let loaded = load_content(&path).unwrap();

        let metadata = loaded.get(&hash('a')).unwrap();
        assert_eq!(metadata.aliases, ["old.jpg", "new.jpg"]);
        assert_eq!(metadata.tag_groups["theme"], ["night"]);
        assert_eq!(metadata.rating, Some(4));
        assert_eq!((metadata.width, metadata.height), (Some(3840), Some(2160)));
        assert_eq!(metadata.autotag.as_ref().unwrap().model, "vision-model");
        assert_eq!(metadata.autotag.as_ref().unwrap().confidence, Some(0.9));
        assert_eq!(
            metadata.autotag.as_ref().unwrap().tagged_at_ms,
            Some(1_700_000_000_000)
        );
        assert_eq!(
            metadata.autotag.as_ref().unwrap().endpoint.as_deref(),
            Some("https://example.test/v1/chat/completions")
        );
        assert_eq!(
            metadata.autotag.as_ref().unwrap().prompt_version.as_deref(),
            Some("aurora-autotag-v1")
        );
        assert_eq!(
            metadata.autotag.as_ref().unwrap().run_id.as_deref(),
            Some("run-123")
        );
        assert_eq!(
            loaded.playlist_filters("focus").unwrap().include["theme"],
            ["night"]
        );
        assert!(loaded.is_legacy_pending("focus", "offline.jpg"));
        assert!(loaded.is_dynamic_playlist("focus"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap()
                ["schema"],
            3
        );
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn older_schemas_load_and_upgrade_on_the_next_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("content.json");
        for schema in [1, 2] {
            std::fs::write(
                &path,
                format!(
                    r#"{{"schema":{schema},"entries":{{"{}":{{"aliases":["old.jpg"]}}}}}}"#,
                    hash('f')
                ),
            )
            .unwrap();

            let store = load_content(&path).unwrap();
            assert_eq!(store.get(&hash('f')).unwrap().aliases, ["old.jpg"]);

            persist_content(&store, &path).unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap())
                    .unwrap()["schema"],
                3
            );
        }
    }

    #[test]
    fn schema_three_does_not_repeat_legacy_reconciliation() {
        let schema_two =
            parse_content(br#"{"schema":2,"legacy_migrated":true,"entries":{}}"#).unwrap();
        let entries = BTreeMap::from([(
            hash('a'),
            serde_json::json!({
                "autotag": {
                    "model": "old-model",
                    "confidence": 0.5,
                    "raw": null
                }
            }),
        )]);
        let schema_three = parse_content(
            &serde_json::to_vec(&serde_json::json!({
                "schema": 3,
                "legacy_migrated": true,
                "entries": entries
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(schema_two.needs_legacy_reconciliation());
        assert!(!schema_three.needs_legacy_reconciliation());
        assert!(!schema_three.is_dynamic_playlist("focus"));
        let provenance = schema_three
            .get(&hash('a'))
            .unwrap()
            .autotag
            .as_ref()
            .unwrap();
        assert_eq!(provenance.tagged_at_ms, None);
        assert_eq!(provenance.endpoint, None);
        assert_eq!(provenance.prompt_version, None);
        assert_eq!(provenance.run_id, None);
    }

    #[test]
    fn invalid_autotag_provenance_does_not_mutate_content() {
        let mut store = ContentStore::default();
        let before = store.clone();

        assert!(store
            .set_autotag(
                &hash('a'),
                &[],
                AutoTagProvenance {
                    model: "model".to_string(),
                    confidence: Some(1.1),
                    raw: serde_json::Value::Null,
                    ..AutoTagProvenance::default()
                },
                (None, None),
            )
            .is_err());
        assert_eq!(store, before);
    }

    #[test]
    fn autotag_provenance_text_fields_are_bounded() {
        for provenance in [
            AutoTagProvenance {
                model: "model".to_string(),
                endpoint: Some("x".repeat(MAX_AUTOTAG_ENDPOINT_BYTES + 1)),
                ..AutoTagProvenance::default()
            },
            AutoTagProvenance {
                model: "model".to_string(),
                prompt_version: Some("x".repeat(MAX_AUTOTAG_LABEL_BYTES + 1)),
                ..AutoTagProvenance::default()
            },
            AutoTagProvenance {
                model: "model".to_string(),
                run_id: Some("run\nid".to_string()),
                ..AutoTagProvenance::default()
            },
        ] {
            assert!(provenance.validate().is_err());
        }
    }

    #[test]
    fn dynamic_playlists_and_content_iteration_are_ordered() {
        let mut store = ContentStore::default();
        store
            .remember_aliases(&hash('b'), &["b.jpg".to_string()], (None, None))
            .unwrap();
        store
            .remember_aliases(&hash('a'), &["a.jpg".to_string()], (None, None))
            .unwrap();

        store.set_dynamic_playlist("focus", true).unwrap();
        assert!(store.is_dynamic_playlist("focus"));
        assert_eq!(
            store
                .iter()
                .map(|(hash, _)| hash.to_string())
                .collect::<Vec<_>>(),
            [hash('a'), hash('b')]
        );

        store.set_dynamic_playlist("focus", false).unwrap();
        assert!(!store.is_dynamic_playlist("focus"));
        store.set_dynamic_playlist("focus", true).unwrap();
        store.remove_dynamic_playlist("focus");
        assert!(!store.is_dynamic_playlist("focus"));
        assert!(store.set_dynamic_playlist(" ", true).is_err());
    }

    #[test]
    fn legacy_merge_unions_tags_and_keeps_conflicting_ratings_local() {
        let mut store = ContentStore::default();
        store
            .merge_legacy(
                &hash('b'),
                &["one.jpg".to_string()],
                &BTreeMap::from([("theme".to_string(), vec!["night".to_string()])]),
                Some(2),
                (Some(10), Some(20)),
            )
            .unwrap();
        store
            .merge_legacy(
                &hash('b'),
                &["two.jpg".to_string()],
                &BTreeMap::from([(
                    "theme".to_string(),
                    vec!["night".to_string(), "city".to_string()],
                )]),
                Some(5),
                (Some(10), Some(20)),
            )
            .unwrap();

        let metadata = store.get(&hash('b')).unwrap();
        assert_eq!(metadata.aliases, ["one.jpg", "two.jpg"]);
        assert_eq!(metadata.tag_groups["theme"], ["night", "city"]);
        assert_eq!(metadata.rating, None);
        assert!(metadata.rating_conflicted);
    }

    #[test]
    fn clearing_a_global_group_does_not_remove_other_groups() {
        let mut store = ContentStore::default();
        let id = hash('c');
        store
            .set_tag_group(&id, &[], "theme", vec!["night".to_string()], (None, None))
            .unwrap();
        store
            .set_tag_group(&id, &[], "artist", vec!["aurora".to_string()], (None, None))
            .unwrap();

        store
            .set_tag_group(&id, &[], "theme", Vec::new(), (None, None))
            .unwrap();

        let metadata = store.get(&id).unwrap();
        assert!(!metadata.tag_groups.contains_key("theme"));
        assert_eq!(metadata.tag_groups["artist"], ["aurora"]);
    }

    #[test]
    fn load_rejects_unknown_schema_without_overwriting_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("content.json");
        std::fs::write(&path, r#"{"schema":99,"entries":{}}"#).unwrap();

        let error = load_content(&path).unwrap_err().to_string();

        assert!(error.contains("unsupported content metadata schema 99"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"schema":99,"entries":{}}"#
        );
    }

    #[test]
    fn playlist_filters_use_or_within_kinds_and_and_across_kinds() {
        let mut store = ContentStore::default();
        store
            .set_playlist_filters(
                "focus",
                BTreeMap::from([
                    (
                        "theme".to_string(),
                        vec!["night".to_string(), "city".to_string()],
                    ),
                    ("medium".to_string(), vec!["anime".to_string()]),
                ]),
                BTreeMap::from([("safety".to_string(), vec!["nsfw".to_string()])]),
            )
            .unwrap();

        let accepted = BTreeMap::from([
            ("theme".to_string(), vec!["city".to_string()]),
            ("medium".to_string(), vec!["anime".to_string()]),
            ("safety".to_string(), vec!["sfw".to_string()]),
        ]);
        let missing_kind = BTreeMap::from([("theme".to_string(), vec!["night".to_string()])]);
        let excluded = BTreeMap::from([
            ("theme".to_string(), vec!["night".to_string()]),
            ("medium".to_string(), vec!["anime".to_string()]),
            ("safety".to_string(), vec!["nsfw".to_string()]),
        ]);

        assert!(store.playlist_accepts("focus", Some(&accepted)));
        assert!(!store.playlist_accepts("focus", Some(&missing_kind)));
        assert!(!store.playlist_accepts("focus", Some(&excluded)));
        assert!(!store.playlist_accepts("focus", None));
        assert!(store.playlist_accepts("unfiltered", None));
    }

    #[test]
    fn empty_filter_update_clears_playlist_rules() {
        let mut store = ContentStore::default();
        store
            .set_playlist_filters(
                "focus",
                BTreeMap::from([("theme".to_string(), vec!["night".to_string()])]),
                BTreeMap::new(),
            )
            .unwrap();

        store
            .set_playlist_filters("focus", BTreeMap::new(), BTreeMap::new())
            .unwrap();

        assert!(store.playlist_filters("focus").is_none());
        assert!(store.playlist_accepts("focus", None));
    }

    #[test]
    fn ambiguous_historical_aliases_are_reported() {
        let mut store = ContentStore::default();
        for id in [hash('d'), hash('e')] {
            store
                .remember_aliases(&id, &["reused-name.jpg".to_string()], (Some(10), Some(10)))
                .unwrap();
        }

        let error = store
            .hash_for_alias("reused-name.jpg")
            .unwrap_err()
            .to_string();

        assert!(error.contains("multiple content IDs"));
    }
}
