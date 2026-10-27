use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=app.manifest");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.ends_with("-windows-msvc") {
        let manifest = Path::new("app.manifest");
        assert!(
            manifest.is_file(),
            "embed-manifest: manifest file {} not found",
            manifest.display()
        );
        embed_manifest::embed_manifest_file(manifest)
            .expect("embed-manifest: unable to embed app.manifest");
    }
}
