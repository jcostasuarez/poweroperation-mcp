//! Graphics Builder (`CtDraw32.exe`) por automatización COM.
//!
//! El servidor COM `GraphicsBuilder.Application` es un exe de 32 bits fuera de proceso. El binder de .NET falla con
//! su type library (`PropertiesButtonGetEx` está declarado dos veces), así que acá se usa IDispatch crudo:
//! `GetIDsOfNames` + `Invoke`, argumentos en orden inverso y parámetros de salida como `VT_BYREF`.
//! Todo corre en un hilo STA propio. Ante errores Graphics Builder abre diálogos modales que bloquean la llamada:
//! un vigilante los cierra (No / OK / Cancelar) y devuelve su texto.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use windows_sys::Win32::Foundation::{CloseHandle, HWND, INVALID_HANDLE_VALUE, LPARAM, SysAllocStringLen, SysFreeString, SysStringLen};
use windows_sys::Win32::System::Com::{CLSIDFromProgID, COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize, EXCEPINFO, SAFEARRAY};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
use windows_sys::Win32::System::Ole::{GetActiveObject, SafeArrayAccessData, SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayGetVartype, SafeArrayUnaccessData};
use windows_sys::Win32::System::Variant::VariantClear;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetClassNameW, GetDlgItem, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, SendMessageW,
};
use windows_sys::core::{BOOL, BSTR, GUID, HRESULT};

const PROGID: &str = "GraphicsBuilder.Application";
/// CLSID de la coclass GraphicsBuilder (CtDraw32.TLB), por si el ProgID no está registrado.
const CLSID_GRAPHICS_BUILDER: GUID = GUID::from_u128(0xeb087a43_4189_46a9_8819_f717b9963c31);
const IID_IDISPATCH: GUID = GUID::from_u128(0x00020400_0000_0000_c000_000000000046);
const IID_NULL: GUID = GUID::from_u128(0);

pub const METHOD: u16 = 1;
pub const GET: u16 = 2;
pub const PUT: u16 = 4;
const DISPID_PROPERTYPUT: i32 = -3;
const DISP_E_EXCEPTION: HRESULT = 0x8002_0009_u32 as i32;

const VT_EMPTY: u16 = 0;
const VT_NULL: u16 = 1;
const VT_I2: u16 = 2;
const VT_I4: u16 = 3;
const VT_R8: u16 = 5;
const VT_BSTR: u16 = 8;
const VT_BOOL: u16 = 11;
const VT_I1: u16 = 16;
const VT_UI1: u16 = 17;
const VT_UI2: u16 = 18;
const VT_UI4: u16 = 19;
const VT_INT: u16 = 22;
const VT_UINT: u16 = 23;
const VT_ARRAY: u16 = 0x2000;
const VT_BYREF: u16 = 0x4000;

/// VARIANT con el mismo tamaño que el de Windows: 16 bytes en x86, 24 en x64 (valor desde el byte 8).
#[repr(C)]
#[derive(Clone, Copy)]
struct Variant {
    vt: u16,
    r1: u16,
    r2: u16,
    r3: u16,
    data: [usize; 2],
}

impl Variant {
    const fn empty() -> Self {
        Self { vt: VT_EMPTY, r1: 0, r2: 0, r3: 0, data: [0; 2] }
    }
}

#[repr(C)]
struct DispParams {
    rgvarg: *mut Variant,
    rgdispid_named: *mut i32,
    c_args: u32,
    c_named: u32,
}

#[repr(C)]
struct DispVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    _get_type_info_count: usize,
    _get_type_info: usize,
    get_ids_of_names: unsafe extern "system" fn(*mut c_void, *const GUID, *const *const u16, u32, u32, *mut i32) -> HRESULT,
    invoke: unsafe extern "system" fn(*mut c_void, i32, *const GUID, u32, u16, *mut DispParams, *mut Variant, *mut EXCEPINFO, *mut u32) -> HRESULT,
}

/// Puntero COM (IUnknown o IDispatch) con Release al soltarlo.
struct ComPtr(*mut c_void);

impl ComPtr {
    fn vtbl(&self) -> &DispVtbl {
        // SAFETY: un objeto COM empieza con el puntero a su vtable; IUnknown/IDispatch tienen este layout.
        unsafe { &**(self.0 as *mut *const DispVtbl) }
    }
}

