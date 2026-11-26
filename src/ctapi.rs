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
    // Mantiene el DLL cargado mientras existan los punteros de arriba.
    _lib: libloading::Library,
}

/// Código de GetLastError() inmediatamente después de una llamada fallida.
fn last_error() -> u32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32
}

pub fn describe_error(code: u32) -> String {
    if code == 0 {
        "sin código de error".into()
    } else if code >= ERROR_USER_DEFINED_BASE {
        format!("error Citect {} (GetLastError 0x{code:08X})", code - ERROR_USER_DEFINED_BASE)
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
