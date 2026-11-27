//! CtAPI: `CtApi.dll` de Power Operation / Plant SCADA cargada en tiempo de ejecución.
//!
//! El DLL no se distribuye con el MCP: se usa el de la instalación (carpeta `Bin\Bin (x64)`), con sus
//! dependencias nativas y .NET al lado. La arquitectura del DLL debe coincidir con la del .exe.
//! Todas las llamadas pasan por un hilo bloqueante con mutex (CtAPI es síncrona) y tiempo máximo.

use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value};

use crate::config::Config;
use crate::discover;
use crate::text::{cstring, decode_cstr};

type Handle = *mut c_void;

type FnOpen = unsafe extern "system" fn(*const c_char, *const c_char, *const c_char, u32) -> Handle;
type FnClose = unsafe extern "system" fn(Handle) -> i32;
type FnTagRead = unsafe extern "system" fn(Handle, *const c_char, *mut c_char, u32) -> i32;
type FnTagWrite = unsafe extern "system" fn(Handle, *const c_char, *const c_char) -> i32;
type FnCicode = unsafe extern "system" fn(Handle, *const c_char, u32, u32, *mut c_char, u32, *mut c_void) -> u32;
type FnFindFirst = unsafe extern "system" fn(Handle, *const c_char, *const c_char, *mut Handle, u32) -> Handle;
type FnFindFirstEx = unsafe extern "system" fn(Handle, *const c_char, *const c_char, *const c_char, *mut Handle, u32) -> Handle;
type FnFindNext = unsafe extern "system" fn(Handle, *mut Handle) -> i32;
type FnFindClose = unsafe extern "system" fn(Handle) -> i32;
type FnGetProperty = unsafe extern "system" fn(Handle, *const c_char, *mut c_void, u32, *mut c_void, u32) -> i32;
type FnSetManagedBin = unsafe extern "system" fn(*const c_char) -> i32;
type FnTagReadEx = unsafe extern "system" fn(Handle, *const c_char, *mut c_char, u32, *mut TagValueItems) -> i32;
type FnTagGetProperty = unsafe extern "system" fn(Handle, *const c_char, *const c_char, *mut c_void, u32, u32) -> i32;
type FnListNew = unsafe extern "system" fn(Handle, u32) -> Handle;
type FnListFree = unsafe extern "system" fn(Handle) -> i32;
type FnListAdd = unsafe extern "system" fn(Handle, *const c_char) -> Handle;
type FnListRead = unsafe extern "system" fn(Handle, *mut c_void) -> i32;
type FnListData = unsafe extern "system" fn(Handle, *mut c_void, u32, u32) -> i32;
type FnListItem = unsafe extern "system" fn(Handle, u32, *mut c_void, u32, u32) -> i32;

/// `CT_TAGVALUE_ITEMS` de ctapi.h (`#pragma pack(push, 1)`: 38 bytes). Los TIMESTAMP son FILETIME (100 ns desde
/// 1601, según la ayuda del tipo TIMESTAMP de Cicode); 0 = sin hora.
#[repr(C, packed)]
#[derive(Default, Clone, Copy)]
pub struct TagValueItems {
    dw_length: u32,
    timestamp: u64,
    value_timestamp: u64,
    quality_timestamp: u64,
    quality_general: u8,
    quality_substatus: u8,
    quality_limit: u8,
    quality_extended: u8,
    datasource_error: u32,
    overridden: u8,
    control_inhibit: u8,
}

/// Partes de la calidad de un tag ("The Quality Item" / QualityGetPart en la ayuda).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Quality {
    pub general: u8,
    pub substatus: u8,
    pub limit: u8,
    pub extended: u8,
    pub datasource_error: u32,
    pub overridden: bool,
    pub control_inhibit: bool,
}

/// Lectura de un tag: valor (texto en unidades de ingeniería) y, si CtAPI la da, calidad y horas (FILETIME).
#[derive(Debug, Clone, Default)]
pub struct TagReading {
    pub value: String,
    pub quality: Option<Quality>,
    pub timestamp: u64,
    pub value_timestamp: u64,
    pub quality_timestamp: u64,
}

// ctapi.h: elementos de ctListItem.
const CT_LIST_TIMESTAMP: u32 = 2;
const CT_LIST_VALUE_TIMESTAMP: u32 = 3;
const CT_LIST_QUALITY_TIMESTAMP: u32 = 4;
const CT_LIST_QUALITY_GENERAL: u32 = 5;
const CT_LIST_QUALITY_SUBSTATUS: u32 = 6;
const CT_LIST_QUALITY_LIMIT: u32 = 7;
const CT_LIST_QUALITY_EXTENDED_SUBSTATUS: u32 = 8;
const CT_LIST_QUALITY_DATASOURCE_ERROR: u32 = 9;
const CT_LIST_QUALITY_OVERRIDE: u32 = 10;
const CT_LIST_QUALITY_CONTROL_MODE: u32 = 11;

const CT_OPEN_RECONNECT: u32 = 0x0002;
const DBTYPE_STR: u32 = 129;
/// CtAPI.h: los errores Citect se devuelven en GetLastError() como ERROR_USER_DEFINED_BASE + código.
const ERROR_USER_DEFINED_BASE: u32 = 0x1000_0000;
const VALUE_BUF: usize = 4096;
const CICODE_BUF: usize = 64 * 1024;

