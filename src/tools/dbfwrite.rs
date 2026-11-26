//! Escritura de configuración en las tablas DBF de un proyecto: tags (variable), dispositivos de memoria
//! (units), alarmas digitales (digalm), usuarios y roles. Solo en proyectos de prueba: MCP_Demo o carpetas con
//! el archivo `_mcp_created` (que crea po_project_create). Cada escritura respalda el .dbf y sus .ndx, escribe a
//! través de un .tmp y reconstruye los índices. Nunca escribe contraseñas.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::project::backup;
use crate::dbf::{Dbf, write_atomic};
use crate::ndx;
use crate::project::{self, Project};
use crate::server::{PowerOpServer, ToolResult, blocking, err};
use crate::text::filter_match;

/// Archivo que marca un proyecto creado por el MCP (lo crea po_project_create).
pub(crate) const MCP_MARKER: &str = "_mcp_created";
const DEMO_PROJECT: &str = "MCP_Demo";
/// Proyectos de sistema y de librería: el MCP nunca escribe en ellos, tengan o no el marcador.
const SYSTEM_PROJECTS: [&str; 5] = ["System", "Include", "PLS_Include", "PSO_Equipment", "IndustrialGraphics_Include"];
const DISK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Deserialize, JsonSchema)]
pub struct RowsAddParams {
    /// Nombre del proyecto (debe ser de prueba: MCP_Demo o con el archivo _mcp_created). Vacío = el por defecto.
    #[serde(default)]
    pub project: Option<String>,
    /// Tabla DBF del proyecto: "variable" (tags), "units" (dispositivos), "digalm" (alarmas digitales), "roles", "users"...
    pub table: String,
    /// Filas a agregar: cada una {CAMPO: valor}. Los campos no indicados quedan en blanco
    /// (si la tabla tiene GUID y no se indica, se genera uno).
    pub rows: Vec<BTreeMap<String, Value>>,
    /// Campo que no puede repetirse, p. ej. "TAG" o "NAME". Rechaza duplicados contra lo existente y dentro del lote.
    #[serde(default)]
    pub unique_field: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct RowsUpdateParams {
    /// Nombre del proyecto (debe ser de prueba: MCP_Demo o con el archivo _mcp_created). Vacío = el por defecto.
    #[serde(default)]
    pub project: Option<String>,
    /// Tabla DBF del proyecto, p. ej. "variable".
    pub table: String,
    /// Condiciones para elegir las filas: {CAMPO: patrón}. Mismo criterio que project_query: con * o ? compara
    /// el valor completo; sin comodines, "contiene". Obligatorio: nunca se actualiza sin condiciones.
    pub r#where: BTreeMap<String, String>,
    /// Valores a escribir en las filas que coincidan: {CAMPO: valor}. null = en blanco.
    pub set: BTreeMap<String, Value>,
    /// Cantidad de filas que deben coincidir. Si no coincide, no se escribe nada.
    #[serde(default)]
    pub expected: Option<usize>,
}

/// Permiso para escribir tablas DBF: solo MCP_Demo o proyectos con el archivo `_mcp_created` (nunca los de
/// sistema ni librerías).
pub(crate) fn ensure_dbf_writable(proj: &Project) -> Result<()> {
    let name = proj.name.trim();
    let system = SYSTEM_PROJECTS.iter().any(|s| name.eq_ignore_ascii_case(s)) || name.to_ascii_lowercase().starts_with("library_");
    if system {
        bail!("El proyecto '{name}' es de sistema o de librería: el MCP nunca escribe sus tablas.");
    }
    if name.eq_ignore_ascii_case(DEMO_PROJECT) || project::find_file(&proj.path, MCP_MARKER).is_some() {
        return Ok(());
    }
    bail!(
        "El proyecto '{name}' no es de prueba: el MCP escribe tablas DBF solo en '{DEMO_PROJECT}' o en proyectos creados por el MCP (con el archivo {MCP_MARKER} en su carpeta). Para otros proyectos, cargá los datos en Power Operation Studio."
    )
}

/// Campos de contraseña: el MCP nunca los escribe. Se cargan en Power Operation Studio.
fn reject_secrets<'a>(fields: impl Iterator<Item = &'a str>) -> Result<()> {
    for f in fields {
        if crate::dbf::is_secret_field(f) {
            bail!("El campo {f} es de contraseña: el MCP no escribe contraseñas. Cargala en Power Operation Studio (usuarios).");
        }
    }
    Ok(())
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

/// Tabla del proyecto abierta, su ruta y sus índices (.ndx). Valida los índices antes de escribir nada.
fn open_table(dir: &Path, table: &str) -> Result<(Dbf, PathBuf, Vec<PathBuf>)> {
    let path = project::table_path(dir, table)
        .ok_or_else(|| anyhow!("No existe la tabla '{table}' en {}. Usá po_project_tables para ver las disponibles.", dir.display()))?;
    let dbf = Dbf::open(&path)?;
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let indexes = ndx::indexes_for(dir, &stem);
    for p in &indexes {
        let file = file_name(p);
        let bytes = std::fs::read(p).with_context(|| format!("No se pudo leer {file}"))?;
        let h = ndx::read_header(&bytes).with_context(|| file.clone())?;
        ndx::key_field(&h, &dbf, &file)?;
    }
    Ok((dbf, path, indexes))
}

/// Respalda el .dbf y sus .ndx, escribe el .dbf nuevo (atómico) y reconstruye los índices.
/// Devuelve (respaldos, índices reconstruidos).
fn commit(path: &Path, indexes: &[PathBuf], bytes: Vec<u8>) -> Result<(Vec<String>, Vec<String>)> {
    let mut backups = vec![backup(path)?.display().to_string()];
    for p in indexes {
        backups.push(backup(p)?.display().to_string());
    }
    write_atomic(path, &bytes).with_context(|| format!("Respaldos hechos: {}", backups.join("; ")))?;
    let new = Dbf::parse(bytes)?;
    let mut rebuilt = Vec::new();
    for p in indexes {
        let n = ndx::rebuild(&new, p).with_context(|| {
            format!("el .dbf ya quedó escrito y falló la reconstrucción de un índice; respaldos: {}", backups.join("; "))
        })?;
        rebuilt.push(format!("{} ({n} claves)", file_name(p)));
    }
    Ok((backups, rebuilt))
}

/// Resultado de una escritura, para el agente.
#[derive(Debug)]
struct Report {
    table: String,
    file: PathBuf,
    /// Filas que coincidieron con `where` (solo en actualizaciones).
    matched: Option<usize>,
    written: usize,
    backups: Vec<String>,
    indexes: Vec<String>,
}

impl Report {
    fn json(&self, project: &str) -> Value {
        json!({
            "project": project,
            "table": self.table,
            "file": self.file,
            "matched": self.matched,
            "written": self.written,
            "backups": self.backups,
            "indexes_rebuilt": self.indexes,
            "next_step": "Compilar el proyecto (po_project_compile, o Compile en Power Operation Studio) para que el runtime tome los cambios.",
        })
    }
}

/// Rechaza las filas cuyo campo único ya existe (en la tabla o dentro del lote), sin distinguir mayúsculas.
fn check_unique(dbf: &Dbf, rows: &[BTreeMap<String, String>], field: &str) -> Result<()> {
    let idx = dbf
        .field_index(field)
        .ok_or_else(|| anyhow!("El campo único '{field}' no existe. Campos: {}", dbf.field_names()))?;
    let mut seen: HashSet<String> = HashSet::new();
    dbf.for_each(|_, v| {
        seen.insert(v[idx].to_lowercase());
        true
    });
    let mut dups = Vec::new();
    for (n, row) in rows.iter().enumerate() {
        let value = row
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(field))
            .map(|(_, v)| v.trim().to_string())
            .unwrap_or_default();
        if value.is_empty() {
            bail!("fila {}: falta el valor del campo único {field}", n + 1);
        }
        if !seen.insert(value.to_lowercase()) {
            dups.push(format!("fila {}: '{value}'", n + 1));
        }
    }
    if !dups.is_empty() {
        bail!("Hay duplicados en {field} (no se escribió nada): {}", dups.join(", "));
    }
    Ok(())
}

