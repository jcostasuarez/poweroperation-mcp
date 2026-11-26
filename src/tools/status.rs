//! `po_status` y el sondeo de los puentes (ver `bridges.rs`): qué fuentes están disponibles y, por lo tanto,
//! qué herramientas se listan.

use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::alarmdb;
use crate::bridges::{self, Bridge};
use crate::ctapi;
use crate::discover;
use crate::modbus;
use crate::procs;
use crate::server::{PowerOpServer, ToolResult, blocking, err};

/// Tiempo máximo del sondeo de CtAPI (no bloquear la lista de herramientas por un runtime colgado).
const CTAPI_PROBE: Duration = Duration::from_secs(10);

#[derive(Deserialize, JsonSchema)]
pub struct StatusParams {
    /// Cerrar y volver a abrir la conexión CtAPI antes de sondear.
    #[serde(default)]
    pub reconnect: bool,
}

fn tcp_open(host: &str, port: u16, timeout: Duration) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    (host, port)
        .to_socket_addrs()
        .map(|mut a| a.any(|a| TcpStream::connect_timeout(&a, timeout).is_ok()))
        .unwrap_or(false)
}

impl PowerOpServer {
    /// Sondea los puentes indicados en paralelo y guarda el resultado (avisa al cliente si cambia la lista).
    pub async fn probe(&self, which: &[Bridge]) {
        let tasks: Vec<_> = which
            .iter()
            .map(|&b| {
                let s = self.clone();
                tokio::spawn(async move {
                    let (ok, detail) = s.probe_one(b).await;
                    s.bridges.set(b, ok, detail);
                })
            })
            .collect();
        for t in tasks {
            let _ = t.await;
        }
    }

    /// Sondeo periódico de CTAPI, ODBC y MODBUS (POWEROP_PROBE_SECS; 0 = desactivado).
    pub async fn probe_loop(self) {
        if self.cfg.probe_secs == 0 {
            return;
        }
        let periodic: Vec<Bridge> = Bridge::ALL.into_iter().filter(|b| b.periodic()).collect();
        loop {
            tokio::time::sleep(Duration::from_secs(self.cfg.probe_secs)).await;
            self.probe(&periodic).await;
        }
    }

    async fn probe_one(&self, b: Bridge) -> (bool, String) {
        match b {
            Bridge::Disco => (true, "archivos locales".into()),
            Bridge::Procesos => (true, "Windows".into()),
            Bridge::Escritura => {
                if self.cfg.allow_write {
                    (true, "POWEROP_ALLOW_WRITE=1: escrituras habilitadas (siempre con confirmación del usuario y auditadas)".into())
                } else {
                    (false, "servidor en solo lectura: el usuario debe arrancarlo con POWEROP_ALLOW_WRITE=1 para habilitar escrituras".into())
                }
            }
            Bridge::Ctapi => self.probe_ctapi().await,
            Bridge::Odbc => self.probe_odbc().await,
            Bridge::Gb => match blocking(Duration::from_secs(10), || Ok(crate::gb::registered())).await {
                Ok(true) => (true, "Graphics Builder registrado (se abre o se conecta con po_gb_status)".into()),
                Ok(false) => (false, "Graphics Builder no está registrado como servidor COM (¿Power Operation Studio instalado?)".into()),
                Err(e) => (false, err(e)),
            },
            Bridge::Studio => {
                let inis = match self.catalog(false).await {
                    Ok(c) => c.inis.clone(),
                    Err(e) => return (false, err(e)),
                };
                let r = blocking(Duration::from_secs(90), move || {
                    let (dll, ini) = super::projadmin::locate(&inis)?;
                    super::projadmin::ps32_exe()?;
                    Ok(format!("{} con {}", dll.display(), ini.display()))
                })
                .await;
                match r {
                    Ok(d) => (true, d),
                    Err(e) => (false, err(e)),
                }
            }
            Bridge::Modbus => {
                let targets = self.cfg.modbus.clone();
                if targets.is_empty() {
                    return (false, "no configurado: definí POWEROP_MODBUS=nombre=host:puerto/unidad (p. ej. PLC1=192.0.2.10:502/255)".into());
                }
                let r = blocking(Duration::from_secs(30), move || {
                    Ok(targets
                        .iter()
                        .map(|t| match modbus::connect(t, Duration::from_secs(2)) {
                            Ok(_) => (true, format!("{t}: puerto abierto")),
                            Err(e) => (false, format!("{t}: {e:#}")),
                        })
                        .collect::<Vec<_>>())
                })
                .await;
                match r {
                    Ok(v) => (v.iter().any(|(ok, _)| *ok), v.into_iter().map(|(_, d)| d).collect::<Vec<_>>().join("; ")),
                    Err(e) => (false, err(e)),
                }
            }
        }
    }

