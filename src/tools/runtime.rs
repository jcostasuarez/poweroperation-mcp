//! Herramientas de runtime: CtAPI (tags, Cicode, browse, tendencias, alarmas) y ODBC de alarmas.

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::alarmdb;
use crate::cicode;
use crate::ctapi::{
    FindResult, Quality, Session, TagReading, is_restarting_error, quality_extended_name, quality_general_name, quality_limit_name,
    quality_substatus_name,
};
use crate::server::{PowerOpServer, ToolResult, blocking, err};
use crate::text::truncate;

fn pretty(v: &Value, max: usize) -> ToolResult {
    let s = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    Ok(truncate(&s, max))
}

/// Literal de cadena Cicode (^ es el carácter de escape).
pub(crate) fn cicode_str(s: &str) -> String {
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

/// Nombre del tipo de la propiedad TYPE de la tabla Tag de CtAPI (código numérico).
fn tag_type_name(code: &str) -> Option<&'static str> {
    Some(match code.trim() {
        "0" => "DIGITAL",
        "1" => "INT",
        "2" => "REAL",
        "3" => "BCD",
        "4" => "LONG",
        "5" => "LONGBCD",
        "7" => "STRING",
        "8" => "BYTE",
        "10" => "UINT",
        "18" => "ULONG",
        _ => return None,
    })
}

/// Calidad de una muestra de TRNQUERY: bits 0-3 tipo de valor (0 = sin muestra), bits 4-7 calidad.
fn quality_desc(q: i64) -> &'static str {
    let good = (q >> 4) & 0xF == 1;
    match (q & 0xF, good) {
        (0, _) => "none",
        (_, false) => "bad",
        (1, true) => "good/interp",
        (2 | 3, true) => "good/raw",
        _ => "good",
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct TagReadParams {
    /// Nombres de tags, p. ej. ["Feeder1\\MMXU1\\TotW"]. Se puede usar Tag.Elemento y Tag[índice].
    /// Con muestras repetidas: máximo 50, sin repetir.
    pub tags: Vec<String>,
    /// Muestras repetidas para seguir una secuencia (máximo 120). Sin samples ni interval_secs: una sola lectura.
    #[serde(default)]
    pub samples: Option<usize>,
    /// Segundos entre muestras (por defecto 1.0, mínimo 0.2; con samples por defecto 5). Duración total =
    /// (samples - 1) × interval_secs, máximo 300 s.
    #[serde(default)]
    pub interval_secs: Option<f64>,
    /// Agregar propiedades del tag (ctTagGetProperty): unidades, descripción, rango de ingeniería y tipo.
    /// Una llamada más por propiedad y tag; solo en la lectura simple (sin samples).
    #[serde(default)]
    pub details: bool,
}

/// FILETIME (100 ns desde 1601, UTC) → hora local RFC 3339 con milisegundos. `None` = sin hora (0).
/// Fuera de 1990–2200 (no parece una hora) se devuelve el número crudo.
fn filetime_json(ft: u64) -> Option<Value> {
    const EPOCH_DIFF_SECS: i64 = 11_644_473_600;
    if ft == 0 {
        return None;
    }
    let secs = (ft / 10_000_000) as i64 - EPOCH_DIFF_SECS;
    let nanos = ((ft % 10_000_000) * 100) as u32;
    match DateTime::from_timestamp(secs, nanos) {
        Some(t) if (631_152_000..7_258_118_400).contains(&secs) => {
            Some(json!(t.with_timezone(&Local).to_rfc3339_opts(chrono::SecondsFormat::Millis, false)))
        }
        _ => Some(json!(ft)),
    }
}

/// Calidad corta: "GOOD", "BAD/COMM_FAILURE", "UNCERTAIN/LAST_USABLE_VALUE"... (con el subestado si no es NON_SPECIFIC).
fn quality_label(q: &Quality) -> String {
    let general = quality_general_name(q.general).map(str::to_string).unwrap_or_else(|| format!("Q{}", q.general));
    match q.substatus {
        0 => general,
        s => format!("{general}/{}", quality_substatus_name(q.general, s).map(str::to_string).unwrap_or_else(|| s.to_string())),
    }
}

/// `true` si la calidad no es un GOOD sin más (subestado, límite, extendido, error, override o control inhibit).
fn quality_noteworthy(q: &Quality) -> bool {
    q.general != 3 || q.substatus != 0 || q.limit != 0 || q.extended != 0 || q.datasource_error != 0 || q.overridden || q.control_inhibit
}

/// JSON de una lectura: valor, calidad (con detalle si no es GOOD a secas) y horas.
fn reading_json(tag: &str, r: &TagReading) -> Value {
    let mut o = Map::new();
    o.insert("tag".into(), json!(tag));
    o.insert("value".into(), json!(r.value));
    if let Some(q) = &r.quality {
        o.insert("quality".into(), json!(quality_label(q)));
        if quality_noteworthy(q) {
            let mut d = Map::new();
            if q.limit != 0 {
                d.insert("limit".into(), quality_limit_name(q.limit).map_or_else(|| json!(q.limit), |n| json!(n)));
            }
            if q.extended != 0 {
                d.insert("extended".into(), quality_extended_name(q.extended).map_or_else(|| json!(q.extended), |n| json!(n)));
            }
            if q.datasource_error != 0 {
                d.insert("datasource_error".into(), json!(q.datasource_error));
            }
            if q.overridden {
                d.insert("override".into(), json!(true));
            }
            if q.control_inhibit {
                d.insert("control_inhibit".into(), json!(true));
            }
            if let Some(t) = filetime_json(r.quality_timestamp) {
                d.insert("quality_timestamp".into(), t);
            }
            if !d.is_empty() {
                o.insert("quality_detail".into(), Value::Object(d));
            }
        }
    }
    if let Some(t) = filetime_json(r.timestamp) {
        o.insert("timestamp".into(), t);
    }
    if r.value_timestamp != r.timestamp
        && let Some(t) = filetime_json(r.value_timestamp)
    {
        o.insert("value_timestamp".into(), t);
    }
    Value::Object(o)
}

/// Nombre del Type de ctTagGetProperty (0 Digital ... 8 Undefined, según la ayuda).
fn tag_property_type_name(code: &str) -> Option<&'static str> {
    Some(match code.trim() {
        "0" => "DIGITAL",
        "1" => "BYTE",
        "2" => "INT",
        "3" => "UINT",
        "4" => "LONG",
        "5" => "REAL",
        "6" => "STRING",
        "7" => "ULONG",
        "8" => "UNDEFINED",
        _ => return None,
    })
}

