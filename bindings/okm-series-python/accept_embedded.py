"""okm-series embedded-mode acceptance: the log driven purely from Python.

`tests/binding.rs` drives the same surface in-process, which means it proves the binding
but not the artifact. This script proves the artifact: install the wheel, run it in a plain
interpreter, and every check below goes through the extension module a user would import.

    maturin develop && python accept_embedded.py     # from this directory
    python -P accept_embedded.py                     # against an installed wheel

`-P` keeps this directory off `sys.path`, so a stray build artifact here cannot shadow the
installed package; the first line printed names the module that actually answered.

What is locked: a typed body round-trips through a block, the log stamps the record's time
and exposes it under `_ts`, the time never goes backwards, named tags work, a partitioned
log refuses a record without the field and keeps one part per partition, `seal` writes a
footer that `parts` then reports, `read_by_time` answers a window, retention removes an old
bucket, and `meta` carries the log's identity.
"""
import tempfile

import okm_series


def typed_body_round_trips(root: str) -> None:
    log = okm_series.Log(root, precision="ms")
    offset = log.append({1: "error", 2: 42, 3: -7, 4: 0.5, 5: b"raw"})
    assert offset == 0, f"first record offset: {offset}"
    log.flush()

    rows = log.read(0, 100)
    assert len(rows) == 1, f"read back {len(rows)} rows"
    record = rows[0]
    assert record.offset == 0
    assert record.ts > 0, "the log stamped the record at write time"
    assert record.body[1] == "error"
    assert record.body[2] == 42
    assert record.body[3] == -7
    assert record.body[4] == 0.5
    assert record.body[5] == b"raw"
    assert record.body["_ts"] == record.ts, "the record's own time is readable as a field"


def the_time_never_goes_backwards(root: str) -> None:
    log = okm_series.Log(root, precision="ms")
    log.append({1: "first"})
    log.append({1: "second"})
    log.flush()
    rows = log.read(0, 10)
    assert len(rows) == 2
    assert rows[1].ts >= rows[0].ts, f"{rows[0].ts} -> {rows[1].ts}"
    assert rows[1].offset > rows[0].offset


def named_tags_need_the_declaration(root: str) -> None:
    log = okm_series.Log(root, tags={"level": 1, "message": 2})
    log.append({"level": "warn", "message": "disk almost full"})
    log.flush()
    body = log.read(0, 10)[0].body
    assert body["level"] == "warn"
    assert body["message"] == "disk almost full"
    # A name the log was not opened with is not a name.
    try:
        log.append({"unknown": 1})
    except ValueError:
        pass
    else:  # pragma: no cover - the refusal is the point
        raise AssertionError("an undeclared name must not be taken for a tag")


def a_partitioned_log_needs_the_field(root: str) -> None:
    log = okm_series.Log(root, partition_tag=9)
    try:
        log.append({1: "no partition"})
    except ValueError as error:
        assert "partition" in str(error), error
    else:  # pragma: no cover
        raise AssertionError("a partitioned log refuses a record without the tag")

    log.append({1: "row", 9: "eu"})
    log.append({1: "row", 9: "us"})
    log.flush()
    parts = log.parts()
    assert len(parts) == 2, f"one part per partition, got {len(parts)}"
    assert {part["partition"] for part in parts} == {"eu", "us"}
    assert all(part["records"] == 1 for part in parts)


def seal_writes_a_footer_the_parts_report(root: str) -> None:
    log = okm_series.Log(root, precision="ms")
    for index in range(3):
        log.append({1: index})
    open_parts = log.parts()
    assert open_parts and not any(part["sealed"] for part in open_parts), "no seal yet"

    footers = log.seal()
    assert len(footers) == 1, f"one part sealed, got {len(footers)}"
    records, last_ts, blocks, digest = footers[0]
    assert records == 3
    assert last_ts > 0
    assert blocks >= 1
    assert digest != 0, "a sealed part carries a whole-part digest"

    sealed = log.parts()
    assert len(sealed) == 1
    assert sealed[0]["sealed"] is True
    assert sealed[0]["records"] == 3
    # A sealed part is still readable — sealing is a footer, not a wall.
    assert len(log.read(0, 10)) == 3


def read_by_time_answers_a_window(root: str) -> None:
    log = okm_series.Log(root, precision="ms")
    log.append({1: "a"})
    log.append({1: "b"})
    log.flush()
    rows = log.read(0, 10)
    first, last = rows[0].ts, rows[-1].ts

    inclusive = log.read_by_time(first, last, 10)
    assert len(inclusive) == 2, f"the window covers both rows, got {len(inclusive)}"
    empty = log.read_by_time(last + 1, last + 1000, 10)
    assert empty == [], "a window past the last record answers nothing"


def retention_removes_an_old_bucket(root: str) -> None:
    log = okm_series.Log(root, precision="ms", bucket_seconds=1)
    log.append({1: "old"})
    log.flush()
    log.seal()
    stamps = [part["first_ts"] for part in log.parts()]
    assert stamps, "the part is there before retention"

    # Drop everything older than the bucket a much later instant falls in.
    dropped = log.retain_before(stamps[0] + 3_600_000)
    assert dropped, "the bucket holding the record was dropped"
    assert log.parts() == [], "and its directory is gone, not tombstoned"


def meta_is_the_logs_identity(root: str) -> None:
    log = okm_series.Log(root, precision="ms", bucket_seconds=60, partition_tag=9)
    meta = log.meta
    assert meta["precision"] == "ms", meta
    assert meta["bucket_seconds"] == 60, meta
    assert meta["partition_tag"] == 9, meta
    assert meta["epoch_unix_ms"] > 0, meta

    # A second open of the same root, with the same shape, sees the same identity.
    again = okm_series.Log(root, precision="ms", bucket_seconds=60, partition_tag=9)
    assert again.meta["epoch_unix_ms"] == meta["epoch_unix_ms"], again.meta

    # And an open that disagrees with the pinned shape is refused by name — the pinned
    # names are precision / bucket / partition (block and codec settings are NOT pinned,
    # so those two can still drift silently: the open owed item in docs/PLAN.md).
    try:
        okm_series.Log(root, precision="ms")
    except ValueError as error:
        assert "bucket" in str(error), error
    else:  # pragma: no cover
        raise AssertionError("a log opened with another bucket size must be refused")


def main() -> int:
    print(f"okm_series: {okm_series.__file__}")
    checks = [
        typed_body_round_trips,
        the_time_never_goes_backwards,
        named_tags_need_the_declaration,
        a_partitioned_log_needs_the_field,
        seal_writes_a_footer_the_parts_report,
        read_by_time_answers_a_window,
        retention_removes_an_old_bucket,
        meta_is_the_logs_identity,
    ]
    for check in checks:
        with tempfile.TemporaryDirectory() as root:
            check(root)
        print(f"ok  {check.__name__}")
    print("series embedded-mode acceptance: ALL PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())