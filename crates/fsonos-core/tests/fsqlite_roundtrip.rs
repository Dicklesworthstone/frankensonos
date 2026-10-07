//! FND-DEPS proof: fsqlite does a real open → create → insert → query
//! round-trip against a file on disk, and the rows survive close + reopen.
//!
//! Two supported ways to drive it, both exercised here:
//! - the raw `!Send` `Connection` (async, no `Cx`) driven by our asupersync
//!   runtime. Its engine futures are deeply nested and overflow a default
//!   thread stack, so it must run on a large-stack thread (fsqlite itself uses
//!   32 MiB for its own worker);
//! - `AsyncConnection`, a `Send` handle over a dedicated large-stack worker
//!   that owns the `Connection`; its `*_sync` methods suit the synchronous
//!   `Store` trait directly.

use asupersync::runtime::{RuntimeBuilder, reactor::create_reactor};
use fsqlite::{AsyncConnection, Connection, Row, SqliteValue};
use std::future::Future;
use std::thread;

const STACK_BYTES: usize = 32 * 1024 * 1024;

const CREATE: &str = "CREATE TABLE play_history (\
    id INTEGER PRIMARY KEY, zone TEXT NOT NULL, \
    source_uri TEXT NOT NULL, played_at INTEGER NOT NULL)";
const INSERT: &str = "INSERT INTO play_history (zone, source_uri, played_at) VALUES (?1, ?2, ?3)";
const SELECT: &str = "SELECT zone, source_uri, played_at FROM play_history ORDER BY played_at";
const COUNT: &str = "SELECT count(*), max(source_uri) FROM play_history";

fn params(i: i64) -> [SqliteValue; 3] {
    [
        SqliteValue::from("Ada\u{2019}s Studio"),
        SqliteValue::from(format!("spotify:track:{i}")),
        SqliteValue::from(1_700_000_000 + i),
    ]
}

fn text(row: &Row, i: usize) -> &str {
    row.get(i)
        .and_then(SqliteValue::as_text)
        .expect("TEXT column")
}

fn int(row: &Row, i: usize) -> i64 {
    row.get(i)
        .and_then(SqliteValue::as_integer)
        .expect("INTEGER column")
}

fn assert_inserted(rows: &[Row]) {
    assert_eq!(rows.len(), 2);
    assert_eq!(text(&rows[0], 0), "Ada\u{2019}s Studio");
    assert_eq!(text(&rows[1], 1), "spotify:track:1");
    assert_eq!(int(&rows[1], 2), 1_700_000_001);
}

fn assert_reopened(rows: &[Row]) {
    assert_eq!(int(&rows[0], 0), 2);
    assert_eq!(text(&rows[0], 1), "spotify:track:1");
}

fn block_on<F: Future>(fut: F) -> F::Output {
    RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("reactor"))
        .build()
        .expect("runtime")
        .block_on(fut)
}

fn on_large_stack(f: impl FnOnce() + Send + 'static) {
    thread::Builder::new()
        .stack_size(STACK_BYTES)
        .spawn(f)
        .expect("spawn large-stack thread")
        .join()
        .expect("large-stack thread");
}

#[test]
fn raw_connection_round_trip_survives_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fsonos.db").to_string_lossy().into_owned();
    on_large_stack(move || {
        block_on(async {
            let conn = Box::pin(Connection::open(path.clone()))
                .await
                .expect("open");
            conn.execute(CREATE).await.expect("create");
            for i in 0..2 {
                let n = conn
                    .execute_with_params(INSERT, &params(i))
                    .await
                    .expect("insert");
                assert_eq!(n, 1);
            }
            assert_inserted(&conn.query(SELECT).await.expect("query"));
            Box::pin(conn.close()).await.expect("close");
        });
        // A fresh runtime and connection sees the committed rows on disk.
        block_on(async {
            let conn = Box::pin(Connection::open(path)).await.expect("reopen");
            assert_reopened(&conn.query(COUNT).await.expect("query after reopen"));
            Box::pin(conn.close()).await.expect("close");
        });
    });
}

#[test]
fn async_connection_sync_api_round_trip_survives_reopen() {
    fn assert_send<T: Send>() {}
    assert_send::<AsyncConnection>();

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fsonos.db").to_string_lossy().into_owned();

    let mut conn = AsyncConnection::open_sync(path.clone()).expect("open");
    conn.execute_sync(CREATE).expect("create");
    for i in 0..2 {
        assert_eq!(
            conn.execute_with_params_sync(INSERT, &params(i))
                .expect("insert"),
            1
        );
    }
    assert_inserted(&conn.query_sync(SELECT).expect("query"));
    conn.close_sync().expect("close");

    let mut conn = AsyncConnection::open_sync(path).expect("reopen");
    assert_reopened(&conn.query_sync(COUNT).expect("query after reopen"));
    conn.close_sync().expect("close");
}
