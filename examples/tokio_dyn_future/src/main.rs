// Phase 3 Feature D batch D3b — `Pin<Box<dyn Future>>` awaitee. The
// debugger asserts D2b's vtable-driven concrete-type lift surfaces
// the underlying `inner_concrete` future in the await-trace's
// `Custom` frame.
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

#[inline(never)]
fn marker() {
    std::hint::black_box(());
}

async fn inner_concrete() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

fn boxed_future() -> Pin<Box<dyn Future<Output = ()> + Send>> {
    Box::pin(inner_concrete())
}

async fn driver() {
    loop {
        let fut = boxed_future();
        fut.await;
        marker();
    }
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_time()
        .build()
        .unwrap();

    runtime.spawn(driver());

    std::thread::sleep(Duration::from_secs(2));
}
