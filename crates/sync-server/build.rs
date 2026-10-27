fn main() {
    println!("cargo:rerun-if-changed=../../web/dist");
    assert!(
        std::path::Path::new("../../web/dist/sync.html").is_file(),
        "build the embedded UI first: pnpm --dir web build"
    );
}
