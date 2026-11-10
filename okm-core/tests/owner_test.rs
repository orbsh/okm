//! The claim file and the switch it drives (ADR-0056 §6).
//!
//! What these locks are about: a directory gives no exclusion, so the file must be
//! READABLE by a build that is not this one (unknown keys ignored), and must never be
//! GUESSED at (an unparsable claim is an error, never a default — the guess is the split
//! brain the file exists to prevent). The lease is the other half: one `AtomicBool` the
//! commit path reads, closed by another holder's write and never reopened.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use std::time::Duration;

use okm_core::changelog::owner::{self, Lease, Owner, Role, State, Timings};

fn short() -> Timings {
    Timings {
        tick: Duration::from_millis(10),
        heartbeat: Duration::from_millis(50),
    }
}

fn writing(holder: &str, epoch: u64) -> Owner {
    Owner {
        state: State::Writing,
        epoch,
        holder: holder.to_string(),
        data: "/srv/store-a".into(),
        position: None,
    }
}

#[test]
fn a_round_trip_keeps_every_field_of_the_claim() {
    let dir = tempfile::tempdir().unwrap();
    let owner = Owner {
        state: State::HandedOver,
        epoch: 7,
        holder: "3f9a1c2e".into(),
        data: "/srv/store-b".into(),
        position: Some("00000042.segment".into()),
    };
    owner::write(dir.path(), &owner).unwrap();
    assert_eq!(owner::read(dir.path()).unwrap(), Some(owner));
}

#[test]
fn an_unclaimed_directory_is_not_a_claim_on_anything() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(owner::read(dir.path()).unwrap(), None);
}

#[test]
fn an_unknown_key_is_ignored_so_a_newer_build_can_write_a_field_this_one_lacks() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        owner::path(dir.path()),
        "state writing\nepoch 3\nholder abc\ndata /srv/store-a\nlease-until 1234\nsomething-added-later x\n",
    )
    .unwrap();
    let owner = owner::read(dir.path()).unwrap().unwrap();
    assert_eq!(owner, writing("abc", 3));
}

#[test]
fn a_claim_without_a_state_is_refused_rather_than_defaulted() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(owner::path(dir.path()), "epoch 3\nholder abc\n").unwrap();
    let err = owner::read(dir.path()).unwrap_err();
    assert!(err.contains("state"), "{err}");

    // An unknown word is refused too: a reader that cannot name the state cannot
    // decide whether the writer is still there.
    std::fs::write(
        owner::path(dir.path()),
        "state halfway\nepoch 3\nholder abc\n",
    )
    .unwrap();
    assert!(owner::read(dir.path()).is_err());
}

#[test]
fn a_second_claim_is_refused_while_the_first_holder_is_writing() {
    let dir = tempfile::tempdir().unwrap();
    let lease = Lease::claim_with(dir.path(), "first", "/srv/store-a", short()).unwrap();
    let err = match Lease::claim_with(dir.path(), "second", "/srv/store-b", short()) {
        Ok(_) => panic!("a second claim was accepted while the first is writing"),
        Err(err) => err,
    };
    assert!(err.contains("take over"), "{err}");
    drop(lease);
}

#[test]
fn the_gate_closes_when_another_holder_takes_the_file_and_stays_closed() {
    let dir = tempfile::tempdir().unwrap();
    let lease = Lease::claim_with(dir.path(), "old", "/srv/store-a", short()).unwrap();
    assert!(lease.may_write());

    // The new build's first write: epoch bumped, state draining.
    let takeover = owner::announce_drain(dir.path(), "new", "/srv/store-b").unwrap();
    assert_eq!(takeover.state, State::Draining);
    assert_eq!(takeover.epoch, lease.owner().epoch + 1);

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while lease.may_write() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !lease.may_write(),
        "the old holder kept writing after a takeover"
    );

    // A claim that went back to `writing` under its own name does not reopen it: the
    // gate is one way on purpose.
    owner::write(dir.path(), &writing("old", lease.owner().epoch)).unwrap();
    std::thread::sleep(Duration::from_millis(60));
    assert!(!lease.may_write());
}

