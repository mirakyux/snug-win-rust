//! Read-only display and optional window-geometry probe; never applies topology.
use snug_win::display::{DisplayApi, WinDisplay};

fn main() {
    for monitor in WinDisplay::new().active_monitors() {
        println!(
            "device={} virtual_id={:?} bounds={} dpi={}",
            monitor.device_name,
            monitor.virtual_display_id,
            monitor.bounds.short(),
            monitor.mode.dpi_scale_percent,
        );
    }
    if let Some(handle) = std::env::args().nth(1).and_then(|s| s.parse::<u64>().ok()) {
        println!("window_bounds={:?}", snug_win::frame_of(handle));
    }
}
