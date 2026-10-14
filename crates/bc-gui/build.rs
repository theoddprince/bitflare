fn main() {
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new().with_style("fluent".into()),
    )
    .expect("slint compile");

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("ui/assets/bitflare.ico")
            .set("ProductName", "Bitflare")
            .set("FileDescription", "Bitflare Disc Burning Suite")
            .set("InternalName", "bitflare")
            .set("OriginalFilename", "bitflare.exe");
        res.compile().expect("embed Windows resources");
    }
}
