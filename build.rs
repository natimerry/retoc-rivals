use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const BINDING_NAME: &str = "KawaiiPhysicsBinding";
const EMBED_RS_NAME: &str = "kawaii_binding_embed.rs";

fn main() {
    println!("cargo:rerun-if-env-changed=RETOC_SKIP_KAWAII_BINDING_BUILD");
    println!("cargo:rerun-if-changed=../UAssetAPI/KawaiiPhysicsBinding");
    println!("cargo:rerun-if-changed=../UAssetAPI/KawaiiPhysicsLegacyPorter.cs");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR was not set"));
    let embed_rs = out_dir.join(EMBED_RS_NAME);

    if env::var_os("RETOC_SKIP_KAWAII_BINDING_BUILD").is_some() {
        write_empty_embed_file(&embed_rs);
        println!(
            "cargo:warning=Skipping KawaiiPhysicsBinding build because RETOC_SKIP_KAWAII_BINDING_BUILD is set"
        );
        return;
    }

    let manifest_dir = PathBuf::from(
        env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR was not set"),
    );

    let project = manifest_dir
        .parent()
        .expect("retoc crate must have a parent directory")
        .join("UAssetAPI")
        .join("KawaiiPhysicsBinding")
        .join("KawaiiPhysicsBinding.csproj");

    if !project.exists() {
        panic!("KawaiiPhysicsBinding project not found: {}", project.display());
    }

    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("missing CARGO_CFG_TARGET_OS");
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").expect("missing CARGO_CFG_TARGET_ARCH");

    let rid = runtime_identifier(&target_os, &target_arch).unwrap_or_else(|| {
        panic!("unsupported target for KawaiiPhysicsBinding: {target_os}-{target_arch}")
    });

    let build_root = out_dir.join("kawaii_physics_binding");
    let publish_dir = build_root.join("publish");

    std::fs::create_dir_all(&publish_dir)
        .expect("failed to create KawaiiPhysicsBinding publish dir");

    let nuget_config = build_root.join("NuGet.Config");

    std::fs::write(
        &nuget_config,
        r#"<?xml version="1.0" encoding="utf-8"?>
<configuration>
  <packageSources>
    <clear />
    <add key="nuget.org" value="https://api.nuget.org/v3/index.json" />
  </packageSources>
</configuration>
"#,
    )
    .expect("failed to write local NuGet.Config");

    let status = Command::new("dotnet")
        .arg("publish")
        .arg(&project)
        .arg("--configfile")
        .arg(&nuget_config)
        .arg("--ignore-failed-sources")
        .arg("-c")
        .arg("Release")
        .arg("-r")
        .arg(rid)
        .arg("--self-contained")
        .arg("true")
        .arg("-p:PublishAot=false")
        .arg("-p:PublishSingleFile=true")
        .arg("-p:IncludeNativeLibrariesForSelfExtract=true")
        .arg("-p:EnableCompressionInSingleFile=true")
        .arg("-p:DebugType=embedded")
        .arg("-p:DebugSymbols=false")
        .arg("-o")
        .arg(&publish_dir)
        .env("DOTNET_CLI_HOME", build_root.join("dotnet_home"))
        .env("DOTNET_SKIP_FIRST_TIME_EXPERIENCE", "1")
        .env("DOTNET_NOLOGO", "1")
        .env("APPDATA", build_root.join("appdata"))
        .env("LOCALAPPDATA", build_root.join("localappdata"))
        .env("USERPROFILE", build_root.join("userprofile"))
        .status()
        .expect("failed to run dotnet publish for KawaiiPhysicsBinding");

    if !status.success() {
        panic!("dotnet publish failed for {}", project.display());
    }

    let binding_binary = find_binding_binary(&publish_dir, &target_os).unwrap_or_else(|| {
        panic!(
            "dotnet publish succeeded, but binding binary was not found in {}",
            publish_dir.display()
        )
    });

    write_embed_file(&embed_rs, &binding_binary, &target_os);

    // Local dev convenience:
    // cargo-dist will not package this sidecar automatically, but local `cargo run`
    // can still use the copied helper beside target/debug or target/release.
    if let Some(profile_dir) = cargo_profile_dir(&out_dir) {
        copy_binding_binary(&binding_binary, &profile_dir).unwrap_or_else(|err| {
            panic!(
                "failed to copy KawaiiPhysicsBinding to {}: {err}",
                profile_dir.display()
            )
        });

        let deps_dir = profile_dir.join("deps");
        if deps_dir.exists() {
            copy_binding_binary(&binding_binary, &deps_dir).unwrap_or_else(|err| {
                panic!(
                    "failed to copy KawaiiPhysicsBinding to {}: {err}",
                    deps_dir.display()
                )
            });
        }
    }

    println!(
        "cargo:warning=Built KawaiiPhysicsBinding managed helper at {}",
        binding_binary.display()
    );

    println!(
        "cargo:warning=Embedded KawaiiPhysicsBinding helper via {}",
        embed_rs.display()
    );
}