/// Propiedades de `details`: (propiedad de ctTagGetProperty, campo del JSON).
const DETAIL_PROPS: [(&str, &str); 5] =
    [("Units", "units"), ("Description", "description"), ("EngUnitsLow", "eng_low"), ("EngUnitsHigh", "eng_high"), ("Type", "type")];

/// Agrega a `o` las propiedades del tag que el runtime devuelva (las vacías se omiten).
fn add_details(s: &Session, tag: &str, o: &mut Map<String, Value>) {
    for (prop, key) in DETAIL_PROPS {
        let Some(v) = s.tag_property(tag, prop).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) else {
            continue;
        };
        if prop == "Type" {
            o.insert("type".into(), json!(tag_property_type_name(&v).unwrap_or(v.as_str())));
        } else {
            o.insert(key.into(), json!(v));
        }
    }
}

/// Máximo de escrituras por llamada de po_tag_write y de tags por muestra repetida de po_tag_read.
const MAX_WRITES: usize = 50;
const MAX_WATCH_TAGS: usize = 50;
/// Muestras y duración máximas de po_tag_read con samples.
const MAX_WATCH_SAMPLES: usize = 120;
const MAX_WATCH_SECS: f64 = 300.0;
const MIN_WATCH_INTERVAL: f64 = 0.2;

#[derive(Deserialize, JsonSchema)]
pub struct TagWriteParams {
    /// Nombre del tag, para escribir uno solo (junto con `value`). No combinar con `writes`.
    #[serde(default)]
    pub tag: Option<String>,
    /// Valor como texto para `tag` (CtAPI escribe en unidades de ingeniería).
    #[serde(default)]
    pub value: Option<String>,
    /// Varias escrituras: [{"tag": "...", "value": "..."}, ...] (máximo 50). Confirmá con el usuario la lista completa antes.
    #[serde(default)]
    pub writes: Option<Vec<TagWriteItem>>,
}

#[derive(Deserialize, JsonSchema)]
pub struct TagWriteItem {
    /// Nombre del tag.
    pub tag: String,
    /// Valor como texto (unidades de ingeniería).
    pub value: String,
}

/// Resultado de escribir un tag: valores antes/después o el error de esa escritura.
struct WriteOutcome {
    tag: String,
    value: String,
    before: Option<String>,
    after: Option<String>,
    error: Option<String>,
    /// Fallo transitorio de reinicio del cliente (la escritura no se reintenta).
    restart: bool,
}

/// Una muestra de tag_watch: hora y valor de cada tag (en el orden pedido).
struct WatchSample {
    t: String,
    values: Vec<(String, String)>,
}

/// Lista de escrituras de tag_write: `tag`+`value` (una) o `writes` (lote). Error si no viene una de las dos formas
/// o vienen ambas.
fn write_list(p: TagWriteParams) -> Result<Vec<(String, String)>, String> {
    match (p.tag.is_some() || p.value.is_some(), p.writes) {
        (true, Some(_)) => Err("Usá tag y value (un tag) o writes (varios), no ambos.".into()),
        (true, None) => match (p.tag, p.value) {
            (Some(t), Some(v)) => Ok(vec![(t, v)]),
            (None, _) => Err("Falta tag. Para un tag indicá tag y value; para varios, writes: [{tag, value}, ...].".into()),
            (Some(_), None) => Err("Falta value para el tag indicado.".into()),
        },
        (false, Some(list)) if list.is_empty() => Err("writes está vacío: indicá al menos un {tag, value}.".into()),
        (false, Some(list)) if list.len() > MAX_WRITES => Err(format!(
            "writes tiene {} escrituras; el máximo por llamada es {MAX_WRITES}. Dividí el lote.",
            list.len()
        )),
        (false, Some(list)) => Ok(list.into_iter().map(|w| (w.tag, w.value)).collect()),
        (false, None) => Err("Indicá tag y value (un tag) o writes: [{tag, value}, ...] (varios).".into()),
    }
}

