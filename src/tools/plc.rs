//! `po_plc_read`: lectura directa de un PLC por Modbus TCP (puente MODBUS). Solo lectura: ver `modbus.rs`.

use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::ModbusTarget;
use crate::modbus::{self, Area};
use crate::server::{PowerOpServer, ToolResult, blocking, err};
use crate::text::truncate;

/// Máximo de elementos por llamada (rango) y de direcciones sueltas.
const MAX_RANGE: u32 = 10_000;
const MAX_ADDRESSES: usize = 200;

#[derive(Deserialize, JsonSchema)]
pub struct PlcReadParams {
    /// Nombre del PLC de POWEROP_MODBUS (p. ej. "PLC1"). Si hay uno solo, se puede omitir.
    #[serde(default)]
    pub plc: Option<String>,
    /// Direcciones sueltas en notación Schneider: %MWn (holding, FC3), %IWn (input register, FC4), %Mn (coil, FC1),
    /// %In (entrada discreta, FC2). Máximo 200. No combinar con area/start/count.
    #[serde(default)]
    pub addresses: Option<Vec<String>>,
    /// Rango: área "holding" (%MW), "input" (%IW), "coils" (%M) o "discrete" (%I).
    #[serde(default)]
    pub area: Option<String>,
    /// Rango: primera dirección (número, como en %MW4000 → 4000).
    #[serde(default)]
    pub start: Option<u32>,
    /// Rango: cantidad de elementos (por defecto 1, máximo 10000; se parte en pedidos válidos).
    #[serde(default)]
    pub count: Option<u32>,
    /// Rango: devolver solo los valores distintos de cero (para explorar qué direcciones usa el PLC).
    #[serde(default)]
    pub nonzero_only: bool,
}

fn value_json(area: Area, addr: u32, v: u16) -> Value {
    let mut o = json!({"addr": format!("{}{addr}", area.prefix()), "value": v});
    if !area.is_bits() && v >= 0x8000 {
        o["signed"] = json!(v as i16);
    }
    o
}

impl PowerOpServer {
    fn plc_target(&self, name: Option<&str>) -> Result<ModbusTarget, String> {
        let list = &self.cfg.modbus;
        match name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => list.iter().find(|t| t.name.eq_ignore_ascii_case(n)).cloned().ok_or_else(|| {
                format!("No hay PLC '{n}' en POWEROP_MODBUS. Configurados: {}", list.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", "))
            }),
            None => match list.as_slice() {
                [only] => Ok(only.clone()),
                [] => Err("No hay PLCs configurados (POWEROP_MODBUS).".into()),
                many => Err(format!("Indicá 'plc'. Configurados: {}", many.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", "))),
            },
        }
    }
}

#[tool_router(router = plc_router, vis = "pub(crate)")]
impl PowerOpServer {
    #[tool(name = "po_plc_read", description = "Lee un PLC por Modbus TCP, SOLO LECTURA (FC1-FC4; el MCP no tiene funciones de escritura Modbus). Dos modos: `addresses` = direcciones sueltas en notación Schneider (%MW4000, %M4005, %IW10, %I3), o rango con area (holding/input/coils/discrete) + start + count, opcionalmente nonzero_only=true para explorar qué direcciones usa el PLC. Los PLCs salen de POWEROP_MODBUS (nombre=host:puerto/unidad). Sirve para mapear señales reales contra los tags del proyecto; no cambia nada en el PLC ni en el proyecto.")]
    async fn po_plc_read(&self, Parameters(p): Parameters<PlcReadParams>) -> ToolResult {
        let target = self.plc_target(p.plc.as_deref())?;
        let timeout = Duration::from_secs(3);
        let label = target.to_string();
        if let Some(addrs) = p.addresses {
            if p.area.is_some() || p.start.is_some() || p.count.is_some() {
                return Err("Usá addresses o area/start/count, no los dos.".into());
            }
            if addrs.is_empty() || addrs.len() > MAX_ADDRESSES {
                return Err(format!("addresses debe tener entre 1 y {MAX_ADDRESSES} direcciones."));
            }
            let parsed: Vec<(Area, u16)> = addrs.iter().map(|a| modbus::parse_address(a)).collect::<anyhow::Result<_>>().map_err(err)?;
            let t = target.clone();
            let values = blocking(Duration::from_secs(60), move || {
                let mut s = modbus::connect(&t, timeout)?;
                let mut tid: u16 = 1;
                let mut out = Vec::with_capacity(parsed.len());
                for (area, n) in parsed {
                    tid = tid.wrapping_add(1);
                    out.push(match modbus::read_once(&mut s, t.unit, tid, area, n, 1) {
                        Ok(v) => value_json(area, u32::from(n), v[0]),
                        Err(e) => {
                            let o = json!({"addr": format!("{}{n}", area.prefix()), "error": format!("{e:#}")});
                            // Tras un error de E/S la conexión puede quedar desfasada: reconectar.
                            if !format!("{e:#}").contains("excepción Modbus") {
                                s = modbus::connect(&t, timeout)?;
                            }
                            o
                        }
                    });
                }
                Ok(out)
            })
            .await
            .map_err(err)?;
            let s = serde_json::to_string_pretty(&json!({"plc": label, "values": values})).map_err(|e| e.to_string())?;
            return Ok(truncate(&s, 60_000));
        }
        let area = Area::parse(p.area.as_deref().ok_or("Indicá addresses, o area + start (+ count).")?).map_err(err)?;
        let start = p.start.ok_or("Indicá start (primera dirección del rango).")?;
        let count = p.count.unwrap_or(1);
        if count == 0 || count > MAX_RANGE {
            return Err(format!("count debe estar entre 1 y {MAX_RANGE}."));
        }
        if start > 65_535 || start + count > 65_536 {
            return Err("El rango pasa de la dirección 65535.".into());
        }
        let t = target.clone();
        let raw = blocking(Duration::from_secs(120), move || modbus::read_range(&t, area, start as u16, count, timeout)).await.map_err(err)?;
        let values: Vec<Value> = raw
            .iter()
            .enumerate()
            .filter(|(_, v)| !p.nonzero_only || **v != 0)
            .map(|(i, v)| value_json(area, start + i as u32, *v))
            .collect();
        let s = serde_json::to_string_pretty(&json!({
            "plc": label,
            "area": area.prefix(),
            "start": start,
            "read": raw.len(),
            "nonzero": raw.iter().filter(|v| **v != 0).count(),
            "values": values,
        }))
        .map_err(|e| e.to_string())?;
        Ok(truncate(&s, 80_000))
    }
}
