//! The binary's arguments, as data.
//!
//! `--out` (also spelled `--store` and `--lake`) names a LAKE, in the one vocabulary
//! [`crate::store`] understands: a directory, a `file://` url, or `s3://bucket/prefix`. The
//! verbs differ in which knobs they take, never in how the lake is named.
//!
//! Parsing lives here rather than in `main` so every form can be asserted without a process —
//! and without a bucket: what a test looks at is the config that came out, never a connection.

use std::path::PathBuf;

use crate::FoldPolicy;
use crate::store::LakeUrl;

/// What the process was asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verb {
    /// The default verb: project the change log into the lake.
    Drain(DrainArgs),
    /// Fold each table into current state.
    Condense(LakeArgs),
    /// Delete the files no kept version reads.
    Vacuum(VacuumArgs),
    /// Say what a lake holds.
    Status(StatusArgs),
    /// Print the usage and stop.
    Help,
}

/// The default verb's own arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrainArgs {
    /// The log directory a capturing store wrote — always local: the log is where the
    /// application is.
    pub root: PathBuf,
    pub lake: LakeUrl,
    pub consumer: String,
    pub max_segments: usize,
    /// One table by its header in hex, instead of every table under `root`.
    pub table: Option<String>,
    pub partition: Option<String>,
    pub max_rows_per_file: usize,
    pub fold: Option<FoldPolicy>,
}

/// The lake-only verbs' arguments (`condense`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LakeArgs {
    pub lake: LakeUrl,
    pub table: Option<String>,
}

/// `vacuum` needs a retention decision as well as a lake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VacuumArgs {
    pub lake: LakeUrl,
    pub table: Option<String>,
    pub keep_versions: u64,
    pub dry_run: bool,
}

/// `status` may fold the table, which is why counting rows is opt-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusArgs {
    pub lake: LakeUrl,
    pub table: Option<String>,
    pub rows: bool,
}

pub const USAGE: &str = "\
okm-lake [--root <log dir> --out <lake>] | <verb>

(no verb)  --root <log dir> --out <lake> [--consumer NAME] [--max-segments N]
           [--table <hex header>] [--partition COLUMN] [--max-rows-per-file N]
           [--fold-after-files N] [--fold-after-bytes B]
condense   --out <lake> [--table <hex header>]
vacuum     --out <lake> --keep-versions N [--table <hex header>] [--dry-run]
status     --out <lake> [--table <hex header>] [--rows]

A lake is a directory, a `file://` path, or `s3://bucket/prefix` (credentials from the
environment: AWS_* or OKM_S3_*). The lake, not the log: `okm-log` has status/compact/replay
for the segments.";

/// Parse the arguments after the program name.
pub fn parse(args: &[String]) -> Result<Verb, String> {
    // Help is welcome anywhere: a verb's own usage is a suffix of this one.
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Ok(Verb::Help);
    }
    let Some(first) = args.first() else {
        return Err(format!("no arguments\n\n{USAGE}"));
    };
    let rest = &args[1..];
    match first.as_str() {
        "condense" => Ok(Verb::Condense(lake_args(rest, condense_usage())?)),
        "vacuum" => Ok(Verb::Vacuum(vacuum_args(rest)?)),
        "status" => Ok(Verb::Status(status_args(rest)?)),
        "-h" | "--help" | "help" => Ok(Verb::Help),
        other if other.starts_with('-') => Ok(Verb::Drain(drain_args(args)?)),
        other => Err(format!(
            "unknown verb `{other}` (okm-lake: condense, vacuum, status; okm-log: status, \
             compact, replay)"
        )),
    }
}

fn condense_usage() -> &'static str {
    "okm-lake condense --out <lake> [--table <hex header>]"
}

fn drain_args(args: &[String]) -> Result<DrainArgs, String> {
    let mut root: Option<PathBuf> = None;
    let mut lake: Option<LakeUrl> = None;
    let mut consumer = String::from("default");
    let mut max_segments = 16usize;
    let mut table: Option<String> = None;
    let mut fold_after_files: usize = 0;
    let mut fold_after_bytes: u64 = 0;
    let mut partition: Option<String> = None;
    let mut max_rows_per_file: usize = 0;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--root" | "--log" => root = Some(PathBuf::from(value()?)),
            "--out" | "--lake" | "--store" => lake = Some(parse_lake(&value()?)?),
            "--consumer" => consumer = value()?,
            "--table" => table = Some(value()?),
            "--max-segments" => max_segments = number(&value()?, "--max-segments takes a number")?,
            "--fold-after-files" => {
                fold_after_files = number(&value()?, "--fold-after-files takes a number")?
            }
            "--fold-after-bytes" => {
                fold_after_bytes = number(&value()?, "--fold-after-bytes takes a number")?
            }
            "--partition" => partition = Some(value()?),
            "--max-rows-per-file" => {
                max_rows_per_file = number(&value()?, "--max-rows-per-file takes a number")?
            }
            "-h" | "--help" => return Err(format!("the default verb's usage\n\n{USAGE}")),
            other => return Err(format!("unknown argument `{other}` (try --help)")),
        }
    }

    let root = root.ok_or("--root is required (the capturing store's log directory)")?;
    let lake = lake.ok_or("--out is required (where window files land)")?;
    Ok(DrainArgs {
        root,
        lake,
        consumer,
        max_segments,
        table,
        partition,
        max_rows_per_file,
        fold: fold_policy(fold_after_files, fold_after_bytes),
    })
}