pub struct CtApiLib {
    pub path: PathBuf,
    open: FnOpen,
    close: FnClose,
    tag_read: FnTagRead,
    tag_write: FnTagWrite,
    cicode: FnCicode,
    find_first: Option<FnFindFirst>,
    find_first_ex: Option<FnFindFirstEx>,
    find_next: FnFindNext,
    find_close: FnFindClose,
    get_property: FnGetProperty,
    // Opcionales (ctapi.h de 8.x; un CtApi.dll viejo puede no tenerlas): sin ellas se usa ctTagRead.
    tag_read_ex: Option<FnTagReadEx>,
    tag_get_property: Option<FnTagGetProperty>,
    list: Option<ListFns>,
    // Mantiene el DLL cargado mientras existan los punteros de arriba.
    _lib: libloading::Library,
}

/// Funciones de listas (ctListNew...): se usan solo si están todas.
#[derive(Clone, Copy)]
struct ListFns {
    new: FnListNew,
    free: FnListFree,
    add: FnListAdd,
    read: FnListRead,
    data: FnListData,
    item: FnListItem,
}

impl TagReading {
    fn from_items(value: String, it: &TagValueItems) -> Self {
        // Copias: los campos de un struct packed no se pueden referenciar.
        let it = *it;
        TagReading {
            value,
            quality: Some(Quality {
                general: it.quality_general,
                substatus: it.quality_substatus,
                limit: it.quality_limit,
                extended: it.quality_extended,
                datasource_error: it.datasource_error,
                overridden: it.overridden != 0,
                control_inhibit: it.control_inhibit != 0,
            }),
            timestamp: it.timestamp,
            value_timestamp: it.value_timestamp,
            quality_timestamp: it.quality_timestamp,
        }
    }
}

/// Número de un elemento de ctListItem (la ayuda dice "formatted string"): entero decimal, hex 0x.. o TRUE/FALSE.
fn parse_list_num(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u64::from_str_radix(h, 16).ok();
    }
    match s.to_ascii_uppercase().as_str() {
        "TRUE" => Some(1),
        "FALSE" => Some(0),
        _ => s.parse().ok(),
    }
}

/// Valor y calidad de un tag de una lista ya leída. `None` si falta el valor o algún elemento no es numérico
/// (formato no esperado: quien llama relee con ctTagReadEx para que la salida sea siempre la misma).
fn list_tag(l: &ListFns, h: Handle) -> Option<TagReading> {
    let mut buf = vec![0u8; VALUE_BUF];
    if unsafe { (l.data)(h, buf.as_mut_ptr().cast(), buf.len() as u32, 0) } == 0 {
        return None;
    }
    let item = |which: u32| -> Option<u64> {
        let mut b = vec![0u8; 128];
        if unsafe { (l.item)(h, which, b.as_mut_ptr().cast(), b.len() as u32, 0) } == 0 {
            return None;
        }
        let s = decode_cstr(&b);
        // Hora vacía = sin hora; cualquier otro texto no numérico es un formato que no se conoce.
        if s.trim().is_empty() && matches!(which, CT_LIST_TIMESTAMP | CT_LIST_VALUE_TIMESTAMP | CT_LIST_QUALITY_TIMESTAMP) {
            return Some(0);
        }
        parse_list_num(&s)
    };
    let byte = |which| item(which).and_then(|v| u8::try_from(v).ok());
    Some(TagReading {
        value: decode_cstr(&buf),
        quality: Some(Quality {
            general: byte(CT_LIST_QUALITY_GENERAL)?,
            substatus: byte(CT_LIST_QUALITY_SUBSTATUS)?,
            limit: byte(CT_LIST_QUALITY_LIMIT)?,
            extended: byte(CT_LIST_QUALITY_EXTENDED_SUBSTATUS)?,
            datasource_error: item(CT_LIST_QUALITY_DATASOURCE_ERROR).and_then(|v| u32::try_from(v).ok())?,
            overridden: item(CT_LIST_QUALITY_OVERRIDE)? != 0,
            control_inhibit: item(CT_LIST_QUALITY_CONTROL_MODE)? != 0,
        }),
        timestamp: item(CT_LIST_TIMESTAMP)?,
        value_timestamp: item(CT_LIST_VALUE_TIMESTAMP)?,
        quality_timestamp: item(CT_LIST_QUALITY_TIMESTAMP)?,
    })
    // Un tag inexistente entra igual a la lista y vuelve vacío con BAD/INVALID_TAG (como ctTagReadEx): se relee solo
    // para que tag_read_ex dé el error real ("error Citect 424 Tag not found").
    .filter(|r| r.quality.is_none_or(|q| q.extended != QUAL_EXT_INVALID_TAG))
}

/// QUAL_EXT_INVALID_TAG: el tag no existe.
const QUAL_EXT_INVALID_TAG: u8 = 0x02;

/// Nombre de la calidad general (QUAL_BAD/QUAL_UNCR/QUAL_GOOD).
pub fn quality_general_name(g: u8) -> Option<&'static str> {
    Some(match g {
        0 => "BAD",
        1 => "UNCERTAIN",
        3 => "GOOD",
        _ => return None,
    })
}

