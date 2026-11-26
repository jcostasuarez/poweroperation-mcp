//! Procesos del runtime de Power Operation (Citect32.exe y RuntimeManager.exe): PID, rol, hora de arranque,
//! si responden y su ventana, más los errores recientes de los logs de Runtime Manager. Solo lectura, sin CtAPI.
//!
//! Rol de cada Citect32, en este orden (ver `inspect`):
//! 1. `log`: la línea `Runtime (32 bit) (<Rol> [<cluster>.<servidor>]) (PID n) Startup` de `Logs\syslog*.dat` con el
//!    mismo PID y una hora de arranque a 60 s. El cliente no tiene corchetes: `Runtime (32 bit)  (PID n) Startup`.
//! 2. `linea_de_comando`: una sola palabra de rol en el CommandLine (wmic; solo si hace falta). No verificado con
//!    procesos reales: al momento de escribirlo no había ningún Citect32 corriendo.
//! 3. `titulo`: la palabra "Client" en el título de la ventana principal (los títulos de páginas no se usan).
//! Si nada coincide, el rol es "desconocido".

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, FixedOffset, Local, NaiveDateTime, TimeZone};
use serde_json::{Value, json};
use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HWND, INVALID_HANDLE_VALUE, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject,
};
use windows_sys::Win32::Storage::Xps::PrintWindow;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsHungAppWindow, IsIconic, IsWindowVisible, PostMessageW,
    SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_CLOSE, WM_NULL,
};

use crate::config::Config;
use crate::discover;
use crate::text::truncate;

/// Procesos que forman el runtime (nombres en minúsculas).
const EXES: &[&str] = &["citect32.exe", "runtimemanager.exe"];
/// Tiempo máximo que se espera a que una ventana responda, y cuántas ventanas se revisan.
const RESPONSE_MS: u32 = 200;
const WINDOWS_CHECKED: usize = 3;
/// Errores de Runtime Manager devueltos como máximo.
const PROBLEMS_MAX: usize = 15;
/// PW_RENDERFULLCONTENT: captura también el contenido de ventanas compuestas (DWM), aunque estén tapadas.
const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;
const FMT: &str = "%Y-%m-%d %H:%M:%S";

/// Ventana visible de nivel superior de un proceso del runtime.
#[derive(Debug, Clone)]
pub struct Win {
    pub hwnd: isize,
    pub title: String,
    pub minimized: bool,
}

/// Proceso del runtime con su rol y su estado.
#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: u32,
    pub name: String,
    pub path: Option<String>,
    pub started: Option<DateTime<Local>>,
    pub role: String,
    pub role_source: &'static str,
    /// Ventanas visibles; la principal (con título) va primero.
    pub windows: Vec<Win>,
    /// None si el proceso no tiene ventanas visibles.
    pub responds: Option<bool>,
}

/// Foto del runtime: procesos, carpeta de logs, errores recientes y fallos parciales.
#[derive(Debug, Default)]
pub struct Inspection {
    pub procs: Vec<Proc>,
    pub logs_dir: Option<PathBuf>,
    pub problems: Vec<Value>,
    pub errors: Vec<String>,
}

/// Ventana del cliente de Power Operation (Citect32 con rol Client).
#[derive(Debug, Clone)]
pub struct ClientWin {
    pub pid: u32,
    pub hwnd: isize,
    pub title: String,
    pub minimized: bool,
    pub responds: Option<bool>,
}

/// Línea de arranque de un proceso del runtime (syslog).
struct Startup {
    pid: u32,
    role: String,
    at: DateTime<Local>,
}

/// Estado completo para `po_runtime_procs` y `po_status`. Nunca falla: los problemas quedan en `errors`.
pub fn runtime_report(cfg: &Config) -> Value {
    let ins = inspect(cfg);
    json!({
        "checked_at": Local::now().format(FMT).to_string(),
        "runtime_running": ins.procs.iter().any(|p| p.name.eq_ignore_ascii_case("citect32.exe")),
        "processes": ins.procs.iter().map(proc_json).collect::<Vec<_>>(),
        "logs_dir": ins.logs_dir.as_ref().map(|p| p.display().to_string()),
        "problems": ins.problems,
        "errors": ins.errors,
    })
}

fn proc_json(p: &Proc) -> Value {
    let first = p.windows.first();
    let main_title = first.filter(|w| !w.title.is_empty()).map(|w| w.title.clone());
    let mut v = json!({
        "pid": p.pid,
        "process": p.name,
        "path": p.path,
        "role": p.role,
        "role_source": p.role_source,
        "started": p.started.map(|t| t.format(FMT).to_string()),
        "responds": p.responds,
        "window_title": main_title,
        "minimized": first.is_some_and(|w| w.minimized),
    });
    if p.role == "Client" {
        v["page_hint"] = json!(main_title_of(p));
    }
    if p.started.is_none() {
        v["error"] = json!("no se pudo leer la hora de arranque (permisos o proceso terminado)");
    }
    v
}

fn main_title_of(p: &Proc) -> Option<String> {
    p.windows.first().filter(|w| !w.title.is_empty()).map(|w| w.title.clone())
}

