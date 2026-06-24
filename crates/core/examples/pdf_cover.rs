//! Verify PDF first-page cover rendering against a real file.
//! Usage: cargo run -p libgen-core --example pdf_cover -- <pdf-path>
use libgen_core::cover_gen;
use std::path::Path;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: pdf_cover <pdf-path>");
    let p = Path::new(&path);
    match cover_gen::render_pdf_first_page(p) {
        Some(b) => println!("render_pdf_first_page: OK — {} bytes (png)", b.len()),
        None => println!("render_pdf_first_page: None (no pdftoppm/mutool, or render failed)"),
    }
    match cover_gen::generate_cover(p, "Some Title", "Some Author") {
        Some(b) => println!(
            "generate_cover:        OK — {} bytes (normalized jpeg)",
            b.len()
        ),
        None => println!("generate_cover:        None"),
    }
}