/// Escribe un tag y lo relee. Sin reintentos: una escritura que pudo aplicarse no se repite.
fn write_one(s: &Session, tag: &str, value: &str) -> WriteOutcome {
    let before = s.tag_read(tag).ok();
    match s.tag_write(tag, value) {
        Ok(()) => WriteOutcome {
            tag: tag.into(),
            value: value.into(),
            before,
            after: s.tag_read(tag).ok(),
            error: None,
            restart: false,
        },
        Err(e) => {
            let restart = is_restarting_error(&e);
            let mut msg = format!("{e:#}");
            if restart {
                msg.push_str(" (no se reintentó la escritura: verificá el valor con po_tag_read antes de volver a escribir)");
            }
            WriteOutcome { tag: tag.into(), value: value.into(), before, after: None, error: Some(msg), restart }
        }
    }
}

/// Resultado de po_tag_write cuando no se llegó a escribir (conexión): todos los tags con el mismo error.
fn fail_all(items: &[(String, String)], e: &anyhow::Error) -> Vec<WriteOutcome> {
    let restart = is_restarting_error(e);
    items
        .iter()
        .map(|(t, v)| WriteOutcome {
            tag: t.clone(),
            value: v.clone(),
            before: None,
            after: None,
            error: Some(format!("{e:#}")),
            restart,
        })
        .collect()
}

/// Valor de un tag para tag_watch: el texto leído, con la calidad entre corchetes si no es GOOD (así un tag que
/// pierde la comunicación cuenta como cambio), o "error: ..." (un tag fallido no corta la muestra).
fn watch_text(r: anyhow::Result<TagReading>) -> String {
    match r {
        Ok(r) => match &r.quality {
            Some(q) if q.general != 3 => format!("{} [{}]", r.value, quality_label(q)),
            _ => r.value,
        },
        Err(e) => format!("error: {e:#}"),
    }
}

/// Hora de una muestra: HH:MM:SS.mmm (local).
fn now_stamp() -> String {
    Local::now().format("%H:%M:%S%.3f").to_string()
}

/// Valida los parámetros de po_tag_read con samples. Devuelve la cantidad de muestras y el intervalo ya comprobados.
fn validate_watch(tags: &[String], samples: Option<usize>, interval: Option<f64>) -> Result<(usize, f64), String> {
    if tags.is_empty() {
        return Err("Indicá al menos un tag en tags.".into());
    }
    if tags.len() > MAX_WATCH_TAGS {
        return Err(format!("tags tiene {} elementos; el máximo es {MAX_WATCH_TAGS}.", tags.len()));
    }
    for (i, t) in tags.iter().enumerate() {
        if t.trim().is_empty() {
            return Err("Hay un tag vacío en tags.".into());
        }
        if tags[..i].contains(t) {
            return Err(format!("El tag '{t}' está repetido en tags."));
        }
    }
    let n = samples.unwrap_or(5);
    if n == 0 || n > MAX_WATCH_SAMPLES {
        return Err(format!("samples debe estar entre 1 y {MAX_WATCH_SAMPLES} (pediste {n})."));
    }
    let iv = interval.unwrap_or(1.0);
    if !iv.is_finite() || iv < MIN_WATCH_INTERVAL {
        return Err(format!("interval_secs debe ser de al menos {MIN_WATCH_INTERVAL} s (pediste {iv})."));
    }
    let total = (n - 1) as f64 * iv;
    if total > MAX_WATCH_SECS {
        return Err(format!(
            "La duración total ({total:.1} s = (samples - 1) × interval_secs) supera el máximo de {MAX_WATCH_SECS:.0} s. Bajá samples o interval_secs."
        ));
    }
    Ok((n, iv))
}

/// JSON de po_tag_read con samples: muestras con hora y, por tag, resumen de valores distintos y del primer cambio.
fn watch_json(tags: &[String], samples: &[WatchSample], requested: usize, interval: f64, interrupted: Option<String>) -> Value {
    let mut summary = Map::new();
    for (i, tag) in tags.iter().enumerate() {
        let series: Vec<&str> = samples.iter().filter_map(|s| s.values.get(i).map(|(_, v)| v.as_str())).collect();
        let mut distinct: Vec<&str> = Vec::new();
        for v in &series {
            if !distinct.contains(v) {
                distinct.push(v);
            }
        }
        let mut changes = 0usize;
        let mut first_change: Option<&str> = None;
        for (k, pair) in samples.windows(2).enumerate() {
            let (a, b) = (&pair[0].values[i].1, &pair[1].values[i].1);
            if a != b {
                changes += 1;
                if first_change.is_none() {
                    first_change = Some(samples[k + 1].t.as_str());
                }
            }
        }
        summary.insert(
            tag.clone(),
            json!({
                "distinct_values": distinct.len(),
                "values": distinct.iter().take(10).collect::<Vec<_>>(),
                "changes": changes,
                "first_change_t": first_change,
                "last": series.last(),
            }),
        );
    }
    let rows: Vec<Value> = samples
        .iter()
        .map(|s| {
            let values: Map<String, Value> = s.values.iter().map(|(t, v)| (t.clone(), json!(v))).collect();
            json!({"t": s.t, "values": values})
        })
        .collect();
    let mut out = json!({
        "samples_requested": requested,
        "samples_taken": samples.len(),
        "interval_secs": interval,
        "samples": rows,
        "summary": summary,
    });
    if let Some(m) = interrupted {
        out["interrupted"] = json!(m);
    }
    out
}

