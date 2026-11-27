//! Verificación contra un Power Operation real con el runtime de `MCP_Demo` corriendo (solo lectura).
//! No corre con `cargo test`: `cargo test --test live -- --ignored --nocapture`.
//! `MCP_Demo` es el proyecto de pruebas (memoria GENERIC, tags `Feeder1\...` y `MCP\Test\...`, alarmas y
//! trends sobre `Feeder1\MMXU1\TotW`, simulación en `MCP_Sim.ci`). POWEROP_LIVE_PROJECT cambia el proyecto.

mod common;

use common::{Checks, Mcp, json_of, temp_dir};
use serde_json::{Value, json};

fn show(name: &str, t: &str) {
    println!("\n===== {name}\n{}", t.chars().take(2500).collect::<String>());
}

fn no_unavailable(j: &Value) -> bool {
    j.get("unavailable_properties").is_none()
}

#[test]
#[ignore = "necesita el runtime de Power Operation corriendo"]
fn verify_mcp_demo() {
    let work = temp_dir("live");
    let project = std::env::var("POWEROP_LIVE_PROJECT").unwrap_or_else(|_| "MCP_Demo".into());
    let mut mcp = Mcp::start_live(&work, &[("POWEROP_PROJECT", project.clone())]);
    let mut c = Checks::default();

    let (_, t) = mcp.call("po_status", json!({}));
    show("po_status", &t);
    c.check("CtAPI conecta", t.contains("CTAPI: DISPONIBLE"), &t);
    c.check("proyecto en runtime (ProjectInfo(0))", t.contains(&format!("Proyecto en runtime: {project}")), &t);
    c.check("servidor de alarmas desde el proyecto", t.contains("localhost:5482") && t.contains("servidor AlarmServer1"), &t);

    let (_, t) = mcp.call("po_tag_read", json!({"tags": ["Feeder1\\MMXU1\\TotW", "Feeder1\\XCBR1\\Pos", "NoExiste"]}));
    show("tag_read", &t);
    let j = json_of(&t);
    c.check("tag_read REAL", j[0]["value"].as_str().is_some_and(|v| v.trim().parse::<f64>().is_ok()), &t);
    c.check("tag_read INT", j[1]["value"] == "1" || j[1]["value"] == "2", &t);
    c.check("tag inexistente = error Citect 424", j[2]["error"].as_str().is_some_and(|e| e.contains("error Citect 424")), &t);

    let (_, t) = mcp.call("po_tag_browse", json!({"filter": "TAG=Feeder1*", "with_values": true}));
    show("tag_browse", &t);
    let j = json_of(&t);
    c.check("tag_browse: 4 tags, propiedades por defecto existen", j["count"] == 4 && no_unavailable(&j), &t);
    c.check(
        "tag_browse: TYPE código + TYPE_NAME, EQUIPMENT e ITEM",
        j["rows"].as_array().is_some_and(|r| r.iter().any(|x| x["TYPE"] == "2" && x["TYPE_NAME"] == "REAL" && x["EQUIPMENT"] == "Feeder1" && x["ITEM"] == "TotW")),
        &t,
    );

    let (_, t) = mcp.call("po_alarm_list", json!({}));
    show("alarm_list", &t);
    let j = json_of(&t);
    c.check("alarm_list: 2 alarmas, campos por defecto existen", j["count"] == 2 && no_unavailable(&j), &t);

    let (_, t) = mcp.call("po_alarm_list", json!({"summary": true, "limit": 5}));
    show("alarm_list summary", &t);
    let j = json_of(&t);
    c.check("alarm_list summary: filas y campos por defecto existen", j["count"].as_u64().unwrap_or(0) >= 1 && no_unavailable(&j), &t);

    let (_, t) = mcp.call("po_trend_query", json!({"tag": "Feeder1\\MMXU1\\TotW", "minutes_back": 1, "period_seconds": 5}));
    show("trend_query", &t);
    let j = json_of(&t);
    c.check(
        "trend_query devuelve muestras buenas en orden cronológico",
        j["returned"].as_u64().unwrap_or(0) >= 10 && j["stats"]["good_samples"].as_u64().unwrap_or(0) >= 5 && j["note"].is_null(),
        &t,
    );
    let (_, t) = mcp.call("po_trend_query", json!({"tag": "Feeder1\\MMXU1\\TotW", "minutes_back": 1, "data_mode": 1, "max_samples": 30}));
    show("trend_query data_mode=1", &t);
    c.check("trend_query data_mode=1 (muestras crudas)", json_of(&t)["returned"].as_u64().unwrap_or(0) >= 1, &t);

    let (e, t) = mcp.call("po_alarm_sql", json!({"sql": "select AlarmSource, DisplayName, AlarmState, OnTime from CiAlarmObject", "limit": 5}));
    show("alarm_sql CiAlarmObject", &t);
    c.check("alarm_sql CiAlarmObject", !e && json_of(&t)["count"].as_u64().unwrap_or(0) >= 1 && t.contains("AlarmSource"), &t);
    let (e, t) = mcp.call("po_alarm_sql", json!({"sql": "select * from CDBEventJournal", "limit": 3}));
    show("alarm_sql CDBEventJournal", &t);
    c.check("alarm_sql CDBEventJournal", !e, &t);

    let (_, t) = mcp.call("po_cicode_run", json!({"command": "TagRead(\"Feeder1\\MMXU1\\TotW\")"}));
    c.check("cicode_run TagRead", t.trim().parse::<f64>().is_ok(), &t);
    let (e, t) = mcp.call("po_cicode_run", json!({"command": "Feeder1\\MMXU1\\TotW"}));
    c.check("tag con '\\' como identificador Cicode = error 292", e && t.contains("error Citect 292"), &t);

    drop(mcp);
    let _ = std::fs::remove_dir_all(&work);
    c.finish();
}

