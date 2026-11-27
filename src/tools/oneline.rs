//! Advanced One-Line: `po_oneline_read` y `po_oneline_validate` (offline, solo lectura) leen los `.ctg` del proyecto
//! con `ctg.rs` (sin Graphics Builder), así que ven también los genies ocultos y los de dentro de grupos.
//! `po_oneline_clone` arma el plan offline (`oneline_clone.rs`), lo aplica con Graphics Builder y verifica el `.ctg`
//! guardado.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::dbf::Dbf;
use crate::gb::Arg;
use crate::oneline::{self, Context, Item, Kind};
use crate::oneline_clone::{self, Numbering, Numbers, Rules, Space};
use crate::project::{self, Project};
use crate::server::{PowerOpServer, ToolResult, blocking, err};
use crate::text::{filter_match, truncate};
use crate::{ctg, text};

use super::graphics::{close_page, is_open, library_project, page_objects, warnings_json, with_project};

/// Proyectos cuyas páginas no se recorren (como la One-Line Configuration Utility: strDoNotIterateProjects y
/// strDoNotDisplayProjects de OneLineConfig.exe.config).
const SKIP: &[&str] = &[
    "System", "Include", "IncV2", "CSV_Include", "Library_Controls", "Library_Equipment", "SXW_Style_Include", "Tab_Style_Include",
    "PLS_Include", "PSO_Equipment", "IndustrialGraphics_Include",
];

