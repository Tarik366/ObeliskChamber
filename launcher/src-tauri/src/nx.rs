// SPDX-License-Identifier: GPL-3.0-or-later
//! Reads an `.nx` archive (NXFormat, https://github.com/fab918/nxformat) and extracts it into the
//! folder layout the rest of the launcher expects: `<out>/base/{exefs,romfs}`,
//! `<out>/updates/<version>/{exefs,romfs}`, and `<out>/dlc/<title id>/<version>/romfs`.
//!
//! Extraction can be performed using either:
//! 1. The C API / shared library (`nxformat.dll` / `libnxformat.so` / `libnxformat.dylib`) via
//!    in-process FFI calls (`nxformat_run`, `nxformat_cancel_*`).
//! 2. The CLI tool (`nxconvert extract ARCHIVE.nx OUT_DIR --frontend`).
//!
//! A finished unpack of the same archive is stamped and reused.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const MARKER: &str = ".nxformat_done";

// --- C API Definitions (ffi/include/nxformat.h) ---

const NXFORMAT_ABI_VERSION: u32 = 1;

#[allow(dead_code)]
pub const NXFORMAT_OK: c_int = 0;
pub const NXFORMAT_FAILED: c_int = 1;
pub const NXFORMAT_CANCELLED: c_int = 2;
pub const NXFORMAT_INVALID: c_int = 3;
pub const NXFORMAT_INTERNAL: c_int = 4;

type NxformatCancel = c_void;
type NxformatLineFn = unsafe extern "C" fn(user: *mut c_void, is_error: c_int, line: *const c_char);

type FnAbiVersion = unsafe extern "C" fn() -> u32;
type FnVersion = unsafe extern "C" fn() -> *const c_char;
type FnCancelNew = unsafe extern "C" fn() -> *mut NxformatCancel;
type FnCancelSet = unsafe extern "C" fn(cancel: *mut NxformatCancel);
type FnCancelFree = unsafe extern "C" fn(cancel: *mut NxformatCancel);
type FnRun = unsafe extern "C" fn(
    args: *const *const c_char,
    count: usize,
    on_line: Option<NxformatLineFn>,
    user: *mut c_void,
    cancel: *mut NxformatCancel,
    error: *mut c_char,
    error_size: usize,
) -> c_int;

struct NxformatLib {
    #[cfg(windows)]
    _handle: windows_sys::HMODULE,
    #[cfg(unix)]
    _handle: *mut c_void,
    abi_version: FnAbiVersion,
    version: FnVersion,
    cancel_new: FnCancelNew,
    cancel_set: FnCancelSet,
    cancel_free: FnCancelFree,
    run: FnRun,
}

#[cfg(windows)]
mod windows_sys {
    pub type HMODULE = *mut std::ffi::c_void;
    pub type FARPROC = *mut std::ffi::c_void;
    pub type LPCWSTR = *const u16;
    pub type LPCSTR = *const u8;

    extern "system" {
        pub fn LoadLibraryW(lpLibFileName: LPCWSTR) -> HMODULE;
        pub fn GetProcAddress(hModule: HMODULE, lpProcName: LPCSTR) -> FARPROC;
        pub fn FreeLibrary(hLibModule: HMODULE) -> i32;
    }
}

#[cfg(unix)]
mod unix_sys {
    use std::ffi::{c_char, c_int, c_void};
    pub const RTLD_NOW: c_int = 2;
    extern "C" {
        pub fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
        pub fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        pub fn dlclose(handle: *mut c_void) -> c_int;
    }
}