/// Subestado según la calidad general (QUAL_BAD_*, QUAL_UNCR_*, QUAL_GOOD_* de QualityGetPart).
pub fn quality_substatus_name(general: u8, sub: u8) -> Option<&'static str> {
    Some(match (general, sub) {
        (_, 0) => "NON_SPECIFIC",
        (0, 1) => "CONFIGURATION_ERROR",
        (0, 2) => "NOT_CONNECTED",
        (0, 3) => "DEVICE_FAILURE",
        (0, 4) => "SENSOR_FAILURE",
        (0, 5) => "LAST_KNOWN_VALUE",
        (0, 6) => "COMM_FAILURE",
        (0, 7) => "OUT_OF_SERVICE",
        (0, 8) => "WAITING_FOR_INITIAL_DATA",
        (1, 1) => "LAST_USABLE_VALUE",
        (1, 4) => "SENSOR_NOT_ACCURATE",
        (1, 5) => "ENG_UNIT_EXCEEDED",
        (1, 6) => "SUBNORMAL",
        (3, 6) => "LOCAL_OVERRIDE",
        _ => return None,
    })
}

/// Límite (QUAL_LIMITED_*).
pub fn quality_limit_name(l: u8) -> Option<&'static str> {
    Some(match l {
        0 => "NOT_LIMITED",
        1 => "LOW",
        2 => "HIGH",
        3 => "CONSTANT",
        _ => return None,
    })
}

/// Subestado extendido (QUAL_EXT_*).
pub fn quality_extended_name(e: u8) -> Option<&'static str> {
    const NAMES: [&str; 21] = [
        "NON_SPECIFIC",
        "SCHEDULED_OFFLINE",
        "INVALID_TAG",
        "INVALID_DATA",
        "SOFTWARE_ERROR",
        "TOO_MANY_DEVICES",
        "COMM_NO_INIT",
        "COMM_BAD",
        "TAG_OUT_OF_RANGE",
        "WRITE_ONLY",
        "WRITE_PROTECTED",
        "NO_CLUSTER_SPECIFIED",
        "CLUSTER_NOT_FOUND",
        "CLUSTER_DISABLED",
        "SESSION_NOT_CONNECTED",
        "TAG_RESOLVE_TIMEOUT",
        "VALUE_OUT_OF_RANGE",
        "COMM_DEV_BAD",
        "COMM_IOSERVER_BAD",
        "STALE",
        "NOT_REPLICATED",
    ];
    NAMES.get(e as usize).copied()
}

/// Código de GetLastError() inmediatamente después de una llamada fallida.
fn last_error() -> u32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32
}

pub fn describe_error(code: u32) -> String {
    if code == 0 {
        "sin código de error".into()
    } else if code >= ERROR_USER_DEFINED_BASE {
        let ct = code - ERROR_USER_DEFINED_BASE;
        match crate::cterrors::title(ct) {
            Some(t) => format!("error Citect {ct} \"{t}\" (GetLastError 0x{code:08X})"),
            None if (1..=31).contains(&ct) => {
                format!("error Citect {ct}: error del dispositivo de I/O o de su comunicación (GetLastError 0x{code:08X})")
            }
            None => format!("error Citect {ct} (GetLastError 0x{code:08X})"),
        }
    } else {
        format!("error Windows {code}: {}", std::io::Error::from_raw_os_error(code as i32))
    }
}

/// Error de CtAPI con el código de GetLastError() que lo causó. El mensaje es el de `describe_error`;
/// el código permite reconocer un error (p. ej. 233) por valor y no solo por el texto.
#[derive(Debug)]
pub struct CtError {
    pub code: u32,
    msg: String,
}

impl std::fmt::Display for CtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for CtError {}

fn ct_error(code: u32, msg: String) -> anyhow::Error {
    anyhow::Error::new(CtError { code, msg })
}

fn fail(op: &str) -> anyhow::Error {
    let code = last_error();
    ct_error(code, format!("{op} falló: {}", describe_error(code)))
}

/// Código Citect 233: el proceso cliente del runtime no está (o se está reiniciando). Transitorio.
const CITECT_CLIENT_DOWN: u32 = 233;
/// Windows 233 (ERROR_PIPE_NOT_CONNECTED): reference.md lo informa para el mismo síntoma.
const WIN_PIPE_NOT_CONNECTED: u32 = 233;
/// Pausa antes de reconectar tras un 233 (el cliente tarda un momento en volver).
const RESTART_PAUSE: std::time::Duration = std::time::Duration::from_secs(1);

/// Código de error que indica que el cliente del runtime se está reiniciando.
fn is_restart_code(code: u32) -> bool {
    code == ERROR_USER_DEFINED_BASE + CITECT_CLIENT_DOWN || code == WIN_PIPE_NOT_CONNECTED
}

/// `true` si el error de CtAPI es de los transitorios de reinicio del cliente (ver `is_restart_code`).
pub fn is_restarting_error(e: &anyhow::Error) -> bool {
    e.downcast_ref::<CtError>().is_some_and(|c| is_restart_code(c.code))
}

