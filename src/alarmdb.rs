//! Base de datos de alarmas vía el driver ODBC "Citect Alarm Driver" (incluido con Plant SCADA /
//! Power Operation). Permite SQL sobre `CiAlarmObject` (alarmas y su estado) y `CDBEventJournal`
//! (journal de eventos / histórico). Puerto por defecto del servidor de alarmas: 5482.

use std::path::PathBuf;

use anyhow::{Context, Result};
use odbc_api::buffers::TextRowSet;
use odbc_api::{ConnectionOptions, Cursor, Environment, ResultSetMetadata};

use crate::config::Config;
use crate::text::decode;

pub const DRIVER: &str = "Citect Alarm Driver";
const SYSTEM_NAME: &str = "PowerOperationMCP";

pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    pub truncated: bool,
}

/// SystemsXml para el driver: el configurado o uno generado con host/puerto.
fn systems_xml(cfg: &Config) -> Result<(String, PathBuf)> {
    if let Some(p) = &cfg.alarm_systems_xml {
        let server = std::env::var("POWEROP_ALARM_SYSTEM").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| SYSTEM_NAME.into());
        return Ok((server, p.clone()));
    }
    let dir = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("poweroperation-mcp");
    std::fs::create_dir_all(&dir).with_context(|| format!("No se pudo crear {}", dir.display()))?;
    let path = dir.join(format!("{SYSTEM_NAME}.auto.xml"));
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Systems>
    <System name="{SYSTEM_NAME}" type="SCX" enabled="true" visibleInViewX="true" clientLicensing="false" defaultSystemPriority="10">
        <Server name="{}" cost="1" port="{}" compress="true" connectTimeout="30000" requestTimeout="120000" disconnectTimeout="30000" disconnectFailedTimeout="500" pollInterval="10" pollTimeout="15000"/>
    </System>
</Systems>"#,
        cfg.alarm_host, cfg.alarm_port
    );
    // UTF-16 LE con BOM, como lo espera el driver.
    let mut bytes = vec![0xFF, 0xFE];
    for u in xml.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    std::fs::write(&path, bytes).with_context(|| format!("No se pudo escribir {}", path.display()))?;
    Ok((SYSTEM_NAME.into(), path))
}

/// Ejecuta una consulta (bloqueante: llamar desde spawn_blocking).
pub fn query(cfg: &Config, sql: &str, limit: usize) -> Result<QueryResult> {
    let (server, xml) = systems_xml(cfg)?;
    let cs = format!("DRIVER={{{DRIVER}}};Server={server};SystemsXml={};", xml.display());
    let env = Environment::new().context("No se pudo inicializar ODBC")?;
    let conn = env.connect_with_connection_string(&cs, ConnectionOptions::default()).with_context(|| {
        format!(
            "No se pudo conectar con el driver ODBC '{DRIVER}' ({}:{}) — {}. \
             Ajustá POWEROP_ALARM_HOST / POWEROP_ALARM_PORT al servidor de alarmas del proyecto.",
            cfg.alarm_host,
            cfg.alarm_port,
            probe(&cfg.alarm_host, cfg.alarm_port)
        )
    })?;
    let mut out = QueryResult { columns: vec![], rows: vec![], truncated: false };
    let Some(mut cursor) = conn.execute(sql, (), None).context("Error ejecutando la consulta")? else {
        return Ok(out);
    };
    out.columns = cursor.column_names()?.collect::<Result<Vec<_>, _>>()?;
    let mut buffers = TextRowSet::for_cursor(256, &mut cursor, Some(4096))?;
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
        format!("el puerto TCP {host}:{port} está abierto (el servidor responde; el problema es otro)")
    } else {
        format!("el puerto TCP {host}:{port} está CERRADO (el servidor de alarmas escucha en otro host/puerto o no está corriendo)")
    }
}

/// Drivers ODBC instalados (para diagnóstico).
pub fn drivers() -> Result<Vec<String>> {
    let env = Environment::new()?;
    Ok(env.drivers()?.into_iter().map(|d| d.description).collect())
}
