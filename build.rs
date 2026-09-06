use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const BINDING_NAME: &str = "KawaiiPhysicsBinding";
const BINDING_DIR_NAME: &str = BINDING_NAME;
const BINDING_ASSEMBLY_NAME: &str = "KawaiiPhysicsBinding.dll";
const BINDING_RUNTIME_CONFIG_NAME: &str = "KawaiiPhysicsBinding.runtimeconfig.json";
const EMBED_RS_NAME: &str = "kawaii_binding_embed.rs";
const STAMP_FILE_NAME: &str = ".retoc-kawaii-binding.stamp";

fn main() {
    println!("cargo:rerun-if-env-changed=RETOC_SKIP_KAWAII_BINDING_BUILD");
    println!("cargo:rerun-if-env-changed=RETOC_KAWAII_BINDING_SELF_CONTAINED");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_DOTNET_SELF_CONTAINED");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR was not set"));
    let embed_rs = out_dir.join(EMBED_RS_NAME);

    if env::var_os("RETOC_SKIP_KAWAII_BINDING_BUILD").is_some() {
        write_empty_embed_file(&embed_rs);
        println!(
            "cargo:warning=Skipping KawaiiPhysicsBinding build because RETOC_SKIP_KAWAII_BINDING_BUILD is set"
        );
        return;
    }

    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR was not set"));
    let uasset_api_dir = manifest_dir
        .parent()
        .expect("retoc crate must have a parent directory")
        .join("UAssetAPI");
    emit_rerun_if_changed_for_dotnet_sources(&uasset_api_dir);

    let project = uasset_api_dir
        .join("KawaiiPhysicsBinding")
        .join("KawaiiPhysicsBinding.csproj");

    if !project.exists() {
        panic!(
            "KawaiiPhysicsBinding project not found: {}",
            project.display()
        );
    }

    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("missing CARGO_CFG_TARGET_OS");
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").expect("missing CARGO_CFG_TARGET_ARCH");

    let rid = runtime_identifier(&target_os, &target_arch).unwrap_or_else(|| {
        panic!("unsupported target for KawaiiPhysicsBinding: {target_os}-{target_arch}")
    });

    let build_root = out_dir.join("kawaii_physics_binding");
    let publish_dir = build_root.join("publish");
    let self_contained = env_flag("RETOC_KAWAII_BINDING_SELF_CONTAINED")
        || env::var_os("CARGO_FEATURE_DOTNET_SELF_CONTAINED").is_some();

    if publish_dir.exists() {
        std::fs::remove_dir_all(&publish_dir)
            .expect("failed to clean KawaiiPhysicsBinding publish dir");
    }
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

    let mut publish = Command::new("dotnet");
    publish
        .arg("publish")
        .arg(&project)
        .arg("--configfile")
        .arg(&nuget_config)
        .arg("--ignore-failed-sources")
        .arg("-c")
        .arg("Release")
        .arg("-r")
        .arg(rid)
        .arg(format!(
            "-p:KawaiiPhysicsBindingSelfContained={}",
            if self_contained { "true" } else { "false" }
        ))
        .arg("-p:GenerateRuntimeConfigurationFiles=true")
        .arg("-p:RollForward=Major")
        .arg("-p:PublishAot=false")
        .arg("-p:PublishSingleFile=false")
        .arg("-p:PublishTrimmed=false")
        .arg("-p:DebugType=embedded")
        .arg("-p:DebugSymbols=false")
        .arg("-o")
        .arg(&publish_dir)
        .env("DOTNET_CLI_HOME", build_root.join("dotnet_home"))
        .env("DOTNET_SKIP_FIRST_TIME_EXPERIENCE", "1")
        .env("DOTNET_NOLOGO", "1")
        .env("APPDATA", build_root.join("appdata"))
        .env("LOCALAPPDATA", build_root.join("localappdata"))
        .env("USERPROFILE", build_root.join("userprofile"));

    let status = publish
        .status()
        .expect("failed to run dotnet publish for KawaiiPhysicsBinding");

    if !status.success() {
        panic!("dotnet publish failed for {}", project.display());
    }

    let binding_assembly = publish_dir.join(BINDING_ASSEMBLY_NAME);
    if !binding_assembly.exists() {
        panic!(
            "dotnet publish succeeded, but {} was not found in {}",
            BINDING_ASSEMBLY_NAME,
            publish_dir.display()
        )
    }

    let runtime_config = publish_dir.join(BINDING_RUNTIME_CONFIG_NAME);
    if !runtime_config.exists() {
        panic!(
            "dotnet publish succeeded, but {} was not found in {}",
            BINDING_RUNTIME_CONFIG_NAME,
            publish_dir.display()
        );
    }

    let binding_executable_name = binding_executable_name(&target_os);
    if self_contained && !publish_dir.join(binding_executable_name).exists() {
        panic!(
            "dotnet publish succeeded, but {} was not found in {}",
            binding_executable_name,
            publish_dir.display()
        );
    }

    let publish_files = collect_publish_files(&publish_dir);
    if publish_files.is_empty() {
        panic!(
            "dotnet publish succeeded, but no publish artifacts were found in {}",
            publish_dir.display()
        );
    }
    let binding_stamp = binding_stamp(&publish_dir, &publish_files);

    write_embed_file(
        &embed_rs,
        &publish_dir,
        &publish_files,
        &binding_stamp,
        self_contained,
        binding_executable_name,
    );

    // Local dev convenience:
    // cargo-dist will not package this sidecar automatically, but local `cargo run`
    // can still use the copied binding directory beside target/debug or target/release.
    if let Some(profile_dir) = cargo_profile_dir(&out_dir) {
        if let Err(err) = copy_binding_artifacts(&publish_dir, &publish_files, &profile_dir) {
            println!(
                "cargo:warning=Failed to copy KawaiiPhysicsBinding artifacts to {}: {err}",
                profile_dir.display()
            );
        }

        let deps_dir = profile_dir.join("deps");
        if deps_dir.exists() {
            if let Err(err) = copy_binding_artifacts(&publish_dir, &publish_files, &deps_dir) {
                println!(
                    "cargo:warning=Failed to copy KawaiiPhysicsBinding artifacts to {}: {err}",
                    deps_dir.display()
                );
            }
        }
    }

    println!(
        "cargo:metadata=binding_build=Built KawaiiPhysicsBinding managed DLL at {} ({})",
        binding_assembly.display(),
        if self_contained {
            "self-contained"
        } else {
            "framework-dependent"
        }
    );

    println!(
        "cargo:metadata=binding_embed=Embedded KawaiiPhysicsBinding managed DLL artifacts via {}",
        embed_rs.display()
    );
}