impl NxformatLib {
    fn load(path: &Path) -> Result<Self, String> {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let handle = unsafe { windows_sys::LoadLibraryW(wide.as_ptr()) };
            if handle.is_null() {
                return Err(format!("cannot load {}", path.display()));
            }

            macro_rules! get_sym {
                ($name:expr, $ty:ty) => {{
                    let c_name = concat!($name, "\0");
                    let proc = unsafe { windows_sys::GetProcAddress(handle, c_name.as_ptr()) };
                    if proc.is_null() {
                        unsafe { windows_sys::FreeLibrary(handle) };
                        return Err(format!("symbol {} not found in {}", $name, path.display()));
                    }
                    unsafe { std::mem::transmute::<windows_sys::FARPROC, $ty>(proc) }
                }};
            }

            let abi_version = get_sym!("nxformat_abi_version", FnAbiVersion);
            let version = get_sym!("nxformat_version", FnVersion);
            let cancel_new = get_sym!("nxformat_cancel_new", FnCancelNew);
            let cancel_set = get_sym!("nxformat_cancel_set", FnCancelSet);
            let cancel_free = get_sym!("nxformat_cancel_free", FnCancelFree);
            let run = get_sym!("nxformat_run", FnRun);

            let lib = Self {
                _handle: handle,
                abi_version,
                version,
                cancel_new,
                cancel_set,
                cancel_free,
                run,
            };

            let abi = unsafe { (lib.abi_version)() };
            if abi != NXFORMAT_ABI_VERSION {
                return Err(format!(
                    "nxformat library ABI version mismatch (got {abi}, expected {NXFORMAT_ABI_VERSION})"
                ));
            }
            Ok(lib)
        }

        #[cfg(unix)]
        {
            let c_path = CString::new(path.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
            let handle = unsafe { unix_sys::dlopen(c_path.as_ptr(), unix_sys::RTLD_NOW) };
            if handle.is_null() {
                return Err(format!("cannot load {}", path.display()));
            }

            macro_rules! get_sym {
                ($name:expr, $ty:ty) => {{
                    let c_name = CString::new($name).unwrap();
                    let sym = unsafe { unix_sys::dlsym(handle, c_name.as_ptr()) };
                    if sym.is_null() {
                        unsafe { unix_sys::dlclose(handle) };
                        return Err(format!("symbol {} not found in {}", $name, path.display()));
                    }
                    unsafe { std::mem::transmute::<*mut c_void, $ty>(sym) }
                }};
            }

            let abi_version = get_sym!("nxformat_abi_version", FnAbiVersion);
            let version = get_sym!("nxformat_version", FnVersion);
            let cancel_new = get_sym!("nxformat_cancel_new", FnCancelNew);
            let cancel_set = get_sym!("nxformat_cancel_set", FnCancelSet);
            let cancel_free = get_sym!("nxformat_cancel_free", FnCancelFree);
            let run = get_sym!("nxformat_run", FnRun);

            let lib = Self {
                _handle: handle,
                abi_version,
                version,
                cancel_new,
                cancel_set,
                cancel_free,
                run,
            };

            let abi = unsafe { (lib.abi_version)() };
            if abi != NXFORMAT_ABI_VERSION {
                return Err(format!(
                    "nxformat library ABI version mismatch (got {abi}, expected {NXFORMAT_ABI_VERSION})"
                ));
            }
            Ok(lib)
        }
    }
}

#[cfg(windows)]
impl Drop for NxformatLib {
    fn drop(&mut self) {
        if !self._handle.is_null() {
            unsafe { windows_sys::FreeLibrary(self._handle) };
        }
    }
}

#[cfg(unix)]
impl Drop for NxformatLib {
    fn drop(&mut self) {
        if !self._handle.is_null() {
            unsafe { unix_sys::dlclose(self._handle) };
        }
    }
}

