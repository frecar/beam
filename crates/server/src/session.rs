use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use beam_protocol::SessionInfo;
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};
use tokio::sync::RwLock;
use uuid::Uuid;

const SESSION_DIR: &str = "/var/lib/beam/sessions";

#[derive(Serialize, Deserialize)]
struct PersistedSession {
    session_id: Uuid,
    username: String,
    display: u32,
    width: u32,
    height: u32,
    created_at: u64,
    agent_pid: u32,
    agent_token: String,
}

/// Constant-time byte comparison to prevent timing side-channel attacks.
/// Returns true only if both slices have equal length and identical contents.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Generate a random hex token for agent authentication.
fn generate_agent_token() -> String {
    use std::fmt::Write;
    use std::io::Read;
    let mut bytes = [0u8; 32];
    let f = std::fs::File::open("/dev/urandom").expect("Failed to open /dev/urandom");
    (&f).read_exact(&mut bytes)
        .expect("Failed to read random bytes");
    let mut hex = String::with_capacity(64);
    for b in &bytes {
        write!(hex, "{b:02x}").unwrap();
    }
    hex
}

/// Manages the lifecycle of remote desktop sessions.
pub struct SessionManager {
    sessions: RwLock<HashMap<Uuid, ManagedSession>>,
    default_width: u32,
    default_height: u32,
    /// Pool of available display numbers for recycling
    display_pool: RwLock<DisplayPool>,
    /// ICE server config JSON to pass to agents
    ice_servers_json: Option<String>,
    /// Path to TLS cert PEM for agent cert pinning
    tls_cert_path: Option<String>,
    /// Video/audio config to pass to agents
    video_config: beam_protocol::VideoConfig,
}

struct DisplayPool {
    next: u32,
    /// Display numbers freed by destroyed sessions
    free: HashSet<u32>,
}

impl DisplayPool {
    fn new(start: u32) -> Self {
        Self {
            next: start,
            free: HashSet::new(),
        }
    }

    fn allocate(&mut self) -> u32 {
        if let Some(&num) = self.free.iter().next() {
            self.free.remove(&num);
            num
        } else {
            let num = self.next;
            self.next += 1;
            num
        }
    }

    fn release(&mut self, num: u32) {
        self.free.insert(num);
    }
}

struct ManagedSession {
    pub info: SessionInfo,
    pub agent_process: Option<Child>,
    /// PID of the agent process, stored separately so we can signal it
    /// even after the Child handle has been taken by the monitor task.
    pub agent_pid: Option<u32>,
    /// Timestamp of last heartbeat/activity (Unix epoch seconds)
    pub last_activity: u64,
    /// Secret token the agent must present on WebSocket upgrade
    pub agent_token: String,
}

