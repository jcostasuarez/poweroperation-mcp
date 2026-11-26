//! Proyecto activo de Power Operation Studio (`po_studio_project`, `po_studio_set_project`).
//!
//! Studio guarda el activo en `[CtEdit] LASTDATABASE` / `LASTDATABASEPATH` del citect.ini, pero solo los lee al
//! abrir; con Studio abierto el cambio pasa por dentro (botón "Make Active" de la vista Projects → servicio
//! ConfigProvider `Project.SetActive` → `ctExplor.dll`), y no hay API externa. Por eso:
//! - Studio y Graphics Builder abiertos, sin páginas abiertas en Graphics Builder: `ProjectSelect` por COM de Graphics
//!   Builder, que cambia también el activo de Studio (título, vistas y LASTDATABASE) en caliente.
//! - Studio abierto sin Graphics Builder: UI Automation sobre su ventana (fila del proyecto seleccionada con flechas e
//!   Invoke de `AID_MakeActiveButton`). Verificado el 2026-10-16: la selección funciona, pero Make Active no cambió el
//!   activo por ninguna vía de entrada.
//! - Studio cerrado (o reiniciado con permiso): se escriben esas claves del citect.ini (con respaldo) y se abre.
//! Un proyecto sin `PROJECT.CIT` no se deja activar: si es un proyecto creado por el MCP, se le crea.

use std::path::{Path, PathBuf};

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::dbfwrite::MCP_MARKER;
use super::graphics::{gb_projects, open_pages, select_project};
use super::projadmin::{ps_str, run_ps32};
use super::runadmin::{citect_ini, ini_backup, ini_update};
use crate::discover::{self, CitectIni};
use crate::procs;
use crate::project::{self, Project};
use crate::server::{PowerOpServer, ToolResult, blocking, err};

const IDE_EXE: &str = "CitectIDE.exe";
/// Proyectos de sistema y librerías: nunca se hacen activos desde el MCP (los prefijos Library_* también).
const PROTECTED: &[&str] = &["System", "Include", "PLS_Include", "PSO_Equipment", "IndustrialGraphics_Include"];
/// Espera a que Studio cambie el activo tras "Make Active".
const SWITCH_WAIT_SECS: u64 = 40;
/// Espera a que Studio muestre el proyecto que seleccionó Graphics Builder.
const GB_SWITCH_WAIT: Duration = Duration::from_secs(20);
/// Espera a que Studio abra y muestre su ventana principal.
const OPEN_WAIT: Duration = Duration::from_secs(180);

#[derive(Deserialize, JsonSchema)]
pub struct SetParams {
    /// Proyecto a dejar activo en Studio: nombre (como en po_project_list) o carpeta completa.
    pub project: String,
    /// true = si Studio está abierto y no cambia en caliente, cerrarlo en orden (WM_CLOSE, nunca a la fuerza),
    /// escribir el citect.ini y volver a abrirlo; si está cerrado, abrirlo. Cierra también lo que Studio tenga
    /// abierto: confirmalo con el usuario antes (que guarde los cambios). Por defecto false.
    #[serde(default)]
    pub restart_studio: bool,
    /// Solo restart_studio: segundos de espera a que Studio se cierre (10 a 180; por defecto 60).
    #[serde(default)]
    pub wait_secs: Option<u64>,
}

#[tool_router(router = studio_router, vis = "pub(crate)")]
impl PowerOpServer {
    #[tool(name = "po_studio_project", description = "Proyecto activo de Power Operation Studio (solo lectura): si Studio está abierto (PID, título de la ventana y proyecto activo que muestra) y las claves del citect.ini [CtEdit] LASTDATABASE / LASTDATABASEPATH (activo de Studio, se leen al abrirlo), LASTSETUPPATH (último Computer Setup Wizard) y Run= (lo que ejecuta el runtime). Avisa si no coinciden.")]
    async fn studio_project(&self) -> ToolResult {
        let v = blocking(Duration::from_secs(20), || Ok(studio_report(&citect_ini()?))).await.map_err(err)?;
        serde_json::to_string_pretty(&v).map_err(|e| e.to_string())
    }