/// Procesos del runtime con rol, ventanas, logs recientes y errores parciales.
pub fn inspect(cfg: &Config) -> Inspection {
    let mut ins = Inspection::default();
    let logs = logs_dir(cfg);
    let startups = match &logs {
        Some(dir) => startup_records(dir),
        None => {
            ins.errors.push("No encontré la carpeta Logs de Power Operation: los roles no salen del log".into());
            vec![]
        }
    };
    ins.logs_dir = logs.clone();

    let list = snapshot().unwrap_or_else(|e| {
        ins.errors.push(format!("Lista de procesos: {e:#}"));
        vec![]
    });
    let windows = all_windows();
    for (pid, name) in list.into_iter().filter(|(_, n)| EXES.contains(&n.to_ascii_lowercase().as_str())) {
        let (started, path) = process_details(pid);
        let mut mine: Vec<Win> = windows.iter().filter(|(p, _)| *p == pid).map(|(_, w)| w.clone()).collect();
        // Stable: primero las ventanas con título.
        mine.sort_by_key(|w| w.title.is_empty());
        let responds = (!mine.is_empty()).then(|| mine.iter().take(WINDOWS_CHECKED).all(|w| window_responds(w.hwnd)));
        let (role, role_source) = if name.eq_ignore_ascii_case("runtimemanager.exe") {
            ("RuntimeManager".to_string(), "nombre")
        } else if let Some(r) = role_from_log(&startups, pid, started) {
            (r, "log")
        } else {
            ("desconocido".to_string(), "pendiente")
        };
        ins.procs.push(Proc { pid, name, path, started, role, role_source, windows: mine, responds });
    }

    if ins.procs.iter().any(|p| p.role_source == "pendiente") {
        let cmds = command_lines().unwrap_or_else(|e| {
            ins.errors.push(format!("Línea de comandos (wmic): {e:#}"));
            BTreeMap::new()
        });
        for p in ins.procs.iter_mut().filter(|p| p.role_source == "pendiente") {
            if let Some(r) = cmds.get(&p.pid).and_then(|c| cef_role(c)) {
                p.role = r;
                p.role_source = "linea_de_comando";
            } else if let Some(r) = cmds.get(&p.pid).and_then(|c| role_from_words(c)) {
                p.role = r.into();
                p.role_source = "linea_de_comando";
            } else if main_title_of(p).is_some_and(|t| has_word(&t, "client")) {
                p.role = "Client".into();
                p.role_source = "titulo";
            } else {
                p.role_source = "desconocido";
            }
        }
    }

    if let Some(dir) = &logs {
        ins.problems = recent_problems(dir);
    }
    ins
}

/// Ventana principal del cliente. Error si no hay cliente, si no tiene ventana visible o si no es posible capturarlo.
pub fn client_window(cfg: &Config) -> Result<ClientWin> {
    let ins = inspect(cfg);
    let Some(p) = ins.procs.iter().find(|p| p.name.eq_ignore_ascii_case("citect32.exe") && p.role == "Client") else {
        let roles: Vec<String> = ins.procs.iter().filter(|p| p.name.eq_ignore_ascii_case("citect32.exe")).map(|p| format!("PID {} = {}", p.pid, p.role)).collect();
        bail!(
            "No hay cliente de Power Operation (Citect32 con rol Client) en ejecución. Procesos Citect32: {}",
            if roles.is_empty() { "ninguno (el runtime no está arrancado)".to_string() } else { roles.join(", ") }
        );
    };
    let Some(w) = p.windows.first() else {
        bail!("El cliente (PID {}) no tiene ventana visible: no hay nada que capturar", p.pid);
    };
    Ok(ClientWin { pid: p.pid, hwnd: w.hwnd, title: w.title.clone(), minimized: w.minimized, responds: p.responds })
}

/// Títulos de las ventanas visibles de RuntimeManager.exe (p. ej. el diálogo "Setup required").
pub fn manager_window_titles() -> Vec<String> {
    let Ok(list) = snapshot() else { return vec![] };
    let pids: Vec<u32> = list.into_iter().filter(|(_, n)| n.eq_ignore_ascii_case("runtimemanager.exe")).map(|(p, _)| p).collect();
    all_windows().into_iter().filter(|(p, w)| pids.contains(p) && !w.title.is_empty()).map(|(_, w)| w.title).collect()
}

/// Explicación cuando Runtime Manager no arranca el runtime porque espera al usuario en un diálogo
/// (verificado: "Setup required" = el equipo quedó configurado para otro proyecto; pide el Computer Setup Wizard).
pub fn manager_block_hint(titles: &[String]) -> Option<String> {
    if titles.iter().any(|t| t.to_lowercase().contains("setup required")) {
        return Some(
            "Runtime Manager muestra el diálogo 'Setup required': este equipo está configurado para otro proyecto y pide \
             correr el Computer Setup Wizard. Lo tiene que hacer el usuario (Yes en el diálogo y completar el asistente) o \
             desactivar ese chequeo en las opciones de Studio; el runtime arranca al terminar el asistente."
                .into(),
        );
    }
    let open: Vec<&str> = titles.iter().map(String::as_str).filter(|t| !t.eq_ignore_ascii_case("Runtime Manager")).collect();
    (!open.is_empty()).then(|| format!("Runtime Manager tiene abierta(s) la(s) ventana(s) {open:?}: puede estar esperando al usuario."))
}