#[derive(Deserialize, JsonSchema)]
pub struct OneLineReadParams {
    /// Proyecto. Vacío = POWEROP_PROJECT / el que corre en runtime.
    #[serde(default)]
    pub project: Option<String>,
    /// Páginas a leer (nombres; con `*`/`?` como comodines, sin ellos "contiene"). Vacío = todas.
    #[serde(default)]
    pub pages: Option<Vec<String>>,
    /// Incluir las páginas de los proyectos incluidos (salvo los de sistema y librerías de PO). Por defecto true.
    #[serde(default)]
    pub include_included: Option<bool>,
    /// Tipos a devolver: breaker, ats, busbar, meter, source, transformer. Vacío = todos.
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
    /// Devolver todas las propiedades de cada genie (por defecto true; false = solo número, equipo, Active y barras).
    #[serde(default)]
    pub all_properties: Option<bool>,
    /// Máximo de objetos en la respuesta (por defecto 1000).
    #[serde(default)]
    pub max_items: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct OneLineValidateParams {
    #[serde(default)]
    pub project: Option<String>,
    /// Páginas a revisar (como en po_oneline_read). Vacío = todas.
    #[serde(default)]
    pub pages: Option<Vec<String>>,
    /// Incluir las páginas de los proyectos incluidos. Por defecto true.
    #[serde(default)]
    pub include_included: Option<bool>,
    /// Textos que no deberían aparecer en ninguna propiedad (p. ej. el prefijo de otra estación: ["EstB_"]).
    #[serde(default)]
    pub foreign: Option<Vec<String>>,
    /// Prefijo que deberían tener todos los equipos (`Equip`) de las páginas revisadas.
    #[serde(default)]
    pub equip_prefix: Option<String>,
    /// Validar contra variable.dbf los tags con `\` usados en las propiedades (por defecto true).
    #[serde(default)]
    pub check_tags: Option<bool>,
    /// Máximo de hallazgos en la respuesta (por defecto 500).
    #[serde(default)]
    pub max_findings: Option<usize>,
}

/// Reemplazo de texto para po_oneline_clone.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloneReplace {
    /// Texto a buscar (sin distinguir mayúsculas), p. ej. el prefijo de la estación de origen "EstA_".
    pub from: String,
    /// Texto que lo reemplaza, p. ej. "EstB_".
    pub to: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct OneLineCloneParams {
    /// Proyecto donde se crea la página nueva. Vacío = POWEROP_PROJECT / el que corre en runtime.
    #[serde(default)]
    pub project: Option<String>,
    /// Página de origen (nombre exacto, sin .ctg).
    pub source_page: String,
    /// Proyecto de la página de origen. Vacío = se busca en el proyecto y sus incluidos.
    #[serde(default)]
    pub source_project: Option<String>,
    /// Nombre de la página nueva (no tiene que existir en el proyecto ni en sus incluidos).
    pub page: String,
    /// Cómo renumerar los números zOL: "next_free" (por defecto: desde el mayor usado + 1, o desde `start`,
    /// saltando los usados), "offset" (viejo + `offset`; error si alguno ya está usado) o "keep" (no renumerar).
    #[serde(default)]
    pub renumber: Option<String>,
    /// Con next_free: primer número a probar.
    #[serde(default)]
    pub start: Option<u32>,
    /// Con offset: lo que se suma a cada número.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Espacios a renumerar: bkr (BkrNum, BkrNum1/2), bus (BusbarNum, SrcBusbarNum, DestBusbarNum...), mtr (MtrNum),
    /// src (SrcNum, SimSrcNum). Vacío = todos. Sin "bus", la página nueva queda conectada a las mismas barras.
    #[serde(default)]
    pub spaces: Option<Vec<String>>,
    /// Reemplazos de texto en las demás propiedades de los genies (Equip, Active, DetailsFn...), en orden, sin
    /// distinguir mayúsculas. P. ej. [{"from": "EstA_", "to": "EstB_"}].
    #[serde(default)]
    pub replace: Option<Vec<CloneReplace>>,
    /// false (por defecto): solo devuelve el plan, sin Graphics Builder ni escrituras. true: crea la página.
    #[serde(default)]
    pub apply: Option<bool>,
    /// Guardar aunque Graphics Builder vea menos cambios que el plan (genies que no encuentra o no puede editar):
    /// esos quedan con los números y textos de origen. Por defecto false: no se guarda nada.
    #[serde(default)]
    pub allow_partial: Option<bool>,
}

fn pretty(v: &Value, max: usize) -> ToolResult {
    let s = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    Ok(truncate(&s, max))
}

/// Lo que se leyó de las páginas.
struct Scan {
    pages: Vec<PageScan>,
    unreadable: Vec<Value>,
}

struct PageScan {
    project: String,
    page: String,
    items: Vec<Item>,
    zol_refs: Vec<(String, String)>,
    /// Genies cuyas propiedades no se pudieron leer.
    unparsed: Vec<String>,
}

fn scan(projects: &[Project], filter: &[String]) -> Scan {
    let mut out = Scan { pages: vec![], unreadable: vec![] };
    for p in projects {
        for file in project::files_with_ext(&p.path, "ctg") {
            let Some(page) = file.file_stem().map(|s| s.to_string_lossy().to_string()) else { continue };
            if !filter.is_empty() && !filter.iter().any(|f| filter_match(f, &page)) {
                continue;
            }
            let data = match std::fs::read(&file) {
                Ok(d) => d,
                Err(e) => {
                    out.unreadable.push(json!({"project": p.name, "page": page, "error": e.to_string()}));
                    continue;
                }
            };
            let g = ctg::genies(&data);
            let items = g
                .genies
                .into_iter()
                .filter_map(|genie| oneline::classify(&genie).map(|kind| Item { project: p.name.clone(), page: page.clone(), kind, genie }))
                .collect();
            out.pages.push(PageScan { project: p.name.clone(), page, items, zol_refs: ctg::zol_refs(&data), unparsed: g.unparsed });
        }
    }
    out
}

/// NAME de una tabla DBF (minúsculas) en el proyecto y sus incluidos.
fn names(projects: &[Project], table: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for p in projects {
        let Some(path) = project::table_path(&p.path, table) else { continue };
        let Ok(dbf) = Dbf::open(&path) else { continue };
        let Some(idx) = dbf.field_index("NAME") else { continue };
        dbf.for_each(|_, v| {
            if let Some(n) = v.get(idx).filter(|n| !n.is_empty()) {
                out.insert(n.to_lowercase());
            }
            true
        });
    }
    out
}

/// AdvOneLine.csv del proyecto (lo genera la One-Line Configuration Utility en la carpeta del proyecto).
fn read_csv(dir: &Path) -> Option<(String, Vec<oneline::CsvRow>)> {
    let path = project::find_file(dir, "AdvOneLine.csv")?;
    let bytes = std::fs::read(&path).ok()?;
    Some((path.display().to_string(), oneline::parse_csv(&text::decode(&bytes))))
}

fn page_projects(all: Vec<Project>, include_included: bool) -> Vec<Project> {
    let root = all[0].name.clone();
    all.into_iter()
        .enumerate()
        .filter(|(i, p)| *i == 0 || (include_included && !SKIP.iter().any(|s| s.eq_ignore_ascii_case(&p.name))))
        .map(|(_, p)| p)
        .filter(|p| p.name == root || !p.name.to_lowercase().starts_with("library_"))
        .collect()
}

fn unparsed_json(s: &Scan) -> Value {
    let list: Vec<Value> = s
        .pages
        .iter()
        .filter(|p| !p.unparsed.is_empty())
        .map(|p| json!({"project": p.project, "page": p.page, "genies": p.unparsed}))
        .collect();
    json!(list)
}

/// Plan de un clonado, armado offline.
struct ClonePlan {
    source: Vec<ctg::GenieInstance>,
    rules: Rules,
    planned: usize,
    json: Value,
}

/// Números zOL (letras + número) de un tag `zOL\Bkr12`.
fn zol_number(tag: &str) -> Option<(Space, u32)> {
    let rest = tag.get(4..)?;
    let digits = rest.find(|c: char| c.is_ascii_digit())?;
    let sp = match rest[..digits].to_ascii_lowercase().as_str() {
        "bkr" => Space::Bkr,
        "bus" => Space::Bus,
        "mtr" => Space::Mtr,
        "src" => Space::Src,
        _ => return None,
    };
    Some((sp, rest[digits..].parse().ok()?))
}

/// Hallazgos de po_oneline_validate que tocan a `page` de `project`, con la página dada en `extra` sumada al resto.
fn page_findings(scan: &Scan, project: &str, page: &str, extra: Vec<Item>, refs: &[(String, String)], ctx: &Context) -> Vec<Value> {
    let mut items: Vec<Item> = scan
        .pages
        .iter()
        .filter(|pg| !(pg.project.eq_ignore_ascii_case(project) && pg.page.eq_ignore_ascii_case(page)))
        .flat_map(|pg| pg.items.iter().cloned())
        .collect();
    let first = items.len();
    items.extend(extra);
    let busbars = oneline::known_buses(&items);
    let own: Vec<&Item> = items[first..].iter().collect();
    let mut f = oneline::validate(&items, ctx);
    f.extend(oneline::stale_refs(project, page, &own, &busbars, refs));
    f.iter()
        .filter(|x| x.at.iter().any(|a| a["page"].as_str().is_some_and(|p| p.eq_ignore_ascii_case(page)) && a["project"].as_str().is_some_and(|p| p.eq_ignore_ascii_case(project))))
        .map(|x| x.json())
        .collect()
}

fn page_items(project: &str, page: &str, genies: &[ctg::GenieInstance]) -> Vec<Item> {
    genies
        .iter()
        .filter_map(|g| oneline::classify(g).map(|kind| Item { project: project.into(), page: page.into(), kind, genie: g.clone() }))
        .collect()
}

/// Lee la página de origen y el proyecto destino y arma el plan (números nuevos, cambios por genie, hallazgos
/// esperados en la página nueva).
#[allow(clippy::too_many_arguments)]
fn clone_plan(
    src_project: &str, src_page: &str, src_file: &Path, dest: &[Project], page: &str, numbering: &Numbering, spaces: &[Space],
    replace: Vec<(String, String)>,
) -> Result<ClonePlan> {
    let data = std::fs::read(src_file)?;
    let g = ctg::genies(&data);
    let mut source = Numbers::new();
    for x in &g.genies {
        oneline_clone::add_numbers(&x.properties, &mut source);
    }
    let projects = page_projects(dest.to_vec(), true);
    let s = scan(&projects, &[]);
    let mut used = Numbers::new();
    for pg in &s.pages {
        for i in &pg.items {
            oneline_clone::add_numbers(&i.genie.properties, &mut used);
        }
    }
    let csv = read_csv(&dest[0].path);
    if let Some((_, rows)) = &csv {
        oneline_clone::add_csv(rows, &mut used);
    }
    let maps = oneline_clone::allocate(&source, &used, spaces, numbering)?;
    let rules = Rules { maps, replace };
    let mut planned = 0;
    let mut changes = Vec::new();
    let mut odd = Vec::new();
    for (i, x) in g.genies.iter().enumerate() {
        let ch = oneline_clone::changes(&x.properties, &rules);
        planned += ch.len();
        for (k, v) in oneline_clone::non_numeric(&x.properties) {
            odd.push(json!({"index": i, "genie": format!("{}.{}", x.library, x.genie), "an": x.prop("AN"), "property": k, "value": v}));
        }
        if !ch.is_empty() {
            let kind = oneline::classify(x).map(|k| k.name());
            changes.push(json!({"index": i, "genie": format!("{}.{}", x.library, x.genie), "kind": kind, "an": x.prop("AN"), "changes": ch.iter().map(|c| c.json()).collect::<Vec<_>>()}));
        }
    }
    // Tags zOL de la página que no salen de las propiedades de sus genies: Graphics Builder no los renumera.
    let refs = ctg::zol_refs(&data);
    let manual: Vec<Value> = refs
        .iter()
        .filter(|(t, _)| zol_number(t).is_some_and(|(sp, n)| rules.maps.contains_key(&sp) && !source.get(&sp).is_some_and(|s| s.contains(&n))))
        .map(|(t, e)| json!({"tag": t, "expression": e}))
        .collect();
    let simulated: Vec<ctg::GenieInstance> = g.genies.iter().map(|x| oneline_clone::apply(x, &rules)).collect();
    let ctx = Context {
        equipment: names(dest, "equip"),
        tags: names(dest, "variable"),
        csv: None,
        foreign: rules.replace.iter().map(|(f, _)| f.clone()).collect(),
        equip_prefix: None,
    };
    let dest_name = dest[0].name.clone();
    let expected = page_findings(&s, &dest_name, page, page_items(&dest_name, page, &simulated), &[], &ctx);
    let used_max: BTreeMap<&str, Option<u32>> = Space::ALL.iter().map(|sp| (sp.name(), used.get(sp).and_then(|s| s.iter().next_back().copied()))).collect();
    // Librerías de genies que el proyecto destino no ve (ni él ni sus incluidos): Graphics Builder muestra esos genies
    // como DummyMarker y no los puede editar (visto el 2026-11-26 con MCP_Demo, que no incluye PLS_Include).
    let libs: std::collections::BTreeSet<String> = g.genies.iter().map(|x| x.library.to_lowercase()).collect();
    let missing_libs: Vec<String> = libs.into_iter().filter(|l| super::graphics::library_project(dest, l, "ctm").is_none()).collect();
    let mut warnings: Vec<String> = Vec::new();
    if !missing_libs.is_empty() {
        warnings.push(format!(
            "{} no ve las librerías {} (ni en sus incluidos, p. ej. falta PLS_Include): la página nueva se guarda, pero en ese proyecto \
             Graphics Builder muestra esos genies como DummyMarker y no anda en runtime. Agregá el incluido antes de usarla.",
            dest[0].name,
            missing_libs.join(", ")
        ));
    }
    if !g.unparsed.is_empty() {
        warnings.push(format!(
            "{} genies de la página de origen no se pudieron leer offline ({}): el plan no los incluye y Graphics Builder puede cambiar \
             más propiedades que las del plan.",
            g.unparsed.len(),
            g.unparsed.join(", ")
        ));
    }
    if !odd.is_empty() {
        warnings.push("Hay propiedades de numeración que no son un número (non_numeric_numbers): no se renumeran.".into());
    }
    if !manual.is_empty() {
        warnings.push("Hay tags zOL escritos a mano en expresiones de la página (manual_zol_refs): no se renumeran; corregilos después con po_gb_page_edit.".into());
    }
    let json = json!({
        "source": {"project": src_project, "page": src_page, "genies": g.genies.len(), "oneline_objects": page_items(src_project, src_page, &g.genies).len()},
        "target": {"project": dest_name, "page": page},
        "renumber": {"mode": match numbering { Numbering::NextFree { .. } => "next_free", Numbering::Offset(_) => "offset", Numbering::Keep => "keep" },
                     "spaces": spaces.iter().map(|s| s.name()).collect::<Vec<_>>(), "used_max": used_max, "maps": oneline_clone::maps_json(&rules.maps)},
        "replace": rules.replace.iter().map(|(f, t)| json!({"from": f, "to": t})).collect::<Vec<_>>(),
        "total_changes": planned,
        "changes": changes,
        "non_numeric_numbers": odd,
        "manual_zol_refs": manual,
        "missing_libraries": missing_libs,
        "expected_findings": expected,
        "warnings": warnings,
    });
    Ok(ClonePlan { source: g.genies, rules, planned, json })
}

/// Hallazgos de `found` que no estaban en `expected` (misma severidad, regla y mensaje).
fn new_findings(found: &[Value], expected: &[Value]) -> Vec<Value> {
    let key = |f: &Value| (f["severity"].as_str().unwrap_or_default().to_string(), f["rule"].as_str().unwrap_or_default().to_string(), f["message"].as_str().unwrap_or_default().to_string());
    let known: HashSet<_> = expected.iter().map(key).collect();
    found.iter().filter(|f| !known.contains(&key(f))).cloned().collect()
}

/// Después de guardar: compara el .ctg nuevo con el plan y valida la página nueva contra el resto del proyecto.
/// `ok` = cada cambio quedó, no quedaron textos de origen y no hay errores que el plan no esperaba
/// (`expected_findings`, p. ej. equipos que todavía no existen en el proyecto destino).
fn clone_verify(plan_source: &[ctg::GenieInstance], rules: &Rules, dest: &[Project], page: &str, expected: &[Value]) -> Value {
    let Some(file) = project::find_file(&dest[0].path, &format!("{page}.ctg")) else {
        return json!({"ok": false, "error": format!("no se encontró {page}.ctg en {} después de guardar", dest[0].name)});
    };
    let data = match std::fs::read(&file) {
        Ok(d) => d,
        Err(e) => return json!({"ok": false, "error": format!("no se pudo leer {}: {e}", file.display())}),
    };
    let saved = ctg::genies(&data);
    let not_applied = oneline_clone::not_applied(plan_source, &saved.genies, rules);
    let leftovers: Vec<Value> = rules
        .replace
        .iter()
        .map(|(f, _)| (f, ctg::strings_containing(&data, f, 20)))
        .filter(|(_, l)| !l.is_empty())
        .map(|(f, l)| json!({"text": f, "strings": l}))
        .collect();
    let dest_name = dest[0].name.clone();
    let s = scan(&page_projects(dest.to_vec(), true), &[]);
    let ctx = Context {
        equipment: names(dest, "equip"),
        tags: names(dest, "variable"),
        csv: None,
        foreign: rules.replace.iter().map(|(f, _)| f.clone()).collect(),
        equip_prefix: None,
    };
    let findings = page_findings(&s, &dest_name, page, page_items(&dest_name, page, &saved.genies), &ctg::zol_refs(&data), &ctx);
    let unexpected = new_findings(&findings, expected);
    let errors = unexpected.iter().filter(|f| f["severity"] == "error").count();
    let applied = not_applied.as_ref().is_ok_and(|v| v.is_empty()) && leftovers.is_empty();
    json!({
        "ok": applied && errors == 0,
        "applied_as_planned": applied,
        "file": file.display().to_string(),
        "not_applied": match &not_applied { Ok(v) => json!(v), Err(e) => json!({"error": e}) },
        "leftovers": leftovers,
        "new_findings": unexpected,
        "findings": findings,
        "unparsed_genies": saved.unparsed,
    })
}

#[tool_router(router = oneline_router, vis = "pub(crate)")]
impl PowerOpServer {
    #[tool(name = "po_oneline_read", description = "Advanced One-Line, sin Graphics Builder (offline, lee los .ctg): por página, los objetos del unifilar (interruptores, ATS, barras, medidores, fuentes, transformadores) con su número (BkrNum/BusbarNum/MtrNum/SrcNum), su tag zOL (zOL\\Bkr12), equipo (Equip), Active, barras de origen/destino y todas las propiedades del genie (también de genies ocultos o dentro de grupos), los tags zOL que aparecen en las expresiones de la página y AdvOneLine.csv si existe. Solo lectura.")]
    async fn oneline_read(&self, Parameters(p): Parameters<OneLineReadParams>) -> ToolResult {
        let proj = self.project(p.project.as_deref()).await.map_err(err)?;
        let (all, _) = self.catalog(false).await.map_err(err)?.with_includes(&proj);
        let kinds: Vec<Kind> = match &p.kinds {
            Some(list) if !list.is_empty() => list
                .iter()
                .map(|k| Kind::parse(k).ok_or_else(|| format!("kind '{k}' no existe: usá breaker, ats, busbar, meter, source o transformer")))
                .collect::<Result<_, _>>()?,
            _ => vec![],
        };
        let filter = p.pages.clone().unwrap_or_default();
        let all_props = p.all_properties.unwrap_or(true);
        let max = p.max_items.unwrap_or(1000).clamp(1, 20_000);
        let projects = page_projects(all, p.include_included.unwrap_or(true));
        let v = blocking(Duration::from_secs(120), move || -> Result<Value> {
            let s = scan(&projects, &filter);
            let csv = read_csv(&projects[0].path);
            let mut total = 0usize;
            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
            let mut pages = Vec::new();
            for pg in &s.pages {
                let items: Vec<&Item> = pg.items.iter().filter(|i| kinds.is_empty() || kinds.contains(&i.kind)).collect();
                if items.is_empty() && pg.zol_refs.is_empty() {
                    continue;
                }
                let mut page_counts: BTreeMap<&str, usize> = BTreeMap::new();
                for i in &items {
                    *page_counts.entry(i.kind.name()).or_default() += 1;
                    *counts.entry(i.kind.name()).or_default() += 1;
                }
                let shown: Vec<Value> = items.iter().take(max.saturating_sub(total)).map(|i| i.json(all_props)).collect();
                total += shown.len();
                let mut refs: Vec<&str> = pg.zol_refs.iter().map(|(t, _)| t.as_str()).collect();
                refs.sort_unstable_by_key(|t| t.to_lowercase());
                refs.dedup_by_key(|t| t.to_lowercase());
                pages.push(json!({"project": pg.project, "page": pg.page, "counts": page_counts, "items": shown, "zol_refs": refs}));
            }
            let truncated = counts.values().sum::<usize>() > total;
            Ok(json!({
                "project": projects[0].name,
                "projects_scanned": projects.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
                "pages_scanned": s.pages.len(),
                "pages_with_oneline": pages.len(),
                "counts": counts,
                "truncated": truncated,
                "pages": pages,
                "csv": csv.map(|(path, rows)| json!({"path": path, "components": rows.iter().map(|r| json!({"component": r.component, "condition": r.condition, "bus1": r.bus1, "bus2": r.bus2})).collect::<Vec<_>>()})),
                "unparsed_genies": unparsed_json(&s),
                "unreadable": s.unreadable,
                "note": "Formato .ctg leído sin Graphics Builder (ver reference.md, One-Line). El tipo se deduce de las propiedades del genie; lo que no se pudo leer está en unparsed_genies.",
            }))
        })
        .await
        .map_err(err)?;
        pretty(&v, 200_000)
    }

