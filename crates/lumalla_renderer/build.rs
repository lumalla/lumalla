use std::env;
use std::fs;
use std::path::Path;

fn main() {
    pkg_config::Config::new()
        .probe("libdrm")
        .expect("pkg-config could not find libdrm");

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
    compile_wgsl(
        "shaders/composite.vert.wgsl",
        &format!("{out_dir}/composite.vert.spv"),
        naga::ShaderStage::Vertex,
    );
    compile_wgsl(
        "shaders/composite.frag.wgsl",
        &format!("{out_dir}/composite.frag.spv"),
        naga::ShaderStage::Fragment,
    );
}

fn compile_wgsl(path: &str, out_path: &str, stage: naga::ShaderStage) {
    println!("cargo:rerun-if-changed={path}");
    let source = fs::read_to_string(path).unwrap_or_else(|err| {
        panic!("Failed to read shader {path}: {err}");
    });
    let file_name = Path::new(path)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("shader");
    let module = naga::front::wgsl::Frontend::new()
        .parse(&source)
        .unwrap_or_else(|err| panic!("Failed to parse {path}: {err}"));

    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .unwrap_or_else(|err| panic!("Failed to validate {path}: {err}"));

    let options = naga::back::spv::Options {
        lang_version: (1, 5),
        ..Default::default()
    };
    let mut writer = naga::back::spv::Writer::new(&options)
        .unwrap_or_else(|err| panic!("Failed to create SPIR-V writer for {path}: {err:?}"));
    let mut spirv = Vec::new();
    writer
        .write(&module, &info, None, &None, &mut spirv)
        .unwrap_or_else(|err| panic!("Failed to compile {path} to SPIR-V: {err:?}"));
    let bytes = spirv
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .collect::<Vec<u8>>();
    fs::write(out_path, bytes).unwrap_or_else(|err| panic!("Failed to write {out_path}: {err}"));
    let _ = (stage, file_name);
}
