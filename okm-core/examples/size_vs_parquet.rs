//! What the same rows cost on disk: OKM over fjall (with and without a declared
//! secondary index) against a Parquet file, and the CSV one emits so the
//! PostgreSQL / InnoDB arms can be measured on identical rows.
//!
//! ```text
//! cargo run -p okm-core --release --example size_vs_parquet --features fjall
//! cargo run -p okm-core --release --example size_vs_parquet --features fjall -- --emit-csv /tmp/okm_orders.csv
//! ```
//!
//! One million order rows of a medium width (two ids, a 1-byte categorical, a
//! 3-byte code, two stamps, a count, a ~20-byte email, a ~53-byte note). Every
//! arm sees the same bytes because the generator is seeded, and `--emit-csv`
//! writes exactly those rows out for the engines this crate cannot link.
//!
//! Two numbers per store, and the difference between them is the point:
//!
//! - **logical** — key + value summed over the keyspace. What the codec wrote,
//!   with no engine in it.
//! - **at rest** — the store's own data files after the journals have been
//!   replayed and rotated away. This is the number to compare against another
//!   database's heap and indexes; a WAL is not part of a table.
//!
//! The measured table, the PostgreSQL and InnoDB counterparts, and why the
//! numbers land where they do are in `docs/internals/benchmarks.md`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::array::{ArrayRef, Int64Array, StringArray, UInt16Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use fjall::Database;
use okm_core::engine::fjall_backend::FjallStore;
use okm_core::{Collection, DocumentEncode, KeyEncode, VirtualStorage, Vocab};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::{WriterProperties, WriterVersion};

const DEFAULT_ROWS: u64 = 1_000_000;
/// Rows per Parquet row group / Arrow batch.
const BATCH: usize = 100_000;
/// A settle that takes longer than this is reported unsettled rather than waited on.
const SETTLE: Duration = Duration::from_secs(600);

// ---------- the declared row, in two shapes ----------

#[derive(KeyEncode, Clone, PartialEq, Debug, Default)]
pub struct OrderKey {
    pub org_id: u32,
    pub order_id: u64,
}

/// The document-only arm: no index, so the entry set is exactly one per row.
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(OrderKey)]
#[ok_ns(0x71)]
#[ok_name("orders")]
pub struct OrderRow {
    pub status: Vocab<u8>,
    pub currency: [u8; 3],
    pub amount: u64,
    pub created_at: i64,
    pub customer_id: u64,
    pub items: u16,
    pub email: String,
    pub note: String,
}

/// The same row plus one declared secondary index — the shape every database in
/// the comparison also carries (a btree in PG, a secondary index in InnoDB).
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(OrderKey)]
#[ok_ns(0x72)]
#[ok_name("orders_indexed")]
#[ok_index(by_created { fields(created_at), includes(amount) })]
pub struct IndexedRow {
    pub status: Vocab<u8>,
    pub currency: [u8; 3],
    pub amount: u64,
    pub created_at: i64,
    pub customer_id: u64,
    pub items: u16,
    pub email: String,
    pub note: String,
}

// ---------- deterministic rows ----------

const STATUS: [&str; 5] = ["pending", "paid", "shipped", "refunded", "cancelled"];
const CURRENCY: [&[u8; 3]; 4] = [b"USD", b"EUR", b"CNY", b"JPY"];
const FIRST: [&str; 32] = [
    "li", "wang", "zhang", "liu", "chen", "yang", "zhao", "huang", "zhou", "wu", "xu", "sun", "ma",
    "zhu", "hu", "guo", "he", "gao", "lin", "luo", "zheng", "liang", "xie", "song", "tang", "han",
    "feng", "deng", "cao", "peng", "zeng", "xiao",
];
const DOMAIN: [&str; 4] = ["example.com", "mail.test", "corp.example", "inbox.test"];
const WORDS: [&str; 40] = [
    "the",
    "order",
    "was",
    "placed",
    "with",
    "express",
    "shipping",
    "and",
    "gift",
    "wrap",
    "customer",
    "asked",
    "for",
    "a",
    "later",
    "delivery",
    "slot",
    "on",
    "friday",
    "afternoon",
    "please",
    "leave",
    "at",
    "front",
    "door",
    "if",
    "nobody",
    "answers",
    "refund",
    "requested",
    "one",
    "item",
    "damaged",
    "in",
    "transit",
    "photo",
    "attached",
    "by",
    "buyer",
    "again",
];

