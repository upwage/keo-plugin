//! The doorbell: a long poll to Keo that answers `wait` or `ring`. No auth; it sends only the
//! machine id. A ring hands its target to the inbox, which pulls the task with auth.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use rand::Rng;
use tokio::sync::{mpsc, watch};

use crate::keo::BaseUrl;
use crate::state::{is_valid_id, Snapshot, Store};

const CLIENT_TIMEOUT: Duration = Duration::from_secs(70);
const BAD_REQUEST_PAUSE: Duration = Duration::from_secs(60);
/// A 429 from the doorbell carries no reliable Retry-After: always wait this long.
const RATE_LIMIT_PAUSE: Duration = Duration::from_secs(60);

/// Who a ring is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// An existing session.
    Session(String),
    /// A new session in this folder (decoded from base64).
    New { cwd: Option<String> },
    /// The ring named no target: pull by machine id.
    Machine,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Wait,
    Ring(Target),
}

/// Parse the text/plain answer.
pub fn parse_answer(body: &str) -> Option<Answer> {
    let mut lines = body.lines().map(str::trim).filter(|l| !l.is_empty());
    match lines.next()? {
        "wait" => Some(Answer::Wait),
        "ring" => Some(Answer::Ring(
            lines.next().map(parse_target).unwrap_or(Target::Machine),
        )),
        _ => None,
    }
}

fn parse_target(line: &str) -> Target {
    let mut target = None;
    let mut cwd = None;
    for (k, v) in url::form_urlencoded::parse(line.as_bytes()) {
        match k.as_ref() {
            "target" => target = Some(v.into_owned()),
            "cwd" => cwd = decode_b64(&v),
            _ => {}
        }
    }
    match target.as_deref() {
        Some("new") => Target::New { cwd },
        Some(id) if is_valid_id(id) => Target::Session(id.to_string()),
        _ => Target::Machine,
    }
}

/// The contract: UTF-8, base64url without padding. Anything else is not a folder.
fn decode_b64(s: &str) -> Option<String> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim())
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
}

/// Exponential backoff with full jitter: a random wait in [0, min(cap, base * 2^attempt)].
#[derive(Debug, Clone)]
pub struct Backoff {
    pub base: Duration,
    pub cap: Duration,
    attempt: u32,
}

impl Backoff {
    pub fn new(base: Duration, cap: Duration) -> Self {
        Self {
            base,
            cap,
            attempt: 0,
        }
    }

    pub fn ceiling(&self, attempt: u32) -> Duration {
        let factor = 1u64.checked_shl(attempt.min(30)).unwrap_or(u64::MAX);
        let ms = (self.base.as_millis() as u64).saturating_mul(factor);
        Duration::from_millis(ms).min(self.cap)
    }

