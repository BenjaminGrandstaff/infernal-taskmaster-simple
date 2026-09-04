//! Goal: give Kubernetes something to probe for a process that never
//! listens for anything else.
//!
//! This service's work loop only makes outbound calls, so a failure has no
//! inbound surface to show up on: a process that cannot enroll, or whose
//! instance lease has expired, keeps running and keeps logging, and
//! Kubernetes sees a healthy container. That is the worst shape a failure
//! can take -- indistinguishable from success on every dashboard.
//!
//! Readiness and liveness are both driven by one fact: when this process
//! last completed a scheduling pass against the kernel. A pass that fails
//! does not update it, so a service stuck on 401 or 403 goes stale and is
//! reported unhealthy.
//!
//! Liveness matters more than readiness here, and deliberately so. Nothing
//! routes traffic to this service, so being "not ready" changes nothing on
//! its own. But an expired instance lease cannot be renewed -- the kernel
//! rejects the renewal along with every other signed call -- and the only
//! recovery is to restart and enroll again. A liveness probe is what makes
//! that restart happen without a human.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::thread;

/// Shared record of when the work loop last succeeded. Zero means "not
/// yet" -- a freshly started process that has enrolled but not completed a
/// pass.
#[derive(Clone, Debug, Default)]
pub struct Heartbeat(Arc<AtomicI64>);

impl Heartbeat {
    pub fn new() -> Self {
        Self(Arc::new(AtomicI64::new(0)))
    }

    pub fn record_success(&self, at: i64) {
        self.0.store(at, Ordering::Relaxed);
    }

    pub fn last_success(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }

    /// Age in seconds, or `None` when nothing has succeeded yet.
    pub fn age(&self, now: i64) -> Option<i64> {
        match self.last_success() {
            0 => None,
            last => Some(now.saturating_sub(last)),
        }
    }
}

/// How stale the last successful pass may be before readiness fails.
pub const DEFAULT_READY_STALE_SECONDS: i64 = 30;
/// How stale it may be before liveness fails and Kubernetes restarts the
/// container. Deliberately much longer than readiness: a restart throws
/// away a working enrollment, so it must not be the response to a brief
/// kernel or network hiccup.
pub const DEFAULT_LIVE_STALE_SECONDS: i64 = 150;

pub struct Thresholds {
    pub ready_stale_seconds: i64,
    pub live_stale_seconds: i64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            ready_stale_seconds: DEFAULT_READY_STALE_SECONDS,
            live_stale_seconds: DEFAULT_LIVE_STALE_SECONDS,
        }
    }
}

/// Decides a probe result without doing any I/O, so the policy is testable
/// on its own.
pub fn evaluate(
    path: &str,
    heartbeat: &Heartbeat,
    thresholds: &Thresholds,
    now: i64,
) -> (&'static str, String) {
    match path {
        "/health/live" => match heartbeat.age(now) {
            // Never succeeded yet: still starting up or enrolling. Report
            // live so Kubernetes' own initialDelay governs startup rather
            // than this returning a restart-worthy failure immediately.
            None => ("200 OK", "starting\n".to_owned()),
            Some(age) if age <= thresholds.live_stale_seconds => ("200 OK", "ok\n".to_owned()),
            Some(age) => (
                "503 Service Unavailable",
                format!("no successful pass for {age}s\n"),
            ),
        },
        "/health/ready" => match heartbeat.age(now) {
            None => (
                "503 Service Unavailable",
                "no successful pass yet\n".to_owned(),
            ),
            Some(age) if age <= thresholds.ready_stale_seconds => ("200 OK", "ok\n".to_owned()),
            Some(age) => (
                "503 Service Unavailable",
                format!("last successful pass {age}s ago\n"),
            ),
        },
        _ => ("404 Not Found", "not found\n".to_owned()),
    }
}

pub fn serve(address: &str, heartbeat: Heartbeat, thresholds: Thresholds) -> std::io::Result<()> {
    let listener = TcpListener::bind(address)?;
    println!("taskmaster health endpoint listening on {address}");
    let thresholds = Arc::new(thresholds);
    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                let heartbeat = heartbeat.clone();
                let thresholds = Arc::clone(&thresholds);
                thread::spawn(move || {
                    let _ = handle_connection(stream, &heartbeat, &thresholds);
                });
            }
            Err(error) => eprintln!("health connection failed: {error}"),
        }
    }
    Ok(())
}

fn handle_connection(
    mut stream: TcpStream,
    heartbeat: &Heartbeat,
    thresholds: &Thresholds,
) -> std::io::Result<()> {
    let mut buffer = [0_u8; 1024];
    let read = stream.read(&mut buffer)?;
    let request = String::from_utf8_lossy(&buffer[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");
    let (status, body) = evaluate(path, heartbeat, thresholds, now_seconds());
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

pub fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thresholds() -> Thresholds {
        Thresholds::default()
    }

    #[test]
    fn a_process_that_has_never_succeeded_is_live_but_not_ready() {
        let heartbeat = Heartbeat::new();
        assert_eq!(
            evaluate("/health/live", &heartbeat, &thresholds(), 1_000).0,
            "200 OK"
        );
        assert_eq!(
            evaluate("/health/ready", &heartbeat, &thresholds(), 1_000).0,
            "503 Service Unavailable"
        );
    }

    #[test]
    fn a_recent_pass_is_ready_and_live() {
        let heartbeat = Heartbeat::new();
        heartbeat.record_success(1_000);
        assert_eq!(
            evaluate("/health/ready", &heartbeat, &thresholds(), 1_010).0,
            "200 OK"
        );
        assert_eq!(
            evaluate("/health/live", &heartbeat, &thresholds(), 1_010).0,
            "200 OK"
        );
    }

    /// The failure this whole module exists for: the loop is running and
    /// logging, but every pass is being rejected.
    #[test]
    fn a_stuck_loop_stops_being_ready_before_it_stops_being_live() {
        let heartbeat = Heartbeat::new();
        heartbeat.record_success(1_000);
        // Past readiness, not yet past liveness: reported unhealthy, not
        // yet restarted.
        assert_eq!(
            evaluate("/health/ready", &heartbeat, &thresholds(), 1_060).0,
            "503 Service Unavailable"
        );
        assert_eq!(
            evaluate("/health/live", &heartbeat, &thresholds(), 1_060).0,
            "200 OK"
        );
        // Past liveness: Kubernetes restarts it, which is what re-enrolls.
        assert_eq!(
            evaluate("/health/live", &heartbeat, &thresholds(), 1_200).0,
            "503 Service Unavailable"
        );
    }

    #[test]
    fn liveness_is_slacker_than_readiness_so_a_hiccup_never_restarts() {
        let defaults = Thresholds::default();
        assert!(defaults.live_stale_seconds > defaults.ready_stale_seconds);
    }

    #[test]
    fn unknown_paths_are_not_found() {
        let heartbeat = Heartbeat::new();
        assert_eq!(
            evaluate("/", &heartbeat, &thresholds(), 1_000).0,
            "404 Not Found"
        );
    }
}
