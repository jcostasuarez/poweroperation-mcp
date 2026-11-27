//! End-to-end del camino CtAPI contra el CtApi.dll falso de `tests/fakectapi` (escrituras habilitadas).
//! La prueba compila ese DLL con el mismo cargo que corre los tests.

mod common;

use std::path::PathBuf;
use std::process::Command;

use common::{Checks, Mcp, fake_projects, json_of, temp_dir};
use serde_json::{Value, json};

/// Compila `tests/fakectapi` y devuelve la carpeta con su `CtApi.dll`.
fn build_fake_ctapi() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fakectapi");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args(["build", "--release", "--quiet", "--manifest-path"])
        .arg(dir.join("Cargo.toml"))
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .expect("no se pudo ejecutar cargo");
    assert!(out.status.success(), "no compiló tests/fakectapi:\n{}", String::from_utf8_lossy(&out.stderr));
    let release = dir.join("target").join("release");
    assert!(release.join("CtApi.dll").is_file(), "falta {}", release.join("CtApi.dll").display());
    release
}

#[test]
fn runtime_tools_with_fake_ctapi() {
    let dll_dir = build_fake_ctapi();
    let work = temp_dir("runtime");
    let root = work.join("fakeuser");
    fake_projects(&root);
    let mut mcp = Mcp::start(
        &work,
        &[
            ("POWEROP_CTAPI", dll_dir.display().to_string()),
            ("POWEROP_ALLOW_WRITE", "1".into()),
            ("POWEROP_PROJECTS_DIR", root.display().to_string()),
        ],
    );
    let mut c = Checks::default();

    let tools = mcp.tool_names();
    c.check(
        "con CtAPI y escrituras se listan las de CTAPI y ESCRITURA",
        ["po_tag_read", "po_tag_write", "po_alarm_ack", "po_cicode_run", "po_cicode_write", "po_runtime_control"].iter().all(|t| tools.iter().any(|x| x == t)),
        &tools.join(", "),
    );
    c.check("sin servidor de alarmas no se lista po_alarm_sql", !tools.iter().any(|x| x == "po_alarm_sql"), &tools.join(", "));

    let (_, t) = mcp.call("po_status", json!({}));
    c.check("po_status: CTAPI conectado", t.contains("CTAPI: DISPONIBLE") && t.contains("hora del SCADA 10:15:00"), &t);
    c.check("po_status muestra proyecto en runtime", t.contains("Proyecto en runtime: PO_Demo"), &t);

    let (e, t) = mcp.call("po_project_query", json!({"table": "variable", "limit": 1}));
    c.check("proyecto por defecto = el del runtime", !e && t.contains("\"matched\": 3"), &t);

    let (e, t) = mcp.call("po_alarm_sql", json!({"sql": "select * from CiAlarmObject"}));
    c.check("po_alarm_sql oculta: dice por qué (puerto o driver)", e && (t.contains("puerto TCP") || t.contains("no instalado")), &t);

    if tools.iter().any(|x| x == "po_gb_page_edit") {
        // Solo si Graphics Builder está registrado en este equipo; el campo desconocido se rechaza sin abrirlo.
        let (e, t) = mcp.call("po_gb_page_edit", json!({"page": "Feeders", "ops": [{"op": "move", "an": 1, "x": 0, "y": 0}, {"op": "line", "x": 0, "y": 0, "x2": 1, "y2": 1, "visible": "A"}]}));
        c.check(
            "campo desconocido dentro de ops: se rechaza y dice dónde",
            e && t.contains("Campo desconocido 'visible' en ops[1]; ¿quisiste decir 'visible_when'?") && t.contains("Campos válidos ahí:"),
            &t,
        );
    }

    let (e, t) = mcp.call("po_tag_read", json!({"tags": ["Feeder1\\XCBR1\\Pos"], "samples": 3, "interval_secs": 0.2}));
    let j = json_of(&t);
    c.check("po_tag_read con samples toma muestras", !e && t.contains("Feeder1\\\\XCBR1\\\\Pos") && j.to_string().contains("samples"), &t);
    let (e, t) = mcp.call("po_tag_read", json!({"tags": ["A"], "samples": 121}));
    c.check("po_tag_read con samples valida límites", e && t.contains("entre 1 y 120"), &t);
    let (e, t) = mcp.call("po_runtime_control", json!({"action": "stop"}));
    c.check("po_runtime_control rechaza action inválida sin hacer nada", e && t.contains("\"start\" o \"restart\""), &t);

    let (_, t) = mcp.call("po_tag_read", json!({"tags": ["Feeder1\\MMXU1\\TotW", "NoExiste"]}));
    let j = json_of(&t);
    c.check("tag_read valor", j[0]["value"] == "1234.5", &t);
    let e1 = j[1]["error"].as_str().unwrap_or("");
    c.check("tag_read error Citect decodificado (424 = tag no encontrado)", e1.contains("error Citect 424") && e1.contains("0x100001A8"), &t);
    c.check("tag_read error Citect con su título de la ayuda", e1.contains("\"Tag not found\""), &t);
    c.check("tag_read con calidad", j[0]["quality"] == "GOOD" && j[0].get("quality_detail").is_none(), &t);
    // ctTagReadEx no falla con un tag inexistente (da INVALID_TAG): el MCP muestra el error de ctTagRead.
    let (_, t) = mcp.call("po_tag_read", json!({"tags": ["NoExiste"]}));
    let e = json_of(&t)[0]["error"].as_str().unwrap_or("").to_string();
    c.check("tag_read de un tag inexistente solo = error 424", e.contains("error Citect 424") && e.contains("\"Tag not found\""), &t);
    // El falso da 10:00:05Z por lista y 10:00:00Z por ctTagReadEx: varios tags van en un pedido (lista) y el
    // que la lista no devuelve se relee por ctTagReadEx para tener el error real.
    let ts = |v: &Value| v["timestamp"].as_str().and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|d| d.timestamp());
    c.check("varios tags: lectura por lista (hora de la lista)", ts(&j[0]) == Some(1_795_773_605), &t);

    let (_, t) = mcp.call("po_tag_read", json!({"tags": ["Feeder2\\MMXU1\\TotW"]}));
    let j = json_of(&t);
    c.check(
        "un tag: ctTagReadEx con calidad mala y detalle",
        j[0]["quality"] == "BAD/COMM_FAILURE" && j[0]["quality_detail"]["extended"] == "COMM_DEV_BAD" && ts(&j[0]) == Some(1_795_773_600),
        &t,
    );

    let (_, t) = mcp.call("po_tag_read", json!({"tags": ["Feeder1\\MMXU1\\TotW"], "details": true}));
    let j = json_of(&t);
    c.check(
        "details: unidades, descripción, rango y tipo (ctTagGetProperty)",
        j[0]["units"] == "kW" && j[0]["description"] == "Potencia total" && j[0]["eng_high"] == "1000" && j[0]["type"] == "REAL",
        &t,
    );

    let (_, t) = mcp.call("po_tag_read", json!({"tags": ["Feeder1\\MMXU1\\TotW", "Feeder2\\MMXU1\\TotW"], "samples": 2, "interval_secs": 0.2}));
    c.check("samples marca la calidad mala en el valor", t.contains("0 [BAD/COMM_FAILURE]"), &t);

    let (_, t) = mcp.call("po_tag_write", json!({"tag": "Feeder1\\XCBR1\\Pos", "value": "0"}));
    let j = json_of(&t);
    c.check("tag_write escribe y relee", j["before"] == "2" && j["after"] == "0", &t);

    let (_, t) = mcp.call("po_tag_browse", json!({"filter": "TAG=Feeder1*", "with_values": true}));
    let j = json_of(&t);
    let unavailable = j["unavailable_properties"].as_array().cloned().unwrap_or_default();
    c.check(
        "tag_browse filtra, lee valores, traduce TYPE y quita props inexistentes",
        j["count"] == 2 && j["rows"][0]["VALUE"] == "1234.5" && j["rows"][0]["TYPE_NAME"] == "REAL" && unavailable.contains(&json!("ADDR")),
        &t,
    );

    let (_, t) = mcp.call("po_tag_browse", json!({"filter": "TAG=Nada*"}));
    let j = json_of(&t);
    c.check("tag_browse vacío con nota", j["count"] == 0 && j["note"].as_str().unwrap_or("").contains("error Citect 275"), &t);

    let (_, t) = mcp.call("po_trend_query", json!({"tag": "Feeder1_kW", "end": "2026-10-14 10:00", "minutes_back": 10, "period_seconds": 60}));
    let j = json_of(&t);
    let rows = j["rows"].as_array().cloned().unwrap_or_default();
    c.check(
        "trend_query 11 muestras (inicio y fin incluidos), forma de 7 argumentos, orden cronológico",
        j["returned"] == 11
            && j["query"].as_str().is_some_and(|q| q.split(',').count() == 8)
            && rows.last().and_then(|r| r[0].as_str()).is_some_and(|s| s.starts_with("2026-10-14 10:00:00"))
            && rows.first().and_then(|r| r[0].as_str()).is_some_and(|s| s.starts_with("2026-10-14 09:50:00")),
        &t,
    );
    c.check(
        "trend_query estadísticas solo con calidad buena",
        j["stats"]["good_samples"] == 10 && j["stats"]["min"] == 1.5 && rows.first().is_some_and(|r| r[2] == "bad"),
        &t,
    );

    let (_, t) = mcp.call("po_trend_query", json!({"tag": "X", "minutes_back": 5, "period_seconds": 60, "format": "csv"}));
    c.check("trend_query CSV", t.contains("time,value,quality") && t.matches('\n').count() > 5, &t);

    let (_, t) = mcp.call("po_alarm_list", json!({}));
    c.check("alarm_list", json_of(&t)["rows"][0]["ALARMTYPE"] == "Digital", &t);

    let (_, t) = mcp.call("po_alarm_list", json!({"summary": true}));
    c.check("alarm_list summary usa AlarmSummary", json_of(&t)["rows"][0]["OFFTIME"] == "10:14:10", &t);

    let (_, t) = mcp.call("po_alarm_ack", json!({"tag": "Feeder1_\"Trip"}));
    c.check("alarm_ack con escape Cicode", t.contains("AlarmAckTag(\"Feeder1_^\"Trip\", \"\") → 0"), &t);

    let (_, t) = mcp.call("po_cicode_run", json!({"command": "TagWrite(\"X\", 1)"}));
    c.check("cicode_run con efectos permitido con ALLOW_WRITE", t == "eval:TagWrite(\"X\", 1)", &t);

    let (_, t) = mcp.call("po_cicode_run", json!({"command": "IntToStr(1)"}));
    c.check("cicode_run lectura", t == "eval:IntToStr(1)", &t);

    let (_, t) = mcp.call("po_ctapi_find", json!({"table": "Tag", "properties": ["tag", "type"], "limit": 2}));
    let j = json_of(&t);
    c.check("ctapi_find con limit y props en mayúsculas", j["count"] == 2 && j["truncated"] == true && j["rows"][0]["TYPE"] == "2", &t);

    let (_, t) = mcp.call("po_status", json!({"reconnect": true}));
    c.check("reconnect", t.contains("CTAPI: DISPONIBLE"), &t);

    let w = "x-new.ci";
    let (e, t) = mcp.call("po_cicode_write", json!({"project": "PO_Demo", "file": w, "content": "FUNCTION Z()\nEND\n"}));
    c.check("cicode_write crea", !e && t.contains("creado"), &t);
    let (_, t) = mcp.call("po_cicode_write", json!({"project": "PO_Demo", "file": w, "content": "FUNCTION Z()\n IF 1 THEN\nEND\n"}));
    c.check("cicode_write reemplaza con backup y reporta chequeo", t.contains("_mcp_backup") && t.contains("\"ok\": false"), &t);
    let raw = std::fs::read(root.join("PO_Demo").join(w)).unwrap_or_default();
    let has = |pat: &[u8]| raw.windows(pat.len()).any(|x| x == pat);
    c.check("cicode_write CRLF", has(b"\r\n") && !has(b"\r\r"), &String::from_utf8_lossy(&raw));

    drop(mcp);
    let audit = std::fs::read_to_string(work.join("audit.log")).unwrap_or_default();
    c.check(
        "auditoría registra escrituras",
        ["tag_write", "alarm_ack", "cicode_run", "cicode_write"].iter().all(|k| audit.contains(k)),
        &audit,
    );

    // Mismo CtApi.dll falso en solo lectura: las de escritura no se listan y cicode_run filtra efectos.
    let mut mcp = Mcp::start(
        &work,
        &[("POWEROP_CTAPI", dll_dir.display().to_string()), ("POWEROP_PROJECTS_DIR", root.display().to_string())],
    );
    let tools = mcp.tool_names();
    c.check(
        "solo lectura: CTAPI sí, escrituras no",
        tools.iter().any(|x| x == "po_cicode_run") && !tools.iter().any(|x| x == "po_tag_write" || x == "po_alarm_ack" || x == "po_runtime_control"),
        &tools.join(", "),
    );
    let (e, t) = mcp.call("po_cicode_run", json!({"command": "TagWrite(\"X\", 1)"}));
    c.check("po_cicode_run con efectos bloqueado en solo lectura", e && t.to_uppercase().contains("TAGWRITE"), &t);
    let (e, t) = mcp.call("po_cicode_run", json!({"cmd": "TagRead(\"X\")"}));
    c.check(
        "argumentos mal escritos listan campos válidos y sugieren el correcto",
        e && t.contains("Campos válidos: command (obligatorio), window")
            && t.contains("Campo desconocido 'cmd'; ¿quisiste decir 'command'?")
            && t.contains("Esquema de entrada:"),
        &t,
    );
    let (e, t) = mcp.call("po_trend_query", json!({"tag": "A,B"}));
    c.check("po_trend_query valida comas", e, &t);
    let (e, t) = mcp.call("po_tag_write", json!({"tag": "Feeder1\\XCBR1\\Pos", "value": "0"}));
    c.check("po_tag_write oculta en solo lectura", e && t.contains("solo lectura") && !t.contains("puente CTAPI"), &t);
    drop(mcp);

    let _ = std::fs::remove_dir_all(&work);
    c.finish();
}