fn emit_rerun_if_changed_for_dotnet_sources(dir: &Path) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            panic!(
                "failed to scan dotnet source directory {}: {err}",
                dir.display()
            );
        }
    };

    for entry in entries {
        let path = entry
            .unwrap_or_else(|err| panic!("failed to read entry in {}: {err}", dir.display()))
            .path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };

        if path.is_dir() {
            if matches!(file_name, "bin" | "obj" | ".git") {
                continue;
            }
            emit_rerun_if_changed_for_dotnet_sources(&path);
            continue;
        }

        if matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("cs" | "csproj" | "props" | "targets" | "json")
        ) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

fn env_flag(name: &str) -> bool {
    env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
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

fn binding_executable_name(target_os: &str) -> &'static str {
    if target_os == "windows" {
        "KawaiiPhysicsBinding.exe"
    } else {
        "KawaiiPhysicsBinding"
    }
}

fn cargo_profile_dir(out_dir: &Path) -> Option<PathBuf> {
    // OUT_DIR usually:
    // target/debug/build/<pkg-hash>/out
    //
    // desired:
    // target/debug
    out_dir.parent()?.parent()?.parent().map(Path::to_path_buf)
}

fn collect_publish_files(publish_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_publish_files_inner(publish_dir, publish_dir, &mut files);
    files.sort();
    files
}

fn collect_publish_files_inner(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("failed to scan publish directory {}: {err}", dir.display()));

    for entry in entries {
        let path = entry
            .unwrap_or_else(|err| panic!("failed to read entry in {}: {err}", dir.display()))
            .path();

        if path.is_dir() {
            collect_publish_files_inner(root, &path, files);
            continue;
        }

        if matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("pdb" | "xml")
        ) {
            continue;
        }

        let relative = path
            .strip_prefix(root)
            .unwrap_or_else(|err| {
                panic!(
                    "failed to compute publish-relative path for {}: {err}",
                    path.display()
                )
            })
            .to_path_buf();

        files.push(relative);
    }
}

