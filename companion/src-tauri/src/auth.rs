//! Sign in with Keo: OAuth 2.1 for a native public client (dynamic registration, PKCE S256,
//! loopback redirect), refresh-token rotation, revoke on sign out.
//!
//! Where secrets live: the refresh token in the macOS keychain (service = the bundle
//! identifier), the access token in memory only. Neither is ever logged.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::keo::BaseUrl;
use crate::state::Store;

const KEYCHAIN_ENTRY: &str = "keo-refresh-token";
const CALLBACK_PATH: &str = "/callback";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const SCOPE: &str = "keo offline_access";
/// Refresh this long before the access token expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("signed out")]
    SignedOut,
    #[error("sign-in timed out")]
    Timeout,
    #[error("sign-in was refused: {0}")]
    Denied(String),
    #[error("the sign-in reply did not match this request")]
    StateMismatch,
    #[error("network: {0}")]
    Network(String),
    #[error("server: {0}")]
    Server(String),
    #[error("keychain: {0}")]
    Keychain(String),
    #[error("io: {0}")]
    Io(String),
    /// A sign-out or another sign-in began while this token request ran: its reply is dropped
    /// unread, so it never overwrites or clears the newer account's tokens.
    #[error("superseded by a newer sign-in")]
    Superseded,
}

// ---- PKCE ----

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn random_b64url(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut bytes);
    b64url(&bytes)
}

/// S256: base64url(sha256(verifier)), 43 characters.
pub fn challenge_for(verifier: &str) -> String {
    b64url(&Sha256::digest(verifier.as_bytes()))
}

/// A 43-character verifier (32 random bytes) and its challenge.
pub fn new_pkce() -> Pkce {
    let verifier = random_b64url(32);
    let challenge = challenge_for(&verifier);
    Pkce {
        verifier,
        challenge,
    }
}

// ---- loopback callback ----

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub iss: Option<String>,
}

/// Parse the request target of `GET /callback?...`. Any other path is `None`.
/// The browser callback's `error`, as a known OAuth code only (it reaches the log).
fn callback_denied(error: &str) -> AuthError {
    AuthError::Denied(crate::keo::contract::oauth_error(error))
}

/// A token endpoint refusal, as a known OAuth code only (it reaches the log).
fn token_error(error: &str, status: u16) -> TokenOutcome {
    match error {
        "invalid_grant" => TokenOutcome::InvalidGrant,
        "invalid_client" => TokenOutcome::InvalidClient,
        "" => TokenOutcome::Other(format!("status {status}")),
        other => TokenOutcome::Other(crate::keo::contract::oauth_error(other)),
    }
}

pub fn parse_callback(target: &str) -> Option<CallbackParams> {
    let url = url::Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    if url.path() != CALLBACK_PATH {
        return None;
    }
    let mut p = CallbackParams::default();
    for (k, v) in url.query_pairs() {
        let v = Some(v.into_owned());
        match k.as_ref() {
            "code" => p.code = v,
            "state" => p.state = v,
            "error" => p.error = v,
            "iss" => p.iss = v,
            _ => {}
        }
    }
    Some(p)
}

/// The request target from the first line of an HTTP request.
fn request_target(head: &str) -> Option<&str> {
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    (parts.next()? == "GET").then_some(())?;
    parts.next()
}

/// Accept connections until one is `GET /callback`, answer it, and return its parameters.
async fn wait_for_callback(listener: TcpListener) -> Result<CallbackParams, AuthError> {
    let accept_loop = async {
        loop {
            let (mut sock, _) = listener
                .accept()
                .await
                .map_err(|e| AuthError::Io(e.to_string()))?;
            let mut buf = vec![0u8; 8192];
            let mut len = 0;
            while len < buf.len() {
                let n =
                    match tokio::time::timeout(Duration::from_secs(10), sock.read(&mut buf[len..]))
                        .await
                    {
                        Ok(Ok(n)) => n,
                        _ => 0,
                    };
                if n == 0 {
                    break;
                }
                len += n;
                if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&buf[..len]).to_string();
            let params = request_target(&head).and_then(parse_callback);
            let (status, body) = match &params {
                Some(p) if p.error.is_some() => (
                    "200 OK",
                    "Sign-in was not completed. You can close this tab and try again from Keo Companion.",
                ),
                Some(_) => (
                    "200 OK",
                    "You are signed in to Keo Companion. You can close this tab.",
                ),
                None => ("404 Not Found", "Not found."),
            };
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(reply.as_bytes()).await;
            let _ = sock.shutdown().await;
            if let Some(p) = params {
                return Ok(p);
            }
        }
    };
    tokio::time::timeout(CALLBACK_TIMEOUT, accept_loop)
        .await
        .map_err(|_| AuthError::Timeout)?
}