fn lake_args(args: &[String], usage: &str) -> Result<LakeArgs, String> {
    let mut lake: Option<LakeUrl> = None;
    let mut table: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--out" | "--lake" | "--store" => lake = Some(parse_lake(&value()?)?),
            "--table" => table = Some(value()?),
            "-h" | "--help" => return Err(usage.to_string()),
            other => return Err(format!("unknown argument `{other}` (try --help)")),
        }
    }
    Ok(LakeArgs {
        lake: lake.ok_or("--out is required (the lake windows were drained into)")?,
        table,
    })
}

fn vacuum_args(args: &[String]) -> Result<VacuumArgs, String> {
    let mut lake: Option<LakeUrl> = None;
    let mut table: Option<String> = None;
    let mut keep: Option<u64> = None;
    let mut dry_run = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--out" | "--lake" | "--store" => lake = Some(parse_lake(&value()?)?),
            "--table" => table = Some(value()?),
            "--keep-versions" => keep = Some(number(&value()?, "--keep-versions takes a number")?),
            "--dry-run" => dry_run = true,
            "-h" | "--help" => {
                return Err(
                    "okm-lake vacuum --out <lake> --keep-versions N [--table <hex header>] \
                     [--dry-run]"
                        .to_string(),
                );
            }
            other => return Err(format!("unknown argument `{other}` (try --help)")),
        }
    }
    Ok(VacuumArgs {
        lake: lake.ok_or("--out is required (the lake windows were drained into)")?,
        table,
        // Deleting files is the one verb that must be told how much history to keep: a
        // default here would be a guess about someone else's readers.
        keep_versions: keep
            .ok_or("--keep-versions is required: the number of versions to keep readable")?,
        dry_run,
    })
}

fn status_args(args: &[String]) -> Result<StatusArgs, String> {
    let mut lake: Option<LakeUrl> = None;
    let mut table: Option<String> = None;
    let mut rows = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--out" | "--lake" | "--store" => lake = Some(parse_lake(&value()?)?),
            "--table" => table = Some(value()?),
            "--rows" => rows = true,
            "-h" | "--help" => {
                return Err(
                    "okm-lake status --out <lake> [--table <hex header>] [--rows]".to_string(),
                );
            }
            other => return Err(format!("unknown argument `{other}` (try --help)")),
        }
    }
    Ok(StatusArgs {
        lake: lake.ok_or(
            "--out is required (the exported lake; `okm-log status --root` is the log's side)",
        )?,
        table,
        rows,
    })
}

fn parse_lake(value: &str) -> Result<LakeUrl, String> {
    LakeUrl::parse(value).map_err(|e| format!("--out: {e}"))
}

fn number<T: std::str::FromStr>(value: &str, complaint: &str) -> Result<T, String> {
    value.parse().map_err(|_| complaint.to_string())
}