/// Locate the `nxformat` shared library (`nxformat.dll`, `libnxformat.so`, or `libnxformat.dylib`).
pub fn find_nxformat_lib() -> Option<PathBuf> {
    let lib_name = if cfg!(windows) {
        "nxformat.dll"
    } else if cfg!(target_os = "macos") {
        "libnxformat.dylib"
    } else {
        "libnxformat.so"
    };

    // 1. Environment variable NXFORMAT_LIB
    if let Ok(p) = std::env::var("NXFORMAT_LIB") {
        let pb = PathBuf::from(&p);
        if pb.is_file() {
            return Some(pb);
        }
    }

    // 2. Next to the executable or in tools/
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let direct = parent.join(lib_name);
            if direct.is_file() {
                return Some(direct);
            }
            let tools_lib = parent.join("tools").join("nxformat").join(lib_name);
            if tools_lib.is_file() {
                return Some(tools_lib);
            }
        }
    }

    // 3. Search PATH
    if let Some(p) = which(lib_name) {
        return Some(p);
    }

    // 4. Common install folders on Windows
    #[cfg(windows)]
    {
        let candidates = [
            PathBuf::from(r"C:\tools\nxformat").join(lib_name),
            PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default())
                .join("nxformat")
                .join(lib_name),
            PathBuf::from(std::env::var("ProgramFiles").unwrap_or_default())
                .join("nxformat")
                .join(lib_name),
        ];
        for c in &candidates {
            if c.is_file() {
                return Some(c.clone());
            }
        }
    }

    None
}

/// Find the `nxconvert` binary CLI tool.
pub fn find_nxconvert() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("NXCONVERT") {
        let pb = PathBuf::from(&p);
        if pb.is_file() {
            return Ok(pb);
        }
    }

    let exe_name = if cfg!(windows) { "nxconvert.exe" } else { "nxconvert" };
    if let Some(p) = which(exe_name) {
        return Ok(p);
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let direct = parent.join(exe_name);
            if direct.is_file() {
                return Ok(direct);
            }
            let tools_bin = parent.join("tools").join("nxconvert").join(exe_name);
            if tools_bin.is_file() {
                return Ok(tools_bin);
            }
        }
    }

    #[cfg(windows)]
    {
        let candidates = [
            PathBuf::from(r"C:\tools\nxconvert\nxconvert.exe"),
            PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default())
                .join("nxconvert")
                .join("nxconvert.exe"),
            PathBuf::from(std::env::var("ProgramFiles").unwrap_or_default())
                .join("nxconvert")
                .join("nxconvert.exe"),
        ];
        for c in &candidates {
            if c.is_file() {
                return Ok(c.clone());
            }
        }
    }

    Err(
        "Neither the nxformat C library (nxformat.dll) nor nxconvert CLI was found. \
         Install nxformat/nxconvert (github.com/fab918/nxformat) or place nxformat.dll on PATH \
         or set the NXFORMAT_LIB / NXCONVERT environment variable."
            .into(),
    )
}

fn which(exe: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(exe))
        .find(|p| p.is_file())
}

/// Verify that the file starts with the NXFormat archive magic:
/// Slot A at offset 0: "NXAR"
/// (or Slot B at offset 4096: "NXAR")
pub fn is_nx_format(path: &Path) -> Result<bool, String> {
    let mut file = File::open(path).map_err(|e| format!("cannot open {}: {}", path.display(), e))?;
    let mut buf = [0u8; 4096 + 4];
    let n = file.read(&mut buf).map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    if n >= 4 && &buf[..4] == b"NXAR" {
        return Ok(true);
    }
    if n >= 4096 + 4 && &buf[4096..4096 + 4] == b"NXAR" {
        return Ok(true);
    }
    Ok(false)
}

fn file_stamp(path: &Path) -> String {
    fs::metadata(path)
        .map(|m| {
            let t = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("{}:{}", m.len(), t)
        })
        .unwrap_or_default()
}

struct CbContext<'a> {
    total_work: u64,
    current_done: u64,
    current_file: String,
    progress: &'a mut dyn FnMut(u64, u64, &str),
}

