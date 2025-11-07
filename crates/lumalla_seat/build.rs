use std::env;
use std::path::PathBuf;

fn main() {
    let lib = pkg_config::Config::new()
        .probe("libseat")
        .expect("pkg-config could not find libseat");

    println!("cargo:rerun-if-changed=wrapper.h");

    let mut bindgen_builder = bindgen::Builder::default()
        .header("wrapper.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate_comments(true);

    // Add include paths from pkg-config
    for path in &lib.include_paths {
        bindgen_builder = bindgen_builder.clang_arg(format!("-I{}", path.display()));
    }

    let bindings = bindgen_builder
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    let bindings_file = out_path.join("bindings.rs");
    bindings
        .write_to_file(&bindings_file)
        .expect("Couldn't write bindings!");
}
