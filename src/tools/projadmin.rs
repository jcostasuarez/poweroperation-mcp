//! Administración de proyectos: compilar con ProjectManager (PowerShell de 32 bits) y leer errlog.dbf.

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Local, Utc};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::runtime::cicode_str;
use crate::dbf::Dbf;
use crate::discover::{self, CitectIni};
use crate::project::{self, Project};
use crate::server::{PowerOpServer, ToolResult, blocking, err};
use crate::text::{decode, truncate};

const DISK_TIMEOUT: Duration = Duration::from_secs(120);
/// Tiempo máximo de una compilación (proyectos grandes pueden tardar varios minutos).
const COMPILE_TIMEOUT: Duration = Duration::from_secs(600);
/// Espera máxima a que el runtime vuelva tras recompilar, para refrescar la página del cliente.
const RUNTIME_BACK_TIMEOUT: Duration = Duration::from_secs(60);
/// Proyectos de sistema y librería: nunca se compilan desde aquí (los prefijos Library_* también).
const PROTECTED: &[&str] = &["System", "Include", "PLS_Include", "PSO_Equipment", "IndustrialGraphics_Include"];
/// CREATE_NO_WINDOW: el PowerShell hijo no abre consola.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Errores y advertencias que se devuelven como máximo (el total va aparte).
const MAX_ITEMS: usize = 200;

#[derive(Deserialize, JsonSchema)]
pub struct CompileParams {
    /// Nombre del proyecto a compilar (o su carpeta completa). Obligatorio: no se usa el proyecto por defecto para no compilar el equivocado.
    pub project: String,
    /// Página a mostrar en el cliente (PageDisplay) si la compilación sale sin errores. Vacío = no refrescar.
    #[serde(default)]
    pub refresh_page: Option<String>,
}

/// Resultado de leer errlog.dbf.
#[derive(Default)]
struct ErrLog {
    items: Vec<Value>,
    errors: usize,
    warnings: usize,
    total: usize,
}

/// Lo que devolvió el proceso de PowerShell.
pub(crate) struct PsRun {
    pub(crate) exit: Option<i32>,
    pub(crate) output: String,
    pub(crate) timed_out: bool,
}

/// Literal de cadena de PowerShell de una sola comilla. PowerShell trata como comilla también las
/// comillas tipográficas (‘ ’ ‚ ‛), así que todas se duplican.
pub(crate) fn ps_str(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// Script de PowerShell (32 bits) que compila el proyecto con ProjectManager. A propósito no usa
/// RunProject: para proyectos fuera de User\ reescribe Run= con una ruta equivocada.
fn compile_script(dll: &Path, ini: &Path, project: &str) -> String {
    let dll_dir = dll.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let (dll, ini) = (dll.to_string_lossy().to_string(), ini.to_string_lossy().to_string());
    format!(
        "Set-Location -LiteralPath {bin}\r\n\
         $env:PATH = {bin} + ';' + $env:PATH\r\n\
         try {{\r\n\
         \x20   $asm = [Reflection.Assembly]::LoadFrom({dll})\r\n\
         \x20   $pm = [Activator]::CreateInstance($asm.GetType('PowerSCADA.Common.Projects.ProjectManager'), @({ini}))\r\n\
         \x20   $pm.Initialize({ini})\r\n\
         \x20   $r = $pm.CompileProject({project})\r\n\
         \x20   Write-Output ('COMPILE_RESULT=[' + $r + ']')\r\n\
         \x20   $e = $pm.LastError\r\n\
         \x20   if ($e) {{ Write-Output ('LAST_ERROR=' + ($e | Out-String)) }}\r\n\
         \x20   exit 0\r\n\
         }} catch {{\r\n\
         \x20   Write-Output ('COMPILE_EXCEPTION=' + $_.Exception.GetBaseException().Message)\r\n\
         \x20   exit 2\r\n\
         }}\r\n",
        bin = ps_str(&dll_dir),
        dll = ps_str(&dll),
        ini = ps_str(&ini),
        project = ps_str(project),
    )
}

/// PowerShell de 32 bits (ProjectManager no carga desde el de 64).
pub(crate) fn ps32_exe() -> Result<PathBuf> {
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).filter(|p| p.is_dir()).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let exe = root.join(r"SysWOW64\WindowsPowerShell\v1.0\powershell.exe");
    if exe.is_file() { Ok(exe) } else { bail!("No se encontró el PowerShell de 32 bits ({})", exe.display()) }
}