#[derive(Deserialize, JsonSchema)]
pub struct TagBrowseParams {
    /// Filtro CtAPI, p. ej. "TAG=Feeder1*" (comodines * y ?). Vacío = todos (usar limit).
    #[serde(default)]
    pub filter: Option<String>,
    /// Propiedades a devolver (por defecto TAG, TYPE, CLUSTER, EQUIPMENT, ITEM, ADDR, ENG_UNITS, ENG_ZERO,
    /// ENG_FULL, COMMENT; también RAW_ZERO, RAW_FULL, ARR_SIZE, FORMAT). TYPE es un código: se agrega TYPE_NAME.
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
    /// Filtro CtAPI, p. ej. "TAG=Feeder1*" o "CATEGORY=1".
    #[serde(default)]
    pub filter: Option<String>,
    /// false (por defecto): tabla Alarm, una fila por alarma configurada con su estado actual.
    /// true: tabla AlarmSummary, una fila por cada activación (hora de activación, desactivación y reconocimiento, usuario).
    #[serde(default)]
    pub summary: bool,
    /// Campos. Alarm: TAG, NAME, DESC, ALARMTYPE, CATEGORY, PRIORITY, STATE, STATE_DESC, VALUE, TIME, DATE, AREA,
    /// EQUIPMENT, ALMCOMMENT, CLUSTER (y HIGH, LOW, HIGHHIGH, LOWLOW, DEADBAND...). AlarmSummary: TAG, NAME, SUMDESC,
    /// SUMSTATE, CATEGORY, PRIORITY, VALUE, ONDATE, ONTIME, OFFDATE, OFFTIME, ACKDATE, ACKTIME, USERNAME, COMMENT, EQUIPMENT.
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
    #[tool(name = "po_tag_read", description = "Lee el valor actual de uno o varios tags (en unidades de ingeniería, como texto) con su calidad (GOOD, BAD/COMM_FAILURE...; quality_detail si hay override, control inhibit, límite o subestado extendido) y la hora de la última actualización. Varios tags se leen en un solo pedido (ctListRead), todos del mismo instante. details=true agrega unidades, descripción, rango de ingeniería y tipo. Con samples y/o interval_secs lee los tags N veces (solo lectura) para verificar secuencias, p. ej. una transferencia o un interruptor: cada muestra es UNA llamada a CtAPI con todos los tags (máximo 50 tags, 120 muestras, intervalo mínimo 0.2 s, duración máxima 300 s); devuelve las muestras con hora (HH:MM:SS.mmm) y, por tag, los valores distintos, el número de cambios y la hora del primer cambio.")]
    async fn tag_read(&self, Parameters(p): Parameters<TagReadParams>) -> ToolResult {
        if p.samples.is_some() || p.interval_secs.is_some() {
            return self.tag_watch(p).await;
        }
        if p.tags.is_empty() {
            return Err("Indicá al menos un tag".into());
        }
        let details = p.details;
        let r = self
            .ct
            .call_retry(move |s| {
                Ok(p.tags
                    .iter()
                    .zip(s.read_many(&p.tags))
                    .map(|(t, r)| {
                        let mut v = match r {
                            Ok(r) => reading_json(t, &r),
                            Err(e) => json!({"tag": t, "error": format!("{e:#}")}),
                        };
                        if details && let Some(o) = v.as_object_mut() {
                            add_details(s, t, o);
                        }
                        v
                    })
                    .collect::<Vec<_>>())
            })
            .await
            .map_err(err)?;
        pretty(&Value::Array(r), 60_000)
    }

