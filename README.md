# infernal-taskmaster-simple

A minimal FIFO/priority scheduler service for the
[infernal-law](https://github.com/BenjaminGrandstaff/infernal-law) governance
kernel.

## What this is

The infernal-law kernel owns correctness: durable requests, routes,
subscriptions, authorization, claim/lease/fencing, idempotency, and audit. It
deliberately does not decide which eligible unit of work runs next, on which
worker, or when — that is scheduling policy, and it lives here instead. See
[ADR-0011: Move scheduling policy to an external scheduler
service](https://github.com/BenjaminGrandstaff/infernal-law/blob/main/docs/architecture/decisions/0011-move-scheduling-policy-outside-the-kernel.md)
for the full reasoning and the kernel/scheduler boundary this project
implements against.

`infernal-taskmaster-simple` is the reference scheduler: plain FIFO ordering,
no Kubernetes- or GPU-aware placement. It is an ordinary authenticated kernel
service principal with no elevated database access. It:

1. calls `GET /v1/routes/eligible`, which returns every route currently
   assigned to this service's own verified identity that has no live,
   unexpired claim — the kernel already returns them in `(created_at,
   route_id)` order, so the first entry is the oldest eligible route and no
   client-side sort is needed;
2. proposes a claim for that route via `POST /v1/routes/{route_id}/claims`;
   and
3. lets the kernel's existing claim/lease/fencing rules be the final word —
   this service never assumes a claim succeeded until the kernel confirms
   it, and a `409` (another worker or scheduler instance claimed it first)
   is logged, not treated as an error.

The kernel does not yet expose a separate worker-class or capability
declaration distinct from destination identity — a route's own destination
service *is* the worker class for now (see the kernel's own [minimum viable
kernel
spec](https://github.com/BenjaminGrandstaff/infernal-law/blob/main/docs/architecture/minimum-viable-kernel.md),
Section 8). More specialized schedulers (Kubernetes capacity-aware,
GPU-affinity-aware, throughput-batching) are expected to be separate
projects built against the same kernel contract, for example
`infernal-taskmaster-k8s` and `infernal-taskmaster-gpu`.

## Data source

**infernal-law is this project's only source of registered state.** Every
signal this scheduler acts on — eligible routes, relayed worker health/capacity
observations, and claim/renewal/completion outcomes — comes exclusively from
authenticated infernal-law kernel contracts. This project:

- never talks to a worker service directly (workers only ever talk to the
  kernel, and a worker's health/capacity report reaches the kernel, not this
  scheduler, directly);
- never reads Kubernetes objects, a separate message bus, or any other event
  source to decide what is eligible or claimed; and
- keeps no independently populated database of its own — any local cache
  exists only as a disposable, kernel-sourced read replica.

This preserves the kernel's zero-trust model (services communicate with the
kernel, not with one another) for the scheduler exactly as for any worker. See
[ADR-0011](https://github.com/BenjaminGrandstaff/infernal-law/blob/main/docs/architecture/decisions/0011-move-scheduling-policy-outside-the-kernel.md#the-kernel-is-the-schedulers-only-source-of-state).

## Protocol

Every call this service makes into the kernel is signed with its own
long-lived instance credential
([ADR-0003](https://github.com/BenjaminGrandstaff/infernal-law/blob/main/docs/architecture/decisions/0003-direct-signed-service-rest.md),
[ADR-0005](https://github.com/BenjaminGrandstaff/infernal-law/blob/main/docs/architecture/decisions/0005-use-ephemeral-per-instance-service-keys.md))
using [`infernal-client-rs`](https://github.com/BenjaminGrandstaff/infernal-client-rs)'s
`SignedRequest::sign`. Per the same "only the outbound call is signed"
design [ADR-0013](https://github.com/BenjaminGrandstaff/infernal-law/blob/main/docs/architecture/decisions/0013-external-stateless-policy-evaluator-for-authority.md)
uses for the kernel's own call to a policy evaluator: the kernel's JSON
response is trusted over the same HTTPS connection this service itself
opened, not by a second signature. `src/kernel_client.rs` splits building a
signed request from sending it, so the signing logic is independently
verified (against `infernal-client-rs`'s own `verify_incoming`) without a
live kernel connection; `src/scheduler.rs`'s FIFO policy sits behind a
`KernelPort` trait so it is proven against a fake kernel, not a real one.

## Configuration

- `KERNEL_AUTHORITY` (required) — the kernel's host (and, if needed, port),
  for example `kernel.example.test`. Never a scheme or path; this is also
  the actual address `infernal-client-rs` connects to (always over HTTPS).
- `TASKMASTER_SERVICE_ID` (required) — this service's own `service_id`, as
  a UUID. Must already be provisioned as an `identities` row and enrolled
  with the kernel (ADR-0008) before any call this process signs will be
  accepted — deployment configuration, not something this scaffold performs
  itself, the same way `infernal-law` treats grant/schema-activation
  provisioning as out-of-band.
- `CLAIM_LEASE_SECONDS` (default `300`) — the lease duration proposed with
  each claim.
- `POLL_INTERVAL_SECONDS` (default `5`) — how often to poll
  `GET /v1/routes/eligible`.

## Status

The eligible-route query and claim contracts (ILK-010/ILK-011) are
implemented kernel-side, and this service's client, FIFO scheduling policy,
and wire formats are implemented and tested against them: `src/routes.rs`
and `src/claims.rs` mirror the kernel's actual JSON shapes field-for-field,
`src/kernel_client.rs`'s signed requests are proven correct without a live
connection, and `src/scheduler.rs`'s FIFO policy is proven against a fake
kernel port, including that a lost claim race is reported, not treated as
an error. Not yet exercised: an actual signed round trip against a live,
enrolled kernel process (this requires real ADR-0008 Kubernetes-TokenReview
enrollment and a reachable HTTPS kernel, neither of which a unit-test
sandbox can provide) and wiring a reference worker on the other end of a
claimed route.

## Development

```sh
cargo build
cargo test
```

## License

MIT. See [LICENSE](LICENSE).
