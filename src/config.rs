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
    /// Vacíos = se toman del proyecto (tablas almsrv/netaddr) o localhost:5482.
    pub alarm_host: Option<String>,
    pub alarm_port: Option<u16>,
    /// Archivo SystemsXml propio (si no se indica, se genera uno con host/puerto).
    pub alarm_systems_xml: Option<PathBuf>,
    /// Log de auditoría de escrituras.
    pub audit_log: PathBuf,
    /// PLCs Modbus TCP para po_plc_read (solo lectura). Vacío = puente MODBUS no configurado.
    pub modbus: Vec<ModbusTarget>,
    /// Segundos entre sondeos de los puentes CTAPI, ODBC y MODBUS (0 = solo al arrancar y con po_status).
    pub probe_secs: u64,
    /// Páginas de proyectos protegidos (librerías de PO como PLS_Include) que po_gb_page_edit puede modificar
    /// (`POWEROP_GB_ALLOW_PAGES=Proyecto/Página;...`). Vacío = ninguna.
    pub gb_allow_pages: Vec<(String, String)>,
}

/// Lee `POWEROP_GB_ALLOW_PAGES`: entradas `Proyecto/Página` (o `Proyecto\Página`) separadas por `;`.
/// Solo páginas exactas: sin comodines, para que habilitar una página no abra toda la librería.
pub fn parse_page_list(v: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for item in v.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let Some((proj, page)) = item.split_once(['/', '\\']) else {
            return Err(format!("'{item}': falta el proyecto (usá Proyecto/Página)"));
        };
        let (proj, page) = (proj.trim(), page.trim());
        if proj.is_empty() || page.is_empty() || page.contains(['/', '\\', '*', '?']) {
            return Err(format!("'{item}': tiene que ser Proyecto/Página con una página exacta (sin comodines)"));
        }
        out.push((proj.to_string(), page.to_string()));
    }
    Ok(out)
}

/// Un PLC Modbus TCP de `POWEROP_MODBUS` (`nombre=host:puerto/unidad`; nombre, puerto y unidad opcionales).
#[derive(Debug, Clone, PartialEq)]
pub struct ModbusTarget {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub unit: u8,
}

impl std::fmt::Display for ModbusTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}={}:{}/{}", self.name, self.host, self.port, self.unit)
    }
}