/// Graphics Builder por COM (solo lectura): lo abre si hace falta. Explora librerías, un genie, una página de
/// Include y su miniatura (guardada en POWEROP_LIVE_OUT si está definida).
#[test]
#[ignore = "necesita Power Operation (abre Graphics Builder)"]
fn gb_read() {
    let work = temp_dir("live-gb");
    let mut mcp = Mcp::start_live(&work, &[("POWEROP_PROJECT", "MCP_Demo".into()), ("POWEROP_TIMEOUT_SECS", "150".into())]);
    let mut c = Checks::default();
    let calls = [
        ("po_gb_status", json!({"launch": true}), ""),
        ("po_project_graphics", json!({}), ""),
        // Estilo, genies y símbolos que usa gb_write.
        ("po_gb_library", json!({"style": "standard"}), "\"normal\""),
        ("po_gb_library", json!({"library": "CONTROLS"}), "Toggle_Button_1"),
        ("po_gb_library", json!({"library": "CONTROLS", "element": "Toggle_Button_1"}), "\"properties\""),
        ("po_gb_library", json!({"library": "lights_round", "kind": "symbol"}), "green"),
        ("po_gb_page_read", json!({"project": "Include", "page": "PageMenu", "max_objects": 15}), "\"objects\""),
    ];
    for (name, args, expect) in calls {
        let (e, t) = mcp.call(name, args.clone());
        show(&format!("{name} {args}{}", if e { " [ERROR]" } else { "" }), &t);
        c.check(&format!("{name} {args}"), !e && t.contains(expect), &t);
    }

    let r = mcp.rpc("tools/call", json!({"name": "po_gb_page_thumbnail", "arguments": {"project": "Include", "page": "PageMenu"}}));
    let img = r["result"]["content"].as_array().and_then(|c| c.iter().find(|x| x["type"] == "image").cloned());
    c.check("miniatura PNG de PageMenu", img.as_ref().is_some_and(|i| i["mimeType"] == "image/png"), &r.to_string());
    if let Some(img) = img {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD.decode(img["data"].as_str().unwrap_or("")).unwrap_or_default();
        c.check("miniatura con firma PNG", bytes.starts_with(b"\x89PNG\r\n\x1a\n"), &format!("{:02X?}", &bytes[..8.min(bytes.len())]));
        if let Ok(dir) = std::env::var("POWEROP_LIVE_OUT") {
            let path = std::path::Path::new(&dir).join("pagemenu.png");
            std::fs::write(&path, &bytes).unwrap();
            println!("\n===== miniatura guardada en {} ({} bytes)", path.display(), bytes.len());
        }
    }
    drop(mcp);
    let _ = std::fs::remove_dir_all(&work);
    c.finish();
}

