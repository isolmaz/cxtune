use std::path::PathBuf;

fn main() {
    // The tests look elements up by label, which needs the debug info; release builds leave it out.
    let debug_info = std::env::var("PROFILE").is_ok_and(|profile| profile == "debug");
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent".into())
        .with_debug_info(debug_info)
        .with_bundled_translations("lang");
    slint_build::compile_with_config("ui/app.slint", config).unwrap();

    let icon = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("assets")
        .join("app.ico");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("app.rc");
    std::fs::write(&out, resource_script(&icon.display().to_string())).unwrap();
    println!("cargo:rerun-if-changed=assets/app.ico");
    embed_resource::compile(&out, embed_resource::NONE)
        .manifest_required()
        .unwrap();
}

/// The Windows resource script: the application icon and the version information that Explorer
/// shows, taken from the package version.
fn resource_script(icon: &str) -> String {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut numbers = version
        .split('.')
        .map(|part| part.parse::<u16>().expect("a numeric package version"));
    let (major, minor, patch) = (
        numbers.next().unwrap(),
        numbers.next().unwrap(),
        numbers.next().unwrap(),
    );
    let icon = icon.replace('\\', "\\\\");
    format!(
        r#"1 ICON "{icon}"

1 VERSIONINFO
FILEVERSION     {major}, {minor}, {patch}, 0
PRODUCTVERSION  {major}, {minor}, {patch}, 0
FILEOS          0x00040004L
FILETYPE        0x00000001L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904B0"
        BEGIN
            VALUE "CompanyName",      "cxtune\0"
            VALUE "FileDescription",  "cxtune\0"
            VALUE "FileVersion",      "{major}.{minor}.{patch}.0\0"
            VALUE "InternalName",     "cxtune-gui\0"
            VALUE "OriginalFilename", "cxtune-gui.exe\0"
            VALUE "ProductName",      "cxtune\0"
            VALUE "ProductVersion",   "{major}.{minor}.{patch}.0\0"
            VALUE "LegalCopyright",   "MIT License\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0409, 0x04B0
    END
END
"#
    )
}
