# Specifications

| Document | Purpose |
|---|---|
| [requirements.md](requirements.md) | Software Requirements Specification: scope, actors, functional and non-functional requirements with stable IDs (`FR-*`, `NFR-*`), constraints, acceptance criteria. |
| [design.md](design.md) | Software Design Document: modules, engine layer, pod lifecycle, data flows, state layout, security model, failure handling. |
| [test-plan.md](test-plan.md) | Test tiers, environments, unit and integration checks, manual tests, traceability to requirement IDs, release gate. |
| [plugins.md](plugins.md) | Plugins: manifests, hooks, the JSON lines protocol, address allocation, and `gantry` group access. |
| [certificates.md](certificates.md) | Proposal: pod certificates from the internal CA, their files and formats, renewal, and the issuer plugin. |

The design and test plan cite requirement IDs in brackets. A change to a
requirement updates all three documents in the same change.
