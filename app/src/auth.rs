//! Single-operator browser authentication. No credentials enter the durable core.
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::http::{HeaderMap, Method, Uri, header};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

const TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_SESSIONS: usize = 32;
const COOKIE: &str = "__Host-relay_session";
const DEV_COOKIE: &str = "relay_dev_session";
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Bearer,
    Session,
    Hybrid,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub mode: Mode,
    pub credentials_file: PathBuf,
    pub public_origin: String,
    #[serde(default)]
    pub allow_insecure_loopback: bool,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    username: String,
    password_hash: String,
}
struct Session {
    expires: Instant,
    revision: String,
}
struct State {
    sessions: HashMap<String, Session>,
    attempts: u32,
    window: Instant,
}
pub struct Auth {
    mode: Mode,
    token: Option<String>,
    config: Option<Config>,
    state: Mutex<State>,
}
impl Auth {
    pub fn bearer(token: String) -> Result<Self> {
        Self::new(None, Some(token))
    }
    pub fn new(config: Option<Config>, token: Option<String>) -> Result<Self> {
        let mode = config.as_ref().map_or(Mode::Bearer, |c| c.mode);
        if matches!(mode, Mode::Bearer | Mode::Hybrid) {
            let t = token
                .as_deref()
                .ok_or("RELAY_TOKEN is required for bearer/hybrid mode")?;
            if !(32..=256).contains(&t.len()) || !t.bytes().all(|b| b.is_ascii_graphic()) {
                return Err("RELAY_TOKEN must be 32–256 non-whitespace ASCII bytes".into());
            }
        } else if token.is_some() {
            return Err(
                "remove RELAY_TOKEN for session-only mode; use hybrid during migration".into(),
            );
        }
        if let Some(c) = &config {
            if c.mode == Mode::Bearer {
                return Err("omit RELAY_AUTH_CONFIG for legacy bearer mode".into());
            }
            validate_origin(c)?;
            read_credentials(&c.credentials_file)?;
        }
        Ok(Self {
            mode,
            token,
            config,
            state: Mutex::new(State {
                sessions: HashMap::new(),
                attempts: 0,
                window: Instant::now(),
            }),
        })
    }
    pub fn load(path: &Path, token: Option<String>) -> Result<Self> {
        let bytes = read_bounded(path, false)?;
        Self::new(Some(serde_json::from_slice(&bytes)?), token)
    }
    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub fn origin_allowed(&self, headers: &HeaderMap) -> bool {
        self.config.as_ref().is_some_and(|c| {
            headers.get(header::ORIGIN).and_then(|h| h.to_str().ok())
                == Some(c.public_origin.as_str())
        })
    }
    pub fn bearer_valid(&self, headers: &HeaderMap) -> bool {
        self.token.as_ref().is_some_and(|expected| {
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.strip_prefix("Bearer "))
                .is_some_and(|s| equal(s, expected))
        })
    }
    fn cookie_name(&self) -> &'static str {
        if self
            .config
            .as_ref()
            .is_some_and(|c| c.allow_insecure_loopback)
        {
            DEV_COOKIE
        } else {
            COOKIE
        }
    }
    fn cookie<'a>(&self, headers: &'a HeaderMap) -> Option<&'a str> {
        let mut found = None;
        for value in headers.get_all(header::COOKIE) {
            for pair in value.to_str().ok()?.split(';') {
                if let Some((name, value)) = pair.trim().split_once('=')
                    && name == self.cookie_name()
                {
                    if found.is_some() {
                        return None;
                    }
                    found = Some(value);
                }
            }
        }
        found.filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
    }
    pub fn session_valid(&self, headers: &HeaderMap) -> bool {
        let Some(c) = &self.config else {
            return false;
        };
        let Ok(credentials) = read_credentials(&c.credentials_file) else {
            return false;
        };
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        let now = Instant::now();
        state
            .sessions
            .retain(|_, s| s.expires > now && s.revision == credentials.password_hash);
        self.cookie(headers)
            .is_some_and(|key| state.sessions.contains_key(key))
    }
    pub fn authorized(&self, headers: &HeaderMap, method: &Method) -> bool {
        // An explicit invalid bearer must never fall back to ambient cookies.
        if headers.contains_key(header::AUTHORIZATION) {
            return self.bearer_valid(headers);
        }
        self.session_valid(headers)
            && (matches!(*method, Method::GET | Method::HEAD) || self.origin_allowed(headers))
    }
    /// Global limiter intentionally ignores spoofable proxy/IP headers. At most
    /// five password verifications/minute and one concurrent hash per process.
    pub fn login(
        &self,
        username: &str,
        password: &str,
        headers: &HeaderMap,
    ) -> std::result::Result<String, u16> {
        if !self.origin_allowed(headers) {
            return Err(403);
        }
        let c = self.config.as_ref().ok_or(401u16)?;
        let mut state = self.state.try_lock().map_err(|_| 429u16)?;
        let now = Instant::now();
        if now.duration_since(state.window) >= Duration::from_secs(60) {
            state.window = now;
            state.attempts = 0;
        }
        if state.attempts >= 5 {
            return Err(429);
        }
        state.attempts += 1;
        if username.len() > 128 || password.len() > 1024 {
            return Err(401);
        }
        let credentials = read_credentials(&c.credentials_file).map_err(|_| 401u16)?;
        let hash = PasswordHash::new(&credentials.password_hash).map_err(|_| 401u16)?;
        let valid = Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok();
        if !valid || !equal(username, &credentials.username) {
            return Err(401);
        }
        state
            .sessions
            .retain(|_, s| s.expires > now && s.revision == credentials.password_hash);
        if let Some(old) = self.cookie(headers) {
            state.sessions.remove(old);
        }
        if state.sessions.len() >= MAX_SESSIONS
            && let Some(key) = state
                .sessions
                .iter()
                .min_by_key(|(_, s)| s.expires)
                .map(|(k, _)| k.clone())
        {
            state.sessions.remove(&key);
        }
        let mut random = [0u8; 32];
        OsRng.fill_bytes(&mut random);
        let key: String = random.iter().map(|b| format!("{b:02x}")).collect();
        state.sessions.insert(
            key.clone(),
            Session {
                expires: now + TTL,
                revision: credentials.password_hash,
            },
        );
        Ok(self.set_cookie(&key, TTL.as_secs()))
    }
    pub fn logout(&self, headers: &HeaderMap) -> std::result::Result<String, u16> {
        if !self.origin_allowed(headers) {
            return Err(403);
        }
        let mut state = self.state.lock().map_err(|_| 503u16)?;
        if let Some(key) = self.cookie(headers) {
            state.sessions.remove(key);
        }
        Ok(self.set_cookie("", 0))
    }
    fn set_cookie(&self, key: &str, age: u64) -> String {
        format!(
            "{}={key}; Path=/; HttpOnly; SameSite=Strict; Max-Age={age}{}",
            self.cookie_name(),
            if self.cookie_name() == COOKIE {
                "; Secure"
            } else {
                ""
            }
        )
    }
}
fn equal(a: &str, b: &str) -> bool {
    let mut mismatch = a.len() ^ b.len();
    for (i, byte) in b.bytes().enumerate() {
        mismatch |= (byte ^ a.as_bytes().get(i).copied().unwrap_or(0)) as usize;
    }
    mismatch == 0
}
fn validate_origin(c: &Config) -> Result<()> {
    let uri: Uri = c.public_origin.parse()?;
    if uri.authority().is_none()
        || uri.authority().is_some_and(|a| a.as_str().contains('@'))
        || uri.path_and_query().is_some_and(|p| p.as_str() != "/")
        || c.public_origin.ends_with('/')
    {
        return Err(
            "public_origin must be an exact origin without a trailing slash or path".into(),
        );
    }
    if c.allow_insecure_loopback {
        if uri.scheme_str() != Some("http")
            || !matches!(uri.host(), Some("127.0.0.1" | "[::1]" | "localhost"))
        {
            return Err("insecure cookies require an explicit http loopback origin".into());
        }
    } else if uri.scheme_str() != Some("https") {
        return Err("session authentication requires an https public_origin".into());
    }
    Ok(())
}
fn read_bounded(path: &Path, private: bool) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.is_file()
        || (private && file.metadata()?.permissions().mode() & 0o077 != 0)
    {
        return Err("credential file must be a private regular file (chmod 600)".into());
    }
    let mut bytes = Vec::new();
    file.take(8193).read_to_end(&mut bytes)?;
    if bytes.len() > 8192 {
        return Err("authentication file too large".into());
    }
    Ok(bytes)
}
fn read_credentials(path: &Path) -> Result<Credentials> {
    let c: Credentials = serde_json::from_slice(&read_bounded(path, true)?)?;
    let hash = PasswordHash::new(&c.password_hash).map_err(|_| "invalid password hash")?;
    if c.username.is_empty()
        || c.username.len() > 128
        || hash.algorithm.as_str() != "argon2id"
        || hash.version != Some(19)
        || hash.params.get_decimal("m") != Some(19456)
        || hash.params.get_decimal("t") != Some(2)
        || hash.params.get_decimal("p") != Some(1)
    {
        return Err("invalid operator credentials or unsupported Argon2id parameters".into());
    }
    Ok(c)
}
/// Operator-only local terminal flow. Passwords never enter argv, env, or stdout.
pub fn initialize(path: &Path, username: Option<&str>) -> Result<()> {
    let rotating = username.is_none();
    let name = match username {
        Some(name) => name.to_owned(),
        None => read_credentials(path)?.username,
    };
    if name.trim().is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        return Err("username must be 1–128 bytes without control characters".into());
    }
    if !rotating && path.exists() {
        return Err("credential file exists; use auth-password to rotate".into());
    }
    let password = Zeroizing::new(rpassword::prompt_password(
        "New Relay password (at least 15 characters): ",
    )?);
    let confirm = Zeroizing::new(rpassword::prompt_password("Confirm password: ")?);
    if password.chars().count() < 15 || password.len() > 1024 || !equal(&password, &confirm) {
        return Err("passwords must match and contain 15+ characters, at most 1024 bytes".into());
    }
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|_| "password hashing failed")?
        .to_string();
    let bytes = serde_json::to_vec(&Credentials {
        username: name,
        password_hash: hash,
    })?;
    let temporary = path.with_extension(format!("new-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let outcome = (|| -> Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        if rotating {
            fs::rename(&temporary, path)?;
        } else {
            fs::hard_link(&temporary, path)?;
            fs::remove_file(&temporary)?;
        }
        Ok(())
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    outcome?;
    println!("{{\"credentials_saved\":true,\"existing_sessions_revoked\":{rotating}}}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    fn fixture(mode: Mode) -> (TempDir, Auth, HeaderMap) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("credentials.json");
        credentials(&path, "fixture-password-only");
        let auth = Auth::new(
            Some(Config {
                mode,
                credentials_file: path,
                public_origin: "https://relay.example".into(),
                allow_insecure_loopback: false,
            }),
            (mode == Mode::Hybrid).then(|| "test-token-00000000000000000000000".into()),
        )
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "https://relay.example".parse().unwrap());
        (dir, auth, headers)
    }
    fn credentials(path: &Path, password: &str) {
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
            .unwrap()
            .to_string();
        fs::write(
            path,
            serde_json::to_vec(&Credentials {
                username: "operator".into(),
                password_hash: hash,
            })
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn login(auth: &Auth, headers: &mut HeaderMap) -> String {
        let cookie = auth
            .login("operator", "fixture-password-only", headers)
            .unwrap();
        headers.insert(
            header::COOKIE,
            cookie.split(';').next().unwrap().parse().unwrap(),
        );
        cookie
    }
    #[test]
    fn session_security_expiry_rotation_logout_and_bounds() {
        let (dir, auth, mut h) = fixture(Mode::Session);
        assert!(!auth.authorized(&h, &Method::GET));
        assert_eq!(auth.login("operator", "wrong", &h), Err(401));
        assert_eq!(auth.login("wrong", "fixture-password-only", &h), Err(401));
        let cookie = login(&auth, &mut h);
        for attribute in [
            "__Host-relay_session=",
            "HttpOnly",
            "Secure",
            "SameSite=Strict",
            "Max-Age=604800",
            "Path=/",
        ] {
            assert!(cookie.contains(attribute));
        }
        assert!(auth.authorized(&h, &Method::GET));
        assert!(auth.authorized(&h, &Method::POST));
        let mut cross = h.clone();
        cross.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        cross.insert("x-forwarded-host", "relay.example".parse().unwrap());
        assert!(!auth.authorized(&cross, &Method::POST));
        assert_eq!(
            auth.login("operator", "fixture-password-only", &cross),
            Err(403)
        );
        assert_eq!(auth.logout(&cross), Err(403));
        cross.remove(header::ORIGIN);
        assert!(!auth.authorized(&cross, &Method::POST));
        h.insert(header::AUTHORIZATION, "Bearer bad".parse().unwrap());
        assert!(!auth.authorized(&h, &Method::GET));
        h.remove(header::AUTHORIZATION);
        assert!(auth.logout(&h).unwrap().contains("Max-Age=0"));
        assert!(!auth.session_valid(&h));
        login(&auth, &mut h);
        auth.state
            .lock()
            .unwrap()
            .sessions
            .values_mut()
            .next()
            .unwrap()
            .expires = Instant::now();
        assert!(!auth.session_valid(&h));
        login(&auth, &mut h);
        credentials(
            &dir.path().join("credentials.json"),
            "changed-fixture-password",
        );
        assert!(!auth.session_valid(&h));
        assert_eq!(auth.login("operator", "wrong", &h), Err(429));
        auth.state.lock().unwrap().window = Instant::now() - Duration::from_secs(61);
        assert_eq!(auth.login("operator", "wrong", &h), Err(401));
    }
    #[test]
    fn renewed_session_revokes_previous_cookie_and_store_is_bounded() {
        let (_dir, auth, mut h) = fixture(Mode::Hybrid);
        login(&auth, &mut h);
        let previous = h.clone();
        login(&auth, &mut h);
        assert!(!auth.session_valid(&previous));
        assert!(auth.session_valid(&h));
        h.insert(header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
        assert!(!auth.authorized(&h, &Method::GET));
        h.remove(header::AUTHORIZATION);
        {
            let mut state = auth.state.lock().unwrap();
            let revision = state.sessions.values().next().unwrap().revision.clone();
            for n in 0..MAX_SESSIONS {
                state.sessions.insert(
                    format!("{n:064x}"),
                    Session {
                        expires: Instant::now() + TTL,
                        revision: revision.clone(),
                    },
                );
            }
            state.sessions.remove(auth.cookie(&h).unwrap());
            assert_eq!(state.sessions.len(), MAX_SESSIONS);
            assert_eq!(
                auth.login("operator", "fixture-password-only", &h),
                Err(429)
            );
        }
        h.remove(header::COOKIE);
        login(&auth, &mut h);
        assert_eq!(auth.state.lock().unwrap().sessions.len(), MAX_SESSIONS);
    }
    #[test]
    fn explicit_modes_and_credentials_fail_closed() {
        assert!(Auth::new(None, None).is_err());
        let (dir, auth, mut h) = fixture(Mode::Hybrid);
        h.insert(
            header::AUTHORIZATION,
            "Bearer test-token-00000000000000000000000".parse().unwrap(),
        );
        h.remove(header::ORIGIN);
        assert!(auth.authorized(&h, &Method::POST));
        h.remove(header::AUTHORIZATION);
        assert!(!auth.authorized(&h, &Method::GET));
        h.insert(header::ORIGIN, "https://relay.example".parse().unwrap());
        login(&auth, &mut h);
        fs::remove_file(dir.path().join("credentials.json")).unwrap();
        assert!(!auth.session_valid(&h));
        for origin in [
            "http://relay.example",
            "https://relay.example/path",
            "https://user@relay.example",
            "https://relay.example/",
            "https://relay.example?query",
        ] {
            assert!(
                validate_origin(&Config {
                    mode: Mode::Session,
                    credentials_file: PathBuf::new(),
                    public_origin: origin.into(),
                    allow_insecure_loopback: false
                })
                .is_err(),
                "{origin}"
            );
        }
        assert!(
            validate_origin(&Config {
                mode: Mode::Session,
                credentials_file: PathBuf::new(),
                public_origin: "http://evil.example".into(),
                allow_insecure_loopback: true
            })
            .is_err()
        );
    }
}
