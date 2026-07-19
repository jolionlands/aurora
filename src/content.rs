use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

const CONTENT_SCHEMA_VERSION: u32 = 1;
const CONTENT_FILENAME: &str = "content.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TagFilters {
    /// Every kind must match at least one listed tag.
    pub include: BTreeMap<String, Vec<String>>,
    /// Any matching tag rejects the image.
    pub exclude: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContentStore {
    entries: BTreeMap<String, ContentMetadata>,
    legacy_migrated: bool,
    playlist_filters: BTreeMap<String, TagFilters>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredContent {
    schema: u32,
    #[serde(default)]
    legacy_migrated: bool,
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
    let stored: StoredContent = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse content metadata {}", path.display()))?;
    if stored.schema != CONTENT_SCHEMA_VERSION {
        anyhow::bail!(
            "unsupported content metadata schema {} in {}; expected {}",
            stored.schema,
            path.display(),
            CONTENT_SCHEMA_VERSION
        );
    }
    let mut store = ContentStore {
        entries: stored.entries,
        legacy_migrated: stored.legacy_migrated,
        playlist_filters: stored.playlist_filters,
    };
    store.normalize()?;
    Ok(store)
}

pub fn persist_content(store: &ContentStore, path: &Path) -> Result<()> {
    let mut normalized = store.clone();
    normalized.normalize()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create content metadata directory {}", parent.display()))?;
    }
    let stored = StoredContent {
        schema: CONTENT_SCHEMA_VERSION,
        legacy_migrated: normalized.legacy_migrated,
        playlist_filters: normalized.playlist_filters,
        entries: normalized.entries,
    };
    let bytes = serde_json::to_vec_pretty(&stored).context("serialize content metadata")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)
        .with_context(|| format!("write content metadata temp file {}", tmp.display()))?;
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

    pub fn get(&self, hash: &str) -> Option<&ContentMetadata> {
        self.entries.get(hash)
    }

    pub fn playlist_filters(&self, name: &str) -> Option<&TagFilters> {
        self.playlist_filters.get(name)
    }

    pub fn set_playlist_filters(
        &mut self,
        name: &str,
        include: BTreeMap<String, Vec<String>>,
        exclude: BTreeMap<String, Vec<String>>,
    ) -> Result<()> {
        if name.trim().is_empty() {
            anyhow::bail!("playlist name must not be empty");
        }
        let filters = TagFilters {
            include: normalize_filter_groups(include)?,
            exclude: normalize_filter_groups(exclude)?,
        };
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
        let matches = |kind: &str, wanted: &[String]| {
            groups
                .and_then(|groups| groups.get(kind))
                .is_some_and(|actual| wanted.iter().any(|tag| actual.contains(tag)))
        };
        filters
            .include
            .iter()
            .all(|(kind, wanted)| matches(kind, wanted))
            && !filters
                .exclude
                .iter()
                .any(|(kind, unwanted)| matches(kind, unwanted))
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
            metadata.rating.is_some() || metadata.tag_groups.values().any(|tags| !tags.is_empty())
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

    pub fn clear_metadata(&mut self, hash: &str) -> Result<()> {
        validate_hash(hash)?;
        if let Some(metadata) = self.entries.get_mut(hash) {
            metadata.tag_groups.clear();
            metadata.rating = None;
            metadata.rating_conflicted = false;
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
        }
        let filters = std::mem::take(&mut self.playlist_filters);
        for (name, filters) in filters {
            if name.trim().is_empty() {
                anyhow::bail!("content metadata has filters for a blank playlist name");
            }
            let include = normalize_filter_groups(filters.include)?;
            let exclude = normalize_filter_groups(filters.exclude)?;
            if !include.is_empty() || !exclude.is_empty() {
                self.playlist_filters
                    .insert(name, TagFilters { include, exclude });
            }
        }
        Ok(())
    }
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
            .set_playlist_filters(
                "focus",
                BTreeMap::from([("themes".to_string(), vec![" night ".to_string()])]),
                BTreeMap::new(),
            )
            .unwrap();

        persist_content(&store, &path).unwrap();
        let loaded = load_content(&path).unwrap();

        let metadata = loaded.get(&hash('a')).unwrap();
        assert_eq!(metadata.aliases, ["old.jpg", "new.jpg"]);
        assert_eq!(metadata.tag_groups["theme"], ["night"]);
        assert_eq!(metadata.rating, Some(4));
        assert_eq!((metadata.width, metadata.height), (Some(3840), Some(2160)));
        assert_eq!(
            loaded.playlist_filters("focus").unwrap().include["theme"],
            ["night"]
        );
        assert!(!path.with_extension("json.tmp").exists());
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