impl SessionManager {
    pub fn new(
        display_start: u32,
        default_width: u32,
        default_height: u32,
        ice_servers_json: Option<String>,
        tls_cert_path: Option<String>,
        video_config: beam_protocol::VideoConfig,
    ) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            default_width,
            default_height,
            display_pool: RwLock::new(DisplayPool::new(display_start)),
            ice_servers_json,
            tls_cert_path,
            video_config,
        }
    }

    /// Create a new session for a user.
    ///
    /// Allocates a display number and spawns the beam-agent process.
    /// Returns an error if max_sessions would be exceeded.
    pub async fn create_session(
        &self,
        username: &str,
        server_url: &str,
        max_sessions: usize,
        initial_width: Option<u32>,
        initial_height: Option<u32>,
    ) -> Result<SessionInfo> {
        // Use client viewport dimensions if provided, clamped to sane bounds.
        // Fall back to config defaults for old clients or missing values.
        let width = initial_width
            .filter(|&w| (320..=3840).contains(&w))
            .unwrap_or(self.default_width);
        let height = initial_height
            .filter(|&h| (240..=2160).contains(&h))
            .unwrap_or(self.default_height);

        // Atomically check max sessions and reserve a slot under the write lock
        // to prevent TOCTOU race (two concurrent logins both passing the check).
        // Both locks are acquired in a single scope to avoid deadlock from
        // inconsistent lock ordering.
        let session_id = Uuid::new_v4();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let agent_token = generate_agent_token();
        let display_num;

        {
            let mut sessions = self.sessions.write().await;
            if sessions.len() >= max_sessions {
                anyhow::bail!("Maximum number of sessions reached ({max_sessions})");
            }

            display_num = self.display_pool.write().await.allocate();

            let info = SessionInfo {
                id: session_id,
                username: username.to_string(),
                display: display_num,
                width,
                height,
                created_at: now,
            };

            // Reserve the slot immediately so concurrent requests see it
            let managed = ManagedSession {
                info: info.clone(),
                agent_process: None,
                agent_pid: None,
                last_activity: now,
                agent_token: agent_token.clone(),
            };
            sessions.insert(session_id, managed);
        }

        let info = SessionInfo {
            id: session_id,
            username: username.to_string(),
            display: display_num,
            width,
            height,
            created_at: now,
        };

        // Clean up stale temp files from previous sessions on this display number.
        // These may be owned by a different user if the previous agent was killed
        // without running its Drop handler (e.g., SIGKILL during deployment).
        let _ = std::fs::remove_file(format!("/tmp/beam-xorg-{display_num}.conf"));
        let _ = std::fs::remove_file(format!("/tmp/beam-pulse-{display_num}.pa"));
        let _ = std::fs::remove_dir_all(format!("/tmp/beam-pulse-{display_num}"));
        // Remove stale X lock file if Xorg didn't clean up
        let _ = std::fs::remove_file(format!("/tmp/.X{display_num}-lock"));

        // Spawn the agent process (outside the write lock to avoid holding it during spawn)
        let agent_process = match self.spawn_agent(&info, server_url, &agent_token).await {
            Ok(child) => child,
            Err(e) => {
                // Clean up the reserved slot on spawn failure
                self.sessions.write().await.remove(&session_id);
                self.display_pool.write().await.release(display_num);
                return Err(e).context("Failed to spawn agent");
            }
        };

        // Update the reserved slot with the agent process
        let agent_pid = agent_process.id();
        {
            let mut sessions = self.sessions.write().await;
            if let Some(session) = sessions.get_mut(&session_id) {
                session.agent_process = Some(agent_process);
                session.agent_pid = agent_pid;
            }
        }

        tracing::info!(
            %session_id,
            %username,
            display_num,
            "Session created"
        );

        Ok(info)
    }

    /// Destroy a session, gracefully stopping the agent process.
    /// Waits for the agent to fully exit before releasing the display number
    /// to prevent race conditions where a new session reuses the display
    /// while the old Xorg is still shutting down.
    pub async fn destroy_session(&self, session_id: Uuid) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        if let Some(mut session) = sessions.remove(&session_id) {
            let display_num = session.info.display;

            // Always signal by stored PID (works even when monitor has taken the Child)
            if let Some(pid) = session.agent_pid {
                tracing::info!(%session_id, pid, "Sending SIGTERM to agent");
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid as i32),
                    nix::sys::signal::Signal::SIGTERM,
                );
            }

            // Drop the write lock before waiting (other operations shouldn't be blocked)
            drop(sessions);

            // If we still own the Child handle, wait for graceful shutdown
            if let Some(ref mut child) = session.agent_process {
                match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
                    Ok(Ok(status)) => {
                        tracing::info!(%session_id, ?status, "Agent exited");
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(%session_id, "Error waiting for agent: {e}");
                    }
                    Err(_) => {
                        tracing::warn!(%session_id, "Agent did not exit in time, killing");
                        let _ = child.kill().await;
                    }
                }
            } else if let Some(pid) = session.agent_pid {
                // Child handle was taken by monitor task — poll the PID to
                // confirm exit so the display is fully cleaned up before reuse.
                let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
                for _ in 0..50 {
                    // signal 0 checks if the process exists without signaling it
                    match nix::sys::signal::kill(nix_pid, None) {
                        Err(nix::errno::Errno::ESRCH) => break, // process gone
                        _ => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
                    }
                }
            }

            // Clean up agent log file
            let log_path = format!("/tmp/beam-agent-{session_id}.log");
            let _ = std::fs::remove_file(&log_path);

            // Now that the agent has exited, recycle the display number
            self.display_pool.write().await.release(display_num);
            tracing::info!(%session_id, "Session destroyed");
        }
        Ok(())
    }

    /// List all active sessions.
    pub async fn list_sessions(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.read().await;
        sessions.values().map(|s| s.info.clone()).collect()
    }

    /// Get a specific session's info.
    pub async fn get_session(&self, session_id: Uuid) -> Option<SessionInfo> {
        let sessions = self.sessions.read().await;
        sessions.get(&session_id).map(|s| s.info.clone())
    }

    /// Update the heartbeat timestamp for a session.
    pub async fn heartbeat(&self, session_id: Uuid) -> bool {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(&session_id) {
            session.last_activity = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            true
        } else {
            false
        }
    }

    /// Return IDs of sessions that haven't had activity in `max_idle_secs`.
    pub async fn stale_sessions(&self, max_idle_secs: u64) -> Vec<Uuid> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let sessions = self.sessions.read().await;
        sessions
            .values()
            .filter(|s| now.saturating_sub(s.last_activity) > max_idle_secs)
            .map(|s| s.info.id)
            .collect()
    }

    /// Verify the agent token for a session. Returns true if valid.
    /// Uses constant-time comparison to prevent timing side-channel attacks.
    pub async fn verify_agent_token(&self, session_id: Uuid, token: &str) -> bool {
        let sessions = self.sessions.read().await;
        sessions
            .get(&session_id)
            .map(|s| constant_time_eq(s.agent_token.as_bytes(), token.as_bytes()))
            .unwrap_or(false)
    }

    /// Take the agent child process for external monitoring.
    /// Returns the Child if it hasn't been taken already.
    pub async fn take_agent_child(&self, session_id: Uuid) -> Option<Child> {
        let mut sessions = self.sessions.write().await;
        sessions
            .get_mut(&session_id)
            .and_then(|s| s.agent_process.take())
    }

    /// Find a session by username (returns the first match).
    pub async fn find_by_username(&self, username: &str) -> Option<SessionInfo> {
        let sessions = self.sessions.read().await;
        sessions
            .values()
            .find(|s| s.info.username == username)
            .map(|s| s.info.clone())
    }

    async fn spawn_agent(
        &self,
        info: &SessionInfo,
        server_url: &str,
        agent_token: &str,
    ) -> Result<Child> {
        let display_str = format!(":{}", info.display);

        let mut cmd = Command::new("beam-agent");
        cmd.arg("--display")
            .arg(&display_str)
            .arg("--session-id")
            .arg(info.id.to_string())
            .arg("--server-url")
            .arg(server_url)
            .arg("--width")
            .arg(info.width.to_string())
            .arg("--height")
            .arg(info.height.to_string())
            .arg("--framerate")
            .arg(self.video_config.framerate.to_string())
            .arg("--bitrate")
            .arg(self.video_config.bitrate.to_string())
            .arg("--min-bitrate")
            .arg(self.video_config.min_bitrate.to_string())
            .arg("--max-bitrate")
            .arg(self.video_config.max_bitrate.to_string());

        // Pass encoder preference if configured
        if let Some(ref encoder) = self.video_config.encoder {
            cmd.arg("--encoder").arg(encoder);
        }

        // Pass agent authentication token via environment variable
        // (CLI args are visible to all users via /proc/<pid>/cmdline)
        cmd.env("BEAM_AGENT_TOKEN", agent_token);

        // Pass TLS cert path for certificate pinning
        if let Some(ref cert_path) = self.tls_cert_path {
            cmd.arg("--tls-cert").arg(cert_path);
        }

        // Pass ICE/TURN server config if available
        if let Some(ref ice_json) = self.ice_servers_json {
            cmd.arg("--ice-servers").arg(ice_json);
        }

        // Set agent log level to info (avoid inheriting server's debug level)
        cmd.env("RUST_LOG", "info");

        // Run agent as the authenticated user for security isolation.
        // Look up the user's UID/GID and set HOME/USER/LOGNAME environment.
        // If the user doesn't exist on the system, run as current user with a warning.
        match lookup_user(&info.username) {
            Some(user_info) => {
                tracing::info!(
                    username = %info.username,
                    uid = user_info.uid,
                    gid = user_info.gid,
                    home = %user_info.home,
                    "Running agent as user"
                );
                let uid = user_info.uid;
                let gid = user_info.gid;
                let username_c = std::ffi::CString::new(info.username.as_str())
                    .unwrap_or_else(|_| std::ffi::CString::new("nobody").unwrap());

                // SAFETY: pre_exec runs between fork and exec. initgroups sets
                // supplementary groups (e.g. input, video, render) needed by the agent.
                unsafe {
                    cmd.pre_exec(move || {
                        // Set supplementary groups from /etc/group
                        if libc::initgroups(username_c.as_ptr(), gid) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        // setgid and setuid (order matters: gid first)
                        if libc::setgid(gid) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        if libc::setuid(uid) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }

                cmd.env("HOME", &user_info.home);
                cmd.env("USER", &info.username);
                cmd.env("LOGNAME", &info.username);
                cmd.env("DISPLAY", &display_str);

                // PulseAudio needs XDG_RUNTIME_DIR
                let runtime_dir = format!("/run/user/{}", user_info.uid);
                // Ensure the directory exists (may not for non-login users)
                let _ = std::fs::create_dir_all(&runtime_dir);
                // Set ownership to the target user
                unsafe {
                    let c_path = std::ffi::CString::new(runtime_dir.as_str()).unwrap();
                    libc::chown(c_path.as_ptr(), user_info.uid, user_info.gid);
                }
                cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
            }
            None => {
                tracing::warn!(
                    username = %info.username,
                    "User not found in system, running agent as current user"
                );
                cmd.env("DISPLAY", &display_str);
            }
        }

        // Write agent logs to a dedicated file per session.
        // IMPORTANT: Never use Stdio::piped() without reading the pipe -
        // the 64KB pipe buffer fills up and blocks the agent.
        let log_path = format!("/tmp/beam-agent-{}.log", info.id);
        let log_file = std::fs::File::create(&log_path)
            .with_context(|| format!("Failed to create agent log at {log_path}"))?;
        let log_file_clone = log_file
            .try_clone()
            .context("Failed to clone agent log file")?;
        tracing::info!(%log_path, "Agent log file created");

        let child = cmd
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_file_clone))
            .spawn()
            .with_context(|| format!("Failed to spawn beam-agent for display {}", display_str))?;

        tracing::info!(
            session_id = %info.id,
            display = display_str,
            pid = child.id().unwrap_or(0),
            "Agent process spawned"
        );

        Ok(child)
    }

    /// Save all active sessions to disk for graceful restart.
    /// Agents are left running — the new server process re-adopts them.
    pub async fn persist_sessions(&self) -> Result<()> {
        let dir = Path::new(SESSION_DIR);
        std::fs::create_dir_all(dir).context("Failed to create session persistence directory")?;

        // Clean old files
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }

        let sessions = self.sessions.read().await;
        let mut count = 0;
        for (id, managed) in sessions.iter() {
            let Some(pid) = managed.agent_pid else {
                continue;
            };
            let persisted = PersistedSession {
                session_id: *id,
                username: managed.info.username.clone(),
                display: managed.info.display,
                width: managed.info.width,
                height: managed.info.height,
                created_at: managed.info.created_at,
                agent_pid: pid,
                agent_token: managed.agent_token.clone(),
            };
            let path = dir.join(format!("{id}.json"));
            let tmp_path = dir.join(format!("{id}.json.tmp"));
            let data = serde_json::to_string_pretty(&persisted)?;

            // Write with restricted permissions (contains agent token)
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_path)
                .with_context(|| format!("Failed to write {}", tmp_path.display()))?;
            file.write_all(data.as_bytes())?;
            std::fs::rename(&tmp_path, &path)?;
            count += 1;
        }

        tracing::info!(count, "Persisted sessions to disk");
        Ok(())
    }

    /// Restore sessions from a previous graceful shutdown.
    /// Verifies each agent is still alive. Returns (session_id, agent_pid) pairs.
    pub async fn restore_sessions(&self) -> Vec<(Uuid, u32)> {
        let dir = Path::new(SESSION_DIR);
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut restored = Vec::new();

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            let data = match std::fs::read_to_string(&path) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(path = %path.display(), "Failed to read session file: {e}");
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
            };

            let persisted: PersistedSession = match serde_json::from_str(&data) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(path = %path.display(), "Failed to parse session file: {e}");
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
            };

            // Verify agent is still alive
            let nix_pid = nix::unistd::Pid::from_raw(persisted.agent_pid as i32);
            if nix::sys::signal::kill(nix_pid, None).is_err() {
                tracing::info!(
                    session_id = %persisted.session_id,
                    pid = persisted.agent_pid,
                    "Agent no longer alive, skipping"
                );
                let _ = std::fs::remove_file(&path);
                continue;
            }

            // Reserve the display number to avoid double-allocation
            {
                let mut pool = self.display_pool.write().await;
                pool.free.remove(&persisted.display);
                if persisted.display >= pool.next {
                    pool.next = persisted.display + 1;
                }
            }

            let info = SessionInfo {
                id: persisted.session_id,
                username: persisted.username.clone(),
                display: persisted.display,
                width: persisted.width,
                height: persisted.height,
                created_at: persisted.created_at,
            };

            let managed = ManagedSession {
                info,
                agent_process: None, // orphaned — no Child handle
                agent_pid: Some(persisted.agent_pid),
                last_activity: now,
                agent_token: persisted.agent_token,
            };

            let mut sessions = self.sessions.write().await;
            sessions.insert(persisted.session_id, managed);
            restored.push((persisted.session_id, persisted.agent_pid));

            tracing::info!(
                session_id = %persisted.session_id,
                username = %persisted.username,
                display = persisted.display,
                pid = persisted.agent_pid,
                "Restored session from disk"
            );

            let _ = std::fs::remove_file(&path);
        }

        restored
    }
}

