//! Write one diagram's SVG to stdout: `cargo run --example render -- diagram.mmd`
use std::io::Read;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut src = String::new();
    if let Some(p) = std::env::args().nth(1) {
        std::fs::File::open(p)?.read_to_string(&mut src)?;
    } else {
        std::io::stdin().read_to_string(&mut src)?;
    }
    println!("{}", mmd_svg::render(&src)?);
    Ok(())
}
