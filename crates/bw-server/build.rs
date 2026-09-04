// The viewer (web/dist) is built by `make web` and embedded with include_str!. Without it the
// include would fail with a bare "No such file" error, so say what to do instead.
fn main() {
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    if !["index.html", "app.js", "app.css"].iter().all(|f| dist.join(f).exists()) {
        eprintln!("web/dist is missing: run `make web` (Node 24) or `make`, which builds the viewer before the binary");
        std::process::exit(1);
    }
}
