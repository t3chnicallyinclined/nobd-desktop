// Embeds the NOBD logo as the Windows .exe icon (shown in Explorer and on the
// desktop shortcut). Reuses the exact same rasterizer the app uses at runtime —
// `include!` pulls in `rgba()` from src/logo.rs so there's a single source of
// truth for the artwork.

include!("src/logo.rs");

fn main() {
    println!("cargo:rerun-if-changed=src/logo.rs");
    println!("cargo:rerun-if-changed=build.rs");

    // Only the Windows resource compiler step is platform-specific.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico_path = out_dir.join("nobd.ico");

    // Multi-resolution .ico so Windows picks a crisp size for every context
    // (16/24/32 in lists, 48 on the desktop, 256 for large/tile views).
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16u32, 24, 32, 48, 64, 128, 256] {
        let image = ico::IconImage::from_rgba_data(size, size, rgba(size, true));
        dir.add_entry(ico::IconDirEntry::encode(&image).expect("encode ico entry"));
    }
    let file = std::fs::File::create(&ico_path).expect("create nobd.ico");
    dir.write(file).expect("write nobd.ico");

    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_path.to_str().unwrap());
    res.compile().expect("embed .exe icon resource");
}