#[test]
fn a_claim_that_cannot_be_read_closes_the_gate() {
    let dir = tempfile::tempdir().unwrap();
    let lease = Lease::claim_with(dir.path(), "old", "/srv/store-a", short()).unwrap();
    assert!(lease.may_write());
    // The directory vanishing is the blunt version of "the file this writer depends on
    // is gone"; an unreadable claim is not evidence of ownership either.
    std::fs::remove_dir_all(dir.path()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while lease.may_write() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!lease.may_write());
}

#[test]
fn a_takeover_request_bumps_the_epoch_each_time_it_is_asked() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        owner::announce_drain(dir.path(), "new", "/srv/store-b")
            .unwrap()
            .epoch,
        1
    );
    assert_eq!(
        owner::announce_drain(dir.path(), "new", "/srv/store-b")
            .unwrap()
            .epoch,
        2
    );
}

#[test]
fn a_handover_names_the_position_the_request_was_made_under() {
    let dir = tempfile::tempdir().unwrap();
    let request = owner::announce_drain(dir.path(), "new", "/srv/store-b").unwrap();

    // Still draining: nothing to claim yet, and the wait must not invent an answer.
    assert_eq!(
        owner::wait_for_handover(dir.path(), request.epoch, Duration::from_millis(80)).unwrap(),
        None
    );

    // The old holder's last write, under the epoch it was asked for.
    owner::hand_over(
        dir.path(),
        request.epoch,
        "old",
        "/srv/store-a",
        "00000007.segment",
    )
    .unwrap();
    assert_eq!(
        owner::wait_for_handover(dir.path(), request.epoch, Duration::from_secs(1)).unwrap(),
        Some("00000007.segment".to_string())
    );

    // A handover from an earlier round answers an earlier request, not this one.
    assert_eq!(
        owner::wait_for_handover(dir.path(), request.epoch + 1, Duration::from_millis(50)).unwrap(),
        None
    );
}

#[test]
fn a_second_instance_on_the_same_store_is_an_error_not_a_role() {
    let dir = tempfile::tempdir().unwrap();
    let lease = Lease::claim_with(dir.path(), "first", "/srv/store-a", short()).unwrap();

    // Same data directory, another token: two processes over one store. Refused,
    // because single-writer is what "old drains, new applies" rests on.
    let err = match Lease::claim_with(dir.path(), "second", "/srv/store-a", short()) {
        Ok(_) => panic!("a second writer over one store was accepted"),
        Err(err) => err,
    };
    assert!(err.contains("share one store"), "{err}");
    assert_eq!(
        owner::decide(dir.path(), "second", "/srv/store-a").unwrap(),
        Role::Shared {
            holder: "first".into(),
            epoch: lease.owner().epoch
        }
    );
}

#[test]
fn a_second_instance_on_its_own_store_is_the_successor() {
    let dir = tempfile::tempdir().unwrap();
    let lease = Lease::claim_with(dir.path(), "first", "/srv/store-a", short()).unwrap();

    // The blue/green case: another store, so this instance is the successor and the
    // handover is what it has to do — claiming would skip the replay.
    assert_eq!(
        owner::decide(dir.path(), "second", "/srv/store-b").unwrap(),
        Role::Successor {
            holder: "first".into(),
            epoch: lease.owner().epoch
        }
    );
    let err = match Lease::claim_with(dir.path(), "second", "/srv/store-b", short()) {
        Ok(_) => panic!("the successor claimed without taking over"),
        Err(err) => err,
    };
    assert!(err.contains("take over"), "{err}");
}

#[test]
fn a_restart_finds_its_own_claim_and_resumes_without_a_handover() {
    let dir = tempfile::tempdir().unwrap();
    let lease = Lease::claim_with(dir.path(), "first", "/srv/store-a", short()).unwrap();
    assert_eq!(
        owner::decide(dir.path(), "first", "/srv/store-a").unwrap(),
        Role::Resume
    );
    drop(lease);
}

#[test]
fn an_unclaimed_log_is_a_first_start() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        owner::decide(dir.path(), "anyone", "/srv/store-a").unwrap(),
        Role::First
    );
}