fn fold_policy(after_files: usize, after_bytes: u64) -> Option<FoldPolicy> {
    if after_files > 0 || after_bytes > 0 {
        Some(FoldPolicy {
            after_files,
            after_bytes,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(|a| a.to_string()).collect()
    }

    #[test]
    fn a_directory_and_a_file_url_parse_to_the_same_lake() {
        let dir = parse(&args("--root /log --out /lake")).expect("a directory");
        let Verb::Drain(dir_args) = dir else {
            panic!("the default verb is the drain");
        };
        assert_eq!(dir_args.lake, LakeUrl::Dir(PathBuf::from("/lake")));
        assert_eq!(dir_args.root, PathBuf::from("/log"));

        let url = parse(&args("--root /log --out file:///lake")).expect("a file url");
        let Verb::Drain(url_args) = url else {
            panic!("the default verb is the drain");
        };
        assert_eq!(url_args.lake, dir_args.lake);
    }

    #[test]
    fn a_bucket_url_parses_into_bucket_and_prefix() {
        let Verb::Drain(parsed) =
            parse(&args("--root /log --out s3://lake-bucket/world/okm")).expect("a bucket")
        else {
            panic!("the default verb is the drain");
        };
        assert_eq!(
            parsed.lake,
            LakeUrl::S3 {
                bucket: "lake-bucket".into(),
                prefix: "world/okm".into(),
            }
        );
        assert_eq!(parsed.lake.describe(), "s3://lake-bucket/world/okm");

        // A trailing slash is the same lake; a bucket with no prefix is legal.
        let Verb::Status(bare) = parse(&args("status --out s3://lake-bucket")).expect("a bucket")
        else {
            panic!("status");
        };
        assert_eq!(
            bare.lake,
            LakeUrl::S3 {
                bucket: "lake-bucket".into(),
                prefix: String::new(),
            }
        );
        let Verb::Status(slash) = parse(&args("status --out s3://lake-bucket/")).expect("a bucket")
        else {
            panic!("status");
        };
        assert_eq!(slash.lake, bare.lake);
    }

    #[test]
    fn a_bucket_url_with_no_bucket_is_refused() {
        let err = parse(&args("status --out s3:///lake")).expect_err("no bucket");
        assert!(err.contains("names no bucket"), "{err}");
    }

    #[test]
    fn the_lake_can_be_spelled_store_or_lake() {
        for flag in ["--out", "--lake", "--store"] {
            let Verb::Condense(parsed) =
                parse(&args(&format!("condense {flag} s3://bucket/prefix")))
                    .unwrap_or_else(|e| panic!("{flag}: {e}"))
            else {
                panic!("condense");
            };
            assert_eq!(
                parsed.lake,
                LakeUrl::S3 {
                    bucket: "bucket".into(),
                    prefix: "prefix".into(),
                },
                "{flag}"
            );
        }
    }

    #[test]
    fn every_verb_takes_a_url_and_its_own_knobs() {
        let Verb::Condense(condense) =
            parse(&args("condense --out s3://b/p --table 0047")).expect("condense")
        else {
            panic!("condense");
        };
        assert_eq!(condense.table.as_deref(), Some("0047"));

        let Verb::Vacuum(vacuum) =
            parse(&args("vacuum --store s3://b/p --keep-versions 7 --dry-run")).expect("vacuum")
        else {
            panic!("vacuum");
        };
        assert_eq!(vacuum.keep_versions, 7);
        assert!(vacuum.dry_run);

        let Verb::Status(status) = parse(&args("status --out s3://b/p --rows")).expect("status")
        else {
            panic!("status");
        };
        assert!(status.rows);
        assert_eq!(status.table, None);
    }

    #[test]
    fn the_drain_keeps_its_knobs_and_turns_the_fold_flags_into_a_policy() {
        let Verb::Drain(parsed) = parse(&args(
            "--root /log --out s3://b/p --consumer sidecar --max-segments 4 \
             --partition org --max-rows-per-file 1000 --fold-after-files 8",
        ))
        .expect("drain") else {
            panic!("drain");
        };
        assert_eq!(parsed.consumer, "sidecar");
        assert_eq!(parsed.max_segments, 4);
        assert_eq!(parsed.partition.as_deref(), Some("org"));
        assert_eq!(parsed.max_rows_per_file, 1000);
        assert_eq!(parsed.fold, Some(FoldPolicy::files(8)));

        // Neither flag means "never fold", which is `None` rather than a zero policy.
        let Verb::Drain(plain) = parse(&args("--root /log --out s3://b/p")).expect("drain") else {
            panic!("drain");
        };
        assert_eq!(plain.fold, None);
    }

    #[test]
    fn a_missing_value_or_required_flag_says_which() {
        assert!(
            parse(&args("--root /log --out"))
                .expect_err("no value")
                .contains("--out needs a value")
        );
        assert!(
            parse(&args("status"))
                .expect_err("no lake")
                .contains("--out is required")
        );
        assert!(
            parse(&args("vacuum --out /lake"))
                .expect_err("no retention")
                .contains("--keep-versions is required")
        );
        assert!(
            parse(&args("condense --out /lake --nope 1"))
                .expect_err("unknown flag")
                .contains("unknown argument")
        );
    }

    #[test]
    fn a_verb_that_is_not_ours_is_named_back() {
        assert!(
            parse(&args("compact --root /log"))
                .expect_err("not this tool's verb")
                .contains("unknown verb `compact`")
        );
        assert!(matches!(parse(&args("--help")), Ok(Verb::Help)));
    }
}
