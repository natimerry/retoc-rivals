use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const BINDING_NAME: &str = "KawaiiPhysicsBinding";

fn main() {
    println!("cargo:rerun-if-env-changed=RETOC_SKIP_KAWAII_BINDING_BUILD");
    println!("cargo:rerun-if-changed=../UAssetAPI/KawaiiPhysicsBinding");
    println!("cargo:rerun-if-changed=../UAssetAPI/KawaiiPhysicsLegacyPorter.cs");

    if env::var_os("RETOC_SKIP_KAWAII_BINDING_BUILD").is_some() {
        println!(
            "cargo:warning=Skipping KawaiiPhysicsBinding build because RETOC_SKIP_KAWAII_BINDING_BUILD is set"
        );
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

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

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
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
        // .arg("-p:OutputType=Exe")
        .arg("-p:PublishAot=false")
        .arg("-p:PublishSingleFile=false")
        .arg("-p:NativeLib=")
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

    let profile_dir = cargo_profile_dir(&out_dir).unwrap_or_else(|| {
        panic!(
            "failed to derive Cargo profile dir from OUT_DIR={}",
            out_dir.display()
        )
    });

    copy_publish_output(&publish_dir, &profile_dir).unwrap_or_else(|err| {
        panic!(
            "failed to copy KawaiiPhysicsBinding publish output to {}: {err}",
            profile_dir.display()
        )
    });

    let deps_dir = profile_dir.join("deps");
    if deps_dir.exists() {
        copy_publish_output(&publish_dir, &deps_dir).unwrap_or_else(|err| {
            panic!(
                "failed to copy KawaiiPhysicsBinding publish output to {}: {err}",
                deps_dir.display()
            )
        });
    }

    let deployed_binary = profile_dir.join(binding_binary.file_name().unwrap());

    println!(
        "cargo:warning=Built KawaiiPhysicsBinding managed exe at {}",
        binding_binary.display()
    );

    println!(
        "cargo:warning=Copied KawaiiPhysicsBinding beside Cargo executable at {}",
        deployed_binary.display()
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

fn copy_publish_output(publish_dir: &Path, dest_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest_dir)?;

    for entry in std::fs::read_dir(publish_dir)? {
        let entry = entry?;
        let src = entry.path();
        let dst = dest_dir.join(entry.file_name());

        if src.is_dir() {
            copy_dir_recursive(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }

    Ok(())
}

fn copy_dir_recursive(src_dir: &Path, dst_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst_dir)?;

    for entry in std::fs::read_dir(src_dir)? {
        let entry = entry?;
        let src = entry.path();
        let dst = dst_dir.join(entry.file_name());

        if src.is_dir() {
            copy_dir_recursive(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }

    Ok(())
}