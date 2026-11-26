//! Búsqueda de la instalación de Power Operation / Plant SCADA en disco (sin registro).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Carpetas de fabricante bajo Program Files / ProgramData donde puede estar la instalación.
const VENDOR_DIRS: &[&str] = &["Schneider Electric", "AVEVA", "Citect", "AVEVA Plant SCADA"];

fn env_dir(k: &str) -> Option<PathBuf> {
    std::env::var_os(k).map(PathBuf::from).filter(|p| p.is_dir())
}

/// Raíces de instalación: `Program Files*\{fabricante}`.
pub fn install_roots() -> Vec<PathBuf> {
    let mut bases: Vec<PathBuf> = ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"].iter().filter_map(|k| env_dir(k)).collect();
    bases.dedup();
    expand_vendors(&bases)
}

/// Raíces de datos: `ProgramData\{fabricante}` y `ProgramData\AVEVA Plant SCADA*`.
pub fn data_roots() -> Vec<PathBuf> {
    let Some(pd) = env_dir("ProgramData").or_else(|| Some(PathBuf::from(r"C:\ProgramData")).filter(|p| p.is_dir())) else {
        return vec![];
    };
    let mut roots = expand_vendors(std::slice::from_ref(&pd));
    if let Ok(rd) = std::fs::read_dir(&pd) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_lowercase();
            if (name.starts_with("aveva plant scada") || name.starts_with("citect")) && e.path().is_dir() && !roots.contains(&e.path()) {
                roots.push(e.path());
            }
        }
    }
    roots
}

fn expand_vendors(bases: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for b in bases {
        for v in VENDOR_DIRS {
            let p = b.join(v);
            if p.is_dir() && !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// Recorre `root` hasta `max_depth` niveles y devuelve los archivos cuyo nombre (en minúsculas) cumple `want`.
/// `max_entries` limita el trabajo en carpetas enormes.
pub fn find_files(root: &Path, max_depth: usize, max_entries: usize, want: &dyn Fn(&str) -> bool) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut visited = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            visited += 1;
            if visited > max_entries {
                return found;
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if depth < max_depth {
                    stack.push((e.path(), depth + 1));
                }
            } else if want(&e.file_name().to_string_lossy().to_lowercase()) {
                found.push(e.path());
            }
        }
    }
    found
}

/// Arquitectura de un DLL/EXE leyendo la cabecera PE.
pub fn pe_arch(path: &Path) -> Option<&'static str> {
    let mut buf = vec![0u8; 4096];
    let n = File::open(path).ok()?.read(&mut buf).ok()?;
    let buf = &buf[..n];
    if buf.len() < 0x40 || &buf[..2] != b"MZ" {
        return None;
    }
    let pe = u32::from_le_bytes(buf[0x3c..0x40].try_into().ok()?) as usize;
    if buf.len() < pe + 6 || &buf[pe..pe + 4] != b"PE\0\0" {
        return None;
    }
    match u16::from_le_bytes([buf[pe + 4], buf[pe + 5]]) {
        0x8664 => Some("x64"),
        0x014c => Some("x86"),
        0xaa64 => Some("arm64"),
        _ => Some("desconocida"),
    }
}

pub fn this_arch() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "x86") {
        "x86"
    } else {
        "arm64"
    }
}

/// Un `citect.ini` encontrado, con las claves relevantes.
#[derive(Debug, Clone)]
pub struct CitectIni {
    pub path: PathBuf,
    /// sección -> clave -> valor (claves en minúsculas).
    pub sections: BTreeMap<String, BTreeMap<String, String>>,
}

impl CitectIni {
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.sections.get(&section.to_lowercase())?.get(&key.to_lowercase()).map(String::as_str)
    }
}

pub fn parse_ini(path: &Path) -> Option<CitectIni> {
    let text = crate::text::decode(&std::fs::read(path).ok()?);
    let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut cur = String::new();
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with(';') || l.is_empty() {
            continue;
        }
        if l.starts_with('[') && l.ends_with(']') {
            cur = l[1..l.len() - 1].trim().to_lowercase();
        } else if let Some((k, v)) = l.split_once('=') {
            sections.entry(cur.clone()).or_default().insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    Some(CitectIni { path: path.to_path_buf(), sections })
}

/// Todos los `citect.ini` bajo ProgramData (configuración de cada versión instalada).
pub fn find_citect_ini() -> Vec<CitectIni> {
    let mut out = Vec::new();
    for root in data_roots() {
        for p in find_files(&root, 4, 50_000, &|n| n == "citect.ini") {
            if let Some(ini) = parse_ini(&p) {
                out.push(ini);
            }
        }
    }
    out
}
