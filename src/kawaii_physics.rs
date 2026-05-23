use crate::legacy_asset::FSerializedAssetBundle;

use anyhow::{bail, Context, Result};
use fs_err as fs;
use libloading::Library;
use std::ffi::{c_void, CString};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use typed_path::Utf8UnixPath as UEPath;

#[cfg(not(windows))]
use std::os::unix::ffi::OsStrExt;

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static SHARED_BINDING: OnceLock<Result<Arc<KawaiiPhysicsBinding>, String>> = OnceLock::new();

const HOSTFXR_DELEGATE_LOAD_ASSEMBLY_AND_GET_FUNCTION_POINTER: i32 = 5;
const NATIVE_EXPORT_TYPE_NAME: &str = "KawaiiPhysicsBinding.NativeExports, KawaiiPhysicsBinding";
const NATIVE_EXPORT_METHOD_NAME: &str = "PortAsset";
const ERROR_BUFFER_LEN: usize = 64 * 1024;

include!(concat!(env!("OUT_DIR"), "/kawaii_binding_embed.rs"));

#[cfg(windows)]
type HostChar = u16;

#[cfg(not(windows))]
type HostChar = std::os::raw::c_char;

type HostfxrHandle = *mut c_void;

type HostfxrInitializeForRuntimeConfigFn =
    unsafe extern "system" fn(*const HostChar, *const c_void, *mut HostfxrHandle) -> i32;
type HostfxrGetRuntimeDelegateFn =
    unsafe extern "system" fn(HostfxrHandle, i32, *mut *mut c_void) -> i32;
type HostfxrCloseFn = unsafe extern "system" fn(HostfxrHandle) -> i32;
type LoadAssemblyAndGetFunctionPointerFn = unsafe extern "system" fn(
    *const HostChar,
    *const HostChar,
    *const HostChar,
    *const HostChar,
    *mut c_void,
    *mut *mut c_void,
) -> i32;

type PortAssetFn = unsafe extern "C" fn(
    *const u8,
    *const u8,
    i32,
    *mut KawaiiPhysicsPortNativeResult,
    *mut u8,
    i32,
) -> i32;

#[repr(C)]
#[derive(Default)]
struct KawaiiPhysicsPortNativeResult {
    visited_anim_nodes: i32,
    ported_anim_nodes: i32,
    skipped_existing_chains: i32,
}

struct Hostfxr {
    _library: Library,
    initialize_for_runtime_config: HostfxrInitializeForRuntimeConfigFn,
    get_runtime_delegate: HostfxrGetRuntimeDelegateFn,
    close: HostfxrCloseFn,
}

pub struct KawaiiPhysicsBinding {
    binding_dir: PathBuf,
    _hostfxr: Hostfxr,
    port_asset: PortAssetFn,
    call_mutex: Mutex<()>,
}

impl KawaiiPhysicsBinding {
    pub fn load_shared_beside_exe() -> Result<Arc<Self>> {
        match SHARED_BINDING.get_or_init(|| {
            Self::load_beside_exe()
                .map(Arc::new)
                .map_err(|err| format!("{err:#}"))
        }) {
            Ok(binding) => Ok(binding.clone()),
            Err(err) => bail!("{err}"),
        }
    }

    pub fn load_beside_exe() -> Result<Self> {
        let exe = std::env::current_exe().context("failed to resolve current executable path")?;

        if KAWAII_BINDING_DIR_NAME.is_empty() || KAWAII_BINDING_FILES.is_empty() {
            bail!(
                "KawaiiPhysicsBinding was not embedded. \
                 RETOC_SKIP_KAWAII_BINDING_BUILD may be set, or build.rs did not generate the helper."
            );
        }

        let binding_dir = exe.with_file_name(KAWAII_BINDING_DIR_NAME);
        extract_binding_files(&binding_dir)?;

        let runtime_config = binding_dir.join(KAWAII_BINDING_RUNTIME_CONFIG_NAME);
        let assembly = binding_dir.join(KAWAII_BINDING_ASSEMBLY_NAME);
        let hostfxr_path = find_hostfxr(&binding_dir).context(
            "failed to locate hostfxr. Install the .NET 8 runtime or publish with \
             RETOC_KAWAII_BINDING_SELF_CONTAINED=true",
        )?;
        let hostfxr = load_hostfxr(&hostfxr_path)?;
        let load_assembly = load_assembly_delegate(&hostfxr, &runtime_config)?;
        let port_asset = load_port_asset_fn(load_assembly, &assembly)?;

        tracing::info!(
            binding_dir = %binding_dir.display(),
            hostfxr = %hostfxr_path.display(),
            "loaded KawaiiPhysics managed DLL binding"
        );

        Ok(Self {
            binding_dir,
            _hostfxr: hostfxr,
            port_asset,
            call_mutex: Mutex::new(()),
        })
    }

