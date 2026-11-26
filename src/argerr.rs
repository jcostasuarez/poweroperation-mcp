//! Mensajes claros cuando fallan los argumentos de una herramienta: rmcp solo dice "missing field `command`".
//! Acá se arma un texto que lista los campos válidos (marcando los obligatorios), señala los campos
//! desconocidos con una sugerencia ("¿quisiste decir 'command'?") y adjunta el JSON Schema de entrada.

use serde_json::{Map, Value};

/// Prefijo con que rmcp reporta los errores de deserialización de argumentos (`router/tool.rs` de rmcp).
const PREFIJO_DESERIALIZACION: &str = "failed to deserialize parameters:";

/// Largo máximo del mensaje para el agente (el esquema se recorta para respetarlo).
pub const MAX_MENSAJE: usize = 3000;

/// Si `texto` es un error de deserialización de argumentos de rmcp, devuelve el detalle
/// (p. ej. "missing field `command`"). Si no, None.
pub fn detalle_deserializacion(texto: &str) -> Option<&str> {
    texto.strip_prefix(PREFIJO_DESERIALIZACION).map(str::trim)
}

/// Campos que mandó el agente y que el esquema no tiene. serde los ignora sin error, así que un opcional mal
/// escrito (`interval_s` por `interval_secs`) pasaba desapercibido (verificado con tag_watch). Solo se decide con
/// esquemas de objeto simples: si el esquema compone tipos o admite propiedades adicionales, no se informa nada.
pub fn desconocidos(schema: &Value, args: &Map<String, Value>) -> Vec<String> {
    let compuesto = ["$ref", "allOf", "anyOf", "oneOf"].iter().any(|k| schema.get(*k).is_some())
        || schema.get("additionalProperties").is_some_and(|v| v != &Value::Bool(false));
    if compuesto {
        return vec![];
    }
    let props = schema.get("properties").and_then(Value::as_object);
    args.keys().filter(|k| !props.is_some_and(|p| p.contains_key(k.as_str()))).cloned().collect()
}