/// PIDs de los Citect32.exe en ejecución.
pub fn citect_pids() -> Result<Vec<u32>> {
    Ok(snapshot()?.into_iter().filter(|(_, n)| n.eq_ignore_ascii_case("citect32.exe")).map(|(p, _)| p).collect())
}

/// Lanza un programa de interfaz (Studio, Citect32) SIN heredar handles: con `std::process::Command` el hijo hereda
/// los pipes de stdin/stdout del MCP y, como Studio sigue vivo, el cliente nunca ve el EOF al cerrar el servidor
/// (visto el 2026-10-16). Intenta salir del job del cliente (CREATE_BREAKAWAY_FROM_JOB) y, si no se puede, lanza
/// igual dentro. Devuelve el PID.
pub fn spawn_detached(exe: &Path) -> Result<u32> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW,
    };
    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let app = wide(exe.as_os_str());
    let dir = wide(exe.parent().unwrap_or_else(|| Path::new(".")).as_os_str());
    // Línea de comandos: solo el exe entre comillas (CreateProcessW puede escribir en el buffer).
    let mut cmd = wide(std::ffi::OsStr::new(&format!("\"{}\"", exe.display())));
    for flags in [CREATE_BREAKAWAY_FROM_JOB | CREATE_NEW_PROCESS_GROUP, CREATE_NEW_PROCESS_GROUP] {
        // SAFETY: buffers locales terminados en 0; estructuras inicializadas con su tamaño; handles cerrados.
        unsafe {
            let mut si: STARTUPINFOW = std::mem::zeroed();
            si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            let ok = CreateProcessW(
                app.as_ptr(),
                cmd.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0, // bInheritHandles = FALSE
                flags,
                std::ptr::null(),
                dir.as_ptr(),
                &si,
                &mut pi,
            );
            if ok != 0 {
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
                return Ok(pi.dwProcessId);
            }
        }
    }
    bail!("no se pudo ejecutar {}: {}", exe.display(), std::io::Error::last_os_error())
}

/// PIDs de los procesos con ese nombre de exe (sin distinguir mayúsculas).
pub fn pids_of(exe: &str) -> Result<Vec<u32>> {
    Ok(snapshot()?.into_iter().filter(|(_, n)| n.eq_ignore_ascii_case(exe)).map(|(p, _)| p).collect())
}

/// Ventanas visibles de nivel superior de un proceso.
pub fn windows_of(pid: u32) -> Vec<Win> {
    all_windows().into_iter().filter(|(p, _)| *p == pid).map(|(_, w)| w).collect()
}

/// Envía WM_CLOSE a las ventanas visibles de esos procesos. Devuelve cuántas ventanas recibieron el mensaje.
pub fn post_close(pids: &[u32]) -> usize {
    let mut n = 0;
    for (pid, w) in all_windows() {
        if pids.contains(&pid) {
            // SAFETY: mensaje asíncrono a una ventana de un proceso del runtime; no espera respuesta.
            if unsafe { PostMessageW(w.hwnd as HWND, WM_CLOSE, 0, 0) } != 0 {
                n += 1;
            }
        }
    }
    n
}

/// Termina un proceso (TerminateProcess). Solo se usa con confirmación explícita (force=true).
pub fn terminate(pid: u32) -> Result<()> {
    // SAFETY: handle propio con permiso de terminar; se cierra en todos los caminos.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h.is_null() {
            bail!("No se pudo abrir el proceso {pid} para terminarlo (permisos)");
        }
        let ok = TerminateProcess(h, 1);
        CloseHandle(h);
        if ok == 0 {
            bail!("TerminateProcess falló para el PID {pid}");
        }
    }
    Ok(())
}

/// Captura una ventana como PNG (contenido completo aunque esté tapada). Devuelve (png, ancho, alto).
pub fn capture_png(hwnd: isize, max_width: u32) -> Result<(Vec<u8>, usize, usize)> {
    let h = hwnd as HWND;
    // SAFETY: GDI sobre DCs y bitmap creados aquí; se liberan en todos los caminos. El bitmap se deselecciona
    // antes de GetDIBits, como exige la API.
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        if GetWindowRect(h, &mut r) == 0 {
            bail!("GetWindowRect falló");
        }
        let (w, hi) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || hi <= 0 {
            bail!("La ventana no tiene tamaño (¿minimizada?)");
        }
        let (w, hi) = (w as usize, hi as usize);
        let screen = GetDC(std::ptr::null_mut());
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w as i32, hi as i32);
        let old = SelectObject(mem, bmp);
        let printed = PrintWindow(h, mem, PW_RENDERFULLCONTENT);
        SelectObject(mem, old);

        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w as i32;
        bi.bmiHeader.biHeight = -(hi as i32); // negativo: filas de arriba abajo
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB;
        let mut px = vec![0u8; w * hi * 4];
        let rows = GetDIBits(mem, bmp, 0, hi as u32, px.as_mut_ptr().cast(), &mut bi, DIB_RGB_COLORS);

        DeleteObject(bmp);
        DeleteDC(mem);
        ReleaseDC(std::ptr::null_mut(), screen);
        if printed == 0 {
            bail!("PrintWindow no pudo capturar la ventana");
        }
        if rows == 0 {
            bail!("GetDIBits falló");
        }
        let (px, nw, nh) = if w > max_width as usize { scale(&px, w, hi, max_width as usize) } else { (px, w, hi) };
        let bmp_file = bmp_bytes(&px, nw, nh);
        let png = crate::png::bmp_to_png(&bmp_file).ok_or_else(|| anyhow!("No se pudo convertir la captura a PNG"))?;
        Ok((png, nw, nh))
    }
}

