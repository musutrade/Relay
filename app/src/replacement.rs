//! Explicit stopped-stage replacement. Authority stays in host records, never prose.
use crate::{
    host::{HostConfig, HostError, Job, Outcome, RunResult},
    selection::{self, RoleSelection, RoleSelections},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{io, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Developer,
    Reviewer,
}
impl Role {
    pub(crate) fn reviewer(self) -> bool {
        self == Self::Reviewer
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoppedStage {
    pub role: Role,
    pub round: u8,
    pub base_sha: Option<String>,
    pub candidate_sha: Option<String>,
    pub max_repairs: u8,
    pub remaining_repairs: u8,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repair_feedback: String,
    pub feedback_complete: bool,
}
impl StoppedStage {
    pub(crate) fn developer(
        round: u8,
        base: Option<&str>,
        candidate: Option<&str>,
        max_repairs: u8,
    ) -> Self {
        Self {
            role: Role::Developer,
            round,
            base_sha: base.map(str::to_owned),
            candidate_sha: candidate.map(str::to_owned),
            max_repairs,
            remaining_repairs: max_repairs.saturating_sub(round),
            repair_feedback: String::new(),
            feedback_complete: true,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replacement {
    pub role: Role,
    pub stopped_stage: StoppedStage,
    pub predecessor_job_digest: String,
    pub predecessor_result_digest: String,
    pub predecessor_owner: String,
    pub session_epoch: String,
    /// Bounded diagnostic data, not an instruction or authority source.
    pub handoff: String,
}
pub(crate) fn current(job: &Job) -> Option<&Replacement> {
    job.continuation.as_ref()?.replacement.as_ref()
}
pub(crate) fn changed(job: &Job, reviewer: bool) -> bool {
    current(job).is_some_and(|p| p.role.reviewer() == reviewer)
}
/// Bound the escaped representation, not only the original UTF-8 string.
pub(crate) fn feedback_fits(feedback: &str) -> bool {
    feedback.len() <= 6144 && serde_json::to_vec(feedback).is_ok_and(|bytes| bytes.len() <= 6146)
}
pub(crate) fn requires_stage_retry(job: &Job) -> bool {
    job.workflow.is_some()
        && job
            .role_epochs
            .as_ref()
            .and_then(|epochs| epochs.developer.as_ref())
            .is_some()
}
pub(crate) fn stage(
    result: &RunResult,
    job: &Job,
    config: &HostConfig,
    reviewer: bool,
) -> Result<StoppedStage, String> {
    let proof = result
        .stopped_stage
        .clone()
        .ok_or("replacement unsupported: legacy result has no host-authored stopped-stage proof")?;
    if proof.role.reviewer() != reviewer
        || !matches!(
            result.outcome,
            Outcome::Failure | Outcome::TimedOut | Outcome::Cancelled
        )
    {
        return Err("replacement is only supported for this role's proven stopped stage".into());
    }
    if !proof.feedback_complete || !feedback_fits(&proof.repair_feedback) {
        return Err("replacement unsupported: exact prior repair feedback exceeds the bounded handoff; inspect retained stage input".into());
    }
    let max = job
        .workflow
        .as_ref()
        .map_or(0, |name| config.workflows[name].max_repairs);
    if proof.max_repairs != max || proof.round > max || proof.remaining_repairs != max - proof.round
    {
        return Err("stopped-stage repair budget proof is inconsistent".into());
    }
    if job.workflow.is_some() {
        if proof.base_sha.as_ref().is_none_or(|s| !valid_sha(s))
            || proof.candidate_sha.as_ref().is_none_or(|s| !valid_sha(s))
        {
            return Err("stopped-stage candidate proof is missing".into());
        }
        if reviewer {
            let review = crate::workflow::review_continuation(result, None)?;
            if Some(&review.base_sha) != proof.base_sha.as_ref()
                || Some(&review.candidate_sha) != proof.candidate_sha.as_ref()
                || review.round != proof.round
            {
                return Err("stopped reviewer stage disagrees with candidate evidence".into());
            }
        }
    } else if reviewer
        || proof.round != 0
        || proof.base_sha.is_some()
        || proof.candidate_sha.is_some()
    {
        return Err("stopped-stage workflow identity is inconsistent".into());
    }
    Ok(proof)
}
fn valid_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn effective_identity(
    profile: &crate::providers::NativeProfile,
    reviewer: bool,
) -> Result<Value, serde_json::Error> {
    let mut profile = profile.clone();
    profile.allowed_permission_modes.clear();
    profile.session_continuity = crate::sessions::enabled(&profile);
    if profile.native_permission.is_none() {
        profile.native_permission = match (profile.provider, reviewer) {
            (
                crate::providers::ProviderKind::CodexCli
                | crate::providers::ProviderKind::CodexAppServer,
                false,
            ) => Some(crate::providers::NativePermission::CodexWorkspaceWrite),
            (crate::providers::ProviderKind::ClaudeCli, true) => {
                Some(crate::providers::NativePermission::ClaudeRestricted)
            }
            _ => None,
        };
    }
    serde_json::to_value(profile)
}
pub(crate) fn compose(
    previous: &Job,
    replacement: &RoleSelection,
    reviewer: bool,
    config: &HostConfig,
) -> Result<Job, HostError> {
    let mut job = previous.clone();
    job.role_binding = None;
    let roles = job.role_selections.get_or_insert(RoleSelections {
        developer: None,
        reviewer: None,
    });
    if reviewer {
        roles.reviewer = Some(replacement.clone());
    } else {
        job.agent = replacement.profile.clone();
        roles.developer = Some(replacement.clone());
    }
    job.validate(config)?;
    let old_native = selection::native_profile(previous, config, reviewer)?;
    let new_native = selection::native_profile(&job, config, reviewer)?;
    let unchanged = match (&old_native, &new_native) {
        (Some(old), Some(new)) => {
            effective_identity(old, reviewer)? == effective_identity(new, reviewer)?
        }
        (None, None) => {
            config
                .agents
                .get(&previous.agent)
                .map(serde_json::to_value)
                .transpose()?
                == config
                    .agents
                    .get(&job.agent)
                    .map(serde_json::to_value)
                    .transpose()?
        }
        _ => false,
    };
    if unchanged {
        return Err(HostError::Job("replacement must change effective execution settings; provenance-only changes cannot reset a session".into()));
    }
    if reviewer
        && old_native.as_ref().is_some_and(crate::sessions::enabled)
            != new_native.as_ref().is_some_and(crate::sessions::enabled)
    {
        return Err(HostError::Job("reviewer_topology_change_unsupported: replacement must preserve the existing reviewer checkout topology; no checkout is reset or copied".into()));
    }
    job.role_binding = Some(selection::binding(&job, config)?);
    Ok(job)
}
pub(crate) fn normalized(job: &Job) -> Job {
    let mut job = job.clone();
    job.continuation = None;
    job
}
pub(crate) fn digest(value: &impl Serialize) -> io::Result<String> {
    selection::secure_fingerprint(value).map_err(io::Error::other)
}
pub(crate) fn freeze(
    previous: &Job,
    successor: &mut Job,
    result: &RunResult,
    owner: &str,
    reviewer: bool,
    config: &HostConfig,
) -> io::Result<()> {
    let stopped_stage = stage(result, previous, config, reviewer).map_err(io::Error::other)?;
    let epoch = crate::sessions::new_epoch();
    let epochs = successor.role_epochs.get_or_insert_with(Default::default);
    if reviewer {
        epochs.reviewer = Some(epoch.clone());
    } else {
        epochs.developer = Some(epoch.clone());
    }
    let handoff = handoff(result, &stopped_stage)?;
    successor
        .continuation
        .as_mut()
        .ok_or_else(|| io::Error::other("replacement requires a continuation"))?
        .replacement = Some(Replacement {
        role: stopped_stage.role,
        stopped_stage,
        predecessor_job_digest: digest(&normalized(previous))?,
        predecessor_result_digest: digest(
            &serde_json::from_str::<Value>(&result.to_json()).map_err(io::Error::other)?,
        )?,
        predecessor_owner: owner.into(),
        session_epoch: epoch,
        handoff,
    });
    Ok(())
}
pub(crate) fn verify_transition(
    previous: &Job,
    successor: &Job,
    result: &RunResult,
    owner: &str,
    config: &HostConfig,
) -> io::Result<()> {
    let proof = current(successor).ok_or_else(|| io::Error::other("replacement proof missing"))?;
    if proof.predecessor_owner != owner
        || proof.predecessor_job_digest != digest(&normalized(previous))?
        || proof.predecessor_result_digest
            != digest(&serde_json::from_str::<Value>(&result.to_json()).map_err(io::Error::other)?)?
        || proof.stopped_stage
            != stage(result, previous, config, proof.role.reviewer()).map_err(io::Error::other)?
        || proof.handoff.len() > 8192
        || proof.handoff != handoff(result, &proof.stopped_stage)?
    {
        return Err(io::Error::other(
            "replacement predecessor or stopped-stage proof changed",
        ));
    }
    previous.validate(config).map_err(io::Error::other)?;
    let selected = selection::role(successor, proof.role.reviewer())
        .ok_or_else(|| io::Error::other("replacement selection missing"))?;
    let mut expected =
        compose(previous, selected, proof.role.reviewer(), config).map_err(io::Error::other)?;
    expected.role_binding = successor.role_binding.clone(); // Current binding is independently verified below.
    selection::verify_binding(successor, config).map_err(io::Error::other)?;
    expected.workspace_quota_bytes = successor.workspace_quota_bytes;
    let epochs = expected.role_epochs.get_or_insert_with(Default::default);
    if proof.role.reviewer() {
        epochs.reviewer = Some(proof.session_epoch.clone());
    } else {
        epochs.developer = Some(proof.session_epoch.clone());
    }
    epochs.validate()?;
    if digest(&normalized(&expected))? != digest(&normalized(successor))? {
        return Err(io::Error::other(
            "replacement changed unrelated job or role settings",
        ));
    }
    Ok(())
}
fn handoff(result: &RunResult, stage: &StoppedStage) -> io::Result<String> {
    // No provider/session IDs, transcripts, repository contents, or full logs.
    let value = json!({"untrusted_diagnostic_data":true,"stage":{"role":stage.role,"round":stage.round,"base_sha":stage.base_sha,"candidate_sha":stage.candidate_sha,"max_repairs":stage.max_repairs,"remaining_repairs":stage.remaining_repairs},"retained_files":"Existing checkout, including dirty and untracked files, is retained in place. Inspect it before continuing.","failure":result.failure.as_ref().map(|f|json!({"code":f.code,"stage":f.stage})),"developer_outcome":result.agent.as_ref().map(|c|c.outcome),"tests":result.tests.as_ref().map(|c|json!({"outcome":c.outcome,"exit_code":c.exit_code})),"verdict":result.workflow.as_ref().and_then(|w|w.rounds.last()).and_then(|r|r.review.as_ref()).map(|r|r.verdict)});
    let text = serde_json::to_string(&value).map_err(io::Error::other)?;
    if text.len() > 8192 {
        return Err(io::Error::other(
            "replacement handoff exceeds 8192 UTF-8 bytes",
        ));
    }
    Ok(text)
}
pub(crate) fn verify_stage_checkpoint(proof: &StoppedStage, path: &Path) -> io::Result<()> {
    if let (Some(base), Some(candidate)) = (&proof.base_sha, &proof.candidate_sha) {
        let actual_base: String = serde_json::from_str(&crate::workspaces::read_marker(
            &path.join("workflow-base.txt"),
        )?)
        .map_err(io::Error::other)?;
        let actual_candidate: String = serde_json::from_str(&crate::workspaces::read_marker(
            &path.join("candidate-head.json"),
        )?)
        .map_err(io::Error::other)?;
        if &actual_base != base || &actual_candidate != candidate {
            return Err(io::Error::other(
                "stopped-stage base or candidate checkpoint changed",
            ));
        }
    }
    Ok(())
}

pub(crate) fn capability(
    job: &Job,
    result: &RunResult,
    config: &HostConfig,
    path: &Path,
    reviewer: bool,
) -> Value {
    let role = if reviewer { "reviewer" } else { "developer" };
    let proof = stage(result, job, config, reviewer).and_then(|proof| {
        verify_stage_checkpoint(&proof, path).map_err(|e| e.to_string())?;
        if reviewer {
            crate::workspaces::verify_review_candidate_checkpoint(config, job, result, path)
                .map_err(|e| e.to_string())?;
        }
        Ok(proof)
    });
    let mut profiles = if let Some(workflow) = job
        .workflow
        .as_ref()
        .and_then(|name| config.workflows.get(name))
    {
        if reviewer {
            selection::selectable(&workflow.reviewer, workflow.selectable_reviewers.as_ref())
        } else {
            selection::selectable(&workflow.developer, workflow.selectable_developers.as_ref())
        }
    } else {
        config
            .agents
            .keys()
            .chain(config.native_agents.keys())
            .cloned()
            .collect()
    };
    if reviewer {
        let isolated = selection::native_profile(job, config, true)
            .ok()
            .flatten()
            .as_ref()
            .is_some_and(crate::sessions::enabled);
        profiles.retain(|name| {
            config.native_agents.get(name).is_some_and(|p| {
                p.provider == crate::providers::ProviderKind::ClaudeCli
                    && crate::sessions::enabled(p) == isolated
                    && p.native_permission
                        .is_none_or(|m| m.compatible(p.provider, true))
            })
        });
    }
    match proof {
        Ok(proof) => {
            json!({"allowed":!profiles.is_empty(),"role":role,"reason":if profiles.is_empty(){Some("no host-approved compatible replacement profile")}else{None},"profiles":profiles,"stopped_stage":proof})
        }
        Err(reason) => {
            json!({"allowed":false,"role":role,"reason":reason,"profiles":[],"stopped_stage":null})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escaped_feedback_budget_and_result_shrinking_preserve_exact_proof() {
        assert!(feedback_fits(&"界".repeat(2048)));
        assert!(!feedback_fits(&"界".repeat(2049)));
        assert!(feedback_fits(&"\n".repeat(3000)));
        assert!(!feedback_fits(&"\u{1}".repeat(1025)));
        let mut result = RunResult::new(Outcome::Failure, None);
        let mut stage = StoppedStage::developer(0, None, None, 0);
        stage.repair_feedback = "界".repeat(2048);
        result.stopped_stage = Some(stage.clone());
        let mut command = crate::host::CommandResult::error(Outcome::Failure, "bounded failure");
        command.stdout = "large diagnostic output".repeat(10000);
        result.agent = Some(command);
        let serialized = result.to_json();
        assert!(serialized.len() <= relay::MAX_RESULT_BYTES);
        let restored: RunResult = serde_json::from_str(&serialized).unwrap();
        assert_eq!(restored.stopped_stage, Some(stage.clone()));
        let handoff = handoff(&restored, &stage).unwrap();
        assert!(handoff.len() < 8192);
        assert!(!handoff.contains("界"));
    }
    #[test]
    fn replacement_chain_proof_size_is_bounded_by_immediate_predecessor() {
        let config:HostConfig=serde_json::from_value(json!({"workspace_root":"/fixture/runs","repositories":{"repo":"/fixture/source"},"native_agents":{"dev":{"provider":"codex_cli","program":"/bin/true"}}})).unwrap();
        let mut job: Job = serde_json::from_value(
            json!({"repository":"repo","requirements":"Keep work","agent":"dev"}),
        )
        .unwrap();
        let mut result = RunResult::new(Outcome::Failure, None);
        result.stopped_stage = Some(StoppedStage::developer(0, None, None, 0));
        let mut lengths = Vec::new();
        for index in 1..101 {
            let choice:RoleSelection=serde_json::from_value(json!({"profile":"dev","model":{"value":format!("model-{index:03}"),"source":"manual"}})).unwrap();
            let mut next = compose(&job, &choice, false, &config).unwrap();
            next.continuation = Some(crate::workspaces::Continuation {
                developer_stage: None,
                replacement: None,
                quota_increase: None,
                workspace_task_id: 1,
                predecessor_task_id: index,
                predecessor_generation: 1,
                review_only: None,
            });
            freeze(&job, &mut next, &result, "host", false, &config).unwrap();
            verify_transition(&normalized(&job), &next, &result, "host", &config).unwrap();
            lengths.push(serde_json::to_vec(&next).unwrap().len());
            job = next;
        }
        assert!(lengths.iter().max().unwrap() - lengths.iter().min().unwrap() < 16);
        assert!(*lengths.iter().max().unwrap() < 4096);
    }
}