/// Agrega al error la indicación de reintentar, si era un 233.
fn restart_hint(e: anyhow::Error) -> anyhow::Error {
    if is_restarting_error(&e) {
        anyhow!("{e:#} (el cliente se está reiniciando; probá de nuevo en unos segundos)")
    } else {
        e
    }
}

/// DLL candidato encontrado en disco.
#[derive(Debug, Clone)]
pub struct DllCandidate {
    pub path: PathBuf,
    pub arch: Option<&'static str>,
}

/// Busca `CtApi.dll` (configurado o en las instalaciones conocidas), los compatibles primero.
pub fn find_dlls(cfg: &Config) -> Vec<DllCandidate> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(p) = &cfg.ctapi_path {
        if p.is_file() {
            paths.push(p.clone());
        } else {
            for sub in ["CtApi.dll", "Bin (x64)\\CtApi.dll", "Bin\\Bin (x64)\\CtApi.dll", "Bin\\CtApi.dll"] {
                let c = p.join(sub);
                if c.is_file() {
                    paths.push(c);
                }
            }
        }
    }
    if paths.is_empty() {
        for root in discover::install_roots() {
            paths.extend(discover::find_files(&root, 5, 200_000, &|n| n == "ctapi.dll"));
        }
    }
    let mut out: Vec<DllCandidate> = paths.into_iter().map(|p| DllCandidate { arch: discover::pe_arch(&p), path: p }).collect();
    let me = discover::this_arch();
    out.sort_by_key(|c| {
        let s = c.path.to_string_lossy().to_lowercase();
        (
            c.arch != Some(me),
            !s.contains("power operation"),
            // Versiones más nuevas primero (v2024 > v2022...).
            std::cmp::Reverse(s),
        )
    });
    out
}

impl CtApiLib {
    /// Carga el DLL y resuelve las funciones.
    ///
    /// # Safety
    /// Ejecuta el código de inicialización de CtApi.dll.
    unsafe fn load(path: &Path) -> Result<Self> {
        let dir = path.parent().context("Ruta de CtApi.dll sin carpeta")?;
        // Las dependencias (CtUtil, CtEng, ensamblados .NET...) están junto al DLL: se agrega su
        // carpeta al PATH del proceso, como hace el SDK oficial con ctSetManagedBinDirectory.
        let current = std::env::var_os("PATH").unwrap_or_default();
        let already = std::env::split_paths(&current).any(|p| p == dir);
        if !already {
            let joined = std::env::join_paths(std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&current)))?;
            // SAFETY: se ejecuta con el mutex de CtAPI tomado; ningún otro código del proceso modifica PATH.
            unsafe { std::env::set_var("PATH", joined) };
        }
        let lib = unsafe {
            libloading::os::windows::Library::load_with_flags(path, libloading::os::windows::LOAD_WITH_ALTERED_SEARCH_PATH)
        }
        .map_err(|e| {
            let hint = match discover::pe_arch(path) {
                Some(a) if a != discover::this_arch() => format!(
                    " — el DLL es {a} y este exe es {}: usá la versión {a} del MCP o el CtApi.dll de 'Bin (x64)'",
                    discover::this_arch()
                ),
                _ => " — ¿faltan dependencias junto al DLL (CtUtil, CtEng, CiDebugHelp, ensamblados .NET)?".into(),
            };
            anyhow!("No se pudo cargar {}: {e}{hint}", path.display())
        })?;
        let lib: libloading::Library = lib.into();
        unsafe {
            macro_rules! sym {
                ($t:ty, $name:literal) => {
                    *lib.get::<$t>(concat!($name, "\0").as_bytes()).with_context(|| format!("CtApi.dll no exporta {}", $name))?
                };
            }
            macro_rules! opt_sym {
                ($t:ty, $name:literal) => {
                    lib.get::<$t>(concat!($name, "\0").as_bytes()).ok().map(|s| *s)
                };
            }
            if let Some(set_bin) = opt_sym!(FnSetManagedBin, "ctSetManagedBinDirectory") {
                let d = cstring(&dir.to_string_lossy())?;
                set_bin(d.as_ptr());
            }
            let find_first = opt_sym!(FnFindFirst, "ctFindFirst");
            let find_first_ex = opt_sym!(FnFindFirstEx, "ctFindFirstEx");
            if find_first.is_none() && find_first_ex.is_none() {
                bail!("CtApi.dll no exporta ctFindFirst ni ctFindFirstEx");
            }
            let list = (|| {
                Some(ListFns {
                    new: opt_sym!(FnListNew, "ctListNew")?,
                    free: opt_sym!(FnListFree, "ctListFree")?,
                    add: opt_sym!(FnListAdd, "ctListAdd")?,
                    read: opt_sym!(FnListRead, "ctListRead")?,
                    data: opt_sym!(FnListData, "ctListData")?,
                    item: opt_sym!(FnListItem, "ctListItem")?,
                })
            })();
            Ok(Self {
                path: path.to_path_buf(),
                open: sym!(FnOpen, "ctOpen"),
                close: sym!(FnClose, "ctClose"),
                tag_read: sym!(FnTagRead, "ctTagRead"),
                tag_write: sym!(FnTagWrite, "ctTagWrite"),
                cicode: sym!(FnCicode, "ctCicode"),
                find_first,
                find_first_ex,
                find_next: sym!(FnFindNext, "ctFindNext"),
                find_close: sym!(FnFindClose, "ctFindClose"),
                get_property: sym!(FnGetProperty, "ctGetProperty"),
                tag_read_ex: opt_sym!(FnTagReadEx, "ctTagReadEx"),
                tag_get_property: opt_sym!(FnTagGetProperty, "ctTagGetProperty"),
                list,
                _lib: lib,
            })
        }
    }
}

