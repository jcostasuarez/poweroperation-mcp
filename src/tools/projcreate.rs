//! Creación de proyectos Power Operation en blanco (`po_project_create`): carpeta nueva en la User del MASTER.DBF con
//! las tablas DBF vacías copiando el esquema de un proyecto instalado, índices vacíos, GLOBAL.CTL de referencia,
//! el marcador `_mcp_created` (para que po_project_rows_add/update puedan escribir), include.dbf y la fila en
//! MASTER.DBF (con respaldo). Nunca restaura .ctz ni compila. Todo se valida en memoria antes de tocar el disco.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Datelike, Local};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::dbfwrite::MCP_MARKER;
use super::project::backup;
use crate::dbf::{Dbf, write_atomic};
use crate::ndx;
use crate::project::{self, Catalog, Project};
use crate::server::{PowerOpServer, ToolResult, blocking, err};

const DISK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// Largo máximo del nombre (si MASTER.DBF tiene un campo NAME más corto, manda ese).
const MAX_NAME: usize = 64;
/// Esquema por defecto: el proyecto de prueba de este equipo.
const SCHEMA_DEFAULT: &str = "MCP_Demo";
/// Incluido por defecto (la librería gráfica/Cicode de Power Operation).
const INCLUDE_DEFAULT: &str = "PLS_Include";
/// Proyectos de sistema o de librería: nunca se crean ni se usan como destino.
const PROTECTED: [&str; 5] = ["System", "Include", "PLS_Include", "PSO_Equipment", "IndustrialGraphics_Include"];
const GLOBAL_CTL: &str = "GLOBAL.CTL";
const LIBRARY_CONTROLS: &str = "Library_Controls";

/// Resultado de la creación (para el JSON del agente).
#[derive(Debug)]
pub struct Summary {
    pub name: String,
    pub path: PathBuf,
    pub schema: PathBuf,
    /// Tablas DBF creadas (nombres en minúsculas).
    pub tables: Vec<String>,
    /// Índices .ndx creados (nombres en minúsculas).
    pub indexes: Vec<String>,
    pub includes: Vec<String>,
    /// Copia de MASTER.DBF hecha antes de agregar la fila.
    pub master_backup: PathBuf,
    /// Archivo marcador `_mcp_created` de la carpeta nueva.
    pub marker: PathBuf,
    /// Archivo del que se copió GLOBAL.CTL (Library_Controls o el esquema).
    pub global_ctl_from: PathBuf,
}

/// Nombre válido de proyecto: no vacío, letras sin acentos, dígitos, `_` y `-`, hasta `max` caracteres y no protegido.
fn validate_name(name: &str, max: usize) -> Result<()> {
    if name.is_empty() {
        bail!("Indicá el nombre del proyecto");
    }
    if name.len() > max {
        bail!("El nombre '{name}' tiene {} caracteres y el máximo es {max}", name.len());
    }
    check_chars(name)?;
    let protegido = PROTECTED.iter().any(|p| p.eq_ignore_ascii_case(name)) || name.to_ascii_lowercase().starts_with("library_");
    if protegido {
        bail!("'{name}' es un proyecto de sistema o de librería: no se crea ni se usa como destino");
    }
    Ok(())
}

fn check_chars(s: &str) -> Result<()> {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        Ok(())
    } else {
        bail!("'{s}' solo puede tener letras sin acentos, dígitos, _ y -")
    }
}

/// Incluidos: no vacíos, no repetidos y no el propio proyecto (que existan se verifica con el catálogo).
fn validate_includes(includes: &[String], name: &str) -> Result<()> {
    let mut vistos = HashSet::new();
    for inc in includes {
        if inc.is_empty() {
            bail!("Hay un incluido vacío");
        }
        if inc.eq_ignore_ascii_case(name) {
            bail!("El proyecto no puede incluirse a sí mismo ('{inc}')");
        }
        if !vistos.insert(inc.to_ascii_lowercase()) {
            bail!("El incluido '{inc}' está repetido");
        }
    }
    Ok(())
}

fn lower_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase();
    norm(a) == norm(b)
}

/// Cabecera de un .dbf del esquema con 0 registros, fecha de hoy y el fin de archivo (0x1A).
fn empty_table(raw: &[u8]) -> Result<Vec<u8>> {
    Dbf::parse(raw.to_vec())?;
    let hl = u16::from_le_bytes([raw[8], raw[9]]) as usize;
    let mut out = raw[..hl].to_vec();
    let now = Local::now();
    out[1] = (now.year() - 1900).clamp(0, 255) as u8;
    out[2] = now.month() as u8;
    out[3] = now.day() as u8;
    out[4..8].copy_from_slice(&0u32.to_le_bytes());
    out.push(0x1A);
    Ok(out)
}

/// Índice vacío con la cabecera del esquema (raíz 1, 2 bloques, un nodo raíz sin claves): 1024 bytes.
fn empty_index(raw: &[u8]) -> Result<Vec<u8>> {
    let h = ndx::read_header(raw)?;
    Ok(ndx::build(&h, vec![]))
}

/// Reconstruye en memoria un .ndx existente con los registros de `dbf` (como `ndx::rebuild`, sin escribir).
fn index_bytes(dbf: &Dbf, path: &Path) -> Result<Vec<u8>> {
    let file = lower_name(path);
    let old = std::fs::read(path).with_context(|| format!("No se pudo leer {}", path.display()))?;
    let h = ndx::read_header(&old).with_context(|| file.clone())?;
    let field = ndx::key_field(&h, dbf, &file)?;
    let keys: Vec<(Vec<u8>, u32)> = dbf.all_raw_records().into_iter().map(|(rec, raw)| (dbf.field_bytes(raw, field).to_vec(), rec as u32)).collect();
    Ok(ndx::build(&h, keys))
}