// ---- keychain ----

/// The refresh token in the OS keychain.
pub struct Keychain {
    service: String,
}

impl Keychain {
    pub fn new(service: &str) -> Self {
        Self {
            service: service.to_string(),
        }
    }

    fn entry(&self) -> Result<keyring::Entry, AuthError> {
        keyring::Entry::new(&self.service, KEYCHAIN_ENTRY)
            .map_err(|e| AuthError::Keychain(e.to_string()))
    }

    pub fn get(&self) -> Result<Option<String>, AuthError> {
        match self.entry()?.get_password() {
            Ok(t) => Ok(Some(t)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(AuthError::Keychain(e.to_string())),
        }
    }

    pub fn set(&self, token: &str) -> Result<(), AuthError> {
        self.entry()?
            .set_password(token)
            .map_err(|e| AuthError::Keychain(e.to_string()))
    }

    pub fn delete(&self) -> Result<(), AuthError> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(AuthError::Keychain(e.to_string())),
        }
    }
}

/// Where the refresh token is kept. `Keychain` in the app; a fake in tests.
pub trait SecretStore: Send + Sync {
    fn get(&self) -> Result<Option<String>, AuthError>;
    fn set(&self, token: &str) -> Result<(), AuthError>;
    fn delete(&self) -> Result<(), AuthError>;
}

impl SecretStore for Keychain {
    fn get(&self) -> Result<Option<String>, AuthError> {
        Keychain::get(self)
    }
    fn set(&self, token: &str) -> Result<(), AuthError> {
        Keychain::set(self, token)
    }
    fn delete(&self) -> Result<(), AuthError> {
        Keychain::delete(self)
    }
}

/// The current refresh token. Memory is the source of truth once a token is known: refresh
/// tokens rotate and are single use, so after a rotation the new token is kept here even when
/// the keychain write fails, and a spent token is never read back and resubmitted. While the
/// write has not succeeded the vault is `unsaved`, and `Auth` hands out no access token: the
/// new token is saved before any use. If the app quits before a write succeeds, the keychain
/// still holds only the spent token; the next launch's refresh then gets `invalid_grant` and
/// lands cleanly in SignedOut, and the person signs in again.
pub struct RefreshVault {
    store: Box<dyn SecretStore>,
    current: Mutex<Option<String>>,
    unsaved: Mutex<bool>,
}

impl RefreshVault {
    pub fn new(store: Box<dyn SecretStore>) -> Self {
        Self {
            store,
            current: Mutex::new(None),
            unsaved: Mutex::new(false),
        }
    }

    /// The current token: from memory, else from the store (once).
    pub fn current(&self) -> Result<Option<String>, AuthError> {
        let mut cur = self.current.lock().expect("vault lock");
        if cur.is_some() {
            return Ok(cur.clone());
        }
        let stored = self.store.get()?;
        cur.clone_from(&stored);
        Ok(stored)
    }

    /// A new token from the server replaces the spent one at once in memory, then is written.
    /// An error means it is kept in memory but not yet saved (`unsaved`); call `flush` again.
    pub fn rotate(&self, token: &str) -> Result<(), AuthError> {
        let mut cur = self.current.lock().expect("vault lock");
        *cur = Some(token.to_string());
        *self.unsaved.lock().expect("vault lock") = true;
        self.flush_locked(&cur)
    }

    /// Retry the write of an unsaved token. Ok when nothing waits.
    pub fn flush(&self) -> Result<(), AuthError> {
        let cur = self.current.lock().expect("vault lock");
        self.flush_locked(&cur)
    }

    /// Forget the token everywhere.
    pub fn clear(&self) -> Result<(), AuthError> {
        let mut cur = self.current.lock().expect("vault lock");
        *cur = None;
        *self.unsaved.lock().expect("vault lock") = false;
        self.store.delete()
    }

    /// Whether a rotated token still waits for its keychain write.
    pub fn unsaved(&self) -> bool {
        *self.unsaved.lock().expect("vault lock")
    }

    fn flush_locked(&self, cur: &Option<String>) -> Result<(), AuthError> {
        let mut unsaved = self.unsaved.lock().expect("vault lock");
        if !*unsaved {
            return Ok(());
        }
        let Some(t) = cur else {
            *unsaved = false;
            return Ok(());
        };
        match self.store.set(t) {
            Ok(()) => {
                *unsaved = false;
                Ok(())
            }
            Err(e) => {
                tracing::warn!(error = %e, "refresh token kept in memory; keychain write will be retried");
                Err(e)
            }
        }
    }
}