/// Reduce BGRA por promedio de bloques. Devuelve (píxeles, ancho, alto).
fn scale(px: &[u8], w: usize, h: usize, nw: usize) -> (Vec<u8>, usize, usize) {
    let nh = (h * nw / w).max(1);
    let mut out = vec![0u8; nw * nh * 4];
    for y in 0..nh {
        let y0 = y * h / nh;
        let y1 = ((y + 1) * h / nh).max(y0 + 1).min(h);
        for x in 0..nw {
            let x0 = x * w / nw;
            let x1 = ((x + 1) * w / nw).max(x0 + 1).min(w);
            let mut acc = [0u32; 4];
            let mut n = 0u32;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let i = (sy * w + sx) * 4;
                    for (c, a) in acc.iter_mut().enumerate() {
                        *a += px[i + c] as u32;
                    }
                    n += 1;
                }
            }
            let o = (y * nw + x) * 4;
            for c in 0..4 {
                out[o + c] = (acc[c] / n) as u8;
            }
        }
    }
    (out, nw, nh)
}

/// Archivo BMP de 32 bits (cabecera + BGRA) con alto negativo (de arriba abajo), para `png::bmp_to_png`.
fn bmp_bytes(px: &[u8], w: usize, h: usize) -> Vec<u8> {
    let data_off = 54u32;
    let mut b = Vec::with_capacity(data_off as usize + px.len());
    b.extend_from_slice(b"BM");
    b.extend_from_slice(&(data_off + px.len() as u32).to_le_bytes());
    b.extend_from_slice(&[0; 4]);
    b.extend_from_slice(&data_off.to_le_bytes());
    b.extend_from_slice(&40u32.to_le_bytes());
    b.extend_from_slice(&(w as i32).to_le_bytes());
    b.extend_from_slice(&(-(h as i32)).to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&32u16.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    b.extend_from_slice(&(px.len() as u32).to_le_bytes());
    b.extend_from_slice(&[0; 16]);
    b.extend_from_slice(px);
    b
}

// ---------------------------------------------------------------------------------------------------------------
// Procesos y ventanas (Win32)

/// Procesos del sistema: (PID, nombre de exe).
fn snapshot() -> Result<Vec<(u32, String)>> {
    let mut out = Vec::new();
    // SAFETY: snapshot de procesos; la estructura se inicializa con su tamaño.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE || snap.is_null() {
            bail!("CreateToolhelp32Snapshot falló");
        }
        let mut pe: PROCESSENTRY32W = std::mem::zeroed();
        pe.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut pe);
        while ok != 0 {
            let len = pe.szExeFile.iter().position(|&c| c == 0).unwrap_or(pe.szExeFile.len());
            out.push((pe.th32ProcessID, String::from_utf16_lossy(&pe.szExeFile[..len])));
            ok = Process32NextW(snap, &mut pe);
        }
        CloseHandle(snap);
    }
    Ok(out)
}

/// Hora de arranque (local) y ruta del ejecutable de un proceso, si se puede abrir.
fn process_details(pid: u32) -> (Option<DateTime<Local>>, Option<String>) {
    // SAFETY: handle propio, cerrado antes de salir; los buffers son locales con su tamaño.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return (None, None);
        }
        let mut created: FILETIME = std::mem::zeroed();
        let mut exited: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        let started = if GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user) != 0 { filetime_local(created) } else { None };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let path = if QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) != 0 {
            Some(String::from_utf16_lossy(&buf[..len as usize]))
        } else {
            None
        };
        CloseHandle(h);
        (started, path)
    }
}

/// FILETIME (UTC, desde 1601) a hora local.
fn filetime_local(ft: FILETIME) -> Option<DateTime<Local>> {
    let t = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    let unix100ns = t.checked_sub(116_444_736_000_000_000)?;
    let secs = (unix100ns / 10_000_000) as i64;
    let nanos = ((unix100ns % 10_000_000) * 100) as u32;
    DateTime::from_timestamp(secs, nanos).map(|u| u.with_timezone(&Local))
}

