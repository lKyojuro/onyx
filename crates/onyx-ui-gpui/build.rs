fn main() -> std::io::Result<()> {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }

    let icon = "../../assets/branding/onyx.ico";
    println!("cargo:rerun-if-changed={icon}");
    winresource::WindowsResource::new()
        // Keep the resource ID in sync with set_window_icon_from_exe.
        .set_icon_with_id(icon, "1")
        .set("ProductName", "Onyx")
        .set(
            "FileDescription",
            "Onyx — Next-Generation Windows File Explorer",
        )
        .set("LegalCopyright", "Copyright © Onyx Contributors")
        .compile_for("onyx-ui-gpui")
}
