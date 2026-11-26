//! Proyectos Power Operation en disco: descubrimiento, proyectos incluidos, tablas DBF y archivos Cicode.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::Serialize;

use crate::config::Config;
use crate::dbf::Dbf;
use crate::discover::{self, CitectIni};

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    /// Es el proyecto indicado en `[CtEdit] Run=` de algún citect.ini (el que corre en runtime).
    pub running: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub projects: Vec<Project>,
    pub inis: Vec<CitectIni>,
    /// citect.ini encontrados dentro de carpetas de proyecto (ignorados).
    pub ignored_ini_copies: usize,
    pub roots: Vec<PathBuf>,
    /// Carpetas User con `MASTER.DBF` (el registro de proyectos de Citect).
    pub masters: Vec<PathBuf>,
    /// Carpetas con forma de proyecto dentro de una carpeta User que no están registradas en su
    /// `MASTER.DBF` (borradas del registro, o copias): no se listan como proyectos.
    pub unregistered: Vec<PathBuf>,
}

/// Carpeta de respaldos que crean las herramientas de escritura dentro del proyecto: nunca es un proyecto.
const BACKUP_DIR: &str = "_mcp_backup";

/// Proyectos registrados en `MASTER.DBF` (sin los registros borrados): (NAME, carpeta).
/// PATH viene con `\` final; si falta, se toma `user_dir\NAME`.
pub fn master_projects(user_dir: &Path) -> Option<Vec<(String, PathBuf)>> {
    let dbf = Dbf::open(&find_file(user_dir, "master.dbf")?).ok()?;
    let (name, path) = (dbf.field_index("NAME")?, dbf.field_index("PATH"));
    let mut out = Vec::new();
    dbf.for_each(|_, v| {
        let n = v[name].trim();
        if !n.is_empty() {
            let p = path.map(|i| v[i].trim().trim_end_matches(['\\', '/'])).filter(|p| !p.is_empty());
            out.push((n.to_string(), p.map(PathBuf::from).unwrap_or_else(|| user_dir.join(n))));
        }
        true
    });
    Some(out)
}

fn is_project_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten().any(|e| {
                let n = e.file_name().to_string_lossy().to_lowercase();
                n == "variable.dbf" || n == "include.dbf"
            })
        })
        .unwrap_or(false)
}

fn norm(p: &Path) -> String {
    p.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase().replace('/', "\\")
}

/// Busca proyectos en las carpetas configuradas, en las de citect.ini y bajo ProgramData.
pub fn discover(cfg: &Config) -> Catalog {
    // Los citect.ini guardados dentro de carpetas de proyecto son copias (backups/restauraciones) y suelen
    // apuntar a otros proyectos o versiones: solo cuentan los de configuración (p. ej. ...\v2024\Config).
    let (inis, copies): (Vec<CitectIni>, Vec<CitectIni>) =
        discover::find_citect_ini().into_iter().partition(|i| !i.path.parent().is_some_and(is_project_dir));
    discover_from(cfg, inis, copies.len(), discover::data_roots())
}

