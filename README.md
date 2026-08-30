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
- `KERNEL_CA_CERT_PATH` (optional) — path to a PEM-encoded certificate
  authority to trust in addition to the default public root store, for a
  kernel reachable only behind a private or self-signed certificate (for
  example `infernal-law`'s own TLS-terminating sidecar in a local or test
  cluster — see that repo's README). Omit for a kernel with an ordinary
  publicly-trusted certificate.

## Status

The eligible-route query and claim contracts (ILK-010/ILK-011) are
implemented kernel-side, and this service's client, FIFO scheduling policy,
and wire formats are implemented and tested against them: `src/routes.rs`
and `src/claims.rs` mirror the kernel's actual JSON shapes field-for-field,
`src/kernel_client.rs`'s signed requests are proven correct without a live
connection, and `src/scheduler.rs`'s FIFO policy is proven against a fake
kernel port, including that a lost claim race is reported, not treated as
an error.

Deployed into a real Kubernetes cluster alongside `infernal-law` and
`infernal-inquisitor-simple` and confirmed to complete a real signed
HTTPS call to the kernel end to end: with `KERNEL_CA_CERT_PATH` pointed at
`infernal-law`'s TLS-terminating sidecar certificate (see that repo's
README), this service's request reaches the kernel, passes signature
verification, and receives the kernel's correct, well-formed `401` for an
identity that is not yet enrolled — not a transport or TLS error. The only
remaining gap before a full round trip is genuine infrastructure, not
code: real ADR-0008 Kubernetes TokenReview enrollment for this service's
identity has not been performed in this test cluster.

## Development

```sh
cargo build
cargo test
```

## Podman

```sh
podman build -t localhost/infernal-taskmaster-simple:latest .
podman run --rm --network infernal-law \
  --env KERNEL_AUTHORITY='infernal-law' \
  --env TASKMASTER_SERVICE_ID='00000000-0000-4000-8000-000000000002' \
  localhost/infernal-taskmaster-simple:latest
```

Join it to the same Podman network as a locally running `infernal-law` (see
that repo's own `README.md`) to reach it by container name. If that
kernel's own TLS-terminating layer uses a self-signed certificate, also
set `KERNEL_CA_CERT_PATH` to a mounted copy of it — otherwise the call
fails at the TLS layer rather than reaching the kernel at all.

## Kubernetes

The base manifests are in [`k8s/base`](k8s/base). Preview or apply them with
the Kustomize support built into `kubectl`:

```sh
kubectl kustomize k8s/base
kubectl apply -k k8s/base
```

There is no `Service`: this process only ever makes outbound calls, so it
has nothing to be reached on. `KERNEL_AUTHORITY` and `TASKMASTER_SERVICE_ID`
in [`k8s/base/deployment.yaml`](k8s/base/deployment.yaml) default to
`infernal-law`'s own in-cluster `Service` name and a placeholder service
ID — adjust both for your deployment, and provision/enroll that service ID
with the kernel out of band (ADR-0008) before expecting a call to succeed.
No Kubernetes RBAC is needed here: this service never calls the Kubernetes
API itself.

## License

MIT. See [LICENSE](LICENSE).
