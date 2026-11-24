# Security boundary

<!-- Replace this section with the product's actual boundary before
implementing mutations: authorized roots/tenants, subjects, side effects and a
real reporting channel. -->

The product exposes one read-only identity tool. It performs no filesystem
mutation, external SaaS access, daemon, or credential handling. Describe the
actual authorized roots, subjects and side effects before implementing
mutations. Tool annotations and caller-supplied actor labels are not
authentication.

The one side effect a socket client can request is the redemption of a banked
reset credit. The service refuses it for every peer UID not named by `serve
--allow-redeem-uid`, and it is never an MCP tool. That check lives in the
service process: the service OS identity itself, and root, are trusted to spend
the reset credits of every account configured with `[sources.redeem]`, because
the root-managed launcher executes whatever request the service hands out. The
service identity still cannot read a credential or alter the provider call,
which runs under the credential owner's UID. The credential owner limits
executions to one per account in 120 seconds on its own side; that bounds the
rate of spending, not the total. See
[docs/api.md](docs/api.md#reset-credit-redemption) and
[docs/linux-owner-collection.md](docs/linux-owner-collection.md#reset-credit-redemption).

Do not publish secrets or raw user content in logs, snapshots or evidence.
Report security concerns privately to the repository owner; establish a real
reporting channel before public distribution.
