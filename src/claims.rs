//! Goal: mirror the kernel's work-claim wire format
//! (`src/http/work_claim_dto.rs` in infernal-law) for the one operation
//! this scheduler performs -- proposing a claim -- and classify every
//! response the kernel's atomic claim arbitration can produce.

use serde::{Deserialize, Serialize};

use crate::error::TaskmasterError;

#[derive(Serialize)]
pub struct ClaimRequest {
    pub lease_seconds: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct WorkClaim {
    pub claim_id: String,
    pub route_id: String,
    pub worker_service_id: String,
    pub worker_instance_id: String,
    pub fencing_token: i64,
    pub status: String,
    pub claimed_at: i64,
    pub lease_expires_at: i64,
}

/// Every outcome the kernel's atomic claim arbitration
/// (`WorkClaimService::claim`, ILK-011) can produce for a proposal this
/// scheduler makes. `AlreadyClaimed` and `RouteNotFound` are not failures
/// of this scheduler -- they are the kernel correctly rejecting a proposal
/// that lost a race or targeted a route this service does not own; the
/// kernel remains the sole arbiter either way (ADR-0011).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaimOutcome {
    Claimed(WorkClaim),
    AlreadyClaimed,
    RouteNotFound,
}

pub fn parse_claim_response(status: u16, body: &[u8]) -> Result<ClaimOutcome, TaskmasterError> {
    match status {
        201 => {
            let claim: WorkClaim = serde_json::from_slice(body)
                .map_err(|error| TaskmasterError::MalformedResponse(error.to_string()))?;
            Ok(ClaimOutcome::Claimed(claim))
        }
        409 => Ok(ClaimOutcome::AlreadyClaimed),
        404 => Ok(ClaimOutcome::RouteNotFound),
        other => Err(TaskmasterError::UnexpectedStatus(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_successful_claim() {
        let body = br#"{"claim_id":"c1","route_id":"r1","worker_service_id":"w1","worker_instance_id":"i1","fencing_token":1,"status":"active","claimed_at":10,"lease_expires_at":310}"#;

        let outcome = parse_claim_response(201, body).unwrap();

        assert_eq!(
            outcome,
            ClaimOutcome::Claimed(WorkClaim {
                claim_id: "c1".to_owned(),
                route_id: "r1".to_owned(),
                worker_service_id: "w1".to_owned(),
                worker_instance_id: "i1".to_owned(),
                fencing_token: 1,
                status: "active".to_owned(),
                claimed_at: 10,
                lease_expires_at: 310,
            })
        );
    }

    #[test]
    fn classifies_a_lost_race_as_already_claimed_not_an_error() {
        assert_eq!(
            parse_claim_response(409, br#"{"code":"route_already_claimed"}"#).unwrap(),
            ClaimOutcome::AlreadyClaimed
        );
    }

    #[test]
    fn classifies_an_unowned_or_unknown_route_as_route_not_found() {
        assert_eq!(
            parse_claim_response(404, br#"{"code":"claim_not_found"}"#).unwrap(),
            ClaimOutcome::RouteNotFound
        );
    }

    #[test]
    fn surfaces_any_other_status_as_an_error() {
        assert!(matches!(
            parse_claim_response(503, b"{}"),
            Err(TaskmasterError::UnexpectedStatus(503))
        ));
    }

    #[test]
    fn rejects_a_malformed_success_body() {
        assert!(matches!(
            parse_claim_response(201, b"not json"),
            Err(TaskmasterError::MalformedResponse(_))
        ));
    }
}