    #[tool(name = "po_oneline_validate", description = "Valida el Advanced One-Line sin Graphics Builder (offline, lee los .ctg): números zOL repetidos (en la misma página o en varias con equipos distintos), números inválidos, barras de origen/destino iguales o inexistentes, equipos que no están en equip.dbf, Active vacío, propiedades que nombran a otra estación (`foreign`) o equipos sin el prefijo esperado (`equip_prefix`), tags inexistentes en las propiedades, tags zOL de las expresiones que ningún objeto de la página define (restos de una copia) y diferencias con AdvOneLine.csv. Incluye genies ocultos y dentro de grupos. Solo lectura.")]
    async fn oneline_validate(&self, Parameters(p): Parameters<OneLineValidateParams>) -> ToolResult {
        let proj = self.project(p.project.as_deref()).await.map_err(err)?;
        let (all, _) = self.catalog(false).await.map_err(err)?.with_includes(&proj);
        let filter = p.pages.clone().unwrap_or_default();
        let max = p.max_findings.unwrap_or(500).clamp(1, 20_000);
        let check_tags = p.check_tags.unwrap_or(true);
        let foreign = p.foreign.clone().unwrap_or_default();
        let equip_prefix = p.equip_prefix.clone();
        let projects = page_projects(all.clone(), p.include_included.unwrap_or(true));
        let v = blocking(Duration::from_secs(120), move || -> Result<Value> {
            let s = scan(&projects, &filter);
            let csv = read_csv(&projects[0].path);
            let ctx = Context {
                equipment: names(&all, "equip"),
                tags: if check_tags { names(&all, "variable") } else { HashSet::new() },
                csv: csv.as_ref().map(|(_, r)| r.clone()),
                foreign,
                equip_prefix,
            };
            let items: Vec<Item> = s.pages.iter().flat_map(|pg| pg.items.iter().cloned()).collect();
            let mut findings = oneline::validate(&items, &ctx);
            let busbars = oneline::known_buses(&items);
            for pg in &s.pages {
                let own: Vec<&Item> = pg.items.iter().collect();
                findings.extend(oneline::stale_refs(&pg.project, &pg.page, &own, &busbars, &pg.zol_refs));
            }
            let count = |sev: &str| findings.iter().filter(|f| f.severity == sev).count();
            let mut by_rule: BTreeMap<&str, usize> = BTreeMap::new();
            for f in &findings {
                *by_rule.entry(f.rule).or_default() += 1;
            }
            let mut notes = Vec::new();
            if ctx.equipment.is_empty() {
                notes.push("No se encontró equip.dbf con filas: no se validan los equipos.");
            }
            if check_tags && ctx.tags.is_empty() {
                notes.push("No se encontró variable.dbf con filas: no se validan los tags.");
            }
            if csv.is_none() {
                notes.push("No hay AdvOneLine.csv en la carpeta del proyecto: no se compara con la configuración del runtime.");
            }
            Ok(json!({
                "project": projects[0].name,
                "projects_scanned": projects.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
                "pages_scanned": s.pages.len(),
                "objects": items.len(),
                "summary": {"errors": count("error"), "warnings": count("warning"), "info": count("info"), "by_rule": by_rule},
                "truncated": findings.len() > max,
                "findings": findings.iter().take(max).map(|f| f.json()).collect::<Vec<_>>(),
                "csv": csv.map(|(path, rows)| json!({"path": path, "components": rows.len()})),
                "unparsed_genies": unparsed_json(&s),
                "unreadable": s.unreadable,
                "notes": notes,
            }))
        })
        .await
        .map_err(err)?;
        pretty(&v, 200_000)
    }

