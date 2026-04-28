// Phase 3 Feature D batch D3b — `tokio::join!` with three branches.
// The debugger asserts the await-trace surfaces every sub-future as
// part of the awaitee chain (some branches may be already-resolved
// while others are still suspended; the renderer should not skip
// completed branches when the join itself is still pending).
use std::time::Duration;

#[inline(never)]
fn marker() {
    std::hint::black_box(());
}

async fn branch_a() -> u32 {
    tokio::time::sleep(Duration::from_millis(40)).await;
    1
}

async fn branch_b() -> u32 {
    tokio::time::sleep(Duration::from_millis(60)).await;
    2
}

async fn branch_c() -> u32 {
    tokio::time::sleep(Duration::from_millis(80)).await;
    3
}

async fn joiner() {
    loop {
        let _ = tokio::join!(branch_a(), branch_b(), branch_c());
        marker();
    }
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .build()
        .unwrap();

    runtime.spawn(joiner());

    std::thread::sleep(Duration::from_secs(3));
}