fn runtime_identifier(target_os: &str, target_arch: &str) -> Option<&'static str> {
    match (target_os, target_arch) {
        ("windows", "x86_64") => Some("win-x64"),
        ("windows", "aarch64") => Some("win-arm64"),

        ("linux", "x86_64") => Some("linux-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),

        ("macos", "x86_64") => Some("osx-x64"),
        ("macos", "aarch64") => Some("osx-arm64"),

        _ => None,
    }
}

fn binding_binary_name(target_os: &str) -> String {
    match target_os {
        "windows" => format!("{BINDING_NAME}.exe"),
        _ => BINDING_NAME.to_string(),
    }
}

fn find_binding_binary(publish_dir: &Path, target_os: &str) -> Option<PathBuf> {
    let binary = publish_dir.join(binding_binary_name(target_os));
    binary.exists().then_some(binary)
}

fn cargo_profile_dir(out_dir: &Path) -> Option<PathBuf> {
    // OUT_DIR usually:
    // target/debug/build/<pkg-hash>/out
    //
    // desired:
    // target/debug
    out_dir.parent()?.parent()?.parent().map(Path::to_path_buf)
}

fn copy_binding_binary(src: &Path, dest_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest_dir)?;

    let file_name = src
        .file_name()
        .expect("binding binary should have a file name");

    let dst = dest_dir.join(file_name);

    std::fs::copy(src, &dst)?;

    Ok(())
}

fn write_embed_file(embed_rs: &Path, binding_binary: &Path, target_os: &str) {
    let binding_name = binding_binary_name(target_os);

    let absolute_binding_path = binding_binary
        .canonicalize()
        .unwrap_or_else(|err| {
            panic!(
                "failed to canonicalize binding binary path {}: {err}",
                binding_binary.display()
            )
        });

    let source = format!(
        r#"
// This file is generated by build.rs. Do not edit by hand.

pub const KAWAII_BINDING_NAME: &str = {binding_name:?};

pub static KAWAII_BINDING_BYTES: &[u8] = include_bytes!({binding_path:?});
"#,
        binding_name = binding_name,
        binding_path = absolute_binding_path.to_string_lossy(),
    );

    std::fs::write(embed_rs, source).unwrap_or_else(|err| {
        panic!(
            "failed to write generated embed file {}: {err}",
            embed_rs.display()
        )
    });
}

fn write_empty_embed_file(embed_rs: &Path) {
    let source = r#"
// This file is generated by build.rs. Do not edit by hand.

pub const KAWAII_BINDING_NAME: &str = "";

pub static KAWAII_BINDING_BYTES: &[u8] = &[];
"#;

    std::fs::write(embed_rs, source).unwrap_or_else(|err| {
        panic!(
            "failed to write generated empty embed file {}: {err}",
            embed_rs.display()
        )
    });
}