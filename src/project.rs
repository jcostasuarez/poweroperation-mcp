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
    let mut roots: Vec<PathBuf> = cfg.projects_dirs.clone();
    let mut running: HashSet<String> = HashSet::new();
    for ini in &inis {
        if let Some(u) = ini.get("CtEdit", "User").filter(|u| !u.is_empty()) {
            roots.push(PathBuf::from(u));
        }
        if let Some(r) = ini.get("CtEdit", "Run").filter(|r| !r.is_empty()) {
            running.insert(norm(Path::new(r)));
            roots.push(PathBuf::from(r));
        }
    }
    roots.extend(discover::data_roots());

    let mut seen = HashSet::new();
    let mut projects = Vec::new();
    let mut add = |dir: &Path, projects: &mut Vec<Project>| {
        let key = norm(dir);
        if seen.insert(key.clone()) {
            projects.push(Project {
                name: dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                path: dir.to_path_buf(),
                running: running.contains(&key),
            });
        }
    };
    for root in &roots {
        if !root.is_dir() {
            continue;
        }
        if is_project_dir(root) {
            add(root, &mut projects);
            continue;
        }
        for f in discover::find_files(root, 6, 300_000, &|n| n == "variable.dbf" || n == "include.dbf") {
            if let Some(dir) = f.parent() {
                add(dir, &mut projects);
            }
        }
    }
    projects.sort_by(|a, b| b.running.cmp(&a.running).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Catalog { projects, inis, ignored_ini_copies: copies.len(), roots }
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
