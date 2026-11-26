mod alarmdb;
mod cicode;
mod config;
mod ctapi;
mod dbf;
mod discover;
mod project;
mod server;
mod text;
mod tools;

use std::sync::Arc;

use anyhow::Result;
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

const HELP: &str = "\
poweroperation-mcp — servidor MCP (stdio) para EcoStruxure Power Operation

Uso:
  poweroperation-mcp.exe             arranca el servidor MCP (lo lanza el agente, no se usa a mano)
  poweroperation-mcp.exe --doctor    diagnóstico: CtApi.dll, conexión al runtime, proyectos, ODBC
  poweroperation-mcp.exe --version

Configuración por variables de entorno o archivo .env junto al exe (ver .env.example).";

#[tokio::main]
async fn main() -> Result<()> {
    let arg = std::env::args().nth(1).unwrap_or_default();
    match arg.as_str() {
        "--version" | "-V" => {
            println!("poweroperation-mcp {} ({})", env!("CARGO_PKG_VERSION"), discover::this_arch());
            return Ok(());
        }
        "--help" | "-h" | "/?" => {
            println!("{HELP}");
            return Ok(());
        }
        "--doctor" => return doctor().await,
        _ => {}
    }

    // stdout es el canal MCP: los logs van a stderr.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("POWEROP_MCP_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let cfg = config::Config::load();
    tracing::info!(allow_write = cfg.allow_write, host = ?cfg.host, "iniciando poweroperation-mcp");
    let service = server::PowerOpServer::new(cfg).serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Diagnóstico por consola para verificar la instalación antes de conectar el agente.
async fn doctor() -> Result<()> {
    let cfg = Arc::new(config::Config::load());
    println!("== poweroperation-mcp {} ({}) ==\n", env!("CARGO_PKG_VERSION"), discover::this_arch());
    println!("-- Configuración --\n{}\n", cfg.describe());

    println!("-- CtApi.dll --");
    let dlls = ctapi::find_dlls(&cfg);
    if dlls.is_empty() {
        println!("NO encontrado. Indicá la carpeta con POWEROP_CTAPI (p. ej. ...\\Power Operation\\v2022\\Bin\\Bin (x64)).");
    }
    for d in &dlls {
        let ok = if d.arch == Some(discover::this_arch()) { "compatible" } else { "otra arquitectura" };
        println!("  {} [{}] {ok}", d.path.display(), d.arch.unwrap_or("?"));
    }

    println!("\n-- Conexión al runtime (ctOpen + Cicode Time(1)) --");
    let ct = ctapi::CtApi::new(cfg.clone());
    match ct.call(|s| s.cicode("Time(1)", 0)).await {
        Ok(t) => println!("OK. Hora del SCADA: {t}"),
        Err(e) => println!("ERROR: {e:#}"),
    }
    if let Some(p) = ct.loaded_dll() {
        println!("DLL usado: {}", p.display());
    }

    println!("\n-- Proyecto y parámetros del runtime (Cicode) --");
    for cmd in [
        "ProjectCurrentGet()",
        "ProjectInfo(0)",
        "ProjectInfo(1)",
        "ProjectInfo(2)",
        r#"ParameterGet("CtEdit", "Run", "")"#,
        r#"ParameterGet("CtEdit", "User", "")"#,
        r#"ParameterGet("CtEdit", "Config", "")"#,
        r#"ParameterGet("Alarm", "Server", "")"#,
    ] {
        match ct.call(move |s| s.cicode(cmd, 0)).await {
            Ok(v) => println!("  {cmd} = {v:?}"),
            Err(e) => println!("  {cmd} → ERROR {e:#}"),
        }
    }
    let running = ct.call(|s| Ok(s.running_project())).await.ok().flatten();
    println!("Proyecto en runtime: {}", running.as_deref().unwrap_or("(no disponible)"));

    println!("\n-- citect.ini y proyectos --");
    let cat = project::discover(&cfg);
    if cat.ignored_ini_copies > 0 {
        println!("  ({} citect.ini dentro de carpetas de proyecto ignorados: son copias)", cat.ignored_ini_copies);
    }
    for ini in &cat.inis {
        println!(
            "  {}\n     Run={}  User={}  [CtAPI] Remote={}",
            ini.path.display(),
            ini.get("CtEdit", "Run").unwrap_or("-"),
            ini.get("CtEdit", "User").unwrap_or("-"),
            ini.get("CtAPI", "Remote").unwrap_or("-")
        );
    }
    if cat.projects.is_empty() {
        println!("  Ningún proyecto encontrado (POWEROP_PROJECTS_DIR para indicar la carpeta User).");
    }
    for p in &cat.projects {
        let run = p.running || running.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(&p.name));
        println!("  {}{}  {}", p.name, if run { " [RUN]" } else { "" }, p.path.display());
    }

    println!("\n-- Servidores y direcciones configurados en los proyectos (tablas con campo *PORT* o de red) --");
    print_server_tables(&cat.projects);

    println!("\n-- Puertos TCP en escucha (sin procesos del sistema) --");
    print_listening_ports();

    println!("\n-- ODBC (alarm_sql) --");
    match alarmdb::drivers() {
        Ok(d) => {
            let has = d.iter().any(|x| x.eq_ignore_ascii_case(alarmdb::DRIVER));
            println!("Driver '{}': {}", alarmdb::DRIVER, if has { "instalado" } else { "NO instalado" });
            if has {
                match alarmdb::query(&cfg, "select AlarmSource, AlarmState from CiAlarmObject", 3) {
                    Ok(r) => println!("Consulta de prueba OK ({} filas de muestra)", r.rows.len()),
                    Err(e) => println!("Consulta de prueba ERROR: {e:#}"),
                }
            }
        }
        Err(e) => println!("ERROR listando drivers: {e:#}"),
    }
    println!("\nListo. Si todo da OK, registrá el exe como servidor MCP en el agente (ver README).");
    Ok(())
}

