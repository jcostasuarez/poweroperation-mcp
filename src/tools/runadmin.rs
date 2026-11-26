//! Estado, captura y control del runtime de Power Operation sin CtAPI: procesos Citect32 (`procs.rs`), captura de la
//! ventana del cliente, y arranque/reinicio del runtime. `po_runtime_control` es escritura (confirmación del usuario
//! + auditoría). Reinicio con force verificado el 2026-10-15 (WM_CLOSE solo no cierra el cliente); arranque sin verificar.

use std::path::{Path, PathBuf};

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use chrono::Local;
use encoding_rs::WINDOWS_1252;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::config::Config;
use crate::discover::{self, CitectIni};
use crate::procs;
use crate::server::{PowerOpServer, ToolResult, blocking, err};

/// Ancho por defecto de la captura del cliente (el PNG se guarda sin comprimir: más píxeles, más bytes).
const DEFAULT_SHOT_WIDTH: u32 = 900;
/// Espera máxima a que aparezcan los Citect32 tras arrancar.
const WAIT_START: Duration = Duration::from_secs(60);
/// Tiempo sin cambios en la lista de procesos para darla por estable.
const STABLE_FOR: Duration = Duration::from_secs(10);
/// Espera a que salgan los Citect32 después de TerminateProcess.
const TERMINATE_WAIT: Duration = Duration::from_secs(45);
/// Espera a que Runtime Manager relance el runtime por su cuenta antes de arrancarlo el MCP.
const RELAUNCH_WAIT: Duration = Duration::from_secs(20);
/// Proyectos de sistema y librerías que no se ejecutan desde el MCP.
const PROTECTED: &[&str] = &["System", "Include", "PLS_Include", "PSO_Equipment", "IndustrialGraphics_Include"];

fn pretty(v: &Value, max: usize) -> ToolResult {
    let s = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    Ok(crate::text::truncate(&s, max))
}

#[derive(Deserialize, JsonSchema)]
pub struct ScreenshotParams {
    /// Ancho máximo de la imagen en píxeles (200 a 2000; la altura se ajusta). Por defecto 900.
    #[serde(default)]
    pub max_width: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ControlParams {
    /// "start" = arrancar (si ya corre, no hace nada) o "restart" = cerrar y volver a arrancar.
    pub action: String,
    /// Carpeta del proyecto a ejecutar (p. ej. `C:\ProgramData\...\User\MCP_Demo`). Si se indica, cambia
    /// `[CtEdit] Run=` del citect.ini antes de arrancar (requiere confirm_ini_change). Sin él se usa el Run= actual.
    #[serde(default)]
    pub project_path: Option<String>,
    /// true = el usuario confirmó que se cambie Run= del citect.ini (se hace respaldo `citect.ini.<fecha>.bak`).
    #[serde(default)]
    pub confirm_ini_change: bool,
    /// Solo restart: true = si tras el cierre ordenado siguen procesos, los termina (TerminateProcess). Sin él no
    /// mata nada. En la práctica hace falta: el cliente ignora WM_CLOSE (visto el 2026-10-15).
    #[serde(default)]
    pub force: bool,
    /// Solo restart: segundos de espera al cierre ordenado (5 a 120; por defecto 30).
    #[serde(default)]
    pub wait_secs: Option<u64>,
}

#[tool_router(router = runadmin_router, vis = "pub(crate)")]
impl PowerOpServer {
    #[tool(name = "po_runtime_procs", description = "Estado del runtime sin CtAPI (solo lectura): procesos Citect32.exe y RuntimeManager.exe con PID, rol (IOServer/Alarm/Trend/Report/Client; role_source dice de dónde sale), hora de arranque, si responden, título de la ventana (page_hint en el cliente) y los últimos errores y advertencias de los logs de Runtime Manager (p. ej. HangMonitor ... State=Not Responding). Sirve para ver si el cliente se colgó.")]
    async fn runtime_procs(&self) -> ToolResult {
        let cfg = self.cfg.clone();
        let v = blocking(Duration::from_secs(10), move || Ok(procs::runtime_report(&cfg))).await.map_err(err)?;
        pretty(&v, 60_000)
    }

