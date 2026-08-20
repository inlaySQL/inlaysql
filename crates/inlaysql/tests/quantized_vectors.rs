use std::fs;
use std::path::{Path, PathBuf};

use inlaysql::{DataType, Database, Error, Value};

struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "inlaysql-{name}-{}-{:?}.inlay",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_file(&path);
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[test]
fn int8_vectors_reopen_search_and_render_as_the_declared_type() {
    let file = TempDb::new("q8-reopen");
    {
        let mut db = Database::open(file.path()).unwrap();
        db.execute(
            "CREATE TABLE docs (id INTEGER PRIMARY KEY, embedding VECTOR(4, INT8))",
            &[],
        )
        .unwrap();
        db.execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])
            .unwrap();
        for (id, vector) in [
            (1, vec![1.0, 0.0, 0.0, 0.0]),
            (2, vec![0.0, 1.0, 0.0, 0.0]),
            (3, vec![0.7, 0.7, 0.0, 0.0]),
        ] {
            db.execute(
                "INSERT INTO docs VALUES (?, ?)",
                &[Value::Integer(id), Value::Vector(vector)],
            )
            .unwrap();
        }
        assert_eq!(
            db.catalog().table("docs").unwrap().columns[1].ty,
            DataType::QuantizedVector(4)
        );
    }

    let mut db = Database::open(file.path()).unwrap();
    let rows = db
        .query(
            "SELECT id, embedding, vector_score(embedding, ?) AS score \
             FROM docs ORDER BY score DESC LIMIT 2",
            &[Value::Vector(vec![1.0, 0.0, 0.0, 0.0])],
        )
        .unwrap();
    assert_eq!(rows.rows[0][0], Value::Integer(1));
    let Value::Vector(vector) = &rows.rows[0][1] else {
        panic!("stored embedding did not decode as a vector")
    };
    assert_eq!(vector.len(), 4);
}

#[test]
fn a_v3_exact_database_opens_but_cannot_silently_gain_v4_values() {
    let file = TempDb::new("v3-grandfather");

    // Construct the empty single-region v3 layout directly. New databases are
    // v5 and place data after four WAL regions, so changing only a v5 header
    // would not be a valid legacy fixture.
    const PAGE: usize = 4096;
    let mut header = [0u8; 24];
    header[..8].copy_from_slice(b"INLAYSQL");
    header[8..12].copy_from_slice(&(PAGE as u32).to_le_bytes());
    header[12..16].copy_from_slice(&3u32.to_le_bytes());
    let header_checksum = fnv1a(&header[..16]);
    header[16..24].copy_from_slice(&header_checksum.to_le_bytes());
    let mut state = [0u8; 32];
    state[8..16].copy_from_slice(&1u64.to_le_bytes());
    let state_checksum = fnv1a(&state[..24]);
    state[24..32].copy_from_slice(&state_checksum.to_le_bytes());
    let legacy = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(file.path())
        .unwrap();
    legacy.set_len(((2 + 256) * PAGE) as u64).unwrap();
    use std::os::unix::fs::FileExt;
    legacy.write_all_at(&header, 0).unwrap();
    legacy.write_all_at(&state, PAGE as u64).unwrap();
    legacy.sync_all().unwrap();

    {
        let mut db = Database::open(file.path()).unwrap();
        db.execute("CREATE TABLE old (embedding VECTOR(3))", &[])
            .unwrap();
        db.execute(
            "INSERT INTO old VALUES (?)",
            &[Value::Vector(vec![1.0, 2.0, 3.0])],
        )
        .unwrap();
    }

    let mut db = Database::open(file.path()).unwrap();
    let rows = db.query("SELECT embedding FROM old", &[]).unwrap();
    assert_eq!(rows.rows[0][0], Value::Vector(vec![1.0, 2.0, 3.0]));
    db.execute("CREATE TABLE still_exact (embedding VECTOR(3))", &[])
        .unwrap();
    let err = db
        .execute("CREATE TABLE too_new (embedding VECTOR(3, INT8))", &[])
        .unwrap_err();
    assert!(matches!(err, Error::FormatVersion(_)), "got {err}");
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    })
}
