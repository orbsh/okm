# 0073 — The least-fitting engine is the fixture, not the backend

Status: decided 2026-11-20, in force as the current test arrangement.

## Context

ADR-0010 chose slatedb for a deployment shape that no longer exists: an engine whose SSTs live in an object store, with its own WAL and its own flush cadence. Storage is now local paths only, so slatedb has no production role, and the obvious move is to delete it with the shape.

That would leave `VirtualStorage` tested against fjall and redb alone — two embedded engines that agree about almost everything: local files, synchronous API, prefix-compressed blocks, a range scan that a test can write without thinking. A contract tested only against engines that agree has not been tested, it has been illustrated.

slatedb is the disagreement. It is asynchronous behind a sync bridge (`SlatedbSync` drives a runtime on a thread and answers one reply per call), its persistence is an object store with its own WAL and flush interval, it compacts on its own schedule, and it hands back an owned scan iterator that core can only carry as `ScanIter::External`. The frictions are measured, not theoretical: a `put` that awaits durability waits for the next flush, which is 100 ms by default — 101 ms per put, flat, independent of size (ADR-0059).

## The decision

1. **slatedb stays, as a conformance fixture.** It is not a supported deployment backend and is not advertised as one: a build that deploys okm does not enable the feature. What it is for is the opposite of deployment — being the engine that fails when a test assumes fjall.
2. **It stays on the matrix as its own arm.** `TestStore::matrix()` keeps `slatedb-mem`, and CI's `--all-features` job runs it. A fixture nobody runs is decoration.
3. **The cheap default is off it.** `TestStore::default()` is the ephemeral engine — the one with no runtime and no async bridge — so the ordinary case stays cheap and slatedb's arm is explicit. `slatedb-mem` is reached by asking for it or by running the matrix; nothing starts on it.
4. **Tests are written against the trait, not against the fixture.** The arm is evidence only while the suite asks `VirtualStorage` questions; a test that special-cases slatedb turns the control into decoration.

## What follows from it

- `okm-core`'s dev-dependency keeps `slatedb` enabled, and that is the price of the control: the test build carries tokio. The shipped build does not — the CI gate that refuses `tokio` and `slatedb` in `okm-core/src` stays.
- The README's engine list says what slatedb is for, so nobody reads it as an option.
- ADR-0010's remote form is gone, but its abstraction keeps a negative control, which is the reason `VirtualStorage` is allowed to remain a trait instead of collapsing into fjall.

## Rejected

- **Deleting slatedb with the remote shape.** Two agreeing engines cannot falsify an abstraction; the disagreement is the only thing in the matrix that can.
- **Promoting it back to a deployment backend.** Storage is local paths only, and the object-store shape is what ADR-0056 refused for the log anyway.