    #[tool(name = "po_runtime_screenshot", description = "Captura SOLO la ventana del cliente de Power Operation (Citect32 con rol Client), aunque otra ventana la tape. Devuelve la imagen PNG. Si la ventana está minimizada o el cliente no responde, lo dice y no captura. Solo lectura. max_width reduce la imagen.")]
    async fn runtime_screenshot(&self, Parameters(p): Parameters<ScreenshotParams>) -> Result<CallToolResult, String> {
        let max_w = p.max_width.unwrap_or(DEFAULT_SHOT_WIDTH).clamp(200, 2000);
        let cfg = self.cfg.clone();
        let (png, w, h, title) = blocking(Duration::from_secs(20), move || {
            let c = procs::client_window(&cfg)?;
            if c.minimized {
                bail!("La ventana del cliente (PID {}) está minimizada: restaurala en el escritorio y volvé a pedir la captura. No se capturó nada.", c.pid);
            }
            if c.responds == Some(false) {
                bail!("El cliente (PID {}) no responde: no se captura para no bloquear el MCP. Mirá po_runtime_procs.", c.pid);
            }
            let (png, w, h) = procs::capture_png(c.hwnd, max_w)?;
            Ok((png, w, h, c.title))
        })
        .await
        .map_err(err)?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        Ok(CallToolResult::success(vec![
            ContentBlock::text(format!("Captura del cliente de Power Operation ({w}x{h} px, ventana '{title}', {} bytes PNG).", png.len())),
            ContentBlock::image(b64, "image/png"),
        ]))
    }