    #[tool(name = "po_studio_set_project", description = "CONFIRMÁ CON EL USUARIO antes. Cambia el proyecto activo de Power Operation Studio. Con Studio y Graphics Builder abiertos: ProjectSelect por COM de Graphics Builder, que cambia también el activo de Studio en caliente (título, vistas, LASTDATABASE/LASTDATABASEPATH; no toca Run=), sin cerrar nada; si Graphics Builder tiene páginas abiertas (incluida una Untitled) da error con la lista (hay que cerrarlas: con páginas abiertas no cambia). Con Studio abierto sin Graphics Builder: UI Automation sobre 'Make Active' de la vista Projects, que no tuvo efecto en las pruebas (NO_CHANGE): conviene abrir Graphics Builder desde Studio o usar restart_studio=true. Si Studio no cambia, con restart_studio=true lo cierra en orden (nunca a la fuerza), escribe [CtEdit] LASTDATABASE y LASTDATABASEPATH en el citect.ini (respaldo citect.ini.<fecha>.bak) y lo vuelve a abrir. Con Studio cerrado escribe el citect.ini (y lo abre si restart_studio=true). Si al proyecto le falta PROJECT.CIT y fue creado por el MCP, se lo crea. No cambia Run= (eso es po_runtime_control) ni LASTSETUPPATH. Rechaza proyectos de sistema y librería. Requiere POWEROP_ALLOW_WRITE=1.")]
    async fn studio_set_project(&self, Parameters(p): Parameters<SetParams>) -> ToolResult {
        self.require_write("po_studio_set_project").map_err(err)?;
        let detail = format!("project={} restart_studio={}", p.project.trim(), p.restart_studio);
        let res = self.set_inner(p).await;
        self.audit(
            "po_studio_set_project",
            &detail,
            &match &res {
                Ok(v) => format!("OK {}", v["result"].as_str().unwrap_or_default()),
                Err(e) => format!("ERROR {e:#}"),
            },
        );
        let v = res.map_err(err)?;
        serde_json::to_string_pretty(&v).map_err(|e| e.to_string())
    }
}

