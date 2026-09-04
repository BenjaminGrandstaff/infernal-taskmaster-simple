//! Goal: implement the outbound signed calls this scheduler makes into the
//! kernel's eligible-route/claim contract (ADR-0011, ILK-010/ILK-011),
//! signing with this process's own long-lived instance credential -- the
//! mirror image of how the kernel itself signs its outbound call to a
//! policy evaluator
//! (`src/infrastructure/http_policy_evaluator.rs` in infernal-law).
//! Building a signed request is split from sending it, so the signing
//! logic is independently verifiable without a live kernel connection, and
//! the actual calls sit behind [`KernelPort`] so scheduling policy can be
//! proven against a fake.
//!
//! Per ADR-0013's design (mirrored here in the opposite direction): only
//! this service's outbound call is signed at the application layer. The
//! kernel's JSON response is trusted over the same HTTPS connection this
//! service itself opened, not by a second signature -- exactly like the
//! kernel trusts a policy evaluator's response.

use std::time::{SystemTime, UNIX_EPOCH};

use infernal_client::{
    CHALLENGE_LENGTH, Client, ClientCredential, EnrolledInstance, EnrollmentSubmission,
    RequestParts, SignedRequest,
};
use uuid::Uuid;

use crate::claims::{ClaimOutcome, ClaimRequest, parse_claim_response};
use crate::error::TaskmasterError;
use crate::instance_lease::{
    RENEW_INSTANCE_PATH, RenewedLease, parse_renewal_response, renewal_request_body,
};
use crate::routes::{ELIGIBLE_ROUTES_PATH, EligibleRoute, parse_eligible_routes};

const SIGNATURE_VALIDITY_SECONDS: i64 = 30;

/// The kernel operations this scheduler needs -- an interface boundary so
/// [`crate::scheduler::schedule_once`] can be proven against a fake, the
/// same way infernal-law's own `PolicyEvaluator` trait separates
/// `AuthorityService` from a specific transport.
pub trait KernelPort {
    fn eligible_routes(&self) -> Result<Vec<EligibleRoute>, TaskmasterError>;

    fn propose_claim(
        &self,
        route_id: &str,
        lease_seconds: i64,
    ) -> Result<ClaimOutcome, TaskmasterError>;
}

pub struct KernelClient {
    client: Client,
    credential: ClientCredential,
    authority: String,
}

impl KernelClient {
    /// `authority` is the kernel's host (and, if needed, port), for example
    /// `kernel.example.test` -- the same shape as an HTTP `Host` header,
    /// never including a scheme or path.
    pub fn new(
        credential: ClientCredential,
        authority: impl Into<String>,
    ) -> Result<Self, TaskmasterError> {
        Ok(Self {
            client: Client::new()?,
            credential,
            authority: authority.into(),
        })
    }

    /// Like [`KernelClient::new`], but additionally trusts
    /// `extra_root_certificate_pem` -- for a kernel reachable only behind
    /// a private or self-signed certificate authority (for example a
    /// TLS-terminating sidecar inside a private cluster network), which
    /// this crate's default public root store would otherwise reject.
    pub fn with_extra_root_certificate(
        credential: ClientCredential,
        authority: impl Into<String>,
        extra_root_certificate_pem: &[u8],
    ) -> Result<Self, TaskmasterError> {
        Ok(Self {
            client: Client::with_extra_root_certificate(extra_root_certificate_pem)?,
            credential,
            authority: authority.into(),
        })
    }

    /// Performs ADR-0008 initial enrollment: signs a proof binding
    /// `challenge` to this process's own credential and submits it to
    /// `POST /v1/enrollments`. `challenge` comes from a kernel operator's
    /// own out-of-band challenge issuance -- there is no self-service call
    /// for requesting one (see `infernal_client::EnrollmentSubmission`'s
    /// own documentation for why). Must be called with the very credential
    /// this `KernelClient` will go on to sign ordinary requests with:
    /// enrollment registers a specific public key, not a service identity
    /// in the abstract.
    /// Asks the kernel to issue this workload its own enrollment challenge.
    /// Used when `ENROLLMENT_CHALLENGE` is unset, which is the normal case:
    /// a challenge is single-use, so an injected one survives only the
    /// first Pod of a Deployment revision.
    pub fn request_challenge(
        &self,
        pod_uid: &str,
        workload_token: &str,
    ) -> Result<[u8; CHALLENGE_LENGTH], TaskmasterError> {
        let issued = self.client.request_enrollment_challenge(
            &format!("https://{}", self.authority),
            pod_uid,
            workload_token,
        )?;
        Ok(issued.challenge_bytes()?)
    }