    async fn probe_ctapi(&self) -> (bool, String) {
        let cfg = self.cfg.clone();
        let known = *self.bridges.ctapi_dll.read().unwrap_or_else(|e| e.into_inner());
        let has_dll = if self.ct.loaded_dll().is_some() || cfg.ctapi_path.is_some() {
            true
        } else if let Some(k) = known {
            k
        } else {
            let k = blocking(Duration::from_secs(60), move || Ok(!ctapi::find_dlls(&cfg).is_empty())).await.unwrap_or(false);
            *self.bridges.ctapi_dll.write().unwrap_or_else(|e| e.into_inner()) = Some(k);
            k
        };
        if !has_dll {
            return (false, "CtApi.dll no encontrado (indicá la carpeta con POWEROP_CTAPI)".into());
        }
        // Local y sin Citect32: no tiene sentido intentar ctOpen (y no ocupar el mutex de CtAPI).
        if self.cfg.host.is_none() && self.cfg.ctapi_path.is_none() {
            let pids = blocking(Duration::from_secs(10), procs::citect_pids).await.unwrap_or_default();
            if pids.is_empty() {
                return (false, "runtime detenido: no hay procesos Citect32.exe (po_runtime_control con action=start lo arranca, con confirmación del usuario)".into());
            }
        }
        match tokio::time::timeout(CTAPI_PROBE, self.ct.call(|s| s.cicode("Time(1)", 0))).await {
            Ok(Ok(t)) => (true, format!("conectado (hora del SCADA {t})")),
            Ok(Err(e)) => {
                self.ct.disconnect().await;
                (false, format!("sin conexión: {}", err(e)))
            }
            Err(_) => (false, format!("sin respuesta en {} s (runtime ocupado, arrancando o colgado: mirá po_runtime_procs)", CTAPI_PROBE.as_secs())),
        }
    }

    async fn probe_odbc(&self) -> (bool, String) {
        let has = match blocking(Duration::from_secs(20), alarmdb::drivers).await {
            Ok(d) => d.iter().any(|x| x.eq_ignore_ascii_case(alarmdb::DRIVER)),
            Err(e) => return (false, format!("no se pudieron listar los drivers ODBC: {}", err(e))),
        };
        if !has {
            return (false, format!("driver ODBC '{}' no instalado", alarmdb::DRIVER));
        }
        let ep = self.alarm_endpoint().await;
        let (host, port) = (ep.host.clone(), ep.port);
        let open = blocking(Duration::from_secs(10), move || Ok(tcp_open(&host, port, Duration::from_secs(3)))).await.unwrap_or(false);
        if open {
            (true, format!("servidor de alarmas {ep}: puerto abierto"))
        } else {
            (false, format!(
                "el puerto TCP del servidor de alarmas {ep} está CERRADO (runtime detenido, o escucha en otro host/puerto: POWEROP_ALARM_HOST/POWEROP_ALARM_PORT)"
            ))
        }
    }
}