/// Graphics Builder: crea, lee, edita y fotografía la página `MCP_GB_Test` de MCP_Demo (escrituras habilitadas).
#[test]
#[ignore = "necesita Graphics Builder abierto; escribe en MCP_Demo"]
fn gb_write() {
    let work = temp_dir("live-gbw");
    let mut mcp = Mcp::start_live(
        &work,
        &[("POWEROP_PROJECT", "MCP_Demo".into()), ("POWEROP_TIMEOUT_SECS", "150".into()), ("POWEROP_ALLOW_WRITE", "1".into())],
    );
    let mut c = Checks::default();
    let page = "MCP_GB_Test";
    let (e, t) = mcp.call("po_gb_status", json!({"launch": true}));
    c.check("gb_status conecta", !e, &t);
    let (e, t) = mcp.call("po_gb_page_delete", json!({"page": page}));
    show("gb_page_delete previo", &format!("{}{t}", if e { "[ERROR] " } else { "" }));
    c.check("gb_page_delete respalda (o no existía)", (!e && t.contains("_mcp_backup")) || t.contains("No existe la página"), &t);

    let (e, t) = mcp.call(
        "po_gb_page_create",
        json!({"page": page, "style": "standard", "template": "normal", "resolution": 3, "titlebar": false, "title": "Prueba del MCP",
               "ops": [
                   {"op": "text", "text": "Feeder1 - página creada por el MCP", "x": 40, "y": 60, "font_size": 18},
                   {"op": "rectangle", "x": 30, "y": 100, "x2": 700, "y2": 400},
                   {"op": "genie", "library": "CONTROLS", "element": "Toggle_Button_1", "x": 80, "y": 160,
                    "properties": {"Tag": r"Feeder1\XCBR1\Trip", "Button_Text": "Disparo"}},
                   {"op": "symbol", "library": "lights_round", "element": "green", "x": 400, "y": 170},
                   {"op": "button", "x": 400, "y": 300, "x2": 560, "y2": 340, "text": "Touch", "command": "TraceMsg(\"down1\")", "up_command": "TraceMsg(\"up1\")"}
               ]}),
    );
    show("gb_page_create", &t);
    c.check("gb_page_create", !e && t.contains("\"page\": \"MCP_GB_Test\""), &t);
    let rect_an = if e { None } else { json_of(&t)["ops"][1]["an"].as_i64() };
    let button_an = if e { None } else { json_of(&t)["ops"][4]["an"].as_i64() };

    let (e, t) = mcp.call("po_gb_page_read", json!({"page": page}));
    show("gb_page_read", &t);
    let j = if e { json!({}) } else { json_of(&t) };
    let objs = j["objects"].as_array().cloned().unwrap_or_default();
    let genie = objs.iter().find(|o| o["library_object"]["element"] == "Toggle_Button_1").cloned();
    c.check("la página tiene el genie con sus propiedades", genie.as_ref().is_some_and(|g| g["properties"]["Tag"] == r"Feeder1\XCBR1\Trip"), &t);
    c.check("la página tiene el texto", objs.iter().any(|o| o["text"].as_str().is_some_and(|s| s.contains("creada por el MCP"))), &t);

    let genie_an = genie.and_then(|g| g["an"].as_i64());
    if let Some(an) = genie_an {
        let (e, t) = mcp.call(
            "po_gb_page_edit",
            json!({"page": page, "ops": [
                {"op": "move", "an": an, "x": 120, "y": 260},
                {"op": "set_genie_properties", "an": an, "properties": {"Button_Text": "Trip"}},
                {"op": "text", "text": "Editada por gb_page_edit", "x": 40, "y": 440}
            ]}),
        );
        show("gb_page_edit", &t);
        c.check("gb_page_edit guarda (con respaldo)", !e && t.contains("\"saved\": true") && t.contains("_mcp_backup"), &t);
        let (_, t) = mcp.call("po_gb_page_read", json!({"page": page}));
        c.check("la edición quedó guardada", t.contains("Editada por gb_page_edit") && t.contains("\"Button_Text\": \"Trip\""), &t);
    }

    // set_touch (#22): PropertiesInputTouchGet lee el comando de cada acción (1 = down, 0 = up) y `after` es lo
    // releído; en pgbutton, DOWN_CMD = al presionar y CMD = al soltar. Verificado el 2026-11-26.
    c.check("el button tiene AN", button_an.is_some(), "sin AN en la respuesta de gb_page_create");
    if let Some(an) = button_an {
        let (e, t) = mcp.call(
            "po_gb_page_edit",
            json!({"page": page, "ops": [
                {"op": "set_touch", "an": an, "action": "down", "command": "TraceMsg(\"down2\")"},
                {"op": "set_touch", "an": an, "action": "up", "command": "TraceMsg(\"up2\")", "log_message": "log up"}
            ]}),
        );
        show("gb_page_edit set_touch", &t);
        let j = if e { json!({}) } else { json_of(&t) };
        c.check("set_touch lee el comando down anterior", j["ops"][0]["before"]["command"] == "TraceMsg(\"down1\")", &t);
        c.check("set_touch lee el comando up anterior", j["ops"][1]["before"]["command"] == "TraceMsg(\"up1\")", &t);
        c.check("set_touch relee lo escrito", j["ops"][0]["after"]["command"] == "TraceMsg(\"down2\")"
            && j["ops"][1]["after"]["log_message"] == "log up", &t);
        let (_, t) = mcp.call("po_project_query", json!({"table": "pgbutton", "where": {"PAGE": page, "AN": an.to_string()}}));
        let row = json_of(&t)["rows"][0].clone();
        c.check("pgbutton DOWN_CMD/CMD/MSG_LOG", row["DOWN_CMD"] == "TraceMsg(\"down2\")" && row["CMD"] == "TraceMsg(\"up2\")"
            && row["MSG_LOG"] == "log up", &t);
    }

    // Grupos (#6): la op `group` agrupa por AN; po_gb_page_read entra en el grupo (PageSelectFirstObjectInGroup) y
    // un genie agrupado se edita por su AN. Verificado el 2026-11-27.
    if let (Some(r), Some(g)) = (rect_an, genie_an) {
        let (e, t) = mcp.call("po_gb_page_edit", json!({"page": page, "ops": [{"op": "group", "ans": [r, g]}]}));
        c.check("gb_page_edit group", !e && t.contains("\"grouped\": true"), &t);
        let (e, t) = mcp.call("po_gb_page_edit", json!({"page": page, "ops": [
            {"op": "set_genie_properties", "an": g, "properties": {"Button_Text": "Agrupado"}}]}));
        c.check("set_genie_properties sobre un genie agrupado", !e && t.contains("\"saved\": true"), &t);
        let (_, t) = mcp.call("po_gb_page_read", json!({"page": page}));
        let objs = json_of(&t)["objects"].as_array().cloned().unwrap_or_default();
        let group = objs.iter().find(|o| o["class"] == "Group" && o["children"] == 2).cloned();
        let inner = objs.iter().find(|o| o["an"] == g).cloned().unwrap_or_default();
        c.check("page_read ve el grupo y su genie", group.is_some_and(|gr| inner["group"] == gr["path"])
            && inner["properties"]["Button_Text"] == "Agrupado", &t);
    }

    let r = mcp.rpc("tools/call", json!({"name": "po_gb_page_thumbnail", "arguments": {"page": page}}));
    let img = r["result"]["content"].as_array().and_then(|c| c.iter().find(|x| x["type"] == "image").cloned());
    c.check("miniatura PNG", img.as_ref().is_some_and(|i| i["mimeType"] == "image/png"), &r.to_string());
    if let (Ok(dir), Some(img)) = (std::env::var("POWEROP_LIVE_OUT"), img) {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD.decode(img["data"].as_str().unwrap()).unwrap();
        std::fs::write(std::path::Path::new(&dir).join("mcp_gb_test.png"), bytes).unwrap();
    }
    let audit = std::fs::read_to_string(work.join("audit.log")).unwrap_or_default();
    c.check("auditoría de gb_page_create/edit", audit.contains("gb_page_create") && audit.contains("gb_page_edit"), &audit);
    drop(mcp);
    let _ = std::fs::remove_dir_all(&work);
    c.finish();
}

