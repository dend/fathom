fn main() {
    // Embeds the icon and version details in the Windows executable, so Explorer,
    // the taskbar and the installer show the app icon. The version comes from
    // Cargo.toml, so there is only one place to bump it.
    #[cfg(windows)]
    {
        use std::path::PathBuf;

        println!("cargo:rerun-if-changed=assets/fathom.rc");
        println!("cargo:rerun-if-changed=assets/icons/icon.ico");
        let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let version = std::env::var("CARGO_PKG_VERSION").unwrap();
        let numbers: Vec<&str> = version.split(['.', '-']).take(3).collect();
        let template = std::fs::read_to_string(manifest.join("assets/fathom.rc")).unwrap();
        let icon = manifest
            .join("assets/icons/icon.ico")
            .display()
            .to_string()
            .replace('\\', "/");
        for (name, description) in [("fathom", "Fathom"), ("fth", "Fathom command line tool")] {
            let resource = template
                .replace("@ICON@", &icon)
                .replace("@NAME@", name)
                .replace("@DESCRIPTION@", description)
                .replace("@VERSION@", &version)
                .replace("@VERSION_NUMBERS@", &format!("{},0", numbers.join(",")));
            let generated =
                PathBuf::from(std::env::var("OUT_DIR").unwrap()).join(format!("{name}.rc"));
            std::fs::write(&generated, resource).unwrap();
            embed_resource::compile_for(&generated, [name], embed_resource::NONE)
                .manifest_optional()
                .unwrap();
        }
    }
}