    pub fn port_asset(
        &self,
        usmap_path: Option<&Path>,
        uasset_path: &Path,
        force_rebuild: bool,
        ported_count: &AtomicUsize,
    ) -> Result<i32> {
        let usmap_path = usmap_path.map(path_to_cstring).transpose()?;
        let uasset_path = path_to_cstring(uasset_path)?;
        let mut result = KawaiiPhysicsPortNativeResult::default();
        let mut error_buffer = vec![0u8; ERROR_BUFFER_LEN];
        let _call_guard = self
            .call_mutex
            .lock()
            .map_err(|_| anyhow::anyhow!("KawaiiPhysics managed binding mutex was poisoned"))?;

        tracing::debug!(
            binding_dir = %self.binding_dir.display(),
            "calling KawaiiPhysics managed DLL binding"
        );

        let status = unsafe {
            (self.port_asset)(
                usmap_path
                    .as_ref()
                    .map(|path| path.as_ptr())
                    .unwrap_or(ptr::null()) as *const u8,
                uasset_path.as_ptr() as *const u8,
                if force_rebuild { 1 } else { 0 },
                &mut result,
                error_buffer.as_mut_ptr(),
                error_buffer.len().min(i32::MAX as usize) as i32,
            )
        };

        if status != 0 {
            let error = nul_terminated_utf8(&error_buffer);
            bail!("KawaiiPhysics managed DLL binding failed with status {status}: {error}");
        }

        let ported = result.ported_anim_nodes.max(0);
        ported_count.fetch_add(ported as usize, Ordering::Relaxed);

        tracing::trace!(
            visited_anim_nodes = result.visited_anim_nodes,
            ported_anim_nodes = result.ported_anim_nodes,
            skipped_existing_chains = result.skipped_existing_chains,
            "KawaiiPhysics managed binding finished"
        );

        Ok(ported)
    }
}

struct HostString {
    #[cfg(windows)]
    inner: Vec<u16>,
    #[cfg(not(windows))]
    inner: CString,
}

impl HostString {
    #[cfg(windows)]
    fn from_path(path: &Path) -> Result<Self> {
        Ok(Self {
            inner: path.as_os_str().encode_wide().chain(Some(0)).collect(),
        })
    }

    #[cfg(not(windows))]
    fn from_path(path: &Path) -> Result<Self> {
        Ok(Self {
            inner: CString::new(path.as_os_str().as_bytes())
                .with_context(|| format!("path contains an interior NUL: {}", path.display()))?,
        })
    }

    #[cfg(windows)]
    fn from_str(value: &str) -> Result<Self> {
        Ok(Self {
            inner: value.encode_utf16().chain(Some(0)).collect(),
        })
    }

    #[cfg(not(windows))]
    fn from_str(value: &str) -> Result<Self> {
        Ok(Self {
            inner: CString::new(value)
                .with_context(|| format!("hostfxr string contains an interior NUL: {value:?}"))?,
        })
    }

    fn as_ptr(&self) -> *const HostChar {
        self.inner.as_ptr()
    }
}