    pub fn enroll(
        &self,
        challenge: [u8; CHALLENGE_LENGTH],
        endpoint: &str,
        pod_uid: &str,
        workload_token: String,
    ) -> Result<EnrolledInstance, TaskmasterError> {
        let submission = EnrollmentSubmission::sign(
            &self.credential,
            challenge,
            endpoint,
            pod_uid,
            workload_token,
        )?;
        Ok(self
            .client
            .submit_enrollment(&format!("https://{}", self.authority), &submission)?)
    }

    /// Extends this instance's own registration lease before the kernel's
    /// default 60-second grant expires -- the kernel has no way to renew a
    /// lease that has *already* expired (every signed call, including
    /// this one, is rejected once that happens), so `lib.rs`'s `run` calls
    /// this proactively, well before the deadline. `expected_revision`
    /// must be this instance's current `lease_revision`, from the last
    /// enrollment or renewal -- a stale value is rejected rather than
    /// silently accepted, the same optimistic-concurrency guard the
    /// kernel's own work-claim leases use.
    pub fn renew_lease(&self, expected_revision: i64) -> Result<RenewedLease, TaskmasterError> {
        let signed = build_renewal_request(
            &self.credential,
            &self.authority,
            expected_revision,
            Uuid::new_v4(),
            unix_time(),
        )?;
        let response = self.client.send(&signed)?;
        parse_renewal_response(response.status, &response.body)
    }
}

impl KernelPort for KernelClient {
    fn eligible_routes(&self) -> Result<Vec<EligibleRoute>, TaskmasterError> {
        let signed = build_eligible_routes_request(
            &self.credential,
            &self.authority,
            Uuid::new_v4(),
            unix_time(),
        )?;
        let response = self.client.send(&signed)?;
        parse_eligible_routes(response.status, &response.body)
    }

    fn propose_claim(
        &self,
        route_id: &str,
        lease_seconds: i64,
    ) -> Result<ClaimOutcome, TaskmasterError> {
        let signed = build_claim_request(
            &self.credential,
            &self.authority,
            route_id,
            lease_seconds,
            Uuid::new_v4(),
            unix_time(),
        )?;
        let response = self.client.send(&signed)?;
        parse_claim_response(response.status, &response.body)
    }
}

fn build_eligible_routes_request(
    credential: &ClientCredential,
    authority: &str,
    request_id: Uuid,
    now: i64,
) -> Result<SignedRequest, TaskmasterError> {
    let parts = RequestParts::new(
        "GET",
        authority,
        ELIGIBLE_ROUTES_PATH,
        "application/json",
        &[],
        request_id,
    )?;
    sign(credential, parts, now)
}

fn build_claim_request(
    credential: &ClientCredential,
    authority: &str,
    route_id: &str,
    lease_seconds: i64,
    request_id: Uuid,
    now: i64,
) -> Result<SignedRequest, TaskmasterError> {
    let body = serde_json::to_vec(&ClaimRequest { lease_seconds })
        .map_err(|error| TaskmasterError::MalformedResponse(error.to_string()))?;
    let path = format!("/v1/routes/{route_id}/claims");
    let parts = RequestParts::new(
        "POST",
        authority,
        &path,
        "application/json",
        &body,
        request_id,
    )?;
    sign(credential, parts, now)
}

fn build_renewal_request(
    credential: &ClientCredential,
    authority: &str,
    expected_revision: i64,
    request_id: Uuid,
    now: i64,
) -> Result<SignedRequest, TaskmasterError> {
    let body = renewal_request_body(expected_revision);
    let parts = RequestParts::new(
        "POST",
        authority,
        RENEW_INSTANCE_PATH,
        "application/json",
        &body,
        request_id,
    )?;
    sign(credential, parts, now)
}

