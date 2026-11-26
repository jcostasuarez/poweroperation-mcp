//! End-to-end por stdio con un proyecto falso y sin CtApi.dll (solo lectura), más un PLC Modbus falso.

mod common;

use std::io::{Read, Write};

use common::{Checks, Mcp, fake_projects, json_of, temp_dir};
use serde_json::json;

/// Cadena con largo delante (u16) como en los .ctg.
fn lp(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// Genie colocado con el formato de PO 2024 (ver src/ctg.rs).
fn lp_genie(library: &str, genie: &str, props: &[(&str, &str)]) -> Vec<u8> {
    let mut out = vec![0x78, 0x00, 0x18, 0x01];
    lp(&mut out, &format!("{library}.{genie}.<ctm"));
    out.extend_from_slice(&[0x01, 0x00, 0x02, 0, 0, 0, 0x0c, 0, 0, 0, 0x02, 0, 0, 0, 0x02, 0, 0, 0]);
    let mut block = (props.len() as u16).to_le_bytes().to_vec();
    for (k, v) in props {
        lp(&mut block, k);
        lp(&mut block, v);
    }
    out.extend_from_slice(&((block.len() + 11) as u16).to_le_bytes());
    out.extend_from_slice(&block);
    out.extend_from_slice(&[0x02, 0, 0, 0, 0x01, 0, 0, 0, 0x80, 0, 0]);
    out
}

/// Animación con una expresión ya expandida.
fn lp_anim(expr: &str) -> Vec<u8> {
    let mut out = vec![0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x49, 0x00, 0x01, 0x00];
    lp(&mut out, "COL_A");
    lp(&mut out, expr);
    out
}

fn oneline_page(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"\x00\x00attributes\x00".to_vec();
    lp(&mut out, "pls_ansi_cb_1.CB_Draw_1.<ctm#dd67fb98");
    for p in parts {
        out.extend_from_slice(&[0u8; 12]);
        out.extend_from_slice(p);
    }
    out
}

/// PLC Modbus TCP falso: FC3/FC4 devuelven el número de registro (4001 = 0 para probar nonzero_only), FC1/FC2
/// bits impares en 1; FC3 desde 9000 responde excepción 2. Atiende conexiones una tras otra.
fn fake_plc() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut c in listener.incoming().map_while(Result::ok) {
            let mut req = [0u8; 12];
            while c.read_exact(&mut req).is_ok() {
                let fc = req[7];
                let start = u16::from_be_bytes([req[8], req[9]]);
                let count = u16::from_be_bytes([req[10], req[11]]);
                let mut resp = vec![req[0], req[1], 0, 0];
                if fc > 4 || (fc == 3 && start >= 9000) {
                    resp.extend([0, 3, req[6], fc | 0x80, if fc > 4 { 1 } else { 2 }]);
                } else {
                    let data: Vec<u8> = if fc >= 3 {
                        (0..count).flat_map(|i| if start + i == 4001 { 0u16 } else { start + i }.to_be_bytes()).collect()
                    } else {
                        let mut b = vec![0u8; (count as usize).div_ceil(8)];
                        for i in 0..count as usize {
                            if (start as usize + i) % 2 == 1 {
                                b[i / 8] |= 1 << (i % 8);
                            }
                        }
                        b
                    };
                    resp.extend(((data.len() + 3) as u16).to_be_bytes());
                    resp.extend([req[6], fc, data.len() as u8]);
                    resp.extend(data);
                }
                if c.write_all(&resp).is_err() {
                    break;
                }
            }
        }
    });
    port
}

