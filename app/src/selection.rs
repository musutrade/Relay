//! Initial-submission role choices. Names and permissions remain host policy;
//! catalog metadata describes capabilities, never grants access or credentials.
use crate::catalog_cache::CatalogView;
use crate::host::{CommandResult, HostConfig, HostError, Job};
use crate::providers::{NativePermission, NativeProfile, ProviderKind, validate_selection_value};
use crate::workflow::WorkflowConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleSelections {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub developer: Option<RoleSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<RoleSelection>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleSelection {
    pub profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_permission: Option<NativePermission>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm_permission_expansion: Option<bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSelection {
    pub value: String,
    pub source: ModelSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<CatalogReference>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSource {
    Catalog,
    Manual,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogReference {
    pub cache_epoch: String,
    pub generation: u64,
}
fn invalid(message: impl Into<String>) -> HostError {
    HostError::Job(message.into())
}
pub(crate) fn role(job: &Job, reviewer: bool) -> Option<&RoleSelection> {
    job.role_selections.as_ref().and_then(|roles| {
        if reviewer {
            roles.reviewer.as_ref()
        } else {
            roles.developer.as_ref()
        }
    })
}
pub(crate) fn profile_name<'a>(
    job: &'a Job,
    config: &'a HostConfig,
    reviewer: bool,
) -> Option<&'a str> {
    role(job, reviewer)
        .map(|selection| selection.profile.as_str())
        .or_else(|| {
            if reviewer {
                job.workflow
                    .as_ref()
                    .and_then(|name| config.workflows.get(name))
                    .map(|w| w.reviewer.as_str())
            } else {
                Some(job.agent.as_str())
            }
        })
}
pub(crate) fn selectable(default: &str, configured: Option<&Vec<String>>) -> Vec<String> {
    let mut names = vec![default.to_owned()];
    if let Some(configured) = configured {
        names.extend(
            configured
                .iter()
                .filter(|name| name.as_str() != default)
                .cloned(),
        );
    }
    names
}
pub(crate) fn validate_job(job: &Job, config: &HostConfig) -> Result<(), HostError> {
    if let Some(roles) = &job.role_selections {
        if roles.developer.is_none() && roles.reviewer.is_none() {
            return Err(invalid("role_selections must select at least one role"));
        }
        if roles.reviewer.is_some() && job.workflow.is_none() {
            return Err(invalid("reviewer selection requires a configured workflow"));
        }
    }
    for reviewer in [false, true] {
        let Some(name) = profile_name(job, config, reviewer) else {
            continue;
        };
        let selection = role(job, reviewer);
        if !reviewer && name != job.agent {
            return Err(invalid(
                "job.agent must equal the selected developer profile",
            ));
        }
        if let Some(workflow) = job
            .workflow
            .as_ref()
            .and_then(|name| config.workflows.get(name))
        {
            let allowed = if reviewer {
                selectable(&workflow.reviewer, workflow.selectable_reviewers.as_ref())
            } else {
                selectable(&workflow.developer, workflow.selectable_developers.as_ref())
            };
            if !allowed.iter().any(|allowed| allowed == name) {
                return Err(invalid("role profile is not allowlisted for this workflow"));
            }
        }
        let native = config.native_agents.get(name);
        if native.is_none() && (!config.agents.contains_key(name) || reviewer) {
            return Err(invalid(
                "role profile is not allowlisted or cannot be a reviewer",
            ));
        }
        if let Some(selection) = selection {
            if selection.confirm_permission_expansion == Some(false) {
                return Err(invalid(
                    "permission expansion confirmation must be true when supplied",
                ));
            }
            if let Some(model) = &selection.model {
                if !validate_selection_value(&model.value) {
                    return Err(invalid(
                        "model must contain 1–256 UTF-8 bytes without controls or a leading '-'",
                    ));
                }
                match (&model.source, &model.catalog) {
                    (ModelSource::Manual, None) => (),
                    (ModelSource::Catalog, Some(reference))
                        if reference.cache_epoch.len() == 32
                            && reference.cache_epoch.bytes().all(|b| b.is_ascii_hexdigit())
                            && reference.generation > 0 => {}
                    _ => {
                        return Err(invalid(
                            "catalog model requires a bounded cache reference; manual model must omit it",
                        ));
                    }
                }
            }
            if let Some(effort) = &selection.effort {
                if !validate_selection_value(effort) {
                    return Err(invalid(
                        "effort must contain 1–256 UTF-8 bytes without controls or a leading '-'",
                    ));
                }
                if !selection
                    .model
                    .as_ref()
                    .is_some_and(|model| model.source == ModelSource::Catalog)
                {
                    return Err(invalid(
                        "effort override requires model-specific catalog metadata",
                    ));
                }
            }
            if native.is_none()
                && (selection.model.is_some()
                    || selection.effort.is_some()
                    || selection.native_permission.is_some()
                    || selection.confirm_permission_expansion.is_some())
            {
                return Err(invalid(
                    "generic profiles do not support native model, effort, or permission overrides",
                ));
            }
        }
        if let Some(profile) = native {
            if reviewer
                && (profile.provider != ProviderKind::ClaudeCli
                    || profile
                        .native_permission
                        .is_some_and(|mode| !mode.compatible(profile.provider, true)))
            {
                return Err(invalid(
                    "review_profile_unsupported: reviewer requires Relay's fixed restricted Claude contract",
                ));
            }
            let mode = selection
                .and_then(|s| s.native_permission)
                .or(profile.native_permission);
            if let Some(mode) = mode {
                if !mode.compatible(profile.provider, reviewer) {
                    return Err(invalid(
                        "native permission mode is incompatible with the adapter or fixed reviewer role",
                    ));
                }
                let safe = matches!(
                    mode,
                    NativePermission::CodexWorkspaceWrite | NativePermission::ClaudeRestricted
                );
                if !safe && !profile.allowed_permission_modes.contains(&mode) {
                    return Err(invalid(
                        "native permission mode is not allowed by host policy",
                    ));
                }
                if mode.requires_confirmation()
                    && (selection.and_then(|s| s.confirm_permission_expansion) != Some(true)
                        || selection.and_then(|s| s.native_permission) != Some(mode))
                {
                    return Err(invalid(
                        "native permission expansion requires explicit confirmation; Full access expands filesystem AND network access",
                    ));
                }
            }
            let resolved = native_profile(job, config, reviewer)?.expect("native profile exists");
            resolved.compile(reviewer).map_err(invalid)?;
        }
    }
    verify_binding(job, config)?;
    Ok(())
}
pub(crate) fn native_profile(
    job: &Job,
    config: &HostConfig,
    reviewer: bool,
) -> Result<Option<NativeProfile>, HostError> {
    let Some(name) = profile_name(job, config, reviewer) else {
        return Ok(None);
    };
    let Some(mut profile) = config.native_agents.get(name).cloned() else {
        return Ok(None);
    };
    if let Some(selection) = role(job, reviewer) {
        if let Some(model) = &selection.model {
            profile.model = Some(model.value.clone());
        }
        if let Some(effort) = &selection.effort {
            profile.effort = Some(effort.clone());
        }
        if let Some(mode) = selection.native_permission {
            profile.native_permission = Some(mode);
        }
    }
    Ok(Some(profile))
}
pub(crate) fn effective_workflow(
    job: &Job,
    config: &HostConfig,
    workflow: &WorkflowConfig,
) -> WorkflowConfig {
    let mut workflow = workflow.clone();
    if let Some(name) = profile_name(job, config, false) {
        workflow.developer = name.into();
    }
    if let Some(name) = profile_name(job, config, true) {
        workflow.reviewer = name.into();
    }
    workflow
}
pub(crate) fn validate_catalog(
    selection: &RoleSelection,
    view: &CatalogView,
) -> Result<(), String> {
    let Some(model) = selection
        .model
        .as_ref()
        .filter(|model| model.source == ModelSource::Catalog)
    else {
        return Ok(());
    };
    let reference = model
        .catalog
        .as_ref()
        .ok_or("catalog model has no cache reference")?;
    if view.name != selection.profile
        || view.stale
        || view.refreshing
        || view.cache_epoch != reference.cache_epoch
        || view.generation != reference.generation
    {
        return Err("model catalog changed or expired; refresh and select again, or explicitly use an unverified manual model".into());
    }
    let catalog = view
        .catalog
        .as_ref()
        .ok_or("model catalog is unavailable")?;
    if catalog.model_catalog.state != crate::capabilities::CapabilityState::Supported {
        return Err("model catalog is not verified available".into());
    }
    let model = catalog
        .models
        .iter()
        .find(|entry| entry.model == model.value)
        .ok_or("selected model is not in the current catalog")?;
    if let Some(effort) = &selection.effort {
        let supported = model
            .supported_efforts
            .as_ref()
            .ok_or("selected model has no effort metadata")?;
        if !supported.iter().any(|entry| entry.effort == *effort) {
            return Err("selected effort is not advertised for this model".into());
        }
    }
    Ok(())
}
pub(crate) fn annotate(
    command: &mut CommandResult,
    job: &Job,
    config: &HostConfig,
    reviewer: bool,
) {
    if let Some(evidence) = command.provider.as_mut().and_then(|p| p.selection.as_mut()) {
        evidence.requested.profile = profile_name(job, config, reviewer).map(str::to_owned);
        evidence.requested.model_source =
            role(job, reviewer).and_then(|s| s.model.as_ref()).map(|m| {
                match m.source {
                    ModelSource::Catalog => "catalog",
                    ModelSource::Manual => "manual",
                }
                .to_owned()
            });
        evidence.bound();
    }
}
pub(crate) fn permission_choices(profile: &NativeProfile) -> Vec<Value> {
    use NativePermission::*;
    let choices: &[NativePermission] = match profile.provider {
        ProviderKind::CodexCli => &[CodexWorkspaceWrite, CodexFullAccess],
        ProviderKind::CodexAppServer => &[CodexWorkspaceWrite, CodexFullAccess, CodexAutoReview],
        ProviderKind::ClaudeCli => &[
            ClaudeDontAsk,
            ClaudeAuto,
            ClaudeBypassPermissions,
            ClaudeRestricted,
        ],
    };
    choices.iter().map(|mode| {
        let safe = matches!(mode, CodexWorkspaceWrite | ClaudeRestricted);
        let allowed = safe || profile.allowed_permission_modes.contains(mode);
        let (label, reason) = match mode {
            CodexWorkspaceWrite => ("Workspace write", "Sandboxed workspace-write; approval never; runtime version/help checks still apply"),
            CodexAutoReview => ("Codex Auto-review", "Developer only: workspace-write + on-request + native auto_review; eligible sandbox escalations may be approved automatically. Not absolute read-only; availability and approval-model selection belong to Codex"),
            CodexFullAccess => ("Full access", "Expands filesystem AND network access; danger-full-access and approval never"),
            ClaudeDontAsk => ("Claude dontAsk", "Native policy decides allowed tools; unanswered permissions are denied"),
            ClaudeAuto => ("Claude auto", "Native classifier availability is unknown until execution; model, provider, account and managed policy may reject or fall back"),
            ClaudeBypassPermissions => ("Claude bypassPermissions", "Bypasses native permission checks; expands filesystem and network access available to the process"),
            ClaudeRestricted => ("Relay restricted reviewer", "Fixed read-only reviewer tools and isolation contract; not a native --permission-mode value"),
        };
        json!({"id":mode,"label":label,"host_allowed":allowed,"availability":if allowed {"unknown"} else {"unsupported"},
            "reason":if allowed {reason} else {"Not allowed by the configured host policy"},"reviewer_only":*mode == ClaudeRestricted,
            "requires_confirmation":mode.requires_confirmation(),"confirmation_text":if mode.requires_confirmation() {Some(if *mode == CodexAutoReview {"I confirm native automatic approval of eligible developer sandbox escalations; this is not an absolute read-only or no-network guarantee"} else if *mode == CodexFullAccess || *mode == ClaudeBypassPermissions {"I confirm expanded filesystem AND network access for this developer execution"} else {"I confirm this developer native permission-mode change; native policy still controls allowed access"})} else {None::<&str>}})
    }).collect()
}

/// Server-owned admission snapshot. Contains requested host defaults, never credentials
/// or a claim about actual provider execution. Its digest is drift detection only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleBinding {
    pub version: u8,
    pub policy_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_reference: Option<String>,
    pub developer: FrozenRole,
    pub reviewer: Option<FrozenRole>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenRole {
    pub profile: String,
    pub provider: Option<ProviderKind>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub native_permission: Option<NativePermission>,
}
fn frozen_role(
    job: &Job,
    config: &HostConfig,
    reviewer: bool,
) -> Result<Option<FrozenRole>, HostError> {
    let Some(name) = profile_name(job, config, reviewer) else {
        return Ok(None);
    };
    let native = native_profile(job, config, reviewer)?;
    Ok(Some(FrozenRole {
        profile: name.into(),
        provider: native.as_ref().map(|p| p.provider),
        model: native.as_ref().and_then(|p| p.model.clone()),
        effort: native.as_ref().and_then(|p| p.effort.clone()),
        native_permission: native.as_ref().and_then(|p| p.native_permission),
    }))
}
pub(crate) fn secure_fingerprint(value: &impl Serialize) -> Result<String, String> {
    use blake2::{Blake2s256, Digest};
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    let digest = Blake2s256::digest(bytes);
    Ok(format!(
        "blake2s256-v1-{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}
pub(crate) fn acceptance_reference(challenge: &str) -> Result<String, String> {
    secure_fingerprint(&json!({"permission_challenge":challenge}))
}
fn program_stamp(program: &std::path::Path) -> Value {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(program) {
        Ok(metadata) => {
            json!({"canonical":program.canonicalize().ok(),"device":metadata.dev(),"inode":metadata.ino(),"length":metadata.len(),"modified":metadata.mtime(),"modified_nanos":metadata.mtime_nsec(),"mode":metadata.mode()})
        }
        Err(_) => Value::Null,
    }
}
fn profile_policy(name: &str, config: &HostConfig) -> Value {
    if let Some(profile) = config.native_agents.get(name) {
        json!({"native":profile,"executable":program_stamp(&profile.program)})
    } else if let Some(profile) = config.agents.get(name) {
        json!({"generic":profile,"executable":program_stamp(&profile.program)})
    } else {
        Value::Null
    }
}
pub(crate) fn binding(job: &Job, config: &HostConfig) -> Result<RoleBinding, HostError> {
    let workflow = job
        .workflow
        .as_ref()
        .and_then(|name| config.workflows.get(name));
    let developer =
        profile_name(job, config, false).ok_or_else(|| invalid("developer profile missing"))?;
    let reviewer = profile_name(job, config, true);
    // Include the original selected profiles too: a changed inherited default must
    // not be hidden by resolving a later clone. Values remain in memory; only the
    // bounded drift fingerprint is persisted, never environment/credential values.
    let policy = json!({"repository":config.repositories.get(&job.repository),"workflow":workflow,
        "developer":profile_policy(developer,config),"reviewer":reviewer.map(|name|profile_policy(name,config)),
        "test":job.test.as_ref().or(workflow.map(|w|&w.test)).and_then(|name|config.tests.get(name)),
        "publisher":job.draft_pr_adapter.as_ref().and_then(|name|config.draft_pr_adapters.get(name))});
    Ok(RoleBinding {
        version: 1,
        policy_fingerprint: secure_fingerprint(&policy).map_err(invalid)?,
        acceptance_reference: None,
        developer: frozen_role(job, config, false)?.expect("developer exists"),
        reviewer: frozen_role(job, config, true)?,
    })
}
pub(crate) fn verify_binding(job: &Job, config: &HostConfig) -> Result<(), HostError> {
    if let Some(accepted) = &job.role_binding {
        let mut acknowledged = accepted.clone();
        acknowledged.acceptance_reference = None;
        if job.role_selections.is_none() || acknowledged != binding(job, config)? {
            return Err(invalid(
                "selected role profile or host policy changed after acceptance; submit a newly reviewed selection instead of silently changing this task",
            ));
        }
    }
    Ok(())
}
pub(crate) fn needs_confirmation(job: &Job, config: &HostConfig) -> bool {
    [false, true].into_iter().any(|reviewer| {
        native_profile(job, config, reviewer)
            .ok()
            .flatten()
            .and_then(|p| p.native_permission)
            .is_some_and(NativePermission::requires_confirmation)
    })
}
fn challenge_scope(job: &Job) -> Result<String, String> {
    let mut roles = job.role_selections.clone();
    if let Some(roles) = &mut roles {
        for role in [&mut roles.developer, &mut roles.reviewer]
            .into_iter()
            .flatten()
        {
            role.confirm_permission_expansion = None;
        }
    }
    serde_json::to_string(&json!({"repository":job.repository,"workflow":job.workflow,"agent":job.agent,"roles":roles})).map_err(|e|e.to_string())
}
fn replacement_scope(job: &Job, predecessor: i64, reviewer: bool) -> Result<String, String> {
    secure_fingerprint(
        &json!({"predecessor_task_id":predecessor,"action":if reviewer {"continue_review"} else {"retry"},"role":if reviewer {"reviewer"} else {"developer"},"selection":challenge_scope(job)?}),
    )
}
const CHALLENGE_TTL: std::time::Duration = std::time::Duration::from_secs(300);
#[derive(Default)]
pub(crate) struct PermissionChallenges {
    entries: std::collections::BTreeMap<String, PermissionChallenge>,
}
struct PermissionChallenge {
    scope: String,
    policy: RoleBinding,
    expires: std::time::Instant,
}
impl PermissionChallenges {
    pub(crate) fn issue(&mut self, job: &Job, config: &HostConfig) -> Result<Value, String> {
        use rand_core::{OsRng, RngCore};
        self.entries
            .retain(|_, entry| entry.expires > std::time::Instant::now());
        if self.entries.len() >= 64 {
            return Err("too many pending permission challenges; wait for expiration".into());
        }
        if !needs_confirmation(job, config) {
            return Err(
                "this role selection does not require a permission expansion challenge".into(),
            );
        }
        let policy = binding(job, config).map_err(|e| e.to_string())?;
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        let challenge: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let scope = json!({"repository":job.repository,"workflow":job.workflow,"developer":policy.developer,"reviewer":policy.reviewer});
        let role = &policy.developer;
        let mode = serde_json::to_value(role.native_permission).map_err(|e| e.to_string())?;
        let consequence = if matches!(
            role.native_permission,
            Some(NativePermission::CodexFullAccess | NativePermission::ClaudeBypassPermissions)
        ) {
            "This expands filesystem AND network access for this developer execution."
        } else if role.native_permission == Some(NativePermission::CodexAutoReview) {
            "Codex native Auto-review may approve eligible filesystem, network and tool escalations beyond workspace-write. This is not absolute read-only or no-network access. Codex chooses the approval reviewer independently of the developer/code-review model; native account and managed policy still apply."
        } else {
            "This changes the developer's native access decisions. Native policy and model/provider eligibility still apply; Claude auto is a classifier, not bypassPermissions."
        };
        let confirmation_text = format!(
            "Confirm developer profile {}, model {}, effort {}, native mode {}. {}",
            role.profile,
            role.model
                .as_deref()
                .unwrap_or("native default (unverified)"),
            role.effort
                .as_deref()
                .unwrap_or("native default (unverified)"),
            mode.as_str().unwrap_or("native default"),
            consequence
        );
        self.entries.insert(
            challenge.clone(),
            PermissionChallenge {
                scope: challenge_scope(job)?,
                policy,
                expires: std::time::Instant::now() + CHALLENGE_TTL,
            },
        );
        let expires_at_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .saturating_add(CHALLENGE_TTL.as_millis())
            .min(u128::from(u64::MAX)) as u64;
        Ok(
            json!({"challenge":challenge,"expires_at_unix_ms":expires_at_unix_ms,"confirmation_text":confirmation_text,"scope":scope}),
        )
    }
    pub(crate) fn validate(
        &self,
        token: Option<&str>,
        job: &Job,
        config: &HostConfig,
    ) -> Result<(), String> {
        let token = token
            .filter(|token| token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or("native permission expansion requires a current server-issued challenge")?;
        let entry = self.entries.get(token).ok_or(
            "permission challenge is missing, consumed, or expired; review the current scope again",
        )?;
        if entry.expires <= std::time::Instant::now()
            || entry.scope != challenge_scope(job)?
            || entry.policy != binding(job, config).map_err(|e| e.to_string())?
        {
            return Err("permission challenge no longer matches this role selection or host policy; review the current scope again".into());
        }
        Ok(())
    }
    pub(crate) fn issue_replacement(
        &mut self,
        job: &Job,
        config: &HostConfig,
        predecessor: i64,
        reviewer: bool,
    ) -> Result<Value, String> {
        let mut response = self.issue(job, config)?;
        let token = response["challenge"]
            .as_str()
            .expect("issued challenge")
            .to_owned();
        self.entries
            .get_mut(&token)
            .expect("issued challenge")
            .scope = replacement_scope(job, predecessor, reviewer)?;
        response["scope"]["predecessor_task_id"] = json!(predecessor);
        response["scope"]["action"] = json!(if reviewer { "continue_review" } else { "retry" });
        response["scope"]["role"] = json!(if reviewer { "reviewer" } else { "developer" });
        Ok(response)
    }
    pub(crate) fn validate_replacement(
        &self,
        token: Option<&str>,
        job: &Job,
        config: &HostConfig,
        predecessor: i64,
        reviewer: bool,
    ) -> Result<(), String> {
        let token = token
            .filter(|t| t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or("replacement permission expansion requires a current server-issued challenge")?;
        let entry = self
            .entries
            .get(token)
            .ok_or("replacement permission challenge is missing, consumed, or expired")?;
        if entry.expires <= std::time::Instant::now()
            || entry.scope != replacement_scope(job, predecessor, reviewer)?
            || entry.policy != binding(job, config).map_err(|e| e.to_string())?
        {
            return Err("replacement permission challenge differs from predecessor, action, role selection, or composed host policy".into());
        }
        Ok(())
    }
    pub(crate) fn consume(&mut self, token: &str) {
        self.entries.remove(token);
    }
}

#[cfg(test)]
mod challenge_tests {
    use super::*;
    #[test]
    fn challenge_expiry_policy_drift_and_scope_changes_are_independent_gates() {
        let root = tempfile::tempdir().unwrap();
        let config:HostConfig=serde_json::from_value(json!({"workspace_root":root.path(),"repositories":{"repo":root.path()},"native_agents":{"dev":{"provider":"codex_cli","program":"/bin/true","allowed_permission_modes":["codex_full_access"]}}})).unwrap();
        let job:Job=serde_json::from_value(json!({"repository":"repo","requirements":"Test","agent":"dev","role_selections":{"developer":{"profile":"dev","native_permission":"codex_full_access","confirm_permission_expansion":true}}})).unwrap();
        let mut cache = PermissionChallenges::default();
        let response = cache.issue(&job, &config).unwrap();
        let token = response["challenge"].as_str().unwrap();
        cache.validate(Some(token), &job, &config).unwrap();
        let mut changed = config.clone();
        changed
            .native_agents
            .get_mut("dev")
            .unwrap()
            .env
            .insert("POLICY".into(), "changed".into());
        assert!(
            cache
                .validate(Some(token), &job, &changed)
                .unwrap_err()
                .contains("no longer matches")
        );
        let mut changed_job = job.clone();
        changed_job
            .role_selections
            .as_mut()
            .unwrap()
            .developer
            .as_mut()
            .unwrap()
            .effort = Some("different".into());
        assert!(cache.validate(Some(token), &changed_job, &config).is_err());
        cache.entries.get_mut(token).unwrap().expires =
            std::time::Instant::now() - std::time::Duration::from_secs(1);
        assert!(cache.validate(Some(token), &job, &config).is_err());
        let response = cache.issue_replacement(&job, &config, 1, false).unwrap();
        let token = response["challenge"].as_str().unwrap();
        cache
            .validate_replacement(Some(token), &job, &config, 1, false)
            .unwrap();
        assert!(cache.validate(Some(token), &job, &config).is_err());
        assert!(
            cache
                .validate_replacement(Some(token), &job, &config, 2, false)
                .is_err()
        );
        assert!(
            cache
                .validate_replacement(Some(token), &job, &config, 1, true)
                .is_err()
        );
        cache.entries.get_mut(token).unwrap().expires =
            std::time::Instant::now() - std::time::Duration::from_secs(1);
        assert!(
            cache
                .validate_replacement(Some(token), &job, &config, 1, false)
                .is_err()
        );
    }
}
