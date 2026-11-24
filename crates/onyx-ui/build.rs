fn main() {
    if let Err(err) = slint_build::compile("ui/main.slint") {
        panic!("Failed to compile Slint UI: {err}");
    }
}
