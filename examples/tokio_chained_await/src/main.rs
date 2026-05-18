// Phase 3 Feature D batch D3b — three nested async fns. The
// debugger asserts that the await-trace contains three AsyncFn
// frames (outer → middle → inner) all in `Suspend` state, each with
// recovered source coords from D1.
use std::time::Duration;

#[inline(never)]
fn marker() {
    std::hint::black_box(());
}

async fn inner() {
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await; // INNER_AWAIT
        marker();
    }
}

async fn middle() {
    loop {
        inner().await; // MIDDLE_AWAIT
    }
}

async fn outer() {
    loop {
        middle().await; // OUTER_AWAIT
    }
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .build()
        .unwrap();

    runtime.spawn(outer());

    std::thread::sleep(Duration::from_secs(2));
}