/// Lee `POWEROP_MODBUS`: entradas separadas por `;` con la forma `[nombre=]host[:puerto][/unidad]`
/// (puerto 502 y unidad 255 por defecto; sin nombre se usa `PLC1`, `PLC2`...).
pub fn parse_modbus(v: &str) -> Result<Vec<ModbusTarget>, String> {
    let mut out: Vec<ModbusTarget> = Vec::new();
    for (i, item) in v.split(';').map(str::trim).filter(|s| !s.is_empty()).enumerate() {
        let (name, rest) = match item.split_once('=') {
            Some((n, r)) => (n.trim().to_string(), r.trim()),
            None => (format!("PLC{}", i + 1), item),
        };
        let (addr, unit) = match rest.split_once('/') {
            Some((a, u)) => (a, u.trim().parse::<u8>().map_err(|_| format!("unidad inválida en '{item}'"))?),
            None => (rest, 255),
        };
        let (host, port) = match addr.rsplit_once(':') {
            Some((h, p)) => (h.trim().to_string(), p.trim().parse::<u16>().map_err(|_| format!("puerto inválido en '{item}'"))?),
            None => (addr.trim().to_string(), 502),
        };
        if host.is_empty() || name.is_empty() {
            return Err(format!("entrada inválida '{item}' (forma: nombre=host:puerto/unidad)"));
        }
        if out.iter().any(|t| t.name.eq_ignore_ascii_case(&name)) {
            return Err(format!("nombre de PLC repetido: {name}"));
        }
        out.push(ModbusTarget { name, host, port, unit });
    }
    Ok(out)
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
            alarm_host: var("POWEROP_ALARM_HOST"),
            alarm_port: var("POWEROP_ALARM_PORT").and_then(|v| v.parse().ok()),
            alarm_systems_xml: var("POWEROP_ALARM_SYSTEMS_XML").map(PathBuf::from),
            audit_log,
            modbus: var("POWEROP_MODBUS")
                .map(|v| {
                    parse_modbus(&v).unwrap_or_else(|e| {
                        tracing::warn!("POWEROP_MODBUS ignorado: {e}");
                        vec![]
                    })
                })
                .unwrap_or_default(),
            probe_secs: var("POWEROP_PROBE_SECS").and_then(|v| v.parse().ok()).unwrap_or(30),
            gb_allow_pages: var("POWEROP_GB_ALLOW_PAGES")
                .map(|v| {
                    parse_page_list(&v).unwrap_or_else(|e| {
                        tracing::warn!("POWEROP_GB_ALLOW_PAGES ignorado: {e}");
                        vec![]
                    })
                })
                .unwrap_or_default(),
        }
    }

    /// Resumen sin secretos, para diagnóstico.
    pub fn describe(&self) -> String {
        let opt = |o: &Option<String>| o.clone().unwrap_or_else(|| "(no)".into());
        format!(
            "POWEROP_CTAPI={}\nPOWEROP_HOST={}\nPOWEROP_USER={}\nPOWEROP_PASSWORD={}\nPOWEROP_CLUSTER={}\n\
             POWEROP_PROJECTS_DIR={}\nPOWEROP_PROJECT={}\nPOWEROP_ALLOW_WRITE={}\nPOWEROP_TIMEOUT_SECS={}\n\
             POWEROP_ALARM_HOST={}\nPOWEROP_ALARM_PORT={}\nPOWEROP_ALARM_SYSTEMS_XML={}\nPOWEROP_AUDIT_LOG={}\n\
             POWEROP_MODBUS={}\nPOWEROP_PROBE_SECS={}\nPOWEROP_GB_ALLOW_PAGES={}",
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
            self.alarm_host.clone().unwrap_or_else(|| "(auto: proyecto o localhost)".into()),
            self.alarm_port.map(|p| p.to_string()).unwrap_or_else(|| "(auto: proyecto o 5482)".into()),
            self.alarm_systems_xml.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(auto)".into()),
            self.audit_log.display(),
            if self.modbus.is_empty() {
                "(no)".into()
            } else {
                self.modbus.iter().map(ToString::to_string).collect::<Vec<_>>().join(";")
            },
            self.probe_secs,
            if self.gb_allow_pages.is_empty() {
                "(no)".into()
            } else {
                self.gb_allow_pages.iter().map(|(p, g)| format!("{p}/{g}")).collect::<Vec<_>>().join(";")
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modbus_por_defecto_y_completo() {
        let t = parse_modbus("192.0.2.10").unwrap();
        assert_eq!(t, vec![ModbusTarget { name: "PLC1".into(), host: "192.0.2.10".into(), port: 502, unit: 255 }]);
        let t = parse_modbus("PLC1=192.0.2.10:5020/1; 192.0.2.11").unwrap();
        assert_eq!(t[0], ModbusTarget { name: "PLC1".into(), host: "192.0.2.10".into(), port: 5020, unit: 1 });
        assert_eq!(t[1].name, "PLC2");
        assert!(parse_modbus("A=x:99999").is_err());
        assert!(parse_modbus("A=x;A=y").is_err());
    }

    #[test]
    fn page_list() {
        let t = parse_page_list(" PLS_Include/!equipment1 ; Lib\\Pag2;").unwrap();
        assert_eq!(t, vec![("PLS_Include".into(), "!equipment1".into()), ("Lib".into(), "Pag2".into())]);
        assert!(parse_page_list("sin_proyecto").is_err());
        assert!(parse_page_list("PLS_Include/*").is_err());
        assert!(parse_page_list("PLS_Include/").is_err());
        assert!(parse_page_list("A/b/c").is_err());
        assert!(parse_page_list("").unwrap().is_empty());
    }
}
