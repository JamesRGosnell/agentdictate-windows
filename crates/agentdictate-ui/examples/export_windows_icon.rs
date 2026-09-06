//! Reproduce the Windows icon from the project's original vector artwork.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let tree = resvg::usvg::Tree::from_data(
        &std::fs::read(root.join("assets/agentdictate.svg"))?,
        &resvg::usvg::Options::default(),
    )?;
    let mut pixels = resvg::tiny_skia::Pixmap::new(256, 256).ok_or("icon allocation failed")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::identity(),
        &mut pixels.as_mut(),
    );
    let image = image::load_from_memory(&pixels.encode_png()?)?;
    image.save(root.join("assets/agentdictate.ico"))?;
    Ok(())
}