impl Drop for ComPtr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: puntero obtenido de COM con una referencia propia.
            unsafe { (self.vtbl().release)(self.0) };
        }
    }
}

/// Argumento de una llamada. `Out*` son parámetros de salida.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Tipos de la type library que todavía no usa ninguna herramienta.
pub enum Arg {
    Str(String),
    I2(i16),
    I4(i32),
    Bool(bool),
    R8(f64),
    OutStr,
    OutI2,
    OutI4,
    OutBool,
    OutR8,
}

/// Valor devuelto (retorno o parámetro de salida).
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Empty,
    Str(String),
    Int(i64),
    Bool(bool),
    R8(f64),
    Bytes(Vec<u8>),
    Other(u16),
}

impl Val {
    pub fn text(&self) -> String {
        match self {
            Val::Empty | Val::Other(_) | Val::Bytes(_) => String::new(),
            Val::Str(s) => s.clone(),
            Val::Int(i) => i.to_string(),
            Val::Bool(b) => b.to_string(),
            Val::R8(f) => f.to_string(),
        }
    }

    pub fn int(&self) -> Option<i64> {
        match self {
            Val::Int(i) => Some(*i),
            Val::Bool(b) => Some(*b as i64),
            Val::R8(f) => Some(*f as i64),
            Val::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn json(&self) -> serde_json::Value {
        match self {
            Val::Empty | Val::Other(_) => serde_json::Value::Null,
            Val::Str(s) => s.clone().into(),
            Val::Int(i) => (*i).into(),
            Val::Bool(b) => (*b).into(),
            Val::R8(f) => (*f).into(),
            Val::Bytes(b) => format!("({} bytes)", b.len()).into(),
        }
    }
}

/// Resultado de una llamada: valor de retorno y parámetros de salida en orden.
#[derive(Debug, Clone)]
pub struct Reply {
    pub ret: Val,
    pub outs: Vec<Val>,
}

/// Color "#RRGGBB" (también "#RGB" o sin "#") al valor que usan las funciones "Ex" (True Color) de Graphics Builder:
/// entero positivo 0x00RRGGBB (la doc dice "An RGB value"). Verificado en runtime: con OLE_COLOR (0x00BBGGRR) un
/// relleno #E00000 se veía azul (0000E0).
/// Se pasa como VT_I4 positivo: como entero de 16 bits con signo (VBScript `&h00B000` = -20480) sale otro color.
pub fn colour_value(s: &str) -> Result<i32> {
    let h = s.trim().trim_start_matches('#');
    if !h.chars().all(|c| c.is_ascii_hexdigit()) || !matches!(h.len(), 3 | 6) {
        bail!("Color inválido '{s}': usá #RRGGBB (por ejemplo #00B000)");
    }
    let full: String = if h.len() == 3 { h.chars().flat_map(|c| [c, c]).collect() } else { h.to_string() };
    let byte = |i: usize| u8::from_str_radix(&full[i..i + 2], 16).map(i32::from);
    let (r, g, b) = (byte(0)?, byte(2)?, byte(4)?);
    Ok((r << 16) | (g << 8) | b)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Graphics Builder registrado como servidor COM (ProgID o CLSID, en la vista de 64 o de 32 bits del registro).
/// Solo lee el registro: no abre Graphics Builder.
pub fn registered() -> bool {
    use windows_sys::Win32::System::Registry::{HKEY, HKEY_CLASSES_ROOT, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY, RegCloseKey, RegOpenKeyExW};
    let clsid = format!("CLSID\\{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        CLSID_GRAPHICS_BUILDER.data1, CLSID_GRAPHICS_BUILDER.data2, CLSID_GRAPHICS_BUILDER.data3,
        CLSID_GRAPHICS_BUILDER.data4[0], CLSID_GRAPHICS_BUILDER.data4[1], CLSID_GRAPHICS_BUILDER.data4[2],
        CLSID_GRAPHICS_BUILDER.data4[3], CLSID_GRAPHICS_BUILDER.data4[4], CLSID_GRAPHICS_BUILDER.data4[5],
        CLSID_GRAPHICS_BUILDER.data4[6], CLSID_GRAPHICS_BUILDER.data4[7]);
    for key in [format!("{PROGID}\\CLSID"), clsid] {
        let w = wide(&key);
        for view in [KEY_WOW64_64KEY, KEY_WOW64_32KEY] {
            let mut h: HKEY = null_mut();
            // SAFETY: clave terminada en cero; el handle se cierra si se abrió.
            if unsafe { RegOpenKeyExW(HKEY_CLASSES_ROOT, w.as_ptr(), 0, KEY_READ | view, &mut h) } == 0 {
                unsafe { RegCloseKey(h) };
                return true;
            }
        }
    }
    false
}

fn bstr_new(s: &str) -> BSTR {
    let w: Vec<u16> = s.encode_utf16().collect();
    // SAFETY: copia `w.len()` caracteres en un BSTR nuevo.
    unsafe { SysAllocStringLen(w.as_ptr(), w.len() as u32) }
}

fn bstr_take(b: BSTR) -> String {
    if b.is_null() {
        return String::new();
    }
    // SAFETY: BSTR válido devuelto por COM; se libera después de copiarlo.
    unsafe {
        let len = SysStringLen(b) as usize;
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(b, len));
        SysFreeString(b);
        s
    }
}

fn bstr_peek(b: BSTR) -> String {
    if b.is_null() {
        return String::new();
    }
    // SAFETY: BSTR válido; no se libera (lo libera VariantClear).
    unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(b, SysStringLen(b) as usize)) }
}

