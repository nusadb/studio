//! Compile the Slint UI (`ui/app.slint`) into generated Rust consumed via
//! `slint::include_modules!()` in `main.rs`.

fn main() {
    slint_build::compile("ui/app.slint").expect("compile ui/app.slint");
}
