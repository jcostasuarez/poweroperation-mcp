use rmcp::{tool, tool_router};

use crate::server::{PowerOpServer, ToolResult};

const REFERENCE: &str = include_str!("reference.md");

#[tool_router(router = reference_router, vis = "pub(crate)")]
impl PowerOpServer {
    #[tool(description = "Guía de referencia de Power Operation / Plant SCADA: carpetas y tablas DBF del proyecto, sintaxis Cicode, CtAPI (filtros, TRNQUERY), SQL de alarmas y reglas de seguridad. Léela antes de escribir Cicode o consultar tablas.")]
    async fn poweroperation_reference(&self) -> ToolResult {
        Ok(REFERENCE.to_string())
    }
}