/// xorshift64*, so every arm — and the CSV — sees the same rows without an rng
/// dependency and without a 127 MB file in the repository.
struct Gen(u64);

impl Gen {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn range(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

struct Row {
    org_id: u32,
    order_id: u64,
    status: &'static str,
    currency: [u8; 3],
    amount: u64,
    created_at: i64,
    customer_id: u64,
    items: u16,
    email: String,
    note: String,
}

fn dataset(count: u64) -> impl Iterator<Item = Row> {
    let mut rng = Gen(0x9E37_79B9_7F4A_7C15);
    (0..count).map(move |i| {
        let mut note = String::with_capacity(64);
        for k in 0..9 {
            if k > 0 {
                note.push(' ');
            }
            note.push_str(WORDS[rng.range(WORDS.len() as u64) as usize]);
        }
        Row {
            org_id: (i % 1_000) as u32,
            order_id: i,
            status: STATUS[rng.range(STATUS.len() as u64) as usize],
            currency: *CURRENCY[rng.range(CURRENCY.len() as u64) as usize],
            amount: rng.range(500_000),
            created_at: 1_700_000_000_000 + (i as i64) * 700 + rng.range(500) as i64,
            customer_id: rng.range(400_000),
            items: rng.range(5) as u16 + 1,
            email: format!(
                "{}{}@{}",
                FIRST[rng.range(FIRST.len() as u64) as usize],
                rng.range(100_000),
                DOMAIN[rng.range(DOMAIN.len() as u64) as usize]
            ),
            note,
        }
    })
}

// ---------- measurement ----------

fn dir_bytes(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        match entry.metadata() {
            Ok(meta) if meta.is_dir() => total += dir_bytes(&entry.path()),
            Ok(meta) => total += meta.len(),
            Err(_) => {}
        }
    }
    total
}

/// Wait until nothing is flushing or compacting, so a byte count is a size
/// rather than a picture of a write burst.
fn quiesce(db: &Database) {
    let deadline = Instant::now() + SETTLE;
    while (db.outstanding_flushes() > 0 || db.active_compactions() > 0) && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Reopen the store and wait for recovery's flush and compaction to finish. What
/// comes back is the settled encoding: the data files (`disk_space` minus the
/// journal), the live journal bytes, and the directory.
///
/// The journal term is reported separately because it is a bounded segment (one
/// write buffer) holding the un-flushed tail, not a copy of the table: at a few
/// million rows it is a few percent, and it disappears into the data files as
/// they flush. Counting it inside the per-row figure would make the number depend
/// on where the write burst happened to end.
fn at_rest(dir: &Path) -> (u64, u64, u64) {
    let db = Database::create_or_recover(fjall::Config::new(dir)).expect("reopen");
    quiesce(&db);
    // The last memtable does not flush on its own — it flushes when it fills, and
    // after a bulk load it is whatever the write buffer did not reach. Waiting for
    // it to drain is a wait for more writes that will not come, so the journal is
    // reported separately instead of being waited away.
    let total = db.disk_space().expect("disk_space");
    let journal = db.journal_disk_space().expect("journal");
    (total - journal, journal, dir_bytes(dir))
}

#[derive(Debug)]
struct Line {
    entries: u64,
    logical: u64,
    data: u64,
    journal: u64,
    dir: u64,
    write: Duration,
}

fn report(label: &str, line: &Line, rows: u64) {
    println!(
        "{label:<26} {:>9.1} B/row  {:>8.1} MB at rest  logical {:>6.1}  journal {:>6.1}  dir {:>6.1}  ({:.3} entry/row, {:.1} s)",
        line.data as f64 / rows as f64,
        line.data as f64 / 1e6,
        line.logical as f64 / rows as f64,
        line.journal as f64 / 1e6,
        line.dir as f64 / 1e6,
        line.entries as f64 / rows as f64,
        line.write.as_secs_f64()
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut emit_csv: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        if arg == "--emit-csv" {
            emit_csv = Some(PathBuf::from(args.next().expect("--emit-csv <path>")));
        }
    }
    let rows: u64 = std::env::var("OKM_SIZE_ROWS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_ROWS);

    println!("{rows} rows, same bytes in every arm\n");

    // ---- okm, document only ----
    let doc_dir = std::env::temp_dir().join("okm-size-doc");
    let _ = std::fs::remove_dir_all(&doc_dir);
    let db = Database::create_or_recover(fjall::Config::new(&doc_dir)).expect("open fjall");
    let store = FjallStore::from_db(db.clone(), "okm").expect("keyspace");
    let t: Collection<FjallStore, OrderKey, OrderRow> = Collection::open(store.clone()).unwrap();
    let started = Instant::now();
    for r in dataset(rows) {
        let key = OrderKey {
            org_id: r.org_id,
            order_id: r.order_id,
        };
        t.put(
            &key,
            &OrderRow {
                status: Vocab::of(r.status),
                currency: r.currency,
                amount: r.amount,
                created_at: r.created_at,
                customer_id: r.customer_id,
                items: r.items,
                email: r.email,
                note: r.note,
            },
        )
        .expect("put");
    }
    let write = started.elapsed();
    store.persist().expect("persist");
    quiesce(&db);
    let (entries, logical) = scan(&store);
    drop(t);
    drop(store);
    drop(db);
    let (data, journal, dir) = at_rest(&doc_dir);
    let doc_line = Line {
        entries,
        logical,
        data,
        journal,
        dir,
        write,
    };
    report("okm, document only", &doc_line, rows);
    let _ = std::fs::remove_dir_all(&doc_dir);

    // ---- okm, document + one declared index ----
    let idx_dir = std::env::temp_dir().join("okm-size-indexed");
    let _ = std::fs::remove_dir_all(&idx_dir);
    let db = Database::create_or_recover(fjall::Config::new(&idx_dir)).expect("open fjall");
    let store = FjallStore::from_db(db.clone(), "okm").expect("keyspace");
    let t: Collection<FjallStore, OrderKey, IndexedRow> = Collection::open(store.clone()).unwrap();
    let started = Instant::now();
    for r in dataset(rows) {
        let key = OrderKey {
            org_id: r.org_id,
            order_id: r.order_id,
        };
        t.put(
            &key,
            &IndexedRow {
                status: Vocab::of(r.status),
                currency: r.currency,
                amount: r.amount,
                created_at: r.created_at,
                customer_id: r.customer_id,
                items: r.items,
                email: r.email,
                note: r.note,
            },
        )
        .expect("put");
    }
    let write = started.elapsed();
    store.persist().expect("persist");
    quiesce(&db);
    let (entries, logical) = scan(&store);
    drop(t);
    drop(store);
    drop(db);
    let (data, journal, dir) = at_rest(&idx_dir);
    let idx_line = Line {
        entries,
        logical,
        data,
        journal,
        dir,
        write,
    };
    report("okm, + one secondary index", &idx_line, rows);
    let _ = std::fs::remove_dir_all(&idx_dir);

    // ---- parquet, the same rows ----
    let total = rows;
    for (label, file_tag, compression) in [
        (
            "parquet zstd(1)",
            "zstd1",
            Compression::ZSTD(parquet_level()),
        ),
        ("parquet none", "none", Compression::UNCOMPRESSED),
    ] {
        let path = std::env::temp_dir().join(format!("okm-size-{file_tag}.parquet"));
        let _ = std::fs::remove_file(&path);
        let schema = parquet_schema();
        let props = WriterProperties::builder()
            .set_writer_version(WriterVersion::PARQUET_1_0)
            .set_compression(compression)
            .build();
        let mut w = ArrowWriter::try_new(
            File::create(&path).expect("create"),
            schema.clone(),
            Some(props),
        )
        .expect("writer");
        let mut batch: Vec<Row> = Vec::with_capacity(BATCH);
        for r in dataset(total) {
            batch.push(r);
            if batch.len() == BATCH {
                write_batch(&schema, &mut w, &mut batch);
            }
        }
        write_batch(&schema, &mut w, &mut batch);
        w.close().expect("close");
        let bytes = std::fs::metadata(&path).expect("meta").len();
        println!(
            "{label:<26} {:>9.1} B/row  {:>8.1} MB total     ({:.3} of the document-only entry set)",
            bytes as f64 / total as f64,
            bytes as f64 / 1e6,
            bytes as f64 / doc_line.logical as f64
        );
        let _ = std::fs::remove_file(&path);
    }

    // ---- the CSV the PostgreSQL and InnoDB arms load ----
    if let Some(path) = emit_csv {
        let file = File::create(&path).expect("create csv");
        let mut out = std::io::BufWriter::new(file);
        use std::io::Write;
        for r in dataset(total) {
            writeln!(
                out,
                "{},{},{},{},{},{},{},{},{},{}",
                r.org_id,
                r.order_id,
                r.status,
                std::str::from_utf8(&r.currency).expect("ascii currency"),
                r.amount,
                r.created_at,
                r.customer_id,
                r.items,
                r.email,
                r.note
            )
            .expect("write csv");
        }
        out.flush().expect("flush csv");
        let bytes = std::fs::metadata(&path).expect("meta").len();
        println!(
            "\nCSV emitted: {} ({:.1} MB, {:.1} B/row of raw text)",
            path.display(),
            bytes as f64 / 1e6,
            bytes as f64 / total as f64
        );
    }
}

/// Every entry in the keyspace: the count, and key+value bytes summed.
fn scan(store: &FjallStore) -> (u64, u64) {
    let mut entries = 0u64;
    let mut logical = 0u64;
    for item in store.scan_range(b"", None) {
        let (k, v) = item.expect("scan");
        entries += 1;
        logical += (k.len() + v.len()) as u64;
    }
    (entries, logical)
}

fn parquet_level() -> parquet::basic::ZstdLevel {
    parquet::basic::ZstdLevel::try_new(1).expect("zstd level 1")
}

fn parquet_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("org_id", DataType::UInt32, false),
        Field::new("order_id", DataType::UInt64, false),
        Field::new("status", DataType::Utf8, false),
        Field::new("currency", DataType::Utf8, false),
        Field::new("amount", DataType::UInt64, false),
        Field::new("created_at", DataType::Int64, false),
        Field::new("customer_id", DataType::UInt64, false),
        Field::new("items", DataType::UInt16, false),
        Field::new("email", DataType::Utf8, false),
        Field::new("note", DataType::Utf8, false),
    ]))
}