    pub fn next_delay(&mut self, rng: &mut impl Rng) -> Duration {
        let ceiling = self.ceiling(self.attempt);
        self.attempt = self.attempt.saturating_add(1);
        Duration::from_millis(rng.gen_range(0..=ceiling.as_millis() as u64))
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PollError {
    RateLimited(Duration),
    BadRequest,
    Other(String),
}

/// The error a status means, if any (before the body is read).
fn status_error(status: u16) -> Option<PollError> {
    match status {
        429 => Some(PollError::RateLimited(RATE_LIMIT_PAUSE)),
        400 => Some(PollError::BadRequest),
        s if !(200..300).contains(&s) => Some(PollError::Other(format!("status {s}"))),
        _ => None,
    }
}

async fn poll_once(
    http: &reqwest::Client,
    url: &str,
    machine_id: &str,
) -> Result<Answer, PollError> {
    let resp = http
        .post(url)
        .timeout(CLIENT_TIMEOUT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("machine_id={machine_id}"))
        .send()
        .await
        .map_err(|e| PollError::Other(e.without_url().to_string()))?;
    let status = resp.status().as_u16();
    status_error(status).map_or(Ok(()), Err)?;
    if !(200..300).contains(&status) {
        return Err(PollError::Other(format!("status {status}")));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| PollError::Other(e.without_url().to_string()))?;
    parse_answer(&body).ok_or_else(|| PollError::Other("unknown answer".into()))
}

/// Resolve when the app leaves the polling states.
async fn stopped_polling(rx: &mut watch::Receiver<Snapshot>) {
    loop {
        if !rx.borrow_and_update().polls() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// The doorbell loop. Polls only while the snapshot polls (signed in, running, not paused).
pub async fn run(
    base: BaseUrl,
    http: reqwest::Client,
    store: Arc<Store>,
    mut state: watch::Receiver<Snapshot>,
    rings: mpsc::Sender<Target>,
) {
    tracing::info!(host = base.origin(), "doorbell loop starting");
    let url = base.doorbell();
    let mut backoff = Backoff::new(Duration::from_secs(2), Duration::from_secs(300));
    loop {
        if !state.borrow_and_update().polls() {
            tracing::info!("doorbell idle (not listening)");
            while !state.borrow_and_update().polls() {
                if state.changed().await.is_err() {
                    return;
                }
            }
            tracing::info!("doorbell polling");
        }
        // Read under the store lock with the state check, so a sign-out's wipe is never undone.
        let machine_id = match store.machine_id_if(|| state.borrow().polls()) {
            Ok(Some(id)) => id,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(error = %e, "no machine id; doorbell waits");
                tokio::time::sleep(BAD_REQUEST_PAUSE).await;
                continue;
            }
        };
        let result = tokio::select! {
            r = poll_once(&http, &url, &machine_id) => r,
            _ = stopped_polling(&mut state) => continue,
        };
        let pause = match result {
            Ok(Answer::Wait) => {
                tracing::info!("doorbell: wait");
                backoff.reset();
                None
            }
            Ok(Answer::Ring(target)) => {
                let kind = match &target {
                    Target::Session(_) => "session",
                    Target::New { .. } => "new",
                    Target::Machine => "machine",
                };
                tracing::info!(target = kind, "doorbell: ring");
                backoff.reset();
                if rings.send(target).await.is_err() {
                    return;
                }
                None
            }
            Err(PollError::RateLimited(wait)) => {
                tracing::info!(seconds = wait.as_secs(), "doorbell: rate limited");
                Some(wait)
            }
            Err(PollError::BadRequest) => {
                tracing::info!("doorbell: 400 (server not upgraded?); backing off");
                Some(BAD_REQUEST_PAUSE)
            }
            Err(PollError::Other(e)) => {
                let wait = backoff.next_delay(&mut rand::thread_rng());
                tracing::warn!(error = %e, wait_ms = wait.as_millis() as u64, "doorbell error");
                Some(wait)
            }
        };
        if let Some(wait) = pause {
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = stopped_polling(&mut state) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        assert_eq!(parse_answer("wait"), Some(Answer::Wait));
        assert_eq!(parse_answer("wait\n"), Some(Answer::Wait));
        assert_eq!(
            parse_answer("ring\ntarget=abc12345"),
            Some(Answer::Ring(Target::Session("abc12345".into())))
        );
        assert_eq!(parse_answer("ring"), Some(Answer::Ring(Target::Machine)));
        // An id of the wrong shape falls back to a machine pull.
        assert_eq!(
            parse_answer("ring\ntarget=bad id"),
            Some(Answer::Ring(Target::Machine))
        );
        assert_eq!(parse_answer("hello"), None);
        assert_eq!(parse_answer(""), None);
    }

    #[test]
    fn new_session_target_decodes_cwd() {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("/Users/me/work");
        assert_eq!(
            parse_answer(&format!("ring\r\ntarget=new&cwd={b64}")),
            Some(Answer::Ring(Target::New {
                cwd: Some("/Users/me/work".into())
            }))
        );
        // Padded or standard-alphabet base64 is not the contract.
        let padded = base64::engine::general_purpose::URL_SAFE.encode("/a b");
        assert!(padded.ends_with('='));
        let encoded: String = url::form_urlencoded::byte_serialize(padded.as_bytes()).collect();
        assert_eq!(
            parse_answer(&format!("ring\ntarget=new&cwd={encoded}")),
            Some(Answer::Ring(Target::New { cwd: None }))
        );
        // The url-safe alphabet ('-' and '_') decodes.
        let tricky = "/w/~~~?";
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tricky);
        assert!(b64.contains('-') || b64.contains('_'));
        assert_eq!(
            parse_answer(&format!("ring\ntarget=new&cwd={b64}")),
            Some(Answer::Ring(Target::New {
                cwd: Some(tricky.into())
            }))
        );
        assert_eq!(
            parse_answer("ring\ntarget=new"),
            Some(Answer::Ring(Target::New { cwd: None }))
        );
    }

    #[test]
    fn rate_limit_waits_sixty_seconds() {
        assert_eq!(
            status_error(429),
            Some(PollError::RateLimited(Duration::from_secs(60)))
        );
        assert_eq!(status_error(400), Some(PollError::BadRequest));
        assert!(matches!(status_error(503), Some(PollError::Other(_))));
        assert_eq!(status_error(200), None);
    }

    #[test]
    fn backoff_bounds() {
        let mut b = Backoff::new(Duration::from_secs(2), Duration::from_secs(300));
        assert_eq!(b.ceiling(0), Duration::from_secs(2));
        assert_eq!(b.ceiling(3), Duration::from_secs(16));
        assert_eq!(b.ceiling(20), Duration::from_secs(300));
        assert_eq!(b.ceiling(u32::MAX), Duration::from_secs(300));
        let mut rng = rand::thread_rng();
        for attempt in 0..50 {
            let d = b.next_delay(&mut rng);
            assert!(d <= b.ceiling(attempt));
        }
        b.reset();
        assert!(b.next_delay(&mut rng) <= Duration::from_secs(2));
    }
}
