//! Native GTK4 + WebKitGTK 6.0 answer viewer (`hey-computer-viewer`).

#[path = "../viewer/mod.rs"]
mod viewer;

fn main() -> anyhow::Result<()> {
    viewer::run()
}