/// Arma el mensaje de error de argumentos para la herramienta `herramienta`.
/// `detalle`: el error de serde; `schema`: el JSON Schema de entrada de la herramienta;
/// `args`: los argumentos que mandó el agente.
pub fn mensaje(herramienta: &str, detalle: &str, schema: &Value, args: &Map<String, Value>) -> String {
    let props: Vec<&str> = schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|p| p.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let requeridos: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let mut out = format!("Parámetros inválidos para '{herramienta}': {detalle}.\n");

    if props.is_empty() {
        out.push_str("La herramienta no admite parámetros.\n");
    } else {
        let lista: Vec<String> = props
            .iter()
            .map(|p| if requeridos.contains(p) { format!("{p} (obligatorio)") } else { (*p).to_string() })
            .collect();
        out.push_str(&format!("Campos válidos: {}.\n", lista.join(", ")));
    }

    let faltan: Vec<String> = requeridos.iter().filter(|r| !args.contains_key(**r)).map(|r| format!("'{r}'")).collect();
    if !faltan.is_empty() {
        out.push_str(&format!("Faltan obligatorios: {}.\n", faltan.join(", ")));
    }

    // Candidatos para sugerir: solo campos válidos que el agente no mandó (si mandó 'command', no sugerir 'command').
    let libres: Vec<&str> = props.iter().copied().filter(|p| !args.contains_key(*p)).collect();
    for k in args.keys().filter(|k| !props.contains(&k.as_str())) {
        match sugerir(k, &libres) {
            Some(s) => out.push_str(&format!("Campo desconocido '{k}'; ¿quisiste decir '{s}'?\n")),
            None => out.push_str(&format!("Campo desconocido '{k}'.\n")),
        }
    }

    // Campo desconocido anidado (p. ej. dentro de `ops`, que rechaza serde con deny_unknown_fields): dónde está,
    // qué campos valen ahí y una sugerencia.
    if let Some((campo, esperados)) = campo_desconocido_serde(detalle)
        && !args.contains_key(&campo)
    {
        let mut donde = Vec::new();
        for (k, v) in args {
            rutas(v, &campo, k.clone(), &mut donde);
        }
        let validos: Vec<&str> = esperados.iter().map(String::as_str).collect();
        let lugar = if donde.is_empty() { String::new() } else { format!(" en {}", donde.join(", ")) };
        match sugerir(&campo, &validos) {
            Some(s) => out.push_str(&format!("Campo desconocido '{campo}'{lugar}; ¿quisiste decir '{s}'?\n")),
            None => out.push_str(&format!("Campo desconocido '{campo}'{lugar}.\n")),
        }
        if validos.is_empty() {
            out.push_str("Ahí no se admiten campos.\n");
        } else {
            out.push_str(&format!("Campos válidos ahí: {}.\n", validos.join(", ")));
        }
    }

    // Esquema, recortado para que el mensaje total no pase MAX_MENSAJE caracteres.
    const ROTULO: &str = "Esquema de entrada: ";
    let usados = out.chars().count() + ROTULO.chars().count() + 1;
    let disponible = MAX_MENSAJE.saturating_sub(usados);
    if disponible > 0 {
        let esquema = serde_json::to_string(schema).unwrap_or_default();
        let texto = if esquema.chars().count() <= disponible {
            esquema
        } else {
            let corto: String = esquema.chars().take(disponible.saturating_sub(1)).collect();
            format!("{corto}…")
        };
        out.push_str(ROTULO);
        out.push_str(&texto);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Campo y nombres válidos de un error "unknown field" de serde: "unknown field `x`, expected one of `a`, `b`",
/// "... expected `a` or `b`", "... expected `a`" o "... there are no fields".
pub fn campo_desconocido_serde(detalle: &str) -> Option<(String, Vec<String>)> {
    let resto = &detalle[detalle.find("unknown field `")? + "unknown field `".len()..];
    let fin = resto.find('`')?;
    let esperados = resto[fin + 1..].split('`').skip(1).step_by(2).map(str::to_string).collect();
    Some((resto[..fin].to_string(), esperados))
}

/// Rutas (estilo `ops[1].fill`) de los objetos dentro de `v` que tienen la clave `campo`.
fn rutas(v: &Value, campo: &str, ruta: String, out: &mut Vec<String>) {
    if out.len() >= 5 {
        return;
    }
    match v {
        Value::Object(o) => {
            if o.contains_key(campo) {
                out.push(ruta.clone());
            }
            for (k, hijo) in o {
                rutas(hijo, campo, format!("{ruta}.{k}"), out);
            }
        }
        Value::Array(a) => {
            for (i, hijo) in a.iter().enumerate() {
                rutas(hijo, campo, format!("{ruta}[{i}]"), out);
            }
        }
        _ => {}
    }
}

/// Nombre válido más parecido a `recibido` (el que el agente escribió mal), o None si ninguno se parece.
/// Ignora mayúsculas, guiones y guiones bajos. Criterios, de mejor a peor: igual; distancia de edición
/// <= 2 (p. ej. "comando" -> "command"); y `recibido` es subsecuencia de un nombre que empieza igual
/// (p. ej. "cmd" -> "command").
pub fn sugerir<'a>(recibido: &str, validos: &[&'a str]) -> Option<&'a str> {
    let norm = |s: &str| -> Vec<char> { s.chars().filter(|c| *c != '_' && *c != '-').flat_map(char::to_lowercase).collect() };
    let r = norm(recibido);
    if r.is_empty() {
        return None;
    }
    let mut mejor: Option<(usize, &'a str)> = None;
    for &v in validos {
        let n = norm(v);
        let puntaje = if n == r {
            0
        } else {
            let d = distancia(&r, &n);
            if d <= 2 {
                d
            } else if r.len() >= 2 && r.first() == n.first() && es_subsecuencia(&r, &n) {
                10 + n.len() - r.len()
            } else {
                continue;
            }
        };
        if mejor.is_none_or(|(p, _)| puntaje < p) {
            mejor = Some((puntaje, v));
        }
    }
    mejor.map(|(_, v)| v)
}

