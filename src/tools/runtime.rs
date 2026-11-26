//! Herramientas de runtime: CtAPI (tags, Cicode, browse, tendencias, alarmas) y ODBC de alarmas.

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::alarmdb;
use crate::cicode;
use crate::ctapi::{self, FindResult};
use crate::discover;
use crate::server::{PowerOpServer, ToolResult, blocking, err};
use crate::text::truncate;

fn pretty(v: &Value, max: usize) -> ToolResult {
    let s = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    Ok(truncate(&s, max))
}

/// Literal de cadena Cicode (^ es el carácter de escape).
fn cicode_str(s: &str) -> String {
    format!("\"{}\"", s.replace('^', "^^").replace('"', "^\""))
}

/// Resultado de búsqueda en JSON: quita las propiedades que no existen (null en todas las filas).
fn find_json(r: FindResult, props: &[String]) -> Value {
    let unavailable: Vec<&String> = if r.rows.is_empty() {
        vec![]
    } else {
        props.iter().filter(|p| r.rows.iter().all(|row| row.get(p.as_str()).is_none_or(Value::is_null))).collect()
    };
    let rows: Vec<Value> = r
        .rows
        .into_iter()
        .map(|mut row| {
            if let Some(o) = row.as_object_mut() {
                o.retain(|k, _| !unavailable.contains(&k));
            }
            row
        })
        .collect();
    let mut out = Map::new();
    out.insert("count".into(), json!(rows.len()));
    if r.truncated {
        out.insert("truncated".into(), json!(true));
    }
    if let Some(n) = r.note {
        out.insert("note".into(), json!(n));
    }
    if !unavailable.is_empty() {
        out.insert("unavailable_properties".into(), json!(unavailable));
    }
    out.insert("rows".into(), Value::Array(rows));
    Value::Object(out)
}

fn props_or(given: Option<Vec<String>>, default: &[&str]) -> Vec<String> {
    match given {
        Some(v) if !v.is_empty() => v.into_iter().map(|p| p.trim().to_uppercase()).collect(),
        _ => default.iter().map(|s| s.to_string()).collect(),
    }
}

/// Fecha/hora: "now", RFC 3339 (con Z u offset) o local "YYYY-MM-DD HH:MM[:SS]" / "YYYY-MM-DDTHH:MM[:SS]".
fn parse_time(s: &str) -> anyhow::Result<DateTime<Local>> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("now") || s.eq_ignore_ascii_case("ahora") {
        return Ok(Local::now());
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Local));
    }
    for f in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, f) {
            return Local.from_local_datetime(&n).earliest().ok_or_else(|| anyhow::anyhow!("Hora local inexistente: {s}"));
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let n = d.and_hms_opt(0, 0, 0).expect("medianoche válida");
        return Local.from_local_datetime(&n).earliest().ok_or_else(|| anyhow::anyhow!("Hora local inexistente: {s}"));
    }
    anyhow::bail!("Fecha no reconocida: '{s}'. Usá 'now', '2026-10-14 08:00' (hora local) o RFC 3339 ('2026-10-14T11:00:00Z').")
}

