use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Step A: Rust -> SPIR-V (via rust-gpu's rustc_codegen_spirv, driven by spirv-builder).
    let compile = spirv_builder::SpirvBuilder::new("../shader", "spirv-unknown-vulkan1.2")
        .release(true)
        .build()?;
    let spv_path: PathBuf = compile.module.unwrap_single().into();

    // Step B: SPIR-V -> WGSL (via naga). Ship WGSL to the client so the runtime
    // wasm bundle doesn't have to pull naga in for translation.
    let spv_bytes = std::fs::read(&spv_path)?;
    let module = naga::front::spv::parse_u8_slice(
        &spv_bytes,
        &naga::front::spv::Options::default(),
    )?;
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::default(),
    )
    .validate(&module)?;
    let wgsl = naga::back::wgsl::write_string(
        &module,
        &info,
        naga::back::wgsl::WriterFlags::empty(),
    )?;

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let wgsl_path = out_dir.join("slash0_shader.wgsl");
    std::fs::write(&wgsl_path, wgsl)?;

    println!("cargo::rustc-env=SLASH0_SHADER_WGSL={}", wgsl_path.display());
    println!("cargo::rerun-if-changed=../shader/src/lib.rs");
    println!("cargo::rerun-if-changed=../shader/Cargo.toml");
    // The shader now pulls Hilbert math from slash0-core; regen WGSL when it moves.
    println!("cargo::rerun-if-changed=../core/src");
    Ok(())
}
