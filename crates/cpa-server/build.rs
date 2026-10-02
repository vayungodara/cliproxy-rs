use std::fmt::Write;
use std::path::Path;

fn collect(path: &Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(path)
        .expect("ui/dist is required; run npm run build in ui")
        .flatten()
    {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, files);
        } else {
            files.push(path);
        }
    }
}

fn main() {
    let root = Path::new("../../ui/dist").canonicalize().expect("ui/dist is required");
    println!("cargo:rerun-if-changed=../../ui/dist");
    let mut files = Vec::new();
    collect(&root, &mut files);
    files.sort();
    let mut output =
        String::from("fn dashboard_asset(path: &str) -> Option<(&'static [u8], &'static str)> { match path {\n");
    for path in files {
        let name = format!("/{}", path.strip_prefix(&root).unwrap().display());
        let mime = match path.extension().and_then(|s| s.to_str()).unwrap_or_default() {
            "html" => "text/html; charset=utf-8",
            "js" => "text/javascript; charset=utf-8",
            "css" => "text/css; charset=utf-8",
            "svg" => "image/svg+xml",
            "woff2" => "font/woff2",
            _ => "application/octet-stream",
        };
        writeln!(
            output,
            "{name:?} => Some((include_bytes!({:?}), {mime:?})),",
            path.to_str().unwrap()
        )
        .unwrap();
    }
    output.push_str("_ => None, } }\n");
    std::fs::write(
        Path::new(&std::env::var("OUT_DIR").unwrap()).join("dashboard.rs"),
        output,
    )
    .unwrap();
}