fn quality_desc(q: i64) -> &'static str {
    let good = (q >> 4) & 0xF == 1;
    match (q & 0xF, good) {
        (_, false) => "bad",
        (1, true) => "good/interp",
        (2 | 3, true) => "good/raw",
        _ => "good",
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct StatusParams {
    /// Cerrar y volver a abrir la conexión CtAPI.
    #[serde(default)]
    pub reconnect: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct TagReadParams {
    /// Nombres de tags, p. ej. ["Feeder1\\MMXU1\\TotW"]. Se puede usar Tag.Elemento y Tag[índice].
    pub tags: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct TagWriteParams {
    /// Nombre del tag.
    pub tag: String,
    /// Valor como texto (CtAPI escribe en unidades de ingeniería).
    pub value: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct TagBrowseParams {
    /// Filtro CtAPI, p. ej. "TAG=Feeder1*" (comodines * y ?). Vacío = todos (usar limit).
    #[serde(default)]
    pub filter: Option<String>,
    /// Propiedades a devolver (por defecto TAG, TYPE, UNIT, ADDR, COMMENT, ENG_UNITS, EQUIP, ITEM, CLUSTER).
    #[serde(default)]
    pub properties: Option<Vec<String>>,
    /// Cluster (vacío = el de POWEROP_CLUSTER o el único activo).
    #[serde(default)]
    pub cluster: Option<String>,
    /// Máximo de registros (por defecto 200).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Leer también el valor actual de cada tag encontrado (más lento).
    #[serde(default)]
    pub with_values: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct FindParams {
    /// Tabla o consulta: "Tag", "Alarm", "Trend", o una consulta "TRNQUERY,..." / "ALMQUERY,...".
    pub table: String,
    /// Filtro "CAMPO=valor" (comodines * ?). Vacío para consultas TRNQUERY/ALMQUERY.
    #[serde(default)]
    pub filter: Option<String>,
    /// Propiedades a leer de cada registro (p. ej. ["TAG","COMMENT"]).
    pub properties: Vec<String>,
    #[serde(default)]
    pub cluster: Option<String>,
    /// Máximo de registros (por defecto 500).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct CicodeParams {
    /// Expresión o llamada Cicode, p. ej. "TagRead(\"Feeder1\\MMXU1\\TotW\")". Las cadenas van entre comillas dobles.
    pub command: String,
    /// Ventana lógica de Citect donde ejecutar (por defecto 0).
    #[serde(default)]
    pub window: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct TrendParams {
    /// Tag de tendencia (nombre del trend tag, no necesariamente el de la variable).
    pub tag: String,
    /// Fin: "now" (por defecto), "2026-10-14 08:00" (hora local) o RFC 3339.
    #[serde(default)]
    pub end: Option<String>,
    /// Inicio (mismo formato). Si se omite se usa minutes_back.
    #[serde(default)]
    pub start: Option<String>,
    /// Minutos hacia atrás desde end cuando no hay start (por defecto 60).
    #[serde(default)]
    pub minutes_back: Option<f64>,
    /// Segundos entre muestras (por defecto: el rango dividido en ~300 muestras).
    #[serde(default)]
    pub period_seconds: Option<f64>,
    /// Máximo de muestras (por defecto 2000, tope 20000).
    #[serde(default)]
    pub max_samples: Option<usize>,
    /// DisplayMode de TrnQuery (bits: orden, condensado, estiramiento...). Por defecto 0 = antiguo→nuevo, promedio, escalón.
    #[serde(default)]
    pub display_mode: Option<u32>,
    /// DataMode: 0 = una muestra por período, interpolando huecos (por defecto); 1 = timestamps reales sin interpolar.
    #[serde(default)]
    pub data_mode: Option<u32>,
    #[serde(default)]
    pub cluster: Option<String>,
    /// "json" (por defecto) o "csv".
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct AlarmListParams {
    /// Filtro CtAPI sobre la tabla Alarm, p. ej. "TAG=Feeder1*" o "CATEGORY=1".
    #[serde(default)]
    pub filter: Option<String>,
    /// Campos (por defecto TAG, NAME, DESC, CATEGORY, PRIORITY, STATE, AREA, ONTIME, OFFTIME, ACKTIME, EQUIPMENT, COMMENT).
    #[serde(default)]
    pub fields: Option<Vec<String>>,
    #[serde(default)]
    pub cluster: Option<String>,
    /// Máximo de registros (por defecto 200).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct AlarmAckParams {
    /// Tag de la alarma a reconocer.
    pub tag: String,
    #[serde(default)]
    pub cluster: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct AlarmSqlParams {
    /// Consulta SQL al driver "Citect Alarm Driver". Tablas: CiAlarmObject (alarmas/estado), CDBEventJournal (eventos).
    /// Fechas en UTC con sintaxis ODBC: {ts '2026-10-14 08:00:00'}.
    pub sql: String,
    /// Máximo de filas (por defecto 500).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[tool_router(router = runtime_router, vis = "pub(crate)")]
impl PowerOpServer {
    #[tool(description = "Diagnóstico: CtApi.dll encontrado (ruta y arquitectura), conexión al runtime, citect.ini, modo de escritura y drivers ODBC de alarmas. Úsalo primero.")]
    async fn runtime_status(&self, Parameters(p): Parameters<StatusParams>) -> ToolResult {
        if p.reconnect {
            self.ct.disconnect().await;
        }
        let mut out = vec![
            format!("poweroperation-mcp {} ({})", env!("CARGO_PKG_VERSION"), discover::this_arch()),
            format!("Escrituras: {}", if self.cfg.allow_write { "HABILITADAS" } else { "deshabilitadas (solo lectura)" }),
        ];
        let cfg = self.cfg.clone();
        let dlls = blocking(std::time::Duration::from_secs(60), move || Ok(ctapi::find_dlls(&cfg))).await.unwrap_or_default();
        if dlls.is_empty() {
            out.push("CtApi.dll: NO encontrado (POWEROP_CTAPI para indicar la ruta)".into());
        }
        for d in dlls.iter().take(6) {
            out.push(format!("CtApi.dll candidato: {} [{}]", d.path.display(), d.arch.unwrap_or("?")));
        }
        match self.ct.call(|s| s.cicode("Time(1)", 0)).await {
            Ok(t) => out.push(format!("Conexión CtAPI: OK (hora del SCADA: {t})")),
            Err(e) => out.push(format!("Conexión CtAPI: ERROR — {}", err(e))),
        }
        if let Some(p) = self.ct.loaded_dll() {
            out.push(format!("DLL cargado: {}", p.display()));
        }
        out.push(format!(
            "Proyecto en runtime: {}",
            self.runtime_project(true).await.unwrap_or_else(|| "(no disponible)".into())
        ));
        if let Ok(cat) = self.catalog(false).await {
            for ini in &cat.inis {
                out.push(format!(
                    "citect.ini: {} | Run={} | User={} | [CtAPI] Remote={}",
                    ini.path.display(),
                    ini.get("CtEdit", "Run").unwrap_or("-"),
                    ini.get("CtEdit", "User").unwrap_or("-"),
                    ini.get("CtAPI", "Remote").unwrap_or("-")
                ));
            }
        }
        match blocking(std::time::Duration::from_secs(20), alarmdb::drivers).await {
            Ok(d) => {
                let has = d.iter().any(|x| x.eq_ignore_ascii_case(alarmdb::DRIVER));
                out.push(format!(
                    "Driver ODBC '{}': {} (alarm_sql → {}:{})",
                    alarmdb::DRIVER,
                    if has { "instalado" } else { "NO instalado (alarm_sql no funcionará)" },
                    self.cfg.alarm_host,
                    self.cfg.alarm_port
                ));
            }
            Err(e) => out.push(format!("ODBC: {}", err(e))),
        }
        Ok(out.join("\n"))
    }

    #[tool(description = "Lee el valor actual de uno o varios tags (en unidades de ingeniería, como texto).")]
    async fn tag_read(&self, Parameters(p): Parameters<TagReadParams>) -> ToolResult {
        if p.tags.is_empty() {
            return Err("Indicá al menos un tag".into());
        }
        let r = self
            .ct
            .call(move |s| {
                Ok(p.tags
                    .iter()
                    .map(|t| match s.tag_read(t) {
                        Ok(v) => json!({"tag": t, "value": v}),
                        Err(e) => json!({"tag": t, "error": format!("{e:#}")}),
                    })
                    .collect::<Vec<_>>())
            })
            .await
            .map_err(err)?;
        pretty(&Value::Array(r), 60_000)
    }

    #[tool(description = "Escribe un valor en un tag y lo relee. OJO: actúa sobre el sistema eléctrico real (interruptores, setpoints). Confirmá con el usuario antes. Requiere POWEROP_ALLOW_WRITE=1.")]
    async fn tag_write(&self, Parameters(p): Parameters<TagWriteParams>) -> ToolResult {
        self.require_write("tag_write").map_err(err)?;
        let (tag, value) = (p.tag.clone(), p.value.clone());
        let r = self
            .ct
            .call(move |s| {
                let before = s.tag_read(&tag).ok();
                s.tag_write(&tag, &value)?;
                let after = s.tag_read(&tag).ok();
                Ok((before, after))
            })
            .await;
        match r {
            Ok((before, after)) => {
                self.audit("tag_write", &format!("{} = {}", p.tag, p.value), &format!("OK (antes={before:?}, después={after:?})"));
                pretty(&json!({"tag": p.tag, "written": p.value, "before": before, "after": after}), 5_000)
            }
            Err(e) => {
                let m = err(e);
                self.audit("tag_write", &format!("{} = {}", p.tag, p.value), &format!("ERROR {m}"));
                Err(m)
            }
        }
    }

    #[tool(description = "Explora los tags definidos en el runtime (ctFindFirst sobre la tabla Tag) con filtro tipo \"TAG=Feeder1*\". Opcionalmente lee sus valores.")]
    async fn tag_browse(&self, Parameters(p): Parameters<TagBrowseParams>) -> ToolResult {
        let props = props_or(p.properties, &["TAG", "TYPE", "UNIT", "ADDR", "COMMENT", "ENG_UNITS", "EQUIP", "ITEM", "CLUSTER"]);
        let filter = p.filter.unwrap_or_default();
        let cluster = p.cluster.or_else(|| self.cfg.cluster.clone());
        let limit = p.limit.unwrap_or(200).min(5000);
        let with_values = p.with_values;
        let props2 = props.clone();
        let mut r = self
            .ct
            .call(move |s| {
                let mut r = s.find("Tag", &filter, cluster.as_deref(), &props2, limit)?;
                if with_values {
                    for row in r.rows.iter_mut() {
                        let name = row.get("TAG").and_then(Value::as_str).map(str::to_string);
                        if let (Some(name), Some(o)) = (name, row.as_object_mut()) {
                            o.insert("VALUE".into(), match s.tag_read(&name) {
                                Ok(v) => json!(v),
                                Err(e) => json!(format!("error: {e:#}")),
                            });
                        }
                    }
                }
                Ok(r)
            })
            .await
            .map_err(err)?;
        let mut props = props;
        if with_values {
            props.push("VALUE".into());
        }
        r.rows.shrink_to_fit();
        pretty(&find_json(r, &props), 80_000)
    }

    #[tool(description = "Acceso genérico a ctFindFirst/ctFindNext/ctGetProperty: cualquier tabla del runtime (Tag, Alarm, Trend...) o consultas TRNQUERY/ALMQUERY. Ver poweroperation_reference.")]
    async fn ctapi_find(&self, Parameters(p): Parameters<FindParams>) -> ToolResult {
        let props = props_or(Some(p.properties), &[]);
        if props.is_empty() {
            return Err("Indicá al menos una propiedad".into());
        }
        let filter = p.filter.unwrap_or_default();
        let cluster = p.cluster.or_else(|| self.cfg.cluster.clone());
        let limit = p.limit.unwrap_or(500).min(20_000);
        let props2 = props.clone();
        let r = self.ct.call(move |s| s.find(&p.table, &filter, cluster.as_deref(), &props2, limit)).await.map_err(err)?;
        pretty(&find_json(r, &props), 100_000)
    }

    #[tool(description = "Ejecuta Cicode en el runtime (ctCicode) y devuelve el resultado como texto. En modo solo lectura solo se permiten funciones de consulta (TagRead, TagGetProperty, *Browse*, Str*, Time*...). Funciones con efectos requieren POWEROP_ALLOW_WRITE=1 y confirmación del usuario.")]
    async fn cicode_run(&self, Parameters(p): Parameters<CicodeParams>) -> ToolResult {
        let violation = cicode::readonly_violation(&p.command);
        if let Some(v) = &violation {
            self.require_write(&format!("Esta llamada Cicode ({v})")).map_err(err)?;
        }
        let cmd = p.command.clone();
        let r = self.ct.call(move |s| s.cicode(&cmd, p.window.unwrap_or(0))).await;
        if violation.is_some() {
            self.audit("cicode_run", &p.command, &match &r {
                Ok(v) => format!("OK → {v}"),
                Err(e) => format!("ERROR {e:#}"),
            });
        }
        let v = r.map_err(err)?;
        Ok(if v.is_empty() { "(resultado vacío)".into() } else { truncate(&v, 60_000) })
    }

    #[tool(description = "Consulta datos históricos de un trend tag (TRNQUERY de CtAPI) y calcula estadísticas (mín, máx, promedio, primero, último). Devuelve JSON o CSV.")]
    async fn trend_query(&self, Parameters(p): Parameters<TrendParams>) -> ToolResult {
        if p.tag.contains(',') {
            return Err("El nombre del tag no puede contener comas".into());
        }
        let end = parse_time(p.end.as_deref().unwrap_or("now")).map_err(err)?;
        let start = match &p.start {
            Some(s) => parse_time(s).map_err(err)?,
            None => end - chrono::Duration::milliseconds((p.minutes_back.unwrap_or(60.0) * 60_000.0) as i64),
        };
        let span = (end - start).num_milliseconds() as f64 / 1000.0;
        if span <= 0.0 {
            return Err("El inicio debe ser anterior al fin".into());
        }
        let max = p.max_samples.unwrap_or(2000).clamp(1, 20_000);
        let period = p.period_seconds.filter(|x| *x > 0.0).unwrap_or_else(|| (span / 300.0).max(1.0));
        let wanted = (span / period).ceil() as usize;
        let samples = wanted.clamp(1, max);
        let query = format!(
            "TRNQUERY,{},{},{},{},{},{},{},0,0",
            end.timestamp(),
            end.timestamp_subsec_millis(),
            period,
            samples,
            p.tag,
            p.display_mode.unwrap_or(0),
            p.data_mode.unwrap_or(0)
        );
        let cluster = p.cluster.or_else(|| self.cfg.cluster.clone());
        let props: Vec<String> = ["DATETIME", "MSECONDS", "VALUE", "QUALITY"].iter().map(|s| s.to_string()).collect();
        let q2 = query.clone();
        let limit = samples + 10;
        let r = self.ct.call(move |s| s.find(&q2, "", cluster.as_deref(), &props, limit)).await.map_err(err)?;

        let mut rows: Vec<(String, Option<f64>, i64)> = Vec::new();
        for row in &r.rows {
            let g = |k: &str| row.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
            let secs: i64 = g("DATETIME").parse().unwrap_or(0);
            let ms: i64 = g("MSECONDS").parse().unwrap_or(0);
            let time = Local
                .timestamp_millis_opt(secs * 1000 + ms)
                .single()
                .map(|t| t.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
                .unwrap_or_else(|| g("DATETIME"));
            let quality: i64 = g("QUALITY").parse().unwrap_or(0);
            rows.push((time, g("VALUE").parse::<f64>().ok(), quality));
        }
        let good: Vec<f64> = rows.iter().filter(|r| (r.2 >> 4) & 0xF == 1).filter_map(|r| r.1).collect();
        let stats = if good.is_empty() {
            json!({"good_samples": 0})
        } else {
            let min = good.iter().cloned().fold(f64::INFINITY, f64::min);
            let max_v = good.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let avg = good.iter().sum::<f64>() / good.len() as f64;
            let sd = (good.iter().map(|v| (v - avg).powi(2)).sum::<f64>() / good.len() as f64).sqrt();
            json!({"good_samples": good.len(), "min": min, "max": max_v, "avg": avg, "stdev": sd,
                   "first": good.first(), "last": good.last()})
        };
        let header = json!({
            "tag": p.tag,
            "start": start.format("%Y-%m-%d %H:%M:%S").to_string(),
            "end": end.format("%Y-%m-%d %H:%M:%S").to_string(),
            "period_seconds": period,
            "requested_samples": samples,
            "limited_by_max_samples": wanted > samples,
            "returned": rows.len(),
            "query": query,
            "note": r.note,
            "stats": stats,
        });
        if p.format.as_deref().is_some_and(|f| f.eq_ignore_ascii_case("csv")) {
            let mut csv = String::from("time,value,quality\n");
            for (t, v, q) in &rows {
                csv.push_str(&format!("{t},{},{}\n", v.map(|x| x.to_string()).unwrap_or_default(), quality_desc(*q)));
            }
            return Ok(format!("{}\n\n{}", serde_json::to_string_pretty(&header).unwrap_or_default(), truncate(&csv, 200_000)));
        }
        let mut out = header;
        out["columns"] = json!(["time", "value", "quality"]);
        out["rows"] = Value::Array(rows.iter().map(|(t, v, q)| json!([t, v, quality_desc(*q)])).collect());
        pretty(&out, 150_000)
    }

    #[tool(description = "Lista alarmas desde el runtime (ctFindFirst sobre la tabla Alarm) con filtro tipo \"TAG=Feeder1*\". Para histórico/journal de eventos usa alarm_sql.")]
    async fn alarm_list(&self, Parameters(p): Parameters<AlarmListParams>) -> ToolResult {
        let fields = props_or(
            p.fields,
            &["TAG", "NAME", "DESC", "CATEGORY", "PRIORITY", "STATE", "AREA", "ONTIME", "OFFTIME", "ACKTIME", "EQUIPMENT", "COMMENT"],
        );
        let filter = p.filter.unwrap_or_default();
        let cluster = p.cluster.or_else(|| self.cfg.cluster.clone());
        let limit = p.limit.unwrap_or(200).min(10_000);
        let f2 = fields.clone();
        let r = self.ct.call(move |s| s.find("Alarm", &filter, cluster.as_deref(), &f2, limit)).await.map_err(err)?;
        pretty(&find_json(r, &fields), 100_000)
    }

    #[tool(description = "Reconoce (ACK) una alarma por tag con AlarmAckTag. Confirmá con el usuario antes. Requiere POWEROP_ALLOW_WRITE=1.")]
    async fn alarm_ack(&self, Parameters(p): Parameters<AlarmAckParams>) -> ToolResult {
        self.require_write("alarm_ack").map_err(err)?;
        let cluster = p.cluster.or_else(|| self.cfg.cluster.clone()).unwrap_or_default();
        let cmd = format!("AlarmAckTag({}, {})", cicode_str(&p.tag), cicode_str(&cluster));
        let c2 = cmd.clone();
        let r = self.ct.call(move |s| s.cicode(&c2, 0)).await;
        self.audit("alarm_ack", &cmd, &match &r {
            Ok(v) => format!("OK → {v}"),
            Err(e) => format!("ERROR {e:#}"),
        });
        let v = r.map_err(err)?;
        Ok(format!("{cmd} → {v} (0 = correcto; otro valor = código de error Cicode)"))
    }

    #[tool(description = "Consulta SQL a la base de alarmas del runtime vía ODBC ('Citect Alarm Driver'). Tablas: CiAlarmObject (configuración y estado actual de cada alarma: AlarmSource, DisplayName, Description, AlarmCategory, AlarmState, OnTime, OffTime, AckTime, Equipment...) y CDBEventJournal (histórico: RecordTime, AlarmStateDesc, Message, Category, User, ClientName). Fechas UTC: {ts 'YYYY-MM-DD HH:MM:SS'}.")]
    async fn alarm_sql(&self, Parameters(p): Parameters<AlarmSqlParams>) -> ToolResult {
        let first = p.sql.split_whitespace().next().unwrap_or("").to_lowercase();
        if first != "select" && first != "with" {
            self.require_write("Una sentencia SQL que no es SELECT").map_err(err)?;
        }
        let cfg = self.cfg.clone();
        let limit = p.limit.unwrap_or(500).min(50_000);
        let sql = p.sql.clone();
        let r = blocking(self.cfg.timeout.max(std::time::Duration::from_secs(60)), move || alarmdb::query(&cfg, &sql, limit))
            .await
            .map_err(err)?;
        let rows: Vec<Value> = r
            .rows
            .iter()
            .map(|row| Value::Object(r.columns.iter().cloned().zip(row.iter().map(|v| json!(v))).collect::<Map<_, _>>()))
            .collect();
        pretty(&json!({"count": rows.len(), "truncated": r.truncated, "rows": rows}), 150_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert!(parse_time("now").is_ok());
        let t = parse_time("2026-10-14 08:30").unwrap();
        assert_eq!(t.format("%H:%M").to_string(), "08:30");
        let z = parse_time("2026-10-14T11:00:00Z").unwrap();
        assert_eq!(z.timestamp(), 1_791_975_600);
        assert!(parse_time("ayer").is_err());
    }

    #[test]
    fn cicode_literals() {
        assert_eq!(cicode_str(r#"A"B^C"#), r#""A^"B^^C""#);
    }

    #[test]
    fn quality() {
        assert_eq!(quality_desc(0x12), "good/raw");
        assert_eq!(quality_desc(0x11), "good/interp");
        assert_eq!(quality_desc(0x02), "bad");
    }
}