/// The client id to reuse for a sign-in. A cached id is reused only while a grant (a refresh
/// token) exists for it: a client from an abandoned sign-in may have been pruned by the server,
/// and its authorize page would never call back.
pub fn reusable_client(cached: Option<String>, has_grant: bool) -> Option<String> {
    cached.filter(|_| has_grant)
}

// ---- token endpoint replies ----

#[derive(Deserialize)]
struct TokenReply {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Deserialize, Default)]
struct OAuthErrorReply {
    #[serde(default)]
    error: String,
}

#[derive(Deserialize)]
struct RegisterReply {
    client_id: String,
}

enum TokenOutcome {
    Ok(TokenReply),
    InvalidGrant,
    InvalidClient,
    Other(String),
}

struct Access {
    token: String,
    expires: Instant,
}

/// Sign-in state and the token logic. Shared as `Arc<Auth>`.
pub struct Auth {
    base: BaseUrl,
    http: reqwest::Client,
    store: Arc<Store>,
    vault: RefreshVault,
    access: Mutex<Option<Access>>,
    /// An access token that arrived with a refresh token whose keychain write failed. It is
    /// published only once that write succeeds.
    held_access: Mutex<Option<Access>>,
    /// Tests only: token endpoint answers to return instead of the network (popped from the end).
    #[cfg(test)]
    token_stub: Mutex<Vec<TokenOutcome>>,
    /// Tests only: the forms sent to the token endpoint.
    #[cfg(test)]
    token_forms: Mutex<Vec<Vec<(String, String)>>>,
    /// Tests only: start a new sign-in generation while the token request is in flight.
    #[cfg(test)]
    bump_during_request: std::sync::atomic::AtomicBool,
    /// Refresh tokens rotate and are single-use: only one refresh runs at a time.
    refresh_lock: tokio::sync::Mutex<()>,
    /// Which sign-in the tokens belong to: bumped when a sign-in starts and at sign out, so a
    /// call begun under one sign-in never retries with another account's token
    /// (`KeoClient::call_tool`).
    generation: AtomicU64,
    /// A browser sign-in is under way: no refresh of the saved account starts meanwhile.
    interactive: std::sync::atomic::AtomicBool,
    /// Bumped to cancel a browser sign-in still waiting for its callback (a new Sign in click,
    /// or Sign out), so nothing holds the app for the callback's 10 minutes.
    cancel: tokio::sync::watch::Sender<u64>,
}

impl Auth {
    pub fn new(base: BaseUrl, http: reqwest::Client, store: Arc<Store>, service: &str) -> Self {
        Self::with_secret_store(base, http, store, Box::new(Keychain::new(service)))
    }

