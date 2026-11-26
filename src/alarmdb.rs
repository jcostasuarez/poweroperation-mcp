//! Base de datos de alarmas vía el driver ODBC "Citect Alarm Driver" (incluido con Plant SCADA /
//! Power Operation). Permite SQL sobre `CiAlarmObject` (alarmas y su estado) y `CDBEventJournal`
//! (journal de eventos / histórico). Puerto por defecto de la base del servidor de alarmas: 5482.

use std::fmt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use odbc_api::buffers::TextRowSet;
use odbc_api::{ColumnDescription, ConnectionOptions, Cursor, Environment, ResultSetMetadata};

use crate::config::Config;
use crate::dbf::Dbf;
use crate::project::{self, Project};
use crate::text::decode;

pub const DRIVER: &str = "Citect Alarm Driver";
const SYSTEM_NAME: &str = "PowerOperationMCP";
const DEFAULT_PORT: u16 = 5482;

pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    pub truncated: bool,
}

/// Servidor de alarmas al que se conecta el driver.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Nombre del sistema en el SystemsXml (el del servidor de alarmas, si se conoce).
    pub system: String,
    pub host: String,
    pub port: u16,
    /// De dónde salieron host y puerto.
    pub source: String,
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{} ({})", self.host, self.port, self.source)
    }
}

/// Servidor de alarmas definido en un proyecto (tablas almsrv y netaddr).
struct ProjectAlarmServer {
    name: String,
    project: String,
    address: Option<String>,
    port: Option<u16>,
}

/// En PO 2024 los servidores locales escuchan solo en `::1`: el loopback IPv4 o el nombre del equipo se
/// pasan a "localhost" (con `IPv6Enabled` el driver prueba IPv4 e IPv6).
fn normalize_host(host: &str) -> String {
    let h = host.trim();
    let l = h.to_lowercase();
    let own = std::env::var("COMPUTERNAME").is_ok_and(|c| c.eq_ignore_ascii_case(h));
    if l == "localhost" || l == "::1" || l.starts_with("127.") || own { "localhost".into() } else { h.to_string() }
}

/// Filas vigentes de una tabla de los proyectos, como (proyecto, campo -> valor).
fn rows(projects: &[Project], table: &str, mut f: impl FnMut(&Project, &dyn Fn(&str) -> String) -> bool) {
    for p in projects {
        let Some(path) = project::table_path(&p.path, table) else { continue };
        let Ok(dbf) = Dbf::open(&path) else { continue };
        let mut more = true;
        dbf.for_each(|_, vals| {
            let get = |name: &str| dbf.field_index(name).and_then(|i| vals.get(i).cloned()).unwrap_or_default();
            more = f(p, &get);
            more
        });
        if !more {
            break;
        }
    }
}

/// Primer servidor de alarmas primario de los proyectos (en orden) con su dirección de red.
fn project_alarm_server(projects: &[Project]) -> Option<ProjectAlarmServer> {
    let mut found: Option<(ProjectAlarmServer, String)> = None;
    rows(projects, "almsrv", |p, get| {
        if get("MODE").eq_ignore_ascii_case("standby") || get("NAME").is_empty() {
            return true;
        }
        let srv = ProjectAlarmServer { name: get("NAME"), project: p.name.clone(), address: None, port: get("DBPORT").trim().parse().ok() };
        found = Some((srv, get("NETADDR")));
        false
    });
    let (mut srv, netaddr) = found?;
    if !netaddr.is_empty() {
        rows(projects, "netaddr", |_, get| {
            if get("NAME").eq_ignore_ascii_case(&netaddr) {
                srv.address = Some(get("ADDRESS")).filter(|a| !a.is_empty());
                return false;
            }
            true
        });
    }
    Some(srv)
}

