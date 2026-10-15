//! Runtime binding of the native engine library (`vr_engine`).
//!
//! The library is loaded with `libloading` instead of being linked, so the client
//! starts and reports a clear error when native files are missing, and so a
//! broken GPU driver can only take down the worker process, never the client.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicI32;

use anyhow::{Context, bail};
use libloading::{Library, Symbol};
use serde::Deserialize;
use verbarelay_protocol::messages::Backend;

/// Must match VR_ENGINE_ABI_VERSION in engine/include/vr_engine.h.
const ABI_VERSION: c_int = 1;

#[cfg(windows)]
const LIBRARY_NAME: &str = "vr_engine.dll";
#[cfg(target_os = "macos")]
const LIBRARY_NAME: &str = "libvr_engine.dylib";
#[cfg(all(unix, not(target_os = "macos")))]
const LIBRARY_NAME: &str = "libvr_engine.so";

type SegmentCallback = unsafe extern "C" fn(*mut c_void, i64, i64, *const c_char);

/// A device registered by a ggml backend.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct EngineDevice {
    pub index: u32,
    pub name: String,
    pub description: String,
    /// "CPU", "Vulkan", "CUDA", ...
    pub backend: String,
    /// "cpu", "gpu", "igpu", "accel"
    #[serde(rename = "type")]
    pub kind: String,
    pub memory_total: u64,
    pub memory_free: u64,
}

impl EngineDevice {
    pub fn is_gpu(&self) -> bool {
        self.kind == "gpu" || self.kind == "igpu"
    }
}

/// The loaded engine library.
pub struct Engine {
    lib: Library,
    dir: PathBuf,
}

/// A loaded model.
pub struct Model<'a> {
    engine: &'a Engine,
    ctx: *mut c_void,
}

fn error_buffer() -> Vec<u8> {
    vec![0u8; 1024]
}

fn buffer_to_string(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

impl Engine {
    /// Path of the engine library inside an installation directory.
    pub fn library_path(dir: &Path) -> PathBuf {
        dir.join(LIBRARY_NAME)
    }

    /// Loads the engine from `dir` and registers every backend module found there.
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let path = Self::library_path(dir);
        if !path.exists() {
            bail!(
                "the native engine is missing ({}); reinstall the client",
                path.display()
            );
        }
        #[cfg(windows)]
        set_dll_directory(dir);
        // SAFETY: loading our own library; its initializers have no preconditions.
        let lib = unsafe { Library::new(&path) }.with_context(|| format!("loading {}", path.display()))?;
        let engine = Self {
            lib,
            dir: dir.to_path_buf(),
        };
        let abi = unsafe { engine.symbol::<unsafe extern "C" fn() -> c_int>(b"vr_engine_abi_version\0")?() };
        if abi != ABI_VERSION {
            bail!("native engine ABI {abi} does not match the client (expected {ABI_VERSION}); reinstall the client");
        }
        let dir_c = CString::new(dir.to_string_lossy().as_bytes())?;
        let count = unsafe {
            engine.symbol::<unsafe extern "C" fn(*const c_char) -> c_int>(b"vr_engine_load_backends\0")?(dir_c.as_ptr())
        };
        if count <= 0 {
            bail!("no compute backend could be loaded from {}", engine.dir.display());
        }
        Ok(engine)
    }

    unsafe fn symbol<T>(&self, name: &[u8]) -> anyhow::Result<Symbol<'_, T>> {
        // SAFETY: the caller states the correct signature for the symbol.
        unsafe { self.lib.get::<T>(name) }.with_context(|| {
            format!(
                "native engine lacks {}",
                String::from_utf8_lossy(&name[..name.len() - 1])
            )
        })
    }

    pub fn whisper_version(&self) -> String {
        unsafe {
            match self.symbol::<unsafe extern "C" fn() -> *const c_char>(b"vr_engine_whisper_version\0") {
                Ok(f) => CStr::from_ptr(f()).to_string_lossy().into_owned(),
                Err(_) => "unknown".into(),
            }
        }
    }

    /// All devices the loaded backends registered.
    pub fn devices(&self) -> anyhow::Result<Vec<EngineDevice>> {
        let list =
            unsafe { self.symbol::<unsafe extern "C" fn(*mut c_char, c_int) -> c_int>(b"vr_engine_list_devices\0")? };
        let needed = unsafe { list(std::ptr::null_mut(), 0) };
        let mut buf = vec![0u8; usize::try_from(needed).unwrap_or(0) + 1];
        unsafe { list(buf.as_mut_ptr().cast(), buf.len() as c_int) };
        Ok(serde_json::from_str(&buffer_to_string(&buf))?)
    }

    /// Loads a model on the given backend.
    pub fn load_model(&self, model_path: &Path, backend: Backend) -> anyhow::Result<Model<'_>> {
        type Init = unsafe extern "C" fn(*const c_char, *const c_char, c_int, *mut c_char, c_int) -> *mut c_void;
        let init = unsafe { self.symbol::<Init>(b"vr_engine_init\0")? };
        let path_c = CString::new(model_path.to_string_lossy().as_bytes())?;
        let backend_c = CString::new(backend.as_str())?;
        let mut err = error_buffer();
        // Flash attention speeds up GPUs; whisper.cpp ignores it where unsupported.
        let flash_attn = c_int::from(backend != Backend::Cpu);
        let ctx = unsafe {
            init(
                path_c.as_ptr(),
                backend_c.as_ptr(),
                flash_attn,
                err.as_mut_ptr().cast(),
                err.len() as c_int,
            )
        };
        if ctx.is_null() {
            bail!("{}", buffer_to_string(&err));
        }
        Ok(Model { engine: self, ctx })
    }
}