unsafe extern "system" fn collect_windows(h: HWND, l: LPARAM) -> BOOL {
    // SAFETY: `l` apunta al Vec de `all_windows`, vivo durante EnumWindows.
    let out = unsafe { &mut *(l as *mut Vec<(u32, Win)>) };
    // SAFETY: consultas de solo lectura sobre una ventana de la enumeración.
    unsafe {
        if IsWindowVisible(h) != 0 {
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, &mut pid);
            let mut buf = [0u16; 512];
            let n = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
            out.push((pid, Win { hwnd: h as isize, title: String::from_utf16_lossy(&buf[..n]), minimized: IsIconic(h) != 0 }));
        }
    }
    1
}

/// Ventanas visibles de nivel superior con su PID.
fn all_windows() -> Vec<(u32, Win)> {
    let mut out: Vec<(u32, Win)> = Vec::new();
    // SAFETY: callback sincrónico; `out` vive durante la llamada.
    unsafe { EnumWindows(Some(collect_windows), &mut out as *mut _ as LPARAM) };
    out
}

/// Una ventana responde si no está marcada como colgada y contesta un mensaje nulo dentro de RESPONSE_MS.
fn window_responds(hwnd: isize) -> bool {
    let h = hwnd as HWND;
    // SAFETY: USER32 sobre un HWND de la enumeración; el timeout acota la espera.
    unsafe {
        if IsHungAppWindow(h) != 0 {
            return false;
        }
        let mut res: usize = 0;
        SendMessageTimeoutW(h, WM_NULL, 0, 0, SMTO_ABORTIFHUNG, RESPONSE_MS, &mut res) != 0
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Roles

/// Subproceso del navegador embebido (CEF) del cliente: Citect32 con `--type=gpu-process`, `--type=utility`...
/// (verificado: el cliente lanza dos unos 15 s después de arrancar). No son roles del runtime.
pub fn cef_role(cmd: &str) -> Option<String> {
    let ty = cmd.split_whitespace().find_map(|t| t.strip_prefix("--type="))?;
    Some(format!("Client-CEF ({})", ty.trim_matches('"')))
}

/// Rol que nombra una línea de comandos: una sola palabra de rol (si hay dos distintas, no se decide).
pub fn role_from_words(text: &str) -> Option<&'static str> {
    let mut found: Option<&'static str> = None;
    for tok in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        let lower = tok.to_ascii_lowercase();
        let base = lower.trim_end_matches(|c: char| c.is_ascii_digit());
        let role = match base {
            "ioserver" => "IOServer",
            "alarmserver" | "alarm" => "Alarm",
            "trendserver" | "trend" => "Trend",
            "reportserver" | "report" => "Report",
            "client" => "Client",
            _ => continue,
        };
        match found {
            None => found = Some(role),
            Some(r) if r == role => {}
            Some(_) => return None,
        }
    }
    found
}

/// Palabra exacta (sin distinguir mayúsculas) dentro de un texto.
fn has_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric()).any(|t| t.eq_ignore_ascii_case(word))
}

/// Rol del log de arranque del mismo PID, si la hora coincide (±60 s).
fn role_from_log(startups: &[Startup], pid: u32, started: Option<DateTime<Local>>) -> Option<String> {
    let started = started?;
    startups
        .iter()
        .filter(|s| s.pid == pid)
        .map(|s| ((s.at - started).num_seconds().abs(), s))
        .filter(|(d, _)| *d <= 60)
        .min_by_key(|(d, _)| *d)
        .map(|(_, s)| s.role.clone())
}

/// Línea `Runtime (32 bit) (Alarm [c1.AlarmServer1]) (PID 456) Startup ...` del syslog.
/// El cliente no tiene rol entre paréntesis: se devuelve "Client".
pub fn parse_startup(line: &str) -> Option<(u32, String, DateTime<Local>)> {
    if !line.contains(" Startup") {
        return None;
    }
    let pid_at = line.find("(PID ")?;
    let pid: u32 = line[pid_at + 5..].split(')').next()?.trim().parse().ok()?;
    let head = &line[..pid_at];
    let rt = head.find("Runtime (")?;
    let after = head[rt + "Runtime (".len()..].split_once(')')?.1.trim();
    let role = if after.is_empty() {
        "Client".to_string()
    } else {
        let inner = after.strip_prefix('(')?.strip_suffix(')')?;
        inner.split([' ', '[']).next()?.trim().to_string()
    };
    if role.is_empty() {
        return None;
    }
    Some((pid, role, parse_log_time(line)?))
}

/// Hora de una línea de log `2026-10-14 22:40:46.587\t-03:00\t...` (la zona del log se respeta).
pub fn parse_log_time(line: &str) -> Option<DateTime<Local>> {
    let naive = NaiveDateTime::parse_from_str(line.get(..23)?, "%Y-%m-%d %H:%M:%S%.3f").ok()?;
    let tz = line.get(23..).and_then(|rest| rest.split('\t').nth(1)).and_then(parse_offset);
    match tz {
        Some(off) => off.from_local_datetime(&naive).single().map(|d| d.with_timezone(&Local)),
        None => Local.from_local_datetime(&naive).single(),
    }
}

