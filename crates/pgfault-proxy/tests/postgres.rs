//! Run through scripts/driver_matrix.py; requires a live PostgreSQL fixture.
use tokio_postgres::{Client, NoTls};
async fn connect(url: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}
#[tokio::test]
#[ignore = "requires PGFAULT_DIRECT, PGFAULT_PROXY and PGFAULT_FAULT; use scripts/driver_matrix.py"]
async fn real_postgres_transparency_and_ambiguous_commit() {
    let direct = std::env::var("PGFAULT_DIRECT").unwrap();
    for url in [&direct, &std::env::var("PGFAULT_PROXY").unwrap()] {
        let mut c = connect(url).await;
        for i in 0i32..10 {
            assert_eq!(
                c.query_one("SELECT $1::int", &[&i])
                    .await
                    .unwrap()
                    .get::<_, i32>(0),
                i
            );
        }
        assert!(!c.simple_query("SELECT 42").await.unwrap().is_empty());
        let tx = c.transaction().await.unwrap();
        assert!(tx.query_one("SELECT 1/0", &[]).await.is_err());
        tx.rollback().await.unwrap();
        assert_eq!(
            c.query_one("SELECT 42", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
            42
        );
    }
    let oracle = connect(&direct).await;
    let table = format!(
        "pgfault_rust_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    oracle
        .batch_execute(&format!("CREATE TABLE {table} (id int)"))
        .await
        .unwrap();
    let mut c = connect(&std::env::var("PGFAULT_FAULT").unwrap()).await;
    let tx = c.transaction().await.unwrap();
    tx.execute(&format!("INSERT INTO {table} VALUES ($1)"), &[&1i32])
        .await
        .unwrap();
    let failed = tx.commit().await.is_err();
    let count = oracle
        .query_one(&format!("SELECT count(*) FROM {table}"), &[])
        .await
        .unwrap()
        .get::<_, i64>(0);
    oracle
        .batch_execute(&format!("DROP TABLE {table}"))
        .await
        .unwrap();
    assert!(failed, "COMMIT must fail at the client");
    assert_eq!(count, 1, "durable write must exist");
}
