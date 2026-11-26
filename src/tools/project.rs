//! Herramientas offline sobre los archivos del proyecto (DBF y Cicode). No necesitan el runtime.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::cicode;
use crate::dbf::{Dbf, MASKED, is_secret_field};
use crate::project::{self, Project};
use crate::server::{PowerOpServer, ToolResult, blocking, err};
use crate::text::{decode, encode, filter_match, truncate};

fn pretty(v: &Value, max: usize) -> ToolResult {
    let s = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    Ok(truncate(&s, max))
}

const DISK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Deserialize, JsonSchema)]
pub struct ListParams {
    /// Volver a buscar proyectos en disco.
    #[serde(default)]
    pub refresh: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectParam {
    /// Nombre del proyecto (o ruta completa). Vacío = POWEROP_PROJECT / el que corre en runtime.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct QueryParams {
    #[serde(default)]
    pub project: Option<String>,
    /// Tabla DBF, p. ej. "variable", "digalm", "anaalm", "advalm", "trend", "units", "equip", "pages", "almcat".
    pub table: String,
    /// Condiciones por campo: {"TAG": "Feeder1*", "UNIT": "IODev1"}. Con * ? compara completo; sin comodines, "contiene". Sin distinguir mayúsculas.
    #[serde(default)]
    pub r#where: BTreeMap<String, String>,
    /// Texto a buscar en cualquier campo del registro.
    #[serde(default)]
    pub text: Option<String>,
    /// Campos a devolver (por defecto todos los no vacíos).
    #[serde(default)]
    pub fields: Option<Vec<String>>,
    /// Buscar también en los proyectos incluidos (librerías de Power Operation).
    #[serde(default)]
    pub include_included: bool,
    /// Máximo de filas (por defecto 100).
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SearchParams {
    #[serde(default)]
    pub project: Option<String>,
    /// Texto a buscar (sin distinguir mayúsculas; admite * ?), p. ej. un nombre de tag o de función.
    pub text: String,
    /// Incluir los proyectos incluidos (por defecto false).
    #[serde(default)]
    pub include_included: bool,
    /// Buscar también en archivos Cicode .ci (por defecto true).
    #[serde(default)]
    pub cicode: Option<bool>,
    /// Máximo de coincidencias (por defecto 200).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct CicodeFilesParams {
    #[serde(default)]
    pub project: Option<String>,
    /// Listar también los .ci de proyectos incluidos.
    #[serde(default)]
    pub include_included: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct CicodeReadParams {
    #[serde(default)]
    pub project: Option<String>,
    /// Archivo .ci (se busca también en los proyectos incluidos).
    pub file: String,
    /// Devolver solo esta función.
    #[serde(default)]
    pub function: Option<String>,
    /// Anteponer números de línea.
    #[serde(default)]
    pub numbered: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct CicodeWriteParams {
    #[serde(default)]
    pub project: Option<String>,
    /// Nombre del archivo .ci (en la carpeta del proyecto).
    pub file: String,
    /// Contenido completo del archivo.
    pub content: String,
    /// true = solo crear (falla si existe). false (por defecto) = crear o reemplazar (con copia de seguridad).
    #[serde(default)]
    pub create_only: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct CicodeCheckParams {
    /// Código Cicode a revisar.
    pub code: String,
}

/// Proyectos a recorrer (el pedido y, opcionalmente, sus incluidos).
async fn scope(srv: &PowerOpServer, project: Option<&str>, include: bool) -> Result<(Vec<Project>, Vec<String>)> {
    let p = srv.project(project).await?;
    if !include {
        return Ok((vec![p], vec![]));
    }
    Ok(srv.catalog(false).await?.with_includes(&p))
}

fn query_table(projects: &[Project], p: &QueryParams) -> Result<Value> {
    let limit = p.limit.unwrap_or(100).min(5000);
    let offset = p.offset.unwrap_or(0);
    let multi = projects.len() > 1;
    let mut rows = Vec::new();
    let mut matched = 0usize;
    let mut found_table = false;
    let mut field_names: Vec<String> = Vec::new();
    for proj in projects {
        let Some(path) = project::table_path(&proj.path, &p.table) else { continue };
        found_table = true;
        let dbf = Dbf::open(&path)?;
        field_names = dbf.fields.iter().map(|f| f.name.clone()).collect();
        let mut conds = Vec::new();
        for (k, v) in &p.r#where {
            let idx = dbf
                .field_index(k)
                .ok_or_else(|| anyhow!("La tabla {} no tiene el campo '{k}'. Campos: {}", p.table, field_names.join(", ")))?;
            // Filtrar por un hash de contraseña permitiría adivinarlo por partes.
            if is_secret_field(&dbf.fields[idx].name) {
                bail!("El campo {k} es de contraseña: no se puede filtrar por él (su valor se muestra como {MASKED}).");
            }
            conds.push((idx, v.as_str()));
        }
        let wanted: Option<Vec<usize>> = match &p.fields {
            Some(fs) if !fs.is_empty() => Some(
                fs.iter()
                    .map(|f| dbf.field_index(f).ok_or_else(|| anyhow!("Campo desconocido '{f}'. Campos: {}", field_names.join(", "))))
                    .collect::<Result<_>>()?,
            ),
            _ => None,
        };
        let secret: Vec<bool> = dbf.fields.iter().map(|f| is_secret_field(&f.name)).collect();
        dbf.for_each(|rec, vals| {
            if !conds.iter().all(|(i, pat)| filter_match(pat, &vals[*i])) {
                return true;
            }
            if let Some(t) = &p.text
                && !vals.iter().enumerate().any(|(i, v)| !secret[i] && filter_match(t, v))
            {
                return true;
            }
            matched += 1;
            if matched > offset && rows.len() < limit {
                let mut o = Map::new();
                if multi {
                    o.insert("_project".into(), json!(proj.name));
                }
                o.insert("_rec".into(), json!(rec));
                let idxs: Box<dyn Iterator<Item = usize>> = match &wanted {
                    Some(w) => Box::new(w.iter().copied()),
                    None => Box::new(0..vals.len()),
                };
                for i in idxs {
                    if wanted.is_some() || !vals[i].is_empty() {
                        let v = if secret[i] && !vals[i].is_empty() { MASKED } else { vals[i].as_str() };
                        o.insert(dbf.fields[i].name.clone(), json!(v));
                    }
                }
                rows.push(Value::Object(o));
            }
            true
        });
    }
    if !found_table {
        bail!("No existe la tabla '{}' en {}. Usá po_project_tables para ver las disponibles.", p.table, projects.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "));
    }
    Ok(json!({
        "table": p.table,
        "matched": matched,
        "returned": rows.len(),
        "offset": offset,
        "more": matched > offset + rows.len(),
        "fields": field_names,
        "rows": rows,
    }))
}

fn search(projects: &[Project], text: &str, cicode_too: bool, limit: usize) -> Result<Value> {
    let mut hits = Vec::new();
    let mut total = 0usize;
    for proj in projects {
        for path in project::files_with_ext(&proj.path, "dbf") {
            let Ok(dbf) = Dbf::open(&path) else { continue };
            let table = path.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
            let secret: Vec<bool> = dbf.fields.iter().map(|f| is_secret_field(&f.name)).collect();
            dbf.for_each(|rec, vals| {
                for (i, v) in vals.iter().enumerate() {
                    // Los hash de contraseña no se buscan ni se muestran.
                    if !secret[i] && !v.is_empty() && filter_match(text, v) {
                        total += 1;
                        if hits.len() < limit {
                            hits.push(json!({
                                "project": proj.name, "table": table, "rec": rec,
                                "field": dbf.fields[i].name, "value": v,
                                // Primer campo no vacío: suele ser el nombre del objeto.
                                "record_key": vals.iter().enumerate().find(|(j, x)| !secret[*j] && !x.is_empty()).map(|(_, x)| x),
                            }));
                        }
                    }
                }
                true
            });
        }
        if cicode_too {
            for path in project::files_with_ext(&proj.path, "ci") {
                let Ok(bytes) = std::fs::read(&path) else { continue };
                let file = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                for (n, line) in decode(&bytes).lines().enumerate() {
                    if filter_match(text, line) {
                        total += 1;
                        if hits.len() < limit {
                            hits.push(json!({"project": proj.name, "file": file, "line": n + 1, "text": line.trim()}));
                        }
                    }
                }
            }
        }
    }
    Ok(json!({"total_matches": total, "returned": hits.len(), "hits": hits}))
}

/// Busca un archivo .ci en los proyectos dados.
fn find_ci(projects: &[Project], file: &str) -> Result<(Project, PathBuf)> {
    let name = valid_ci_name(file)?;
    for p in projects {
        if let Some(path) = project::find_file(&p.path, &name) {
            return Ok((p.clone(), path));
        }
    }
    bail!("No se encontró {name} en {}", projects.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "))
}

fn valid_ci_name(file: &str) -> Result<String> {
    let f = file.trim();
    if f.is_empty() || f.contains(['\\', '/', ':']) || f.contains("..") {
        bail!("Nombre de archivo no válido: '{file}' (solo el nombre, sin carpetas)");
    }
    Ok(if f.to_lowercase().ends_with(".ci") { f.to_string() } else { format!("{f}.ci") })
}

pub(super) fn backup(path: &Path) -> Result<PathBuf> {
    let dir = path.parent().context("sin carpeta")?.join("_mcp_backup");
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    // Dos escrituras en el mismo segundo: sufijo -2, -3... para no pisar el respaldo del estado original.
    let mut dest = dir.join(format!("{name}.{stamp}.bak"));
    for n in 2.. {
        if !dest.exists() {
            break;
        }
        dest = dir.join(format!("{name}.{stamp}-{n}.bak"));
    }
    std::fs::copy(path, &dest)?;
    Ok(dest)
}

#[tool_router(router = project_router, vis = "pub(crate)")]
impl PowerOpServer {
    #[tool(name = "po_project_list", description = "Lista los proyectos de Power Operation encontrados en disco (carpetas User de ProgramData, citect.ini y POWEROP_PROJECTS_DIR), cuál corre en runtime y sus proyectos incluidos. En una carpeta User manda su MASTER.DBF: las carpetas no registradas salen aparte en unregistered_folders.")]
    async fn project_list(&self, Parameters(p): Parameters<ListParams>) -> ToolResult {
        let cat = self.catalog(p.refresh).await.map_err(err)?;
        let cat2 = cat.clone();
        let projects = blocking(DISK_TIMEOUT, move || {
            Ok(cat2
                .projects
                .iter()
                .map(|p| {
                    json!({
                        "name": p.name, "path": p.path, "running": p.running,
                        "includes": project::included_names(&p.path),
                        "cicode_files": project::files_with_ext(&p.path, "ci").len(),
                    })
                })
                .collect::<Vec<_>>())
        })
        .await
        .map_err(err)?;
        let inis: Vec<Value> = cat
            .inis
            .iter()
            .map(|i| json!({"path": i.path, "run": i.get("CtEdit", "Run"), "user": i.get("CtEdit", "User")}))
            .collect();
        let runtime_project = self.runtime_project(false).await;
        pretty(&json!({"default_project": self.cfg.default_project, "runtime_project": runtime_project, "citect_ini": inis, "searched_roots": cat.roots, "user_folders_with_master_dbf": cat.masters, "projects": projects, "unregistered_folders": cat.unregistered}), 80_000)
    }

    #[tool(name = "po_project_tables", description = "Tablas DBF de un proyecto con cantidad de registros y campos (nombre, tipo, largo). Útil para saber qué consultar con po_project_query.")]
    async fn project_tables(&self, Parameters(p): Parameters<ProjectParam>) -> ToolResult {
        let proj = self.project(p.project.as_deref()).await.map_err(err)?;
        let dir = proj.path.clone();
        let tables = blocking(DISK_TIMEOUT, move || {
            Ok(project::files_with_ext(&dir, "dbf")
                .iter()
                .map(|path| {
                    let name = path.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
                    match Dbf::open(path) {
                        Ok(d) => json!({
                            "table": name, "records": d.count(),
                            "fields": d.fields.iter().map(|f| format!("{}({}{})", f.name, f.kind, f.len)).collect::<Vec<_>>().join(" "),
                        }),
                        Err(e) => json!({"table": name, "error": format!("{e:#}")}),
                    }
                })
                .collect::<Vec<_>>())
        })
        .await
        .map_err(err)?;
        pretty(&json!({"project": proj.name, "path": proj.path, "tables": tables}), 120_000)
    }

    #[tool(name = "po_project_query", description = "Consulta una tabla DBF del proyecto (variable = tags, digalm/anaalm/advalm = alarmas, trend, units = dispositivos I/O, equip, pages...) con filtros por campo, texto libre, paginación y opcionalmente en proyectos incluidos.")]
    async fn project_query(&self, Parameters(p): Parameters<QueryParams>) -> ToolResult {
        let (projects, missing) = scope(self, p.project.as_deref(), p.include_included).await.map_err(err)?;
        let mut v = blocking(DISK_TIMEOUT, move || query_table(&projects, &p)).await.map_err(err)?;
        if !missing.is_empty() {
            v["included_not_found"] = json!(missing);
        }
        pretty(&v, 120_000)
    }

    #[tool(name = "po_project_search", description = "Busca un texto (nombre de tag, función, dispositivo...) en TODAS las tablas DBF y archivos Cicode del proyecto. Sirve para saber dónde se usa algo.")]
    async fn project_search(&self, Parameters(p): Parameters<SearchParams>) -> ToolResult {
        if p.text.trim().is_empty() {
            return Err("Indicá el texto a buscar".into());
        }
        let (projects, _) = scope(self, p.project.as_deref(), p.include_included).await.map_err(err)?;
        let limit = p.limit.unwrap_or(200).min(5000);
        let ci = p.cicode.unwrap_or(true);
        let v = blocking(DISK_TIMEOUT, move || search(&projects, &p.text, ci, limit)).await.map_err(err)?;
        pretty(&v, 120_000)
    }

    #[tool(name = "po_cicode_files", description = "Lista los archivos Cicode (.ci) del proyecto con las funciones que declara cada uno (nombre, línea, alcance, tipo de retorno).")]
    async fn cicode_files(&self, Parameters(p): Parameters<CicodeFilesParams>) -> ToolResult {
        let (projects, _) = scope(self, p.project.as_deref(), p.include_included).await.map_err(err)?;
        let v = blocking(DISK_TIMEOUT, move || {
            let mut files = Vec::new();
            for proj in &projects {
                for path in project::files_with_ext(&proj.path, "ci") {
                    let src = decode(&std::fs::read(&path)?);
                    let fns: Vec<String> = cicode::functions(&src).iter().map(|f| format!("{} (l.{})", f.name, f.line)).collect();
                    files.push(json!({
                        "project": proj.name,
                        "file": path.file_name().map(|n| n.to_string_lossy().to_string()),
                        "lines": src.lines().count(),
                        "functions": fns,
                    }));
                }
            }
            Ok(json!({"files": files}))
        })
        .await
        .map_err(err)?;
        pretty(&v, 120_000)
    }

    #[tool(name = "po_cicode_read", description = "Lee un archivo Cicode (.ci) del proyecto (o de sus incluidos), completo o solo una función.")]
    async fn cicode_read(&self, Parameters(p): Parameters<CicodeReadParams>) -> ToolResult {
        let (projects, _) = scope(self, p.project.as_deref(), true).await.map_err(err)?;
        blocking(DISK_TIMEOUT, move || {
            let (proj, path) = find_ci(&projects, &p.file)?;
            let src = decode(&std::fs::read(&path)?);
            let lines: Vec<&str> = src.lines().collect();
            let (from, to) = match &p.function {
                Some(name) => {
                    let fns = cicode::functions(&src);
                    let i = fns.iter().position(|f| f.name.eq_ignore_ascii_case(name)).ok_or_else(|| {
                        anyhow!("No hay una función '{name}' en {}. Funciones: {}", p.file, fns.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", "))
                    })?;
                    // Incluye los comentarios inmediatamente anteriores a la declaración.
                    let mut start = fns[i].line - 1;
                    while start > 0 {
                        let prev = lines[start - 1].trim_start();
                        if prev.starts_with("//") || prev.starts_with('!') || prev.starts_with("/*") || prev.starts_with('*') {
                            start -= 1;
                        } else {
                            break;
                        }
                    }
                    let end = fns.get(i + 1).map(|n| n.line - 1).unwrap_or(lines.len());
                    (start, end)
                }
                None => (0, lines.len()),
            };
            let body: Vec<String> =
                lines[from..to].iter().enumerate().map(|(k, l)| if p.numbered { format!("{:5}| {l}", from + k + 1) } else { l.to_string() }).collect();
            Ok(format!("// {} — proyecto {} — líneas {}-{} de {}\n{}", path.display(), proj.name, from + 1, to, lines.len(), body.join("\n")))
        })
        .await
        .map(|s| truncate(&s, 200_000))
        .map_err(err)
    }

    #[tool(name = "po_cicode_write", description = "Crea o reemplaza un archivo Cicode (.ci) en la carpeta del proyecto (guarda copia en _mcp_backup y hace un chequeo estructural). Después hay que compilar en Power Operation Studio. Confirmá con el usuario. Requiere POWEROP_ALLOW_WRITE=1.")]
    async fn cicode_write(&self, Parameters(p): Parameters<CicodeWriteParams>) -> ToolResult {
        self.require_write("cicode_write").map_err(err)?;
        let proj = self.project(p.project.as_deref()).await.map_err(err)?;
        let name = valid_ci_name(&p.file).map_err(err)?;
        let report = cicode::check(&p.content);
        let pname = proj.name.clone();
        let r = blocking(DISK_TIMEOUT, move || {
            let path = project::find_file(&proj.path, &name).unwrap_or_else(|| proj.path.join(&name));
            let mut note = "creado".to_string();
            if path.exists() {
                if p.create_only {
                    bail!("{} ya existe (create_only=true)", path.display());
                }
                note = format!("reemplazado; copia anterior en {}", backup(&path)?.display());
            }
            // Citect usa ANSI y fin de línea CRLF.
            let text = p.content.replace("\r\n", "\n").replace('\n', "\r\n");
            std::fs::write(&path, encode(&text))?;
            Ok((path, note))
        })
        .await;
        match r {
            Ok((path, note)) => {
                self.audit("cicode_write", &format!("{pname}: {}", path.display()), &note);
                pretty(
                    &json!({
                        "file": path, "result": note,
                        "structural_check": report,
                        "next_step": "Compilar el proyecto en Power Operation Studio (Compile) y recargar/reiniciar el runtime para que tome los cambios.",
                    }),
                    20_000,
                )
            }
            Err(e) => Err(err(e)),
        }
    }

    #[tool(name = "po_cicode_check", description = "Chequeo estructural de código Cicode sin compilar: bloques FUNCTION/IF/WHILE/FOR/SELECT CASE/REPEAT balanceados, paréntesis y cadenas; lista las funciones declaradas. No reemplaza compilar en Studio.")]
    async fn cicode_check(&self, Parameters(p): Parameters<CicodeCheckParams>) -> ToolResult {
        pretty(&serde_json::to_value(cicode::check(&p.code)).map_err(|e| e.to_string())?, 20_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbf::tests::build;
    use crate::testutil::TempDir;

    /// Proyecto temporal con variable.dbf (3 tags) y Breakers.ci. El TempDir lo borra al terminar.
    fn temp_project(name: &str) -> (TempDir, Project) {
        let dir = TempDir::new(&format!("proj-{name}"));
        std::fs::write(
            dir.join("variable.dbf"),
            build(
                &[("TAG", 40), ("TYPE", 10), ("UNIT", 16), ("COMMENT", 40)],
                &[
                    (&["Feeder1\\MMXU1\\TotW", "REAL", "IODev1", "Potencia activa total"], false),
                    (&["Feeder1\\XCBR1\\Pos", "DIGITAL", "IODev1", "Posición interruptor"], false),
                    (&["Feeder2\\MMXU1\\TotW", "REAL", "IODev2", ""], false),
                ],
            ),
        )
        .unwrap();
        std::fs::write(dir.join("Breakers.ci"), encode("// Abre\r\nINT FUNCTION BrkOpen(STRING s)\r\n  TagWrite(s, 0);\r\n  RETURN 0;\r\nEND\r\n")).unwrap();
        let proj = Project { name: name.into(), path: dir.path().to_path_buf(), running: false };
        (dir, proj)
    }

    #[test]
    fn query_and_search() {
        let (_dir, proj) = temp_project("q");
        let mut q = QueryParams {
            project: None,
            table: "Variable".into(),
            r#where: BTreeMap::from([("tag".into(), "Feeder1*".into())]),
            text: None,
            fields: Some(vec!["TAG".into(), "COMMENT".into()]),
            include_included: false,
            limit: Some(1),
            offset: None,
        };
        let v = query_table(std::slice::from_ref(&proj), &q).unwrap();
        assert_eq!((&v["matched"], &v["returned"], &v["more"]), (&json!(2), &json!(1), &json!(true)), "{v}");
        assert_eq!(v["rows"][0]["TAG"], "Feeder1\\MMXU1\\TotW");
        // Segunda página: la última fila, sin más.
        q.offset = Some(1);
        let v = query_table(std::slice::from_ref(&proj), &q).unwrap();
        assert_eq!((&v["returned"], &v["more"]), (&json!(1), &json!(false)), "{v}");
        assert_eq!(v["rows"][0]["TAG"], "Feeder1\\XCBR1\\Pos");

        let s = search(std::slice::from_ref(&proj), "XCBR1", true, 50).unwrap();
        assert_eq!(s["total_matches"], 1);
        let s = search(std::slice::from_ref(&proj), "tagwrite", true, 50).unwrap();
        assert_eq!(s["hits"][0]["line"], 3);

        let (_, path) = find_ci(std::slice::from_ref(&proj), "breakers").unwrap();
        assert!(path.ends_with("Breakers.ci"));
    }

    /// po_cicode_write solo recibe un nombre de archivo: nada de carpetas ni unidades.
    #[test]
    fn ci_names_without_folders() {
        assert_eq!(valid_ci_name(" Nuevo ").unwrap(), "Nuevo.ci");
        assert_eq!(valid_ci_name("Feeders.CI").unwrap(), "Feeders.CI");
        for bad in ["", "  ", "..\\x.ci", "../x.ci", "sub/x.ci", "C:x.ci", "a..b"] {
            let e = valid_ci_name(bad).unwrap_err().to_string();
            assert!(e.contains("sin carpetas"), "{bad:?}: {e}");
        }
    }

    /// Dos respaldos del mismo archivo en el mismo segundo no se pisan: el primero guarda el estado original.
    #[test]
    fn backups_in_the_same_second_keep_the_original() {
        let dir = TempDir::new("backup");
        let f = dir.join("variable.dbf");
        std::fs::write(&f, b"original").unwrap();
        let b1 = backup(&f).unwrap();
        std::fs::write(&f, b"cambio 1").unwrap();
        let b2 = backup(&f).unwrap();
        assert_ne!(b1, b2);
        assert_eq!(std::fs::read(&b1).unwrap(), b"original");
        assert_eq!(std::fs::read(&b2).unwrap(), b"cambio 1");
        assert!(b1.starts_with(dir.join("_mcp_backup")));
    }

    #[test]
    fn passwords_are_masked() {
        let (_dir, proj) = temp_project("pw");
        std::fs::write(
            proj.path.join("users.dbf"),
            build(&[("NAME", 16), ("PASSWORD", 40), ("SEED", 20), ("PWD_VER", 4)], &[(&["Operador", "AB12CD34", "S33D", ""], false)]),
        )
        .unwrap();
        let mut q = QueryParams {
            project: None,
            table: "users".into(),
            r#where: BTreeMap::new(),
            text: None,
            fields: None,
            include_included: false,
            limit: None,
            offset: None,
        };
        let v = query_table(std::slice::from_ref(&proj), &q).unwrap();
        let row = &v["rows"][0];
        assert_eq!(row["NAME"], "Operador");
        assert_eq!(row["PASSWORD"], MASKED);
        assert_eq!(row["SEED"], MASKED);
        assert!(row.get("PWD_VER").is_none(), "vacío: no se muestra");
        assert!(!v.to_string().contains("AB12CD34") && !v.to_string().contains("S33D"), "{v}");
        // Pedido explícito por fields: igual enmascarado; vacío sigue vacío.
        q.fields = Some(vec!["password".into(), "seed".into(), "pwd_ver".into()]);
        let v = query_table(std::slice::from_ref(&proj), &q).unwrap();
        assert_eq!(v["rows"][0]["PASSWORD"], MASKED);
        assert_eq!(v["rows"][0]["SEED"], MASKED);
        assert_eq!(v["rows"][0]["PWD_VER"], "");
        // No se puede adivinar el hash filtrando ni con texto libre.
        q.fields = None;
        q.r#where = BTreeMap::from([("password".into(), "AB*".into())]);
        assert!(query_table(std::slice::from_ref(&proj), &q).is_err());
        q.r#where = BTreeMap::new();
        q.text = Some("AB12*".into());
        assert_eq!(query_table(std::slice::from_ref(&proj), &q).unwrap()["matched"], 0);
        assert_eq!(search(std::slice::from_ref(&proj), "AB12CD34", false, 50).unwrap()["total_matches"], 0);
        assert_eq!(search(std::slice::from_ref(&proj), "Operador", false, 50).unwrap()["total_matches"], 1);
    }
}
