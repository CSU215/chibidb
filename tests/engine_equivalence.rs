use chaoticdb::value::Value;
use chaoticdb::{Database, ResultSet};

/// Tiny deterministic xorshift so the differential sequence is reproducible
/// without pulling in a dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Builds a deterministic statement script that mixes inserts (including
/// duplicate primary keys), updates, deletes, ordered scans, index lookups,
/// range scans and aggregates.
fn build_script(steps: u64) -> Vec<String> {
    let mut rng = Rng::new(0xC0FFEE);
    let mut next_id = 0u64;
    let mut script = Vec::with_capacity(steps as usize);
    for _ in 0..steps {
        let op = rng.below(100);
        let sql = if op < 45 {
            // mostly fresh ids so the table grows; sometimes reuse one to
            // exercise the duplicate-primary-key path
            let id = if next_id > 0 && rng.below(5) == 0 {
                rng.below(next_id)
            } else {
                let v = next_id;
                next_id += 1;
                v
            };
            let grp =
                if rng.below(4) == 0 { "null".to_string() } else { rng.below(10).to_string() };
            let name = format!("n{}", rng.below(20));
            let score = rng.below(1000) as i64 - 500;
            format!("insert into t values ({id}, {grp}, '{name}', {score});")
        } else if op < 65 {
            let id = rng.below(next_id.max(1));
            let score = rng.below(1000) as i64 - 500;
            format!("update t set score = {score} where id = {id};")
        } else if op < 80 {
            let id = rng.below(next_id.max(1));
            format!("delete from t where id = {id};")
        } else if op < 86 {
            "select id, grp, name, score from t order by id;".to_string()
        } else if op < 90 {
            let id = rng.below(next_id.max(1));
            format!("select id, name from t where id = {id};")
        } else if op < 94 {
            let lo = rng.below(next_id.max(1));
            let hi = lo + rng.below(8);
            format!("select id, score from t where id >= {lo} and id <= {hi} order by id;")
        } else if op < 97 {
            "select count(*), sum(score), min(id), max(id) from t;".to_string()
        } else {
            let grp = rng.below(10);
            format!("select id, name from t where grp = {grp} order by id;")
        };
        script.push(sql);
    }
    script
}

/// Runs one statement, returning the visible rows for selects (empty for
/// non-selects) or an opaque error marker. Errors are compared by presence,
/// not message, since the two engines should agree on *whether* a statement
/// succeeds.
fn query(db: &Database, sql: &str) -> std::result::Result<Vec<Vec<Value>>, ()> {
    match db.execute_sql(sql) {
        Err(_) => Err(()),
        Ok(mut sets) => {
            let first = sets.remove(0);
            Ok(match first {
                ResultSet::Rows { rows, .. } => rows,
                _ => Vec::new(),
            })
        }
    }
}

fn assert_same(a: &Database, b: &Database, sql: &str, step: u64) {
    let ra = query(a, sql);
    let rb = query(b, sql);
    assert_eq!(ra, rb, "step {step}: engines diverged executing `{sql}`");
}

const SNAPSHOT: &str = "select id, grp, name, score from t order by id;";

const CREATE_HEAP: &str =
    "create table t (id int primary key, grp int, name char(16), score int) engine = heap;";
const CREATE_LSM: &str =
    "create table t (id int primary key, grp int, name char(16), score int) engine = lsm;";

fn assert_snapshot(a: &Database, b: &Database, step: u64) {
    assert_same(a, b, SNAPSHOT, step);
}

#[test]
fn heap_and_lsm_agree_on_a_random_workload() {
    let heap_dir = tempfile::tempdir().unwrap();
    let lsm_dir = tempfile::tempdir().unwrap();
    let a = Database::open(heap_dir.path()).unwrap();
    let b = Database::open(lsm_dir.path()).unwrap();
    a.execute_sql(CREATE_HEAP).unwrap();
    b.execute_sql(CREATE_LSM).unwrap();

    for (step, sql) in build_script(500).iter().enumerate() {
        let step = step as u64;
        assert_same(&a, &b, sql, step);
        assert_snapshot(&a, &b, step);

        // periodically push the LSM memtable down to SSTables so the random
        // reads cross the memtable/sstable boundary on both engines
        if step % 50 == 49 {
            a.flush().unwrap();
            b.flush().unwrap();
        }
    }
}

#[test]
fn lsm_matches_heap_across_reopens() {
    let heap_dir = tempfile::tempdir().unwrap();
    let lsm_dir = tempfile::tempdir().unwrap();
    let a = Database::open(heap_dir.path()).unwrap();
    a.execute_sql(CREATE_HEAP).unwrap();
    let mut b = Some(Database::open(lsm_dir.path()).unwrap());
    b.as_ref().unwrap().execute_sql(CREATE_LSM).unwrap();

    let mut reopens = 0u32;
    for (step, sql) in build_script(600).iter().enumerate() {
        let step = step as u64;
        let db = b.as_ref().unwrap();
        assert_same(&a, db, sql, step);
        assert_snapshot(&a, db, step);

        if step % 40 == 39 {
            a.flush().unwrap();
            db.flush().unwrap();
        }

        if step % 150 == 149 {
            // even reopens flush first (SSTables + manifest), odd ones are a
            // plain drop that must rebuild the same state from the WAL
            if reopens.is_multiple_of(2) {
                db.flush().unwrap();
            }
            drop(b.take());
            b = Some(Database::open(lsm_dir.path()).unwrap());
            reopens += 1;
            assert_snapshot(&a, b.as_ref().unwrap(), step);
        }
    }
}