fn hr_text(hr: HRESULT) -> String {
    let msg = std::io::Error::from_raw_os_error(hr).to_string();
    format!("HRESULT 0x{:08X} ({})", hr as u32, msg.trim())
}

/// Bytes de un SAFEARRAY de una dimensión de bytes (miniaturas).
unsafe fn safearray_bytes(psa: *const SAFEARRAY) -> Val {
    if psa.is_null() {
        return Val::Empty;
    }
    let mut vt = 0u16;
    let (mut lo, mut hi) = (0i32, -1i32);
    // SAFETY: SAFEARRAY válido del VARIANT de retorno (lo libera VariantClear).
    unsafe {
        SafeArrayGetVartype(psa, &mut vt);
        if vt != VT_UI1 && vt != VT_I1 {
            return Val::Other(VT_ARRAY | vt);
        }
        SafeArrayGetLBound(psa, 1, &mut lo);
        SafeArrayGetUBound(psa, 1, &mut hi);
        let n = (hi - lo + 1).max(0) as usize;
        let mut data: *mut c_void = null_mut();
        if SafeArrayAccessData(psa, &mut data) < 0 || data.is_null() {
            return Val::Bytes(vec![]);
        }
        let v = std::slice::from_raw_parts(data as *const u8, n).to_vec();
        SafeArrayUnaccessData(psa);
        Val::Bytes(v)
    }
}

/// Lee un VARIANT de retorno (sin liberarlo).
fn read_variant(v: &Variant) -> Val {
    let d = v.data[0];
    match v.vt {
        VT_EMPTY | VT_NULL => Val::Empty,
        VT_I2 => Val::Int(d as u16 as i16 as i64),
        VT_I4 | VT_INT => Val::Int(d as u32 as i32 as i64),
        VT_I1 => Val::Int(d as u8 as i8 as i64),
        VT_UI1 => Val::Int(d as u8 as i64),
        VT_UI2 => Val::Int(d as u16 as i64),
        VT_UI4 | VT_UINT => Val::Int(d as u32 as i64),
        // SAFETY: el double ocupa los 8 bytes desde el offset 8 (puede no estar alineado en x86).
        VT_R8 => Val::R8(unsafe { std::ptr::read_unaligned(v.data.as_ptr() as *const f64) }),
        VT_BSTR => Val::Str(bstr_peek(d as BSTR)),
        VT_BOOL => Val::Bool(d as u16 != 0),
        t if t & VT_ARRAY != 0 => unsafe { safearray_bytes(d as *const SAFEARRAY) },
        t => Val::Other(t),
    }
}

/// `CtDraw32.exe` de la instalación (para abrir Graphics Builder si no está abierto).
pub fn ctdraw_exe() -> Option<PathBuf> {
    static EXE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    EXE.get_or_init(|| {
        let mut found: Vec<PathBuf> = crate::discover::install_roots()
            .iter()
            .flat_map(|r| crate::discover::find_files(r, 4, 100_000, &|n| n == "ctdraw32.exe"))
            .collect();
        found.sort_by_key(|p| !p.to_string_lossy().to_lowercase().contains("power operation"));
        found.into_iter().next()
    })
    .clone()
}

/// PIDs de Graphics Builder abiertos.
pub fn running_pids() -> Vec<u32> {
    find_pids("CtDraw32.exe")
}