fn extract_binding_files(binding_dir: &Path) -> Result<()> {
    fs::create_dir_all(binding_dir)
        .with_context(|| format!("failed to create {}", binding_dir.display()))?;

    let stamp_path = binding_dir.join(KAWAII_BINDING_STAMP_FILE_NAME);
    let needs_full_extract = fs::read_to_string(&stamp_path)
        .map(|stamp| stamp != KAWAII_BINDING_STAMP)
        .unwrap_or(true);

    for (relative, bytes) in KAWAII_BINDING_FILES {
        let path = binding_artifact_path(binding_dir, relative)?;
        let should_write = needs_full_extract
            || fs::metadata(&path)
                .map(|metadata| metadata.len() != bytes.len() as u64)
                .unwrap_or(true);

        if !should_write {
            continue;
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        fs::write(&path, bytes).with_context(|| {
            format!("failed to extract KawaiiPhysics binding {}", path.display())
        })?;
    }

    fs::write(&stamp_path, KAWAII_BINDING_STAMP).with_context(|| {
        format!(
            "failed to write KawaiiPhysics binding stamp {}",
            stamp_path.display()
        )
    })?;

    Ok(())
}

fn binding_artifact_path(binding_dir: &Path, relative: &str) -> Result<PathBuf> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        bail!("invalid embedded KawaiiPhysics binding path: {relative}");
    }

    Ok(binding_dir.join(relative_path))
}

fn load_hostfxr(hostfxr_path: &Path) -> Result<Hostfxr> {
    let library = unsafe { Library::new(hostfxr_path) }
        .with_context(|| format!("failed to load hostfxr {}", hostfxr_path.display()))?;

    let (initialize_for_runtime_config, get_runtime_delegate, close) = unsafe {
        (
            *library
                .get::<HostfxrInitializeForRuntimeConfigFn>(
                    b"hostfxr_initialize_for_runtime_config\0",
                )
                .context("hostfxr_initialize_for_runtime_config was not found")?,
            *library
                .get::<HostfxrGetRuntimeDelegateFn>(b"hostfxr_get_runtime_delegate\0")
                .context("hostfxr_get_runtime_delegate was not found")?,
            *library
                .get::<HostfxrCloseFn>(b"hostfxr_close\0")
                .context("hostfxr_close was not found")?,
        )
    };

    Ok(Hostfxr {
        _library: library,
        initialize_for_runtime_config,
        get_runtime_delegate,
        close,
    })
}

fn load_assembly_delegate(
    hostfxr: &Hostfxr,
    runtime_config: &Path,
) -> Result<LoadAssemblyAndGetFunctionPointerFn> {
    let runtime_config = HostString::from_path(runtime_config)?;
    let mut host_context: HostfxrHandle = ptr::null_mut();
    let init_status = unsafe {
        (hostfxr.initialize_for_runtime_config)(
            runtime_config.as_ptr(),
            ptr::null(),
            &mut host_context,
        )
    };

    if init_status < 0 || host_context.is_null() {
        bail!("hostfxr failed to initialize .NET runtime: status {init_status:#x}");
    }

    let mut delegate: *mut c_void = ptr::null_mut();
    let delegate_status = unsafe {
        (hostfxr.get_runtime_delegate)(
            host_context,
            HOSTFXR_DELEGATE_LOAD_ASSEMBLY_AND_GET_FUNCTION_POINTER,
            &mut delegate,
        )
    };
    unsafe {
        (hostfxr.close)(host_context);
    }

    if delegate_status != 0 || delegate.is_null() {
        bail!(
            "hostfxr failed to get load_assembly_and_get_function_pointer: status {delegate_status:#x}"
        );
    }

    Ok(
        unsafe {
            std::mem::transmute::<*mut c_void, LoadAssemblyAndGetFunctionPointerFn>(delegate)
        },
    )
}

fn load_port_asset_fn(
    load_assembly: LoadAssemblyAndGetFunctionPointerFn,
    assembly: &Path,
) -> Result<PortAssetFn> {
    let assembly = HostString::from_path(assembly)?;
    let type_name = HostString::from_str(NATIVE_EXPORT_TYPE_NAME)?;
    let method_name = HostString::from_str(NATIVE_EXPORT_METHOD_NAME)?;
    let mut delegate: *mut c_void = ptr::null_mut();

    let status = unsafe {
        load_assembly(
            assembly.as_ptr(),
            type_name.as_ptr(),
            method_name.as_ptr(),
            unmanaged_callers_only_method(),
            ptr::null_mut(),
            &mut delegate,
        )
    };

    if status != 0 || delegate.is_null() {
        bail!(
            "failed to load KawaiiPhysics native export {NATIVE_EXPORT_TYPE_NAME}.{NATIVE_EXPORT_METHOD_NAME}: status {status:#x}"
        );
    }

    Ok(unsafe { std::mem::transmute::<*mut c_void, PortAssetFn>(delegate) })
}

