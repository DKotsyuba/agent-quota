# Read-only observation for machine consumers

Companion guidance, revision 1, 2026-10-20. This is an optional integration profile for an actual desktop or programmatic reader. It does not introduce a universal transport, shared runtime, observer daemon or automatic migration of existing products. Product contracts remain authoritative.

## Producer ownership and authority

The producer owns execution, storage, identity, authorization and retained observations. A consumer uses its documented API, typed MCP output or explicit CLI JSON; it does not open another product's database or parse human prose. Reuse an adequate existing endpoint before adding a service.

An observer grant permits only the named reads. Enforce that boundary in the producer, including legacy, private and administrative ingress paths. A native agent's assertion, membership or leader identity must not be fabricated for a desktop reader. An administrative credential is not a substitute for an absent observer grant. Credentials stay in protected producer/native configuration, outside renderer arguments, result fields, command lines and logs.

A read or refresh does not start an agent/model/check, replay admission, settle delivery, claim an outbox item, acquire a work session, migrate state, prune runtime files or restart a resident. Commands named doctor or status require a side-effect audit; names and annotations alone do not establish read-only behavior. Add an explicit safe mode when a legacy diagnostic also performs maintenance, preserving its existing callers.

## Observable contract

| Concern | Required integration meaning |
| --- | --- |
| Identity | Report product and package/runtime version separately. Capabilities describe the responding producer. An unknown deployed revision remains null or explicitly unavailable. |
| Versioning | Version the machine envelope and document open versus closed vocabularies. Test actual discovery and success/error output; a declared MCP outputSchema requires conforming structuredContent. |
| Errors | Distinguish missing, refused, unavailable, malformed, partial and empty-success results. Never replace a failure with an empty list, zero counter or invented success. Keep error codes safe and bounded. |
| Clocks | Preserve the producer's original observation/result time and units. Consumer receipt time is a separate clock. Missing timestamps remain unknown; refresh does not make a saved report current. |
| Coverage | State scope, omitted sources, unavailable probes, retention and any lower bound. A scanned subset is not a complete project or inventory. |
| Pagination | Return stable identities, explicit continuation and whether more rows remain. Advance only after included rows. Fence mutable pages by revision or declare the weaker observation guarantee. |
| Encoded bounds | Budget rows, content bytes and the complete escaped wire envelope separately, including framing. Do not cut final JSON, UTF-8, identifiers or cursors. |
| Large content | Bind chunks to object/execution identity and verified content digest. Validate byte offsets and completeness; a resumed execution cannot be mixed with an earlier answer. |
| Relationships | Join objects by confirmed source identity, canonical repository identity or validated working path. Matching display names do not establish identity. |
| Independent facts | Execution completion, answer availability, a consensus vote, decision acceptance, custody and native delivery remain distinct states. |

A versioned result can use an exclusive data/error envelope, or preserve an established product shape with explicit error routes. Document the exact variant; do not impose another wrapper on every existing tool. Preserve the supported legacy text result and isError behavior. A declared legacy presentation failure may retain valid structured read data only when the consumer surfaces that degradation explicitly; it is not general permission to ignore tool errors.

## Consumer lifecycle

The consumer has a closed read allowlist and validates external shapes before rendering. Source prose remains data, without HTML/script execution or authority. Demo data is an explicit development selection, never a fallback for a failed live read.

Bound connections, subprocesses, pages, output bytes and deadlines. Cancel obsolete view subscriptions, reject late generations and preserve the last good observation with its original date during a disconnect. Reconnect rediscovers capabilities. Stop observers when the application closes and reap only app-owned frontend/read children; disconnecting a reader never cancels agents or kills resident services.

## Evidence before integration acceptance

Exercise the real producer dispatch and native transport with isolated fixtures: authorized reads and every denied mutation route; more than one page without gaps; Unicode and encoded frame boundaries; resumed/changed content; stale and partial observations; malformed or unavailable sources; cancellation, EOF and owned-child cleanup. Check state before and after reads to prove no writes, inference, checks or hidden settlement. Include a positive authorized control for denial tests.

Local source gates, a desktop build, installed producer activation and live qualification are separate results. Preserve exact tested source/artifact identity and explicit omissions. No grant provisioning, service restart, release publication or installation is implied by this profile.