/// Tablas de configuración de servidores (alarmas, tendencias, I/O, red...): las que tienen un campo
/// con "PORT" en el nombre o se llaman como direcciones de red. Muestra pocas filas de cada una.
fn print_server_tables(projects: &[project::Project]) {
    const MAX_ROWS: usize = 8;
    for p in projects {
        for path in project::files_with_ext(&p.path, "dbf") {
            let table = path.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
            let Ok(d) = dbf::Dbf::open(&path) else { continue };
            let has_port = d.fields.iter().any(|f| f.name.to_uppercase().contains("PORT"));
            let is_net = table.contains("net") || table.contains("addr") || table.contains("srv") || table.contains("server");
            if !(has_port || is_net) {
                continue;
            }
            let mut rows = Vec::new();
            let mut total = 0;
            d.for_each(|_, vals| {
                total += 1;
                if rows.len() < MAX_ROWS {
                    let cells: Vec<String> = d
                        .fields
                        .iter()
                        .zip(&vals)
                        .filter(|(_, v)| !v.is_empty())
                        .map(|(f, v)| format!("{}={}", f.name, text::truncate(v, 60)))
                        .collect();
                    rows.push(cells.join("  "));
                }
                true
            });
            if total == 0 {
                continue;
            }
            println!("  {}/{} ({} registros)", p.name, table, total);
            for r in rows {
                println!("     {r}");
            }
        }
    }
}

/// Puertos TCP en escucha con el proceso dueño (vía PowerShell Get-NetTCPConnection).
fn print_listening_ports() {
    const SYSTEM: &[&str] = &["system", "svchost", "lsass", "wininit", "services", "spoolsv", "idle", ""];
    let ps = "Get-NetTCPConnection -State Listen | ForEach-Object { \
              $p = Get-Process -Id $_.OwningProcess -ErrorAction SilentlyContinue; \
              '{0}|{1}|{2}' -f $p.ProcessName, $_.LocalAddress, $_.LocalPort }";
    match std::process::Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", ps]).output() {
        Ok(o) => {
            let mut lines: Vec<(String, String, u32)> = String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| {
                    let mut it = l.trim().splitn(3, '|');
                    Some((it.next()?.to_string(), it.next()?.to_string(), it.next()?.trim().parse().ok()?))
                })
                .filter(|(p, _, _)| !SYSTEM.contains(&p.to_lowercase().as_str()))
                .collect();
            lines.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()).then(a.2.cmp(&b.2)));
            lines.dedup();
            if lines.is_empty() {
                println!("  (sin datos: {})", String::from_utf8_lossy(&o.stderr).trim());
            }
            for (p, addr, port) in lines {
                println!("  {p:<32} {addr:<16} {port}");
            }
        }
        Err(e) => println!("  No se pudo ejecutar PowerShell: {e}"),
    }
}