/// Genie propio de MCP_Demo colocado en una página de MCP_Demo con otro proyecto seleccionado en Graphics Builder
/// (sin páginas abiertas). Crea y borra el genie `mcp_tmp.mcp_tmp_genie` (queda `mcp_tmp.ctm` vacío) y la página
/// `MCP_GB_GenieTmp`. Verificado el 2026-10-16 con Transferencia_Ejemplo seleccionado.
#[test]
#[ignore = "necesita Graphics Builder abierto con otro proyecto seleccionado"]
fn gb_genie_otro_proyecto() {
    let work = temp_dir("live-gbg");
    let mut mcp = Mcp::start_live(
        &work,
        &[("POWEROP_PROJECT", "MCP_Demo".into()), ("POWEROP_TIMEOUT_SECS", "150".into()), ("POWEROP_ALLOW_WRITE", "1".into())],
    );
    let mut c = Checks::default();
    let (page, lib, genie) = ("MCP_GB_GenieTmp", "mcp_tmp", "mcp_tmp_genie");
    let selected = |mcp: &mut Mcp| json_of(&mcp.call("po_gb_status", json!({})).1)["selected_project"].as_str().unwrap_or("").to_string();
    let before = selected(&mut mcp);
    println!("proyecto seleccionado: {before}");
    c.check("otro proyecto seleccionado", !before.is_empty() && !before.eq_ignore_ascii_case("MCP_Demo"), &before);

    let (e, t) = mcp.call(
        "po_gb_genie_create",
        json!({"library": lib, "genie": genie, "hotspot": {"x": 0, "y": 0}, "replace": true,
               "ops": [{"op": "rectangle", "x": 0, "y": 0, "x2": 60, "y2": 30}, {"op": "text", "text": "%Label%", "x": 5, "y": 5}]}),
    );
    show("gb_genie_create", &t);
    c.check("genie_create con otro proyecto seleccionado", !e && t.contains("\"restored\": true"), &t);
    c.check("selección restaurada tras genie_create", selected(&mut mcp).eq_ignore_ascii_case(&before), "");

    let (e, t) = mcp.call(
        "po_gb_page_create",
        json!({"page": page, "style": "standard", "template": "normal", "resolution": 3, "titlebar": false, "replace": true,
               "ops": [{"op": "genie", "library": lib, "element": genie, "x": 100, "y": 120, "properties": {"Label": "Otro"}}]}),
    );
    show("gb_page_create", &t);
    c.check("op genie con otro proyecto seleccionado", !e && t.contains("\"restored\": true"), &t);
    c.check("selección restaurada tras page_create", selected(&mut mcp).eq_ignore_ascii_case(&before), "");

    let (e, t) = mcp.call("po_gb_page_read", json!({"page": page}));
    show("gb_page_read", &t);
    c.check("la página tiene el genie", !e && t.contains(genie) && t.contains("\"Label\": \"Otro\""), &t);

    let (e, t) = mcp.call("po_gb_page_delete", json!({"page": page}));
    c.check("page_delete", !e, &t);
    let (e, t) = mcp.call("po_gb_genie_delete", json!({"library": lib, "genie": genie}));
    show("gb_genie_delete", &t);
    c.check("genie_delete con otro proyecto seleccionado", !e && t.contains("\"deleted\": true"), &t);
    c.check("selección restaurada al final", selected(&mut mcp).eq_ignore_ascii_case(&before), "");
    drop(mcp);
    let _ = std::fs::remove_dir_all(&work);
    c.finish();
}