/// Collects segments from the C callback.
struct CallbackState<'f> {
    on_segment: &'f mut dyn FnMut(u64, u64, String),
}

unsafe extern "C" fn segment_trampoline(user: *mut c_void, t0: i64, t1: i64, text: *const c_char) {
    // SAFETY: `user` is the CallbackState passed to vr_engine_transcribe, valid for the call.
    let state = unsafe { &mut *user.cast::<CallbackState<'_>>() };
    let text = if text.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned()
    };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (state.on_segment)(t0.max(0) as u64, t1.max(0) as u64, text);
    }));
}

impl Model<'_> {
    /// Transcribes 16 kHz mono samples. Segments are reported as they are produced.
    /// Returns the detected language.
    pub fn transcribe(
        &self,
        samples: &[f32],
        language: Option<&str>,
        translate: bool,
        threads: u32,
        abort: &AtomicI32,
        on_segment: &mut dyn FnMut(u64, u64, String),
    ) -> anyhow::Result<String> {
        type Transcribe = unsafe extern "C" fn(
            *mut c_void,
            *const f32,
            c_int,
            *const c_char,
            c_int,
            c_int,
            Option<SegmentCallback>,
            *mut c_void,
            *const AtomicI32,
            *mut c_char,
            c_int,
        ) -> c_int;
        let transcribe = unsafe { self.engine.symbol::<Transcribe>(b"vr_engine_transcribe\0")? };
        let language_c = CString::new(language.unwrap_or("auto"))?;
        let mut state = CallbackState { on_segment };
        let mut err = error_buffer();
        let n = c_int::try_from(samples.len()).context("audio is too long for one run")?;
        let threads = if threads == 0 { default_threads() } else { threads };
        let rc = unsafe {
            transcribe(
                self.ctx,
                samples.as_ptr(),
                n,
                language_c.as_ptr(),
                c_int::from(translate),
                threads as c_int,
                Some(segment_trampoline),
                (&mut state as *mut CallbackState<'_>).cast(),
                abort as *const AtomicI32,
                err.as_mut_ptr().cast(),
                err.len() as c_int,
            )
        };
        if rc != 0 {
            bail!("{}", buffer_to_string(&err));
        }
        let lang = unsafe {
            self.engine
                .symbol::<unsafe extern "C" fn(*mut c_void) -> *const c_char>(b"vr_engine_last_language\0")?
        };
        Ok(unsafe { CStr::from_ptr(lang(self.ctx)) }.to_string_lossy().into_owned())
    }
}

impl Drop for Model<'_> {
    fn drop(&mut self) {
        if let Ok(free) = unsafe {
            self.engine
                .symbol::<unsafe extern "C" fn(*mut c_void)>(b"vr_engine_free\0")
        } {
            unsafe { free(self.ctx) };
        }
    }
}

/// Whisper scales up to about eight threads; more rarely helps.
pub fn default_threads() -> u32 {
    std::thread::available_parallelism()
        .map_or(4, |n| n.get() as u32)
        .clamp(1, 8)
}

/// Lets Windows find dependent DLLs (CUDA runtime, cuBLAS) next to the executable.
#[cfg(windows)]
fn set_dll_directory(dir: &Path) {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetDllDirectoryW(path: *const u16) -> i32;
    }
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    unsafe { SetDllDirectoryW(wide.as_ptr()) };
}
