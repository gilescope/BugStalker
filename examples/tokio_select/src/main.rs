// Phase 3 Feature D batch D3b — `tokio::select!` with multiple
// branches. The debugger asserts the await-trace identifies which
// branch's awaitee is currently active. The "fast" branch (50ms)
// completes first; the "slow" branch (500ms) is still pending when
// `marker()` runs.
use std::time::Duration;

#[inline(never)]
fn marker() {
    std::hint::black_box(());
}

async fn fast() -> u32 {
    tokio::time::sleep(Duration::from_millis(50)).await;
    1
}

async fn slow() -> u32 {
    tokio::time::sleep(Duration::from_millis(500)).await;
    2
}

async fn racer() {
    loop {
        let _ = tokio::select! {
            v = fast() => v, // FAST_BRANCH
            v = slow() => v, // SLOW_BRANCH
        };
        marker();
    }
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .build()
        .unwrap();

    runtime.spawn(racer());

    std::thread::sleep(Duration::from_secs(3));
}