/// Ejecuta un script con PowerShell de 32 bits, con tiempo máximo. Captura stdout y stderr del hijo.
fn run_script(exe: &Path, file: &Path, timeout: Duration) -> Result<PsRun> {
    let mut child = Command::new(exe)
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("No se pudo iniciar PowerShell de 32 bits")?;
    // Lectura en hilos con canal: si un proceso nieto hereda los pipes, no bloqueamos para siempre.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    if let Some(mut out) = child.stdout.take() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
    }
    if let Some(mut errs) = child.stderr.take() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = errs.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
    }
    drop(tx);

    let deadline = Instant::now() + timeout;
    let (exit, timed_out) = loop {
        if let Some(st) = child.try_wait()? {
            break (st.code(), false);
        }
        if Instant::now() >= deadline {
            // Mata solo el PowerShell: los procesos que haya lanzado la compilación siguen su curso.
            let _ = child.kill();
            let _ = child.wait();
            break (None, true);
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let mut output = String::new();
    while let Ok(buf) = rx.recv_timeout(Duration::from_secs(5)) {
        output.push_str(&decode(&buf));
    }
    Ok(PsRun { exit, output: output.trim().to_string(), timed_out })
}

fn run_compile(script: &str) -> Result<PsRun> {
    run_ps32(script, "compile", COMPILE_TIMEOUT)
}

/// Escribe el script en un temporal (con BOM: PowerShell 5.1 lee sin BOM como ANSI), lo ejecuta con el PowerShell
/// de 32 bits con tiempo máximo y lo borra. `tag` va en el nombre del temporal.
pub(crate) fn run_ps32(script: &str, tag: &str, timeout: Duration) -> Result<PsRun> {
    let exe = ps32_exe()?;
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let file = std::env::temp_dir().join(format!("pomcp-{tag}-{}-{nanos}.ps1", std::process::id()));
    let mut bytes = b"\xEF\xBB\xBF".to_vec();
    bytes.extend_from_slice(script.as_bytes());
    std::fs::write(&file, &bytes).context("No se pudo escribir el script temporal de PowerShell")?;
    let res = run_script(&exe, &file, timeout);
    let _ = std::fs::remove_file(&file);
    res
}

/// Carpeta bin de la instalación (con PowerSCADA.Common.dll) y citect.ini de Power Operation.
pub(crate) fn locate(inis: &[CitectIni]) -> Result<(PathBuf, PathBuf)> {
    let mut dlls: Vec<PathBuf> = discover::install_roots()
        .iter()
        .flat_map(|r| discover::find_files(r, 4, 100_000, &|n| n == "powerscada.common.dll"))
        .collect();
    dlls.sort_by_key(|p| !p.to_string_lossy().to_lowercase().contains("power operation"));
    let dll = dlls.into_iter().next().ok_or_else(|| anyhow!("No se encontró PowerSCADA.Common.dll (instalación de Power Operation)"))?;
    let ini = pick_ini(inis).ok_or_else(|| anyhow!("No se encontró el citect.ini de Power Operation"))?;
    Ok((dll, ini))
}

/// Prefiere el citect.ini de configuración de Power Operation (...\Power Operation\...\Config\citect.ini).
fn pick_ini(inis: &[CitectIni]) -> Option<PathBuf> {
    inis.iter()
        .map(|i| i.path.clone())
        .min_by_key(|p| {
            let s = p.to_string_lossy().to_lowercase();
            !(s.contains("power operation") && s.contains("config"))
        })
}

/// Proyectos de sistema y librería (por nombre o carpeta).
fn is_protected(p: &Project) -> bool {
    let dir = p.path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    [p.name.to_lowercase(), dir]
        .iter()
        .any(|n| PROTECTED.iter().any(|x| x.eq_ignore_ascii_case(n)) || n.starts_with("library_"))
}

/// Lee errlog.dbf (E_TYPE, E_PAGE, E_FIELD, E_ERROR, E_CONTEXT y, si existen, E_TABLE y E_TAG).
fn read_errlog(path: &Path) -> Result<ErrLog> {
    let dbf = Dbf::open(path)?;
    let col = |n: &str| dbf.field_index(n);
    let (ty, page, field, error, context) = (col("E_TYPE"), col("E_PAGE"), col("E_FIELD"), col("E_ERROR"), col("E_CONTEXT"));
    let (table, tag) = (col("E_TABLE"), col("E_TAG"));
    let mut log = ErrLog::default();
    dbf.for_each(|_, v| {
        let get = |i: Option<usize>| i.map(|i| v[i].clone()).unwrap_or_default();
        let kind = get(ty);
        let up = kind.to_uppercase();
        log.total += 1;
        if up.starts_with("WARN") {
            log.warnings += 1;
        } else if !(up.starts_with("INFO") || up.starts_with("NOTE")) {
            // Cualquier otro tipo (ERROR, FATAL...) cuenta como error: mejor sobrar que no avisar.
            log.errors += 1;
        }
        if log.items.len() < MAX_ITEMS {
            let mut item = json!({"type": kind, "page": get(page), "field": get(field), "error": get(error), "context": get(context)});
            for (k, i) in [("table", table), ("tag", tag)] {
                let s = get(i);
                if !s.is_empty() {
                    item[k] = json!(s);
                }
            }
            log.items.push(item);
        }
        true
    });
    Ok(log)
}

/// Resultado de la compilación según lo que se reescribió: errlog.dbf (con errores o advertencias) o, si no,
/// compilersummary.xml (compilación limpia: el compilador no reescribe errlog.dbf sin nada que informar).
fn outcome(compiled: bool, errlog_fresh: bool, summary_fresh: bool, errors: usize) -> (bool, &'static str) {
    if !compiled {
        (false, "PowerShell terminó con error: ver compile_output.")
    } else if errlog_fresh && errors > 0 {
        (false, "Hay errores de compilación: ver items.")
    } else if errlog_fresh {
        (true, "Compilado sin errores.")
    } else if summary_fresh {
        (true, "Compilado sin errores ni advertencias (errlog.dbf no se reescribe en ese caso; compilersummary.xml sí).")
    } else {
        (false, "Ni errlog.dbf ni compilersummary.xml se reescribieron: no se puede confirmar que compiló (revisar en Studio).")
    }
}

/// Hora local `AAAA-MM-DD HH:MM:SS` de un instante de archivo.
fn fmt_time(t: SystemTime) -> String {
    DateTime::<Utc>::from(t).with_timezone(&Local).format("%Y-%m-%d %H:%M:%S").to_string()
}

#[tool_router(router = projadmin_router, vis = "pub(crate)")]
impl PowerOpServer {
    #[tool(name = "po_project_compile", description = "Compila un proyecto de Power Operation (ProjectManager por PowerShell de 32 bits) y devuelve errores y advertencias de errlog.dbf (tipo, página, campo, error, contexto). ATENCIÓN: si el proyecto es el que corre en el runtime, Runtime Manager A VECES reinicia el runtime al recompilar (todos los Citect32 cambian de PID; visto el 2026-10-14) y a veces no (2026-10-15): confirmá con el usuario antes y mirá po_runtime_procs después. Las páginas nuevas se ven con refresh_page; tags y Cicode nuevos pueden necesitar po_runtime_control action=restart. No usa RunProject. Rechaza proyectos de sistema y librería. Requiere POWEROP_ALLOW_WRITE=1.")]
    async fn project_compile(&self, Parameters(p): Parameters<CompileParams>) -> ToolResult {
        self.require_write("project_compile").map_err(err)?;
        let name = p.project.trim().to_string();
        match self.compile_flow(&name, p.refresh_page.as_deref()).await {
            Ok(v) => {
                let outcome = format!(
                    "{}: {} errores, {} advertencias, {} s",
                    if v["ok"] == true { "OK" } else { "CON ERRORES" },
                    v["errors"],
                    v["warnings"],
                    v["duration_s"]
                );
                self.audit("project_compile", v["path"].as_str().unwrap_or(&name), &outcome);
                pretty(&v)
            }
            Err(e) => {
                let m = err(e);
                self.audit("project_compile", &name, &format!("ERROR: {m}"));
                Err(m)
            }
        }
    }
}

impl PowerOpServer {
    async fn compile_flow(&self, name: &str, refresh_page: Option<&str>) -> Result<Value> {
        if name.is_empty() {
            bail!("Indicá el proyecto a compilar (nombre o carpeta completa).");
        }
        let cat = self.catalog(false).await?;
        let proj = cat.resolve(name)?;
        if is_protected(&proj) {
            bail!(
                "No se compila '{}': es un proyecto de sistema o librería (System, Include, Library_*, PLS_Include, PSO_Equipment, IndustrialGraphics_Include).",
                proj.name
            );
        }
        let inis = cat.inis.clone();
        let (dll, ini) = blocking(DISK_TIMEOUT, move || locate(&inis)).await?;
        // CompileProject no devuelve los errores: quedan en errlog.dbf de la carpeta del proyecto.
        let errlog = project::find_file(&proj.path, "errlog.dbf").unwrap_or_else(|| proj.path.join("errlog.dbf"));
        let script = compile_script(&dll, &ini, &proj.name);

        let started = SystemTime::now();
        let t0 = Instant::now();
        let run = blocking(COMPILE_TIMEOUT + Duration::from_secs(30), move || run_compile(&script)).await?;
        let duration_s = (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0;
        if run.timed_out {
            bail!(
                "La compilación superó {} s: se terminó el PowerShell. Revisá el estado del proyecto en Power Operation Studio.",
                COMPILE_TIMEOUT.as_secs()
            );
        }

        // errlog.dbf solo cuenta si se reescribió durante esta compilación (no devolver errores viejos).
        // Sin errores ni advertencias el compilador no lo toca (verificado): entonces la señal de que
        // compiló es compilersummary.xml, que se reescribe en cada compilación.
        let (errlog2, dir) = (errlog.clone(), proj.path.clone());
        let (mtime, log, summary_fresh) = blocking(DISK_TIMEOUT, move || {
            let fresh = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok().filter(|m| *m >= started);
            let mtime = std::fs::metadata(&errlog2).and_then(|m| m.modified()).ok();
            let log = if fresh(&errlog2).is_some() { Some(read_errlog(&errlog2)?) } else { None };
            let summary = project::find_file(&dir, "compilersummary.xml").and_then(|p| fresh(&p)).is_some();
            Ok((mtime, log, summary))
        })
        .await?;
        let updated = log.is_some();
        let l = log.unwrap_or_default();
        let (ok, note) = outcome(run.exit == Some(0), updated, summary_fresh, l.errors);

        let mut page_refreshed = Value::Null;
        let mut page_error = None;
        if let Some(page) = refresh_page.map(str::trim).filter(|s| !s.is_empty()) {
            if ok {
                match self.show_page(page).await {
                    Ok(()) => page_refreshed = json!(true),
                    Err(e) => {
                        page_refreshed = json!(false);
                        page_error = Some(err(e));
                    }
                }
            } else {
                page_error = Some("No se refrescó la página: la compilación no salió sin errores.".into());
            }
        }

        Ok(json!({
            "project": proj.name,
            "path": proj.path,
            "ok": ok,
            "errors": l.errors,
            "warnings": l.warnings,
            "items": l.items,
            "items_total": l.total,
            "duration_s": duration_s,
            "errlog_mtime": mtime.map(fmt_time),
            "errlog_updated": updated,
            "summary_updated": summary_fresh,
            "note": note,
            "compile_output": truncate(&run.output, 2000),
            "page_refreshed": page_refreshed,
            "page_refresh_error": page_error,
        }))
    }

    /// Muestra una página en el cliente (PageDisplay). Tras recompilar el runtime tarda en volver:
    /// espera un poco y reintenta hasta RUNTIME_BACK_TIMEOUT.
    async fn show_page(&self, page: &str) -> Result<()> {
        let cmd = format!("PageDisplay({})", cicode_str(page));
        tokio::time::sleep(Duration::from_secs(5)).await;
        let deadline = Instant::now() + RUNTIME_BACK_TIMEOUT;
        loop {
            let c = cmd.clone();
            match self.ct.call(move |s| s.cicode(&c, 0)).await {
                Ok(_) => return Ok(()),
                Err(_) if Instant::now() < deadline => tokio::time::sleep(Duration::from_secs(3)).await,
                Err(e) => return Err(e.context(format!("el runtime no respondió en {} s tras compilar", RUNTIME_BACK_TIMEOUT.as_secs()))),
            }
        }
    }
}

fn pretty(v: &Value) -> ToolResult {
    let s = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    Ok(truncate(&s, 60_000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbf::tests::build;
    use crate::testutil::TempDir;

    fn proj(name: &str, dir: &str) -> Project {
        Project { name: name.into(), path: PathBuf::from(format!(r"C:\x\{dir}")), running: false }
    }

    /// Veredicto de la compilación: (compiló PowerShell, errlog.dbf nuevo, compilersummary.xml nuevo, errores).
    #[test]
    fn outcome_uses_summary_when_errlog_untouched() {
        let casos = [
            ((false, true, true, 0), false, "PowerShell terminó con error"),
            ((true, true, true, 2), false, "Hay errores de compilación"),
            ((true, true, false, 0), true, "Compilado sin errores."),
            // Compilación limpia: errlog.dbf viejo, compilersummary.xml nuevo.
            ((true, false, true, 0), true, "compilersummary.xml sí"),
            ((true, false, false, 0), false, "no se puede confirmar"),
        ];
        for ((compiled, errlog, summary, errors), ok, msg) in casos {
            let (got, note) = outcome(compiled, errlog, summary, errors);
            assert_eq!(got, ok, "{note}");
            assert!(note.contains(msg), "{note}");
        }
    }

    #[test]
    fn script_escapes_names_and_never_runs_project() {
        let dll = Path::new(r"C:\Program Files (x86)\Schneider Electric\Power Operation\v2024\bin\PowerSCADA.Common.dll");
        let ini = Path::new(r"C:\ProgramData\Schneider Electric\Power Operation\v2024\Config\citect.ini");
        let s = compile_script(dll, ini, "O'Brien \u{2019}x\u{2019}");
        assert!(s.contains("CompileProject('O''Brien \u{2019}\u{2019}x\u{2019}\u{2019}')"), "{s}");
        assert!(s.contains("LoadFrom('C:\\Program Files (x86)\\Schneider Electric\\Power Operation\\v2024\\bin\\PowerSCADA.Common.dll')"));
        assert!(s.contains("Set-Location -LiteralPath 'C:\\Program Files (x86)"));
        assert!(s.contains("GetType('PowerSCADA.Common.Projects.ProjectManager')"));
        // Nunca arranca el proyecto (RunProject reescribe Run= del citect.ini).
        assert!(!s.to_lowercase().contains("runproject"), "{s}");
    }

    #[test]
    fn protected_projects() {
        for name in PROTECTED {
            assert!(is_protected(&proj(name, name)), "{name}");
            assert!(is_protected(&proj(&name.to_lowercase(), "Otra")), "{name} por nombre, sin distinguir mayúsculas");
        }
        assert!(is_protected(&proj("Library_Controls", "Library_Controls")));
        assert!(is_protected(&proj("Estilos", "Library_Styles")), "por carpeta");
        assert!(!is_protected(&proj("MCP_Demo", "MCP_Demo")));
        assert!(!is_protected(&proj("Transferencia_Demo", "Transferencia_Demo")));
    }

    #[test]
    fn prefers_power_operation_ini() {
        let mk = |p: &str| CitectIni { path: PathBuf::from(p), sections: Default::default() };
        let po = r"C:\ProgramData\Schneider Electric\Power Operation\v2024\Config\citect.ini";
        let inis = [mk(r"C:\Users\Usuario\Desktop\prueba\citect.ini"), mk(po)];
        assert_eq!(pick_ini(&inis).unwrap(), PathBuf::from(po));
        // Sin el de Power Operation: el primero.
        assert_eq!(pick_ini(&inis[..1]).unwrap(), PathBuf::from(r"C:\Users\Usuario\Desktop\prueba\citect.ini"));
        assert!(pick_ini(&[]).is_none());
    }

    #[test]
    fn errlog_rows_are_counted_and_listed() {
        let dir = TempDir::new("errlog");
        let path = dir.join("errlog.dbf");
        std::fs::write(
            &path,
            build(
                &[("E_TYPE", 8), ("E_REC", 6), ("E_PAGE", 64), ("E_FIELD", 16), ("E_TABLE", 240), ("E_ERROR", 96), ("E_CONTEXT", 240), ("E_TAG", 64)],
                &[
                    (&["ERROR", "12", "Unifilar", "Expresion", "", "Syntax error (E1001)", "Unifilar.ctg:12", ""], false),
                    (&["WARNING", "", "PageMenu", "", "ROLES.DBF", "'Allow RPC' not defined (W1032)", "", "Feeder1\\XCBR1\\Pos"], false),
                    (&["WARNING", "", "", "", "", "aviso borrado", "", ""], true),
                    (&["warning", "", "", "", "", "otro aviso", "", ""], false),
                    (&["INFO", "", "", "", "", "informativo", "", ""], false),
                ],
            ),
        )
        .unwrap();
        let log = read_errlog(&path).unwrap();
        // La fila borrada no cuenta; INFO entra en el total pero no es error ni advertencia; el tipo no distingue mayúsculas.
        assert_eq!((log.errors, log.warnings, log.total), (1, 2, 4));
        assert_eq!(log.items.len(), 4);
        assert_eq!(log.items[0]["type"], "ERROR");
        assert_eq!(log.items[0]["page"], "Unifilar");
        assert_eq!(log.items[0]["field"], "Expresion");
        assert_eq!(log.items[0]["error"], "Syntax error (E1001)");
        assert_eq!(log.items[0]["context"], "Unifilar.ctg:12");
        assert!(log.items[0].get("tag").is_none());
        assert_eq!(log.items[1]["table"], "ROLES.DBF");
        assert_eq!(log.items[1]["tag"], "Feeder1\\XCBR1\\Pos");
    }

    #[test]
    fn ps_process_plumbing() {
        // Prueba del proceso (salida y código) con un script inocuo, sin tocar el SCADA.
        let Ok(exe) = ps32_exe() else {
            eprintln!("OMITIDO ps_process_plumbing: no hay PowerShell de 32 bits");
            return;
        };
        let dir = TempDir::new("ps");
        let file = dir.join("t.ps1");
        std::fs::write(&file, "Write-Output 'hola'\r\n[Console]::Error.WriteLine('err')\r\nexit 3\r\n").unwrap();
        let r = run_script(&exe, &file, Duration::from_secs(60)).unwrap();
        assert_eq!(r.exit, Some(3));
        assert!(!r.timed_out);
        let lines: Vec<&str> = r.output.lines().map(str::trim).collect();
        assert!(lines.contains(&"hola") && lines.contains(&"err"), "{}", r.output);
    }
}
