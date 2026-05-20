use crate::legacy_asset::FSerializedAssetBundle;

use anyhow::{bail, Context, Result};
use fs_err as fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use typed_path::Utf8UnixPath as UEPath;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

include!(concat!(env!("OUT_DIR"), "/kawaii_binding_embed.rs"));

pub struct KawaiiPhysicsBinding {
    exe_path: PathBuf,
}

impl KawaiiPhysicsBinding {
    pub fn load_beside_exe() -> Result<Self> {
        let exe = std::env::current_exe().context("failed to resolve current executable path")?;

        if KAWAII_BINDING_NAME.is_empty() || KAWAII_BINDING_BYTES.is_empty() {
            bail!(
                "KawaiiPhysicsBinding was not embedded. \
                 RETOC_SKIP_KAWAII_BINDING_BUILD may be set, or build.rs did not generate the helper."
            );
        }

        let path = exe.with_file_name(KAWAII_BINDING_NAME);

        if !path.exists() {
            fs::write(&path, KAWAII_BINDING_BYTES).with_context(|| {
                format!(
                    "failed to extract KawaiiPhysicsBinding helper to {}",
                    path.display()
                )
            })?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;

                let mut perms = fs::metadata(&path)
                    .with_context(|| format!("failed to stat {}", path.display()))?
                    .permissions();

                perms.set_mode(0o755);

                fs::set_permissions(&path, perms)
                    .with_context(|| format!("failed to chmod {}", path.display()))?;
            }
        }

        tracing::info!(
            path = %path.display(),
            "loaded KawaiiPhysics managed binding helper"
        );

        Ok(Self { exe_path: path })
    }

    pub fn port_asset(
        &self,
        usmap_path: Option<&Path>,
        uasset_path: &Path,
        force_rebuild: bool,
    ) -> Result<i32> {
        let mut cmd = Command::new(&self.exe_path);

        cmd.arg("port");

        if std::env::var_os("RETOC_KAWAII_PASS_USMAP").is_some() {
            if let Some(usmap_path) = usmap_path {
                cmd.arg(usmap_path);
            }
        }

        cmd.arg(uasset_path);

        if force_rebuild {
            cmd.arg("--force-rebuild");
        }

        tracing::debug!(
            command = ?cmd,
            "running KawaiiPhysics managed binding"
        );

        let output = cmd
            .output()
            .with_context(|| format!("failed to run {}", self.exe_path.display()))?;

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

        if !output.status.success() {
            bail!(
                "KawaiiPhysics managed binding failed with status {}\nstdout:\n{}\nstderr:\n{}",
                output.status,
                stdout,
                stderr
            );
        }

        let ported = parse_ported_count(&stdout)
            .or_else(|| parse_ported_count(&stderr))
            .unwrap_or(0);

        tracing::debug!(
            ported_anim_nodes = ported,
            stdout = %stdout,
            stderr = %stderr,
            "KawaiiPhysics managed binding finished"
        );

        Ok(ported)
    }
}

pub(crate) fn should_try_port(path: &UEPath) -> bool {
    let path = path.as_str().to_ascii_lowercase();
    let file_name = path.rsplit('/').next().unwrap_or(path.as_str());

    // Keep this broad for parity with your working CLI.
    // It patched both:
    // - ABP_NudePhysicsLobby.uasset
    // - Post_1053001_Physics.uasset
    // path.contains("physics")
    //     || file_name.starts_with("abp_")
    //     || path.contains("/abp_")
    //     || path.contains("/animation/")
    //     || path.contains("/animations/")

    true
}