fn add_rows(dir: &Path, table: &str, rows: &[BTreeMap<String, String>], unique: Option<&str>) -> Result<Report> {
    if rows.is_empty() {
        bail!("No hay filas para agregar.");
    }
    for row in rows {
        reject_secrets(row.keys().map(String::as_str))?;
    }
    let (dbf, path, indexes) = open_table(dir, table)?;
    let guid = dbf.field_index("GUID");
    let mut appends = Vec::with_capacity(rows.len());
    for (n, row) in rows.iter().enumerate() {
        let vals: Vec<(String, String)> = row.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut cells = dbf.encode_values(&vals).map_err(|e| anyhow!("fila {}: {e:#}", n + 1))?;
        if let Some(g) = guid
            && !row.keys().any(|k| k.eq_ignore_ascii_case("GUID"))
        {
            cells.push((g, crate::dbf::encode_cell(&dbf.fields[g], &new_guid())?));
        }
        appends.push(cells);
    }
    if let Some(u) = unique {
        check_unique(&dbf, rows, u)?;
    }
    let bytes = dbf.rewrite(&[], &appends)?;
    let (backups, rebuilt) = commit(&path, &indexes, bytes)?;
    Ok(Report { table: table.into(), file: path, matched: None, written: rows.len(), backups, indexes: rebuilt })
}

