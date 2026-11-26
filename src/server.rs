use std::io::Write as _;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, tool_handler};

use crate::config::Config;
use crate::ctapi::CtApi;
use crate::project::{self, Catalog, Project};

/// Resultado de una herramienta: `Ok` con texto para el agente, `Err` con el mensaje de error
/// (rmcp lo marca como `isError` para que el agente lo vea y pueda corregir).
pub type ToolResult = Result<String, String>;

pub fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

#[derive(Clone)]
pub struct PowerOpServer {
    pub cfg: Arc<Config>,
    pub ct: CtApi,
    catalog: Arc<tokio::sync::Mutex<Option<Arc<Catalog>>>>,
    /// Proyecto en ejecución según el runtime: None = sin consultar; Some(None) = no disponible.
    runtime_project: Arc<tokio::sync::Mutex<Option<Option<String>>>>,
    tool_router: ToolRouter<Self>,
}

/// Ejecuta trabajo bloqueante (disco, ODBC) fuera del runtime async, con tiempo máximo.
pub async fn blocking<T, F>(timeout: std::time::Duration, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    match tokio::time::timeout(timeout, tokio::task::spawn_blocking(f)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => Err(anyhow!("Error interno: {e}")),
        Err(_) => Err(anyhow!("Tiempo agotado ({} s)", timeout.as_secs())),
    }
}

impl PowerOpServer {
    pub fn new(cfg: Config) -> Self {
        let cfg = Arc::new(cfg);
        Self {
            ct: CtApi::new(cfg.clone()),
            cfg,
            catalog: Arc::new(tokio::sync::Mutex::new(None)),
            runtime_project: Arc::new(tokio::sync::Mutex::new(None)),
            tool_router: Self::runtime_router() + Self::project_router() + Self::reference_router(),
        }
    }

    /// Catálogo de proyectos (se busca una vez; `refresh` fuerza a buscar de nuevo).
    pub async fn catalog(&self, refresh: bool) -> Result<Arc<Catalog>> {
        let mut guard = self.catalog.lock().await;
        if refresh || guard.is_none() {
            let cfg = self.cfg.clone();
            // Recorrer ProgramData puede tardar en equipos con mucho contenido: margen amplio.
            let cat = blocking(std::time::Duration::from_secs(120), move || Ok(project::discover(&cfg))).await?;
            *guard = Some(Arc::new(cat));
        }
        Ok(guard.clone().expect("cargado arriba"))
    }

    /// Nombre del proyecto que corre en el runtime (ver `Session::running_project`). Se consulta una vez;
    /// `refresh` vuelve a preguntar. En PO 2024 el `[CtEdit] Run=` del citect.ini puede estar vacío.
    pub async fn runtime_project(&self, refresh: bool) -> Option<String> {
        let mut g = self.runtime_project.lock().await;
        if refresh || g.is_none() {
            *g = Some(self.ct.call(|s| Ok(s.running_project())).await.ok().flatten());
        }
        g.clone().flatten()
    }

    /// Proyecto pedido, o el por defecto (POWEROP_PROJECT, el que corre en runtime, o el único).
    pub async fn project(&self, name: Option<&str>) -> Result<Project> {
        let cat = self.catalog(false).await?;
        if let Some(n) = name.map(str::trim).filter(|n| !n.is_empty()) {
            return cat.resolve(n);
        }
        if let Some(n) = &self.cfg.default_project {
            return cat.resolve(n);
        }
        if let Some(rp) = self.runtime_project(false).await
            && let Ok(p) = cat.resolve(&rp)
        {
            return Ok(p);
        }
        if let Some(p) = cat.projects.iter().find(|p| p.running) {
            return Ok(p.clone());
        }
        match cat.projects.as_slice() {
            [only] => Ok(only.clone()),
            [] => bail!("No se encontraron proyectos. Indicá la carpeta User con POWEROP_PROJECTS_DIR."),
            many => bail!(
                "Indicá 'project'. Proyectos encontrados: {}",
                many.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    /// Falla si el servidor está en modo solo lectura.
    pub fn require_write(&self, what: &str) -> Result<()> {
        if !self.cfg.allow_write {
            bail!(
                "{what} requiere escrituras habilitadas. El servidor está en modo solo lectura: \
                 el usuario debe arrancarlo con POWEROP_ALLOW_WRITE=1 si quiere permitirlo."
            );
        }
        Ok(())
    }

    /// Registra una operación de escritura en el log de auditoría (no falla).
    pub fn audit(&self, tool: &str, detail: &str, outcome: &str) {
        let line = format!(
            "{}\t{tool}\t{}\t{}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
            detail.replace(['\n', '\r', '\t'], " "),
            outcome.replace(['\n', '\r', '\t'], " ")
        );
        tracing::info!(target: "audit", "{}", line.trim_end());
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.cfg.audit_log) {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

const INSTRUCTIONS: &str = "\
Servidor MCP para EcoStruxure Power Operation (basado en AVEVA Plant SCADA / Citect).

Dos fuentes de datos:
- RUNTIME (CtAPI, requiere el runtime de Power Operation corriendo): tag_read, tag_browse, trend_query,
  alarm_list, alarm_sql (ODBC del servidor de alarmas), cicode_run, ctapi_find. Escrituras: tag_write, alarm_ack.
- PROYECTO OFFLINE (archivos .dbf/.ci en disco, no necesita runtime): project_list, project_tables,
  project_query, project_search, cicode_files, cicode_read, cicode_write, cicode_check.

Flujo recomendado:
1. `runtime_status` y `project_list` para orientarte (DLL encontrado, conexión, proyectos, cuál corre).
2. Lee `poweroperation_reference` antes de escribir Cicode o consultar tablas: nombres de tablas DBF, campos,
   sintaxis Cicode, filtros de CtAPI y consultas SQL de alarmas.
3. Para saber qué tags existen: offline con project_query (tabla variable) o en vivo con tag_browse.
4. Power Operation es un SCADA eléctrico: escribir tags u operar interruptores actúa sobre equipos reales.
   Confirmá SIEMPRE con el usuario antes de tag_write, alarm_ack, cicode_run con efectos o cicode_write.
   Por defecto el servidor está en solo lectura (POWEROP_ALLOW_WRITE=1 para habilitar). Todo se audita.
5. Los cambios en archivos .ci requieren compilar el proyecto en Power Operation Studio y recargar el runtime.";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PowerOpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("poweroperation-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}
