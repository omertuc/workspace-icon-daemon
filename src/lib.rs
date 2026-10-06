//! Dynamic application icons in i3 and Sway workspace names and titlebars.

pub mod assets;
pub mod atspi;
pub mod daemon;
pub mod desktop;
pub mod favicons;
pub mod font_builder;
pub mod icon_map;
pub mod ipc;
pub mod pidfile;
pub mod platform;
pub mod raster;
pub mod xdg;

/// Apply `f` to every item on all CPUs, keeping the order.
pub fn parallel_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let chunk = items.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|chunk| scope.spawn(|| chunk.iter().map(&f).collect::<Vec<R>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("worker panicked"))
            .collect()
    })
}