pub(crate) fn port_bundle(
    mut bundle: FSerializedAssetBundle,
    path: &UEPath,
    usmap_path: Option<&Path>,
    binding: &KawaiiPhysicsBinding,
    force_rebuild: bool,
) -> Result<FSerializedAssetBundle> {
    let temp_dir = make_temp_dir()?;

    let result = (|| -> Result<FSerializedAssetBundle> {
        let stem = path
            .file_stem()
            .filter(|x| !x.is_empty())
            .unwrap_or("asset");

        let uasset_path = temp_dir.join(format!("{stem}.uasset"));
        let uexp_path = temp_dir.join(format!("{stem}.uexp"));
        let ubulk_path = temp_dir.join(format!("{stem}.ubulk"));
        let uptnl_path = temp_dir.join(format!("{stem}.uptnl"));
        let mubulk_path = temp_dir.join(format!("{stem}.m.ubulk"));

        fs::write(&uasset_path, &bundle.asset_file_buffer)
            .with_context(|| format!("failed to write temp uasset {}", uasset_path.display()))?;

        fs::write(&uexp_path, &bundle.exports_file_buffer)
            .with_context(|| format!("failed to write temp uexp {}", uexp_path.display()))?;

        write_optional(&ubulk_path, bundle.bulk_data_buffer.as_deref())
            .with_context(|| format!("failed to write temp ubulk {}", ubulk_path.display()))?;

        write_optional(&uptnl_path, bundle.optional_bulk_data_buffer.as_deref())
            .with_context(|| format!("failed to write temp uptnl {}", uptnl_path.display()))?;

        write_optional(
            &mubulk_path,
            bundle.memory_mapped_bulk_data_buffer.as_deref(),
        )
        .with_context(|| format!("failed to write temp m.ubulk {}", mubulk_path.display()))?;

        let ported = binding
            .port_asset(usmap_path, &uasset_path, force_rebuild)
            .with_context(|| format!("failed to port KawaiiPhysics asset {path}"))?;

        bundle.asset_file_buffer = fs::read(&uasset_path)
            .with_context(|| format!("failed to read patched uasset {}", uasset_path.display()))?;

        bundle.exports_file_buffer = fs::read(&uexp_path)
            .with_context(|| format!("failed to read patched uexp {}", uexp_path.display()))?;

        bundle.bulk_data_buffer = read_optional_if_exists(&ubulk_path)
            .with_context(|| format!("failed to read patched ubulk {}", ubulk_path.display()))?;

        bundle.optional_bulk_data_buffer = read_optional_if_exists(&uptnl_path)
            .with_context(|| format!("failed to read patched uptnl {}", uptnl_path.display()))?;

        bundle.memory_mapped_bulk_data_buffer = read_optional_if_exists(&mubulk_path)
            .with_context(|| format!("failed to read patched m.ubulk {}", mubulk_path.display()))?;

        tracing::debug!(
            path = %path,
            ported_anim_nodes = ported,
            "KawaiiPhysics bundle port finished"
        );

        Ok(bundle)
    })();

    if let Err(err) = fs::remove_dir_all(&temp_dir) {
        tracing::warn!(
            temp_dir = %temp_dir.display(),
            error = %err,
            "failed to clean KawaiiPhysics temp dir"
        );
    }

    result
}

fn parse_ported_count(text: &str) -> Option<i32> {
    // Native/export style:
    // visited=21 ported=21 skipped_existing=0
    if let Some(idx) = text.find("ported=") {
        let rest = &text[idx + "ported=".len()..];
        let number = rest
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>();

        if !number.is_empty() {
            return number.parse().ok();
        }
    }

    // CLI style:
    // Ported AnimNodes: 21
    for line in text.lines() {
        let line = line.trim();

        if let Some(rest) = line.strip_prefix("Ported AnimNodes:") {
            return rest.trim().parse().ok();
        }
    }

    None
}

fn write_optional(path: &Path, bytes: Option<&[u8]>) -> Result<()> {
    if let Some(bytes) = bytes {
        fs::write(path, bytes)?;
    }

    Ok(())
}

fn read_optional_if_exists(path: &Path) -> Result<Option<Vec<u8>>> {
    if path.exists() {
        Ok(Some(fs::read(path)?))
    } else {
        Ok(None)
    }
}

fn make_temp_dir() -> Result<PathBuf> {
    let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);

    let dir = std::env::temp_dir().join(format!("retoc-kawaii-{}-{id}", std::process::id()));

    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create temp dir {}", dir.display()))?;

    Ok(dir)
}
