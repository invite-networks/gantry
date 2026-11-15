# Gantry documentation

| Area | Contents | Status |
|---|---|---|
| [spec/](spec/README.md) | Requirements (SRS), design (SDD) and test plan | Draft for review |
| [runbooks/](runbooks/README.md) | Install, operation, recovery, upgrade | Draft, partly validated |
| [compose-spec.md](compose-spec.md) | The project file format | Current |
| [DECISIONS.md](../DECISIONS.md) | Design decisions and open assumptions (serves as the decision log) | Current |

The INVITE documentation baseline also calls for `uml/`, `adr/`, `dev/` and
`guides/`. They are not written yet. Until then, the diagrams live in the
[design](spec/design.md), decisions in [DECISIONS.md](../DECISIONS.md),
developer setup in the [README](../README.md#development) and the user guide
in the [README](../README.md) and [compose-spec.md](compose-spec.md).

## Maintenance

* Change the requirements and design before code that changes behavior.
* Keep requirement IDs stable. Retire an ID rather than reuse it.
* Update the test plan's traceability table when tests are added or
  renamed.
* Runbook steps stay marked as planned until someone other than the author
  has run them on a test host.

## Review record

| Revision | Reviewer | Outcome |
|---|---|---|
| Initial draft | Not yet reviewed | Pending |