#[tool_router(router = status_router, vis = "pub(crate)")]
impl PowerOpServer {
    #[tool(name = "po_status", description = "Diagnóstico general; úsalo primero. Vuelve a sondear todos los puentes y muestra cuáles están disponibles, por qué no lo están y qué herramientas habilita cada uno (las de un puente no disponible no se listan). Además: CtApi.dll encontrado, conexión y proyecto del runtime, citect.ini, modo de escritura, servidor de alarmas ODBC y PLCs Modbus configurados. Solo lectura.")]
    async fn po_status(&self, Parameters(p): Parameters<StatusParams>) -> ToolResult {
        if p.reconnect {
            self.ct.disconnect().await;
        }
        *self.bridges.ctapi_dll.write().unwrap_or_else(|e| e.into_inner()) = None;
        self.probe(&Bridge::ALL).await;

        let mut out = vec![
            format!("poweroperation-mcp {} ({})", env!("CARGO_PKG_VERSION"), discover::this_arch()),
            "Puentes (una herramienta se lista solo si están disponibles todos sus puentes):".to_string(),
        ];
        let mut visible = 0;
        for (name, _) in bridges::TOOLS {
            if self.bridges.visible(name) {
                visible += 1;
            }
        }
        for b in Bridge::ALL {
            let st = self.bridges.get(b);
            let (ok, detail, at) = st.map(|s| (s.ok, s.detail, s.checked)).unwrap_or((false, "sin sondear".into(), "-".into()));
            // Disponible: las que siguen ocultas por otro puente lo dicen, para no dar a entender que están.
            let tools: Vec<String> = bridges::tools_of(b)
                .into_iter()
                .map(|t| {
                    let others: Vec<&str> = self.bridges.missing(t).into_iter().filter(|m| *m != b).map(|m| m.name()).collect();
                    if ok && !others.is_empty() { format!("{t} (falta {})", others.join("+")) } else { t.to_string() }
                })
                .collect();
            out.push(format!(
                "- {}: {} ({at}) — {detail}\n    {}: {}",
                b.name(),
                if ok { "DISPONIBLE" } else { "NO DISPONIBLE" },
                if ok { "habilita" } else { "oculta" },
                if tools.is_empty() { "-".into() } else { tools.join(", ") }
            ));
        }
        out.push(format!(
            "Herramientas visibles: {visible} de {} (po_status y po_reference siempre). Los puentes CTAPI, ODBC y MODBUS se vuelven a sondear cada {} s.",
            bridges::TOOLS.len(),
            self.cfg.probe_secs
        ));

        out.push("Detalle:".into());
        out.push(format!("Escrituras: {}", if self.cfg.allow_write { "HABILITADAS" } else { "deshabilitadas (solo lectura)" }));
        let cfg = self.cfg.clone();
        let dlls = blocking(Duration::from_secs(60), move || Ok(ctapi::find_dlls(&cfg))).await.unwrap_or_default();
        if dlls.is_empty() {
            out.push("CtApi.dll: NO encontrado (POWEROP_CTAPI para indicar la ruta)".into());
        }
        for d in dlls.iter().take(6) {
            out.push(format!("CtApi.dll candidato: {} [{}]", d.path.display(), d.arch.unwrap_or("?")));
        }
        if let Some(p) = self.ct.loaded_dll() {
            out.push(format!("DLL cargado: {}", p.display()));
        }
        if self.bridges.ok(Bridge::Ctapi) {
            out.push(format!(
                "Proyecto en runtime: {}",
                self.runtime_project(true).await.unwrap_or_else(|| "(no disponible)".into())
            ));
        }
        let pids = blocking(Duration::from_secs(10), procs::citect_pids).await.unwrap_or_default();
        out.push(format!("Procesos Citect32.exe: {} (roles, ventanas y logs en po_runtime_procs)", pids.len()));
        if let Ok(cat) = self.catalog(false).await {
            for ini in &cat.inis {
                out.push(format!(
                    "citect.ini: {} | Run={} | User={} | [CtAPI] Remote={}",
                    ini.path.display(),
                    ini.get("CtEdit", "Run").unwrap_or("-"),
                    ini.get("CtEdit", "User").unwrap_or("-"),
                    ini.get("CtAPI", "Remote").unwrap_or("-")
                ));
            }
        }
        out.push(format!("Servidor de alarmas para po_alarm_sql: {}", self.alarm_endpoint().await));
        if self.cfg.modbus.is_empty() {
            out.push("PLCs Modbus: ninguno (POWEROP_MODBUS)".into());
        }
        for t in &self.cfg.modbus {
            out.push(format!("PLC Modbus: {t} (solo lectura con po_plc_read)"));
        }
        Ok(out.join("\n"))
    }
}