    pub fn with_secret_store(
        base: BaseUrl,
        http: reqwest::Client,
        store: Arc<Store>,
        secrets: Box<dyn SecretStore>,
    ) -> Self {
        Self {
            base,
            http,
            store,
            vault: RefreshVault::new(secrets),
            access: Mutex::new(None),
            held_access: Mutex::new(None),
            #[cfg(test)]
            token_stub: Mutex::new(Vec::new()),
            #[cfg(test)]
            bump_during_request: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            token_forms: Mutex::new(Vec::new()),
            refresh_lock: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(0),
            interactive: std::sync::atomic::AtomicBool::new(false),
            cancel: tokio::sync::watch::channel(0).0,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// End the current generation: called once a sign-out has wiped its state, so work that
    /// started while that sign-out was still running (a folder edit, say) can never act after.
    pub fn end_generation(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Whether a refresh token is stored.
    pub fn has_refresh_token(&self) -> bool {
        matches!(self.vault.current(), Ok(Some(_)))
    }

    /// The full browser sign-in. `open` shows the authorize URL in the system browser.
    pub async fn sign_in(
        &self,
        open: impl FnOnce(&str) -> Result<(), String>,
    ) -> Result<(), AuthError> {
        let mut cancelled = self.cancel.subscribe();
        cancelled.borrow_and_update();
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.interactive.store(true, Ordering::SeqCst);
        let result = self.browser_sign_in(generation, cancelled, open).await;
        self.interactive.store(false, Ordering::SeqCst);
        result
    }

    /// Stop a browser sign-in that is still waiting for its callback; it returns `Superseded`.
    pub fn cancel_sign_in(&self) {
        self.cancel.send_modify(|n| *n += 1);
    }

    async fn browser_sign_in(
        &self,
        generation: u64,
        mut cancelled: tokio::sync::watch::Receiver<u64>,
        open: impl FnOnce(&str) -> Result<(), String>,
    ) -> Result<(), AuthError> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| AuthError::Io(e.to_string()))?;
        let port = listener
            .local_addr()
            .map_err(|e| AuthError::Io(e.to_string()))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
        let client_id = self.client_id(&redirect_uri).await?;
        let pkce = new_pkce();
        let state = random_b64url(24);
        let resource = self.base.mcp();

        let mut url = url::Url::parse(&self.base.join("/api/mcp/oauth/authorize"))
            .map_err(|e| AuthError::Io(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("scope", SCOPE)
            .append_pair("resource", &resource);
        open(url.as_str()).map_err(AuthError::Io)?;
        tracing::info!("sign-in: waiting for the browser");

        let params = tokio::select! {
            params = wait_for_callback(listener) => params?,
            _ = cancelled.changed() => {
                tracing::info!("sign-in: cancelled");
                return Err(AuthError::Superseded);
            }
        };
        if let Some(err) = params.error {
            return Err(callback_denied(&err));
        }
        if params.state.as_deref() != Some(state.as_str()) {
            return Err(AuthError::StateMismatch);
        }
        // RFC 9207: the issuer must be this Keo (its path, if any, is not checked here).
        if let Some(iss) = params.iss.as_deref() {
            let same = iss == self.base.origin()
                || iss
                    .strip_prefix(self.base.origin())
                    .is_some_and(|rest| rest.starts_with('/'));
            if !same {
                return Err(AuthError::StateMismatch);
            }
        }
        let code = params
            .code
            .ok_or_else(|| AuthError::Server("no code in the reply".into()))?;

        let form = [
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", client_id.as_str()),
            ("code_verifier", pkce.verifier.as_str()),
            ("resource", resource.as_str()),
        ];
        // Cancel still works during the code exchange: a slow token reply never signs in a
        // person who already pressed Cancel.
        let outcome = tokio::select! {
            outcome = self.token_request(&form) => outcome?,
            _ = cancelled.changed() => return Err(AuthError::Superseded),
        };
        // Take the refresh lock so no refresh of the old account interleaves with the swap,
        // and drop the reply if a sign-out, a later sign-in or a Cancel came meanwhile.
        let _lock = self.refresh_lock.lock().await;
        if self.generation() != generation || cancelled.has_changed().unwrap_or(true) {
            return Err(AuthError::Superseded);
        }
        match outcome {
            TokenOutcome::Ok(reply) => {
                self.accept(reply)?;
                // Calls still in flight with the old account's token are fenced off.
                self.generation.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            TokenOutcome::InvalidClient => {
                // The server pruned the client: register again on the next try.
                self.forget_client();
                Err(AuthError::Server(
                    "client expired, please sign in again".into(),
                ))
            }
            TokenOutcome::InvalidGrant => Err(AuthError::Denied("invalid_grant".into())),
            TokenOutcome::Other(e) => Err(AuthError::Server(e)),
        }
    }

    /// A valid access token, refreshed when it is about to expire.
    pub async fn access_token(&self) -> Result<String, AuthError> {
        if let Some(t) = self.cached() {
            return Ok(t);
        }
        self.refresh().await
    }

    /// Drop the cached access token if it is still `used` (after a 401).
    pub fn invalidate(&self, used: &str) {
        let mut guard = self.access.lock().expect("access lock");
        if guard.as_ref().is_some_and(|a| a.token == used) {
            *guard = None;
        }
    }

    /// Revoke (best effort), then forget the tokens. Err when the keychain entry could not be
    /// deleted: `clear_leftover` removes it at the next start.
    pub async fn sign_out(&self) -> Result<(), AuthError> {
        self.generation.fetch_add(1, Ordering::SeqCst);
        let _lock = self.refresh_lock.lock().await;
        *self.access.lock().expect("access lock") = None;
        *self.held_access.lock().expect("access lock") = None;
        let refresh = self.vault.current().ok().flatten();
        let client_id = self.store.get().client_id;
        if let (Some(token), Some(client_id)) = (refresh, client_id) {
            let form = [("token", token.as_str()), ("client_id", client_id.as_str())];
            let res = self
                .http
                .post(self.base.join("/api/mcp/oauth/revoke"))
                .timeout(Duration::from_secs(10))
                .form(&form)
                .send()
                .await;
            match res {
                Ok(r) => tracing::info!(status = r.status().as_u16(), "revoke answered"),
                Err(e) => tracing::warn!(error = %e.without_url(), "revoke failed"),
            }
        }
        self.vault.clear().inspect_err(|e| {
            tracing::warn!(error = %e, "keychain entry not deleted");
        })
    }

    /// A refresh token with no client id is what a sign-out left when its keychain delete
    /// failed (sign-out wipes state.json, which holds the client id; a sign-in saves the client
    /// id first). Delete it now. Returns whether one was found.
    pub fn clear_leftover(&self) -> bool {
        if self.store.get().client_id.is_some() || !self.has_refresh_token() {
            return false;
        }
        match self.vault.clear() {
            Ok(()) => tracing::info!("leftover keychain entry from a sign-out deleted"),
            Err(e) => tracing::warn!(error = %e, "leftover keychain entry not deleted"),
        }
        true
    }

    fn cached(&self) -> Option<String> {
        let guard = self.access.lock().expect("access lock");
        guard
            .as_ref()
            .filter(|a| a.expires > Instant::now() + EXPIRY_MARGIN)
            .map(|a| a.token.clone())
    }

    async fn refresh(&self) -> Result<String, AuthError> {
        let _lock = self.refresh_lock.lock().await;
        // A browser sign-in replaces the saved account: refreshing it now could only race it.
        if self.interactive.load(Ordering::SeqCst) {
            return Err(AuthError::Superseded);
        }
        let generation = self.generation();
        // Another task may have refreshed while this one waited.
        if let Some(t) = self.cached() {
            return Ok(t);
        }
        // A rotated token that is not saved yet: save it first. Until then no access token is
        // handed out and no new refresh runs.
        if self.vault.unsaved() {
            self.vault.flush()?;
            let held = self.held_access.lock().expect("access lock").take();
            if let Some(a) = held {
                *self.access.lock().expect("access lock") = Some(a);
            }
            if let Some(t) = self.cached() {
                return Ok(t);
            }
        }
        let refresh = self.vault.current()?.ok_or(AuthError::SignedOut)?;
        let client_id = self.store.get().client_id.ok_or(AuthError::SignedOut)?;
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
            ("client_id", client_id.as_str()),
        ];
        let outcome = self.token_request(&form).await?;
        // A sign-out or a new sign-in began while the request ran: the reply belongs to the old
        // account, so it neither rotates nor clears what the newer one saved.
        if self.generation() != generation {
            return Err(AuthError::Superseded);
        }
        match outcome {
            TokenOutcome::Ok(reply) => {
                self.accept(reply)?;
                self.cached().ok_or(AuthError::SignedOut)
            }
            TokenOutcome::InvalidGrant => {
                // Also the clean path for a spent token left in the keychain by an older run.
                tracing::info!("refresh token refused; signing out");
                let _ = self.vault.clear();
                Err(AuthError::SignedOut)
            }
            TokenOutcome::InvalidClient => {
                self.forget_client();
                let _ = self.vault.clear();
                Err(AuthError::SignedOut)
            }
            TokenOutcome::Other(e) => Err(AuthError::Server(e)),
        }
    }

    /// Save the new refresh token BEFORE the access token is published: the old one is spent.
    /// If the keychain write fails, the new token stays in memory as current (the spent one is
    /// never resubmitted), the access token is held back, and the error goes to the caller; the
    /// next `access_token()` retries the write first.
    fn accept(&self, reply: TokenReply) -> Result<(), AuthError> {
        let ttl = Duration::from_secs(reply.expires_in.unwrap_or(3600));
        let access = Access {
            token: reply.access_token,
            expires: Instant::now() + ttl,
        };
        if let Some(rt) = reply.refresh_token.as_deref() {
            if let Err(e) = self.vault.rotate(rt) {
                *self.held_access.lock().expect("access lock") = Some(access);
                return Err(e);
            }
        }
        *self.held_access.lock().expect("access lock") = None;
        *self.access.lock().expect("access lock") = Some(access);
        Ok(())
    }

    /// Tests answer from `token_stub` (and record each form) instead of the network.
    #[cfg(test)]
    async fn token_request(&self, form: &[(&str, &str)]) -> Result<TokenOutcome, AuthError> {
        self.token_forms.lock().expect("stub lock").push(
            form.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        if self.bump_during_request.load(Ordering::SeqCst) {
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
        match self.token_stub.lock().expect("stub lock").pop() {
            Some(o) => Ok(o),
            None => Err(AuthError::Network("no stubbed reply".into())),
        }
    }

    #[cfg(not(test))]
    async fn token_request(&self, form: &[(&str, &str)]) -> Result<TokenOutcome, AuthError> {
        self.token_request_http(form).await
    }

    #[cfg_attr(test, allow(dead_code))]
    async fn token_request_http(&self, form: &[(&str, &str)]) -> Result<TokenOutcome, AuthError> {
        let resp = self
            .http
            .post(self.base.join("/api/mcp/oauth/token"))
            .timeout(crate::keo::HTTP_TIMEOUT)
            .form(form)
            .send()
            .await
            .map_err(|e| AuthError::Network(e.without_url().to_string()))?;
        let status = resp.status();
        let body = resp
            .bytes()
            .await
            .map_err(|e| AuthError::Network(e.without_url().to_string()))?;
        if status.is_success() {
            return serde_json::from_slice::<TokenReply>(&body)
                .map(TokenOutcome::Ok)
                .map_err(|_| AuthError::Server("unreadable token reply".into()));
        }
        let err = serde_json::from_slice::<OAuthErrorReply>(&body).unwrap_or_default();
        Ok(token_error(&err.error, status.as_u16()))
    }

    /// The saved client id while a grant exists for it, else a new one from dynamic
    /// registration (a loopback redirect matches on any port). A fresh sign-in therefore always
    /// registers, so a client the server pruned cannot strand it.
    async fn client_id(&self, redirect_uri: &str) -> Result<String, AuthError> {
        let has_grant = matches!(self.vault.current(), Ok(Some(_)));
        if let Some(id) = reusable_client(self.store.get().client_id, has_grant) {
            return Ok(id);
        }
        let body = serde_json::json!({
            "client_name": "Keo Companion",
            "redirect_uris": [redirect_uri],
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "application_type": "native",
        });
        let resp = self
            .http
            .post(self.base.join("/api/mcp/oauth/register"))
            .timeout(crate::keo::HTTP_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(|e| AuthError::Network(e.without_url().to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(AuthError::Server(format!(
                "registration answered {}",
                status.as_u16()
            )));
        }
        let reply: RegisterReply = resp
            .json()
            .await
            .map_err(|_| AuthError::Server("unreadable registration reply".into()))?;
        self.store
            .update(|p| p.client_id = Some(reply.client_id.clone()))
            .map_err(|e| AuthError::Io(e.to_string()))?;
        tracing::info!("registered a new OAuth client");
        Ok(reply.client_id)
    }

    fn forget_client(&self) {
        let _ = self.store.update(|p| p.client_id = None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_verifier_char(c: char) -> bool {
        c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~')
    }

    /// A store whose writes can be made to fail.
    #[derive(Clone, Default)]
    struct FakeStore {
        saved: Arc<Mutex<Option<String>>>,
        fail_writes: Arc<Mutex<bool>>,
    }

    impl SecretStore for FakeStore {
        fn get(&self) -> Result<Option<String>, AuthError> {
            Ok(self.saved.lock().unwrap().clone())
        }
        fn set(&self, token: &str) -> Result<(), AuthError> {
            if *self.fail_writes.lock().unwrap() {
                return Err(AuthError::Keychain("locked".into()));
            }
            *self.saved.lock().unwrap() = Some(token.to_string());
            Ok(())
        }
        fn delete(&self) -> Result<(), AuthError> {
            *self.saved.lock().unwrap() = None;
            Ok(())
        }
    }

    #[test]
    fn rotation_survives_a_failed_keychain_write() {
        let fake = FakeStore::default();
        *fake.saved.lock().unwrap() = Some("spent-token".into());
        let vault = RefreshVault::new(Box::new(fake.clone()));
        assert_eq!(vault.current().unwrap().as_deref(), Some("spent-token"));

        *fake.fail_writes.lock().unwrap() = true;
        assert!(vault.rotate("fresh-token").is_err());
        // The spent token is never handed out again, though the keychain still holds it.
        assert_eq!(vault.current().unwrap().as_deref(), Some("fresh-token"));
        assert!(vault.unsaved());
        assert_eq!(fake.saved.lock().unwrap().as_deref(), Some("spent-token"));
        assert!(vault.flush().is_err());

        // The retried write sticks.
        *fake.fail_writes.lock().unwrap() = false;
        vault.flush().unwrap();
        assert_eq!(vault.current().unwrap().as_deref(), Some("fresh-token"));
        assert!(!vault.unsaved());
        assert_eq!(fake.saved.lock().unwrap().as_deref(), Some("fresh-token"));

        vault.clear().unwrap();
        assert_eq!(vault.current().unwrap(), None);
    }

    fn test_auth(fake: &FakeStore) -> (Auth, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::load(dir.path()));
        let auth = Auth::with_secret_store(
            BaseUrl::prod(),
            reqwest::Client::new(),
            store,
            Box::new(fake.clone()),
        );
        (auth, dir)
    }

    fn sent_refresh_tokens(auth: &Auth) -> Vec<String> {
        auth.token_forms
            .lock()
            .unwrap()
            .iter()
            .filter_map(|f| {
                f.iter()
                    .find(|(k, _)| k == "refresh_token")
                    .map(|(_, v)| v.clone())
            })
            .collect()
    }

    #[tokio::test]
    async fn sign_out_starts_a_new_sign_in_generation() {
        let fake = FakeStore::default();
        let (auth, _dir) = test_auth(&fake);
        let before = auth.generation();
        auth.sign_out().await.unwrap();
        assert!(auth.generation() > before);
    }

    #[test]
    fn a_token_left_by_a_failed_sign_out_is_deleted_at_start() {
        let fake = FakeStore::default();
        *fake.saved.lock().unwrap() = Some("left-token".into());
        let (auth, _dir) = test_auth(&fake);
        // Signed in: the client id is saved, so the token is kept.
        auth.store
            .update(|p| p.client_id = Some("client-1".into()))
            .unwrap();
        assert!(!auth.clear_leftover());
        assert!(auth.has_refresh_token());
        // state.json wiped by the sign-out, the keychain entry still there.
        auth.store.update(|p| p.client_id = None).unwrap();
        assert!(auth.clear_leftover());
        assert_eq!(*fake.saved.lock().unwrap(), None);
        assert!(!auth.has_refresh_token());
    }

    #[tokio::test]
    async fn refresh_saves_before_publishing_and_never_resubmits() {
        let fake = FakeStore::default();
        *fake.saved.lock().unwrap() = Some("spent-token".into());
        let (auth, _dir) = test_auth(&fake);
        auth.store
            .update(|p| p.client_id = Some("client-1".into()))
            .unwrap();
        auth.token_stub
            .lock()
            .unwrap()
            .push(TokenOutcome::Ok(reply("access-one", "fresh-token")));
        *fake.fail_writes.lock().unwrap() = true;
        // Through refresh(): the rotation's keychain write fails, so no access token.
        assert!(matches!(
            auth.access_token().await,
            Err(AuthError::Keychain(_))
        ));
        assert!(matches!(
            auth.access_token().await,
            Err(AuthError::Keychain(_))
        ));
        // Only one refresh went out, with the old token; the spent one is never sent again.
        assert_eq!(sent_refresh_tokens(&auth), vec!["spent-token".to_string()]);
        *fake.fail_writes.lock().unwrap() = false;
        assert_eq!(auth.access_token().await.unwrap(), "access-one");
        assert_eq!(sent_refresh_tokens(&auth).len(), 1);
        assert_eq!(fake.saved.lock().unwrap().as_deref(), Some("fresh-token"));
    }

    #[tokio::test]
    async fn relaunch_with_a_spent_token_signs_out_cleanly() {
        // The app quit before the rotated token was saved: the keychain holds the spent one.
        let fake = FakeStore::default();
        *fake.saved.lock().unwrap() = Some("spent-token".into());
        let (auth, _dir) = test_auth(&fake);
        auth.store
            .update(|p| p.client_id = Some("client-1".into()))
            .unwrap();
        auth.token_stub
            .lock()
            .unwrap()
            .push(TokenOutcome::InvalidGrant);
        assert!(matches!(
            auth.access_token().await,
            Err(AuthError::SignedOut)
        ));
        assert_eq!(*fake.saved.lock().unwrap(), None);
        assert!(!auth.has_refresh_token());
    }

    #[tokio::test]
    async fn a_refresh_overtaken_by_a_new_sign_in_changes_nothing() {
        for outcome in [
            TokenOutcome::Ok(reply("old-access", "old-rotated")),
            TokenOutcome::InvalidGrant,
        ] {
            let fake = FakeStore::default();
            *fake.saved.lock().unwrap() = Some("newer-account".into());
            let (auth, _dir) = test_auth(&fake);
            auth.store
                .update(|p| p.client_id = Some("client-1".into()))
                .unwrap();
            auth.token_stub.lock().unwrap().push(outcome);
            auth.bump_during_request.store(true, Ordering::SeqCst);
            assert!(matches!(
                auth.access_token().await,
                Err(AuthError::Superseded)
            ));
            // The late reply neither rotated nor cleared the newer account's token.
            assert_eq!(fake.saved.lock().unwrap().as_deref(), Some("newer-account"));
            assert!(auth.cached().is_none());
        }
    }

    #[tokio::test]
    async fn no_refresh_runs_during_a_browser_sign_in() {
        let fake = FakeStore::default();
        *fake.saved.lock().unwrap() = Some("saved".into());
        let (auth, _dir) = test_auth(&fake);
        auth.store
            .update(|p| p.client_id = Some("client-1".into()))
            .unwrap();
        auth.interactive.store(true, Ordering::SeqCst);
        assert!(matches!(
            auth.access_token().await,
            Err(AuthError::Superseded)
        ));
        assert!(sent_refresh_tokens(&auth).is_empty());
    }

    #[test]
    fn oauth_errors_reach_the_log_as_known_codes_only() {
        const SECRET: &str = "private_prompt_text";
        assert!(!callback_denied(SECRET).to_string().contains(SECRET));
        assert!(callback_denied("access_denied")
            .to_string()
            .contains("access_denied"));
        assert!(matches!(token_error(SECRET, 400), TokenOutcome::Other(ref e) if e == "other"));
        assert!(
            matches!(token_error("invalid_scope", 400), TokenOutcome::Other(ref e) if e == "invalid_scope")
        );
        assert!(matches!(
            token_error("invalid_grant", 400),
            TokenOutcome::InvalidGrant
        ));
    }

    fn reply(access: &str, refresh: &str) -> TokenReply {
        TokenReply {
            access_token: access.into(),
            expires_in: Some(3600),
            refresh_token: Some(refresh.into()),
        }
    }

    #[tokio::test]
    async fn access_token_only_after_the_refresh_token_is_saved() {
        let fake = FakeStore::default();
        *fake.saved.lock().unwrap() = Some("spent-token".into());
        let (auth, _dir) = test_auth(&fake);
        *fake.fail_writes.lock().unwrap() = true;
        assert!(matches!(
            auth.accept(reply("access-one", "fresh-token")),
            Err(AuthError::Keychain(_))
        ));
        // Not published, and asking again only retries the write (no network, no spent token).
        assert_eq!(auth.cached(), None);
        assert!(matches!(
            auth.access_token().await,
            Err(AuthError::Keychain(_))
        ));
        assert_eq!(fake.saved.lock().unwrap().as_deref(), Some("spent-token"));
        // Once the keychain takes the new token, the held access token is released.
        *fake.fail_writes.lock().unwrap() = false;
        assert_eq!(auth.access_token().await.unwrap(), "access-one");
        assert_eq!(fake.saved.lock().unwrap().as_deref(), Some("fresh-token"));
        // A normal rotation publishes at once.
        auth.accept(reply("access-two", "newer-token")).unwrap();
        assert_eq!(auth.cached().as_deref(), Some("access-two"));
    }

    #[test]
    fn cached_client_reused_only_with_a_grant() {
        assert_eq!(
            reusable_client(Some("c1".into()), true).as_deref(),
            Some("c1")
        );
        assert_eq!(reusable_client(Some("c1".into()), false), None);
        assert_eq!(reusable_client(None, true), None);
    }

    #[test]
    fn pkce_shape() {
        let p = new_pkce();
        assert!((43..=128).contains(&p.verifier.len()));
        assert!(p.verifier.chars().all(is_verifier_char));
        assert_eq!(p.challenge.len(), 43);
        assert!(p
            .challenge
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(new_pkce().verifier, p.verifier);
    }

    #[test]
    fn challenge_is_base64url_sha256() {
        // Computed independently: sha256 by hand-rolled hex, then base64url by the standard
        // alphabet with the two url-safe swaps and no padding.
        let verifier = "abc";
        let digest = Sha256::digest(verifier.as_bytes());
        let std_b64 = base64::engine::general_purpose::STANDARD.encode(digest);
        let expected = std_b64
            .trim_end_matches('=')
            .replace('+', "-")
            .replace('/', "_");
        assert_eq!(challenge_for(verifier), expected);
        // The sha256 of "abc" begins with the well-known bytes ba 78 16 bf.
        assert_eq!(&digest[..4], &[0xba, 0x78, 0x16, 0xbf]);
    }

    #[test]
    fn callback_parsing() {
        let p = parse_callback("/callback?code=c-1&state=s-1&iss=https%3A%2F%2Faskkeo.ai").unwrap();
        assert_eq!(p.code.as_deref(), Some("c-1"));
        assert_eq!(p.state.as_deref(), Some("s-1"));
        assert_eq!(p.iss.as_deref(), Some("https://askkeo.ai"));
        let e = parse_callback("/callback?error=access_denied&state=s").unwrap();
        assert_eq!(e.error.as_deref(), Some("access_denied"));
        assert!(parse_callback("/favicon.ico").is_none());
        assert!(parse_callback("/callbackx?code=1").is_none());
    }

    #[test]
    fn request_line() {
        assert_eq!(
            request_target("GET /callback?code=1 HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some("/callback?code=1")
        );
        assert_eq!(request_target("POST /callback HTTP/1.1\r\n"), None);
        assert_eq!(request_target(""), None);
    }

    #[tokio::test]
    async fn loopback_answers_and_returns_params() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(wait_for_callback(listener));
        // A stray request first, then the real one.
        for path in ["/favicon.ico", "/callback?code=abc&state=xyz"] {
            let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).await.unwrap();
            assert!(out.starts_with("HTTP/1.1 "));
        }
        let p = server.await.unwrap().unwrap();
        assert_eq!(p.code.as_deref(), Some("abc"));
        assert_eq!(p.state.as_deref(), Some("xyz"));
    }
}