/// Catálogo a partir de los citect.ini y las raíces de datos dadas (`discover` lo llama con los del equipo).
fn discover_from(cfg: &Config, inis: Vec<CitectIni>, ignored_ini_copies: usize, data_roots: Vec<PathBuf>) -> Catalog {
    let mut roots: Vec<PathBuf> = cfg.projects_dirs.clone();
    let mut running: HashSet<String> = HashSet::new();
    let mut run_dirs: Vec<PathBuf> = Vec::new();
    for ini in &inis {
        if let Some(u) = ini.get("CtEdit", "User").filter(|u| !u.is_empty()) {
            roots.push(PathBuf::from(u));
        }
        if let Some(r) = ini.get("CtEdit", "Run").filter(|r| !r.is_empty()) {
            running.insert(norm(Path::new(r)));
            roots.push(PathBuf::from(r));
            run_dirs.push(PathBuf::from(r));
        }
    }
    roots.extend(data_roots);
    let (mut projects, masters, mut unregistered) = collect(&roots, &running);

    // La carpeta de `[CtEdit] Run=` SIEMPRE entra al catálogo, esté o no en MASTER.DBF y esté donde esté
    // (p. ej. Desktop\prueba\...). Cuenta como proyecto si existe y tiene algún .dbf; sin duplicados.
    for dir in run_dirs.iter().filter(|d| d.is_absolute() && d.is_dir() && !files_with_ext(d, "dbf").is_empty()) {
        let key = norm(dir);
        unregistered.retain(|u| norm(u) != key);
        if !projects.iter().any(|p| norm(&p.path) == key) {
            let name = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            projects.push(Project { name, path: dir.clone(), running: true });
        }
    }
    for p in projects.iter_mut().filter(|p| running.contains(&norm(&p.path))) {
        p.running = true;
    }
    projects.sort_by(|a, b| b.running.cmp(&a.running).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));

    Catalog { projects, inis, ignored_ini_copies, roots, masters, unregistered }
}

