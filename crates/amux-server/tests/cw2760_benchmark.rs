use std::{path::Path, sync::Arc, time::Instant};

#[tokio::test]
#[ignore]
async fn copied_db_attribute_tasks_benchmark() {
    let p = std::env::var("CW2760_DB").unwrap_or_else(|_| "/models-fast/lmqr/jobs/CW-2760/board-copy.db".into());
    let path = Path::new(&p);
    let store = Arc::new(amux_server::db::Store::open(path).unwrap());
    let start = Instant::now();
    amux_server::runtime_jobs::token_ledger::attribute_tasks(&store).await.unwrap();
    let elapsed = start.elapsed();
    println!("attribute_tasks elapsed_ms={:.3}", elapsed.as_secs_f64() * 1000.0);
    assert!(elapsed.as_secs_f64() < 0.100, "attribution exceeded 100 ms");
}
