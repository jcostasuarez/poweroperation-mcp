use std::path::PathBuf;
use std::time::Duration;

/// Configuración leída de variables de entorno (y del archivo `.env` junto al ejecutable).
#[derive(Debug, Clone)]
pub struct Config {
    /// Ruta a `CtApi.dll` o a la carpeta que la contiene. Vacío = búsqueda automática.
    pub ctapi_path: Option<PathBuf>,
    /// Equipo SCADA para CtAPI. Vacío = conexión local.
    pub host: Option<String>,
    /// Usuario/contraseña del proyecto (obligatorios para conexión remota).
    pub user: Option<String>,
    pub password: Option<String>,
    /// Cluster por defecto para búsquedas, tendencias y alarmas.
    pub cluster: Option<String>,
    /// Carpetas `User` (o carpetas de proyecto) adicionales donde buscar proyectos.
    pub projects_dirs: Vec<PathBuf>,
    /// Proyecto por defecto cuando una herramienta no recibe `project`.
    pub default_project: Option<String>,
    /// Permite escrituras (tags, Cicode arbitrario, reconocer alarmas, archivos .ci).
    pub allow_write: bool,
    /// Tiempo máximo por llamada a CtAPI / ODBC.
    pub timeout: Duration,
    /// Dirección y puerto del servidor de alarmas (driver ODBC "Citect Alarm Driver").
    pub alarm_host: String,
    pub alarm_port: u16,
    /// Archivo SystemsXml propio (si no se indica, se genera uno con host/puerto).
    pub alarm_systems_xml: Option<PathBuf>,
    /// Log de auditoría de escrituras.
    pub audit_log: PathBuf,
}

impl Config {
    pub fn load() -> Self {
        let _ = dotenvy::dotenv();
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.to_path_buf()));
        if let Some(dir) = &exe_dir {
            // Junto al exe y en sus padres (para target\release durante el desarrollo).
            let mut d = Some(dir.clone());
            for _ in 0..4 {
                let Some(cur) = d else { break };
                let candidate = cur.join(".env");
                if candidate.is_file() {
                    let _ = dotenvy::from_path(&candidate);
                    break;
                }
                d = cur.parent().map(|p| p.to_path_buf());
            }
        }

        let var = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let flag = |k: &str| var(k).is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("si"));

        let audit_log = var("POWEROP_AUDIT_LOG").map(PathBuf::from).unwrap_or_else(|| {
            exe_dir.clone().unwrap_or_else(|| PathBuf::from(".")).join("poweroperation-mcp-audit.log")
        });

        Self {
            ctapi_path: var("POWEROP_CTAPI").map(PathBuf::from),
            host: var("POWEROP_HOST"),
            user: var("POWEROP_USER"),
            password: var("POWEROP_PASSWORD"),
            cluster: var("POWEROP_CLUSTER"),
            projects_dirs: var("POWEROP_PROJECTS_DIR")
                .map(|v| v.split(';').map(str::trim).filter(|s| !s.is_empty()).map(PathBuf::from).collect())
                .unwrap_or_default(),
            default_project: var("POWEROP_PROJECT"),
            allow_write: flag("POWEROP_ALLOW_WRITE"),
            timeout: Duration::from_secs(var("POWEROP_TIMEOUT_SECS").and_then(|v| v.parse().ok()).unwrap_or(30)),
            alarm_host: var("POWEROP_ALARM_HOST").unwrap_or_else(|| "127.0.0.1".into()),
            alarm_port: var("POWEROP_ALARM_PORT").and_then(|v| v.parse().ok()).unwrap_or(5482),
            alarm_systems_xml: var("POWEROP_ALARM_SYSTEMS_XML").map(PathBuf::from),
            audit_log,
        }
    }

    /// Resumen sin secretos, para diagnóstico.
    pub fn describe(&self) -> String {
        let opt = |o: &Option<String>| o.clone().unwrap_or_else(|| "(no)".into());
        format!(
            "POWEROP_CTAPI={}\nPOWEROP_HOST={}\nPOWEROP_USER={}\nPOWEROP_PASSWORD={}\nPOWEROP_CLUSTER={}\n\
             POWEROP_PROJECTS_DIR={}\nPOWEROP_PROJECT={}\nPOWEROP_ALLOW_WRITE={}\nPOWEROP_TIMEOUT_SECS={}\n\
             POWEROP_ALARM_HOST={}:{}\nPOWEROP_ALARM_SYSTEMS_XML={}\nPOWEROP_AUDIT_LOG={}",
            self.ctapi_path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(auto)".into()),
            self.host.clone().unwrap_or_else(|| "(local)".into()),
            opt(&self.user),
            if self.password.is_some() { "(definida)" } else { "(no)" },
            opt(&self.cluster),
            if self.projects_dirs.is_empty() {
                "(auto)".into()
            } else {
                self.projects_dirs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(";")
            },
            opt(&self.default_project),
            if self.allow_write { "1 (escrituras habilitadas)" } else { "0 (solo lectura)" },
            self.timeout.as_secs(),
            self.alarm_host,
            self.alarm_port,
            self.alarm_systems_xml.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(auto)".into()),
            self.audit_log.display(),
        )
    }
}