/// Conexión abierta: funciones del DLL + handle de ctOpen.
pub struct Session<'a> {
    lib: &'a CtApiLib,
    h: Handle,
}

/// Resultado de una búsqueda ctFindFirst/ctFindNext.
pub struct FindResult {
    pub rows: Vec<Value>,
    pub truncated: bool,
    /// Motivo cuando ctFindFirst no devuelve nada (sin coincidencias o error).
    pub note: Option<String>,
}

impl Session<'_> {
    pub fn tag_read(&self, tag: &str) -> Result<String> {
        let t = cstring(tag)?;
        let mut buf = vec![0u8; VALUE_BUF];
        let ok = unsafe { (self.lib.tag_read)(self.h, t.as_ptr(), buf.as_mut_ptr().cast(), buf.len() as u32) };
        if ok == 0 {
            return Err(fail(&format!("ctTagRead('{tag}')")));
        }
        Ok(decode_cstr(&buf))
    }

    /// Valor, calidad y horas de un tag (ctTagReadEx). Si el DLL no tiene ctTagReadEx: solo el valor (ctTagRead).
    pub fn tag_read_ex(&self, tag: &str) -> Result<TagReading> {
        let Some(f) = self.lib.tag_read_ex else {
            return self.tag_read(tag).map(|value| TagReading { value, ..Default::default() });
        };
        let t = cstring(tag)?;
        let mut buf = vec![0u8; VALUE_BUF];
        let mut items = TagValueItems { dw_length: size_of::<TagValueItems>() as u32, ..Default::default() };
        let ok = unsafe { f(self.h, t.as_ptr(), buf.as_mut_ptr().cast(), buf.len() as u32, &mut items) };
        if ok == 0 {
            return Err(fail(&format!("ctTagReadEx('{tag}')")));
        }
        // Con un tag inexistente ctTagReadEx no falla: devuelve TRUE, valor vacío y BAD/INVALID_TAG (PO 2024,
        // 2026-11-27). ctTagRead sí da el error real (424 "Tag not found"), que es lo que se muestra.
        if items.quality_extended == QUAL_EXT_INVALID_TAG
            && let Err(e) = self.tag_read(tag)
        {
            return Err(e);
        }
        Ok(TagReading::from_items(decode_cstr(&buf), &items))
    }

    /// Lee varios tags. Con dos o más y las funciones de listas en el DLL, en un solo pedido (ctListNew/ctListAdd/
    /// ctListRead: todos los valores del mismo instante); un tag que la lista no devuelve completo (valor y calidad)
    /// se relee con ctTagReadEx, que da el error real. Sin listas: ctTagReadEx tag por tag.
    pub fn read_many(&self, tags: &[String]) -> Vec<Result<TagReading>> {
        if tags.len() >= 2
            && let Some(list) = self.list_read(tags)
        {
            let fallback = list.iter().filter(|r| r.is_none()).count();
            tracing::debug!(tags = tags.len(), fallback, "lectura por lista (ctListRead)");
            return list.into_iter().zip(tags).map(|(r, t)| r.map_or_else(|| self.tag_read_ex(t), Ok)).collect();
        }
        tags.iter().map(|t| self.tag_read_ex(t)).collect()
    }

    /// Una lectura por lista. `None` si la lista no se pudo crear; por tag, `None` si no dio valor y calidad.
    fn list_read(&self, tags: &[String]) -> Option<Vec<Option<TagReading>>> {
        let l = self.lib.list?;
        let hl = unsafe { (l.new)(self.h, 0) };
        if hl.is_null() {
            tracing::debug!("ctListNew falló: {}", describe_error(last_error()));
            return None;
        }
        let handles: Vec<Handle> = tags
            .iter()
            .map(|t| match cstring(t) {
                Ok(c) if !t.trim().is_empty() => unsafe { (l.add)(hl, c.as_ptr()) },
                _ => null_mut(),
            })
            .collect();
        // ctListRead puede dar FALSE si falla algún tag aunque los demás tengan datos: se mira cada uno.
        if unsafe { (l.read)(hl, null_mut()) } == 0 {
            tracing::debug!("ctListRead devolvió FALSE: {}", describe_error(last_error()));
        }
        let out = handles.iter().map(|&h| (!h.is_null()).then(|| list_tag(&l, h)).flatten()).collect();
        // ctListFree libera también los tags agregados.
        unsafe { (l.free)(hl) };
        Some(out)
    }

    /// Propiedad de un tag por ctTagGetProperty ("Units", "Description", "EngUnitsLow"...), como texto.
    pub fn tag_property(&self, tag: &str, prop: &str) -> Option<String> {
        let f = self.lib.tag_get_property?;
        let (t, p) = (cstring(tag).ok()?, cstring(prop).ok()?);
        let mut buf = vec![0u8; VALUE_BUF];
        let ok = unsafe { f(self.h, t.as_ptr(), p.as_ptr(), buf.as_mut_ptr().cast(), buf.len() as u32, DBTYPE_STR) };
        (ok != 0).then(|| decode_cstr(&buf))
    }

    pub fn tag_write(&self, tag: &str, value: &str) -> Result<()> {
        let (t, v) = (cstring(tag)?, cstring(value)?);
        let ok = unsafe { (self.lib.tag_write)(self.h, t.as_ptr(), v.as_ptr()) };
        if ok == 0 {
            return Err(fail(&format!("ctTagWrite('{tag}')")));
        }
        Ok(())
    }

    pub fn cicode(&self, cmd: &str, window: u32) -> Result<String> {
        let c = cstring(cmd)?;
        let mut buf = vec![0u8; CICODE_BUF];
        let ok = unsafe { (self.lib.cicode)(self.h, c.as_ptr(), window, 0, buf.as_mut_ptr().cast(), buf.len() as u32, null_mut()) };
        if ok == 0 {
            return Err(fail(&format!("ctCicode('{cmd}')")));
        }
        Ok(decode_cstr(&buf))
    }

    /// Proyecto que corre en el runtime: ProjectInfo(0) (nombre del proyecto compilado; en PO 2024
    /// ProjectCurrentGet() da error Citect 292), ProjectCurrentGet() en versiones anteriores y, si no,
    /// el `[CtEdit] Run` del proceso (ParameterGet), reducido al nombre de la carpeta.
    pub fn running_project(&self) -> Option<String> {
        let clean = |s: String| {
            let s = s.trim().trim_end_matches(['\\', '/']).to_string();
            let name = s.rsplit(['\\', '/']).next().unwrap_or("").trim().to_string();
            (!name.is_empty()).then_some(name)
        };
        ["ProjectInfo(0)", "ProjectCurrentGet()", r#"ParameterGet("CtEdit", "Run", "")"#]
            .iter()
            .find_map(|cmd| self.cicode(cmd, 0).ok().and_then(clean))
    }

    fn get_property(&self, obj: Handle, name: &str) -> Option<String> {
        let n = cstring(name).ok()?;
        let mut buf = vec![0u8; VALUE_BUF];
        // El largo devuelto es DWORD en la doc y size_t en algunos wrappers: se reserva 8 bytes.
        let mut len: u64 = 0;
        let ok = unsafe {
            (self.lib.get_property)(obj, n.as_ptr(), buf.as_mut_ptr().cast(), buf.len() as u32, (&mut len as *mut u64).cast(), DBTYPE_STR)
        };
        (ok != 0).then(|| decode_cstr(&buf))
    }

    /// Recorre una tabla/consulta de CtAPI ("Tag", "Alarm", "Trend", "TRNQUERY,...", etc.).
    pub fn find(&self, table: &str, filter: &str, cluster: Option<&str>, props: &[String], limit: usize) -> Result<FindResult> {
        let t = cstring(table)?;
        let f = cstring(filter)?;
        let c = cstring(cluster.unwrap_or(""))?;
        let fptr = if filter.is_empty() { null() } else { f.as_ptr() };
        let cptr = if cluster.unwrap_or("").is_empty() { null() } else { c.as_ptr() };
        let mut obj: Handle = null_mut();
        let hfind = unsafe {
            match (self.lib.find_first_ex, self.lib.find_first) {
                (Some(fx), _) => fx(self.h, t.as_ptr(), fptr, cptr, &mut obj, 0),
                (None, Some(ff)) => ff(self.h, t.as_ptr(), fptr, &mut obj, 0),
                (None, None) => unreachable!(),
            }
        };
        if hfind.is_null() {
            let code = last_error();
            return Ok(FindResult {
                rows: vec![],
                truncated: false,
                note: Some(format!("ctFindFirst no devolvió registros ({}). Puede ser que no haya coincidencias o que la tabla/filtro no sean válidos.", describe_error(code))),
            });
        }
        let mut rows = Vec::new();
        let mut truncated = false;
        loop {
            if rows.len() >= limit {
                truncated = true;
                break;
            }
            let mut row = Map::new();
            for p in props {
                row.insert(p.clone(), self.get_property(obj, p).map(Value::String).unwrap_or(Value::Null));
            }
            rows.push(Value::Object(row));
            if unsafe { (self.lib.find_next)(hfind, &mut obj) } == 0 {
                break;
            }
        }
        unsafe { (self.lib.find_close)(hfind) };
        Ok(FindResult { rows, truncated, note: None })
    }
}

