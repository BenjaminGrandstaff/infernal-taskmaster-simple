//! Reference FIFO/priority scheduler for the infernal-law kernel's
//! eligible-route/claim contract (ADR-0011, ILK-010/ILK-011): queries which
//! routes the kernel currently reports as eligible for this service's own
//! destination identity, and proposes a claim for the oldest one. Owns no
//! authoritative state of its own -- every signal it acts on comes from an
//! authenticated kernel read, and the kernel remains the sole arbiter of
//! whether a proposed claim actually succeeds.

pub mod claims;
pub mod error;
pub mod instance_lease;
pub mod kernel_client;
pub mod routes;
pub mod scheduler;

use std::env;
use std::time::Duration;

use infernal_client::ClientCredential;
use uuid::Uuid;

use crate::error::TaskmasterError;
use crate::instance_lease::RENEWAL_MARGIN_SECONDS;
use crate::kernel_client::KernelClient;

const KERNEL_AUTHORITY_ENV: &str = "KERNEL_AUTHORITY";
const TASKMASTER_SERVICE_ID_ENV: &str = "TASKMASTER_SERVICE_ID";
const CLAIM_LEASE_SECONDS_ENV: &str = "CLAIM_LEASE_SECONDS";
const POLL_INTERVAL_SECONDS_ENV: &str = "POLL_INTERVAL_SECONDS";
/// Path to a PEM-encoded certificate authority this process should trust
/// in addition to the default public root store, for a kernel reachable
/// only behind a private or self-signed CA. Optional: a kernel with an
/// ordinary publicly-trusted certificate needs no configuration here.
const KERNEL_CA_CERT_PATH_ENV: &str = "KERNEL_CA_CERT_PATH";
/// A base64url-encoded, 32-byte ADR-0008 enrollment challenge, from a
/// kernel operator's own out-of-band challenge issuance -- infernal-law has
/// no self-service HTTP call for requesting one (see
/// `infernal_client::EnrollmentSubmission`'s own documentation for why).
/// Optional: unset entirely if this process's identity was already
/// enrolled some other way (or does not need to be, for a kernel not
/// requiring ADR-0008 enrollment). When set, `SERVICE_ENDPOINT` and
/// `POD_UID` become required.
const ENROLLMENT_CHALLENGE_ENV: &str = "ENROLLMENT_CHALLENGE";
/// This process's own HTTPS endpoint, submitted as part of the enrollment
/// proof. This service has no inbound listener of its own (it only ever
/// makes outbound calls), so nothing currently connects to this address --
/// it is recorded by the kernel as instance metadata, not verified for
/// reachability at enrollment time.
const SERVICE_ENDPOINT_ENV: &str = "SERVICE_ENDPOINT";
/// This Pod's own UID, for example from the Kubernetes Downward API
/// (`fieldRef: metadata.uid`) -- must match the Pod UID Kubernetes binds to
/// the workload token at `WORKLOAD_TOKEN_PATH`.
const POD_UID_ENV: &str = "POD_UID";
/// Path to this Pod's own projected ServiceAccount token for the
/// `infernal-law-enrollment` audience.
const WORKLOAD_TOKEN_PATH_ENV: &str = "WORKLOAD_TOKEN_PATH";
const DEFAULT_WORKLOAD_TOKEN_PATH: &str = "/var/run/secrets/infernal-law-enrollment/token";
const DEFAULT_LEASE_SECONDS: i64 = 300;
const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;

pub struct Config {
    pub client: KernelClient,
    pub lease_seconds: i64,
    pub poll_interval: Duration,
    /// This process's own instance lease, tracked only when this process
    /// performed its own enrollment at startup (see `run`'s renewal
    /// logic). `None` when `ENROLLMENT_CHALLENGE` was unset because this
    /// identity was already enrolled some other way -- there is no way to
    /// discover another process's enrollment's current lease state after
    /// the fact, so such a process cannot renew and simply keeps today's
    /// behavior of failing once its lease expires.
    pub instance_lease: Option<InstanceLease>,
}

