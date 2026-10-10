// Embeds Windows version information (product name, version, description) in hwcast.exe.
// Code signing enforces these values, so they must stay "hwcast" and the Cargo version.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set("ProductName", "hwcast")
            .set("FileDescription", "hwcast hardware dashboard")
            .set("InternalName", "hwcast")
            .set("OriginalFilename", "hwcast.exe")
            .set("CompanyName", "hwcast")
            .set("LegalCopyright", "Copyright (c) 2026 Shreyam. MIT License.");
        res.compile().expect("failed to embed the Windows version resource");
    }
}