    #[tool(name = "po_tag_write", description = "Escribe un valor en uno o varios tags y relee cada uno (antes/después). OJO: actúa sobre el sistema eléctrico real (interruptores, setpoints). Un tag: tag + value. Varios: writes = [{tag, value}, ...] (máximo 50): confirmá con el usuario la LISTA COMPLETA de tags y valores ANTES de llamar. Cada tag tiene su línea de auditoría; si un tag falla se siguen los demás y se informa cada resultado. Si el cliente del runtime se está reiniciando se reconecta una vez, pero la escritura NO se reintenta: verificá con po_tag_read antes de volver a escribir. Requiere POWEROP_ALLOW_WRITE=1.")]
    async fn tag_write(&self, Parameters(p): Parameters<TagWriteParams>) -> ToolResult {
        self.require_write("tag_write").map_err(err)?;
        let batch = p.writes.is_some();
        let items = write_list(p)?;
        // Conexión con reintento ante 233; la escritura en sí no se reintenta (ver write_one).
        let outcomes: Vec<WriteOutcome> = match self.ct.connect_retry().await {
            Ok(()) => {
                let list = items.clone();
                match self.ct.call(move |s| Ok(list.iter().map(|(t, v)| write_one(s, t, v)).collect::<Vec<_>>())).await {
                    Ok(o) => o,
                    Err(e) => fail_all(&items, &e),
                }
            }
            Err(e) => fail_all(&items, &e),
        };
        if outcomes.iter().any(|o| o.restart) {
            // Conexión rota: la próxima llamada vuelve a conectar.
            self.ct.disconnect().await;
        }
        for o in &outcomes {
            let detail = format!("{} = {}", o.tag, o.value);
            match &o.error {
                None => self.audit("tag_write", &detail, &format!("OK (antes={:?}, después={:?})", o.before, o.after)),
                Some(m) => self.audit("tag_write", &detail, &format!("ERROR {m}")),
            }
        }
        let failed = outcomes.iter().filter(|o| o.error.is_some()).count();
        let report: Vec<Value> = outcomes
            .iter()
            .map(|o| match &o.error {
                None => json!({"tag": o.tag, "written": o.value, "before": o.before, "after": o.after}),
                Some(m) => json!({"tag": o.tag, "written": o.value, "error": m}),
            })
            .collect();
        if !batch {
            return match outcomes.first().and_then(|o| o.error.clone()) {
                Some(m) => Err(m),
                None => pretty(&report[0], 5_000),
            };
        }
        let text = pretty(&json!({"ok": outcomes.len() - failed, "failed": failed, "results": report}), 20_000)?;
        if failed > 0 {
            return Err(format!("{failed} de {} escrituras fallaron. Detalle por tag:\n{text}", outcomes.len()));
        }
        Ok(text)
    }

    #[tool(name = "po_tag_browse", description = "Explora los tags definidos en el runtime (ctFindFirst sobre la tabla Tag) con filtro tipo \"TAG=Feeder1*\". Opcionalmente lee sus valores.")]
    async fn tag_browse(&self, Parameters(p): Parameters<TagBrowseParams>) -> ToolResult {
        let props = props_or(p.properties, &["TAG", "TYPE", "CLUSTER", "EQUIPMENT", "ITEM", "ADDR", "ENG_UNITS", "ENG_ZERO", "ENG_FULL", "COMMENT"]);
        let filter = p.filter.unwrap_or_default();
        let cluster = p.cluster.or_else(|| self.cfg.cluster.clone());
        let limit = p.limit.unwrap_or(200).min(5000);
        let with_values = p.with_values;
        let props2 = props.clone();
        let mut r = self
            .ct
            .call_retry(move |s| {
                let mut r = s.find("Tag", &filter, cluster.as_deref(), &props2, limit)?;
                if with_values {
                    let names: Vec<String> =
                        r.rows.iter().map(|row| row.get("TAG").and_then(Value::as_str).unwrap_or("").to_string()).collect();
                    for ((row, name), read) in r.rows.iter_mut().zip(&names).zip(s.read_many(&names)) {
                        if let (false, Some(o)) = (name.is_empty(), row.as_object_mut()) {
                            match read {
                                Ok(v) => {
                                    o.insert("VALUE".into(), json!(v.value));
                                    if let Some(q) = &v.quality {
                                        o.insert("QUALITY".into(), json!(quality_label(q)));
                                    }
                                }
                                Err(e) => {
                                    o.insert("VALUE".into(), json!(format!("error: {e:#}")));
                                }
                            }
                        }
                    }
                }
                Ok(r)
            })
            .await
            .map_err(err)?;
        let mut props = props;
        if props.iter().any(|p| p == "TYPE") {
            for row in r.rows.iter_mut() {
                let name = row.get("TYPE").and_then(Value::as_str).and_then(tag_type_name);
                if let (Some(n), Some(o)) = (name, row.as_object_mut()) {
                    o.insert("TYPE_NAME".into(), json!(n));
                }
            }
            props.push("TYPE_NAME".into());
        }
        if with_values {
            props.push("VALUE".into());
            props.push("QUALITY".into());
        }
        pretty(&find_json(r, &props), 80_000)
    }

    #[tool(name = "po_ctapi_find", description = "Acceso genérico a ctFindFirst/ctFindNext/ctGetProperty: cualquier tabla del runtime (Tag, Alarm, Trend...) o consultas TRNQUERY/ALMQUERY. Ver po_reference.")]
    async fn ctapi_find(&self, Parameters(p): Parameters<FindParams>) -> ToolResult {
        let props = props_or(Some(p.properties), &[]);
        if props.is_empty() {
            return Err("Indicá al menos una propiedad".into());
        }
        let filter = p.filter.unwrap_or_default();
        let cluster = p.cluster.or_else(|| self.cfg.cluster.clone());
        let limit = p.limit.unwrap_or(500).min(20_000);
        let props2 = props.clone();
        let r = self.ct.call_retry(move |s| s.find(&p.table, &filter, cluster.as_deref(), &props2, limit)).await.map_err(err)?;
        pretty(&find_json(r, &props), 100_000)
    }