/// `-03:00` / `+05:30` a desfase fijo.
fn parse_offset(s: &str) -> Option<FixedOffset> {
    let s = s.trim();
    let sign = match s.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (h, m) = s[1..].split_once(':')?;
    let secs = sign * (h.parse::<i32>().ok()? * 3600 + m.parse::<i32>().ok()? * 60);
    FixedOffset::east_opt(secs)
}

// ---------------------------------------------------------------------------------------------------------------
// Logs

/// Carpeta `Logs` de Power Operation: junto a las carpetas de proyecto configuradas o bajo ProgramData.
/// Se elige la que tiene el log más reciente de Runtime Manager.
pub fn logs_dir(cfg: &Config) -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    for d in &cfg.projects_dirs {
        cands.extend(d.ancestors().skip(1).take(3).map(|a| a.join("Logs")));
    }
    for root in discover::data_roots() {
        for e in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            if !e.file_name().to_string_lossy().to_ascii_lowercase().starts_with("power operation") {
                continue;
            }
            for v in std::fs::read_dir(e.path()).into_iter().flatten().flatten() {
                cands.push(v.path().join("Logs"));
            }
        }
    }
    cands.retain(|p| p.is_dir());
    cands.into_iter().max_by_key(|p| modified(&p.join("tracelog.RuntimeManager.BackEnd.dat")))
}

fn modified(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok()?.modified().ok()
}

struct LogFile {
    path: PathBuf,
    name: String,
    modified: SystemTime,
}

/// Archivos de `dir` cuyo nombre en minúsculas cumple `want`, del más reciente al más viejo.
fn recent_files(dir: &Path, want: impl Fn(&str) -> bool, max: usize) -> Vec<LogFile> {
    let mut v: Vec<LogFile> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !want(&name.to_ascii_lowercase()) {
                return None;
            }
            let md = e.metadata().ok()?;
            if !md.is_file() {
                return None;
            }
            Some(LogFile { path: e.path(), name, modified: md.modified().ok()? })
        })
        .collect();
    v.sort_by(|a, b| b.modified.cmp(&a.modified));
    v.truncate(max);
    v
}

/// Últimos `max` bytes de un log como texto, sin la línea cortada al inicio. Los archivos grandes no se leen enteros.
fn tail_text(path: &Path, max: u64) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    if start > 0 {
        let cut = buf.iter().position(|&b| b == b'\n').map(|i| i + 1).unwrap_or(buf.len());
        buf.drain(..cut);
    }
    Ok(crate::text::decode(&buf))
}

/// Líneas de arranque (`syslog*.dat`) de los procesos del runtime.
fn startup_records(logs: &Path) -> Vec<Startup> {
    let mut out = Vec::new();
    for f in recent_files(logs, |n| n.starts_with("syslog") && n.ends_with(".dat"), 12) {
        if let Ok(text) = tail_text(&f.path, 256 * 1024) {
            out.extend(text.lines().filter_map(parse_startup).map(|(pid, role, at)| Startup { pid, role, at }));
        }
    }
    out
}

/// Error o advertencia de una línea de log: (hora, nivel, mensaje). `file_lower` es el nombre del archivo en minúsculas.
/// `tracelog.*` tiene nivel en la 5.ª columna (tabulada); los syslog y debug se buscan por palabra clave.
pub fn parse_problem(file_lower: &str, line: &str) -> Option<(DateTime<Local>, &'static str, String)> {
    let at = parse_log_time(line)?;
    let (level, msg) = if file_lower.starts_with("tracelog.") {
        // fecha, zona, hilo, contador, nivel, componente, (vacío), mensaje
        let f: Vec<&str> = line.splitn(8, '\t').collect();
        let level = match f.get(4)?.trim() {
            "Error" => "Error",
            "Warning" => "Warning",
            _ => return None,
        };
        (level, f.get(7)?.trim().to_string())
    } else {
        // "error count: 0" (resumen de sincronización de alarmas) no es un error.
        let lower = line.to_ascii_lowercase().replace("error count: 0", "");
        let level = if lower.contains("error") || lower.contains("not responding") || lower.contains("exception") {
            "Error"
        } else if lower.contains("warning") {
            "Warning"
        } else {
            return None;
        };
        (level, line.splitn(3, '\t').nth(2)?.trim().to_string())
    };
    Some((at, level, msg))
}