    #[tool(name = "po_oneline_clone", description = "Clona una página del Advanced One-Line con números zOL nuevos: copia la página de origen con Graphics Builder y en cada genie renumera BkrNum/BusbarNum/MtrNum/SrcNum y las barras de origen/destino (mismo número viejo → mismo número nuevo, sin repetir los usados en el proyecto ni en AdvOneLine.csv) y reemplaza textos en las demás propiedades (Equip, Active, DetailsFn...: `replace` EstA_ → EstB_). Con apply=false (por defecto) solo devuelve el plan, offline: números nuevos, cambios por genie y lo que po_oneline_validate encontraría en la página nueva. Con apply=true guarda la página nueva (no toca la de origen) y la verifica leyendo el .ctg guardado. Confirmá el plan con el usuario antes de apply=true. Requiere POWEROP_ALLOW_WRITE=1.")]
    async fn oneline_clone(&self, Parameters(p): Parameters<OneLineCloneParams>) -> ToolResult {
        self.require_write("oneline_clone").map_err(err)?;
        let apply = p.apply.unwrap_or(false);
        let allow_partial = p.allow_partial.unwrap_or(false);
        let page = p.page.trim().to_string();
        let src_page = p.source_page.trim().to_string();
        if page.is_empty() || src_page.is_empty() {
            return Err("Indicá source_page y page".into());
        }
        let numbering = match p.renumber.as_deref().map(str::trim).unwrap_or("next_free") {
            "next_free" => Numbering::NextFree { start: p.start },
            "offset" => Numbering::Offset(p.offset.filter(|o| *o > 0).ok_or("renumber=\"offset\" necesita `offset` mayor que 0")?),
            "keep" => Numbering::Keep,
            o => return Err(format!("renumber '{o}' no existe: usá next_free, offset o keep")),
        };
        if p.start.is_some() && !matches!(numbering, Numbering::NextFree { .. }) || p.offset.is_some() && !matches!(numbering, Numbering::Offset(_)) {
            return Err("`start` va con renumber=\"next_free\" y `offset` con renumber=\"offset\"".into());
        }
        let spaces: Vec<Space> = match &p.spaces {
            Some(l) if !l.is_empty() => l
                .iter()
                .map(|s| Space::parse(s).ok_or_else(|| format!("espacio '{s}' no existe: usá bkr, bus, mtr o src")))
                .collect::<Result<_, _>>()?,
            _ => Space::ALL.to_vec(),
        };
        let replace: Vec<(String, String)> = p.replace.clone().unwrap_or_default().into_iter().filter(|r| !r.from.is_empty()).map(|r| (r.from, r.to)).collect();

        let dest = self.project(p.project.as_deref()).await.map_err(err)?;
        super::graphics::check_writable(&dest.name).map_err(err)?;
        let cat = self.catalog(false).await.map_err(err)?;
        let (dest_all, _) = cat.with_includes(&dest);
        let ctg_name = format!("{src_page}.ctg");
        let (src_proj, src_file) = match p.source_project.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(n) => {
                let sp = cat.resolve(n).map_err(err)?;
                let f = project::find_file(&sp.path, &ctg_name).ok_or_else(|| format!("No existe la página {src_page} en {}", sp.name))?;
                (sp, f)
            }
            None => dest_all
                .iter()
                .find_map(|pr| project::find_file(&pr.path, &ctg_name).map(|f| (pr.clone(), f)))
                .ok_or_else(|| format!("No existe la página {src_page} en {} ni en sus incluidos (indicá source_project)", dest.name))?,
        };
        if let Some(pr) = dest_all.iter().find(|pr| project::find_file(&pr.path, &format!("{page}.ctg")).is_some()) {
            return Err(format!("La página {page} ya existe en {}: elegí otro nombre (po_oneline_clone no reemplaza páginas)", pr.name));
        }