/// po_oneline_clone contra Graphics Builder: copia la página de ejemplo PSO_Equipment/SingleLinesOverview_T300 a
/// MCP_Demo como `MCP_OL_Clone` con números nuevos y equipos reasignados, verifica el .ctg guardado (que
/// LibraryObjectPutProperty vuelva a expandir las expresiones y que PageSaveAs guarde en otro proyecto) y la borra.
#[test]
#[ignore = "necesita Graphics Builder; escribe la página MCP_OL_Clone en MCP_Demo y la borra"]
fn oneline_clone() {
    let work = temp_dir("live-olc");
    let mut mcp = Mcp::start_live(
        &work,
        &[("POWEROP_PROJECT", "MCP_Demo".into()), ("POWEROP_TIMEOUT_SECS", "150".into()), ("POWEROP_ALLOW_WRITE", "1".into())],
    );
    let mut c = Checks::default();
    let page = "MCP_OL_Clone";
    let _ = mcp.call("po_gb_page_delete", json!({"page": page}));
    let args = json!({"source_project": "PSO_Equipment", "source_page": "SingleLinesOverview_T300", "page": page,
                      "renumber": "offset", "offset": 500, "replace": [{"from": "MA_T300_", "to": "MCP_T301_"}]});
    let (e, t) = mcp.call("po_oneline_clone", args.clone());
    show("oneline_clone (plan)", &t);
    let plan = if e { json!({}) } else { json_of(&t) };
    c.check("plan con cambios", plan["total_changes"].as_u64().unwrap_or(0) > 0 && plan["applied"] == false, &t);

    let mut args = args;
    args["apply"] = json!(true);
    let (e, t) = mcp.call("po_oneline_clone", args);
    show("oneline_clone (apply)", &t);
    let j = if e { json!({}) } else { json_of(&t) };
    let v = &j["verification"];
    println!("\n===== graphics_builder\n{:#}\n===== verification\n{:#}", j["graphics_builder"], v);
    c.check("guardada y cada cambio del plan quedó en el .ctg", !e && v["not_applied"] == json!([]), &t);
    c.check("sin textos de origen", v["leftovers"] == json!([]), &t);
    c.check(
        "las expresiones se volvieron a expandir (sin zOL viejos)",
        v["findings"].as_array().is_some_and(|f| !f.iter().any(|x| x["rule"] == "StaleZolReference")),
        &t,
    );
    c.check("verificación ok (sin errores que el plan no esperaba)", v["ok"] == true && v["applied_as_planned"] == true && v["new_findings"] == json!([]), &t);
    c.check("guardada en la carpeta de MCP_Demo", v["file"].as_str().is_some_and(|f| f.contains("\\MCP_Demo\\")), &t);
    let (e, t) = mcp.call("po_project_query", json!({"table": "pages", "where": {"NAME": page}}));
    c.check("fila en pages.dbf de MCP_Demo", !e && json_of(&t)["matched"] == 1, &t);
    let (e, t) = mcp.call("po_oneline_read", json!({"pages": [page]}));
    c.check("po_oneline_read ve los números nuevos", !e && t.contains("Bkr502"), &t);
    // POWEROP_LIVE_KEEP=1 deja la página para revisarla en Graphics Builder (borrarla después con po_gb_page_delete).
    if std::env::var("POWEROP_LIVE_KEEP").is_ok_and(|v| v == "1") {
        println!("\n===== {page} queda en MCP_Demo (POWEROP_LIVE_KEEP=1)");
    } else {
        let (e, t) = mcp.call("po_gb_page_delete", json!({"page": page}));
        show("gb_page_delete", &format!("{}{t}", if e { "[ERROR] " } else { "" }));
        c.check("page_delete", !e, &t);
    }
    let audit = std::fs::read_to_string(work.join("audit.log")).unwrap_or_default();
    c.check("auditoría de oneline_clone", audit.contains("oneline_clone"), &audit);
    drop(mcp);
    let _ = std::fs::remove_dir_all(&work);
    c.finish();
}
/// po_oneline_clone dentro del mismo proyecto: `MCP_OL_Test` (creado con po_project_create, incluye PLS_Include y
/// PSO_Equipment) tiene `MCP_OL_A` (copia de SingleLinesOverview_T300). Clona A como `MCP_OL_Tmp` con next_free,
/// verifica que no repita números de A (po_oneline_validate sin errores) y la borra. Verificado el 2026-11-27.
/// Graphics Builder tiene que haberse abierto después de crear el proyecto (carga la lista de proyectos al abrir).
#[test]
#[ignore = "necesita Graphics Builder y el proyecto MCP_OL_Test con la página MCP_OL_A; escribe MCP_OL_Tmp y la borra"]
fn oneline_clone_mismo_proyecto() {
    let work = temp_dir("live-olc2");
    let mut mcp = Mcp::start_live(
        &work,
        &[("POWEROP_PROJECT", "MCP_OL_Test".into()), ("POWEROP_TIMEOUT_SECS", "150".into()), ("POWEROP_ALLOW_WRITE", "1".into())],
    );
    let mut c = Checks::default();
    let page = "MCP_OL_Tmp";
    let _ = mcp.call("po_gb_page_delete", json!({"page": page}));
    let args = json!({"source_page": "MCP_OL_A", "page": page, "renumber": "next_free",
                      "replace": [{"from": "MA_T300_", "to": "MCP_T303_"}]});
    let (e, t) = mcp.call("po_oneline_clone", args.clone());
    show("oneline_clone (plan)", &t);
    let plan = if e { json!({}) } else { json_of(&t) };
    c.check("plan con cambios y sin librerías faltantes", plan["total_changes"].as_u64().unwrap_or(0) > 0
        && plan["missing_libraries"] == json!([]), &t);
    let mut args = args;
    args["apply"] = json!(true);
    let (e, t) = mcp.call("po_oneline_clone", args);
    show("oneline_clone (apply)", &t);
    let j = if e { json!({}) } else { json_of(&t) };
    let v = &j["verification"];
    c.check("verificación ok", !e && v["ok"] == true && v["applied_as_planned"] == true && v["new_findings"] == json!([])
        && v["leftovers"] == json!([]), &t);
    let (e, t) = mcp.call("po_oneline_validate", json!({}));
    c.check("sin errores ni advertencias en el proyecto (A y la copia no comparten números)",
        !e && json_of(&t)["summary"]["errors"] == 0 && json_of(&t)["summary"]["warnings"] == 0, &t);
    let (e, t) = mcp.call("po_gb_page_delete", json!({"page": page}));
    c.check("page_delete", !e, &t);
    drop(mcp);
    let _ = std::fs::remove_dir_all(&work);
    c.finish();
}
