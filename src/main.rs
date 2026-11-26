mod alarmdb;
mod argerr;
mod bridges;
mod cicode;
mod config;
mod ctapi;
mod ctg;
mod dbf;
mod discover;
mod ndx;
mod oneline;
mod gb;
mod modbus;
mod png;
mod procs;
mod project;
mod server;
mod text;
#[cfg(test)]
mod testutil;
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
    let server = server::PowerOpServer::new(cfg);
    // Primer sondeo de todos los puentes: se espera un poco para que la primera lista de herramientas ya los
    // refleje; lo que tarde más (p. ej. buscar el compilador de Studio) se publica con tools/list_changed.
    let first = tokio::spawn({
        let s = server.clone();
        async move { s.probe(&bridges::Bridge::ALL).await }
    });
    let _ = tokio::time::timeout(std::time::Duration::from_secs(8), first).await;
    let service = server.clone().serve(rmcp::transport::stdio()).await?;
    server.bridges.set_peer(service.peer().clone());
    tokio::spawn(server.clone().probe_loop());
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
    for m in &cat.masters {
        println!("  Registro de proyectos: {}", m.join("MASTER.DBF").display());
    }
    for p in &cat.projects {
        let run = p.running || running.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(&p.name));
        println!("  {}{}  {}", p.name, if run { " [RUN]" } else { "" }, p.path.display());
    }
    if !cat.unregistered.is_empty() {
        println!("  Carpetas con forma de proyecto NO registradas en MASTER.DBF (ignoradas):");
        for d in &cat.unregistered {
            println!("     {}", d.display());
        }
    }

    let base = running.as_deref().or(cfg.default_project.as_deref()).and_then(|n| cat.resolve(n).ok());
    let scope = base.as_ref().map(|p| cat.with_includes(p).0).unwrap_or_default();
    match &base {
        Some(p) => println!("\n-- Servidores configurados: {} y sus incluidos --", p.name),
        None => println!("\n-- Servidores configurados: todos los proyectos (no hay proyecto en runtime ni POWEROP_PROJECT) --"),
    }
    print_servers(if base.is_some() { &scope } else { &cat.projects });

    println!("\n-- Puertos TCP en escucha (sin procesos del sistema) --");
    print_listening_ports();

    println!("\n-- ODBC (po_alarm_sql) --");
    let ep = alarmdb::endpoint(&cfg, &scope);
    println!("Servidor de alarmas: {ep}");
    match alarmdb::drivers() {
        Ok(d) => {
            let has = d.iter().any(|x| x.eq_ignore_ascii_case(alarmdb::DRIVER));
            println!("Driver '{}': {}", alarmdb::DRIVER, if has { "instalado" } else { "NO instalado" });
            if has {
                match alarmdb::query(&cfg, &ep, "select AlarmSource, AlarmState from CiAlarmObject", 3) {
                    Ok(r) => println!("Consulta de prueba OK ({} filas de muestra)", r.rows.len()),
                    Err(e) => println!("Consulta de prueba ERROR: {e:#}"),
                }
            }
        }
        Err(e) => println!("ERROR listando drivers: {e:#}"),
    }

    println!("\n-- Graphics Builder (gb_*) --");
    match gb::ctdraw_exe() {
        Some(exe) => println!("Ejecutable: {}", exe.display()),
        None => println!("No se encontró CtDraw32.exe en la instalación (po_gb_status con launch=true no podrá abrirlo)"),
    }
    match gb::running_pids().as_slice() {
        [] => println!("No está abierto: las herramientas po_gb_* lo abren o se conectan con po_gb_status (po_project_graphics no lo necesita)."),
        pids => println!("Abierto (PID {})", pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")),
    }
    println!("\nListo. Si todo da OK, registrá el exe como servidor MCP en el agente (ver README).");
    Ok(())
}

/// Registro de una tabla DBF de proyecto: proyecto de origen y campos (nombre en mayúsculas → valor).
struct Row {
    project: String,
    fields: std::collections::HashMap<String, String>,
}

impl Row {
    fn get(&self, field: &str) -> &str {
        self.fields.get(field).map(String::as_str).unwrap_or("")
    }
}

/// Registros no borrados de `table` en todos los proyectos dados.
fn table_rows(projects: &[project::Project], table: &str) -> Vec<Row> {
    let mut out = Vec::new();
    for p in projects {
        let Some(d) = project::table_path(&p.path, table).and_then(|t| dbf::Dbf::open(&t).ok()) else { continue };
        d.for_each(|_, vals| {
            let fields = d.fields.iter().zip(vals).map(|(f, v)| (f.name.to_uppercase(), v)).collect();
            out.push(Row { project: p.name.clone(), fields });
            true
        });
    }
    out
}