        let (sname, dall, pg, sp) = (src_proj.name.clone(), dest_all.clone(), page.clone(), src_page.clone());
        let plan = blocking(Duration::from_secs(120), move || clone_plan(&sname, &sp, &src_file, &dall, &pg, &numbering, &spaces, replace))
            .await
            .map_err(err)?;
        let mut v = plan.json.clone();
        if !apply {
            v["applied"] = json!(false);
            v["next_step"] = json!("Revisá el plan con el usuario (números nuevos, cambios, expected_findings) y repetí con apply=true.");
            return pretty(&v, 200_000);
        }

        if !blocking(Duration::from_secs(10), || Ok(crate::gb::registered())).await.unwrap_or(false) {
            return Err(
                "apply=true necesita Graphics Builder (puente GB: Power Operation Studio instalado y registrado). El plan se puede armar sin él (apply=false)."
                    .into(),
            );
        }
        let detail = format!("{}/{src_page} → {}/{page} ({} cambios en propiedades)", src_proj.name, dest.name, plan.planned);
        let (sname, dname, spage, npage) = (src_proj.name.clone(), dest.name.clone(), src_page.clone(), page.clone());
        let (rules, planned) = (plan.rules.clone(), plan.planned);
        // Graphics Builder tiene que ver el proyecto de origen y las librerías de los genies de la página.
        let mut needs = vec![sname.clone()];
        for g in &plan.source {
            if let Some(lp) = library_project(&dest_all, &g.library, "ctm")
                && !needs.iter().any(|n| n.eq_ignore_ascii_case(&lp))
            {
                needs.push(lp);
            }
        }
        let r = self
            .gb
            .call(move |gb| {
                gb.ensure(None)?;
                with_project(gb, &dname, &cat, &needs, |gb| {
                    if is_open(gb, &sname, &spage) {
                        bail!(
                            "La página de origen {sname}/{spage} está abierta en Graphics Builder: guardala o cerrala y repetí (el clon se arma \
                             abriéndola y guardándola con otro nombre)."
                        );
                    }
                    if is_open(gb, &dname, &npage) {
                        bail!("Hay una página {npage} abierta en Graphics Builder: cerrala y repetí.");
                    }
                    gb.m("PageOpen", &[Arg::Str(sname.clone()), Arg::Str(spage.clone())])
                        .map_err(|e| anyhow!("No se pudo abrir la página {sname}/{spage}: {e:#}"))?;
                    let r: Result<Value> = (|| {
                        let (objs, truncated) = page_objects(gb, 20_000, true);
                        if truncated {
                            bail!("La página tiene más de 20000 objetos: no se clona.");
                        }
                        let mut edits = Vec::new();
                        for o in &objs {
                            let (Some(props), Some(an)) = (o["properties"].as_object(), o["an"].as_i64().filter(|a| *a > 0)) else { continue };
                            let props: Vec<(String, String)> = props.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect();
                            let ch = oneline_clone::changes(&props, &rules);
                            if !ch.is_empty() {
                                edits.push((an as i16, ch));
                            }
                        }
                        let seen: usize = edits.iter().map(|(_, c)| c.len()).sum();
                        let dummies = objs.iter().filter(|o| o["class"].as_str().is_some_and(|c| c.eq_ignore_ascii_case("DummyMarker"))).count();
                        if seen < planned && dummies > 0 {
                            bail!(
                                "Graphics Builder ve {seen} cambios y el plan offline {planned}: muestra {dummies} genies como DummyMarker porque \
                                 {sname} (con sus incluidos) no ve sus librerías (p. ej. falta PLS_Include; ver missing_libraries del plan). \
                                 No se guardó nada. Agregá el incluido o cloná desde un proyecto que lo tenga."
                            );
                        }
                        if seen < planned && !allow_partial {
                            bail!(
                                "Graphics Builder ve {seen} cambios y el plan offline {planned}: hay genies que no encuentra o no puede editar. \
                                 No se guardó nada. Revisá la página de origen con po_gb_page_read o repetí con allow_partial=true (quedan con los valores \
                                 de origen; ver verification.not_applied)."
                            );
                        }
                        for (an, ch) in &edits {
                            gb.m("PageSelectObject", &[Arg::I2(*an)])?;
                            for c in ch {
                                gb.m("LibraryObjectPutProperty", &[Arg::Str(c.property.clone()), Arg::Str(c.new.clone())])
                                    .map_err(|e| anyhow!("AN {an}, {}: {e:#}. No se guardó nada.", c.property))?;
                            }
                        }
                        gb.m("PageSaveAs", &[Arg::Str(dname.clone()), Arg::Str(npage.clone())])?;
                        Ok(json!({"objects": objs.len(), "genies_edited": edits.len(), "properties_set": seen}))
                    })();
                    // Cierra la página nueva ya guardada, o la de origen sin guardar si algo falló.
                    close_page(gb);
                    let mut v = r?;
                    v["warnings"] = warnings_json(gb);
                    Ok(v)
                })
            })
            .await;
        let gbv = match r {
            Ok(g) => g,
            Err(e) => {
                let m = err(e);
                self.audit("oneline_clone", &detail, &format!("ERROR {m}"));
                return Err(m);
            }
        };
        let (src, rules, dall, pg) = (plan.source, plan.rules, dest_all, page.clone());
        let expected = plan.json["expected_findings"].as_array().cloned().unwrap_or_default();
        let verification = blocking(Duration::from_secs(120), move || Ok(clone_verify(&src, &rules, &dall, &pg, &expected))).await.map_err(err)?;
        let ok = verification["ok"].as_bool().unwrap_or(false);
        self.audit("oneline_clone", &detail, if ok { "OK" } else { "OK (verificación con observaciones)" });
        v["applied"] = json!(true);
        v["graphics_builder"] = gbv;
        v["verification"] = verification;
        v["next_step"] = json!(if ok {
            "Compilar el proyecto y regenerar AdvOneLine.csv con la One-Line Configuration Utility (los números zOL nuevos no están en el csv)."
        } else {
            "La página se guardó pero la verificación encontró diferencias (verification): revisalas con po_gb_page_read / po_oneline_validate \
             y corregí con po_gb_page_edit, o borrala con po_gb_page_delete."
        });
        pretty(&v, 200_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hallazgos_nuevos_respecto_del_plan() {
        let f = |sev: &str, rule: &str, msg: &str| json!({"severity": sev, "rule": rule, "message": msg, "at": []});
        let expected = vec![f("error", "InvalidEquipment", "El equipo 'B_Q1' no existe")];
        let found = vec![f("error", "InvalidEquipment", "El equipo 'B_Q1' no existe"), f("warning", "StaleZolReference", r"zOL\Bkr2 ...")];
        let n = new_findings(&found, &expected);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0]["rule"], "StaleZolReference");
        assert!(new_findings(&expected, &expected).is_empty());
    }

    #[test]
    fn numero_de_un_tag_zol() {
        assert_eq!(zol_number(r"zOL\Bkr502"), Some((Space::Bkr, 502)));
        assert_eq!(zol_number(r"zOL\Bus3"), Some((Space::Bus, 3)));
        assert_eq!(zol_number(r"zOL\Clr3"), None);
    }
}
