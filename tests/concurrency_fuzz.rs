//! Randomized concurrent schedule testing.
//!
//! Several sessions run a random interleaving of transfers (atomic, balance
//! conserving), duplicate-key insert attempts (must fail), and reads. Two
//! invariants must hold at every observable point:
//!
//! * the total balance is conserved -- a transfer debits one account and credits
//!   another inside one transaction, so any snapshot sees the initial sum;
//! * the primary key stays unique -- re-inserting an existing id must fail.
//!
//! A lost update, a torn multi-statement commit, a duplicate row, or a missed
//! conflict all break one of these. Seeds are fixed so a failure is
//! reproducible.

use std::sync::Arc;

use chibidb::value::Value;
use chibidb::{Database, ResultSet, Session};

/// A small deterministic xorshift so schedules are reproducible from a seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9e37_79b9_7f4a_7c15 | 1)
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

fn scalar(db: &Database, sql: &str) -> i64 {
    match db.execute_sql(sql).unwrap().remove(0) {
        ResultSet::Rows { rows, .. } => match rows[0][0] {
            Value::Int(n) => n,
            ref other => panic!("expected int from {sql}, got {other:?}"),
        },
        other => panic!("expected rows from {sql}, got {other:?}"),
    }
}

fn dump(db: &Database) -> String {
    let rows = match db.execute_sql("select id, bal from acct order by id;").unwrap().remove(0) {
        ResultSet::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    };
    format!(
        "rows={} sum={} data={rows:?}",
        rows.len(),
        scalar(db, "select sum(bal) from acct;")
    )
}

const ACCOUNTS: i64 = 8;
const PER_ACCOUNT: i64 = 100;
const TOTAL: i64 = ACCOUNTS * PER_ACCOUNT;

fn run_schedule(seed: u64, threads: u64, iters: u64) {
    let db = Arc::new(Database::open_in_memory().unwrap());
    db.execute_sql("create table acct (id int primary key, bal int);").unwrap();
    for id in 0..ACCOUNTS {
        db.execute_sql(&format!("insert into acct values ({id}, {PER_ACCOUNT});")).unwrap();
    }

    std::thread::scope(|scope| {
        for tid in 0..threads {
            let db = Arc::clone(&db);
            scope.spawn(move || {
                let mut rng = Rng::new(seed.wrapping_mul(1_000_003).wrapping_add(tid + 1));
                let mut session = Session::new();
                for step in 0..iters {
                    let ctx = format!("seed={seed} tid={tid} step={step}");
                    // keep the session clean even if a previous op aborted
                    db.rollback_session(&mut session).unwrap();
                    match rng.below(4) {
                        0 => {
                            // atomic transfer between two distinct accounts
                            let a = rng.below(ACCOUNTS as u64) as i64;
                            let mut b = rng.below(ACCOUNTS as u64) as i64;
                            if b == a {
                                b = (b + 1) % ACCOUNTS;
                            }
                            let sql = format!(
                                "begin; \
                                 update acct set bal = bal - 1 where id = {a}; \
                                 update acct set bal = bal + 1 where id = {b}; \
                                 commit;"
                            );
                            // a deadlock/timeout may abort the transfer; that is
                            // fine as long as the transaction is discarded whole
                            let _ = db.execute_sql_with(&mut session, &sql);
                            let sum = scalar(&db, "select sum(bal) from acct;");
                            assert_eq!(sum, TOTAL, "{ctx} {}", dump(&db));
                        }
                        1 => {
                            // conservation must hold in every observable snapshot
                            let sum = scalar(&db, "select sum(bal) from acct;");
                            assert_eq!(sum, TOTAL, "{ctx} {}", dump(&db));
                        }
                        2 => {
                            // re-inserting an existing primary key must fail
                            let id = rng.below(ACCOUNTS as u64);
                            let dup = db.execute_sql_with(
                                &mut session,
                                &format!("insert into acct values ({id}, 0);"),
                            );
                            assert!(dup.is_err(), "duplicate key accepted: {ctx} {}", dump(&db));
                        }
                        _ => {
                            let id = rng.below(ACCOUNTS as u64);
                            let _ = db
                                .execute_sql_with(&mut session, &format!("select bal from acct where id = {id};"))
                                .unwrap();
                        }
                    }
                }
            });
        }
    });

    assert_eq!(scalar(&db, "select sum(bal) from acct;"), TOTAL, "seed={seed} total");
    assert_eq!(scalar(&db, "select count(*) from acct;"), ACCOUNTS, "seed={seed} rows");
    assert_eq!(
        scalar(&db, "select count(distinct id) from acct;"),
        ACCOUNTS,
        "seed={seed} distinct keys"
    );
}

#[test]
fn randomized_schedules_conserve_state() {
    for seed in 0..4 {
        run_schedule(seed, 6, 200);
    }
}