struct State {
    lib: Option<Arc<CtApiLib>>,
    /// Handle de ctOpen como entero (los punteros crudos no son Send).
    handle: usize,
}

impl State {
    fn ensure(&mut self, cfg: &Config) -> Result<(Arc<CtApiLib>, Handle)> {
        if self.lib.is_none() {
            let candidates = find_dlls(cfg);
            let first = candidates.first().ok_or_else(|| {
                anyhow!(
                    "No se encontró CtApi.dll. ¿Está instalado Power Operation en este equipo? \
                     Indicá la ruta con POWEROP_CTAPI (archivo o carpeta, normalmente '...\\Power Operation\\v20xx\\Bin\\Bin (x64)')."
                )
            })?;
            self.lib = Some(Arc::new(unsafe { CtApiLib::load(&first.path)? }));
        }
        let lib = self.lib.clone().expect("cargado arriba");
        if self.handle == 0 {
            let host = cfg.host.as_deref().map(cstring).transpose()?;
            let user = cfg.user.as_deref().map(cstring).transpose()?;
            let pass = cfg.password.as_deref().map(cstring).transpose()?;
            let p = |c: &Option<std::ffi::CString>| c.as_ref().map_or(null(), |s| s.as_ptr());
            let h = unsafe { (lib.open)(p(&host), p(&user), p(&pass), CT_OPEN_RECONNECT) };
            if h.is_null() {
                let code = last_error();
                return Err(ct_error(
                    code,
                    format!(
                        "ctOpen falló: {}. ¿Está corriendo el runtime de Power Operation{}? \
                         Para conexión remota hace falta [CtAPI] Remote=1 en el citect.ini del servidor y POWEROP_USER/POWEROP_PASSWORD.",
                        describe_error(code),
                        cfg.host.as_deref().map(|h| format!(" en {h}")).unwrap_or_else(|| " en este equipo".into())
                    ),
                ));
            }
            self.handle = h as usize;
        }
        Ok((lib, self.handle as Handle))
    }

