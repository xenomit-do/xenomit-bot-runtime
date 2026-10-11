//! The [`SessionProvider`] seam: where an authenticated Backpage `Session`
//! comes from. The GUI collects credentials on its sign-in form and hands
//! them to [`BackpageSessionProvider`]; a headless front-end loads the same
//! credential shapes from its mounted credentials file (orchestrator doc
//! §2.4) and can re-authenticate in-process on `HostAction::Refresh`.
use xenomit_backpage::login::{login_with_credentials, login_with_sid};
pub use xenomit_backpage::session::Session;

/// Credential material for one Backpage authentication attempt — either an
/// account password or an already-issued session id (dosid).
#[derive(Clone, Debug)]
pub enum SessionCredentials {
    Password { username: String, password: String },
    SessionId { server: String, sid: String },
}

/// Produces an authenticated `Session`. Blocking: implementations drive any
/// async internally so callers can stay on plain worker threads.
pub trait SessionProvider: Send + Sync {
    fn authenticate(&self) -> Result<Session, String>;
}

/// Authenticates against the real Backpage — the sign-in flows the GUI has
/// always used, also what a headless front-end needs.
pub struct BackpageSessionProvider {
    credentials: SessionCredentials,
}

impl BackpageSessionProvider {
    pub fn new(credentials: SessionCredentials) -> Self {
        Self { credentials }
    }
}

impl SessionProvider for BackpageSessionProvider {
    fn authenticate(&self) -> Result<Session, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("Could not initialize the network runtime: {error}"))?;
        runtime.block_on(async {
            match &self.credentials {
                SessionCredentials::Password { username, password } => {
                    let session = login_with_credentials(username, password)
                        .await
                        .map_err(|error| format!("Backpage sign-in failed: {error}"))?;
                    login_with_sid(&session.server, &session.sid)
                        .await
                        .map_err(|error| format!("Could not validate the new session: {error}"))
                }
                SessionCredentials::SessionId { server, sid } => login_with_sid(server, sid)
                    .await
                    .map_err(|error| format!("Backpage rejected the session: {error}")),
            }
        })
    }
}
