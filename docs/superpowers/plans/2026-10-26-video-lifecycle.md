# Video lifecycle qualification plan

Goal: qualify the existing 30-second covered-video lifecycle without changing rendering.
Spec: the requested follow-up to PR #1; Windows visual success must remain unclaimed until observed.

- Add exact-boundary, canceled-cover and repeated-cycle regression tests against the production timer.
- Add Windows process-handle assertions for release, idempotent cleanup, replacement and drop.
- Add an executable Windows desktop runner using a known source and existing daemon, scoped to its owned mpv processes. Record monotonic timestamps, ownership, windows and screenshots; fail process checks independently of visual review.
- Document fixture generation, prerequisites, execution, limitations, visual review and efficiency remeasurement.
- Run available local checks; use the repository Windows/Linux CI for native checks unavailable locally. Open a focused PR with evidence clearly separated.

Constraints: preserve all mpv rendering arguments and reveal policy; no timing sleeps in timer unit tests. Screenshots cannot prove absence of transient black frames. Never terminate unrelated mpv processes.