    fn close(&mut self) {
        if let (Some(lib), true) = (&self.lib, self.handle != 0) {
            unsafe { (lib.close)(self.handle as Handle) };
        }
        self.handle = 0;
    }
}

#[derive(Clone)]
pub struct CtApi {
    cfg: Arc<Config>,
    state: Arc<Mutex<State>>,
}

impl CtApi {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self { cfg, state: Arc::new(Mutex::new(State { lib: None, handle: 0 })) }
    }

    /// Ejecuta `job` en un hilo bloqueante, con el mutex de CtAPI tomado y tiempo máximo.
    async fn run<T, F>(&self, job: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut State, &Config) -> Result<T> + Send + 'static,
    {
        let state = self.state.clone();
        let cfg = self.cfg.clone();
        let task = tokio::task::spawn_blocking(move || {
            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
            job(&mut *st, cfg.as_ref())
        });
        match tokio::time::timeout(self.cfg.timeout, task).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => bail!("La llamada a CtAPI terminó con error interno: {e}"),
            Err(_) => bail!(
                "Tiempo agotado ({} s) esperando a CtAPI. El runtime puede estar ocupado o sin responder (POWEROP_TIMEOUT_SECS).",
                self.cfg.timeout.as_secs()
            ),
        }
    }

    /// Ejecuta `f` con una sesión abierta (carga el DLL y conecta si hace falta).
    pub async fn call<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Session) -> Result<T> + Send + 'static,
    {
        self.run(move |st, cfg| {
            let (lib, h) = st.ensure(cfg)?;
            f(&Session { lib: &lib, h })
        })
        .await
    }

    /// Como `call`, pero si falla con 233 (el cliente del runtime se está reiniciando) cierra la conexión,
    /// espera un momento, reconecta y reintenta UNA vez. Si vuelve a fallar, el error indica que se reintente.
    /// `f` puede ejecutarse dos veces: usarla solo para lecturas (o para escrituras que no cambian nada si se repiten).
    pub async fn call_retry<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: Fn(&Session) -> Result<T> + Send + Sync + 'static,
    {
        let f = Arc::new(f);
        let this = self;
        with_restart_retry(this, RESTART_PAUSE, move || {
            let f = f.clone();
            this.call(move |s| (*f)(s))
        })
        .await
    }

    /// Solo abre la conexión (ctOpen), con el mismo reintento ante 233 que `call_retry`. No ejecuta nada en el
    /// runtime: sirve antes de una escritura, que nunca se reintenta.
    pub async fn connect_retry(&self) -> Result<()> {
        let this = self;
        with_restart_retry(this, RESTART_PAUSE, move || this.run(|st, cfg| st.ensure(cfg).map(|_| ()))).await
    }

    /// Cierra la conexión actual (la próxima llamada vuelve a conectar).
    pub async fn disconnect(&self) {
        let state = self.state.clone();
        let _ = tokio::task::spawn_blocking(move || state.lock().unwrap_or_else(|e| e.into_inner()).close()).await;
    }

    /// DLL cargado, si lo hay.
    pub fn loaded_dll(&self) -> Option<PathBuf> {
        self.state.try_lock().ok().and_then(|s| s.lib.as_ref().map(|l| l.path.clone()))
    }
}

