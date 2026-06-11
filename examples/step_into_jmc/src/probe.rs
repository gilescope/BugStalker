// Throwaway probe binary for exercising "step into, skip libraries"
// over Vec / nested-user-call shapes. Not a test; driven by hand.
fn compute(x: &i32) -> i32 {
    x * 2
}

fn helper(v: &[i32]) -> i32 {
    let mut total = 0;
    for x in v {
        total += compute(x);
    }
    total
}

fn main() {
    let v = vec![1, 2, 3];
    let r = helper(&v);
    println!("{r}");
}