impl PowerOpServer {
    async fn set_inner(&self, p: SetParams) -> Result<Value> {
        let name = p.project.trim().to_string();
        if name.is_empty() {
            bail!("Indicá el proyecto (nombre o carpeta completa).");
        }
        let cat = self.catalog(false).await?;
        let proj = cat.resolve(&name)?;
        check_target(&proj)?;
        let demo = cat.resolve("MCP_Demo").ok().map(|d| d.path);
        let inis = cat.inis.clone();
        let wait = Duration::from_secs(p.wait_secs.unwrap_or(60).clamp(10, 180));
        // Studio abierto con otro activo y Graphics Builder abierto: se intenta en caliente por Graphics Builder.
        let (title, gb_open) = blocking(Duration::from_secs(20), || Ok((studio()?.map(|s| s.title), !crate::gb::running_pids().is_empty()))).await?;
        let shown = title.as_deref().and_then(active_from_title);
        let gb = match (title.is_some(), gb_open) {
            (true, true) if !shown.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(&proj.name)) => Some(self.gb_select(&proj, shown).await),
            (true, false) => Some(GbPath::Unavailable("Graphics Builder no está abierto".into())),
            _ => None,
        };
        blocking(Duration::from_secs(600), move || set_project(&proj, &inis, demo.as_deref(), gb, p.restart_studio, wait)).await
    }

    /// `ProjectSelect` del proyecto en Graphics Builder (que cambia también el activo de Studio). Solo si no tiene
    /// páginas abiertas (si no, no hace nada), lo lista y tiene GLOBAL.CTL (si no, Graphics Builder muestra
    /// "Cannot convert" y aceptarlo lo cierra).
    async fn gb_select(&self, proj: &Project, studio_active: Option<String>) -> GbPath {
        if let Some(hint) = super::graphics::global_ctl_hint(proj) {
            return GbPath::Blocked(hint);
        }
        let name = proj.name.clone();
        let ini = match citect_ini() {
            Ok(i) => i.path,
            Err(e) => return GbPath::Unavailable(format!("{e:#}")),
        };
        let r = self
            .gb
            .call(move |gb| {
                gb.ensure(None)?;
                let prev = gb.m("ProjectSelected", &[]).map(|r| r.ret.text()).unwrap_or_default();
                let (pages, _) = open_pages(gb);
                if !pages.is_empty() {
                    // Sin proyecto: el de PageAppearanceGetEx es el del template, no el de la página (visto el 2026-10-16).
                    let names: Vec<String> = pages
                        .iter()
                        .map(|p| {
                            let name = p["page"].as_str().unwrap_or("").to_string();
                            if p["modified"].as_bool() == Some(true) { format!("{name} (con cambios sin guardar)") } else { name }
                        })
                        .collect();
                    return Ok(GbPath::Blocked(format!(
                        "Graphics Builder tiene {} página(s) abierta(s) ({}) y con páginas abiertas su ProjectSelect no cambia el proyecto (ni el de Studio). \
                         Pedile al usuario que las cierre en Graphics Builder (guardando lo que quiera conservar; las 'Untitled' son páginas nuevas sin guardar) y reintentá.",
                        pages.len(),
                        names.join(", ")
                    )));
                }
                if !gb_projects(gb).iter().any(|p| p.eq_ignore_ascii_case(&name)) {
                    return Ok(GbPath::Blocked(format!(
                        "Graphics Builder no lista el proyecto {name} (lo carga al abrirse: si el proyecto es nuevo, cerrá y volvé a abrir Graphics Builder desde Studio)."
                    )));
                }
                // Studio reescribe LASTDATABASE/LASTDATABASEPATH al cambiar: respaldo antes, como en el camino del citect.ini.
                let bak = ini_backup(&ini, &std::fs::read(&ini)?)?;
                let run_before = ini_val(&ini, "Run");
                let t0 = Instant::now();
                // Si Graphics Builder ya lo tenía seleccionado (no siempre sigue a Studio), seleccionarlo otra vez no
                // haría nada: primero el que muestra Studio.
                if prev.eq_ignore_ascii_case(&name)
                    && let Some(a) = studio_active.as_deref()
                {
                    select_project(gb, a)?;
                }
                select_project(gb, &name)?;
                Ok(GbPath::Selected { prev, ms: t0.elapsed().as_millis(), bak, run_before })
            })
            .await;
        r.unwrap_or_else(|e| GbPath::Unavailable(format!("{e:#}")))
    }
}

/// Intento de cambio por Graphics Builder.
enum GbPath {
    /// Graphics Builder cerrado o sin respuesta: no se intentó.
    Unavailable(String),
    /// No se puede por Graphics Builder (páginas abiertas, proyecto que no lista, sin GLOBAL.CTL): el motivo.
    Blocked(String),
    /// `ProjectSelect` hecho en `ms`; antes estaba `prev`; respaldo previo del citect.ini en `bak` y su `Run=` en `run_before`.
    Selected { prev: String, ms: u128, bak: PathBuf, run_before: String },
}