struct UserInfo {
    uid: u32,
    gid: u32,
    home: String,
}

/// Look up a Unix user by name, returning UID, GID, and home directory.
/// Uses getpwnam via nix, which supports NSS (LDAP, SSSD, etc.).
fn lookup_user(username: &str) -> Option<UserInfo> {
    let user = nix::unistd::User::from_name(username).ok()??;
    Some(UserInfo {
        uid: user.uid.as_raw(),
        gid: user.gid.as_raw(),
        home: user.dir.to_string_lossy().into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_root_user() {
        // root always exists on Linux
        let user = lookup_user("root");
        assert!(user.is_some(), "root user should exist");
        let user = user.unwrap();
        assert_eq!(user.uid, 0);
        assert_eq!(user.gid, 0);
        assert_eq!(user.home, "/root");
    }

    #[test]
    fn lookup_nonexistent_user() {
        let user = lookup_user("beam_nonexistent_user_12345");
        assert!(user.is_none());
    }

    #[test]
    fn display_pool_allocates_sequentially() {
        let mut pool = DisplayPool::new(10);
        assert_eq!(pool.allocate(), 10);
        assert_eq!(pool.allocate(), 11);
        assert_eq!(pool.allocate(), 12);
    }

    #[test]
    fn display_pool_recycles() {
        let mut pool = DisplayPool::new(10);
        assert_eq!(pool.allocate(), 10);
        assert_eq!(pool.allocate(), 11);
        pool.release(10);
        // Should reuse 10 before allocating 12
        assert_eq!(pool.allocate(), 10);
        assert_eq!(pool.allocate(), 12);
    }

    #[test]
    fn agent_token_is_64_hex_chars() {
        let token = generate_agent_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn agent_token_is_unique() {
        let t1 = generate_agent_token();
        let t2 = generate_agent_token();
        assert_ne!(t1, t2);
    }

    #[tokio::test]
    async fn verify_agent_token_rejects_wrong_token() {
        let manager = SessionManager::new(
            100,
            1920,
            1080,
            None,
            None,
            beam_protocol::VideoConfig::default(),
        );
        let id = Uuid::new_v4();
        // Non-existent session should reject
        assert!(!manager.verify_agent_token(id, "fake-token").await);
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hell"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