/// Últimos errores y advertencias de los logs de Runtime Manager y de los procesos del runtime.
fn recent_problems(logs: &Path) -> Vec<Value> {
    let mut found: Vec<(DateTime<Local>, Value)> = Vec::new();
    let want = |n: &str| (n.starts_with("tracelog.") || n.starts_with("syslog.") || n.starts_with("debug.")) && (n.ends_with(".dat") || n.ends_with(".log"));
    for f in recent_files(logs, want, 12) {
        let Ok(text) = tail_text(&f.path, 192 * 1024) else { continue };
        for line in text.lines() {
            if let Some((at, level, msg)) = parse_problem(&f.name.to_ascii_lowercase(), line) {
                found.push((
                    at,
                    json!({
                        "time": at.format(FMT).to_string(),
                        "level": level,
                        "file": f.name,
                        "file_modified": DateTime::<Local>::from(f.modified).format(FMT).to_string(),
                        "message": truncate(&msg, 300),
                    }),
                ));
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().take(PROBLEMS_MAX).map(|(_, v)| v).collect()
}

// ---------------------------------------------------------------------------------------------------------------
// Línea de comandos (wmic)

/// CommandLine de Citect32.exe y RuntimeManager.exe por PID. wmic tarda poco; se acota a 3 s.
fn command_lines() -> Result<BTreeMap<u32, String>> {
    let mut child = Command::new("wmic")
        .args(["process", "where", "name='Citect32.exe' or name='RuntimeManager.exe'", "get", "CommandLine,ProcessId", "/format:csv"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    // Se lee en un hilo para que el proceso no quede bloqueado si la salida llena el pipe.
    let mut out = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(o) = out.as_mut() {
            let _ = o.read_to_end(&mut b);
        }
        b
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("wmic no respondió en 3 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let raw = reader.join().unwrap_or_default();
    Ok(parse_wmic_csv(&decode_wmic(&raw)))
}

/// wmic escribe UTF-16 al redirigir; si no, ANSI/UTF-8.
fn decode_wmic(raw: &[u8]) -> String {
    let utf16 = raw.starts_with(&[0xFF, 0xFE]) || raw.get(1) == Some(&0);
    if utf16 {
        let body = if raw.starts_with(&[0xFF, 0xFE]) { &raw[2..] } else { raw };
        let units: Vec<u16> = body.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&units);
    }
    crate::text::decode(raw)
}

/// Salida `/format:csv` (`Node,CommandLine,ProcessId`). La línea de comandos puede tener comas: el PID es el último campo.
pub fn parse_wmic_csv(text: &str) -> BTreeMap<u32, String> {
    let mut out = BTreeMap::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with("Node,") {
            continue;
        }
        let Some((_, rest)) = line.split_once(',') else { continue };
        let Some((cmd, pid)) = rest.rsplit_once(',') else { continue };
        if let Ok(pid) = pid.trim().parse::<u32>() {
            out.insert(pid, cmd.trim().to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_hint_explains_setup_wizard() {
        let h = manager_block_hint(&["Setup required".into()]).unwrap();
        assert!(h.contains("Computer Setup Wizard"), "{h}");
        assert!(manager_block_hint(&[]).is_none());
        assert!(manager_block_hint(&["Runtime Manager".into()]).is_none());
        assert!(manager_block_hint(&["Error".into()]).unwrap().contains("puede estar esperando al usuario"));
    }

    #[test]
    fn startup_roles_and_client() {
        let line = "2026-10-14 22:40:46.587\t-03:00\tRuntime (32 bit) (Report [c1.ReportServer1]) (PID 15616) Startup 8.50.0.368";
        let (pid, role, at) = parse_startup(line).unwrap();
        assert_eq!((pid, role.as_str()), (15616, "Report"));
        assert_eq!(Some(at), parse_log_time(line));
        let (pid, role, _) = parse_startup("2026-10-14 22:40:54.631\t-03:00\tRuntime (32 bit)  (PID 9608) Startup 8.50.0.368").unwrap();
        assert_eq!((pid, role.as_str()), (9608, "Client"));
        let (_, role, _) = parse_startup("2026-10-14 22:40:50.606\t-03:00\tRuntime (32 bit) (Alarm [c1.AlarmServer1]) (PID 456) Startup 8.50.0.368").unwrap();
        assert_eq!(role, "Alarm");
        let (_, role, _) = parse_startup("2026-10-14 22:40:46.587\t-03:00\tRuntime (32 bit) (IOServer [c1.IOServer1]) (PID 10292) Startup").unwrap();
        assert_eq!(role, "IOServer");
        // Líneas que no son de arranque.
        assert!(parse_startup("2026-10-14 22:50:38.841\t-03:00\t***    SCADA process stopped     ***").is_none());
        assert!(parse_startup("2026-10-14 22:40:50.423\t-03:00\tAlarm: Synchronizing alarm state").is_none());
        // Con " Startup" pero sin "(PID n)": cae en el primer guard.
        assert!(parse_startup("2026-10-14 22:40:46.587\t-03:00\tRuntime (32 bit) (Alarm [c1.AlarmServer1]) Startup 8.50.0.368").is_none());
        // Con "(PID n)" pero sin "Runtime (": cae en el segundo guard.
        assert!(parse_startup("2026-10-14 22:40:46.587\t-03:00\tAlarm (PID 456) Startup 8.50.0.368").is_none());
    }

    #[test]
    fn log_time_respects_offset() {
        // Mismo instante en -03:00 y en UTC.
        let a = parse_log_time("2026-10-14 22:40:46.587\t-03:00\tx").unwrap();
        let b = parse_log_time("2026-10-15 01:40:46.587\t+00:00\tx").unwrap();
        assert_eq!(a, b);
        assert!(parse_log_time("no es fecha").is_none());
        assert_eq!(parse_offset("-03:00").map(|o| o.local_minus_utc()), Some(-3 * 3600));
        assert_eq!(parse_offset("+05:30").map(|o| o.local_minus_utc()), Some(5 * 3600 + 1800));
        assert!(parse_offset("UTC").is_none());
    }

    #[test]
    fn problems_tracelog_and_syslog() {
        let hang = "2026-10-14 22:25:52.152\t-03:00\t16180\t  0\tError\tRuntimeManager\t\tHangMonitor::timerElapsed() PID=11284:C:Client; State=Not Responding";
        let (_, level, msg) = parse_problem("tracelog.runtimemanager.backend.dat", hang).unwrap();
        assert_eq!(level, "Error");
        assert!(msg.contains("State=Not Responding"));
        let info = "2026-10-14 22:25:52.152\t-03:00\t16180\t  0\tInfo\tRuntimeManager\t\tcosa normal";
        assert!(parse_problem("tracelog.runtimemanager.backend.dat", info).is_none());
        let sys = "2026-10-14 22:45:48.703\t-03:00\tKerMain: Task ran for more than 1 sec";
        assert!(parse_problem("syslog.dat", sys).is_none());
        let sys_err = "2026-10-14 22:45:48.703\t-03:00\tDatabase error: timeout";
        assert_eq!(parse_problem("syslog.dat", sys_err).unwrap().1, "Error");
        let sync = "2026-10-14 22:40:50.411\t-03:00\tAlarm: Database objects synchronization completed (synchronized: 16, error count: 0).";
        assert!(parse_problem("syslog.alarm.c1.alarmserver1.dat", sync).is_none());
    }

    #[test]
    fn role_words() {
        assert_eq!(role_from_words(r#""C:\Program Files (x86)\Schneider Electric\Power Operation\v2024\bin\Citect32.exe" -sc1.AlarmServer1"#), Some("Alarm"));
        assert_eq!(role_from_words("Citect32.exe -rClient"), None); // "rClient" no es palabra de rol: no se adivina
        assert_eq!(role_from_words("Citect32.exe client"), Some("Client"));
        assert_eq!(role_from_words("Citect32.exe Alarm Trend"), None); // dos roles: no se decide
        assert_eq!(role_from_words("Citect32.exe C:\\Users\\Usuario\\Desktop\\prueba\\Transferencia_Demo"), None);
        // Líneas reales de PO 2024 (Runtime Manager).
        assert_eq!(role_from_words(r#""C:\Program Files (x86)\Schneider Electric\Power Operation\v2024\Bin\Citect32.exe"  /R[C:Client] /C:0,1"#), Some("Client"));
        assert_eq!(role_from_words(r#""C:\Program Files (x86)\Schneider Electric\Power Operation\v2024\Bin\Citect32.exe"  /R[A:c1.AlarmServer1] /C:0,1"#), Some("Alarm"));
        assert_eq!(role_from_words(r#""...\Bin\Citect32.exe"  /R[I:c1.IOServer1] /C:0,1"#), Some("IOServer"));
        let gpu = r#""C:\Program Files (x86)\Schneider Electric\Power Operation\v2024\Bin\Citect32.exe" --type=gpu-process --log-severity=warning --user-data-dir="C:\Users\Usuario\AppData\Local\AVEVA Plant SCADA\WebContent""#;
        assert_eq!(cef_role(gpu).as_deref(), Some("Client-CEF (gpu-process)"));
        assert!(cef_role(r#""...\Citect32.exe"  /R[C:Client] /C:0,1"#).is_none());
        assert!(has_word("Unifilar - Client", "client"));
        assert!(!has_word("Clientes", "client"));
    }

    #[test]
    fn wmic_csv_with_commas_in_command_line() {
        let text = "\r\nNode,CommandLine,ProcessId\r\nPC,\"C:\\x\\Citect32.exe\" -a,b,1234\r\nPC,C:\\RuntimeManager.exe,99\r\n\r\n";
        let m = parse_wmic_csv(text);
        assert_eq!(m.get(&1234).map(String::as_str), Some("\"C:\\x\\Citect32.exe\" -a,b"));
        assert_eq!(m.get(&99).map(String::as_str), Some("C:\\RuntimeManager.exe"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn bmp_builds_png_through_existing_converter() {
        let px = vec![0u8, 0, 255, 0, 0, 255, 0, 0, 0, 0, 0, 0, 255, 255, 255, 0];
        let bmp = bmp_bytes(&px, 2, 2);
        let png = crate::png::bmp_to_png(&bmp).expect("BMP 32 bits de arriba abajo");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        // IHDR (tras la firma y la longitud + tipo): ancho y alto big-endian, iguales a los de la captura 2x2.
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 2);
    }

    #[test]
    fn scale_averages_blocks() {
        // 2x1 blanco y negro -> 1x1 gris medio.
        let px = vec![255, 255, 255, 0, 0, 0, 0, 0];
        let (out, w, h) = scale(&px, 2, 1, 1);
        assert_eq!((w, h), (1, 1));
        assert_eq!(&out[..3], &[127, 127, 127]);
    }
}