/// Procesos con este nombre de exe (sin distinguir mayúsculas).
fn find_pids(exe: &str) -> Vec<u32> {
    let mut out = Vec::new();
    // SAFETY: snapshot de procesos; la estructura se inicializa con su tamaño.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE || snap.is_null() {
            return out;
        }
        let mut pe: PROCESSENTRY32W = std::mem::zeroed();
        pe.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut pe);
        while ok != 0 {
            let len = pe.szExeFile.iter().position(|&c| c == 0).unwrap_or(pe.szExeFile.len());
            if String::from_utf16_lossy(&pe.szExeFile[..len]).eq_ignore_ascii_case(exe) {
                out.push(pe.th32ProcessID);
            }
            ok = Process32NextW(snap, &mut pe);
        }
        CloseHandle(snap);
    }
    out
}

// ---------------------------------------------------------------------------------------------------------------
// Vigilante de diálogos modales

struct Watch {
    busy: AtomicBool,
    stop: AtomicBool,
    pid: AtomicU32,
    seen: Mutex<Vec<String>>,
}

fn window_text(h: HWND) -> String {
    let mut buf = [0u16; 1024];
    // SAFETY: buffer local con su tamaño.
    let n = unsafe { GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

fn class_name(h: HWND) -> String {
    let mut buf = [0u16; 128];
    // SAFETY: buffer local con su tamaño.
    let n = unsafe { GetClassNameW(h, buf.as_mut_ptr(), buf.len() as i32) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

unsafe extern "system" fn collect_static(h: HWND, l: LPARAM) -> BOOL {
    // SAFETY: `l` apunta al Vec<String> de `dialog_text`, vivo durante EnumChildWindows.
    let texts = unsafe { &mut *(l as *mut Vec<String>) };
    if class_name(h).eq_ignore_ascii_case("Static") {
        let t = window_text(h).replace(['\r', '\n'], " ").trim().to_string();
        if !t.is_empty() {
            texts.push(t);
        }
    }
    1
}

fn dialog_text(h: HWND) -> String {
    let mut texts: Vec<String> = Vec::new();
    // SAFETY: callback sincrónico sobre una variable local.
    unsafe { EnumChildWindows(h, Some(collect_static), &mut texts as *mut _ as LPARAM) };
    format!("[{}] {}", window_text(h), texts.join(" "))
}

struct EnumCtx {
    pid: u32,
    found: Vec<HWND>,
}

unsafe extern "system" fn collect_dialogs(h: HWND, l: LPARAM) -> BOOL {
    // SAFETY: `l` apunta al EnumCtx de `dismiss_dialogs`.
    let ctx = unsafe { &mut *(l as *mut EnumCtx) };
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h, &mut pid) };
    if pid == ctx.pid && unsafe { IsWindowVisible(h) } != 0 && class_name(h) == "#32770" {
        ctx.found.push(h);
    }
    1
}

/// Avisos de upgrade de proyecto ("Cannot convert '...' to version 8.50 format"): no bloquean las llamadas y
/// aceptarlos hace que Graphics Builder se cierre, así que se dejan abiertos y solo se informan.
fn is_upgrade_notice(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("upgrad") || t.contains("cannot convert")
}

/// Aviso de Graphics Builder lanzado sin Power Operation Studio abierto ("Plant SCADA could not communicate with
/// Project Editor", verificado en PO 2024): así no se registra por COM y `ensure` esperaría en vano.
fn is_studio_missing(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("could not communicate") && t.contains("project editor")
}

/// Diálogos modales visibles (#32770) del proceso.
fn visible_dialogs(pid: u32) -> Vec<HWND> {
    let mut ctx = EnumCtx { pid, found: vec![] };
    // SAFETY: callback sincrónico sobre una variable local.
    unsafe { EnumWindows(Some(collect_dialogs), &mut ctx as *mut _ as LPARAM) };
    ctx.found
}

/// Cierra los diálogos modales visibles del proceso: No si existe (no guardar, no sobrescribir), si no OK,
/// si no Cancelar. Devuelve sus textos. Los avisos de upgrade se informan una vez (`reported`) y no se tocan.
fn dismiss_dialogs(pid: u32, reported: &mut HashSet<usize>) -> Vec<String> {
    const BM_CLICK: u32 = 0x00F5;
    let mut out = Vec::new();
    for h in visible_dialogs(pid) {
        let text = dialog_text(h);
        if is_upgrade_notice(&text) {
            if reported.insert(h as usize) {
                // GB muestra "Cannot convert" cuando no puede abrir <proyecto>\GLOBAL.CTL o su versión es < 6.
                let cause = if text.to_lowercase().contains("cannot convert") { " Causa habitual: al proyecto le falta GLOBAL.CTL (ver po_gb_status)." } else { "" };
                out.push(format!("{text} (se deja abierto: aceptarlo cierra Graphics Builder.{cause})"));
            }
            continue;
        }
        // IDNO = 7, IDOK = 1, IDCANCEL = 2.
        let button = [7, 1, 2].iter().map(|&id| unsafe { GetDlgItem(h, id) }).find(|b| !b.is_null());
        if let Some(b) = button {
            // SAFETY: botón de un diálogo de otro proceso; BM_CLICK es seguro entre procesos.
            unsafe { SendMessageW(b, BM_CLICK, 0, 0) };
            out.push(format!("{text} (cerrado: {})", window_text(b).replace('&', "")));
        } else {
            out.push(format!("{text} (sin botón para cerrarlo)"));
        }
    }
    out
}

fn spawn_watch(watch: Arc<Watch>) {
    std::thread::spawn(move || {
        let mut reported = HashSet::new();
        while !watch.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(250));
            let pid = watch.pid.load(Ordering::Relaxed);
            if pid != 0 && watch.busy.load(Ordering::Relaxed) {
                let closed = dismiss_dialogs(pid, &mut reported);
                if !closed.is_empty() {
                    watch.seen.lock().unwrap_or_else(|e| e.into_inner()).extend(closed);
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------------------------------------------
// Sesión COM (vive en el hilo STA)

pub struct Session {
    disp: Option<ComPtr>,
    ids: HashMap<String, i32>,
    watch: Arc<Watch>,
    /// Diálogos que Graphics Builder mostró (y el vigilante cerró) durante llamadas que igual terminaron bien.
    pub warnings: Vec<String>,
    /// Graphics Builder lo lanzó este proceso (abre una página "Untitled" que no hay que guardar).
    pub launched: bool,
}

impl Session {
    fn new(watch: Arc<Watch>) -> Self {
        Self { disp: None, ids: HashMap::new(), watch, warnings: vec![], launched: false }
    }

    pub fn pid(&self) -> u32 {
        self.watch.pid.load(Ordering::Relaxed)
    }

    fn attach(&mut self) -> Result<bool> {
        let mut clsid = CLSID_GRAPHICS_BUILDER;
        let progid = wide(PROGID);
        // SAFETY: ProgID terminado en cero; si no está registrado se usa el CLSID conocido.
        if unsafe { CLSIDFromProgID(progid.as_ptr(), &mut clsid) } < 0 {
            clsid = CLSID_GRAPHICS_BUILDER;
        }
        let mut unk: *mut c_void = null_mut();
        // SAFETY: busca la instancia registrada en la ROT.
        if unsafe { GetActiveObject(&clsid, null_mut(), &mut unk) } < 0 || unk.is_null() {
            return Ok(false);
        }
        let unk = ComPtr(unk);
        let mut disp: *mut c_void = null_mut();
        // SAFETY: QueryInterface sobre un IUnknown válido.
        let hr = unsafe { (unk.vtbl().query_interface)(unk.0, &IID_IDISPATCH, &mut disp) };
        if hr < 0 || disp.is_null() {
            bail!("Graphics Builder no expone IDispatch: {}", hr_text(hr));
        }
        self.disp = Some(ComPtr(disp));
        self.ids.clear();
        if let Some(&pid) = find_pids("ctdraw32.exe").first() {
            self.watch.pid.store(pid, Ordering::Relaxed);
        }
        Ok(true)
    }

    /// Conecta con Graphics Builder abierto o, con `launch`, lo abre (`exe`) y espera a que se registre.
    pub fn ensure(&mut self, launch: Option<&PathBuf>) -> Result<()> {
        if self.disp.is_some() {
            // ¿Sigue vivo? (lo pudo cerrar el usuario)
            if self.invoke("ProjectSelected", METHOD | GET, &[]).is_ok() {
                return Ok(());
            }
            self.disp = None;
        }
        if self.attach()? {
            return Ok(());
        }
        let Some(exe) = launch else {
            bail!(
                "Graphics Builder no está abierto. Abrilo desde Power Operation Studio o pedí launch=true \
                 (abre {}).",
                "CtDraw32.exe"
            );
        };
        let child = std::process::Command::new(exe)
            .current_dir(exe.parent().unwrap_or(std::path::Path::new(".")))
            .spawn()
            .map_err(|e| anyhow!("No se pudo lanzar {}: {e}", exe.display()))?;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(90) {
            std::thread::sleep(Duration::from_secs(1));
            if self.attach()? {
                self.launched = true;
                // Termina de inicializarse después de registrarse en la ROT.
                std::thread::sleep(Duration::from_secs(2));
                return Ok(());
            }
            // Sin Studio, Graphics Builder muestra un aviso y no se registra: no tiene sentido esperar los 90 s.
            // El aviso se deja abierto (no se sabe qué hace Graphics Builder al aceptarlo).
            let mut pids = find_pids("ctdraw32.exe");
            pids.push(child.id());
            pids.sort_unstable();
            pids.dedup();
            if let Some(text) = pids.into_iter().flat_map(visible_dialogs).map(dialog_text).find(|t| is_studio_missing(t)) {
                bail!(
                    "Graphics Builder necesita Power Operation Studio abierto. Al lanzarlo solo mostró: {text}. \
                     Pedile al usuario que cierre ese aviso (OK), abra Studio (con el proyecto) y vuelva a llamar \
                     po_gb_status (con launch=true, o abriendo Graphics Builder desde Studio)."
                );
            }
        }
        bail!("Graphics Builder ({}) no se registró en 90 s", exe.display())
    }

    fn dispid(&mut self, name: &str) -> Result<i32> {
        if let Some(&id) = self.ids.get(name) {
            return Ok(id);
        }
        let disp = self.disp.as_ref().ok_or_else(|| anyhow!("Sin conexión con Graphics Builder"))?;
        let w = wide(name);
        let names = [w.as_ptr()];
        let mut id = 0i32;
        // SAFETY: un nombre, un DISPID de salida.
        let hr = unsafe { (disp.vtbl().get_ids_of_names)(disp.0, &IID_NULL, names.as_ptr(), 1, 0, &mut id) };
        if hr < 0 {
            bail!("Graphics Builder no tiene el método '{name}': {}", hr_text(hr));
        }
        self.ids.insert(name.to_string(), id);
        Ok(id)
    }

    fn invoke(&mut self, name: &str, flags: u16, args: &[Arg]) -> Result<Reply> {
        let id = self.dispid(name)?;
        let disp = self.disp.as_ref().ok_or_else(|| anyhow!("Sin conexión con Graphics Builder"))?;
        let n = args.len();
        let mut vars = vec![Variant::empty(); n];
        let mut bstrs: Vec<BSTR> = Vec::new();
        // Almacenamiento de los parámetros de salida (8 bytes alcanzan para BSTR, I2, I4, BOOL y R8).
        let mut outs: Vec<(u16, Box<u64>)> = Vec::new();
        for (k, a) in args.iter().enumerate() {
            let v = &mut vars[n - 1 - k];
            match a {
                Arg::Str(s) => {
                    let b = bstr_new(s);
                    bstrs.push(b);
                    v.vt = VT_BSTR;
                    v.data[0] = b as usize;
                }
                Arg::I2(i) => {
                    v.vt = VT_I2;
                    v.data[0] = *i as u16 as usize;
                }
                Arg::I4(i) => {
                    v.vt = VT_I4;
                    v.data[0] = *i as u32 as usize;
                }
                Arg::Bool(b) => {
                    v.vt = VT_BOOL;
                    v.data[0] = if *b { 0xFFFF } else { 0 };
                }
                // SAFETY: escribe el double en los 8 bytes de datos.
                Arg::R8(f) => unsafe {
                    v.vt = VT_R8;
                    std::ptr::write_unaligned(v.data.as_mut_ptr() as *mut f64, *f);
                },
                out => {
                    let vt = match out {
                        Arg::OutStr => VT_BSTR,
                        Arg::OutI2 => VT_I2,
                        Arg::OutI4 => VT_I4,
                        Arg::OutBool => VT_BOOL,
                        _ => VT_R8,
                    };
                    let mut slot = Box::new(0u64);
                    v.vt = VT_BYREF | vt;
                    v.data[0] = (&mut *slot) as *mut u64 as usize;
                    outs.push((vt, slot));
                }
            }
        }
        let mut named = DISPID_PROPERTYPUT;
        let mut params = DispParams {
            rgvarg: if n == 0 { null_mut() } else { vars.as_mut_ptr() },
            rgdispid_named: if flags & PUT != 0 { &mut named } else { null_mut() },
            c_args: n as u32,
            c_named: if flags & PUT != 0 { 1 } else { 0 },
        };
        let mut result = Variant::empty();
        // SAFETY: EXCEPINFO es POD; se completa si el método falla.
        let mut excep: EXCEPINFO = unsafe { std::mem::zeroed() };
        let mut arg_err = 0u32;
        // SAFETY: argumentos armados arriba; viven hasta después de la llamada.
        let hr = unsafe { (disp.vtbl().invoke)(disp.0, id, &IID_NULL, 0, flags, &mut params, &mut result, &mut excep, &mut arg_err) };
        for b in bstrs {
            // SAFETY: BSTR creados en esta función; los argumentos de entrada no los toma el servidor.
            unsafe { SysFreeString(b) };
        }
        let out_vals: Vec<Val> = outs
            .into_iter()
            .map(|(vt, slot)| {
                let raw = *slot;
                match vt {
                    VT_BSTR => Val::Str(bstr_take(raw as usize as BSTR)),
                    VT_I2 => Val::Int(raw as u16 as i16 as i64),
                    VT_I4 => Val::Int(raw as u32 as i32 as i64),
                    VT_BOOL => Val::Bool(raw as u16 != 0),
                    _ => Val::R8(f64::from_bits(raw)),
                }
            })
            .collect();
        if hr < 0 {
            let detail = if hr == DISP_E_EXCEPTION {
                let desc = bstr_take(excep.bstrDescription);
                let _ = bstr_take(excep.bstrSource);
                let _ = bstr_take(excep.bstrHelpFile);
                format!("excepción 0x{:08X}{}", excep.scode as u32, if desc.is_empty() { String::new() } else { format!(": {desc}") })
            } else {
                hr_text(hr)
            };
            bail!("{name} falló: {detail}");
        }
        let ret = read_variant(&result);
        // SAFETY: libera lo que el servidor haya puesto en el resultado.
        unsafe { VariantClear(&mut result as *mut Variant as *mut _) };
        Ok(Reply { ret, outs: out_vals })
    }

    /// Llamada con el vigilante de diálogos activo.
    pub fn call(&mut self, name: &str, flags: u16, args: &[Arg]) -> Result<Reply> {
        self.watch.busy.store(true, Ordering::Relaxed);
        let r = self.invoke(name, flags, args);
        self.watch.busy.store(false, Ordering::Relaxed);
        let dialogs: Vec<String> = std::mem::take(&mut *self.watch.seen.lock().unwrap_or_else(|e| e.into_inner()));
        match r {
            Ok(rep) => {
                self.warnings.extend(dialogs.into_iter().map(|d| format!("{name}: {d}")));
                Ok(rep)
            }
            Err(e) if dialogs.is_empty() => Err(e),
            Err(e) => Err(e.context(format!("Graphics Builder mostró: {}", dialogs.join(" | ")))),
        }
    }

    /// Método (con o sin valor de retorno).
    pub fn m(&mut self, name: &str, args: &[Arg]) -> Result<Reply> {
        self.call(name, METHOD | GET, args)
    }

    /// Lectura de propiedad.
    pub fn get(&mut self, name: &str) -> Result<Val> {
        Ok(self.call(name, GET, &[])?.ret)
    }

    /// Escritura de propiedad.
    pub fn put(&mut self, name: &str, value: Arg) -> Result<()> {
        self.call(name, PUT, &[value]).map(|_| ())
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Hilo STA y acceso desde async

type Job = Box<dyn FnOnce(&mut Session) + Send>;

struct Worker {
    tx: mpsc::Sender<Job>,
}

fn spawn_worker() -> Worker {
    let (tx, rx) = mpsc::channel::<Job>();
    std::thread::spawn(move || {
        // SAFETY: inicializa COM en este hilo como STA.
        unsafe { CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32) };
        let watch = Arc::new(Watch { busy: AtomicBool::new(false), stop: AtomicBool::new(false), pid: AtomicU32::new(0), seen: Mutex::new(vec![]) });
        spawn_watch(watch.clone());
        let mut session = Session::new(watch.clone());
        for job in rx {
            job(&mut session);
        }
        watch.stop.store(true, Ordering::Relaxed);
        drop(session);
        // SAFETY: par de CoInitializeEx.
        unsafe { CoUninitialize() };
    });
    Worker { tx }
}

/// Acceso a Graphics Builder: un hilo STA con la sesión COM; las llamadas tienen tiempo máximo.
#[derive(Clone)]
pub struct Gb {
    worker: Arc<Mutex<Option<Worker>>>,
    timeout: Duration,
}

impl Gb {
    pub fn new(timeout: Duration) -> Self {
        Self { worker: Arc::new(Mutex::new(None)), timeout }
    }

    /// Ejecuta `f` en el hilo COM. Si se agota el tiempo, el hilo se abandona (puede seguir bloqueado en un
    /// diálogo) y la próxima llamada abre uno nuevo.
    pub async fn call<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Session) -> Result<T> + Send + 'static,
    {
        let (rtx, rrx) = tokio::sync::oneshot::channel();
        let job: Job = Box::new(move |s| {
            let _ = rtx.send(f(s));
        });
        {
            let mut w = self.worker.lock().unwrap_or_else(|e| e.into_inner());
            let worker = w.get_or_insert_with(spawn_worker);
            if let Err(mpsc::SendError(job)) = worker.tx.send(job) {
                let fresh = spawn_worker();
                fresh.tx.send(job).map_err(|_| anyhow!("No se pudo iniciar el hilo COM de Graphics Builder"))?;
                *w = Some(fresh);
            }
        }
        match tokio::time::timeout(self.timeout, rrx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => {
                *self.worker.lock().unwrap_or_else(|e| e.into_inner()) = None;
                bail!("El hilo COM de Graphics Builder terminó inesperadamente")
            }
            Err(_) => {
                *self.worker.lock().unwrap_or_else(|e| e.into_inner()) = None;
                bail!(
                    "Tiempo agotado ({} s) esperando a Graphics Builder: puede tener un diálogo abierto o estar ocupado. \
                     Revisá la ventana de Graphics Builder.",
                    self.timeout.as_secs()
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_size_matches_windows() {
        assert_eq!(std::mem::size_of::<Variant>(), if cfg!(target_pointer_width = "64") { 24 } else { 16 });
    }

    #[test]
    fn bstr_roundtrip() {
        for s in ["Página ñ", ""] {
            assert_eq!(bstr_take(bstr_new(s)), s);
        }
        assert_eq!(bstr_take(std::ptr::null()), "");
    }

    /// `#RRGGBB` → 0x00RRGGBB positivo (VT_I4): rojo en el byte alto, visto en runtime; como I2 el verde daba -20480.
    #[test]
    fn colour_value_is_rgb_positive() {
        for (s, v) in [
            ("#E00000", 0xE00000),
            ("#00B000", 0x00B000),
            ("#0000FF", 0x0000FF),
            ("#FFFFFF", 0xFFFFFF),
            ("#000", 0),
            ("#fff", 0xFFFFFF),
            ("  #aBc123 ", 0xABC123),
            ("00ff00", 0x00FF00),
        ] {
            assert_eq!(colour_value(s).unwrap(), v, "{s}");
        }
        for bad in ["", "#", "#12345", "#1234567", "#GG0000", "red", "#00B 000", "ñññ"] {
            let e = colour_value(bad).unwrap_err().to_string();
            assert!(e.contains("usá #RRGGBB"), "{bad:?}: {e}");
        }
    }

    /// Avisos modales: el de Studio cerrado corta el arranque; el de conversión de versión no se acepta (cierra GB).
    #[test]
    fn classifies_notices() {
        assert!(is_studio_missing("[Graphics Builder] Plant SCADA could not communicate with Project Editor."));
        assert!(is_studio_missing("[Plant SCADA] PLANT SCADA COULD NOT COMMUNICATE WITH PROJECT EDITOR"));
        assert!(!is_studio_missing("[Graphics Builder] Cannot convert 'MCP_Demo' to version 8.50 format"));
        assert!(!is_studio_missing("[Graphics Builder] Could not communicate with the I/O server"));
        assert!(is_upgrade_notice("[Graphics Builder] Cannot convert 'MCP_Demo' to version 8.50 format"));
        assert!(is_upgrade_notice("Project needs to be upgraded"));
        assert!(!is_upgrade_notice("[Graphics Builder] Plant SCADA could not communicate with Project Editor."));
    }
}
