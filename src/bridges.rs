//! Puentes: las fuentes externas de las que depende cada herramienta (runtime por CtAPI, ODBC de alarmas,
//! Graphics Builder, compilador de Studio, PLC Modbus...) y el modo escritura. Una herramienta se lista solo si
//! todos sus puentes están disponibles; cuando cambia la disponibilidad se avisa al cliente con
//! `notifications/tools/list_changed`. El sondeo está en `tools/status.rs`.

use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};

use rmcp::RoleServer;
use rmcp::service::Peer;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Bridge {
    Disco,
    Escritura,
    Procesos,
    Ctapi,
    Odbc,
    Gb,
    Studio,
    Modbus,
}

use Bridge::*;

impl Bridge {
    pub const ALL: [Bridge; 8] = [Disco, Escritura, Procesos, Ctapi, Odbc, Gb, Studio, Modbus];

    pub fn name(self) -> &'static str {
        match self {
            Disco => "DISCO",
            Escritura => "ESCRITURA",
            Procesos => "PROCESOS",
            Ctapi => "CTAPI",
            Odbc => "ODBC",
            Gb => "GB",
            Studio => "STUDIO",
            Modbus => "MODBUS",
        }
    }

    /// Qué es y qué hace falta para que esté disponible.
    pub fn what(self) -> &'static str {
        match self {
            Disco => "archivos del proyecto en disco (.dbf, .ci, .ctg); siempre disponible",
            Escritura => "modo escritura: POWEROP_ALLOW_WRITE=1 al arrancar el servidor",
            Procesos => "procesos y ventanas del runtime en Windows; siempre disponible",
            Ctapi => "runtime de Power Operation por CtAPI (CtApi.dll y runtime corriendo)",
            Odbc => "base de alarmas por ODBC (driver 'Citect Alarm Driver' y servidor de alarmas escuchando)",
            Gb => "Graphics Builder por COM (instalado y registrado)",
            Studio => "compilador de Studio (PowerSCADA.Common.dll, citect.ini y PowerShell de 32 bits)",
            Modbus => "PLC por Modbus TCP, solo lectura (POWEROP_MODBUS configurado y alcanzable)",
        }
    }

    /// Se vuelve a sondear cada POWEROP_PROBE_SECS y al llamar una herramienta oculta. Los demás no cambian
    /// mientras corre el servidor (o solo con po_status).
    pub fn periodic(self) -> bool {
        matches!(self, Ctapi | Odbc | Modbus)
    }
}

/// Cada herramienta y los puentes que necesita. `tool_router` y esta tabla deben coincidir (hay un test).
pub const TOOLS: &[(&str, &[Bridge])] = &[
    ("po_status", &[]),
    ("po_reference", &[]),
    ("po_project_list", &[Disco]),
    ("po_project_tables", &[Disco]),
    ("po_project_query", &[Disco]),
    ("po_project_search", &[Disco]),
    ("po_project_graphics", &[Disco]),
    ("po_oneline_read", &[Disco]),
    ("po_oneline_validate", &[Disco]),
    ("po_cicode_files", &[Disco]),
    ("po_cicode_read", &[Disco]),
    ("po_cicode_check", &[Disco]),
    ("po_cicode_write", &[Disco, Escritura]),
    ("po_project_create", &[Disco, Escritura]),
    ("po_project_unregister", &[Disco, Escritura]),
    ("po_project_rows_add", &[Disco, Escritura]),
    ("po_project_rows_update", &[Disco, Escritura]),
    ("po_project_compile", &[Studio, Escritura]),
    ("po_studio_project", &[Procesos]),
    ("po_studio_set_project", &[Studio, Escritura]),
    ("po_tag_read", &[Ctapi]),
    ("po_tag_browse", &[Ctapi]),
    ("po_ctapi_find", &[Ctapi]),
    ("po_trend_query", &[Ctapi]),
    ("po_alarm_list", &[Ctapi]),
    ("po_cicode_run", &[Ctapi]),
    ("po_tag_write", &[Ctapi, Escritura]),
    ("po_alarm_ack", &[Ctapi, Escritura]),
    ("po_alarm_sql", &[Odbc]),
    ("po_runtime_procs", &[Procesos]),
    ("po_runtime_screenshot", &[Procesos]),
    ("po_runtime_control", &[Procesos, Escritura]),
    ("po_gb_status", &[Gb]),
    ("po_gb_library", &[Gb]),
    ("po_gb_page_read", &[Gb]),
    ("po_gb_page_thumbnail", &[Gb]),
    ("po_gb_page_create", &[Gb, Escritura]),
    ("po_gb_page_edit", &[Gb, Escritura]),
    ("po_gb_page_delete", &[Gb, Escritura]),
    ("po_gb_genie_read", &[Gb]),
    ("po_gb_genie_create", &[Gb, Escritura]),
    ("po_gb_genie_edit", &[Gb, Escritura]),
    ("po_gb_genie_delete", &[Gb, Escritura]),
    ("po_plc_read", &[Modbus]),
];

/// Nombres de la versión 0.2 que cambiaron o se fusionaron, con el reemplazo.
const RENAMED: &[(&str, &str)] = &[
    ("runtime_status", "po_status"),
    ("poweroperation_reference", "po_reference"),
    ("tag_watch", "po_tag_read (con samples e interval_secs)"),
    ("runtime_start", "po_runtime_control (action=start)"),
    ("runtime_restart", "po_runtime_control (action=restart)"),
    ("page_display", "po_cicode_run con PageDisplay(\"Pagina\"), o po_project_compile con refresh_page"),
];