/// Servidor de alarmas a usar: POWEROP_ALARM_HOST/PORT si están, si no el definido en los proyectos
/// (el que corre y sus incluidos), si no localhost:5482.
pub fn endpoint(cfg: &Config, projects: &[Project]) -> Endpoint {
    let srv = project_alarm_server(projects);
    let mut source = Vec::new();
    let host = match (&cfg.alarm_host, srv.as_ref().and_then(|s| s.address.as_ref())) {
        (Some(h), _) => {
            source.push("host de POWEROP_ALARM_HOST".to_string());
            h.clone()
        }
        (None, Some(a)) => {
            source.push(format!("host de netaddr del proyecto {}", srv.as_ref().map(|s| s.project.as_str()).unwrap_or("")));
            a.clone()
        }
        (None, None) => {
            source.push("host por defecto".into());
            "localhost".into()
        }
    };
    let port = match (cfg.alarm_port, srv.as_ref().and_then(|s| s.port)) {
        (Some(p), _) => {
            source.push("puerto de POWEROP_ALARM_PORT".into());
            p
        }
        (None, Some(p)) => {
            source.push("puerto DBPORT del proyecto".into());
            p
        }
        (None, None) => {
            source.push("puerto por defecto".into());
            DEFAULT_PORT
        }
    };
    let system = match &srv {
        Some(s) => {
            source.push(format!("servidor {} del proyecto {}", s.name, s.project));
            s.name.clone()
        }
        None => SYSTEM_NAME.into(),
    };
    Endpoint { system, host: normalize_host(&host), port, source: source.join(", ") }
}

/// SystemsXml para el driver: el configurado o uno generado con el servidor de alarmas.
fn systems_xml(cfg: &Config, ep: &Endpoint) -> Result<(String, PathBuf)> {
    if let Some(p) = &cfg.alarm_systems_xml {
        let server = std::env::var("POWEROP_ALARM_SYSTEM").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| ep.system.clone());
        return Ok((server, p.clone()));
    }
    let dir = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("poweroperation-mcp");
    std::fs::create_dir_all(&dir).with_context(|| format!("No se pudo crear {}", dir.display()))?;
    let path = dir.join(format!("{SYSTEM_NAME}.auto.xml"));
    let esc = |s: &str| s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;");
    // IPv6Enabled: sin él, el driver no llega a un servidor que escucha solo en ::1 ("The system is offline").
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Systems>
    <System name="{}" type="SCX" enabled="true" visibleInViewX="true" clientLicensing="false" defaultSystemPriority="10" IPv6Enabled="true">
        <Server name="{}" cost="1" port="{}" compress="true" connectTimeout="30000" requestTimeout="120000" disconnectTimeout="30000" disconnectFailedTimeout="500" pollInterval="10" pollTimeout="15000"/>
    </System>
</Systems>"#,
        esc(&ep.system),
        esc(&ep.host),
        ep.port
    );
    // UTF-16 LE con BOM, como lo espera el driver.
    let mut bytes = vec![0xFF, 0xFE];
    for u in xml.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    std::fs::write(&path, bytes).with_context(|| format!("No se pudo escribir {}", path.display()))?;
    Ok((ep.system.clone(), path))
}

/// Ejecuta una consulta (bloqueante: llamar desde spawn_blocking).
pub fn query(cfg: &Config, ep: &Endpoint, sql: &str, limit: usize) -> Result<QueryResult> {
    let (server, xml) = systems_xml(cfg, ep)?;
    let cs = format!("DRIVER={{{DRIVER}}};Server={server};SystemsXml={};", xml.display());
    let env = Environment::new().context("No se pudo inicializar ODBC")?;
    let conn = env.connect_with_connection_string(&cs, ConnectionOptions::default()).with_context(|| {
        format!(
            "No se pudo conectar con el driver ODBC '{DRIVER}' en {ep} — {}. \
             Si el servidor de alarmas está en otro equipo o puerto, indicalo con POWEROP_ALARM_HOST / POWEROP_ALARM_PORT.",
            probe(&ep.host, ep.port)
        )
    })?;
    let mut out = QueryResult { columns: vec![], rows: vec![], truncated: false };
    let Some(mut cursor) = conn.execute(sql, (), None).context("Error ejecutando la consulta")? else {
        return Ok(out);
    };
    // El driver de PO no implementa SQLColAttribute ("Driver not capable"): los nombres salen de
    // SQLDescribeCol y los buffers se dimensionan fijos en vez de consultar el tamaño de cada columna.
    let ncols = cursor.num_result_cols()?.max(0) as u16;
    for c in 1..=ncols {
        let mut d = ColumnDescription::default();
        let name = match cursor.describe_col(c, &mut d) {
            Ok(()) => d.name_to_string().unwrap_or_default(),
            Err(_) => String::new(),
        };
        out.columns.push(if name.is_empty() { format!("COL{c}") } else { name });
    }
    let mut buffers = TextRowSet::from_max_str_lens(256, std::iter::repeat_n(4096, ncols as usize))?;
    let mut rc = cursor.bind_buffer(&mut buffers)?;
    'outer: while let Some(batch) = rc.fetch()? {
        for r in 0..batch.num_rows() {
            if out.rows.len() >= limit {
                out.truncated = true;
                break 'outer;
            }
            out.rows.push((0..batch.num_cols()).map(|c| batch.at(c, r).map(decode)).collect());
        }
    }
    Ok(out)
}