/// Ejecuta `attempt`; si falla con 233 cierra la conexión, espera `pause` y lo intenta una vez más.
async fn with_restart_retry<T, Fut, G>(api: &CtApi, pause: std::time::Duration, mut attempt: G) -> Result<T>
where
    Fut: std::future::Future<Output = Result<T>>,
    G: FnMut() -> Fut,
{
    match attempt().await {
        Err(e) if is_restarting_error(&e) => {
            api.disconnect().await;
            tokio::time::sleep(pause).await;
            attempt().await.map_err(restart_hint)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api() -> CtApi {
        CtApi::new(Arc::new(crate::testutil::config(&std::env::temp_dir())))
    }

    #[test]
    fn detecta_233_por_valor() {
        // Citect 233 tal como lo arma `fail`/`ct_error`.
        let citect = ct_error(ERROR_USER_DEFINED_BASE + 233, format!("ctTagRead('X') falló: {}", describe_error(ERROR_USER_DEFINED_BASE + 233)));
        assert!(is_restarting_error(&citect));
        assert!(citect.to_string().contains("error Citect 233"));
        // Windows 233 (pipe no conectado): mismo síntoma según reference.md.
        assert!(is_restarting_error(&ct_error(233, "x".into())));
        // Otros códigos Citect (424 = tag inexistente, 292, 274) no son transitorios.
        assert!(!is_restarting_error(&ct_error(ERROR_USER_DEFINED_BASE + 424, "x".into())));
        assert!(!is_restarting_error(&ct_error(ERROR_USER_DEFINED_BASE + 274, "x".into())));
        // El texto solo no cuenta: sin código no se reintenta.
        assert!(!is_restarting_error(&anyhow!("error Citect 233")));
    }

    #[test]
    fn layout_de_ct_tagvalue_items() {
        // ctapi.h: pack(1) → 4 + 3×8 + 4×1 + 4 + 1 + 1.
        assert_eq!(size_of::<TagValueItems>(), 38);
        assert_eq!(std::mem::offset_of!(TagValueItems, quality_general), 28);
        assert_eq!(std::mem::offset_of!(TagValueItems, datasource_error), 32);
        assert_eq!(std::mem::offset_of!(TagValueItems, control_inhibit), 37);
    }

    #[test]
    fn numeros_de_ctlistitem() {
        assert_eq!(parse_list_num(" 3 "), Some(3));
        assert_eq!(parse_list_num("0x1A"), Some(26));
        assert_eq!(parse_list_num("TRUE"), Some(1));
        assert_eq!(parse_list_num("2026-11-27 10:00"), None);
    }

    #[test]
    fn nombres_de_calidad() {
        assert_eq!(quality_general_name(3), Some("GOOD"));
        assert_eq!(quality_substatus_name(0, 6), Some("COMM_FAILURE"));
        assert_eq!(quality_substatus_name(1, 6), Some("SUBNORMAL"));
        assert_eq!(quality_substatus_name(3, 6), Some("LOCAL_OVERRIDE"));
        assert_eq!(quality_extended_name(0x14), Some("NOT_REPLICATED"));
        assert_eq!(quality_extended_name(0x15), None);
        assert_eq!(quality_limit_name(2), Some("HIGH"));
    }

    #[test]
    fn errores_citect_con_titulo() {
        assert_eq!(describe_error(ERROR_USER_DEFINED_BASE + 424), "error Citect 424 \"Tag not found\" (GetLastError 0x100001A8)");
        assert!(describe_error(ERROR_USER_DEFINED_BASE + 8).contains("dispositivo de I/O"));
        assert_eq!(describe_error(ERROR_USER_DEFINED_BASE + 233), "error Citect 233 (GetLastError 0x100000E9)");
    }

    #[test]
    fn mensaje_de_reinicio() {
        let e = restart_hint(ct_error(ERROR_USER_DEFINED_BASE + 233, "ctOpen falló".into()));
        assert!(format!("{e:#}").ends_with("(el cliente se está reiniciando; probá de nuevo en unos segundos)"));
        let otro = restart_hint(ct_error(ERROR_USER_DEFINED_BASE + 424, "ctTagRead falló".into()));
        assert_eq!(format!("{otro:#}"), "ctTagRead falló");
    }

    #[tokio::test]
    async fn reintenta_una_vez_tras_233() {
        let api = api();
        let mut intentos = 0;
        let r: Result<u32> = with_restart_retry(&api, std::time::Duration::ZERO, || {
            intentos += 1;
            let n = intentos;
            async move { if n == 1 { Err(ct_error(ERROR_USER_DEFINED_BASE + 233, "x".into())) } else { Ok(n) } }
        })
        .await;
        assert_eq!(r.unwrap(), 2);
        assert_eq!(intentos, 2);
    }

    #[tokio::test]
    async fn no_reintenta_otros_errores_y_avisa_si_233_persiste() {
        let api = api();
        let mut intentos = 0;
        let r: Result<u32> = with_restart_retry(&api, std::time::Duration::ZERO, || {
            intentos += 1;
            async { Err(ct_error(ERROR_USER_DEFINED_BASE + 424, "tag no existe".into())) }
        })
        .await;
        assert_eq!(intentos, 1);
        assert_eq!(format!("{:#}", r.unwrap_err()), "tag no existe");

        let mut intentos = 0;
        let r: Result<u32> = with_restart_retry(&api, std::time::Duration::ZERO, || {
            intentos += 1;
            async { Err(ct_error(ERROR_USER_DEFINED_BASE + 233, "ctOpen falló".into())) }
        })
        .await;
        assert_eq!(intentos, 2, "debe reintentar una sola vez");
        assert!(format!("{:#}", r.unwrap_err()).contains("el cliente se está reiniciando"));
    }
}
