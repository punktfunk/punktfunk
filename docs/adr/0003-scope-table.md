# 0003 — The scope table

Status: accepted, 2026-11-12.

## Context

What a management route does on a seat was answered in places that did not know of each other.
The seat proxy refused a list of first segments. A seat on a person's own Linux account refused
three route groups by prefix. The console kept its own list of what a seat may be asked, and read
the proxy's list out of the Rust source. A route added without a decision in each did whatever
fell out: the proxy let a seat answer for the desk's displays, the GPU preference and the client
logs. A lane for a person managing their own profile would have been one more list.

## Decision

1. **One row per operation.** `mgmt::scope::SCOPES` gives each operation of the API document one
   scope: Box, Host, ProfileData or ProfileExec (`CONTEXT.md`). It is spelled as the document
   spells it and matched segment by segment with `mgmt_routes::path_matches`. A path with a dot
   segment has no scope.
2. **Pinned to the document.** `mgmt::tests::routes` fails on an operation with no row, a row with
   no operation, a device route that runs code, and a Box route a device calls that is not on its
   short list. The document carries each row as `x-scope`.
3. **The gates read it.** The seat proxy passes a route only when it has a scope and that scope is
   not Box; a plugin UI's credential stays refused by name. A seat on a person's own Linux account
   refuses a write to a ProfileExec route. The console pins its password gates to `x-scope`.
4. **The door's tables stay.** `DOOR` and `DEVICE` in `mgmt::owner` answer which host process
   answers a call. The scope table answers whose the route is. Neither is derived from the other.

## Consequences

- A new route fails the build until someone decides whose it is.
- The proxy refuses the Box routes its list missed: the device exchange, client logs, the desk's
  display policy, the GPU preference and the browser plane's hash. The console called none of
  them through a seat.
- A seat on a person's own Linux account also refuses plugin registration and log ingest, a
  provider's library writes and a launch hold's release. Hooks keep their own refusal, on both
  systems.
- Rejected: deriving the proxy's rule from the door's tables (another question), and a mark on
  each handler (a handler nobody marked would pass).

Evidence: `design/seat-scoped-features.md` §2.6 and §4 decision 8, and
`design/impl/phase-0-scope-table.md`, in the planning repository.
