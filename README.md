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

`infernal-taskmaster-simple` is the reference scheduler: plain FIFO/priority
ordering, no Kubernetes- or GPU-aware placement. It is an ordinary
authenticated kernel service principal with no elevated database access. It:

1. queries the kernel's eligible-route contract for a declared worker class;
2. picks the next route to run, in submission order (optionally weighted by
   priority once the kernel exposes one);
3. requests a claim from the kernel for that route and worker; and
4. lets the kernel's existing claim/lease/fencing rules be the final word —
   this service never assumes a claim succeeded until the kernel confirms it.

More specialized schedulers (Kubernetes capacity-aware, GPU-affinity-aware,
throughput-batching) are expected to be separate projects built against the
same kernel contract, for example `infernal-taskmaster-k8s` and
`infernal-taskmaster-gpu`.

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

## Status

Early scaffold. The kernel's eligible-route query and claim contracts (ILK-010
/ ILK-011) are not implemented yet on the kernel side, so this project has no
scheduling logic yet either. See the kernel's [minimum viable kernel
spec](https://github.com/BenjaminGrandstaff/infernal-law/blob/main/docs/architecture/minimum-viable-kernel.md)
for what's implemented versus pending.

## Development

```sh
cargo build
cargo test
```

## License

MIT. See [LICENSE](LICENSE).