/// Clusters, direcciones de red y servidores (alarmas, tendencias, reportes, I/O) de los proyectos,
/// con la dirección de red resuelta y los puertos por defecto cuando el campo está vacío.
fn print_servers(projects: &[project::Project]) {
    let clusters = table_rows(projects, "cluster");
    let netaddrs = table_rows(projects, "netaddr");
    let computers = table_rows(projects, "computer");
    let port = |v: &str, default: Option<u16>| match (v.trim(), default) {
        ("", Some(d)) => format!("{d} (def.)"),
        ("", None) => "-".into(),
        (v, _) => v.to_string(),
    };

    let list: Vec<String> = clusters.iter().map(|c| format!("{} [{}]", c.get("NAME"), c.project)).collect();
    println!("  Clusters: {}", if list.is_empty() { "(ninguno)".into() } else { list.join(", ") });

    println!("  Direcciones de red (netaddr):");
    if netaddrs.is_empty() {
        println!("     (ninguna)");
    }
    for n in &netaddrs {
        let comp = n.get("COMPUTER");
        let dns = computers.iter().find(|c| c.get("NAME").eq_ignore_ascii_case(comp)).map(|c| c.get("DNSNAME")).unwrap_or("");
        let comp = match (comp, dns) {
            ("", _) => String::new(),
            (c, "") => format!("  equipo {c}"),
            (c, d) => format!("  equipo {c} ({d})"),
        };
        println!("     {:<20} {:<16}{comp}  [{}]", n.get("NAME"), n.get("ADDRESS"), n.project);
    }

    // (tabla, rótulo, puerto por defecto, puertos extra (campo, rótulo, defecto))
    let kinds: [(&str, &str, u16, &[(&str, &str, Option<u16>)]); 4] = [
        ("almsrv", "Alarmas", 2080, &[("DBPORT", "BD", Some(5482)), ("PUBPORT", "pub", None)]),
        ("trnsrv", "Tendencias", 2085, &[("PFPORT", "PF", None)]),
        ("repsrv", "Reportes", 2084, &[("PFPORT", "PF", None)]),
        ("scanner", "I/O", 2082, &[("PEERPORT", "peer", None)]),
    ];
    println!("  Servidores:");
    let mut any = false;
    for (table, label, default, extra) in kinds {
        for s in table_rows(projects, table) {
            any = true;
            let na = s.get("NETADDR");
            let addr = match netaddrs.iter().find(|n| n.get("NAME").eq_ignore_ascii_case(na)) {
                _ if na.is_empty() => "(sin NETADDR)".to_string(),
                Some(n) => format!("{na} → {}", n.get("ADDRESS")),
                None => format!("{na} (no está en netaddr)"),
            };
            let mode = if s.get("MODE").is_empty() { "Primary" } else { s.get("MODE") };
            let mut ports = format!("puerto {}", port(s.get("PORT"), Some(default)));
            for (f, l, d) in extra {
                if d.is_some() || !s.get(f).is_empty() {
                    ports.push_str(&format!("  {l} {}", port(s.get(f), *d)));
                }
            }
            println!(
                "     {label:<10} {:<16} {mode:<8} cluster {:<8} {addr}  {ports}  [{}]",
                s.get("NAME"),
                if s.get("CLUSTER").is_empty() { "-" } else { s.get("CLUSTER") },
                s.project
            );
        }
    }
    if !any {
        println!("     (ninguno)");
    }

    const MAX_UNITS: usize = 15;
    let units = table_rows(projects, "units");
    if !units.is_empty() {
        println!("  Dispositivos de I/O (units, {}):", units.len());
        for u in units.iter().take(MAX_UNITS) {
            let mem = if u.get("MEMORY").eq_ignore_ascii_case("TRUE") { "  memoria" } else { "" };
            println!(
                "     {:<20} servidor {:<14} {} {}{mem}  [{}]",
                u.get("NAME"),
                u.get("SERVER"),
                u.get("PROTOCOL"),
                text::truncate(u.get("PORT"), 30),
                u.project
            );
        }
        if units.len() > MAX_UNITS {
            println!("     ... y {} más (po_project_query table=units)", units.len() - MAX_UNITS);
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