fn update_rows(
    dir: &Path,
    table: &str,
    conds: &BTreeMap<String, String>,
    set: &BTreeMap<String, String>,
    expected: Option<usize>,
) -> Result<Report> {
    if conds.is_empty() {
        bail!("where vacío: no se actualiza sin condiciones. Indicá al menos un campo y su patrón.");
    }
    if let Some((k, _)) = conds.iter().find(|(_, p)| p.trim().is_empty()) {
        bail!("La condición de {k} está vacía: un patrón vacío coincidiría con todas las filas.");
    }
    if set.is_empty() {
        bail!("set vacío: indicá al menos un campo a escribir.");
    }
    reject_secrets(set.keys().map(String::as_str))?;
    let (dbf, path, indexes) = open_table(dir, table)?;
    let mut tests: Vec<(usize, &str)> = Vec::new();
    for (k, pat) in conds {
        let idx = dbf
            .field_index(k)
            .ok_or_else(|| anyhow!("La tabla {table} no tiene el campo '{k}'. Campos: {}", dbf.field_names()))?;
        tests.push((idx, pat.as_str()));
    }
    let mut matched: Vec<usize> = Vec::new();
    dbf.for_each(|rec, vals| {
        if tests.iter().all(|(i, pat)| filter_match(pat, &vals[*i])) {
            matched.push(rec);
        }
        true
    });
    if let Some(e) = expected
        && e != matched.len()
    {
        bail!("Coinciden {} filas y se esperaban {e}. No se escribió nada.", matched.len());
    }
    if matched.is_empty() {
        if expected == Some(0) {
            return Ok(Report { table: table.into(), file: path, matched: Some(0), written: 0, backups: vec![], indexes: vec![] });
        }
        bail!("Ninguna fila coincide con where. No se escribió nada.");
    }
    let vals: Vec<(String, String)> = set.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let cells = dbf.encode_values(&vals)?;
    let updates: Vec<(usize, Vec<(usize, Vec<u8>)>)> = matched.iter().map(|&r| (r, cells.clone())).collect();
    let bytes = dbf.rewrite(&updates, &[])?;
    let (backups, rebuilt) = commit(&path, &indexes, bytes)?;
    Ok(Report {
        table: table.into(),
        file: path,
        matched: Some(matched.len()),
        written: matched.len(),
        backups,
        indexes: rebuilt,
    })
}