/// Prueba TCP al servidor de alarmas, para distinguir "nada escucha ahí" de otros problemas.
pub fn probe(host: &str, port: u16) -> String {
    use std::net::{TcpStream, ToSocketAddrs};
    let addrs: Vec<_> = match (host, port).to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(e) => return format!("no se pudo resolver {host}: {e}"),
    };
    if addrs.iter().any(|a| TcpStream::connect_timeout(a, std::time::Duration::from_secs(3)).is_ok()) {
        format!("el puerto TCP {host}:{port} está abierto (el servidor responde; revisá que el runtime esté corriendo y el nombre del servidor de alarmas)")
    } else {
        format!("el puerto TCP {host}:{port} está CERRADO (el servidor de alarmas escucha en otro host/puerto o no está corriendo)")
    }
}

/// Drivers ODBC instalados (para diagnóstico).
pub fn drivers() -> Result<Vec<String>> {
    let env = Environment::new()?;
    Ok(env.drivers()?.into_iter().map(|d| d.description).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbf::tests::build;
    use crate::testutil::TempDir;

    #[test]
    fn endpoint_from_project_tables() {
        let tmp = TempDir::new("alm");
        let dir = tmp.path();
        std::fs::write(
            dir.join("almsrv.dbf"),
            build(
                &[("NAME", 16), ("MODE", 16), ("NETADDR", 70), ("DBPORT", 16)],
                &[(&["AlmStby", "Standby", "Net2", ""], false), (&["AlmPri", "", "Net1", "5500"], false)],
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("netaddr.dbf"),
            build(&[("NAME", 16), ("Address", 64)], &[(&["Net1", "127.0.0.1"], false), (&["Net2", "10.0.0.2"], false)]),
        )
        .unwrap();
        let p = Project { name: "Demo".into(), path: dir.to_path_buf(), running: true };
        let ep = endpoint(&crate::testutil::config(dir), std::slice::from_ref(&p));
        assert_eq!((ep.system.as_str(), ep.host.as_str(), ep.port), ("AlmPri", "localhost", 5500));
        assert_eq!(ep.source, "host de netaddr del proyecto Demo, puerto DBPORT del proyecto, servidor AlmPri del proyecto Demo");

        let mut c = crate::testutil::config(dir);
        c.alarm_host = Some("10.0.0.50".into());
        let ep = endpoint(&c, std::slice::from_ref(&p));
        assert_eq!((ep.host.as_str(), ep.port), ("10.0.0.50", 5500));
        assert_eq!(ep.source, "host de POWEROP_ALARM_HOST, puerto DBPORT del proyecto, servidor AlmPri del proyecto Demo");

        let ep = endpoint(&crate::testutil::config(dir), &[]);
        assert_eq!((ep.system.as_str(), ep.host.as_str(), ep.port), (SYSTEM_NAME, "localhost", DEFAULT_PORT));
        assert_eq!(ep.source, "host por defecto, puerto por defecto");
    }

    #[test]
    fn loopback_is_localhost() {
        assert_eq!(normalize_host("127.0.0.1"), "localhost");
        assert_eq!(normalize_host("::1"), "localhost");
        assert_eq!(normalize_host("LOCALHOST"), "localhost");
        assert_eq!(normalize_host("127.0.0.2"), "localhost");
        assert_eq!(normalize_host("10.0.5.150"), "10.0.5.150");
    }
}