    #[tool(name = "po_runtime_control", description = "CONFIRMÁ CON EL USUARIO antes. Arranca (action=start) o reinicia (action=restart) el runtime de Power Operation. start: ejecuta Citect32.exe SIN argumentos desde la carpeta Bin (igual que el acceso 'Power Operation Runtime'), que lee [CtEdit] Run= del citect.ini; si ya hay Citect32 corriendo no hace nada; espera hasta ~60 s y devuelve el estado de los procesos. restart: cierre ordenado (WM_CLOSE y espera de wait_secs) y, con force=true, TerminateProcess de los que sigan (el cliente ignora WM_CLOSE: en la práctica hace falta force); espera a que salgan y, si Runtime Manager relanza el runtime solo, no arranca otro. Con project_path cambia Run= antes (exige confirm_ini_change=true; respaldo citect.ini.<fecha>.bak). Shutdown() por po_cicode_run no sirve (devolvió 274). Requiere POWEROP_ALLOW_WRITE=1.")]
    async fn runtime_control(&self, Parameters(p): Parameters<ControlParams>) -> ToolResult {
        self.require_write("po_runtime_control").map_err(err)?;
        let action = p.action.trim().to_ascii_lowercase();
        let cfg = self.cfg.clone();
        let (detail, res) = match action.as_str() {
            "start" => {
                let detail = format!("start project_path={}", p.project_path.as_deref().unwrap_or("(Run= actual)"));
                let res = blocking(Duration::from_secs(150), move || start_runtime(&cfg, p.project_path.as_deref(), p.confirm_ini_change)).await;
                (detail, res)
            }
            "restart" => {
                let detail = format!("restart project_path={} force={}", p.project_path.as_deref().unwrap_or("(Run= actual)"), p.force);
                let wait = Duration::from_secs(p.wait_secs.unwrap_or(30).clamp(5, 120));
                let res = blocking(Duration::from_secs(300), move || {
                    restart_runtime(&cfg, p.project_path.as_deref(), p.confirm_ini_change, p.force, wait)
                })
                .await;
                (detail, res)
            }
            o => return Err(format!("action '{o}' inválida: usá \"start\" o \"restart\". No se hizo nada.")),
        };
        self.audit("po_runtime_control", &detail, &outcome(&res));
        res.map_err(err)
    }
}

/// Texto de auditoría: primera línea del resultado, o el error.
fn outcome(res: &Result<String>) -> String {
    match res {
        Ok(s) => format!("OK {}", s.lines().next().unwrap_or("")),
        Err(e) => format!("ERROR {e:#}"),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Arranque y reinicio

/// Estado del runtime como texto + JSON (para la respuesta de las herramientas de escritura).
fn with_report(cfg: &Config, head: &str) -> String {
    let report = serde_json::to_string_pretty(&procs::runtime_report(cfg)).unwrap_or_default();
    format!("{head}\n{report}")
}

fn start_runtime(cfg: &Config, project: Option<&str>, confirm: bool) -> Result<String> {
    if let Some(p) = project {
        if !confirm {
            bail!("Cambiar Run= del citect.ini requiere confirm_ini_change=true, el OK explícito del usuario. No se hizo nada.");
        }
        check_project_dir(p)?;
    }
    let running = procs::citect_pids()?;
    if !running.is_empty() {
        if project.is_some() {
            bail!(
                "El runtime ya está corriendo ({} proceso(s) Citect32). Run= solo se aplica al arrancar: usá po_runtime_control con action=restart y project_path.",
                running.len()
            );
        }
        return Ok(with_report(cfg, &format!("Ya hay {} proceso(s) Citect32 corriendo; no se arrancó nada.", running.len())));
    }
    let ini = citect_ini()?;
    let exe = citect_exe(&ini)?;
    let mut head = String::new();
    if let Some(p) = project {
        head.push_str(&set_run(&ini.path, p)?);
        head.push('\n');
    }
    spawn_citect(&exe)?;
    let pids = wait_for_runtime()?;
    head.push_str(&format!(
        "Citect32 arrancado desde {} (Run= en citect.ini: {}). Procesos tras la espera: {}.",
        exe.display(),
        run_value(&ini.path),
        pids.len()
    ));
    push_block_hint(&mut head, &pids);
    Ok(with_report(cfg, &head))
}

fn restart_runtime(cfg: &Config, project: Option<&str>, confirm: bool, force: bool, wait: Duration) -> Result<String> {
    if let Some(p) = project {
        if !confirm {
            bail!("Cambiar Run= del citect.ini requiere confirm_ini_change=true, el OK explícito del usuario. No se hizo nada.");
        }
        check_project_dir(p)?;
    }
    let ini = citect_ini()?;
    let exe = citect_exe(&ini)?;
    let mut lines = Vec::new();
    // Run= antes de cerrar: si Runtime Manager relanza el runtime por su cuenta, usa el proyecto nuevo.
    if let Some(p) = project {
        lines.push(set_run(&ini.path, p)?);
    }
    let (stopped, old) = stop_runtime(wait, force).map_err(|e| {
        if lines.is_empty() { e } else { anyhow!("{e:#}. Run= ya quedó cambiado: {}", lines.join(" ")) }
    })?;
    lines.push(stopped);
    // Runtime Manager suele relanzar el runtime solo (visto el 2026-10-15): esperar a ver PIDs nuevos antes de
    // arrancar otro, para no duplicar.
    let relaunched = wait_new_pids(&old, RELAUNCH_WAIT)?;
    if !relaunched.is_empty() {
        let pids = wait_for_runtime()?;
        lines.push(format!(
            "Runtime Manager relanzó el runtime por su cuenta (PID nuevos {relaunched:?}): no se arrancó otro. Procesos tras la espera: {}.",
            pids.len()
        ));
    } else {
        spawn_citect(&exe)?;
        let pids = wait_for_runtime()?;
        let mut line = format!("Citect32 arrancado desde {} (Run= en citect.ini: {}). Procesos tras la espera: {}.", exe.display(), run_value(&ini.path), pids.len());
        push_block_hint(&mut line, &pids);
        lines.push(line);
    }
    Ok(with_report(cfg, &lines.join("\n")))
}

/// Espera hasta `max` a que aparezca algún Citect32 que no esté en `old`. Devuelve los PID nuevos.
fn wait_new_pids(old: &[u32], max: Duration) -> Result<Vec<u32>> {
    let start = Instant::now();
    loop {
        let new: Vec<u32> = procs::citect_pids()?.into_iter().filter(|p| !old.contains(p)).collect();
        if !new.is_empty() || start.elapsed() >= max {
            return Ok(new);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Cierre ordenado (WM_CLOSE) de los Citect32 y, con `force`, TerminateProcess de los que queden.
/// Devuelve el resumen y los PID que había.
fn stop_runtime(wait: Duration, force: bool) -> Result<(String, Vec<u32>)> {
    let pids = procs::citect_pids()?;
    if pids.is_empty() {
        return Ok(("No había procesos Citect32 corriendo.".into(), pids));
    }
    let posted = procs::post_close(&pids);
    let left = wait_gone(&pids, wait)?;
    if left.is_empty() {
        return Ok((format!("Runtime cerrado en orden: {} proceso(s), WM_CLOSE a {posted} ventana(s).", pids.len()), pids));
    }
    if !force {
        bail!(
            "Tras {} s siguen corriendo {} proceso(s) Citect32 (PID {:?}). WM_CLOSE se envió a {posted} ventana(s): puede haber un diálogo de confirmación abierto. No se terminó nada; con force=true se terminan a la fuerza.",
            wait.as_secs(),
            left.len(),
            left
        );
    }
    for p in &left {
        procs::terminate(*p)?;
    }
    // Citect32 tarda en salir después de TerminateProcess (con 10 s se informó un falso "no bastó" el 2026-10-15).
    let still = wait_gone(&left, TERMINATE_WAIT)?;
    if !still.is_empty() {
        let new: Vec<u32> = procs::citect_pids()?.into_iter().filter(|p| !pids.contains(p)).collect();
        bail!(
            "Se envió TerminateProcess a {:?}, pero tras {} s siguen en la lista de procesos los PID {still:?} (pueden estar terminando). {}No se arrancó otro runtime: mirá po_runtime_procs en unos segundos.",
            left,
            TERMINATE_WAIT.as_secs(),
            if new.is_empty() { String::new() } else { format!("Ya hay Citect32 nuevos (PID {new:?}): Runtime Manager lo está relanzando. ") }
        );
    }
    Ok((
        format!("Runtime detenido: cierre ordenado a {} proceso(s) y {} terminado(s) a la fuerza.", pids.len() - left.len(), left.len()),
        pids,
    ))
}

/// Espera hasta `max` a que salgan los `pids` de la lista de procesos. Devuelve los que siguen.
fn wait_gone(pids: &[u32], max: Duration) -> Result<Vec<u32>> {
    let start = Instant::now();
    loop {
        let left: Vec<u32> = procs::citect_pids()?.into_iter().filter(|p| pids.contains(p)).collect();
        if left.is_empty() || start.elapsed() >= max {
            return Ok(left);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Espera hasta WAIT_START a que aparezcan los Citect32; da por terminado el arranque cuando la lista queda estable.
fn wait_for_runtime() -> Result<Vec<u32>> {
    let start = Instant::now();
    let mut last: Vec<u32> = Vec::new();
    let mut stable = Instant::now();
    loop {
        let mut now = procs::citect_pids()?;
        now.sort_unstable();
        if now != last {
            last = now;
            stable = Instant::now();
        }
        if !last.is_empty() && stable.elapsed() >= STABLE_FOR {
            break;
        }
        // Runtime Manager esperando al usuario (Computer Setup Wizard): no tiene sentido esperar más.
        if last.is_empty() && start.elapsed() >= STABLE_FOR && blocked_by_setup() {
            break;
        }
        if start.elapsed() >= WAIT_START {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(last)
}

fn blocked_by_setup() -> bool {
    procs::manager_window_titles().iter().any(|t| t.to_lowercase().contains("setup required"))
}

/// Si no quedó ningún Citect32, agrega por qué (diálogo de Runtime Manager), si se sabe.
fn push_block_hint(text: &mut String, pids: &[u32]) {
    if pids.is_empty()
        && let Some(h) = procs::manager_block_hint(&procs::manager_window_titles())
    {
        text.push_str("\nEL RUNTIME NO ARRANCÓ: ");
        text.push_str(&h);
    }
}

/// Lanza Citect32.exe sin argumentos, desde su carpeta, sin heredar los pipes del MCP (ver procs::spawn_detached).
fn spawn_citect(exe: &Path) -> Result<()> {
    procs::spawn_detached(exe).map(|_| ())
}

// ---------------------------------------------------------------------------------------------------------------
// citect.ini

/// `citect.ini` de la instalación de Power Operation (`...\v20xx\Config\citect.ini`). Si hay varias, la versión más nueva.
pub(crate) fn citect_ini() -> Result<CitectIni> {
    discover::find_citect_ini()
        .into_iter()
        .filter(|i| {
            let p = i.path.to_string_lossy().to_ascii_lowercase();
            p.contains("power operation") && p.ends_with("\\config\\citect.ini")
        })
        .max_by(|a, b| a.path.cmp(&b.path))
        .ok_or_else(|| anyhow!("No encontré Config\\citect.ini de Power Operation en ProgramData"))
}

/// `Citect32.exe` de la carpeta `Bin` del citect.ini; si no está ahí, se busca en la instalación.
fn citect_exe(ini: &CitectIni) -> Result<PathBuf> {
    if let Some(bin) = ini.get("CtEdit", "Bin") {
        let exe = PathBuf::from(bin).join("Citect32.exe");
        if exe.is_file() {
            return Ok(exe);
        }
    }
    for root in discover::install_roots() {
        if let Some(exe) = discover::find_files(&root, 5, 50_000, &|n| n == "citect32.exe").into_iter().next() {
            return Ok(exe);
        }
    }
    bail!("No encontré Citect32.exe (Bin del citect.ini ni instalación de Power Operation)")
}

/// Valor actual de `[CtEdit] Run=` (para informar).
fn run_value(ini: &Path) -> String {
    crate::discover::parse_ini(ini)
        .and_then(|i| i.get("CtEdit", "Run").map(str::to_string))
        .unwrap_or_else(|| "(sin Run=)".into())
}

/// Carpeta de proyecto válida para Run=: existe, tiene VARIABLE.DBF o INCLUDE.DBF y no es de sistema ni librería.
fn check_project_dir(project: &str) -> Result<PathBuf> {
    let p = project.trim().trim_end_matches(['\\', '/']);
    if p.is_empty() {
        bail!("project_path está vacío");
    }
    let dir = PathBuf::from(p);
    if !dir.is_dir() {
        bail!("project_path no es una carpeta: {p}");
    }
    let name = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if PROTECTED.iter().any(|x| x.eq_ignore_ascii_case(&name)) || name.to_ascii_lowercase().starts_with("library_") {
        bail!("'{name}' es un proyecto de sistema o librería: no se ejecuta desde el MCP");
    }
    let has_db = std::fs::read_dir(&dir)?
        .flatten()
        .any(|e| matches!(e.file_name().to_string_lossy().to_ascii_lowercase().as_str(), "variable.dbf" | "include.dbf"));
    if !has_db {
        bail!("'{p}' no parece un proyecto de Power Operation (no tiene VARIABLE.DBF ni INCLUDE.DBF)");
    }
    Ok(dir)
}

/// Cambia `[CtEdit] Run=` de `ini`: respaldo fechado al lado, escritura atómica y verificación. Devuelve un resumen.
fn set_run(ini: &Path, project: &str) -> Result<String> {
    check_project_dir(project)?;
    let value = project.trim().trim_end_matches(['\\', '/']).to_string();
    let bak = ini_update(ini, &[("CtEdit", "Run", &value)])?;
    Ok(format!("Run= cambiado a {value} (respaldo: {})", bak.display()))
}

/// Respaldo fechado `<ini>.<yyyyMMdd-HHmmss>.bak` al lado de `ini`, verificado contra `raw` (su contenido actual).
pub(crate) fn ini_backup(ini: &Path, raw: &[u8]) -> Result<PathBuf> {
    let name = ini.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "citect.ini".into());
    let stamp = Local::now().format("%Y%m%d-%H%M%S");
    let bak = ini.with_file_name(format!("{name}.{stamp}.bak"));
    if bak.exists() {
        bail!("ya existe el respaldo {}: esperá un segundo y reintentá", bak.display());
    }
    std::fs::copy(ini, &bak).with_context(|| format!("no se pudo hacer el respaldo {}", bak.display()))?;
    if std::fs::read(&bak)? != raw {
        bail!("El respaldo {} no coincide con el original: no se modificó citect.ini", bak.display());
    }
    Ok(bak)
}

/// Cambia varias claves de `ini` (sección, clave, valor): respaldo fechado al lado, escritura atómica y
/// verificación de cada valor. Devuelve la ruta del respaldo.
pub(crate) fn ini_update(ini: &Path, sets: &[(&str, &str, &str)]) -> Result<PathBuf> {
    let raw = std::fs::read(ini).with_context(|| format!("no se pudo leer {}", ini.display()))?;
    let mut new = raw.clone();
    for (sec, key, value) in sets {
        new = ini_set_value(&new, sec, key, value)?;
    }
    let bak = ini_backup(ini, &raw)?;
    let name = ini.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "citect.ini".into());
    let tmp = ini.with_file_name(format!("{name}.mcp-tmp"));
    std::fs::write(&tmp, &new)?;
    if let Err(e) = std::fs::rename(&tmp, ini) {
        let _ = std::fs::remove_file(&tmp);
        bail!("No se pudo reemplazar citect.ini ({e}). Respaldo: {}", bak.display());
    }
    let parsed = crate::discover::parse_ini(ini);
    for (sec, key, value) in sets {
        let now = parsed.as_ref().and_then(|i| i.get(sec, key)).unwrap_or_default();
        if now != *value {
            bail!("Verificación fallida: [{sec}] {key}= es '{now}' y se esperaba '{value}'. Respaldo: {}", bak.display());
        }
    }
    Ok(bak)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum IniEnc {
    Utf16Le,
    Utf8Bom,
    Utf8,
    Ansi,
}

fn ini_encoding(raw: &[u8]) -> Result<IniEnc> {
    if raw.starts_with(&[0xFE, 0xFF]) {
        bail!("citect.ini en UTF-16 big endian no es compatible");
    }
    Ok(if raw.starts_with(&[0xFF, 0xFE]) {
        IniEnc::Utf16Le
    } else if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
        IniEnc::Utf8Bom
    } else if std::str::from_utf8(raw).is_ok() {
        IniEnc::Utf8
    } else {
        IniEnc::Ansi
    })
}

fn ini_decode(enc: IniEnc, raw: &[u8]) -> Result<String> {
    match enc {
        IniEnc::Utf16Le => {
            let body = &raw[2..];
            if body.len() % 2 != 0 {
                bail!("citect.ini UTF-16 con longitud impar");
            }
            let units: Vec<u16> = body.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            String::from_utf16(&units).map_err(|_| anyhow!("citect.ini UTF-16 inválido"))
        }
        IniEnc::Utf8Bom => String::from_utf8(raw[3..].to_vec()).map_err(|_| anyhow!("citect.ini UTF-8 inválido")),
        IniEnc::Utf8 => String::from_utf8(raw.to_vec()).map_err(|_| anyhow!("citect.ini UTF-8 inválido")),
        IniEnc::Ansi => WINDOWS_1252
            .decode_without_bom_handling_and_without_replacement(raw)
            .map(|c| c.into_owned())
            .ok_or_else(|| anyhow!("citect.ini ANSI inválido")),
    }
}

fn ini_encode(enc: IniEnc, text: &str) -> Result<Vec<u8>> {
    Ok(match enc {
        IniEnc::Utf16Le => {
            let mut v = vec![0xFF, 0xFE];
            for u in text.encode_utf16() {
                v.extend_from_slice(&u.to_le_bytes());
            }
            v
        }
        IniEnc::Utf8Bom => {
            let mut v = vec![0xEF, 0xBB, 0xBF];
            v.extend_from_slice(text.as_bytes());
            v
        }
        IniEnc::Utf8 => text.as_bytes().to_vec(),
        IniEnc::Ansi => {
            let (b, _, had_errors) = WINDOWS_1252.encode(text);
            if had_errors {
                bail!("El valor no se puede escribir en la codificación ANSI de citect.ini");
            }
            b.into_owned()
        }
    })
}

/// Cambia el valor de `key` en `[section]` de un ini, dejando el resto de los bytes igual (codificación, CRLF/LF,
/// espacios alrededor del `=`). Si la clave no existe la agrega al final de la sección; si la sección no existe,
/// la agrega al final del archivo. Si la codificación no se puede preservar exactamente, falla sin devolver nada.
pub(crate) fn ini_set_value(raw: &[u8], section: &str, key: &str, value: &str) -> Result<Vec<u8>> {
    if value.contains(['\r', '\n']) {
        bail!("El valor no puede tener saltos de línea");
    }
    let enc = ini_encoding(raw)?;
    let text = ini_decode(enc, raw)?;
    if ini_encode(enc, &text)? != raw {
        bail!("No puedo preservar la codificación de citect.ini: no se modificó nada");
    }
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.split_inclusive('\n').map(String::from).collect();

    let sec = section.to_ascii_lowercase();
    let key_l = key.to_ascii_lowercase();
    let mut in_sec = false;
    let mut last_in_sec: Option<usize> = None;
    let mut key_at: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            in_sec = t[1..t.len() - 1].trim().to_ascii_lowercase() == sec;
            if in_sec {
                last_in_sec = Some(i);
            }
            continue;
        }
        if !in_sec || t.is_empty() || t.starts_with(';') {
            continue;
        }
        last_in_sec = Some(i);
        if key_at.is_none() && t.split_once('=').is_some_and(|(k, _)| k.trim().to_ascii_lowercase() == key_l) {
            key_at = Some(i);
        }
    }

    if let Some(i) = key_at {
        let line = lines[i].clone();
        let body = line.trim_end_matches(['\r', '\n']);
        let term = &line[body.len()..];
        let eq = body.find('=').context("clave sin '='")?;
        let after = &body[eq + 1..];
        let ws = after.len() - after.trim_start_matches([' ', '\t']).len();
        lines[i] = format!("{}{value}{term}", &body[..eq + 1 + ws]);
    } else if let Some(i) = last_in_sec {
        if !lines[i].ends_with('\n') {
            lines[i].push_str(eol);
        }
        lines.insert(i + 1, format!("{key} ={value}{eol}"));
    } else {
        if let Some(l) = lines.last_mut() {
            if !l.ends_with('\n') {
                l.push_str(eol);
            }
        }
        lines.push(format!("[{section}]{eol}{key} ={value}{eol}"));
    }
    ini_encode(enc, &lines.concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn replaces_run_keeping_other_bytes_ansi_crlf() {
        // ANSI (ó = 0xF3), CRLF, espacios alrededor del '='.
        let raw = b"[CtEdit]\r\nBin = C:\\x\r\nRun =C:\\old\\Proy\r\nDescripci\xF3n = x\r\n[Other]\r\nRun = no\r\n";
        let out = ini_set_value(raw, "CtEdit", "Run", "C:\\new\\MCP_Demo").unwrap();
        let expected = b"[CtEdit]\r\nBin = C:\\x\r\nRun =C:\\new\\MCP_Demo\r\nDescripci\xF3n = x\r\n[Other]\r\nRun = no\r\n";
        assert_eq!(out, expected.to_vec());
    }

    #[test]
    fn keeps_utf8_bom_and_lf() {
        let mut raw = vec![0xEF, 0xBB, 0xBF];
        raw.extend_from_slice("[CtEdit]\nRun = a\nBackup = \n".as_bytes());
        let out = ini_set_value(&raw, "CtEdit", "Run", "b").unwrap();
        let mut expected = vec![0xEF, 0xBB, 0xBF];
        expected.extend_from_slice("[CtEdit]\nRun = b\nBackup = \n".as_bytes());
        assert_eq!(out, expected);
    }

    #[test]
    fn keeps_utf16_le() {
        let text = "[CtEdit]\r\nRun =x\r\n";
        let mut raw = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            raw.extend_from_slice(&u.to_le_bytes());
        }
        let out = ini_set_value(&raw, "CtEdit", "Run", "C:\\Ñandú").unwrap();
        let back = ini_decode(IniEnc::Utf16Le, &out).unwrap();
        assert_eq!(back, "[CtEdit]\r\nRun =C:\\Ñandú\r\n");
        assert_eq!(&out[..2], &[0xFF, 0xFE]);
    }

    #[test]
    fn inserts_missing_key_and_section() {
        let raw = b"[CtEdit]\r\nBin = x\r\n\r\n[Other]\r\nA = 1\r\n";
        let out = String::from_utf8(ini_set_value(raw, "CtEdit", "Run", "P").unwrap()).unwrap();
        assert_eq!(out, "[CtEdit]\r\nBin = x\r\nRun =P\r\n\r\n[Other]\r\nA = 1\r\n");
        let out = String::from_utf8(ini_set_value(b"[Other]\nA = 1", "CtEdit", "Run", "P").unwrap()).unwrap();
        assert_eq!(out, "[Other]\nA = 1\n[CtEdit]\nRun =P\n");
    }

    #[test]
    fn refuses_unrepresentable_value_in_ansi() {
        let raw = b"[CtEdit]\r\nRun = a\r\nX = \xF3\r\n"; // ANSI
        // '中' y 'ő' no están en cp1252 (a diferencia de '€').
        let e = ini_set_value(raw, "CtEdit", "Run", "C:\\中").unwrap_err().to_string();
        assert!(e.contains("codificación ANSI"), "{e}");
        let e = ini_set_value(raw, "CtEdit", "Run", "C:\\ő").unwrap_err().to_string();
        assert!(e.contains("codificación ANSI"), "{e}");
        let e = ini_set_value(raw, "CtEdit", "Run", "línea\nnueva").unwrap_err().to_string();
        assert!(e.contains("saltos de línea"), "{e}");
        assert!(!e.contains("codificación"), "{e}");
    }

    #[test]
    fn set_run_writes_backup_and_new_value() {
        let d = TempDir::new("setrun");
        let proj = d.join("MCP_Demo");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("VARIABLE.DBF"), b"x").unwrap();
        let ini = d.join("citect.ini");
        let original = b"[CtEdit]\r\nRun =C:\\viejo\r\n".to_vec();
        std::fs::write(&ini, &original).unwrap();
        let msg = set_run(&ini, proj.to_str().unwrap()).unwrap();
        assert!(msg.contains("respaldo"));
        let now = std::fs::read(&ini).unwrap();
        assert_eq!(String::from_utf8(now).unwrap(), format!("[CtEdit]\r\nRun ={}\r\n", proj.to_str().unwrap()));
        let baks: Vec<_> = std::fs::read_dir(d.path()).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("citect.ini.") && e.file_name().to_string_lossy().ends_with(".bak")).collect();
        assert_eq!(baks.len(), 1);
        assert_eq!(std::fs::read(baks[0].path()).unwrap(), original);
        // Sin temporal `citect.ini.mcp-tmp` tras la escritura.
        assert!(!std::fs::read_dir(d.path()).unwrap().flatten().any(|e| e.file_name().to_string_lossy().ends_with(".mcp-tmp")));
    }

    #[test]
    fn project_checks() {
        let d = TempDir::new("projcheck");
        let err = |p: &str| check_project_dir(p).unwrap_err().to_string();
        assert!(err("").contains("está vacío"));
        let empty = d.join("Vacio");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(err(empty.to_str().unwrap()).contains("no parece un proyecto"));
        let sys = d.join("System");
        std::fs::create_dir_all(&sys).unwrap();
        std::fs::write(sys.join("VARIABLE.DBF"), b"x").unwrap();
        assert!(err(sys.to_str().unwrap()).contains("sistema o librería"));
        assert!(err(d.join("no-existe").to_str().unwrap()).contains("no es una carpeta"));
        // Control positivo: carpeta con VARIABLE.DBF y nombre que no es de sistema.
        let ok = d.join("MCP_Demo");
        std::fs::create_dir_all(&ok).unwrap();
        std::fs::write(ok.join("VARIABLE.DBF"), b"x").unwrap();
        assert_eq!(check_project_dir(ok.to_str().unwrap()).unwrap(), ok);
    }
}
