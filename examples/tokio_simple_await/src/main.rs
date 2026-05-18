// Phase 3 Feature D batch D3b — exercises a single async fn with
// one .await. The debugger sets a breakpoint on `marker` and reads
// the await-trace; the only suspended frame should report
// `worker_task` paused at the `tokio::time::sleep` await point on
// the line marked `// AWAIT_HERE` below.
use std::time::Duration;

#[inline(never)]
fn marker() {
    std::hint::black_box(());
}

async fn worker_task() {
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await; // AWAIT_HERE
        marker();
    }
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .build()
        .unwrap();

    runtime.spawn(worker_task());

    std::thread::sleep(Duration::from_secs(2));
}