    #[tool(name = "po_cicode_run", description = "Ejecuta Cicode en el runtime (ctCicode) y devuelve el resultado como texto. En modo solo lectura solo se permiten funciones de consulta (TagRead, TagGetProperty, *Browse*, Str*, Time*...). Funciones con efectos requieren POWEROP_ALLOW_WRITE=1 y confirmación del usuario.")]
    async fn cicode_run(&self, Parameters(p): Parameters<CicodeParams>) -> ToolResult {
        let violation = cicode::readonly_violation(&p.command);
        if let Some(v) = &violation {
            self.require_write(&format!("Esta llamada Cicode ({v})")).map_err(err)?;
        }
        let cmd = p.command.clone();
        let window = p.window.unwrap_or(0);
        // Solo las consultas (sin efectos) se reintentan ante 233: repetir un comando con efectos no es seguro.
        let r = if violation.is_none() {
            self.ct.call_retry(move |s| s.cicode(&cmd, window)).await
        } else {
            self.ct.call(move |s| s.cicode(&cmd, window)).await
        };
        if violation.is_some() {
            self.audit("cicode_run", &p.command, &match &r {
                Ok(v) => format!("OK → {v}"),
                Err(e) => format!("ERROR {e:#}"),
            });
        }
        let v = r.map_err(err)?;
        Ok(if v.is_empty() { "(resultado vacío)".into() } else { truncate(&v, 60_000) })
    }

    #[tool(name = "po_trend_query", description = "Consulta datos históricos de un trend tag (TRNQUERY de CtAPI) y calcula estadísticas (mín, máx, promedio, primero, último). Devuelve JSON o CSV.")]
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
        // TRNQUERY toma el inicio como End - Period × (NumSamples - 1).
        let wanted = (span / period).floor() as usize + 1;
        let samples = wanted.clamp(1, max);
        // Forma de 7 argumentos: en PO 2024, InstantTrend=0 con SamplePeriod=0 al final da error Citect 274.
        let query = format!(
            "TRNQUERY,{},{},{},{},{},{},{}",
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
        let r = self.ct.call_retry(move |s| s.find(&q2, "", cluster.as_deref(), &props, limit)).await.map_err(err)?;

        // El servidor devuelve las muestras de la más nueva a la más vieja (y hasta 2 extra fuera del rango):
        // se ordenan cronológicamente.
        let mut stamped: Vec<(i64, String, Option<f64>, i64)> = Vec::new();
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
            stamped.push((secs * 1000 + ms, time, g("VALUE").parse::<f64>().ok(), quality));
        }
        stamped.sort_by_key(|r| r.0);
        let rows: Vec<(String, Option<f64>, i64)> = stamped.into_iter().map(|(_, t, v, q)| (t, v, q)).collect();
        let good: Vec<f64> = rows.iter().filter(|r| r.2 & 0xF != 0 && (r.2 >> 4) & 0xF == 1).filter_map(|r| r.1).collect();
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

    #[tool(name = "po_alarm_list", description = "Alarmas del runtime vía CtAPI con filtro tipo \"TAG=Feeder1*\": estado actual de cada alarma (tabla Alarm) o, con summary=true, el resumen de activaciones con horas de activación, desactivación y reconocimiento (tabla AlarmSummary). Para el journal histórico por SQL usa po_alarm_sql.")]
    async fn alarm_list(&self, Parameters(p): Parameters<AlarmListParams>) -> ToolResult {
        let (table, default): (&str, &[&str]) = if p.summary {
            (
                "AlarmSummary",
                &["TAG", "NAME", "SUMDESC", "SUMSTATE", "CATEGORY", "PRIORITY", "VALUE", "ONDATE", "ONTIME", "OFFDATE", "OFFTIME", "ACKDATE", "ACKTIME", "USERNAME", "COMMENT", "EQUIPMENT"],
            )
        } else {
            (
                "Alarm",
                &["TAG", "NAME", "DESC", "ALARMTYPE", "CATEGORY", "PRIORITY", "STATE", "STATE_DESC", "VALUE", "TIME", "DATE", "AREA", "EQUIPMENT", "ALMCOMMENT", "CLUSTER"],
            )
        };
        let fields = props_or(p.fields, default);
        let filter = p.filter.unwrap_or_default();
        let cluster = p.cluster.or_else(|| self.cfg.cluster.clone());
        let limit = p.limit.unwrap_or(200).min(10_000);
        let f2 = fields.clone();
        let r = self.ct.call_retry(move |s| s.find(table, &filter, cluster.as_deref(), &f2, limit)).await.map_err(err)?;
        pretty(&find_json(r, &fields), 100_000)
    }

    #[tool(name = "po_alarm_ack", description = "Reconoce (ACK) una alarma por tag con AlarmAckTag. Confirmá con el usuario antes. Requiere POWEROP_ALLOW_WRITE=1.")]
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