fn unmanaged_callers_only_method() -> *const HostChar {
    usize::MAX as *const HostChar
}

fn path_to_cstring(path: &Path) -> Result<CString> {
    CString::new(path.to_string_lossy().as_bytes())
        .with_context(|| format!("path contains an interior NUL: {}", path.display()))
}

fn nul_terminated_utf8(buffer: &[u8]) -> String {
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).trim().to_string()
}

fn find_hostfxr(binding_dir: &Path) -> Option<PathBuf> {
    let local = binding_dir.join(hostfxr_library_name());
    if local.exists() {
        return Some(local);
    }

    if let Some(hostfxr) = newest_hostfxr_in_dotnet_root(binding_dir) {
        return Some(hostfxr);
    }

    for root in dotnet_roots() {
        let direct = root.join(hostfxr_library_name());
        if direct.exists() {
            return Some(direct);
        }

        if let Some(hostfxr) = newest_hostfxr_in_dotnet_root(&root) {
            return Some(hostfxr);
        }
    }

    None
}

fn newest_hostfxr_in_dotnet_root(root: &Path) -> Option<PathBuf> {
    let fxr_dir = root.join("host").join("fxr");
    let entries = fs::read_dir(fxr_dir).ok()?;

    entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let hostfxr = path.join(hostfxr_library_name());
            hostfxr.exists().then_some(hostfxr)
        })
        .max_by_key(|path| hostfxr_version_key(path))
}

fn hostfxr_version_key(path: &Path) -> Vec<u32> {
    path.parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .map(|version| {
            version
                .split(|ch: char| !ch.is_ascii_digit())
                .filter(|part| !part.is_empty())
                .map(|part| part.parse::<u32>().unwrap_or(0))
                .collect()
        })
        .unwrap_or_default()
}

fn dotnet_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    for name in ["DOTNET_ROOT", "DOTNET_ROOT(x86)"] {
        if let Some(root) = std::env::var_os(name) {
            roots.push(PathBuf::from(root));
        }
    }

    #[cfg(windows)]
    {
        for name in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(root) = std::env::var_os(name) {
                roots.push(PathBuf::from(root).join("dotnet"));
            }
        }
    }

    #[cfg(not(windows))]
    {
        roots.push(PathBuf::from("/usr/share/dotnet"));
        roots.push(PathBuf::from("/usr/local/share/dotnet"));
    }

    roots
}

fn hostfxr_library_name() -> &'static str {
    #[cfg(windows)]
    {
        "hostfxr.dll"
    }

    #[cfg(target_os = "macos")]
    {
        "libhostfxr.dylib"
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        "libhostfxr.so"
    }
}

pub(crate) fn should_try_port(_path: &UEPath) -> bool {
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
    total_ported: &AtomicUsize,
) -> Result<FSerializedAssetBundle> {
    if !bundle_may_contain_kawaii_physics(&bundle) {
        tracing::trace!(asset = %path, "skipping KawaiiPhysics binding for unrelated asset");
        return Ok(bundle);
    }

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

        let _ = binding
            .port_asset(usmap_path, &uasset_path, force_rebuild, total_ported)
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

fn bundle_may_contain_kawaii_physics(bundle: &FSerializedAssetBundle) -> bool {
    contains_kawaii_physics_marker(&bundle.asset_file_buffer)
        || contains_kawaii_physics_marker(&bundle.exports_file_buffer)
}

fn contains_kawaii_physics_marker(bytes: &[u8]) -> bool {
    const ASCII_NEEDLE: &[u8] = b"KawaiiPhysics";
    const UTF16LE_NEEDLE: &[u8] = &[
        b'K', 0, b'a', 0, b'w', 0, b'a', 0, b'i', 0, b'i', 0, b'P', 0, b'h', 0, b'y', 0, b's', 0,
        b'i', 0, b'c', 0, b's', 0,
    ];

    contains_bytes(bytes, ASCII_NEEDLE) || contains_bytes(bytes, UTF16LE_NEEDLE)
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
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