fn write_batch(schema: &Arc<Schema>, w: &mut ArrowWriter<File>, batch: &mut Vec<Row>) {
    if batch.is_empty() {
        return;
    }
    let n = batch.len();
    let mut org = Vec::with_capacity(n);
    let mut id = Vec::with_capacity(n);
    let mut status = Vec::with_capacity(n);
    let mut currency = Vec::with_capacity(n);
    let mut amount = Vec::with_capacity(n);
    let mut created = Vec::with_capacity(n);
    let mut customer = Vec::with_capacity(n);
    let mut items = Vec::with_capacity(n);
    let mut email: Vec<String> = Vec::with_capacity(n);
    let mut note: Vec<String> = Vec::with_capacity(n);
    for r in batch.drain(..) {
        org.push(r.org_id);
        id.push(r.order_id);
        status.push(r.status);
        currency.push(std::str::from_utf8(&r.currency).expect("ascii").to_string());
        amount.push(r.amount);
        created.push(r.created_at);
        customer.push(r.customer_id);
        items.push(r.items);
        email.push(r.email);
        note.push(r.note);
    }
    let columns: Vec<ArrayRef> = vec![
        Arc::new(UInt32Array::from(org)),
        Arc::new(UInt64Array::from(id)),
        Arc::new(StringArray::from(status)),
        Arc::new(StringArray::from(currency)),
        Arc::new(UInt64Array::from(amount)),
        Arc::new(Int64Array::from(created)),
        Arc::new(UInt64Array::from(customer)),
        Arc::new(UInt16Array::from(items)),
        Arc::new(StringArray::from(email)),
        Arc::new(StringArray::from(note)),
    ];
    let rb = RecordBatch::try_new(schema.clone(), columns).expect("batch");
    w.write(&rb).expect("write");
}