/// Proyectos bajo `roots`. Una carpeta con `MASTER.DBF` es una carpeta User y su registro manda: solo
/// cuentan sus proyectos no borrados (así no aparecen proyectos eliminados ni copias de respaldo).
/// Fuera de ellas, cualquier carpeta con variable.dbf o include.dbf es un proyecto.
fn collect(roots: &[PathBuf], running: &HashSet<String>) -> (Vec<Project>, Vec<PathBuf>, Vec<PathBuf>) {
    let mut found: Vec<(Option<String>, PathBuf)> = Vec::new();
    let mut masters: Vec<PathBuf> = Vec::new();
    let mut candidates: Vec<PathBuf> = Vec::new();
    for root in roots.iter().filter(|r| r.is_dir()) {
        if is_project_dir(root) {
            found.push((None, root.clone()));
            continue;
        }
        let want = |n: &str| n == "variable.dbf" || n == "include.dbf" || n == "master.dbf";
        for f in discover::find_files(root, 6, 300_000, &want) {
            let Some(dir) = f.parent() else { continue };
            if f.file_name().is_some_and(|n| n.eq_ignore_ascii_case("master.dbf")) {
                masters.push(dir.to_path_buf());
            } else {
                candidates.push(dir.to_path_buf());
            }
        }
    }
    masters.sort_by_key(|m| norm(m));
    masters.dedup_by_key(|m| norm(m));

    let mut registered: HashSet<String> = HashSet::new();
    for m in &masters {
        for (name, dir) in master_projects(m).unwrap_or_default() {
            registered.insert(norm(&dir));
            if dir.is_dir() {
                found.push((Some(name), dir));
            }
        }
    }
    let master_keys: Vec<String> = masters.iter().map(|m| format!("{}\\", norm(m))).collect();
    let mut unregistered = Vec::new();
    for dir in candidates {
        let key = norm(&dir);
        if registered.contains(&key) || key.split('\\').any(|c| c.eq_ignore_ascii_case(BACKUP_DIR)) {
            continue;
        }
        if master_keys.iter().any(|m| key.starts_with(m.as_str())) {
            unregistered.push(dir);
        } else {
            found.push((None, dir));
        }
    }
    unregistered.sort_by_key(|d| norm(d));
    unregistered.dedup_by_key(|d| norm(d));

    let mut seen = HashSet::new();
    let mut projects = Vec::new();
    for (name, dir) in found {
        let key = norm(&dir);
        if seen.insert(key.clone()) {
            projects.push(Project {
                name: name.unwrap_or_else(|| dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()),
                running: running.contains(&key),
                path: dir,
            });
        }
    }
    projects.sort_by(|a, b| b.running.cmp(&a.running).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    (projects, masters, unregistered)
}

impl Catalog {
    /// Proyecto por nombre (sin distinguir mayúsculas) o por ruta completa.
    pub fn resolve(&self, name: &str) -> Result<Project> {
        let p = Path::new(name);
        if p.is_absolute() {
            if is_project_dir(p) {
                return Ok(Project { name: p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), path: p.into(), running: false });
            }
            bail!("{name} no parece una carpeta de proyecto (no tiene variable.dbf ni include.dbf)");
        }
        let mut matches: Vec<&Project> = self.projects.iter().filter(|p| p.name.eq_ignore_ascii_case(name)).collect();
        matches.sort_by_key(|p| !p.running);
        match matches.first() {
            Some(p) => Ok((*p).clone()),
            None => bail!(
                "No se encontró el proyecto '{name}'. Proyectos encontrados: {}",
                self.projects.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    /// El proyecto + sus incluidos (recursivo, sin repetir), en orden de búsqueda.
    pub fn with_includes(&self, root: &Project) -> (Vec<Project>, Vec<String>) {
        let mut out = vec![root.clone()];
        let mut missing = Vec::new();
        let mut seen: HashSet<String> = HashSet::from([norm(&root.path)]);
        let mut i = 0;
        while i < out.len() && out.len() < 200 {
            for inc in included_names(&out[i].path) {
                // Preferir el incluido de la misma carpeta User que el proyecto que lo incluye.
                let parent = out[i].path.parent().map(norm);
                let mut cands: Vec<&Project> = self.projects.iter().filter(|p| p.name.eq_ignore_ascii_case(&inc)).collect();
                cands.sort_by_key(|p| p.path.parent().map(norm) != parent);
                match cands.first() {
                    Some(p) if seen.insert(norm(&p.path)) => out.push((*p).clone()),
                    Some(_) => {}
                    None => missing.push(inc),
                }
            }
            i += 1;
        }
        // Citect incluye siempre el proyecto de sistema `Include` (librerías, estilos y páginas base).
        if let Some(sys) = self.projects.iter().find(|p| p.name.eq_ignore_ascii_case("Include"))
            && seen.insert(norm(&sys.path))
        {
            out.push(sys.clone());
        }
        (out, missing)
    }
}

/// Nombres de proyectos incluidos (include.dbf).
pub fn included_names(dir: &Path) -> Vec<String> {
    let Some(path) = find_file(dir, "include.dbf") else { return vec![] };
    let Ok(dbf) = Dbf::open(&path) else { return vec![] };
    let idx = dbf.field_index("NAME").unwrap_or(0);
    let mut names = Vec::new();
    dbf.for_each(|_, v| {
        if let Some(n) = v.get(idx).filter(|n| !n.is_empty()) {
            names.push(n.clone());
        }
        true
    });
    names
}

/// Archivo en `dir` sin distinguir mayúsculas.
pub fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| {
        p.is_file() && p.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(name))
    })
}

/// Archivos de `dir` con la extensión dada (sin punto, sin distinguir mayúsculas), ordenados.
pub fn files_with_ext(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.to_string_lossy().eq_ignore_ascii_case(ext)))
                .collect()
        })
        .unwrap_or_default();
    v.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    v
}