fn sign(
    credential: &ClientCredential,
    parts: RequestParts,
    now: i64,
) -> Result<SignedRequest, TaskmasterError> {
    let nonce = infernal_client::generate_nonce()?;
    Ok(SignedRequest::sign(
        parts,
        credential,
        now,
        now + SIGNATURE_VALIDITY_SECONDS,
        &nonce,
    )?)
}

fn unix_time() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use infernal_client::{IncomingRequest, verify_incoming};

    use super::*;

    fn incoming_from(signed: &SignedRequest) -> IncomingRequest {
        IncomingRequest::from_wire(
            signed.parts().clone(),
            &signed.service_id().to_string(),
            &signed.instance_id().to_string(),
            signed.content_digest(),
            signed.signature_input(),
            signed.signature(),
        )
        .unwrap()
    }

    #[test]
    fn the_eligible_routes_request_verifies_under_its_own_public_key() {
        let credential = ClientCredential::generate(Uuid::new_v4());

        let signed = build_eligible_routes_request(
            &credential,
            "kernel.example.test",
            Uuid::new_v4(),
            1_000,
        )
        .unwrap();

        assert_eq!(signed.parts().method(), "GET");
        assert_eq!(signed.parts().path_and_query(), ELIGIBLE_ROUTES_PATH);
        assert!(signed.parts().body().is_empty());
        let verified =
            verify_incoming(&incoming_from(&signed), credential.public_key(), 1_005).unwrap();
        assert_eq!(verified.service_id(), credential.public_key().service_id());
    }

    #[test]
    fn the_renewal_request_targets_the_instances_path_and_carries_the_expected_revision() {
        let credential = ClientCredential::generate(Uuid::new_v4());

        let signed =
            build_renewal_request(&credential, "kernel.example.test", 3, Uuid::new_v4(), 1_000)
                .unwrap();

        assert_eq!(signed.parts().method(), "POST");
        assert_eq!(signed.parts().path_and_query(), RENEW_INSTANCE_PATH);
        assert_eq!(signed.parts().body(), br#"{"expected_revision":3}"#);
        verify_incoming(&incoming_from(&signed), credential.public_key(), 1_005).unwrap();
    }

    #[test]
    fn the_claim_request_targets_the_right_route_and_carries_the_lease() {
        let credential = ClientCredential::generate(Uuid::new_v4());

        let signed = build_claim_request(
            &credential,
            "kernel.example.test",
            "route-42",
            300,
            Uuid::new_v4(),
            1_000,
        )
        .unwrap();

        assert_eq!(signed.parts().method(), "POST");
        assert_eq!(
            signed.parts().path_and_query(),
            "/v1/routes/route-42/claims"
        );
        assert_eq!(signed.parts().body(), br#"{"lease_seconds":300}"#);
        let verified =
            verify_incoming(&incoming_from(&signed), credential.public_key(), 1_005).unwrap();
        assert_eq!(verified.service_id(), credential.public_key().service_id());
    }

    #[test]
    fn a_tampered_claim_body_fails_verification() {
        let credential = ClientCredential::generate(Uuid::new_v4());
        let signed = build_claim_request(
            &credential,
            "kernel.example.test",
            "route-42",
            300,
            Uuid::new_v4(),
            1_000,
        )
        .unwrap();
        let tampered_parts = RequestParts::new(
            signed.parts().method(),
            signed.parts().authority(),
            signed.parts().path_and_query(),
            signed.parts().content_type(),
            br#"{"lease_seconds":999999}"#,
            signed.parts().request_id(),
        )
        .unwrap();
        let tampered = IncomingRequest::from_wire(
            tampered_parts,
            &signed.service_id().to_string(),
            &signed.instance_id().to_string(),
            signed.content_digest(),
            signed.signature_input(),
            signed.signature(),
        )
        .unwrap();

        assert!(verify_incoming(&tampered, credential.public_key(), 1_005).is_err());
    }
}
