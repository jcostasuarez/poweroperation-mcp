use std::io::Write as _;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData, Implementation, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler, tool_handler};

use crate::argerr;
use crate::bridges::{self, Bridges};
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
    pub gb: crate::gb::Gb,
    catalog: Arc<tokio::sync::Mutex<Option<Arc<Catalog>>>>,
    /// Proyecto en ejecución según el runtime: None = sin consultar; Some(None) = no disponible.
    runtime_project: Arc<tokio::sync::Mutex<Option<Option<String>>>>,
    /// Disponibilidad de los puentes: decide qué herramientas se listan (ver `bridges.rs`).
    pub bridges: Arc<Bridges>,
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
            gb: crate::gb::Gb::new(cfg.timeout.max(std::time::Duration::from_secs(150))),
            cfg,
            catalog: Arc::new(tokio::sync::Mutex::new(None)),
            runtime_project: Arc::new(tokio::sync::Mutex::new(None)),
            bridges: Arc::new(Bridges::default()),
            tool_router: Self::all_routers(),
        }
    }

    fn all_routers() -> ToolRouter<Self> {
        Self::status_router()
            + Self::runtime_router()
            + Self::runadmin_router()
            + Self::project_router()
            + Self::projadmin_router()
            + Self::projcreate_router()
            + Self::dbfwrite_router()
            + Self::graphics_router()
            + Self::plc_router()
            + Self::reference_router()
            + Self::studio_router()
    }

    /// Nombres de todas las herramientas registradas (para tests de consistencia con `bridges::TOOLS`).
    #[cfg(test)]
    pub fn router_tool_names() -> Vec<String> {
        Self::all_routers().list_all().into_iter().map(|t| t.name.to_string()).collect()
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

    /// Servidor de alarmas para alarm_sql: configurado, o el definido en el proyecto por defecto / en
    /// ejecución y sus incluidos, o localhost:5482.
    pub async fn alarm_endpoint(&self) -> crate::alarmdb::Endpoint {
        let projects = match (self.project(None).await, self.catalog(false).await) {
            (Ok(p), Ok(cat)) => cat.with_includes(&p).0,
            _ => vec![],
        };
        let cfg = self.cfg.clone();
        blocking(std::time::Duration::from_secs(30), move || Ok(crate::alarmdb::endpoint(&cfg, &projects)))
            .await
            .unwrap_or_else(|_| crate::alarmdb::endpoint(&self.cfg, &[]))
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

Todas las herramientas empiezan con po_. Cada una depende de uno o más PUENTES y solo se lista si todos están
disponibles; cuando un puente aparece o se cae, la lista cambia (notifications/tools/list_changed):
- DISCO (siempre): po_project_list/tables/query/search/graphics, po_cicode_files/read/check.
- CTAPI (runtime corriendo y conectado): po_tag_read (también muestras repetidas), po_tag_browse, po_alarm_list,
  po_trend_query, po_cicode_run, po_ctapi_find. Escrituras: po_tag_write, po_alarm_ack.
- ODBC (driver y servidor de alarmas): po_alarm_sql.
- PROCESOS (siempre): po_runtime_procs, po_runtime_screenshot. Escritura: po_runtime_control (start/restart).
- GB (Graphics Builder instalado): po_gb_status/library/page_read/page_thumbnail. Escrituras: po_gb_page_create/edit/delete.
- STUDIO (compilador): po_project_compile (escritura) y po_studio_set_project (proyecto activo de Studio; escritura).
  po_studio_project (siempre) muestra el activo de Studio y las claves del citect.ini.
- MODBUS (POWEROP_MODBUS configurado y alcanzable): po_plc_read, solo lectura.
- ESCRITURA (POWEROP_ALLOW_WRITE=1): sin él, las herramientas de escritura no se listan. Además de las anteriores:
  po_cicode_write, po_project_create, po_project_rows_add/update.
po_status muestra cada puente, por qué no está disponible y qué herramientas habilita; po_reference es la guía.

Flujo recomendado:
1. `po_status` y `po_project_list` para orientarte (puentes, conexión, procesos, proyectos, cuál corre).
2. Lee `po_reference` antes de escribir Cicode o consultar tablas: nombres de tablas DBF, campos,
   sintaxis Cicode, filtros de CtAPI y consultas SQL de alarmas.
3. Para saber qué tags existen: offline con po_project_query (tabla variable) o en vivo con po_tag_browse.
4. Power Operation es un SCADA eléctrico: escribir tags u operar interruptores actúa sobre equipos reales.
   Confirmá SIEMPRE con el usuario antes de cualquier escritura (po_tag_write, po_alarm_ack, po_cicode_run con
   efectos, po_cicode_write, po_project_*, po_gb_page_*, po_runtime_control, po_studio_set_project). Todo se audita. Nunca escribas en un
   PLC: po_plc_read solo lee.
5. Los cambios en archivos .ci o tablas requieren compilar (po_project_compile, o Studio). Si el proyecto es el que
   corre, Runtime Manager A VECES reinicia el runtime al recompilar: mirá po_runtime_procs. Las páginas se ven
   con refresh_page; tags y Cicode nuevos pueden necesitar po_runtime_control action=restart (con confirmación).";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PowerOpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_tool_list_changed().build())
            .with_server_info(Implementation::new("poweroperation-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    /// Solo las herramientas cuyos puentes están disponibles (ver `bridges.rs`).
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.bridges.set_peer(context.peer.clone());
        let tools = self.tool_router.list_all().into_iter().filter(|t| self.bridges.visible(&t.name)).collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    /// Mismo despacho que genera `#[tool_handler]` (el router), pero: rechaza herramientas cuyo puente no está
    /// disponible (o nombres viejos, con el reemplazo), y si los argumentos no se pueden deserializar, el error
    /// dice qué campos son válidos y trae el esquema (ver `argerr`).
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let nombre = request.name.to_string();
        let args = request.arguments.clone().unwrap_or_default();
        self.bridges.set_peer(context.peer.clone());
        if self.tool_router.get(&nombre).is_none() {
            if let Some(nuevo) = bridges::renamed(&nombre) {
                let msg = format!("La herramienta {nombre} ya no existe: ahora es {nuevo}. No se ejecutó nada.");
                return Ok(CallToolResponse::Complete(CallToolResult::error(vec![ContentBlock::text(msg)])));
            }
        } else if !self.bridges.visible(&nombre) {
            // Puede haber vuelto desde el último sondeo: se reintenta con los puentes que cambian solos.
            let periodic: Vec<_> = self.bridges.missing(&nombre).into_iter().filter(|b| b.periodic()).collect();
            if !periodic.is_empty() {
                self.probe(&periodic).await;
            }
            if !self.bridges.visible(&nombre) {
                let msg = self.bridges.unavailable_message(&nombre);
                return Ok(CallToolResponse::Complete(CallToolResult::error(vec![ContentBlock::text(msg)])));
            }
        }
        // Campos desconocidos: serde los ignora, así que se rechazan acá antes de ejecutar (y de escribir nada).
        if let Some(t) = self.tool_router.get(&nombre) {
            let schema = serde_json::Value::Object((*t.input_schema).clone());
            let extra = argerr::desconocidos(&schema, &args);
            if !extra.is_empty() {
                let detalle = format!("campo(s) desconocido(s) {}; no se ejecutó nada", extra.join(", "));
                let msg = argerr::mensaje(&nombre, &detalle, &schema, &args);
                return Ok(CallToolResponse::Complete(CallToolResult::error(vec![ContentBlock::text(msg)])));
            }
        }
        let resp = self.tool_router.call(ToolCallContext::new(self, request, context)).await?;
        match resp {
            CallToolResponse::Complete(res) if res.is_error == Some(true) => {
                let texto = res.content.iter().find_map(|c| c.as_text()).map(|t| t.text.clone()).unwrap_or_default();
                let Some(detalle) = argerr::detalle_deserializacion(&texto) else {
                    return Ok(CallToolResponse::Complete(res));
                };
                let schema = self.tool_router.get(&nombre).map(|t| serde_json::Value::Object((*t.input_schema).clone()));
                let schema = schema.unwrap_or_default();
                let msg = argerr::mensaje(&nombre, detalle, &schema, &args);
                Ok(CallToolResponse::Complete(CallToolResult::error(vec![ContentBlock::text(msg)])))
            }
            otro => Ok(otro),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Toda herramienta registrada tiene puentes en `bridges::TOOLS` y viceversa, sin repetidos y con prefijo po_.
    #[test]
    fn tabla_de_puentes_coincide_con_el_router() {
        let mut router = PowerOpServer::router_tool_names();
        router.sort();
        let mut tabla: Vec<String> = bridges::TOOLS.iter().map(|(n, _)| n.to_string()).collect();
        tabla.sort();
        assert_eq!(router, tabla);
        let mut sin_repetir = tabla.clone();
        sin_repetir.dedup();
        assert_eq!(sin_repetir, tabla, "nombres repetidos en bridges::TOOLS");
        assert!(tabla.iter().all(|n| n.starts_with("po_")), "{tabla:?}");
    }
}