    #[tool(name = "po_alarm_sql", description = "Consulta SQL a la base de alarmas del runtime vía ODBC ('Citect Alarm Driver'). Tablas: CiAlarmObject (configuración y estado actual de cada alarma: AlarmSource, DisplayName, Description, AlarmCategory, AlarmState, OnTime, OffTime, AckTime, Equipment...) y CDBEventJournal (histórico: RecordTime, AlarmStateDesc, Message, Category, User, ClientName). Fechas UTC: {ts 'YYYY-MM-DD HH:MM:SS'}.")]
    async fn alarm_sql(&self, Parameters(p): Parameters<AlarmSqlParams>) -> ToolResult {
        let first = p.sql.split_whitespace().next().unwrap_or("").to_lowercase();
        if first != "select" && first != "with" {
            self.require_write("Una sentencia SQL que no es SELECT").map_err(err)?;
        }
        let cfg = self.cfg.clone();
        let ep = self.alarm_endpoint().await;
        let server = ep.to_string();
        let limit = p.limit.unwrap_or(500).min(50_000);
        let sql = p.sql.clone();
        let r = blocking(self.cfg.timeout.max(std::time::Duration::from_secs(60)), move || alarmdb::query(&cfg, &ep, &sql, limit))
            .await
            .map_err(err)?;
        let rows: Vec<Value> = r
            .rows
            .iter()
            .map(|row| Value::Object(r.columns.iter().cloned().zip(row.iter().map(|v| json!(v))).collect::<Map<_, _>>()))
            .collect();
        pretty(&json!({"server": server, "count": rows.len(), "truncated": r.truncated, "rows": rows}), 150_000)
    }
}

