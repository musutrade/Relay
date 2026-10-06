//! In-memory configured-profile catalogs. Reads never launch processes.
use crate::{
    capabilities::{ProfileCatalog, ProfileStamp, TaskModelObservation, profile_stamp},
    providers::NativeProfile,
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
const CATALOG_TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Serialize)]
pub struct CatalogView {
    pub name: String,
    pub cache_epoch: String,
    pub generation: u64,
    pub stale: bool,
    pub refreshing: bool,
    pub catalog: Option<ProfileCatalog>,
    pub task_observation: Option<TaskModelObservation>,
    pub task_observation_stale: bool,
}
pub(crate) struct CatalogCache {
    epoch: String,
    entries: BTreeMap<String, Entry>,
    cleanup_blocked: bool,
}
impl Default for CatalogCache {
    fn default() -> Self {
        use rand_core::{OsRng, RngCore};
        let mut bytes = [0u8; 16];
        OsRng.fill_bytes(&mut bytes);
        Self {
            epoch: bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
            entries: BTreeMap::new(),
            cleanup_blocked: false,
        }
    }
}
struct Entry {
    stamp: ProfileStamp,
    generation: u64,
    checked: Option<Instant>,
    refreshing: bool,
    catalog: Option<ProfileCatalog>,
    observation: Option<(TaskModelObservation, Instant)>,
}
impl CatalogCache {
    pub(crate) fn reconciled_guard(&mut self, guard_present: bool) {
        if !guard_present {
            self.cleanup_blocked = false;
        }
    }
    fn entry(&mut self, name: &str, profile: &NativeProfile) -> &mut Entry {
        let stamp = profile_stamp(profile);
        let entry = self.entries.entry(name.into()).or_insert_with(|| Entry {
            stamp: stamp.clone(),
            generation: 0,
            checked: None,
            refreshing: false,
            catalog: None,
            observation: None,
        });
        if entry.stamp != stamp {
            entry.stamp = stamp;
            entry.generation += 1;
            entry.checked = None;
            entry.catalog = None;
            entry.observation = None;
            // The old probe keeps its slot until completion, but cannot populate
            // this newly invalidated generation.
        }
        entry
    }
    pub(crate) fn view(&mut self, name: &str, profile: &NativeProfile) -> CatalogView {
        let cache_epoch = self.epoch.clone();
        let entry = self.entry(name, profile);
        CatalogView {
            cache_epoch,
            name: name.into(),
            generation: entry.generation,
            stale: entry
                .checked
                .is_none_or(|time| time.elapsed() >= CATALOG_TTL),
            refreshing: entry.refreshing,
            catalog: entry.catalog.clone(),
            task_observation: entry.observation.as_ref().map(|(value, _)| value.clone()),
            task_observation_stale: entry
                .observation
                .as_ref()
                .is_none_or(|(_, time)| time.elapsed() >= CATALOG_TTL),
        }
    }
    pub(crate) fn observe(
        &mut self,
        name: &str,
        profile: &NativeProfile,
        observation: TaskModelObservation,
        stamp: ProfileStamp,
    ) {
        let entry = self.entry(name, profile);
        if entry.stamp == stamp {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64;
            // A long task does not make its initialization snapshot fresh again.
            // Future clock values fail stale instead of extending evidence life.
            let age = now
                .checked_sub(observation.checked_at_unix_ms)
                .map(Duration::from_millis)
                .unwrap_or(CATALOG_TTL)
                .min(CATALOG_TTL);
            entry.observation = Some((observation, Instant::now() - age));
            // Do not disturb an explicit refresh's generation or a selectable
            // standalone catalog. Task observations have independent freshness.
        }
    }
    /// One probe tree for the service. Repeated same-profile requests don't queue.
    pub(crate) fn begin(
        &mut self,
        name: &str,
        profile: &NativeProfile,
    ) -> Result<Option<u64>, &'static str> {
        self.entry(name, profile);
        if self.cleanup_blocked {
            return Err(
                "catalog process cleanup is unknown; trusted host inspection is required before further discovery",
            );
        }
        if self.entries.get(name).is_some_and(|entry| entry.refreshing) {
            return Ok(None);
        }
        if self.entries.values().any(|entry| entry.refreshing) {
            return Err("another catalog refresh is in progress; retry when complete");
        }
        let entry = self.entries.get_mut(name).expect("inserted entry");
        entry.generation += 1;
        entry.refreshing = true;
        Ok(Some(entry.generation))
    }
    pub(crate) fn finish(
        &mut self,
        name: &str,
        profile: &NativeProfile,
        generation: u64,
        catalog: ProfileCatalog,
        cleanup_confirmed: bool,
    ) -> CatalogView {
        self.cleanup_blocked |= !cleanup_confirmed;
        let entry = self.entry(name, profile);
        entry.refreshing = false;
        if entry.generation == generation {
            entry.catalog = Some(catalog);
            entry.checked = Some(Instant::now());
        }
        self.view(name, profile)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn profile(path: &std::path::Path) -> NativeProfile {
        serde_json::from_value(json!({"provider":"codex_app_server","program":path})).unwrap()
    }
    #[test]
    fn reads_never_probe_and_binary_or_profile_changes_invalidate_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake");
        std::fs::write(&path, "initial").unwrap();
        let mut profile = profile(&path);
        let mut cache = CatalogCache::default();
        let initial = cache.view("native", &profile);
        assert_eq!(initial.generation, 0);
        assert!(initial.stale && initial.catalog.is_none());
        assert_eq!(cache.view("native", &profile).generation, 0);
        std::fs::write(path, "new executable version").unwrap();
        assert_eq!(cache.view("native", &profile).generation, 1);
        profile.model = Some("operator-model".into());
        assert_eq!(cache.view("native", &profile).generation, 2);
        profile
            .env
            .insert("PRIVATE_CREDENTIAL".into(), "never-return-this".into());
        let view = cache.view("native", &profile);
        assert_eq!(view.generation, 3);
        assert!(
            !serde_json::to_string(&view)
                .unwrap()
                .contains("never-return-this")
        );
    }
    #[test]
    fn duplicate_and_cross_profile_refreshes_share_one_slot() {
        let profile = profile(std::path::Path::new("/nonexistent-fixture"));
        let mut cache = CatalogCache::default();
        assert_eq!(cache.begin("first", &profile), Ok(Some(1)));
        assert_eq!(cache.begin("first", &profile), Ok(None));
        assert!(cache.begin("second", &profile).is_err());
        assert!(cache.view("first", &profile).refreshing);
    }
    fn catalog() -> ProfileCatalog {
        let capability = json!({"state":"unknown","reason":"fixture","source":"fixture"});
        serde_json::from_value(json!({
            "provider":"codex_app_server","checked_at_unix_ms":1,"cli_version":null,
            "executable":capability,"compatibility":capability,"authentication":capability,
            "reviewer_isolation":capability,"permission_control":capability,
            "session_continuity":capability,"process_cleanup":capability,"model_catalog":capability,
            "startup_context":capability,
            "models":[],"selection":{"requested_model":null,"requested_effort":null,
                "effective_model":null,"effective_effort":null,"status":capability}
        }))
        .unwrap()
    }
    #[test]
    fn completion_is_fenced_and_expiry_does_not_refresh() {
        let mut profile = profile(std::path::Path::new("/missing-fixture"));
        let mut cache = CatalogCache::default();
        let generation = cache.begin("native", &profile).unwrap().unwrap();
        profile.model = Some("changed-model".into());
        let view = cache.finish("native", &profile, generation, catalog(), true);
        assert!(view.catalog.is_none() && view.stale && !view.refreshing);
        let generation = cache.begin("native", &profile).unwrap().unwrap();
        assert!(
            !cache
                .finish("native", &profile, generation, catalog(), true)
                .stale
        );
        cache.entries.get_mut("native").unwrap().checked = Some(Instant::now() - CATALOG_TTL);
        let expired = cache.view("native", &profile);
        assert!(expired.stale && expired.catalog.is_some() && !expired.refreshing);
        assert_eq!(expired.generation, generation);
    }
    #[test]
    fn unknown_cleanup_blocks_every_profile() {
        let profile = profile(std::path::Path::new("/missing-fixture"));
        let mut cache = CatalogCache::default();
        let generation = cache.begin("native", &profile).unwrap().unwrap();
        cache.finish("native", &profile, generation, catalog(), false);
        assert!(cache.begin("native", &profile).is_err());
        assert!(cache.begin("other", &profile).is_err());
        cache.reconciled_guard(true);
        assert!(cache.begin("native", &profile).is_err());
        cache.reconciled_guard(false);
        assert!(cache.begin("native", &profile).is_ok());
    }

    #[test]
    fn task_observation_is_separate_expires_and_cannot_survive_context_drift() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake");
        std::fs::write(&path, "initial").unwrap();
        let mut profile = profile(&path);
        let mut cache = CatalogCache::default();
        let observation = TaskModelObservation {
            task_id: 1,
            repository: "repo".into(),
            role: "reviewer".into(),
            cli_version: Some("2.1.291".into()),
            checked_at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            requested_model: None,
            requested_effort: None,
            native_permission: None,
            models: vec![],
        };
        let stamp = profile_stamp(&profile);
        cache.observe("native", &profile, observation.clone(), stamp.clone());
        let view = cache.view("native", &profile);
        assert!(view.catalog.is_none() && view.stale && !view.task_observation_stale);
        assert_eq!(view.task_observation.unwrap().task_id, 1);
        cache
            .entries
            .get_mut("native")
            .unwrap()
            .observation
            .as_mut()
            .unwrap()
            .1 = Instant::now() - CATALOG_TTL;
        assert!(cache.view("native", &profile).task_observation_stale);
        let mut old = observation.clone();
        old.checked_at_unix_ms = old.checked_at_unix_ms.saturating_sub(300_001);
        cache.observe("native", &profile, old, stamp.clone());
        assert!(cache.view("native", &profile).task_observation_stale);
        let mut future = observation.clone();
        future.checked_at_unix_ms = u64::MAX;
        cache.observe("native", &profile, future, stamp.clone());
        assert!(cache.view("native", &profile).task_observation_stale);
        profile.model = Some("changed-profile".into());
        cache.observe("native", &profile, observation.clone(), stamp);
        assert!(cache.view("native", &profile).task_observation.is_none());
        cache.observe("native", &profile, observation, profile_stamp(&profile));
        std::fs::write(&path, "changed executable").unwrap();
        assert!(cache.view("native", &profile).task_observation.is_none());
    }

    #[test]
    fn service_instances_have_distinct_epochs_and_reset_generations() {
        let profile = profile(std::path::Path::new("/missing-fixture"));
        let mut first = CatalogCache::default();
        let mut second = CatalogCache::default();
        first.begin("native", &profile).unwrap();
        let old = first.view("native", &profile);
        let new = second.view("native", &profile);
        assert_ne!(old.cache_epoch, new.cache_epoch);
        assert_eq!(new.generation, 0);
        assert_eq!(new.cache_epoch.len(), 32);
    }
}
