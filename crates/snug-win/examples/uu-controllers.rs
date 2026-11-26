//! Read-only probe of the production UU control-list adapter.
//! Pass the native UU window handle as a decimal integer. Never invokes controls.

fn main() {
    let handle = std::env::args()
        .nth(1)
        .and_then(|argument| argument.parse::<u64>().ok())
        .expect("usage: uu-controllers <UU window handle>");
    let uia = snug_win::uia::Uia::new().expect("initialize UI Automation");
    let started = std::time::Instant::now();
    let table = uia.read_controllers(&[handle]);
    // Do not print device names, account details, passwords, or the raw UI tree.
    println!(
        "readable={} controller_count={} elapsed_ms={}",
        table.readable,
        table.names.len(),
        started.elapsed().as_millis()
    );
}
