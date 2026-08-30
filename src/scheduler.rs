//! Goal: implement the one scheduling policy this reference service
//! provides -- plain FIFO, no priority/placement/capacity awareness (that
//! belongs to a more specialized scheduler; see ADR-0011) -- over whatever
//! the kernel currently reports as eligible.

use crate::claims::ClaimOutcome;
use crate::error::TaskmasterError;
use crate::kernel_client::KernelPort;

/// What one scheduling pass did, for the caller (typically the main loop)
/// to log.
#[derive(Debug)]
pub enum ScheduleOutcome {
    /// No eligible route existed to propose a claim for.
    NothingEligible,
    /// A claim was proposed for `route_id`; `outcome` is the kernel's
    /// atomic arbitration result.
    Proposed {
        route_id: String,
        outcome: ClaimOutcome,
    },
}

/// Fetches the kernel's current eligible-route set and proposes a claim for
/// the oldest one (the kernel already returns them in creation order, so no
/// client-side sort is needed). A claim losing the race
/// (`ClaimOutcome::AlreadyClaimed`) is not an error -- another worker or
/// scheduler instance simply won first, exactly the outcome ILK-011's
/// fencing exists to make safe. The kernel remains the sole arbiter of
/// whether a proposal actually succeeds (ADR-0011); this function never
/// assumes a claim succeeded before the kernel confirms it.
pub fn schedule_once(
    port: &impl KernelPort,
    lease_seconds: i64,
) -> Result<ScheduleOutcome, TaskmasterError> {
    let routes = port.eligible_routes()?;
    let Some(route) = routes.into_iter().next() else {
        return Ok(ScheduleOutcome::NothingEligible);
    };
    let outcome = port.propose_claim(&route.route_id, lease_seconds)?;
    Ok(ScheduleOutcome::Proposed {
        route_id: route.route_id,
        outcome,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crate::claims::WorkClaim;
    use crate::routes::EligibleRoute;

    use super::*;

    #[derive(Default)]
    struct FakePort {
        routes: Vec<EligibleRoute>,
        claimed: Mutex<Vec<String>>,
        next_outcome: Option<ClaimOutcome>,
    }

    impl KernelPort for FakePort {
        fn eligible_routes(&self) -> Result<Vec<EligibleRoute>, TaskmasterError> {
            Ok(self.routes.clone())
        }

        fn propose_claim(
            &self,
            route_id: &str,
            _lease_seconds: i64,
        ) -> Result<ClaimOutcome, TaskmasterError> {
            self.claimed.lock().unwrap().push(route_id.to_owned());
            Ok(self
                .next_outcome
                .clone()
                .unwrap_or(ClaimOutcome::AlreadyClaimed))
        }
    }

    fn route(id: &str, created_at: i64) -> EligibleRoute {
        EligibleRoute {
            route_id: id.to_owned(),
            request_id: "request-1".to_owned(),
            subscription_id: "subscription-1".to_owned(),
            destination_service_id: "destination-1".to_owned(),
            created_at,
        }
    }

    #[test]
    fn does_nothing_when_no_route_is_eligible() {
        let port = FakePort::default();

        let outcome = schedule_once(&port, 300).unwrap();

        assert!(matches!(outcome, ScheduleOutcome::NothingEligible));
        assert!(port.claimed.lock().unwrap().is_empty());
    }

    #[test]
    fn proposes_a_claim_for_the_oldest_eligible_route() {
        let claim = WorkClaim {
            claim_id: "c1".to_owned(),
            route_id: "first".to_owned(),
            worker_service_id: "w1".to_owned(),
            worker_instance_id: "i1".to_owned(),
            fencing_token: 1,
            status: "active".to_owned(),
            claimed_at: 1,
            lease_expires_at: 301,
        };
        let port = FakePort {
            routes: vec![route("first", 1), route("second", 2)],
            claimed: Mutex::new(Vec::new()),
            next_outcome: Some(ClaimOutcome::Claimed(claim.clone())),
        };

        let outcome = schedule_once(&port, 300).unwrap();

        assert_eq!(port.claimed.lock().unwrap().as_slice(), ["first"]);
        match outcome {
            ScheduleOutcome::Proposed { route_id, outcome } => {
                assert_eq!(route_id, "first");
                assert_eq!(outcome, ClaimOutcome::Claimed(claim));
            }
            ScheduleOutcome::NothingEligible => panic!("expected a proposal"),
        }
    }

    #[test]
    fn a_lost_race_is_reported_not_treated_as_an_error() {
        let port = FakePort {
            routes: vec![route("first", 1)],
            claimed: Mutex::new(Vec::new()),
            next_outcome: Some(ClaimOutcome::AlreadyClaimed),
        };

        let outcome = schedule_once(&port, 300).unwrap();

        match outcome {
            ScheduleOutcome::Proposed { outcome, .. } => {
                assert_eq!(outcome, ClaimOutcome::AlreadyClaimed);
            }
            ScheduleOutcome::NothingEligible => panic!("expected a proposal"),
        }
    }
}
