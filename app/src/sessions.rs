//! Small host-owned session bindings, outside the opaque queue and agent checkout.
//! They are not credentials or permission grants. Unknown/in-flight sessions fail
//! closed after restart; the host must establish process and side-effect safety.
use crate::providers::{NativeProfile, ProviderKind};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const LIMIT: u64 = 4096;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    role: String,
    cwd: PathBuf,
    profile_binding: String,
    session_id: Option<String>,
    ready: bool,
    #[serde(default)]
    attempt: u64,
}
pub(crate) struct Session {
    path: PathBuf,
    record: Record,
    pub resume: Option<String>,
    pub fresh_claude: Option<String>,
}
impl Session {
    pub fn begin(
        workspace: &Path,
        cwd: &Path,
        profile: &NativeProfile,
        read_only: bool,
        attempt: u64,
    ) -> io::Result<Self> {
        let role = if read_only { "reviewer" } else { "developer" };
        let root = workspace.join("sessions");
        match fs::DirBuilder::new().mode(0o700).create(&root) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        if !fs::symlink_metadata(&root)?.is_dir() || root.canonicalize()? != root {
            return Err(io::Error::other("session directory was redirected"));
        }
        let path = root.join(format!("{role}.json"));
        let cwd = cwd.canonicalize()?;
        let binding = binding(profile)?;
        let existing = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(LIMIT + 1).read_to_end(&mut bytes)?;
                if bytes.len() as u64 > LIMIT {
                    return Err(io::Error::other("session record exceeds its bound"));
                }
                Some(serde_json::from_slice::<Record>(&bytes).map_err(io::Error::other)?)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let resume = if let Some(record) = existing {
            if record.version != 1
                || record.role != role
                || record.cwd != cwd
                || record.profile_binding != binding
            {
                return Err(io::Error::other(
                    "session binding changed; do not resume across task, role, workspace, or profile",
                ));
            }
            if !record.ready && record.attempt >= attempt {
                return Err(io::Error::other(
                    "prior session turn is not confirmed complete; inspect before recovery",
                ));
            }
            let id = record
                .session_id
                .ok_or_else(|| io::Error::other("completed session has no ID"))?;
            validate_id(&id)?;
            Some(id)
        } else {
            None
        };
        let fresh_claude = if resume.is_none() && profile.provider == ProviderKind::ClaudeCli {
            Some(uuid())
        } else {
            None
        };
        let record = Record {
            version: 1,
            role: role.into(),
            cwd,
            profile_binding: binding,
            session_id: resume.clone().or(fresh_claude.clone()),
            ready: false,
            attempt,
        };
        let session = Self {
            path,
            record,
            resume,
            fresh_claude,
        };
        session.save()?; // Persist in-flight before a process can perform external effects.
        Ok(session)
    }
    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            path: self.path.clone(),
            record: self.record.clone(),
        }
    }
    pub fn complete(mut self, id: Option<&str>) -> io::Result<()> {
        let id =
            id.ok_or_else(|| io::Error::other("provider did not return a resumable session ID"))?;
        validate_id(id)?;
        if self
            .record
            .session_id
            .as_deref()
            .is_some_and(|expected| expected != id)
        {
            return Err(io::Error::other("provider returned a different session ID"));
        }
        self.record.session_id = Some(id.into());
        self.record.ready = true;
        self.save()
    }
    fn save(&self) -> io::Result<()> {
        atomic_write(&self.path, &self.record)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Checkpoint {
    path: PathBuf,
    record: Record,
}
impl Checkpoint {
    pub fn started(&self, id: &str) -> io::Result<()> {
        validate_id(id)?;
        let mut record = self.record.clone();
        if record
            .session_id
            .as_deref()
            .is_some_and(|expected| expected != id)
        {
            return Err(io::Error::other("session checkpoint changed thread ID"));
        }
        record.session_id = Some(id.into());
        atomic_write(&self.path, &record)
    }
}
pub(crate) fn enabled(profile: &NativeProfile) -> bool {
    profile.provider == ProviderKind::CodexAppServer || profile.session_continuity
}
fn binding(profile: &NativeProfile) -> io::Result<String> {
    let mut profile = profile.clone();
    profile.env.retain(|key, _| !key.starts_with("RELAY_"));
    fingerprint(&profile)
}
pub(crate) fn fingerprint(value: &impl Serialize) -> io::Result<String> {
    // Configuration drift detection, not an authentication boundary. Never persist env secrets.
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    let hash = bytes.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    Ok(format!("fnv1a-v1-{hash:016x}"))
}
pub(crate) fn atomic_write(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", uuid()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)?;
    let result = (|| {
        serde_json::to_writer(&mut file, value).map_err(io::Error::other)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        std::fs::File::open(path.parent().expect("metadata directory"))?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn validate_id(id: &str) -> io::Result<()> {
    if id.is_empty()
        || id.len() > 256
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
    {
        Err(io::Error::other("invalid bounded session ID"))
    } else {
        Ok(())
    }
}
fn uuid() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn sessions_reject_inflight_role_profile_and_cwd_mixups() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cwd = tmp.path().join("repository");
        fs::create_dir(&cwd).unwrap();
        let profile: NativeProfile =
            serde_json::from_value(json!({"provider":"codex_app_server","program":"/bin/true"}))
                .unwrap();
        let session = Session::begin(tmp.path(), &cwd, &profile, false, 1).unwrap();
        assert!(Session::begin(tmp.path(), &cwd, &profile, false, 1).is_err());
        session.complete(Some("thread-1")).unwrap();
        let mut changed = profile.clone();
        changed.model = Some("different".into());
        assert!(Session::begin(tmp.path(), &cwd, &changed, false, 1).is_err());
        let other = tmp.path().join("other");
        fs::create_dir(&other).unwrap();
        assert!(Session::begin(tmp.path(), &other, &profile, false, 1).is_err());
        fs::copy(
            tmp.path().join("sessions/developer.json"),
            tmp.path().join("sessions/reviewer.json"),
        )
        .unwrap();
        assert!(Session::begin(tmp.path(), &cwd, &profile, true, 1).is_err());
        let resumed = Session::begin(tmp.path(), &cwd, &profile, false, 1).unwrap();
        assert_eq!(resumed.resume.as_deref(), Some("thread-1"));
        assert!(resumed.complete(Some("wrong-thread")).is_err());
    }
}