/// Distancia de Levenshtein entre dos secuencias de caracteres.
fn distancia(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let costo = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + costo).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// `corto` aparece en `largo` respetando el orden de los caracteres ("cmd" en "command").
fn es_subsecuencia(corto: &[char], largo: &[char]) -> bool {
    let mut it = largo.iter();
    corto.iter().all(|c| it.any(|l| l == c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema_cicode() -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Llamada Cicode"},
                "window": {"type": "integer", "description": "Ventana"}
            },
            "required": ["command"]
        })
    }

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn campo_mal_escrito_sugiere_el_correcto() {
        let m = mensaje("cicode_run", "missing field `command`", &schema_cicode(), &args(json!({"cmd": "Time(1)"})));
        assert!(m.contains("Parámetros inválidos para 'cicode_run': missing field `command`."), "{m}");
        assert!(m.contains("Campos válidos: command (obligatorio), window."), "{m}");
        assert!(m.contains("Faltan obligatorios: 'command'."), "{m}");
        assert!(m.contains("Campo desconocido 'cmd'; ¿quisiste decir 'command'?"), "{m}");
        assert!(m.contains("Esquema de entrada: {"), "{m}");
    }

    /// Sin sugerencia si el campo parecido ya se envió ('cmd' con 'command') o si no hay ninguno parecido.
    #[test]
    fn no_sugiere_un_campo_que_ya_se_envio_ni_sin_parecido() {
        for extra in ["cmd", "zzzz"] {
            let m = mensaje("cicode_run", "x", &schema_cicode(), &args(json!({"command": "a", extra: "b"})));
            assert!(m.contains(&format!("Campo desconocido '{extra}'.\n")), "{m}");
            assert!(!m.contains("quisiste decir"), "{m}");
            assert!(!m.contains("Faltan obligatorios"), "{m}");
        }
    }

    #[test]
    fn desconocidos_solo_con_esquemas_simples() {
        let s = json!({"type": "object", "properties": {"tags": {}, "interval_secs": {}}});
        assert_eq!(desconocidos(&s, &args(json!({"tags": [], "interval_s": 2}))), vec!["interval_s".to_string()]);
        assert!(desconocidos(&s, &args(json!({"tags": [], "interval_secs": 2}))).is_empty());
        // Sin parámetros: cualquier campo sobra.
        assert_eq!(desconocidos(&json!({"type": "object"}), &args(json!({"x": 1}))), vec!["x".to_string()]);
        // Esquemas compuestos o abiertos: no se decide.
        assert!(desconocidos(&json!({"anyOf": [], "properties": {}}), &args(json!({"x": 1}))).is_empty());
        assert!(desconocidos(&json!({"additionalProperties": true, "properties": {}}), &args(json!({"x": 1}))).is_empty());
    }

    #[test]
    fn campo_desconocido_anidado() {
        let s = json!({"type": "object", "properties": {"page": {}, "ops": {}}, "required": ["page", "ops"]});
        let a = args(json!({"page": "P", "ops": [{"op": "delete", "an": 1}, {"op": "rectangle", "fill": {"colours": [{"state": 0, "color": "#000"}]}}]}));
        let detalle = "unknown field `color`, expected `state` or `colour`";
        let m = mensaje("gb_page_edit", detalle, &s, &a);
        assert!(m.contains("Campo desconocido 'color' en ops[1].fill.colours[0]; ¿quisiste decir 'colour'?"), "{m}");
        assert!(m.contains("Campos válidos ahí: state, colour."), "{m}");
        // Formatos de serde: uno, dos, varios, ninguno.
        let c = |d: &str| campo_desconocido_serde(d).unwrap();
        assert_eq!(c("unknown field `z`, expected `an`"), ("z".into(), vec!["an".to_string()]));
        assert_eq!(c("unknown field `vis`, expected one of `op`, `x`, `visible_when`").1, ["op", "x", "visible_when"]);
        assert!(c("unknown field `q`, there are no fields").1.is_empty());
        assert!(campo_desconocido_serde("missing field `page`").is_none());
        // Un campo de primer nivel no se repite como anidado.
        let m = mensaje("x", "unknown field `zzz`, expected `page`", &s, &args(json!({"zzz": 1})));
        assert_eq!(m.matches("Campo desconocido 'zzz'").count(), 1, "{m}");
    }

    #[test]
    fn herramienta_sin_parametros() {
        let s = json!({"type": "object", "properties": {}});
        let m = mensaje("runtime_status", "x", &s, &args(json!({"foo": 1})));
        assert!(m.contains("La herramienta no admite parámetros."), "{m}");
        assert!(m.contains("Campo desconocido 'foo'."), "{m}");
    }

    #[test]
    fn mensaje_acotado_aunque_el_esquema_sea_enorme() {
        let grande = "x".repeat(10_000);
        let s = json!({"type": "object", "properties": {"command": {"type": "string", "description": grande}}, "required": ["command"]});
        let m = mensaje("cicode_run", "x", &s, &args(json!({})));
        assert!(m.chars().count() <= MAX_MENSAJE, "largo {}", m.chars().count());
        assert!(m.contains("Campos válidos: command (obligatorio)."), "{m}");
        assert!(m.contains('…'), "el esquema debe quedar recortado");
    }

    #[test]
    fn sugerencias() {
        let v = ["command", "window"];
        assert_eq!(sugerir("Command", &v), Some("command"));
        assert_eq!(sugerir("comando", &v), Some("command"));
        assert_eq!(sugerir("cmd", &v), Some("command"));
        assert_eq!(sugerir("windw", &v), Some("window"));
        assert_eq!(sugerir("zzz", &v), None);
        assert_eq!(sugerir("", &v), None);
        assert_eq!(sugerir("cmd", &["window"]), None);
    }

    #[test]
    fn detalle_solo_de_errores_de_argumentos() {
        assert_eq!(detalle_deserializacion("failed to deserialize parameters: missing field `command`"), Some("missing field `command`"));
        assert_eq!(detalle_deserializacion("tag no encontrado"), None);
    }
}