/// Carpeta User del proyecto: la que tiene MASTER.DBF y contiene al esquema; si no hay una sola, da error.
fn pick_user_dir(cat: &Catalog, schema: &Project) -> Result<PathBuf> {
    let parent = schema.path.parent();
    if let Some(m) = cat.masters.iter().find(|m| parent.is_some_and(|p| same_dir(p, m))) {
        return Ok(m.clone());
    }
    match cat.masters.as_slice() {
        [only] => Ok(only.clone()),
        [] => bail!("No hay ninguna carpeta User con MASTER.DBF. Indicá POWEROP_PROJECTS_DIR."),
        many => bail!(
            "Hay varias carpetas User con MASTER.DBF y el esquema no está en ninguna: {}",
            many.iter().map(|m| m.display().to_string()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Crea el proyecto `name` en `user_dir` copiando el esquema de `schema_dir`. `title` vacío = el nombre.
#[cfg(test)]
pub(crate) fn create_project(user_dir: &Path, schema_dir: &Path, name: &str, title: &str, includes: &[String]) -> Result<Summary> {
    create_project_in(user_dir, user_dir, schema_dir, name, title, includes)
}

/// Como `create_project`, pero la carpeta del proyecto va en `parent` (puede estar fuera de `user_dir`; la fila de
/// MASTER.DBF de `user_dir` lleva la ruta completa, como Transferencia_Demo).
/// Valida todo antes de escribir; si falla al escribir la carpeta, la borra (es nueva) y no toca MASTER.DBF.
pub(crate) fn create_project_in(
    user_dir: &Path,
    parent: &Path,
    schema_dir: &Path,
    name: &str,
    title: &str,
    includes: &[String],
) -> Result<Summary> {
    if !parent.is_dir() {
        bail!("La carpeta {} no existe (parent_dir tiene que ser una carpeta existente)", parent.display());
    }
    // MASTER.DBF: registro de proyectos (define el largo máximo del nombre y las filas vivas).
    let master_path = project::find_file(user_dir, "MASTER.DBF").ok_or_else(|| anyhow!("No hay MASTER.DBF en {}", user_dir.display()))?;
    let master_raw = std::fs::read(&master_path).with_context(|| format!("No se pudo leer {}", master_path.display()))?;
    let master = Dbf::parse(master_raw.clone()).with_context(|| format!("{} no es un DBF válido", master_path.display()))?;
    let name_idx = master.field_index("NAME").ok_or_else(|| anyhow!("MASTER.DBF no tiene el campo NAME"))?;

    let name = name.trim();
    validate_name(name, master.fields[name_idx].len.min(MAX_NAME))?;
    validate_includes(includes, name)?;
    let title = if title.trim().is_empty() { name.to_string() } else { title.trim().to_string() };

    // La carpeta y el registro no pueden existir (sin distinguir mayúsculas; las filas borradas no cuentan).
    let dst = parent.join(name);
    if dst.exists() {
        bail!("Ya existe la carpeta {}", dst.display());
    }
    let mut vivo = false;
    master.for_each(|_, v| {
        vivo = v[name_idx].trim().eq_ignore_ascii_case(name);
        !vivo
    });
    if vivo {
        bail!("El proyecto {name} ya está registrado en MASTER.DBF");
    }

    // Esquema: cabeceras de todas las .dbf (sin registros) y los .ndx vacíos.
    let dbfs = project::files_with_ext(schema_dir, "dbf");
    if dbfs.is_empty() {
        bail!("El esquema {} no tiene tablas .dbf", schema_dir.display());
    }
    let mut tables: Vec<(String, Vec<u8>)> = Vec::new();
    for f in &dbfs {
        let raw = std::fs::read(f).with_context(|| format!("No se pudo leer {}", f.display()))?;
        let vacia = empty_table(&raw).with_context(|| format!("esquema: {}", f.display()))?;
        tables.push((lower_name(f), vacia));
    }

    // include.dbf: una fila por incluido (sin las filas del esquema).
    let inc_pos = tables
        .iter()
        .position(|(n, _)| n == "include.dbf")
        .ok_or_else(|| anyhow!("El esquema {} no tiene include.dbf", schema_dir.display()))?;
    let inc_dbf = Dbf::parse(tables[inc_pos].1.clone())?;
    let appends = includes
        .iter()
        .map(|i| inc_dbf.encode_values(&[("NAME".to_string(), i.clone())]))
        .collect::<Result<Vec<_>>>()?;
    tables[inc_pos].1 = inc_dbf.rewrite(&[], &appends)?;

    let mut indexes: Vec<(String, Vec<u8>)> = Vec::new();
    for f in project::files_with_ext(schema_dir, "ndx") {
        let raw = std::fs::read(&f).with_context(|| format!("No se pudo leer {}", f.display()))?;
        let vacio = empty_index(&raw).with_context(|| format!("esquema: {}", f.display()))?;
        indexes.push((lower_name(&f), vacio));
    }

    // GLOBAL.CTL de referencia (352 bytes en una instalación sana): sin él Graphics Builder trata el
    // proyecto como versión < 5.31. Primero Library_Controls, después el esquema.
    let mut global: Option<(PathBuf, Vec<u8>)> = None;
    for dir in [user_dir.join(LIBRARY_CONTROLS), schema_dir.to_path_buf()] {
        if let Some(p) = project::find_file(&dir, GLOBAL_CTL) {
            let bytes = std::fs::read(&p).with_context(|| format!("No se pudo leer {}", p.display()))?;
            if !bytes.is_empty() {
                global = Some((p, bytes));
                break;
            }
        }
    }
    let (global_src, global_bytes) = global.ok_or_else(|| {
        anyhow!(
            "No hay {GLOBAL_CTL} de referencia en {}\\{LIBRARY_CONTROLS} ni en el esquema {}. Sin ese archivo Graphics Builder \
             trata el proyecto como versión < 5.31; no se creó nada.",
            user_dir.display(),
            schema_dir.display()
        )
    })?;

    // MASTER.DBF: fila nueva (solo con los campos que existan) y sus índices, todo en memoria.
    let now = Local::now();
    let fecha = match master.fields.iter().find(|f| f.name.eq_ignore_ascii_case("DATE")) {
        Some(f) if f.kind == 'D' => now.format("%Y-%m-%d").to_string(),
        _ => now.format("%-d %b %Y").to_string(),
    };
    let path_txt = format!("{}\\", dst.display());
    let valores: Vec<(String, String)> = [
        ("NAME", name.to_string()),
        ("TITLE", title.clone()),
        ("PATH", path_txt),
        ("MAJOR_VER", "0000".to_string()),
        ("MINOR_VER", "0000".to_string()),
        ("DATE", fecha),
        ("TIME", now.format("%H:%M").to_string()),
    ]
    .into_iter()
    .filter(|(f, _)| master.field_index(f).is_some())
    .map(|(f, v)| (f.to_string(), v))
    .collect();
    let fila = master.encode_values(&valores).context("MASTER.DBF: no entra la fila del proyecto")?;
    let master_new = master.rewrite(&[], &[fila])?;
    let master_new_dbf = Dbf::parse(master_new.clone())?;
    let master_ndx: Vec<(PathBuf, Vec<u8>)> = ndx::indexes_for(user_dir, "master")
        .into_iter()
        .map(|p| index_bytes(&master_new_dbf, &p).map(|b| (p, b)))
        .collect::<Result<_>>()?;

    // Escritura: carpeta nueva con todo lo del esquema.
    std::fs::create_dir(&dst).with_context(|| format!("No se pudo crear {}", dst.display()))?;
    let escritos = (|| -> Result<()> {
        for (n, b) in tables.iter().chain(indexes.iter()) {
            std::fs::write(dst.join(n), b).with_context(|| format!("No se pudo escribir {n}"))?;
        }
        std::fs::write(dst.join(GLOBAL_CTL), &global_bytes).context("No se pudo escribir GLOBAL.CTL")?;
        std::fs::write(dst.join(MCP_MARKER), b"").context("No se pudo escribir el marcador")?;
        Ok(())
    })();
    if let Err(e) = escritos {
        let _ = std::fs::remove_dir_all(&dst);
        return Err(e.context(format!("Se borró la carpeta nueva {} (estaba incompleta). MASTER.DBF no se modificó.", dst.display())));
    }

    // MASTER.DBF: respaldo primero; si falla antes de escribir, se borra la carpeta nueva.
    let master_backup = match backup(&master_path) {
        Ok(b) => b,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dst);
            return Err(e.context("No se pudo respaldar MASTER.DBF. Se borró la carpeta nueva; MASTER.DBF no se modificó."));
        }
    };
    if let Err(e) = write_atomic(&master_path, &master_new) {
        let _ = std::fs::remove_dir_all(&dst);
        return Err(e.context(format!("No se pudo escribir MASTER.DBF. Se borró la carpeta nueva. Respaldo: {}", master_backup.display())));
    }
    for (p, bytes) in &master_ndx {
        write_atomic(p, bytes).with_context(|| {
            format!(
                "MASTER.DBF ya tiene la fila de {name} y la carpeta {} existe, pero no se pudo reconstruir {}. Respaldo de MASTER: {}",
                dst.display(),
                p.display(),
                master_backup.display()
            )
        })?;
    }

    Ok(Summary {
        name: name.to_string(),
        path: dst.clone(),
        schema: schema_dir.to_path_buf(),
        tables: tables.iter().map(|(n, _)| n.clone()).collect(),
        indexes: indexes.iter().map(|(n, _)| n.clone()).collect(),
        includes: includes.to_vec(),
        master_backup,
        marker: dst.join(MCP_MARKER),
        global_ctl_from: global_src,
    })
}

/// Proyecto quitado del registro: nombre (como estaba en MASTER.DBF) y ruta de su carpeta (que queda en disco).
#[derive(Debug)]
pub struct Unregistered {
    pub name: String,
    pub path: String,
}

/// Quita `names` de MASTER.DBF de `user_dir`: marca sus filas vivas como borradas (como Studio) y reconstruye
/// los índices de MASTER. No toca las carpetas. Valida todo antes de escribir; respalda MASTER.DBF.
pub(crate) fn unregister_projects(user_dir: &Path, names: &[String]) -> Result<(Vec<Unregistered>, PathBuf)> {
    if names.is_empty() {
        bail!("Indicá al menos un proyecto");
    }
    for n in names {
        let protegido = PROTECTED.iter().any(|p| p.eq_ignore_ascii_case(n))
            || n.to_ascii_lowercase().starts_with("library_")
            || n.eq_ignore_ascii_case(LIBRARY_CONTROLS);
        if protegido {
            bail!("'{n}' es un proyecto de sistema o de librería: el MCP no lo quita del registro");
        }
    }
    let master_path = project::find_file(user_dir, "MASTER.DBF").ok_or_else(|| anyhow!("No hay MASTER.DBF en {}", user_dir.display()))?;
    let master = Dbf::open(&master_path)?;
    let name_idx = master.field_index("NAME").ok_or_else(|| anyhow!("MASTER.DBF no tiene el campo NAME"))?;
    let path_idx = master.field_index("PATH");
    let mut recs = Vec::new();
    let mut found = Vec::new();
    master.for_each(|rec, v| {
        if names.iter().any(|n| n.trim().eq_ignore_ascii_case(v[name_idx].trim())) {
            recs.push(rec);
            found.push(Unregistered { name: v[name_idx].trim().to_string(), path: path_idx.map(|i| v[i].trim().to_string()).unwrap_or_default() });
        }
        true
    });
    let faltan: Vec<&String> = names.iter().filter(|n| !found.iter().any(|f| f.name.eq_ignore_ascii_case(n.trim()))).collect();
    if !faltan.is_empty() {
        bail!("No están registrados en {}: {faltan:?}. No se modificó nada.", master_path.display());
    }
    let master_new = master.mark_deleted(&recs)?;
    let master_new_dbf = Dbf::parse(master_new.clone())?;
    let master_ndx: Vec<(PathBuf, Vec<u8>)> = ndx::indexes_for(user_dir, "master")
        .into_iter()
        .map(|p| index_bytes(&master_new_dbf, &p).map(|b| (p, b)))
        .collect::<Result<_>>()?;
    let master_backup = backup(&master_path).context("No se pudo respaldar MASTER.DBF; no se modificó nada")?;
    write_atomic(&master_path, &master_new).with_context(|| format!("No se pudo escribir MASTER.DBF. Respaldo: {}", master_backup.display()))?;
    for (p, bytes) in &master_ndx {
        write_atomic(p, bytes)
            .with_context(|| format!("MASTER.DBF ya se escribió, pero no se pudo reconstruir {}. Respaldo de MASTER: {}", p.display(), master_backup.display()))?;
    }
    Ok((found, master_backup))
}

/// ¿`user_dir`/MASTER.DBF tiene una fila viva con ese NAME?
fn master_has(user_dir: &Path, name: &str) -> bool {
    let Some(p) = project::find_file(user_dir, "MASTER.DBF") else { return false };
    let Ok(m) = Dbf::open(&p) else { return false };
    let Some(ni) = m.field_index("NAME") else { return false };
    let mut hay = false;
    m.for_each(|_, v| {
        hay |= v[ni].trim().eq_ignore_ascii_case(name);
        !hay
    });
    hay
}

/// Script de PowerShell (32 bits) que crea `PROJECT.CIT` (almacenamiento OLE con nombre, descripción, revisión,
/// fecha, versión, estilo y ProjectGUID) con `SE.SCADASystem.CitFileAccess.dll` de la carpeta bin. Copia las
/// propiedades de `src` (versión, estilo, fondo) y pone nombre, descripción, revisión 1.0, fecha de hoy y un GUID
/// nuevo. Si `dst` ya tiene un PROJECT.CIT legible no lo toca. Sin PROJECT.CIT, Studio lista el proyecto sin
/// revisión ni fecha y "Make Active" no hace nada (visto el 2026-10-16).
fn project_cit_script(bin: &Path, src: Option<&Path>, dst: &Path, name: &str, description: &str) -> String {
    use super::projadmin::ps_str;
    let s = |p: &Path| ps_str(&p.to_string_lossy());
    format!(
        "$ErrorActionPreference = 'Stop'\r\n\
         Set-Location -LiteralPath {bin}\r\n\
         $env:PATH = {bin} + ';' + $env:PATH\r\n\
         try {{\r\n\
         \x20   $a = [Reflection.Assembly]::LoadFrom((Join-Path {bin} 'SE.SCADASystem.CitFileAccess.dll'))\r\n\
         \x20   $PT = $a.GetType('SchneiderElectric.SCADASystem.ProjectProperties')\r\n\
         \x20   $CT = $a.GetType('SchneiderElectric.SCADASystem.CitFileAccess')\r\n\
         \x20   function Props($dir) {{\r\n\
         \x20       if (-not $dir) {{ return $null }}\r\n\
         \x20       $c = [Activator]::CreateInstance($CT, @($dir, [Text.Encoding]::Default))\r\n\
         \x20       try {{ $x = [object[]]@([Activator]::CreateInstance($PT)); $hr = $CT.GetMethod('GetProperties').Invoke($c, $x); if ($hr -ne 0) {{ return $null }}; return $x[0] }} finally {{ $c.Dispose() }}\r\n\
         \x20   }}\r\n\
         \x20   if (Props {dst}) {{ Write-Output 'CIT_RESULT=EXISTS'; exit 0 }}\r\n\
         \x20   $pp = Props {src}\r\n\
         \x20   if (-not $pp) {{ $pp = [Activator]::CreateInstance($PT); $pp.TemplateStyle = 'Standard'; $pp.Background = 21571; $pp.BackGroundColor = 12566463 }}\r\n\
         \x20   $now = Get-Date; $inv = [Globalization.CultureInfo]::InvariantCulture\r\n\
         \x20   $pp.Name = {name}; $pp.Description = {desc}; $pp.MajorRevision = '1'; $pp.MinorRevision = '0'\r\n\
         \x20   $pp.Date = $now.ToString('MM/dd/yyyy', $inv); $pp.Time = $now.ToString('HH:mm:ss', $inv)\r\n\
         \x20   $pp.IsCompiled = $false; $pp.IsReadOnly = $false; $pp.ProjectOID = ''; $pp.ProjectGUID = [guid]::NewGuid().ToString()\r\n\
         \x20   $c = [Activator]::CreateInstance($CT, @({dst}, [Text.Encoding]::Default))\r\n\
         \x20   try {{ $hr = $CT.GetMethod('SetProperties').Invoke($c, @($pp)) }} finally {{ $c.Dispose() }}\r\n\
         \x20   if ($hr -ne 0) {{ Write-Output ('CIT_RESULT=ERROR SetProperties devolvió ' + $hr); exit 3 }}\r\n\
         \x20   $r = Props {dst}\r\n\
         \x20   if (-not $r -or $r.ProjectGUID -ne $pp.ProjectGUID) {{ Write-Output 'CIT_RESULT=ERROR no se pudo releer PROJECT.CIT'; exit 3 }}\r\n\
         \x20   Write-Output ('CIT_RESULT=OK version=' + $r.Version + ' guid=' + $r.ProjectGUID)\r\n\
         \x20   exit 0\r\n\
         }} catch {{\r\n\
         \x20   Write-Output ('CIT_RESULT=ERROR ' + $_.Exception.GetBaseException().Message)\r\n\
         \x20   exit 2\r\n\
         }}\r\n",
        bin = s(bin),
        src = src.map(s).unwrap_or_else(|| "''".into()),
        dst = s(dst),
        name = ps_str(name),
        desc = ps_str(description),
    )
}

/// Crea `PROJECT.CIT` en `dst` (ver `project_cit_script`). Devuelve la línea de resultado ("OK ...", "EXISTS").
pub(crate) fn write_project_cit(bin: &Path, src: Option<&Path>, dst: &Path, name: &str, description: &str) -> Result<String> {
    let script = project_cit_script(bin, src, dst, name, description);
    let run = super::projadmin::run_ps32(&script, "cit", std::time::Duration::from_secs(60))?;
    if run.timed_out {
        bail!("PowerShell no terminó en 60 s al crear PROJECT.CIT");
    }
    let line = run.output.lines().find_map(|l| l.trim().strip_prefix("CIT_RESULT=")).map(str::to_string);
    match line {
        Some(l) if l.starts_with("OK") || l == "EXISTS" => Ok(l),
        Some(l) => bail!("No se pudo crear PROJECT.CIT: {l}"),
        None => bail!("No se pudo crear PROJECT.CIT (salida de PowerShell: {})", crate::text::truncate(&run.output, 500)),
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct CreateParams {
    /// Nombre de la carpeta y del proyecto: letras sin acentos, dígitos, _ y -; hasta 64 caracteres. No puede existir
    /// ni ser de sistema o librería (System, Include, PLS_Include, PSO_Equipment, IndustrialGraphics_Include, Library_*).
    pub name: String,
    /// Título visible del proyecto. Por defecto, el nombre.
    #[serde(default)]
    pub title: Option<String>,
    /// Proyectos incluidos (deben existir en el catálogo). Por defecto ["PLS_Include"]; [] = ninguno.
    #[serde(default)]
    pub includes: Option<Vec<String>>,
    /// Proyecto cuyo esquema de tablas (cabeceras DBF e índices) se copia. Por defecto MCP_Demo si existe.
    #[serde(default)]
    pub schema_from: Option<String>,
    /// Carpeta existente (ruta absoluta) donde crear la del proyecto, p. ej. `C:\Users\Usuario\Desktop\prueba`.
    /// Por defecto, la carpeta User de MASTER.DBF. Se registra igual en ese MASTER.DBF, con la ruta completa.
    #[serde(default)]
    pub parent_dir: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct UnregisterParams {
    /// Proyectos a quitar del registro (MASTER.DBF), por nombre. Las carpetas quedan en disco.
    pub names: Vec<String>,
}

#[tool_router(router = projcreate_router, vis = "pub(crate)")]
impl PowerOpServer {
    #[tool(name = "po_project_create", description = "ESCRITURA: crea un proyecto de Power Operation en blanco en la carpeta User de MASTER.DBF (o en parent_dir, registrado igual en ese MASTER.DBF): tablas DBF vacías con el esquema de schema_from (por defecto MCP_Demo), índices vacíos, GLOBAL.CTL de referencia (de Library_Controls), marcador _mcp_created (permite po_project_rows_add/po_project_rows_update), include.dbf con los incluidos, PROJECT.CIT (propiedades que lee Studio, con GUID nuevo; sin él Studio no deja hacerlo activo) y fila en MASTER.DBF (con respaldo). Rechaza nombres existentes (carpeta, catálogo o MASTER.DBF) y de sistema/librería. No restaura .ctz y no compila: después cargá tags con po_project_rows_add y compilá con po_project_compile. Requiere POWEROP_ALLOW_WRITE=1; confirmá con el usuario antes.")]
    async fn project_create(&self, Parameters(p): Parameters<CreateParams>) -> ToolResult {
        let detalle = format!(
            "nombre={} schema_from={} parent_dir={} includes={}",
            p.name.trim(),
            p.schema_from.as_deref().unwrap_or("(por defecto)"),
            p.parent_dir.as_deref().unwrap_or("(User)"),
            p.includes.as_ref().map(|v| v.join(",")).unwrap_or_else(|| INCLUDE_DEFAULT.to_string())
        );
        let r = match self.require_write("project_create") {
            Ok(()) => self.create_inner(p).await,
            Err(e) => Err(e),
        };
        match r {
            Ok(j) => {
                self.audit("project_create", &detalle, &format!("OK: {}", j["path"].as_str().unwrap_or_default()));
                serde_json::to_string_pretty(&j).map_err(|e| e.to_string())
            }
            Err(e) => {
                self.audit("project_create", &detalle, &format!("ERROR: {e:#}"));
                Err(err(e))
            }
        }
    }

    #[tool(name = "po_project_unregister", description = "ESCRITURA: quita proyectos del registro de Power Operation (marca sus filas de MASTER.DBF como borradas, con respaldo, y reconstruye master.ndx). Las carpetas y archivos del proyecto quedan en disco: para volver a verlo hay que registrarlo de nuevo desde Studio. Rechaza proyectos de sistema o librería (System, Include, PLS_Include, PSO_Equipment, IndustrialGraphics_Include, Library_*), proyectos en ejecución y proyectos incluidos por otro que queda registrado. Si algo falla no modifica nada. Requiere POWEROP_ALLOW_WRITE=1; confirmá con el usuario antes.")]
    async fn project_unregister(&self, Parameters(p): Parameters<UnregisterParams>) -> ToolResult {
        let detalle = format!("names={}", p.names.join(","));
        let r = match self.require_write("project_unregister") {
            Ok(()) => self.unregister_inner(p.names).await,
            Err(e) => Err(e),
        };
        match r {
            Ok(j) => {
                self.audit("project_unregister", &detalle, &format!("OK: {}", j["results"]));
                serde_json::to_string_pretty(&j).map_err(|e| e.to_string())
            }
            Err(e) => {
                self.audit("project_unregister", &detalle, &format!("ERROR: {e:#}"));
                Err(err(e))
            }
        }
    }
}

impl PowerOpServer {
    /// Validación contra el catálogo (existen, no corren, nadie que queda los incluye) y baja en cada MASTER.DBF.
    async fn unregister_inner(&self, names: Vec<String>) -> Result<Value> {
        let names: Vec<String> = names.iter().map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).collect();
        if names.is_empty() {
            bail!("Indicá al menos un proyecto");
        }
        let cat = self.catalog(true).await?;
        let mut targets = Vec::new();
        for n in &names {
            let pr = cat.resolve(n).map_err(|_| anyhow!("'{n}' no está en el catálogo de proyectos. No se modificó nada."))?;
            if pr.running {
                bail!("'{}' está en ejecución: detenelo antes de quitarlo del registro. No se modificó nada.", pr.name);
            }
            targets.push(pr);
        }
        // Un proyecto que queda no puede incluir a uno que se va: dejaría de compilar.
        for pr in cat.projects.iter().filter(|x| !targets.iter().any(|t| t.name.eq_ignore_ascii_case(&x.name))) {
            let inc = project::included_names(&pr.path);
            if let Some(t) = targets.iter().find(|t| inc.iter().any(|i| i.eq_ignore_ascii_case(&t.name))) {
                bail!("'{}' lo incluye '{}', que queda registrado. Quitá también '{}' o sacalo de sus incluidos. No se modificó nada.", t.name, pr.name, pr.name);
            }
        }
        // Cada uno se quita del MASTER.DBF que lo registra (normalmente hay uno solo).
        let mut by_user: Vec<(PathBuf, Vec<String>)> = Vec::new();
        for t in &targets {
            let user = cat
                .masters
                .iter()
                .find(|m| master_has(m, &t.name))
                .cloned()
                .ok_or_else(|| anyhow!("No encuentro el MASTER.DBF donde está registrado '{}'. No se modificó nada.", t.name))?;
            match by_user.iter_mut().find(|(u, _)| same_dir(u, &user)) {
                Some((_, v)) => v.push(t.name.clone()),
                None => by_user.push((user, vec![t.name.clone()])),
            }
        }
        let mut out = Vec::new();
        for (user, ns) in by_user {
            let u = user.clone();
            let (gone, bk) = blocking(DISK_TIMEOUT, move || unregister_projects(&u, &ns)).await?;
            out.push(json!({
                "master": user,
                "master_backup": bk,
                "unregistered": gone.iter().map(|g| json!({"name": g.name, "folder_left_on_disk": g.path})).collect::<Vec<_>>(),
            }));
        }
        let _ = self.catalog(true).await;
        Ok(json!({
            "results": out,
            "note": "Las carpetas quedan en disco. Si Studio está abierto, puede seguir mostrándolos hasta refrescar la lista o reiniciarlo.",
        }))
    }

    /// Validación contra el catálogo (existencia de incluidos y esquema, nombre libre) y creación en disco.
    async fn create_inner(&self, p: CreateParams) -> Result<Value> {
        let name = p.name.trim().to_string();
        validate_name(&name, MAX_NAME)?;
        let cat = self.catalog(true).await?;
        if cat.projects.iter().any(|x| x.name.eq_ignore_ascii_case(&name)) {
            bail!("Ya existe un proyecto '{name}' en el catálogo");
        }
        let mut includes = Vec::new();
        for i in p.includes.unwrap_or_else(|| vec![INCLUDE_DEFAULT.to_string()]) {
            let i = i.trim();
            let pr = cat.resolve(i).map_err(|_| anyhow!("El incluido '{i}' no existe en el catálogo de proyectos"))?;
            includes.push(pr.name);
        }
        let schema = match p.schema_from.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => cat.resolve(s)?,
            None => cat
                .resolve(SCHEMA_DEFAULT)
                .map_err(|_| anyhow!("Indicá schema_from: no existe {SCHEMA_DEFAULT} para copiar el esquema"))?,
        };
        let user = pick_user_dir(&cat, &schema)?;
        let title = p.title.unwrap_or_default();
        let parent = match p.parent_dir.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(d) if !Path::new(d).is_absolute() => bail!("parent_dir tiene que ser una ruta absoluta: {d}"),
            Some(d) => PathBuf::from(d),
            None => user.clone(),
        };

        let (u, s, n, inc) = (user.clone(), schema.path.clone(), name.clone(), includes.clone());
        let desc = if title.trim().is_empty() { name.clone() } else { title.clone() };
        let sum = blocking(DISK_TIMEOUT, move || create_project_in(&u, &parent, &s, &n, &title, &inc)).await?;
        // PROJECT.CIT (Studio lo necesita para mostrar la revisión y hacer activo el proyecto). Si falla, el proyecto
        // queda creado igual y se informa.
        let inis = cat.inis.clone();
        let (src, dst, n) = (schema.path.clone(), sum.path.clone(), sum.name.clone());
        let cit = blocking(std::time::Duration::from_secs(90), move || {
            let (dll, _) = super::projadmin::locate(&inis)?;
            let bin = dll.parent().map(Path::to_path_buf).unwrap_or_default();
            write_project_cit(&bin, Some(&src), &dst, &n, &desc)
        })
        .await;
        let project_cit = match cit {
            Ok(l) => json!(format!("PROJECT.CIT {l}")),
            Err(e) => json!(format!("ADVERTENCIA: {e:#}. Studio puede no dejar hacerlo activo; reintentá con po_studio_set_project.")),
        };
        // Refresca el catálogo: el proyecto nuevo ya está en disco aunque esto falle.
        let _ = self.catalog(true).await;
        Ok(json!({
            "project": sum.name,
            "path": sum.path,
            "schema_from": sum.schema,
            "tables": sum.tables,
            "indexes": sum.indexes,
            "includes": sum.includes,
            "master_backup": sum.master_backup,
            "marker": sum.marker,
            "global_ctl_from": sum.global_ctl_from,
            "project_cit": project_cit,
            "next": "cargar tags con po_project_rows_add, compilar con po_project_compile; para Studio, po_studio_set_project",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbf::tests::{build, build_typed};
    use crate::testutil::TempDir;
    use chrono::{Datelike, Local};

    /// User temporal: MASTER.DBF (Demo viva con carpeta, Viejo borrada, Fantasma viva sin carpeta), master.ndx,
    /// esquema MCP_Demo (VARIABLE.DBF con filas, digalm.dbf, include.dbf con una fila, variable.ndx) y
    /// Library_Controls\GLOBAL.CTL de 352 bytes. El TempDir lo borra al terminar.
    struct Fx {
        root: TempDir,
        user: PathBuf,
        schema: PathBuf,
        global: Vec<u8>,
        master: Vec<u8>,
    }

    fn fixture() -> Fx {
        let root = TempDir::new("create");
        let user = root.join("User");
        let schema = user.join("MCP_Demo");
        std::fs::create_dir_all(&schema).unwrap();
        std::fs::create_dir_all(user.join("Demo")).unwrap();
        std::fs::write(user.join("Demo").join("variable.dbf"), b"x").unwrap();
        let lib = user.join(LIBRARY_CONTROLS);
        std::fs::create_dir_all(&lib).unwrap();
        let global: Vec<u8> = (0..352u32).map(|i| (i * 7 % 251) as u8).collect();
        std::fs::write(lib.join("GLOBAL.CTL"), &global).unwrap();

        let master = build(
            &[("NAME", 64), ("TITLE", 64), ("PATH", 240), ("MAJOR_VER", 4), ("MINOR_VER", 4), ("DATE", 11), ("TIME", 5)],
            &[
                (&["Demo", "Demo", "C:\\x\\User\\Demo\\", "0000", "0000", "1 Jan 2026", "10:00"], false),
                (&["Viejo", "Viejo", "", "0000", "0000", "", ""], true),
                (&["Fantasma", "Fantasma", "", "0000", "0000", "", ""], false),
            ],
        );
        std::fs::write(user.join("MASTER.DBF"), &master).unwrap();
        std::fs::write(user.join("master.ndx"), ndx::test_header("NAME", 64, 4)).unwrap();

        std::fs::write(
            schema.join("VARIABLE.DBF"),
            build(&[("TAG", 16), ("DESC", 40)], &[(&["Feeder1", "x"], false), (&["Feeder2", "y"], false)]),
        )
        .unwrap();
        std::fs::write(schema.join("digalm.dbf"), build_typed(&[("TAG", 'C', 16, 0), ("VALOR", 'N', 8, 2)], &[(&["a", "1.50"], false)])).unwrap();
        std::fs::write(schema.join("include.dbf"), build(&[("NAME", 64), ("COMMENT", 40)], &[(&["Viejo_Inc", "c"], false)])).unwrap();
        std::fs::write(schema.join("variable.ndx"), ndx::test_header("TAG", 16, 4)).unwrap();
        Fx { root, user, schema, global, master }
    }

    /// Un intento fallido no deja rastro: ni carpeta, ni MASTER.DBF cambiado, ni respaldo.
    fn assert_nothing_created(fx: &Fx, name: &str) {
        assert!(!fx.user.join(name).exists(), "quedó la carpeta {name}");
        assert_eq!(std::fs::read(fx.user.join("MASTER.DBF")).unwrap(), fx.master, "MASTER.DBF cambió");
        assert!(!fx.user.join("_mcp_backup").exists(), "quedó un respaldo de MASTER.DBF");
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        v.sort();
        v
    }

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    /// Fila viva de MASTER.DBF con ese NAME (campo -> valor).
    fn master_row(fx: &Fx, name: &str) -> Option<std::collections::HashMap<String, String>> {
        let m = Dbf::open(&fx.user.join("MASTER.DBF")).unwrap();
        let ni = m.field_index("NAME").unwrap();
        let mut fila = None;
        m.for_each(|_, v| {
            if v[ni] == name {
                fila = Some(m.fields.iter().map(|f| f.name.clone()).zip(v.iter().cloned()).collect());
            }
            true
        });
        fila
    }

    #[test]
    fn crea_proyecto_completo() {
        let fx = fixture();
        let s = create_project(&fx.user, &fx.schema, "Nuevo", "Proyecto nuevo", &["PLS_Include".to_string()]).unwrap();
        let dst = fx.user.join("Nuevo");
        assert_eq!(s.path, dst);

        // Carpeta: tablas en minúsculas (sin VARIABLE.DBF), índice, GLOBAL.CTL y marcador.
        assert_eq!(names_in(&dst), ["GLOBAL.CTL", "_mcp_created", "digalm.dbf", "include.dbf", "variable.dbf", "variable.ndx"]);
        assert_eq!(s.tables, ["digalm.dbf", "include.dbf", "variable.dbf"]);
        assert_eq!(s.indexes, ["variable.ndx"]);

        // Tabla: cabecera igual al esquema salvo fecha y conteo; 0 registros; termina en 0x1A.
        let schema_v = std::fs::read(fx.schema.join("VARIABLE.DBF")).unwrap();
        let new_v = std::fs::read(dst.join("variable.dbf")).unwrap();
        let hl = u16::from_le_bytes([schema_v[8], schema_v[9]]) as usize;
        assert_eq!(new_v.len(), hl + 1);
        assert_eq!(new_v[hl], 0x1A);
        assert_eq!(&new_v[8..hl], &schema_v[8..hl]);
        assert_eq!(u32_at(&new_v, 4), 0);
        let now = Local::now();
        assert_eq!(new_v[1..4], [(now.year() - 1900) as u8, now.month() as u8, now.day() as u8]);
        assert_eq!(Dbf::open(&dst.join("variable.dbf")).unwrap().count(), 0);

        // Índice vacío: 1024 bytes, raíz 1, 2 bloques, nodo raíz sin claves.
        let idx = std::fs::read(dst.join("variable.ndx")).unwrap();
        assert_eq!(idx.len(), 1024);
        assert_eq!((u32_at(&idx, 0), u32_at(&idx, 4), u32_at(&idx, 512)), (1, 2, 0));

        // GLOBAL.CTL: copia byte a byte de Library_Controls.
        assert_eq!(std::fs::read(dst.join("GLOBAL.CTL")).unwrap(), fx.global);
        assert_eq!(s.global_ctl_from, fx.user.join(LIBRARY_CONTROLS).join("GLOBAL.CTL"));
        assert!(dst.join(MCP_MARKER).is_file());
        assert_eq!(s.marker, dst.join(MCP_MARKER));

        // include.dbf: una fila (la del esquema no se copia).
        let inc = Dbf::open(&dst.join("include.dbf")).unwrap();
        let i = inc.field_index("NAME").unwrap();
        let mut nombres = Vec::new();
        inc.for_each(|_, v| {
            nombres.push(v[i].clone());
            true
        });
        assert_eq!(nombres, ["PLS_Include"]);
        assert_eq!(s.includes, ["PLS_Include"]);

        // MASTER: respaldo idéntico al original y fila nueva al final (Viejo sigue borrada).
        assert_eq!(std::fs::read(&s.master_backup).unwrap(), fx.master);
        assert!(s.master_backup.starts_with(fx.user.join("_mcp_backup")));
        let m = Dbf::open(&fx.user.join("MASTER.DBF")).unwrap();
        assert_eq!((m.declared, m.count()), (4, 3));
        let fila = master_row(&fx, "Nuevo").expect("fila de Nuevo");
        assert_eq!(fila["TITLE"], "Proyecto nuevo");
        assert_eq!(fila["PATH"], format!("{}\\", dst.display()));
        assert_eq!((fila["MAJOR_VER"].as_str(), fila["MINOR_VER"].as_str()), ("0000", "0000"));
        assert_eq!(fila["DATE"], now.format("%-d %b %Y").to_string());
        assert_eq!(fila["TIME"].len(), 5, "HH:MM: {}", fila["TIME"]);

        // master.ndx existente: reconstruido con las 4 filas (incluidas las borradas, como dBase).
        let mk = std::fs::read(fx.user.join("master.ndx")).unwrap();
        assert_eq!(ndx::read_keys(&mk).unwrap().len(), 4);
    }

    #[test]
    fn title_por_defecto_y_global_del_esquema() {
        let fx = fixture();
        // GLOBAL.CTL vacío en Library_Controls: no sirve de referencia, se usa el del esquema.
        std::fs::write(fx.user.join(LIBRARY_CONTROLS).join("GLOBAL.CTL"), b"").unwrap();
        let otro = vec![9u8; 352];
        std::fs::write(fx.schema.join("GLOBAL.CTL"), &otro).unwrap();
        let s = create_project(&fx.user, &fx.schema, "Otro", "  ", &[]).unwrap();
        assert_eq!(std::fs::read(s.path.join("GLOBAL.CTL")).unwrap(), otro);
        assert_eq!(s.global_ctl_from, fx.schema.join("GLOBAL.CTL"));
        assert!(s.includes.is_empty());
        assert_eq!(Dbf::open(&s.path.join("include.dbf")).unwrap().count(), 0);
        assert_eq!(master_row(&fx, "Otro").unwrap()["TITLE"], "Otro");
        // Título justo del largo del campo TITLE (64): entra.
        let t64 = "T".repeat(64);
        create_project(&fx.user, &fx.schema, "Largo", &t64, &[]).unwrap();
        assert_eq!(master_row(&fx, "Largo").unwrap()["TITLE"], t64);
    }

    /// Todo se valida antes de escribir: si algo falla, no queda nada.
    #[test]
    fn falla_sin_crear_nada() {
        let sin_global = |fx: &Fx| std::fs::remove_file(fx.user.join(LIBRARY_CONTROLS).join("GLOBAL.CTL")).unwrap();
        let sin_include = |fx: &Fx| std::fs::remove_file(fx.schema.join("include.dbf")).unwrap();
        let nada = |_: &Fx| {};
        let largo = "T".repeat(65);
        let casos: [(&str, &dyn Fn(&Fx), &str, &str, &str); 5] = [
            ("sin GLOBAL.CTL de referencia", &sin_global, "Nuevo", "", "GLOBAL.CTL"),
            ("esquema sin include.dbf", &sin_include, "Nuevo", "", "no tiene include.dbf"),
            ("título más largo que TITLE", &nada, "Nuevo", &largo, "admite 64"),
            ("carpeta existente", &nada, "Demo", "", "Ya existe la carpeta"),
            ("fila viva sin carpeta, sin distinguir mayúsculas", &nada, "fantasma", "", "ya está registrado"),
        ];
        for (caso, preparar, name, title, msg) in casos {
            let fx = fixture();
            preparar(&fx);
            let e = format!("{:#}", create_project(&fx.user, &fx.schema, name, title, &[]).unwrap_err());
            assert!(e.contains(msg), "{caso}: {e}");
            if name != "Demo" {
                assert_nothing_created(&fx, name);
            } else {
                assert_eq!(std::fs::read(fx.user.join("MASTER.DBF")).unwrap(), fx.master, "{caso}");
            }
        }
    }

    #[test]
    fn fila_borrada_no_bloquea_el_nombre() {
        let fx = fixture();
        create_project(&fx.user, &fx.schema, "Viejo", "", &[]).unwrap();
        assert!(fx.user.join("Viejo").join(MCP_MARKER).is_file());
        assert!(master_row(&fx, "Viejo").is_some(), "falta la fila viva nueva");
    }

    #[test]
    fn crea_fuera_de_user_con_ruta_completa() {
        let fx = fixture();
        let afuera = fx.root.join("Escritorio");
        // Carpeta inexistente: error sin tocar nada.
        let e = create_project_in(&fx.user, &afuera, &fx.schema, "Planta", "", &[]).unwrap_err();
        assert!(format!("{e:#}").contains("no existe"), "{e:#}");
        assert_nothing_created(&fx, "Planta");

        std::fs::create_dir_all(&afuera).unwrap();
        let s = create_project_in(&fx.user, &afuera, &fx.schema, "Planta", "", &[]).unwrap();
        assert_eq!(s.path, afuera.join("Planta"));
        assert!(s.path.join(MCP_MARKER).is_file());
        assert!(!fx.user.join("Planta").exists());
        assert_eq!(master_row(&fx, "Planta").unwrap()["PATH"], format!("{}\\", afuera.join("Planta").display()));
    }

    #[test]
    fn nombres_validos_y_protegidos() {
        for ok in ["Nuevo", "Prueba_01-B", "MCP_Demo2", &"a".repeat(64)] {
            assert!(validate_name(ok, 64).is_ok(), "{ok}");
        }
        let largo = "a".repeat(64);
        let muy_largo = "a".repeat(65);
        let casos: [(&str, usize, &str); 15] = [
            ("", 64, "Indicá el nombre"),
            (&largo, 10, "máximo es 10"),
            (&muy_largo, 64, "máximo es 64"),
            ("a b", 64, "solo puede tener"),
            ("ñandu", 64, "solo puede tener"),
            ("a/b", 64, "solo puede tener"),
            ("..", 64, "solo puede tener"),
            ("x.y", 64, "solo puede tener"),
            ("System", 64, "sistema o de librería"),
            ("include", 64, "sistema o de librería"),
            ("PLS_Include", 64, "sistema o de librería"),
            ("pso_equipment", 64, "sistema o de librería"),
            ("IndustrialGraphics_Include", 64, "sistema o de librería"),
            ("Library_Controls", 64, "sistema o de librería"),
            ("library_x", 64, "sistema o de librería"),
        ];
        for (mal, max, msg) in casos {
            let e = validate_name(mal, max).unwrap_err().to_string();
            assert!(e.contains(msg), "'{mal}': {e}");
        }
    }

    #[test]
    fn includes_invalidos() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        assert!(validate_includes(&s(&["PLS_Include"]), "Nuevo").is_ok());
        assert!(validate_includes(&s(&[]), "Nuevo").is_ok());
        for (v, msg) in [(s(&["A", "a"]), "repetido"), (s(&["nuevo"]), "a sí mismo"), (s(&[""]), "vacío")] {
            let e = validate_includes(&v, "Nuevo").unwrap_err().to_string();
            assert!(e.contains(msg), "{v:?}: {e}");
        }
    }

    #[test]
    fn unregister_marca_borradas_y_reindexa() {
        let fx = fixture();
        let ndx_antes = std::fs::read(fx.user.join("master.ndx")).unwrap();
        let (gone, bk) = unregister_projects(&fx.user, &["demo".to_string()]).unwrap();
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].name, "Demo");
        assert_eq!(gone[0].path, r"C:\x\User\Demo\");
        assert!(bk.exists(), "sin respaldo");
        assert_eq!(std::fs::read(&bk).unwrap(), fx.master);
        assert!(master_row(&fx, "Demo").is_none(), "Demo sigue viva");
        assert!(master_row(&fx, "Fantasma").is_some(), "Fantasma desapareció");
        assert!(fx.user.join("Demo").exists(), "borró la carpeta");
        let m = Dbf::open(&fx.user.join("MASTER.DBF")).unwrap();
        assert_eq!(m.declared, 3, "cambió la cantidad de registros");
        assert_ne!(std::fs::read(fx.user.join("master.ndx")).unwrap(), ndx_antes, "no reconstruyó master.ndx");
    }

    #[test]
    fn unregister_rechaza_sin_tocar() {
        let fx = fixture();
        for (names, msg) in [
            (vec!["Demo".to_string(), "NoExiste".to_string()], "No están registrados"),
            (vec!["Viejo".to_string()], "No están registrados"),
            (vec!["PLS_Include".to_string()], "sistema o de librería"),
            (vec!["library_Foo".to_string()], "sistema o de librería"),
            (vec![], "al menos un proyecto"),
        ] {
            let e = unregister_projects(&fx.user, &names).unwrap_err().to_string();
            assert!(e.contains(msg), "{names:?}: {e}");
            assert_nothing_created(&fx, "nada");
        }
    }
}
