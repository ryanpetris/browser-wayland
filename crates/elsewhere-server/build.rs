use std::{env, fs, path::Path};

fn collect(dir: &Path, root: &Path, entries: &mut String) {
    let mut paths: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect(&path, root, entries);
        } else {
            let name = path.strip_prefix(root).unwrap().to_str().unwrap();
            let mime = match path.extension().and_then(|e| e.to_str()) {
                Some("js") => "text/javascript",
                Some("css") => "text/css",
                Some("html") => "text/html",
                _ => "text/plain; charset=utf-8",
            };
            entries.push_str(&format!("({:?}, {:?}, include_bytes!({:?})),\n", name, mime, path));
        }
    }
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/dist");
    if !["index.html", "app.js", "app.css"].iter().all(|name| root.join(name).is_file()) {
        eprintln!("web/dist is missing: run make web with Node 24, or make to build the viewer and binary");
        std::process::exit(1);
    }
    let root = root.canonicalize().unwrap();
    println!("cargo:rerun-if-changed={}", root.display());
    let mut entries = String::from("&[\n");
    if root.join("assets").is_dir() {
        collect(&root.join("assets"), &root, &mut entries);
    }
    entries.push_str("]");
    fs::write(Path::new(&env::var_os("OUT_DIR").unwrap()).join("web_assets.rs"), entries).unwrap();
}