/// This process's own registration lease with the kernel, tracked entirely
/// client-side from the last enrollment or renewal response so `run` knows
/// when to renew next and what revision to renew with.
#[derive(Clone, Copy, Debug)]
pub struct InstanceLease {
    pub revision: i64,
    pub expires_at: i64,
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
        let client = match env::var(KERNEL_CA_CERT_PATH_ENV) {
            Ok(path) => {
                let pem = std::fs::read(&path).map_err(TaskmasterError::CaCertificateUnreadable)?;
                KernelClient::with_extra_root_certificate(credential, authority, &pem)?
            }
            Err(_) => KernelClient::new(credential, authority)?,
        };
        let mut instance_lease = None;
        if let Ok(challenge) = env::var(ENROLLMENT_CHALLENGE_ENV) {
            let endpoint = env::var(SERVICE_ENDPOINT_ENV)
                .map_err(|_| TaskmasterError::MissingEnv(SERVICE_ENDPOINT_ENV))?;
            let pod_uid =
                env::var(POD_UID_ENV).map_err(|_| TaskmasterError::MissingEnv(POD_UID_ENV))?;
            let token_path = env::var(WORKLOAD_TOKEN_PATH_ENV)
                .unwrap_or_else(|_| DEFAULT_WORKLOAD_TOKEN_PATH.to_owned());
            let workload_token = std::fs::read_to_string(&token_path)
                .map_err(TaskmasterError::EnrollmentTokenUnreadable)?
                .trim()
                .to_owned();
            let challenge = decode_challenge(&challenge)?;
            let enrolled = client.enroll(challenge, &endpoint, &pod_uid, workload_token)?;
            println!("enrolled with the kernel: {enrolled:?}");
            instance_lease = Some(InstanceLease {
                revision: enrolled.lease_revision,
                expires_at: enrolled.lease_expires_at,
            });
        }
        Ok(Self {
            client,
            lease_seconds,
            poll_interval: Duration::from_secs(poll_interval_seconds),
            instance_lease,
        })
    }
}

fn decode_challenge(
    value: &str,
) -> Result<[u8; infernal_client::CHALLENGE_LENGTH], TaskmasterError> {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| TaskmasterError::InvalidEnrollmentChallenge)?
        .try_into()
        .map_err(|_| TaskmasterError::InvalidEnrollmentChallenge)
}

/// Runs the scheduling loop forever: poll, propose, sleep, repeat. A
/// failed pass is logged and retried on the next tick rather than crashing
/// the process -- a transient kernel or network hiccup should not take a
/// scheduler down entirely, and the kernel's own claim arbitration is what
/// actually has to be correct, not this loop's uptime.
pub fn run(config: Config) -> ! {
    let Config {
        client,
        lease_seconds,
        poll_interval,
        mut instance_lease,
    } = config;
    loop {
        renew_lease_if_due(&client, &mut instance_lease);
        match scheduler::schedule_once(&client, lease_seconds) {
            Ok(scheduler::ScheduleOutcome::NothingEligible) => {}
            Ok(scheduler::ScheduleOutcome::Proposed { route_id, outcome }) => {
                println!("proposed claim for route {route_id}: {outcome:?}");
            }
            Err(error) => eprintln!("scheduling pass failed: {error}"),
        }
        std::thread::sleep(poll_interval);
    }
}

/// Renews this process's own instance lease well before the kernel's
/// grant expires -- see `InstanceLease`'s own documentation for why this
/// is only possible when this process performed its own enrollment at
/// startup. A failed renewal is logged and retried on the next tick, the
/// same tolerance `run`'s own scheduling-pass loop already has for a
/// transient kernel or network hiccup; if every attempt fails before the
/// lease actually expires, every subsequent signed call -- including the
/// next renewal attempt -- starts failing until this process restarts
/// and re-enrolls, exactly as it always has.
fn renew_lease_if_due(client: &KernelClient, instance_lease: &mut Option<InstanceLease>) {
    let Some(lease) = instance_lease else {
        return;
    };
    if unix_time() < lease.expires_at - RENEWAL_MARGIN_SECONDS {
        return;
    }
    match client.renew_lease(lease.revision) {
        Ok(renewed) => {
            lease.revision = renewed.lease_revision;
            lease.expires_at = renewed.lease_expires_at;
            println!("renewed instance lease: {renewed:?}");
        }
        Err(error) => eprintln!("instance lease renewal failed: {error}"),
    }
}

fn unix_time() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .unwrap_or(i64::MAX)
}