#[test]
fn offline_tools() {
    let work = temp_dir("offline");
    let root = work.join("fakeuser");
    fake_projects(&root);
    // Respaldo de las herramientas de escritura con forma de proyecto: no debe listarse.
    let backup = root.join("PO_Demo").join("_mcp_backup").join("20261014-164138");
    std::fs::create_dir_all(&backup).unwrap();
    std::fs::copy(root.join("PO_Demo").join("variable.dbf"), backup.join("variable.dbf")).unwrap();
    let mut mcp = Mcp::start(
        &work,
        &[
            ("POWEROP_PROJECTS_DIR", root.display().to_string()),
            ("POWEROP_PROJECT", "PO_Demo".into()),
            ("POWEROP_CTAPI", root.join("no-existe").display().to_string()),
            ("POWEROP_MODBUS", format!("PLCT=127.0.0.1:{}/1", fake_plc())),
        ],
    );
    let mut c = Checks::default();

    // Sin CtAPI ni escrituras: solo DISCO, PROCESOS, MODBUS y (si está instalado en este equipo) GB.
    let tools = mcp.tool_names();
    let all = tools.join(", ");
    c.check("todas con prefijo po_", tools.iter().all(|t| t.starts_with("po_")), &all);
    c.check(
        "se listan las de DISCO, PROCESOS y MODBUS",
        [
            "po_status", "po_reference", "po_project_list", "po_project_tables", "po_project_query", "po_project_search",
            "po_project_graphics", "po_cicode_files", "po_cicode_read", "po_cicode_check", "po_runtime_procs",
            "po_runtime_screenshot", "po_studio_project", "po_plc_read", "po_oneline_read", "po_oneline_validate",
        ]
        .iter()
        .all(|t| tools.iter().any(|x| x == t)),
        &all,
    );
    c.check(
        "no se listan las de CTAPI, ODBC, STUDIO ni escrituras",
        ["po_tag_read", "po_cicode_run", "po_alarm_sql", "po_project_compile", "po_tag_write", "po_cicode_write", "po_project_rows_add", "po_runtime_control", "po_gb_page_create"]
            .iter()
            .all(|t| !tools.iter().any(|x| x == t)),
        &all,
    );
    let gb = tools.iter().filter(|t| t.starts_with("po_gb_")).count();
    c.check("GB: las 5 de lectura o ninguna", tools.len() == 16 + gb && (gb == 0 || gb == 5), &all);

    // Catálogo de librerías gráficas: sale de los archivos, sin Graphics Builder.
    for f in ["PO_Demo/Feeders.ctg", "PLS_Include/pls_controls.ctm", "PLS_Include/pls_lights.ctl", "PLS_Include/pls_style_1.ctt"] {
        std::fs::write(root.join(f), b"").unwrap();
    }
    let (e, t) = mcp.call("po_project_graphics", json!({}));
    let j = if e { json!({}) } else { json_of(&t) };
    let lib = j["projects"].as_array().and_then(|ps| ps.iter().find(|p| p["project"] == "PLS_Include").cloned()).unwrap_or_default();
    c.check(
        "po_project_graphics cataloga páginas y librerías sin Graphics Builder",
        t.contains("\"Feeders\"") && lib["genie_libraries"] == json!(["pls_controls"]) && lib["symbol_libraries"] == json!(["pls_lights"]) && lib["template_styles"] == json!(["pls_style_1"]),
        &t,
    );
    // Advanced One-Line offline: dos interruptores con el mismo BkrNum y equipos distintos (uno de otra estación),
    // barras 1 y 2, y una animación que quedó apuntando a zOL\Bkr7 (resto de una copia).
    common::write_dbf(&root.join("PO_Demo/equip.dbf"), &[("NAME", 40)], &[&["EstA.Q1"], &["EstA.Q2"]]);
    let page = oneline_page(&[
        lp_genie("pls_gen_base_1", "Busbar_H", &[("BusbarNum", "1")]),
        lp_genie("pls_gen_base_1", "Busbar_H", &[("BusbarNum", "2")]),
        lp_genie("pls_ansi_cb_1", "CB_Draw_1", &[("AN", "40"), ("BkrNum", "12"), ("Equip", "EstA.Q1"), ("SrcBusbarNum", "1"), ("DestBusbarNum", "2")]),
        lp_genie("pls_ansi_cb_1", "CB_Draw_1", &[("AN", "41"), ("BkrNum", "12"), ("Equip", "EstB.Q1"), ("SrcBusbarNum", "1"), ("DestBusbarNum", "2")]),
        lp_anim("PLS_Bkr_Color_Bkr(7, zOL\\Bus1, zOL\\Bus2, zOL\\Bkr7)"),
    ]);
    std::fs::write(root.join("PO_Demo/EstA_Unifilar.ctg"), page).unwrap();
    let (e, t) = mcp.call("po_oneline_read", json!({"pages": ["EstA_*"]}));
    let j = if e { json!({}) } else { json_of(&t) };
    let items = j["pages"][0]["items"].as_array().cloned().unwrap_or_default();
    c.check(
        "po_oneline_read: barras e interruptores con número, zOL, equipo y barras",
        !e && j["counts"] == json!({"breaker": 2, "busbar": 2})
            && items.iter().any(|i| i["zol"] == "zOL\\Bkr12" && i["equip"] == "EstB.Q1" && i["an"] == "41" && i["buses"]["DestBusbarNum"] == "2")
            && j["pages"][0]["zol_refs"].as_array().is_some_and(|r| r.iter().any(|x| x == "zOL\\Bkr7")),
        &t,
    );
    let (e, t) = mcp.call("po_oneline_validate", json!({"foreign": ["EstB"]}));
    let j = if e { json!({}) } else { json_of(&t) };
    let rules = &j["summary"]["by_rule"];
    c.check(
        "po_oneline_validate: repetido con equipos distintos, equipo inexistente, otra estación y resto de copia",
        !e && rules["DuplicateBreaker"] == 1 && rules["InvalidEquipment"] == 1 && rules["ForeignReference"] == 1 && rules["StaleZolReference"] == 1
            && j["findings"][0]["severity"] == "error",
        &t,
    );

    let (e, t) = mcp.call("po_gb_page_create", json!({"page": "Nueva"}));
    c.check("po_gb_page_create oculta en solo lectura, con el motivo", e && t.contains("no está disponible") && t.contains("POWEROP_ALLOW_WRITE"), &t);

    let (e, t) = mcp.call("tag_read", json!({"tags": ["X"]}));
    c.check("nombre viejo: dice el nuevo", e && t.contains("ahora es po_tag_read"), &t);
    let (e, t) = mcp.call("runtime_restart", json!({}));
    c.check("nombre fusionado: dice el reemplazo", e && t.contains("po_runtime_control (action=restart)"), &t);

    // PLC Modbus (solo lectura).
    let (e, t) = mcp.call("po_plc_read", json!({"addresses": ["%MW4000", "%M5", "%IW7", "%MW9000"]}));
    let j = json_of(&t);
    c.check(
        "po_plc_read direcciones sueltas, con excepción por dirección",
        !e && j["values"][0]["value"] == 4000 && j["values"][1]["value"] == 1 && j["values"][2]["value"] == 7
            && j["values"][3]["error"].as_str().is_some_and(|m| m.contains("excepción Modbus 2")),
        &t,
    );
    let (e, t) = mcp.call("po_plc_read", json!({"area": "holding", "start": 4000, "count": 300, "nonzero_only": true}));
    let j = json_of(&t);
    c.check(
        "po_plc_read rango largo partido en pedidos, nonzero_only",
        !e && j["read"] == 300 && j["nonzero"] == 299 && j["values"][1]["addr"] == "%MW4002",
        &t,
    );
    let (e, t) = mcp.call("po_plc_read", json!({"addresses": ["%QW1"]}));
    c.check("po_plc_read rechaza notación desconocida", e && t.contains("%MWn"), &t);

    let (_, t) = mcp.call("po_project_list", json!({}));
    let j = json_of(&t);
    let names: Vec<&str> = j["projects"].as_array().unwrap().iter().filter_map(|p| p["name"].as_str()).collect();
    c.check("project_list encuentra 2 proyectos", names.len() == 2 && names.contains(&"PO_Demo") && names.contains(&"PLS_Include"), &t);
    c.check(
        "citect.ini dentro de proyecto ignorado",
        j["projects"].as_array().unwrap().iter().all(|p| p["running"] == false) && j["citect_ini"] == json!([]),
        &t,
    );

    let (_, t) = mcp.call("po_project_tables", json!({}));
    c.check("project_tables lista variable", t.contains("\"variable\"") && t.contains("TAG(C60)"), &t);

    let (_, t) = mcp.call("po_project_query", json!({"table": "variable", "where": {"TAG": "Feeder1*"}, "fields": ["TAG", "COMMENT"]}));
    let j = json_of(&t);
    c.check("project_query filtra y decodifica cp1252", j["matched"] == 2 && j["rows"][1]["COMMENT"] == "Posición interruptor", &t);

    let (_, t) = mcp.call("po_project_query", json!({"table": "variable", "text": "heartbeat", "include_included": true}));
    c.check("project_query en incluidos", t.contains("\"_project\": \"PLS_Include\""), &t);

    let (e, t) = mcp.call("po_project_query", json!({"table": "variable", "where": {"NOPE": "x"}}));
    c.check("campo inexistente da error claro", e && t.contains("Campos:"), &t);

    let (_, t) = mcp.call("po_project_search", json!({"text": "XCBR1"}));
    let j = json_of(&t);
    let tables: Vec<&str> = j["hits"].as_array().unwrap().iter().filter_map(|h| h["table"].as_str()).collect();
    c.check("project_search encuentra en variable y digalm", tables.contains(&"variable") && tables.contains(&"digalm"), &t);

    let (_, t) = mcp.call("po_cicode_files", json!({"include_included": true}));
    c.check("cicode_files lista funciones", t.contains("FeederPower (l.2)") && t.contains("PlsInit"), &t);

    let (_, t) = mcp.call("po_cicode_read", json!({"file": "Feeders", "function": "FeederPower", "numbered": true}));
    c.check("cicode_read extrae función con comentario", t.contains("1| // Lee potencia") && t.contains("4| END"), &t);

    let (_, t) = mcp.call("po_cicode_read", json!({"file": "PlsLib.ci"}));
    c.check("cicode_read busca en incluidos", t.contains("PLS_Include"), &t);

    let (_, t) = mcp.call("po_cicode_check", json!({"code": "FUNCTION A()\n IF x THEN\nEND\n"}));
    c.check("cicode_check detecta IF sin END", t.contains("\"ok\": false"), &t);

    let (e, t) = mcp.call("po_cicode_write", json!({"file": "Nuevo.ci", "content": "FUNCTION X()\nEND\n"}));
    c.check("po_cicode_write oculta en solo lectura", e && t.contains("POWEROP_ALLOW_WRITE"), &t);

    let (e, t) = mcp.call("po_tag_write", json!({"tag": "Feeder1\\XCBR1\\Pos", "value": "0"}));
    c.check("po_tag_write oculta: sin CtAPI y en solo lectura", e && t.contains("solo lectura") && t.contains("puente CTAPI"), &t);

    let (e, t) = mcp.call("po_cicode_run", json!({"command": "TagRead(\"X\")"}));
    c.check("po_cicode_run oculta sin CtApi.dll, con el motivo", e && t.contains("CtApi.dll") && t.contains("po_status"), &t);

    let (_, t) = mcp.call("po_status", json!({}));
    c.check(
        "po_status informa puentes y sin DLL",
        t.contains("NO encontrado") && t.contains("CTAPI: NO DISPONIBLE") && t.contains("DISCO: DISPONIBLE") && t.contains("MODBUS: DISPONIBLE")
            && t.contains("ESCRITURA: NO DISPONIBLE"),
        &t,
    );
    c.check("po_status: en un puente disponible marca las que faltan por otro", t.contains("po_cicode_write (falta ESCRITURA)"), &t);

    let (_, t) = mcp.call("po_reference", json!({}));
    c.check("reference", t.contains("TRNQUERY") && t.contains("CiAlarmObject"), &t.chars().take(200).collect::<String>());

    drop(mcp);
    let _ = std::fs::remove_dir_all(&work);
    c.finish();
}