/// Tabla DBF de un proyecto: acepta "variable", "Variable.DBF", etc.
pub fn table_path(dir: &Path, table: &str) -> Option<PathBuf> {
    let t = table.trim();
    if t.contains(['\\', '/']) || t.contains("..") {
        return None;
    }
    let name = if t.to_lowercase().ends_with(".dbf") { t.to_string() } else { format!("{t}.dbf") };
    find_file(dir, &name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbf::tests::build;
    use crate::testutil::TempDir;

    fn touch_project(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("variable.dbf"), build(&[("TAG", 16)], &[])).unwrap();
    }

    #[test]
    fn master_dbf_is_authoritative() {
        let tmp = TempDir::new("master");
        let user = tmp.join("User");
        let other = tmp.join("Otros").join("Suelto");
        for p in ["Demo", "Lib", "Borrado", "SinRegistro"] {
            touch_project(&user.join(p));
        }
        touch_project(&user.join("Demo").join(BACKUP_DIR).join("20261014-164138"));
        touch_project(&other);
        let lib = format!("{}\\", user.join("Lib").display());
        std::fs::write(
            user.join("MASTER.DBF"),
            build(
                &[("NAME", 64), ("PATH", 240)],
                &[(&["Demo", ""], false), (&["Lib", lib.as_str()], false), (&["Borrado", ""], true), (&["NoExiste", ""], false)],
            ),
        )
        .unwrap();

        let running = HashSet::from([norm(&user.join("Demo"))]);
        let (projects, masters, unregistered) = collect(&[user.clone(), tmp.join("Otros")], &running);
        let names: Vec<&str> = projects.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Demo", "Lib", "Suelto"]);
        assert!(projects[0].running);
        assert_eq!(masters.len(), 1);
        let unreg: Vec<String> = unregistered.iter().map(|d| d.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(unreg, ["Borrado", "SinRegistro"]);
    }

    #[test]
    fn without_master_skips_backups() {
        let tmp = TempDir::new("nomaster");
        touch_project(&tmp.join("A"));
        touch_project(&tmp.join("A").join(BACKUP_DIR).join("x"));
        let (projects, masters, unregistered) = collect(&[tmp.path().to_path_buf()], &HashSet::new());
        assert_eq!(projects.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["A"]);
        assert!(masters.is_empty() && unregistered.is_empty());
    }

    /// citect.ini temporal con `[CtEdit]` (Run= y User=) leído con el parser real.
    fn temp_ini(dir: &Path, name: &str, run: &Path, user: &Path) -> CitectIni {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, format!("[CtEdit]\r\nRun = {}\r\nUser = {}\r\n", run.display(), user.display())).unwrap();
        discover::parse_ini(&path).unwrap()
    }

    #[test]
    fn run_folder_is_always_listed() {
        let tmp = TempDir::new("run");
        let user = tmp.join("User");
        touch_project(&user.join("Demo"));
        std::fs::write(user.join("MASTER.DBF"), build(&[("NAME", 64), ("PATH", 240)], &[(&["Demo", ""], false)])).unwrap();
        // Proyecto fuera de User: solo tiene un .dbf (sin variable.dbf), así que collect() no lo encuentra solo.
        let outside = tmp.join("Desktop").join("prueba").join("Transferencia_Demo");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("errlog.dbf"), build(&[("E_TYPE", 8)], &[])).unwrap();
        // Carpeta de Run= sin ningún .dbf: no es proyecto.
        let vacia = tmp.join("Vacia");
        std::fs::create_dir_all(&vacia).unwrap();

        let inis = vec![
            temp_ini(&tmp.join("ini1"), "citect.ini", &outside, &user),
            temp_ini(&tmp.join("ini2"), "citect.ini", &user.join("Demo"), &user),
            temp_ini(&tmp.join("ini3"), "citect.ini", &vacia, &user),
        ];
        let cat = discover_from(&crate::testutil::config(tmp.path()), inis, 0, vec![]);

        let trans = cat.projects.iter().filter(|p| p.path == outside).collect::<Vec<_>>();
        assert_eq!(trans.len(), 1, "{:?}", cat.projects);
        assert!(trans[0].running && trans[0].name == "Transferencia_Demo");
        let demo: Vec<&Project> = cat.projects.iter().filter(|p| p.name == "Demo").collect();
        assert_eq!(demo.len(), 1, "sin duplicados: {:?}", cat.projects);
        assert!(demo[0].running);
        assert!(!cat.projects.iter().any(|p| p.path == vacia));
        assert!(cat.projects[0].running, "los proyectos en runtime van primero");
        assert!(cat.unregistered.is_empty());
    }
}