fn binding_stamp(publish_dir: &Path, publish_files: &[PathBuf]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;

    for relative in publish_files {
        update_hash(&mut hash, relative.to_string_lossy().as_bytes());
        update_hash(&mut hash, &[0]);

        let bytes = std::fs::read(publish_dir.join(relative)).unwrap_or_else(|err| {
            panic!(
                "failed to read publish artifact {}: {err}",
                publish_dir.join(relative).display()
            )
        });
        update_hash(&mut hash, &bytes);
    }

    format!("{hash:016x}")
}

fn update_hash(hash: &mut u64, bytes: &[u8]) {
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

fn copy_binding_artifacts(
    publish_dir: &Path,
    publish_files: &[PathBuf],
    dest_dir: &Path,
) -> std::io::Result<()> {
    let dest_root = dest_dir.join(BINDING_DIR_NAME);
    if dest_root.exists() {
        std::fs::remove_dir_all(&dest_root)?;
    }
    std::fs::create_dir_all(&dest_root)?;

    for relative in publish_files {
        let src = publish_dir.join(relative);
        let dst = dest_root.join(relative);

        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::copy(src, dst)?;
    }

    Ok(())
}

fn write_embed_file(
    embed_rs: &Path,
    publish_dir: &Path,
    publish_files: &[PathBuf],
    binding_stamp: &str,
    self_contained: bool,
    binding_executable_name: &str,
) {
    let mut entries = String::new();

    for relative in publish_files {
        let absolute_path = publish_dir
            .join(relative)
            .canonicalize()
            .unwrap_or_else(|err| {
                panic!(
                    "failed to canonicalize publish artifact {}: {err}",
                    publish_dir.join(relative).display()
                )
            });
        let relative = relative.to_string_lossy().replace('\\', "/");
        entries.push_str(&format!(
            "    ({relative:?}, include_bytes!({path:?})),\n",
            relative = relative,
            path = absolute_path.to_string_lossy()
        ));
    }

    let source = format!(
        r#"
// This file is generated by build.rs. Do not edit by hand.

pub const KAWAII_BINDING_DIR_NAME: &str = {binding_dir_name:?};

pub const KAWAII_BINDING_ASSEMBLY_NAME: &str = {binding_assembly_name:?};

pub const KAWAII_BINDING_RUNTIME_CONFIG_NAME: &str = {runtime_config_name:?};

pub const KAWAII_BINDING_EXECUTABLE_NAME: &str = {binding_executable_name:?};

pub const KAWAII_BINDING_SELF_CONTAINED: bool = {self_contained};

pub const KAWAII_BINDING_STAMP_FILE_NAME: &str = {stamp_file_name:?};

pub const KAWAII_BINDING_STAMP: &str = {binding_stamp:?};

pub static KAWAII_BINDING_FILES: &[(&str, &[u8])] = &[
{entries}];
"#,
        binding_dir_name = BINDING_DIR_NAME,
        binding_assembly_name = BINDING_ASSEMBLY_NAME,
        runtime_config_name = BINDING_RUNTIME_CONFIG_NAME,
        binding_executable_name = binding_executable_name,
        self_contained = self_contained,
        stamp_file_name = STAMP_FILE_NAME,
        binding_stamp = binding_stamp,
        entries = entries,
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

pub const KAWAII_BINDING_DIR_NAME: &str = "";

pub const KAWAII_BINDING_ASSEMBLY_NAME: &str = "";

pub const KAWAII_BINDING_RUNTIME_CONFIG_NAME: &str = "";

pub const KAWAII_BINDING_EXECUTABLE_NAME: &str = "";

pub const KAWAII_BINDING_SELF_CONTAINED: bool = false;

pub const KAWAII_BINDING_STAMP_FILE_NAME: &str = "";

pub const KAWAII_BINDING_STAMP: &str = "";

pub static KAWAII_BINDING_FILES: &[(&str, &[u8])] = &[];
"#;

    std::fs::write(embed_rs, source).unwrap_or_else(|err| {
        panic!(
            "failed to write generated empty embed file {}: {err}",
            embed_rs.display()
        )
    });
}
