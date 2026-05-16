// Phase 3 Feature D batch D3b — `tokio::join!` with three branches.
// The debugger asserts the await-trace surfaces every sub-future as
// part of the awaitee chain. To do that, the runtime needs to be
// paused *while the join is still pending* (post-join the branches'
// `MaybeDone` slots transition to `Gone` and disappear from the
// state machine). The shortest branch calls `marker()` between two
// `.await`s so that, when the debugger breaks at `marker`, branch A
// is mid-poll on this thread while branches B and C are still
// suspended on their longer sleeps — all three live `MaybeDone`
// futures are reachable from the joiner state.
use std::time::Duration;

#[inline(never)]
fn marker() {
    std::hint::black_box(());
}

async fn branch_a() -> u32 {
    tokio::time::sleep(Duration::from_millis(40)).await;
    marker();
    tokio::time::sleep(Duration::from_millis(200)).await;
    1
}

async fn branch_b() -> u32 {
    tokio::time::sleep(Duration::from_millis(300)).await;
    2
}

async fn branch_c() -> u32 {
    tokio::time::sleep(Duration::from_millis(400)).await;
    3
}

async fn joiner() {
    loop {
        let _ = tokio::join!(branch_a(), branch_b(), branch_c());
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