pub fn bridges_of(tool: &str) -> Option<&'static [Bridge]> {
    TOOLS.iter().find(|(n, _)| *n == tool).map(|(_, b)| *b)
}

/// Reemplazo de un nombre viejo (`tag_read` → `po_tag_read`, `tag_watch` → `po_tag_read (...)`).
pub fn renamed(old: &str) -> Option<String> {
    if let Some((_, new)) = RENAMED.iter().find(|(o, _)| *o == old) {
        return Some((*new).to_string());
    }
    let new = format!("po_{old}");
    bridges_of(&new).map(|_| new)
}

/// Herramientas que dependen de un puente.
pub fn tools_of(b: Bridge) -> Vec<&'static str> {
    TOOLS.iter().filter(|(_, bs)| bs.contains(&b)).map(|(n, _)| *n).collect()
}

#[derive(Clone, Debug)]
pub struct Status {
    pub ok: bool,
    pub detail: String,
    /// Hora del último sondeo (HH:MM:SS).
    pub checked: String,
}

/// Estado de los puentes, compartido por todas las copias del servidor.
#[derive(Default)]
pub struct Bridges {
    state: RwLock<BTreeMap<Bridge, Status>>,
    peer: OnceLock<Peer<RoleServer>>,
    /// Resultado de buscar CtApi.dll en la instalación (la búsqueda es lenta: se hace una vez).
    pub ctapi_dll: RwLock<Option<bool>>,
}

impl Bridges {
    pub fn get(&self, b: Bridge) -> Option<Status> {
        self.state.read().unwrap_or_else(|e| e.into_inner()).get(&b).cloned()
    }

    /// Sin sondear todavía = no disponible.
    pub fn ok(&self, b: Bridge) -> bool {
        self.get(b).is_some_and(|s| s.ok)
    }

    /// Guarda el resultado de un sondeo. Si cambió la disponibilidad, avisa al cliente (la lista de herramientas
    /// visibles depende solo de eso).
    pub fn set(&self, b: Bridge, ok: bool, detail: String) {
        let checked = chrono::Local::now().format("%H:%M:%S").to_string();
        let prev = {
            let mut st = self.state.write().unwrap_or_else(|e| e.into_inner());
            st.insert(b, Status { ok, detail, checked }).map(|s| s.ok)
        };
        if prev != Some(ok) {
            tracing::info!(puente = b.name(), disponible = ok, "cambio de disponibilidad");
            if prev.is_some() || ok {
                self.notify();
            }
        }
    }

    /// Puentes que le faltan a una herramienta (vacío = se puede usar). Herramienta desconocida: ninguno.
    pub fn missing(&self, tool: &str) -> Vec<Bridge> {
        bridges_of(tool).unwrap_or(&[]).iter().copied().filter(|b| !self.ok(*b)).collect()
    }

    pub fn visible(&self, tool: &str) -> bool {
        self.missing(tool).is_empty()
    }

    pub fn set_peer(&self, peer: Peer<RoleServer>) {
        let _ = self.peer.set(peer);
    }

    fn notify(&self) {
        if let Some(peer) = self.peer.get() {
            let peer = peer.clone();
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move {
                    if let Err(e) = peer.notify_tool_list_changed().await {
                        tracing::warn!("no se pudo avisar tools/list_changed: {e}");
                    }
                });
            }
        }
    }

    /// Mensaje para una herramienta oculta que el agente igual llamó.
    pub fn unavailable_message(&self, tool: &str) -> String {
        let reasons: Vec<String> = self
            .missing(tool)
            .iter()
            .map(|b| {
                let why = self.get(*b).map(|s| s.detail).unwrap_or_else(|| "todavía sin sondear".into());
                format!("puente {} ({}): {why}", b.name(), b.what())
            })
            .collect();
        format!(
            "{tool} no está disponible ahora y no se ejecutó nada. {}. po_status muestra cada puente y qué herramientas habilita.",
            reasons.join("; ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renombres() {
        assert_eq!(renamed("tag_read").as_deref(), Some("po_tag_read"));
        assert_eq!(renamed("tag_watch").as_deref(), Some("po_tag_read (con samples e interval_secs)"));
        assert_eq!(renamed("runtime_restart").as_deref(), Some("po_runtime_control (action=restart)"));
        assert_eq!(renamed("no_existe"), None);
        // Toda herramienta que nombra un reemplazo existe (y ningún nombre viejo sigue siendo una herramienta).
        for (old, new) in RENAMED {
            assert!(bridges_of(old).is_none() && bridges_of(&format!("po_{old}")).is_none(), "{old} todavía existe");
            for t in new.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|w| w.starts_with("po_")) {
                assert!(bridges_of(t).is_some(), "{old} → {t}: no existe");
            }
        }
    }

    #[test]
    fn visibilidad_por_puente() {
        let b = Bridges::default();
        assert!(b.visible("po_status"));
        assert!(!b.visible("po_tag_read"));
        b.set(Ctapi, true, "ok".into());
        assert!(b.visible("po_tag_read"));
        assert!(!b.visible("po_tag_write"));
        assert_eq!(b.missing("po_tag_write"), vec![Escritura]);
        b.set(Escritura, false, "solo lectura: POWEROP_ALLOW_WRITE=1".into());
        let m = b.unavailable_message("po_tag_write");
        assert!(m.starts_with("po_tag_write no está disponible ahora y no se ejecutó nada."), "{m}");
        assert!(m.contains(&format!("puente {} (", Escritura.name())) && m.contains("POWEROP_ALLOW_WRITE=1"), "{m}");
        assert!(!m.contains("CTAPI"), "CTAPI está disponible: {m}");
    }
}