/// Destino válido: carpeta existente, no de sistema ni librería.
fn check_target(p: &Project) -> Result<()> {
    let dir = p.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if [&p.name, &dir].iter().any(|n| PROTECTED.iter().any(|x| x.eq_ignore_ascii_case(n)) || n.to_ascii_lowercase().starts_with("library_")) {
        bail!("'{}' es un proyecto de sistema o librería: no se hace activo desde el MCP", p.name);
    }
    if !p.path.is_dir() {
        bail!("La carpeta del proyecto no existe: {}", p.path.display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// Estado

/// Studio abierto: PID y título de su ventana principal.
struct Studio {
    pid: u32,
    title: String,
}

fn studio() -> Result<Option<Studio>> {
    for pid in procs::pids_of(IDE_EXE)? {
        if let Some(w) = procs::windows_of(pid).into_iter().find(|w| active_from_title(&w.title).is_some()) {
            return Ok(Some(Studio { pid, title: w.title }));
        }
    }
    // Abriendo (pantalla de inicio sin la ventana principal todavía).
    Ok(procs::pids_of(IDE_EXE)?.first().map(|&pid| Studio { pid, title: String::new() }))
}

/// "Transferencia_Demo - Power Operation Studio" → "Transferencia_Demo".
pub(crate) fn active_from_title(title: &str) -> Option<String> {
    let (name, app) = title.rsplit_once(" - ")?;
    (app.contains("Studio") && !name.trim().is_empty()).then(|| name.trim().to_string())
}

fn ini_val(ini: &Path, key: &str) -> String {
    discover::parse_ini(ini).and_then(|i| i.get("CtEdit", key).map(str::to_string)).unwrap_or_default()
}

fn same_path(a: &str, b: &str) -> bool {
    let n = |s: &str| s.trim().trim_end_matches(['\\', '/']).to_ascii_lowercase();
    !a.trim().is_empty() && n(a) == n(b)
}

fn studio_report(ini: &CitectIni) -> Value {
    let st = studio().ok().flatten();
    let active = st.as_ref().and_then(|s| active_from_title(&s.title));
    let (db, db_path, setup, run) = (
        ini_val(&ini.path, "LASTDATABASE"),
        ini_val(&ini.path, "LASTDATABASEPATH"),
        ini_val(&ini.path, "LASTSETUPPATH"),
        ini_val(&ini.path, "Run"),
    );
    let mut notes = Vec::new();
    if let Some(a) = &active
        && !a.eq_ignore_ascii_case(&db)
    {
        notes.push(format!("Studio muestra '{a}' pero LASTDATABASE es '{db}' (Studio escribe el ini al cambiar o al cerrar)."));
    }
    if !run.is_empty() && !same_path(&run, &db_path) {
        notes.push("Run= (runtime) no es el activo de Studio: el runtime ejecuta Run=; Studio compila y corre su activo.".into());
    }
    if !setup.is_empty() && !same_path(&setup, &run) {
        notes.push("LASTSETUPPATH no es Run=: el Computer Setup Wizard se corrió para otro proyecto.".into());
    }
    json!({
        "studio_running": st.is_some(),
        "studio_pid": st.as_ref().map(|s| s.pid),
        "studio_title": st.as_ref().map(|s| s.title.clone()),
        "studio_active_project": active,
        "citect_ini": ini.path,
        "LASTDATABASE": db,
        "LASTDATABASEPATH": db_path,
        "LASTSETUPPATH": setup,
        "Run": run,
        "notes": notes,
    })
}

// ---------------------------------------------------------------------------------------------------------------
// Cambio

fn set_project(proj: &Project, inis: &[CitectIni], demo: Option<&Path>, gb: Option<GbPath>, restart: bool, close_wait: Duration) -> Result<Value> {
    let ini = citect_ini()?;
    let mut steps: Vec<String> = Vec::new();
    ensure_cit(proj, inis, demo, &mut steps)?;

    match studio()? {
        Some(st) => {
            // Tras el ProjectSelect de Graphics Builder Studio ya puede mostrar el nuevo (cambia en ~200 ms).
            if !matches!(gb, Some(GbPath::Selected { .. })) && active_from_title(&st.title).is_some_and(|a| a.eq_ignore_ascii_case(&proj.name)) {
                return Ok(done(&ini, "ya era el activo", steps));
            }
            match gb {
                Some(GbPath::Selected { prev, ms, bak, run_before }) => {
                    steps.push(format!("citect.ini respaldado en {}", bak.display()));
                    steps.push(format!("Graphics Builder: ProjectSelect({}) en {ms} ms (tenía seleccionado {prev})", proj.name));
                    match wait_active(&proj.name, GB_SWITCH_WAIT) {
                        Ok(title) => {
                            steps.push(format!("Studio (PID {}) muestra '{title}'", st.pid));
                            let run_after = ini_val(&ini.path, "Run");
                            if !run_after.eq_ignore_ascii_case(&run_before) {
                                steps.push(format!("AVISO: Run= del citect.ini cambió de '{run_before}' a '{run_after}'"));
                            }
                            let mut v = done(&ini, "cambiado en Studio abierto (ProjectSelect de Graphics Builder)", steps);
                            v["via"] = json!("graphics_builder");
                            return Ok(v);
                        }
                        Err(e) => {
                            steps.push(format!("{e:#}"));
                            if !restart {
                                bail!(
                                    "Graphics Builder seleccionó {} pero Studio no lo mostró como activo en {} s. Revisá Studio y Graphics Builder (po_studio_project, po_gb_status). Pasos: {}",
                                    proj.name,
                                    GB_SWITCH_WAIT.as_secs(),
                                    steps.join(" | ")
                                );
                            }
                        }
                    }
                }
                Some(GbPath::Blocked(why)) => {
                    if !restart {
                        bail!(
                            "No se puede cambiar el proyecto activo con Studio abierto: {why} Alternativa: restart_studio=true (cierra y reabre Studio; confirmalo con el usuario). No se cambió nada."
                        );
                    }
                    steps.push(format!("Graphics Builder no sirve: {why}"));
                }
                other => {
                    let why = match other {
                        Some(GbPath::Unavailable(w)) => w,
                        _ => "Graphics Builder no está abierto".into(),
                    };
                    let (result, lines) = make_active_ui(&proj.name)?;
                    steps.push(format!("UI Automation en Studio (PID {}): {result} {}", st.pid, lines.join(" ")));
                    if result == "OK" || result == "ALREADY" {
                        return Ok(done(&ini, "cambiado en Studio abierto", steps));
                    }
                    if !restart {
                        bail!(
                            "Studio no cambió el proyecto activo ({result}; {}). 'Make Active' se invocó y no tuvo efecto (probado el 2026-10-16 por UI Automation, teclado y clic real). \
                             Con Graphics Builder abierto (desde Studio, sin páginas abiertas) el cambio se hace en caliente; no se usó porque: {why}. \
                             Si no, con restart_studio=true se cierra Studio en orden, se escribe el citect.ini y se vuelve a abrir (confirmalo con el usuario: cierra lo que tenga abierto). No se cambió nada.",
                            lines.join(" ")
                        );
                    }
                }
            }
            close_studio(st.pid, close_wait)?;
            steps.push(format!("Studio (PID {}) cerrado en orden", st.pid));
            steps.push(write_ini(&ini, proj)?);
            let exe = ide_exe(&ini)?;
            launch(&exe)?;
            steps.push(format!("Studio abierto: {}", exe.display()));
            let title = wait_open()?;
            let ok = active_from_title(&title).is_some_and(|a| a.eq_ignore_ascii_case(&proj.name));
            if !ok {
                bail!("Studio se volvió a abrir con '{title}', no con '{}'. Pasos: {}", proj.name, steps.join(" | "));
            }
            Ok(done(&ini, "Studio reiniciado con el proyecto activo", steps))
        }
        None => {
            steps.push(write_ini(&ini, proj)?);
            if !restart {
                return Ok(done(&ini, "citect.ini cambiado; Studio lo toma al abrir", steps));
            }
            let exe = ide_exe(&ini)?;
            launch(&exe)?;
            steps.push(format!("Studio abierto: {}", exe.display()));
            let title = wait_open()?;
            if !active_from_title(&title).is_some_and(|a| a.eq_ignore_ascii_case(&proj.name)) {
                bail!("Studio abrió con '{title}', no con '{}'. Pasos: {}", proj.name, steps.join(" | "));
            }
            Ok(done(&ini, "Studio abierto con el proyecto activo", steps))
        }
    }
}

fn done(ini: &CitectIni, result: &str, steps: Vec<String>) -> Value {
    let mut v = studio_report(ini);
    v["result"] = json!(result);
    v["steps"] = json!(steps);
    v
}

/// PROJECT.CIT: si falta y el proyecto es del MCP se crea (propiedades de MCP_Demo); si no, error.
fn ensure_cit(proj: &Project, inis: &[CitectIni], demo: Option<&Path>, steps: &mut Vec<String>) -> Result<()> {
    if project::find_file(&proj.path, "PROJECT.CIT").is_some() {
        return Ok(());
    }
    if project::find_file(&proj.path, MCP_MARKER).is_none() {
        bail!(
            "'{}' no tiene PROJECT.CIT (las propiedades del proyecto que lee Studio) y no fue creado por el MCP: abrí sus propiedades o agregalo desde Studio. No se cambió nada.",
            proj.name
        );
    }
    let (dll, _) = super::projadmin::locate(inis)?;
    let bin = dll.parent().map(Path::to_path_buf).unwrap_or_default();
    let line = super::projcreate::write_project_cit(&bin, demo, &proj.path, &proj.name, &proj.name)?;
    steps.push(format!("PROJECT.CIT creado: {line}"));
    Ok(())
}

fn write_ini(ini: &CitectIni, proj: &Project) -> Result<String> {
    let path = proj.path.to_string_lossy().trim_end_matches(['\\', '/']).to_string();
    let bak = ini_update(&ini.path, &[("CtEdit", "LASTDATABASE", &proj.name), ("CtEdit", "LASTDATABASEPATH", &path)])?;
    Ok(format!("citect.ini: LASTDATABASE={} LASTDATABASEPATH={path} (respaldo: {})", proj.name, bak.display()))
}

/// WM_CLOSE a las ventanas de Studio y espera a que salga. Nunca lo termina a la fuerza.
fn close_studio(pid: u32, wait: Duration) -> Result<()> {
    let posted = procs::post_close(&[pid]);
    let start = Instant::now();
    while start.elapsed() < wait {
        if !procs::pids_of(IDE_EXE)?.contains(&pid) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let dialogs: Vec<String> = procs::windows_of(pid).into_iter().map(|w| w.title).filter(|t| !t.is_empty()).collect();
    bail!(
        "Studio (PID {pid}) no se cerró en {} s (WM_CLOSE a {posted} ventana(s)); ventanas abiertas: {:?}. Puede estar preguntando si guardar: respondé en Studio y reintentá. No se forzó el cierre ni se cambió el citect.ini.",
        wait.as_secs(),
        dialogs
    )
}

/// `CitectIDE.exe` de la carpeta Bin del citect.ini o de la instalación.
fn ide_exe(ini: &CitectIni) -> Result<PathBuf> {
    if let Some(bin) = ini.get("CtEdit", "Bin") {
        let exe = Path::new(bin).join("CitectIDE").join(IDE_EXE);
        if exe.is_file() {
            return Ok(exe);
        }
    }
    for root in discover::install_roots() {
        if let Some(exe) = discover::find_files(&root, 6, 100_000, &|n| n == "citectide.exe").into_iter().next() {
            return Ok(exe);
        }
    }
    bail!("No encontré {IDE_EXE} (Bin del citect.ini ni instalación de Power Operation)")
}

/// Abre Studio desde su carpeta, sin heredar los pipes del MCP y fuera del job del cliente si se puede.
fn launch(exe: &Path) -> Result<()> {
    procs::spawn_detached(exe).map(|_| ())
}

/// Espera a que Studio muestre su ventana principal ("<proyecto> - ... Studio") y devuelve el título.
fn wait_open() -> Result<String> {
    let start = Instant::now();
    while start.elapsed() < OPEN_WAIT {
        if let Some(st) = studio()?
            && active_from_title(&st.title).is_some()
        {
            return Ok(st.title);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Err(anyhow!("Studio no mostró su ventana principal en {} s", OPEN_WAIT.as_secs()))
}

/// Espera a que el título de Studio muestre `name` como activo y devuelve el título.
fn wait_active(name: &str, wait: Duration) -> Result<String> {
    let start = Instant::now();
    let mut last = String::new();
    while start.elapsed() < wait {
        if let Some(st) = studio()? {
            if active_from_title(&st.title).is_some_and(|a| a.eq_ignore_ascii_case(name)) {
                return Ok(st.title);
            }
            last = st.title;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(anyhow!("Studio sigue mostrando '{last}' a los {} s", wait.as_secs()))
}

/// UI Automation sobre Studio (PowerShell de 32 bits): vista Projects, selección de la fila con las flechas (mensajes
/// de teclado a su ventana, sin mover el mouse) e Invoke de "Make Active"; espera a que cambie el activo.
/// Devuelve el RESULT (OK, ALREADY, NO_STUDIO, NO_PROJECTS_VIEW, NOT_LISTED, NO_FOCUS, NO_BUTTON, NO_CHANGE,
/// ERROR ...) y las demás líneas.
/// Verificado 2026-10-16 con Studio abierto: la selección por teclado funciona; el clic por PostMessage no seleccionaba la fila
/// en esa posición de la ventana. Pero "Make Active" no hace nada por ninguna vía (Invoke de UI Automation,
/// teclado sobre el botón, PostMessage o clic real del mouse sobre el botón): queda NO_CHANGE.
fn make_active_ui(name: &str) -> Result<(String, Vec<String>)> {
    let script = UIA_SCRIPT.replace("__NAME__", &ps_str(name)).replace("__WAIT__", &SWITCH_WAIT_SECS.to_string());
    let run = run_ps32(&script, "studio", Duration::from_secs(SWITCH_WAIT_SECS + 60))?;
    if run.timed_out {
        bail!("UI Automation sobre Studio no terminó a tiempo");
    }
    let mut result = None;
    let mut lines = Vec::new();
    for l in run.output.lines().map(str::trim).filter(|l| !l.is_empty()) {
        match l.strip_prefix("RESULT=") {
            Some(r) => result = Some(r.to_string()),
            None => lines.push(l.to_string()),
        }
    }
    let result = result.ok_or_else(|| anyhow!("UI Automation sin resultado: {}", crate::text::truncate(&run.output, 500)))?;
    Ok((result, lines))
}

const UIA_SCRIPT: &str = r#"$ErrorActionPreference = 'Stop'
try {
    Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
    Add-Type -TypeDefinition @"
using System; using System.Runtime.InteropServices;
public static class PoMcpWin {
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
}
"@
    $name = __NAME__
    $wait = __WAIT__
    $A = [Windows.Automation.AutomationElement]; $TS = [Windows.Automation.TreeScope]
    $TW = [Windows.Automation.TreeWalker]::ControlViewWalker
    function ById($root, $id) { $root.FindFirst($TS::Descendants, (New-Object Windows.Automation.PropertyCondition($A::AutomationIdProperty, $id))) }
    function ActiveName($root) { $t = ById $root 'AID_ActiveProjectTextBlock'; if ($t -and $t.Current.Name -match '^\s*-?\s*(.+?)\s*\[') { $Matches[1] } else { '' } }
    function RowOf($el) { $r = $el; while ($r -and $r.Current.ClassName -ne 'TreeListViewRow') { $r = $TW.GetParent($r) }; $r }
    function RowName($row) { if (-not $row) { return '' }; $t = ById $row 'AID_ProjectNameTextBlock'; if ($t) { $t.Current.Name } else { '' } }
    # Tecla como mensajes WM_KEYDOWN / WM_KEYUP a la ventana de Studio: no mueve el mouse.
    function Key($h, $vk, $scan, $pause) {
        $d = [IntPtr](($scan -shl 16) -bor 1); $u = [IntPtr](0xC0000001 -bor ($scan -shl 16))
        [void][PoMcpWin]::PostMessage($h, 0x0100, [IntPtr]$vk, $d); Start-Sleep -Milliseconds 60
        [void][PoMcpWin]::PostMessage($h, 0x0101, [IntPtr]$vk, $u); Start-Sleep -Milliseconds $pause
    }
    function WaitActive($root, $secs) {
        $deadline = (Get-Date).AddSeconds($secs)
        while ((Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500; if ((ActiveName $root) -ieq $name) { return $true } }
        return $false
    }
    $p = Get-Process CitectIDE -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
    if (-not $p) { 'RESULT=NO_STUDIO'; exit 0 }
    $h = $p.MainWindowHandle; $root = $A::FromHandle($h)
    $before = ActiveName $root; "BEFORE=$before"
    if ($before -ieq $name) { 'RESULT=ALREADY'; exit 0 }
    $grid = ById $root 'AID_ProjectTreeGrid'
    if (-not $grid) {
        $b = ById $root 'AID_ProjectsButton'
        if ($b) { $b.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke(); Start-Sleep -Seconds 2; $grid = ById $root 'AID_ProjectTreeGrid' }
    }
    if (-not $grid) { 'RESULT=NO_PROJECTS_VIEW'; exit 0 }
    $cond = New-Object Windows.Automation.PropertyCondition($A::AutomationIdProperty, 'AID_ProjectNameTextBlock')
    $names = @($grid.FindAll($TS::Descendants, $cond) | Sort-Object { $_.Current.BoundingRectangle.Y })
    $idx = -1
    for ($i = 0; $i -lt $names.Count; $i++) { if ($names[$i].Current.Name -ieq $name) { $idx = $i; break } }
    if ($idx -lt 0) { 'RESULT=NOT_LISTED'; exit 0 }
    "ROWS=$($names.Count) TARGET_INDEX=$idx"
    # Foco en la grilla (UIA SetFocus sobre una celda de la primera fila) y después las flechas.
    $cell = $names[0]; while ($cell -and $cell.Current.ClassName -ne 'GridViewCell') { $cell = $TW.GetParent($cell) }
    if (-not $cell) { 'RESULT=NO_FOCUS'; exit 0 }
    $cell.SetFocus(); Start-Sleep -Milliseconds 400
    $q = $A::FocusedElement; $inGrid = $false
    while ($q) { if ($q.Current.AutomationId -eq 'AID_ProjectTreeGrid') { $inGrid = $true; break }; $q = $TW.GetParent($q) }
    if (-not $inGrid) { 'RESULT=NO_FOCUS'; exit 0 }
    # Arriba de todo (sobran flechas arriba) y abajo hasta la fila del proyecto.
    for ($i = 0; $i -le $names.Count; $i++) { Key $h 0x26 0x48 120 }
    for ($i = 0; $i -lt $idx; $i++) { Key $h 0x28 0x50 120 }
    # El botón "Make Active" aparece en la fila seleccionada: solo se usa si está en la fila del proyecto pedido.
    $btn = $null
    for ($t = 0; $t -lt 12 -and -not $btn; $t++) {
        Start-Sleep -Milliseconds 250
        foreach ($b in @($root.FindAll($TS::Descendants, (New-Object Windows.Automation.PropertyCondition($A::AutomationIdProperty, 'AID_MakeActiveButton'))))) {
            if ((RowName (RowOf $b)) -ieq $name) { $btn = $b; break }
        }
    }
    if (-not $btn) { "SELECTED_ROW=$(RowName (RowOf (ById $root 'AID_MakeActiveButton')))"; 'RESULT=NO_BUTTON'; exit 0 }
    $btn.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    if (WaitActive $root $wait) { "AFTER=$name"; 'RESULT=OK'; exit 0 }
    "AFTER=$(ActiveName $root)"; 'RESULT=NO_CHANGE'; exit 0
} catch {
    'RESULT=ERROR ' + $_.Exception.GetBaseException().Message
    exit 2
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_from_window_title() {
        assert_eq!(active_from_title("Transferencia_Demo - Power Operation Studio").as_deref(), Some("Transferencia_Demo"));
        assert_eq!(active_from_title("A - B - Plant SCADA Studio").as_deref(), Some("A - B"));
        assert_eq!(active_from_title("Graphics Builder"), None);
        assert_eq!(active_from_title(" - Power Operation Studio"), None);
    }

    #[test]
    fn rejects_system_projects() {
        let p = |n: &str| Project { name: n.into(), path: std::env::temp_dir(), running: false };
        for n in ["Include", "library_controls"] {
            let e = check_target(&p(n)).unwrap_err().to_string();
            assert!(e.contains("sistema o librería"), "{n}: {e}");
        }
        // Por carpeta, aunque el nombre del catálogo sea otro.
        let lib = Project { name: "Estilos".into(), path: std::env::temp_dir().join("Library_Styles"), running: false };
        assert!(check_target(&lib).unwrap_err().to_string().contains("sistema o librería"));
        assert!(check_target(&p("Planta")).is_ok());
        let e = check_target(&Project { name: "Planta".into(), path: std::env::temp_dir().join("pomcp-no-existe"), running: false }).unwrap_err().to_string();
        assert!(e.contains("no existe"), "{e}");
    }

    #[test]
    fn uia_script_quotes_name() {
        let s = UIA_SCRIPT.replace("__NAME__", &ps_str("O'Brien")).replace("__WAIT__", "40");
        assert!(s.contains("$name = 'O''Brien'"));
        assert!(s.contains("$wait = 40"));
        assert!(!s.contains("__"));
    }
}
