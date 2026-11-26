//! Advanced One-Line (offline, solo lectura): `po_oneline_read` y `po_oneline_validate`. Leen los `.ctg` del
//! proyecto con `ctg.rs` (sin Graphics Builder), así que ven también los genies ocultos y los de dentro de grupos.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::dbf::Dbf;
use crate::oneline::{self, Context, Item, Kind};
use crate::project::{self, Project};
use crate::server::{PowerOpServer, ToolResult, blocking, err};
use crate::text::{filter_match, truncate};
use crate::{ctg, text};

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
}