impl PowerOpServer {
    /// po_tag_read con muestras repetidas.
    async fn tag_watch(&self, p: TagReadParams) -> ToolResult {
        let (n, interval) = validate_watch(&p.tags, p.samples, p.interval_secs)?;
        let mut taken: Vec<WatchSample> = Vec::with_capacity(n);
        let mut interrupted = None;
        for i in 0..n {
            if i > 0 {
                // Espera fuera de la llamada a CtAPI (no ocupa el mutex ni el hilo bloqueante).
                tokio::time::sleep(std::time::Duration::from_secs_f64(interval)).await;
            }
            let names = p.tags.clone();
            let read = move |s: &Session| Ok(names.iter().cloned().zip(s.read_many(&names).into_iter().map(watch_text)).collect::<Vec<_>>());
            match self.ct.call_retry(read).await {
                Ok(values) => taken.push(WatchSample { t: now_stamp(), values }),
                Err(e) => {
                    interrupted = Some(format!("{e:#}"));
                    break;
                }
            }
        }
        if taken.is_empty() {
            return Err(interrupted.unwrap_or_else(|| "Sin muestras".into()));
        }
        let out = watch_json(&p.tags, &taken, n, interval, interrupted);
        Ok(truncate(&serde_json::to_string(&out).map_err(|e| e.to_string())?, 100_000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        let t = parse_time("2026-10-14 08:30").unwrap();
        assert_eq!(t.format("%Y-%m-%d %H:%M:%S").to_string(), "2026-10-14 08:30:00");
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
        assert_eq!(quality_desc(0x00), "none");
        assert_eq!(quality_desc(0x14), "good");
    }

    /// 2026-11-27T10:00:00Z como FILETIME.
    const FT: u64 = (1_795_773_600 + 11_644_473_600) * 10_000_000;

    #[test]
    fn horas_filetime() {
        assert_eq!(filetime_json(0), None);
        let t = filetime_json(FT + 1_230_000).unwrap();
        let back = DateTime::parse_from_rfc3339(t.as_str().unwrap()).unwrap();
        assert_eq!(back.timestamp(), 1_795_773_600);
        assert_eq!(back.timestamp_subsec_millis(), 123);
        // Algo que no parece una hora sale crudo.
        assert_eq!(filetime_json(42), Some(json!(42)));
    }

    #[test]
    fn lectura_en_json() {
        let good = Quality { general: 3, ..Default::default() };
        let r = TagReading { value: "12.5".into(), quality: Some(good), timestamp: FT, value_timestamp: FT, quality_timestamp: FT };
        let j = reading_json("T", &r);
        assert_eq!(j["quality"], "GOOD");
        assert!(j.get("quality_detail").is_none() && j.get("value_timestamp").is_none());
        assert!(j["timestamp"].is_string());

        let bad = Quality { general: 0, substatus: 6, extended: 0x11, overridden: true, ..Default::default() };
        let r = TagReading { value: "0".into(), quality: Some(bad), timestamp: FT, value_timestamp: FT - 600_000_000, quality_timestamp: FT };
        let j = reading_json("T", &r);
        assert_eq!(j["quality"], "BAD/COMM_FAILURE");
        assert_eq!(j["quality_detail"]["extended"], "COMM_DEV_BAD");
        assert_eq!(j["quality_detail"]["override"], true);
        assert!(j["value_timestamp"].is_string());

        // Sin ctTagReadEx (DLL viejo): solo el valor.
        let j = reading_json("T", &TagReading { value: "1".into(), ..Default::default() });
        assert_eq!(j, json!({"tag": "T", "value": "1"}));
    }

    #[test]
    fn muestra_con_calidad() {
        let bad = Quality { general: 0, substatus: 6, ..Default::default() };
        assert_eq!(watch_text(Ok(TagReading { value: "1".into(), quality: Some(bad), ..Default::default() })), "1 [BAD/COMM_FAILURE]");
        let good = Quality { general: 3, ..Default::default() };
        assert_eq!(watch_text(Ok(TagReading { value: "1".into(), quality: Some(good), ..Default::default() })), "1");
        assert!(watch_text(Err(anyhow::anyhow!("x"))).starts_with("error: "));
    }

    fn tw(tag: Option<&str>, value: Option<&str>, writes: Option<Vec<(&str, &str)>>) -> TagWriteParams {
        TagWriteParams {
            tag: tag.map(String::from),
            value: value.map(String::from),
            writes: writes.map(|v| v.into_iter().map(|(t, v)| TagWriteItem { tag: t.into(), value: v.into() }).collect()),
        }
    }

    #[test]
    fn write_list_formas() {
        assert_eq!(write_list(tw(Some("A"), Some("1"), None)).unwrap(), vec![("A".into(), "1".into())]);
        let lote = write_list(tw(None, None, Some(vec![("A", "1"), ("B", "0")]))).unwrap();
        assert_eq!(lote.len(), 2);
        assert_eq!(lote[1], ("B".into(), "0".into()));
        assert!(write_list(tw(None, None, None)).unwrap_err().contains("Indicá tag y value"));
        assert!(write_list(tw(Some("A"), Some("1"), Some(vec![("B", "2")]))).unwrap_err().contains("no ambos"));
        assert!(write_list(tw(Some("A"), None, None)).unwrap_err().contains("Falta value"));
        assert!(write_list(tw(None, Some("1"), None)).unwrap_err().contains("Falta tag"));
        assert!(write_list(tw(None, None, Some(vec![]))).unwrap_err().contains("vacío"));
        let muchos: Vec<(&str, &str)> = (0..=MAX_WRITES).map(|_| ("A", "1")).collect();
        assert!(write_list(tw(None, None, Some(muchos))).unwrap_err().contains("máximo por llamada"));
        let justo: Vec<(&str, &str)> = (0..MAX_WRITES).map(|_| ("A", "1")).collect();
        assert_eq!(write_list(tw(None, None, Some(justo))).unwrap().len(), MAX_WRITES);
    }

    #[test]
    fn validate_watch_limites() {
        let t = vec!["A".to_string()];
        assert_eq!(validate_watch(&t, None, None).unwrap(), (5, 1.0));
        assert!(validate_watch(&[], None, None).is_err());
        assert!(validate_watch(&["A".into(), "A".into()], None, None).unwrap_err().contains("repetido"));
        assert!(validate_watch(&t, Some(121), None).unwrap_err().contains("entre 1 y 120"));
        assert!(validate_watch(&t, Some(0), None).is_err());
        assert!(validate_watch(&t, None, Some(0.1)).unwrap_err().contains("0.2"));
        assert_eq!(validate_watch(&t, None, Some(0.2)).unwrap(), (5, 0.2));
        assert!(validate_watch(&t, None, Some(f64::NAN)).is_err());
        // 120 muestras cada 2.5 s = 297.5 s: dentro; cada 3 s = 357 s: fuera.
        assert!(validate_watch(&t, Some(120), Some(2.5)).is_ok());
        assert!(validate_watch(&t, Some(120), Some(3.0)).unwrap_err().contains("300 s"));
    }

    #[test]
    fn watch_resumen_de_cambios() {
        let s = |t: &str, a: &str, b: &str| WatchSample {
            t: t.into(),
            values: vec![("A".into(), a.into()), ("B".into(), b.into())],
        };
        let muestras = vec![s("10:00:00.000", "0", "1"), s("10:00:01.000", "0", "1"), s("10:00:02.000", "1", "1"), s("10:00:03.000", "0", "2")];
        let tags = vec!["A".to_string(), "B".to_string()];
        let j = watch_json(&tags, &muestras, 4, 1.0, None);
        assert_eq!(j["samples_taken"], 4);
        assert_eq!(j["samples"][2]["values"]["A"], "1");
        assert_eq!(j["summary"]["A"]["distinct_values"], 2);
        assert_eq!(j["summary"]["A"]["changes"], 2);
        assert_eq!(j["summary"]["A"]["first_change_t"], "10:00:02.000");
        assert_eq!(j["summary"]["B"]["changes"], 1);
        assert_eq!(j["summary"]["B"]["first_change_t"], "10:00:03.000");
        assert_eq!(j["summary"]["B"]["last"], "2");
        assert!(j.get("interrupted").is_none());
        let corte = watch_json(&tags, &muestras[..1], 4, 1.0, Some("error Citect 233".into()));
        assert_eq!(corte["interrupted"], "error Citect 233");
        assert_eq!(corte["summary"]["A"]["changes"], 0);
        assert!(corte["summary"]["A"]["first_change_t"].is_null());
    }
}