/// GUID nuevo (v4) para las filas que lo necesitan. No es criptográfico: alcanza para identificar objetos.
fn new_guid() -> String {
    use std::hash::{BuildHasher, Hasher, RandomState};
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut b = [0u8; 16];
    for (i, chunk) in b.chunks_mut(8).enumerate() {
        let mut h = RandomState::new().build_hasher();
        h.write_usize(i);
        h.write_u128(nanos);
        chunk.copy_from_slice(&h.finish().to_le_bytes());
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// Valor JSON → texto para los campos DBF (null = en blanco).
fn value_text(v: &Value) -> Result<String> {
    Ok(match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => bail!("valor no admitido {other}: usá texto, número, booleano o null"),
    })
}

fn values(row: &BTreeMap<String, Value>) -> Result<BTreeMap<String, String>> {
    row.iter()
        .map(|(k, v)| -> Result<(String, String)> {
            Ok((k.clone(), value_text(v).with_context(|| format!("campo {k}"))?))
        })
        .collect()
}

fn texts(rows: &[BTreeMap<String, Value>]) -> Result<Vec<BTreeMap<String, String>>> {
    let mut out = Vec::with_capacity(rows.len());
    for (n, row) in rows.iter().enumerate() {
        out.push(values(row).with_context(|| format!("fila {}", n + 1))?);
    }
    Ok(out)
}

/// Registra la operación en la auditoría (OK y ERROR) y arma la respuesta.
fn finish(srv: &PowerOpServer, tool: &str, project: &str, detail: &str, r: Result<Report>) -> ToolResult {
    match r {
        Ok(rep) => {
            srv.audit(tool, detail, &format!("OK: {} escrita(s); respaldos: {}", rep.written, rep.backups.join(", ")));
            serde_json::to_string_pretty(&rep.json(project)).map_err(|e| e.to_string())
        }
        Err(e) => {
            srv.audit(tool, detail, &format!("ERROR: {e:#}"));
            Err(err(e))
        }
    }
}

#[tool_router(router = dbfwrite_router, vis = "pub(crate)")]
impl PowerOpServer {
    #[tool(name = "po_project_rows_add", description = "Agrega filas a una tabla DBF de un proyecto de prueba (MCP_Demo o con el archivo _mcp_created): tags (variable), dispositivos de memoria (units), alarmas digitales (digalm), roles/usuarios. Respalda el .dbf y los .ndx, escribe atómicamente y reconstruye los índices. Campos desconocidos, valores que no entran (largo máximo) o tipos mal formados dan error antes de escribir. unique_field rechaza duplicados. Nunca escribe contraseñas: esas se cargan en Studio. Requiere POWEROP_ALLOW_WRITE=1; confirmá con el usuario antes. Después compilar el proyecto.")]
    async fn project_rows_add(&self, Parameters(p): Parameters<RowsAddParams>) -> ToolResult {
        self.require_write("project_rows_add").map_err(err)?;
        let proj = self.project(p.project.as_deref()).await.map_err(err)?;
        let detail = format!("{}: {} +{} fila(s)", proj.name, p.table, p.rows.len());
        let rows = texts(&p.rows).map_err(err)?;
        let (table, unique) = (p.table, p.unique_field);
        let pj = proj.clone();
        let r = blocking(DISK_TIMEOUT, move || {
            ensure_dbf_writable(&pj)?;
            add_rows(&pj.path, &table, &rows, unique.as_deref())
        })
        .await;
        finish(self, "project_rows_add", &proj.name, &detail, r)
    }

    #[tool(name = "po_project_rows_update", description = "Actualiza filas existentes de una tabla DBF de un proyecto de prueba (MCP_Demo o con el archivo _mcp_created). Elige las filas con where (mismo criterio que po_project_query; obligatorio y sin patrones vacíos) y escribe set en todas ellas. expected = cantidad de filas que deben coincidir; si no coincide no se escribe nada. Respalda, escribe atómicamente y reconstruye los índices. Nunca escribe contraseñas. Requiere POWEROP_ALLOW_WRITE=1; confirmá con el usuario antes. Después compilar el proyecto.")]
    async fn project_rows_update(&self, Parameters(p): Parameters<RowsUpdateParams>) -> ToolResult {
        self.require_write("project_rows_update").map_err(err)?;
        let proj = self.project(p.project.as_deref()).await.map_err(err)?;
        let set = values(&p.set).map_err(err)?;
        let detail = format!("{}: {} where {:?} set {:?}", proj.name, p.table, p.r#where, set.keys().collect::<Vec<_>>());
        let (table, conds, expected) = (p.table, p.r#where, p.expected);
        let pj = proj.clone();
        let r = blocking(DISK_TIMEOUT, move || {
            ensure_dbf_writable(&pj)?;
            update_rows(&pj.path, &table, &conds, &set, expected)
        })
        .await;
        finish(self, "project_rows_update", &proj.name, &detail, r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbf::tests::build_typed;
    use crate::testutil::TempDir;

    const FIELDS: &[(&str, char, u8, u8)] = &[
        ("NAME", 'C', 40, 0),
        ("TYPE", 'C', 16, 0),
        ("COMMENT", 'C', 40, 0),
        ("GUID", 'C', 36, 0),
        ("VAL", 'N', 6, 1),
        ("ON", 'L', 1, 0),
    ];

    /// Carpeta temporal con un proyecto (marcado o no). Hay que mantener vivo el TempDir mientras dure el test.
    fn make_project(name: &str, marker: bool) -> (Project, TempDir) {
        let dir = TempDir::new(name);
        if marker {
            std::fs::write(dir.join(MCP_MARKER), "").unwrap();
        }
        (Project { name: name.into(), path: dir.path().to_path_buf(), running: false }, dir)
    }

    /// variable.dbf con dos filas vivas y una borrada, más variable.NDX (índice por NAME) bien armado.
    fn make_variable(dir: &Path) {
        let data = build_typed(
            FIELDS,
            &[
                (&["Feeder1\\XCBR1\\Pos", "INT", "posición", "g-1", "1.5", "T"], false),
                (&["Feeder1\\MMXU1\\TotW", "REAL", "potencia", "g-2", "2", "F"], false),
                (&["Borrado", "", "", "", "", ""], true),
            ],
        );
        std::fs::write(dir.join("variable.dbf"), data).unwrap();
        // Como dBase: el registro borrado (3) también está en el índice.
        let keys = vec![(b"Feeder1\\XCBR1\\Pos".to_vec(), 1u32), (b"Feeder1\\MMXU1\\TotW".to_vec(), 2u32), (b"Borrado".to_vec(), 3u32)];
        let h = ndx::read_header(&ndx::test_header("NAME", 40, 5)).unwrap();
        std::fs::write(dir.join("variable.NDX"), ndx::build(&h, keys)).unwrap();
    }

    fn pad(s: &str) -> Vec<u8> {
        let mut v = s.as_bytes().to_vec();
        v.resize(40, b' ');
        v
    }

    #[test]
    fn writable_only_for_test_projects() {
        let (demo, _d) = make_project("MCP_Demo", false);
        assert!(ensure_dbf_writable(&demo).is_ok());
        let (other, _o) = make_project("Foo", false);
        let e = ensure_dbf_writable(&other).unwrap_err().to_string();
        assert!(e.contains("no es de prueba") && e.contains(MCP_MARKER), "{e}");
        let (marked, _m) = make_project("Foo2", true);
        assert!(ensure_dbf_writable(&marked).is_ok());
        for sys in ["System", "Include", "Library_Controls", "PLS_Include", "PSO_Equipment", "IndustrialGraphics_Include"] {
            let (p, _p) = make_project(sys, true);
            let e = ensure_dbf_writable(&p).unwrap_err().to_string();
            assert!(e.contains("sistema o de librería"), "{sys} no debe ser escribible: {e}");
        }
    }

    #[test]
    fn passwords_are_rejected() {
        assert!(reject_secrets(["NAME", "PASSWORD"].into_iter()).is_err());
        assert!(reject_secrets(["PWD_VER"].into_iter()).is_err());
        // is_secret_field busca PASS, PWD y SEED sin distinguir mayúsculas.
        assert!(reject_secrets(["SEED"].into_iter()).is_err());
        assert!(reject_secrets(["password"].into_iter()).is_err());
        assert!(reject_secrets(["NAME", "ROLES"].into_iter()).is_ok());
    }

    #[test]
    fn add_rows_writes_backs_up_and_rebuilds_index() {
        let (proj, dir) = make_project("Prueba1", true);
        make_variable(dir.path());
        let rows = vec![BTreeMap::from([
            ("name".to_string(), "Feeder2\\XCBR1\\Pos".to_string()),
            ("TYPE".into(), "INT".into()),
            ("comment".into(), "nuevo".into()),
        ])];
        let rep = add_rows(&proj.path, "variable", &rows, Some("NAME")).unwrap();
        assert_eq!(rep.written, 1);
        assert_eq!(rep.backups.len(), 2);
        for b in &rep.backups {
            assert!(Path::new(b).exists(), "falta el respaldo {b}");
        }
        // Tres filas vivas, la nueva al final, con GUID generado.
        let dbf = Dbf::open(&dir.join("variable.dbf")).unwrap();
        let mut got = Vec::new();
        dbf.for_each(|_, v| {
            got.push(v);
            true
        });
        assert_eq!(got.len(), 3);
        assert_eq!(got[2][0], "Feeder2\\XCBR1\\Pos");
        assert_eq!(got[2][3].len(), 36);
        // El índice se reconstruyó con todos los registros (incluido el borrado), en orden.
        let keys = ndx::read_keys(&std::fs::read(dir.join("variable.NDX")).unwrap()).unwrap();
        assert_eq!(keys.iter().map(|k| k.1).collect::<Vec<_>>(), vec![3, 2, 1, 4]);
        assert_eq!(keys[3].0, pad("Feeder2\\XCBR1\\Pos"));

        // Duplicado contra lo existente (sin distinguir mayúsculas): no se escribe nada.
        let before = std::fs::read(dir.join("variable.dbf")).unwrap();
        let dup = vec![BTreeMap::from([("NAME".to_string(), "feeder1\\xcbr1\\pos".to_string())])];
        let e = add_rows(&proj.path, "variable", &dup, Some("NAME")).unwrap_err().to_string();
        assert!(e.contains("duplicad"), "{e}");
        assert_eq!(std::fs::read(dir.join("variable.dbf")).unwrap(), before);

        // Duplicado dentro del lote: no se escribe nada.
        let before = std::fs::read(dir.join("variable.dbf")).unwrap();
        let lote = vec![BTreeMap::from([("NAME".to_string(), "Z1".to_string())]), BTreeMap::from([("NAME".to_string(), "z1".to_string())])];
        let e = add_rows(&proj.path, "variable", &lote, Some("NAME")).unwrap_err().to_string();
        assert!(e.contains("duplicados"), "{e}");
        assert_eq!(std::fs::read(dir.join("variable.dbf")).unwrap(), before);

        // Valor demasiado largo: el error dice el máximo.
        let largo = vec![BTreeMap::from([("NAME".to_string(), "x".repeat(41))])];
        let e = add_rows(&proj.path, "variable", &largo, None).unwrap_err().to_string();
        assert!(e.contains("admite 40"), "{e}");
        assert_eq!(std::fs::read(dir.join("variable.dbf")).unwrap(), before);

        // Campo desconocido: lista los válidos.
        let raro = vec![BTreeMap::from([("NOMBRE".to_string(), "x".to_string())])];
        let e = add_rows(&proj.path, "variable", &raro, None).unwrap_err().to_string();
        assert!(e.contains("NOMBRE") && e.contains("NAME, TYPE"), "{e}");
        assert_eq!(std::fs::read(dir.join("variable.dbf")).unwrap(), before);

        // Contraseña: rechazada antes de tocar nada.
        let pw = vec![BTreeMap::from([("PASSWORD".to_string(), "x".to_string())])];
        let e = add_rows(&proj.path, "variable", &pw, None).unwrap_err().to_string();
        assert!(e.contains("es de contraseña"), "{e}");
        assert_eq!(std::fs::read(dir.join("variable.dbf")).unwrap(), before);
    }

    #[test]
    fn update_rows_checks_where_and_expected() {
        let (proj, dir) = make_project("Prueba2", true);
        make_variable(dir.path());
        let set = BTreeMap::from([("COMMENT".to_string(), "cambiado".to_string())]);
        let vacio: BTreeMap<String, String> = BTreeMap::new();
        let e = update_rows(&proj.path, "variable", &vacio, &set, None).unwrap_err().to_string();
        assert!(e.contains("where vacío"), "{e}");
        let patron_vacio = BTreeMap::from([("NAME".to_string(), "  ".to_string())]);
        assert!(update_rows(&proj.path, "variable", &patron_vacio, &set, None).is_err());

        let w = BTreeMap::from([("NAME".to_string(), "Feeder1*".to_string())]);
        let before = std::fs::read(dir.join("variable.dbf")).unwrap();
        let e = update_rows(&proj.path, "variable", &w, &set, Some(1)).unwrap_err().to_string();
        assert!(e.contains("se esperaban 1"), "{e}");
        assert_eq!(std::fs::read(dir.join("variable.dbf")).unwrap(), before);

        let rep = update_rows(&proj.path, "variable", &w, &set, Some(2)).unwrap();
        assert_eq!(rep.written, 2);
        let dbf = Dbf::open(&dir.join("variable.dbf")).unwrap();
        let mut comments = Vec::new();
        dbf.for_each(|_, v| {
            comments.push(v[2].clone());
            true
        });
        assert_eq!(comments, vec!["cambiado", "cambiado"]);

        // Cambiar la clave reordena el índice: "Zeta" queda al final.
        let w2 = BTreeMap::from([("NAME".to_string(), "Feeder1\\XCBR1*".to_string())]);
        let set2 = BTreeMap::from([("NAME".to_string(), "Zeta".to_string())]);
        update_rows(&proj.path, "variable", &w2, &set2, Some(1)).unwrap();
        let keys = ndx::read_keys(&std::fs::read(dir.join("variable.NDX")).unwrap()).unwrap();
        assert_eq!(keys.last().unwrap(), &(pad("Zeta"), 1));

        let none = BTreeMap::from([("NAME".to_string(), "nada*".to_string())]);
        assert!(update_rows(&proj.path, "variable", &none, &set, None).is_err());
    }
}