unsafe extern "C" fn on_line_callback(user: *mut c_void, _is_error: c_int, line: *const c_char) {
    if user.is_null() || line.is_null() {
        return;
    }
    let ctx = &mut *(user as *mut CbContext);
    let str_line = CStr::from_ptr(line).to_string_lossy();
    let trimmed = str_line.trim();

    if trimmed.starts_with("@TOTAL") {
        let parts: Vec<&str> = trimmed.split('\t').collect();
        if parts.len() >= 2 {
            if let Ok(tot) = parts[1].trim().parse::<u64>() {
                ctx.total_work = tot;
            }
        }
    } else if trimmed.starts_with("@PROGRESS") {
        let parts: Vec<&str> = trimmed.split('\t').collect();
        if parts.len() >= 2 {
            if let Ok(cur) = parts[1].trim().parse::<u64>() {
                ctx.current_done = cur;
            }
            if parts.len() >= 3 {
                ctx.current_file = parts[2].trim().to_string();
            }
        }
        (ctx.progress)(ctx.current_done, ctx.total_work, &ctx.current_file);
    } else if !trimmed.is_empty() && !trimmed.starts_with('@') {
        ctx.current_file = trimmed.to_string();
        (ctx.progress)(ctx.current_done, ctx.total_work, &ctx.current_file);
    }
}

/// Extract via in-process C library (`nxformat.dll`).
fn extract_with_c_lib(
    lib: &NxformatLib,
    nx_path: &Path,
    out: &Path,
    progress: &mut dyn FnMut(u64, u64, &str),
    cancel: &dyn Fn() -> bool,
) -> Result<(), String> {
    let cancel_handle = unsafe { (lib.cancel_new)() };
    if cancel_handle.is_null() {
        return Err("failed to allocate nxformat cancel handle".into());
    }

    let cancel_poll = Arc::new(AtomicBool::new(false));
    let cancel_thread = {
        let cancel_ptr = cancel_handle as usize;
        let cancel_set_fn = lib.cancel_set;
        let cancel_poll = cancel_poll.clone();
        std::thread::spawn(move || {
            while !cancel_poll.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            unsafe {
                cancel_set_fn(cancel_ptr as *mut NxformatCancel);
            }
        })
    };

    let arg_extract = CString::new("extract").unwrap();
    let arg_src = CString::new(nx_path.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
    let arg_out = CString::new(out.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
    let arg_frontend = CString::new("--frontend").unwrap();

    let args = [
        arg_extract.as_ptr(),
        arg_src.as_ptr(),
        arg_out.as_ptr(),
        arg_frontend.as_ptr(),
    ];

    let mut ctx = CbContext {
        total_work: 0,
        current_done: 0,
        current_file: String::new(),
        progress,
    };

    let mut err_buf = [0u8; 1024];

    // Check cancellation before calling
    if cancel() {
        cancel_poll.store(true, Ordering::Relaxed);
        let _ = cancel_thread.join();
        unsafe { (lib.cancel_free)(cancel_handle) };
        return Err("cancelled".into());
    }

    let code = unsafe {
        (lib.run)(
            args.as_ptr(),
            args.len(),
            Some(on_line_callback),
            &mut ctx as *mut _ as *mut c_void,
            cancel_handle,
            err_buf.as_mut_ptr() as *mut c_char,
            err_buf.len(),
        )
    };

    cancel_poll.store(true, Ordering::Relaxed);
    let _ = cancel_thread.join();
    unsafe { (lib.cancel_free)(cancel_handle) };

    match code {
        0 => Ok(()),
        NXFORMAT_CANCELLED => Err("cancelled".into()),
        _ => {
            let msg = CStr::from_bytes_until_nul(&err_buf)
                .map(|cs| cs.to_string_lossy().into_owned())
                .unwrap_or_else(|_| format!("nxformat_run failed with code {code}"));
            Err(msg)
        }
    }
}

/// Extract via `nxconvert` CLI subprocess.
fn extract_with_cli(
    nxconvert: &Path,
    nx_path: &Path,
    out: &Path,
    progress: &mut dyn FnMut(u64, u64, &str),
    cancel: &dyn Fn() -> bool,
) -> Result<(), String> {
    let mut cmd = Command::new(nxconvert);
    cmd.arg("extract")
        .arg(nx_path)
        .arg(out)
        .arg("--frontend");

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    crate::setup::quiet(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not start nxconvert ({}): {}", nxconvert.display(), e))?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let stderr_handle = std::thread::spawn(move || {
        let mut err_lines = Vec::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            err_lines.push(line);
        }
        err_lines
    });

    let mut total_work: u64 = 0;
    let mut current_done: u64 = 0;
    let mut current_file = String::new();

    let reader = BufReader::new(stdout);
    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.starts_with("@TOTAL") {
            let parts: Vec<&str> = trimmed.split('\t').collect();
            if parts.len() >= 2 {
                if let Ok(tot) = parts[1].trim().parse::<u64>() {
                    total_work = tot;
                }
            }
        } else if trimmed.starts_with("@PROGRESS") {
            let parts: Vec<&str> = trimmed.split('\t').collect();
            if parts.len() >= 2 {
                if let Ok(cur) = parts[1].trim().parse::<u64>() {
                    current_done = cur;
                }
                if parts.len() >= 3 {
                    current_file = parts[2].trim().to_string();
                }
            }
            progress(current_done, total_work, &current_file);
        } else if !trimmed.is_empty() && !trimmed.starts_with('@') {
            current_file = trimmed.to_string();
            progress(current_done, total_work, &current_file);
        }

        if cancel() {
            crate::setup::kill_tree(child.id());
            let _ = child.wait();
            return Err("cancelled".into());
        }
    }

    let status = child.wait().map_err(|e| e.to_string())?;
    let stderr_lines = stderr_handle.join().unwrap_or_default();

    if !status.success() {
        let err_msg = stderr_lines
            .iter()
            .rev()
            .find(|l| !l.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| format!("nxconvert exited with status: {}", status));
        return Err(format!("nxconvert extraction failed: {}", err_msg));
    }

    Ok(())
}

