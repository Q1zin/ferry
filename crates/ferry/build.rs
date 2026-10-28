use std::{env, fs, path::Path};

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    println!("cargo:rerun-if-changed={}", dir.display());
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect(root, &path, out);
        } else if kind.is_file() {
            println!("cargo:rerun-if-changed={}", path.display());
            if let (Ok(relative), Ok(full)) = (path.strip_prefix(root), path.canonicalize()) {
                out.push((
                    format!("/{}", relative.to_string_lossy().replace('\\', "/")),
                    full.to_string_lossy().into_owned(),
                ));
            }
        }
    }
}

fn main() {
    let root = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../apps/ferry/dist");
    let mut files = Vec::new();
    collect(&root, &root, &mut files);
    files.sort();
    let output = Path::new(&env::var("OUT_DIR").unwrap()).join("dashboard-assets");
    fs::create_dir_all(&output).unwrap();
    let mut source = String::from("pub static ASSETS: &[(&str, &[u8])] = &[\n");
    for (index, (name, path)) in files.into_iter().enumerate() {
        // Include a build-owned snapshot: Vite can replace dist after this script ends.
        let copied = output.join(index.to_string());
        fs::copy(&path, &copied).expect(
            "dashboard assets changed during build; finish bun run build before cargo build",
        );
        let path = copied.to_string_lossy();
        source.push_str(&format!("({name:?}, include_bytes!({path:?})),\n"));
    }
    source.push_str("];\n");
    fs::write(
        Path::new(&env::var("OUT_DIR").unwrap()).join("dashboard_assets.rs"),
        source,
    )
    .unwrap();
}
