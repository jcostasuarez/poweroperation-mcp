//! Utilidades de los tests unitarios: carpeta temporal que se borra sola y `Config` sin leer el entorno.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::config::Config;

/// Carpeta temporal única por test (proceso + etiqueta + contador); se borra al salir de alcance, aunque el test falle.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!("pomcp-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, p: impl AsRef<Path>) -> PathBuf {
        self.0.join(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `Config` por defecto (sin `.env` ni variables POWEROP_*): solo lectura, sin CtAPI ni servidor de alarmas fijo.
pub fn config(audit_dir: &Path) -> Config {
    Config {
        ctapi_path: None,
        host: None,
        user: None,
        password: None,
        cluster: None,
        projects_dirs: vec![],
        default_project: None,
        allow_write: false,
        timeout: std::time::Duration::from_secs(5),
        alarm_host: None,
        alarm_port: None,
        alarm_systems_xml: None,
        audit_log: audit_dir.join("audit.log"),
        modbus: vec![],
        probe_secs: 0,
    }
}
