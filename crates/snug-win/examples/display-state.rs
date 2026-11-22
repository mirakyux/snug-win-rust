//! Read-only display and optional window-geometry probe; never applies topology.
use snug_win::display::{DisplayApi, WinDisplay};

#[link(name = "user32")]
unsafe extern "system" {
    fn SetThreadDpiAwarenessContext(value: isize) -> isize;
}

fn main() {
    // Match the GUI manifest's PerMonitorV2 context so geometry is physical pixels.
    unsafe { SetThreadDpiAwarenessContext(-4); }
    let began = std::time::Instant::now();
    let monitors = WinDisplay::new().active_monitors();
    println!("display_snapshot_ms={}", began.elapsed().as_millis());
    for monitor in monitors {
        println!(
            "device={} virtual_id={:?} bounds={} mode={}x{} dpi={}",
            monitor.device_name,
            monitor.virtual_display_id,
            monitor.bounds.short(),
            monitor.mode.width,
            monitor.mode.height,
            monitor.mode.dpi_scale_percent,
        );
    }
    if let Some(handle) = std::env::args().nth(1).and_then(|s| s.parse::<u64>().ok()) {
        println!("window_bounds={:?}", snug_win::frame_of(handle));
    }
}