/// Extracts an `.nx` (NXFormat) archive into `out`.
/// Prefers the in-process C library (`nxformat.dll` / `libnxformat.so`) if available;
/// falls back to `nxconvert` executable if the shared library is not present.
pub fn extract(
    nx_path: &Path,
    out: &Path,
    progress: &mut dyn FnMut(u64, u64, &str),
    cancel: &dyn Fn() -> bool,
) -> Result<PathBuf, String> {
    if !is_nx_format(nx_path)? {
        return Err("This file is not a valid .nx (NXFormat) archive (missing NXAR magic).".into());
    }

    let stamp = format!("{}:{}", crate::dump::fwd(nx_path), file_stamp(nx_path));
    if fs::read_to_string(out.join(MARKER))
        .map(|s| s.trim() == stamp)
        .unwrap_or(false)
    {
        return Ok(out.to_path_buf());
    }

    if out.exists() {
        fs::remove_dir_all(out).map_err(|e| format!("cannot clear {}: {}", out.display(), e))?;
    }
    // Do not pre-create `out` with fs::create_dir_all:
    // nxformat / nxconvert expects NEW_FOLDER to NOT exist yet, and creates it with
    // create_dir / OpenOptions::create_new(true). If `out` already exists, Windows returns
    // ERROR_ALREADY_EXISTS (os error 183).
    if let Some(parent) = out.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {}", parent.display(), e))?;
        }
    }

    progress(0, 0, "Starting NXFormat extraction...");

    // Try C library first
    if let Some(lib_path) = find_nxformat_lib() {
        if let Ok(lib) = NxformatLib::load(&lib_path) {
            extract_with_c_lib(&lib, nx_path, out, progress, cancel)?;
            progress(1, 1, "");
            fs::write(out.join(MARKER), stamp).map_err(|e| e.to_string())?;
            return Ok(out.to_path_buf());
        }
    }

    // Fall back to CLI executable
    let nxconvert = find_nxconvert()?;
    extract_with_cli(&nxconvert, nx_path, out, progress, cancel)?;

    progress(1, 1, "");
    fs::write(out.join(MARKER), stamp).map_err(|e| e.to_string())?;

    Ok(out.to_path_buf())
}
