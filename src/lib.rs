//! Reference FIFO/priority scheduler for the infernal-law kernel's
//! eligible-route/claim contract (ADR-0011, ILK-010/ILK-011): queries which
//! routes the kernel currently reports as eligible for this service's own
//! destination identity, and proposes a claim for the oldest one. Owns no
//! authoritative state of its own -- every signal it acts on comes from an
//! authenticated kernel read, and the kernel remains the sole arbiter of
//! whether a proposed claim actually succeeds.

pub mod claims;
pub mod error;
pub mod kernel_client;
pub mod routes;
pub mod scheduler;

use std::env;
use std::time::Duration;

use infernal_client::ClientCredential;
use uuid::Uuid;

use crate::error::TaskmasterError;
use crate::kernel_client::KernelClient;

const KERNEL_AUTHORITY_ENV: &str = "KERNEL_AUTHORITY";
const TASKMASTER_SERVICE_ID_ENV: &str = "TASKMASTER_SERVICE_ID";
const CLAIM_LEASE_SECONDS_ENV: &str = "CLAIM_LEASE_SECONDS";
const POLL_INTERVAL_SECONDS_ENV: &str = "POLL_INTERVAL_SECONDS";
const DEFAULT_LEASE_SECONDS: i64 = 300;
const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;

pub struct Config {
    pub client: KernelClient,
    pub lease_seconds: i64,
    pub poll_interval: Duration,
}

impl Config {
    /// `TASKMASTER_SERVICE_ID` names a `service_id` that must already be
    /// provisioned and enrolled with the kernel (an `identities` row, plus
    /// the real ADR-0008 Kubernetes-TokenReview enrollment for this
    /// process's freshly generated instance key) before any call this
    /// process signs will be accepted -- deployment configuration, not
    /// something this scaffold performs itself, the same way infernal-law
    /// itself treats grant/schema-activation provisioning.
    pub fn from_env() -> Result<Self, TaskmasterError> {
        let authority = env::var(KERNEL_AUTHORITY_ENV)
            .map_err(|_| TaskmasterError::MissingEnv(KERNEL_AUTHORITY_ENV))?;
        let service_id: Uuid = env::var(TASKMASTER_SERVICE_ID_ENV)
            .map_err(|_| TaskmasterError::MissingEnv(TASKMASTER_SERVICE_ID_ENV))?
            .parse()
            .map_err(|_| TaskmasterError::InvalidServiceId)?;
        let lease_seconds = env::var(CLAIM_LEASE_SECONDS_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_LEASE_SECONDS);
        let poll_interval_seconds = env::var(POLL_INTERVAL_SECONDS_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS);
        let credential = ClientCredential::generate(service_id);
        let client = KernelClient::new(credential, authority)?;
        Ok(Self {
            client,
            lease_seconds,
            poll_interval: Duration::from_secs(poll_interval_seconds),
        })
    }
}

/// Runs the scheduling loop forever: poll, propose, sleep, repeat. A
/// failed pass is logged and retried on the next tick rather than crashing
/// the process -- a transient kernel or network hiccup should not take a
/// scheduler down entirely, and the kernel's own claim arbitration is what
/// actually has to be correct, not this loop's uptime.
pub fn run(config: Config) -> ! {
    loop {
        match scheduler::schedule_once(&config.client, config.lease_seconds) {
            Ok(scheduler::ScheduleOutcome::NothingEligible) => {}
            Ok(scheduler::ScheduleOutcome::Proposed { route_id, outcome }) => {
                println!("proposed claim for route {route_id}: {outcome:?}");
            }
            Err(error) => eprintln!("scheduling pass failed: {error}"),
        }
        std::thread::sleep(config.poll_interval);
    }
}
