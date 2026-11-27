# Power Operation MCP

Servidor MCP (Rust + rmcp 3.5, stdio) para EcoStruxure Power Operation 2024 (AVEVA Plant SCADA 8.50 / Citect).
Mismo stack y estilo que `..\ignition-mcp`. Se entrega como exe (`dist\poweroperation-mcp-<versión>.zip`).
45 herramientas `po_*` (desde 0.3.0; antes sin prefijo): runtime por CtAPI (`src\tools\runtime.rs`), procesos y arranque del runtime (`src\tools\runadmin.rs`
sobre `src\procs.rs`), proyecto offline por DBF/Cicode (`src\tools\project.rs`), escritura de tablas DBF
(`src\tools\dbfwrite.rs` sobre `src\dbf.rs` y `src\ndx.rs`), creación de proyectos (`src\tools\projcreate.rs`),
compilación (`src\tools\projadmin.rs`), proyecto activo de Studio (`src\tools\studio.rs`: UI Automation, citect.ini),
Graphics Builder por COM (`src\tools\graphics.rs` sobre `src\gb.rs`), Advanced One-Line offline
(`src\tools\oneline.rs` sobre `src\oneline.rs`, `src\oneline_clone.rs` (renumeración de `po_oneline_clone`) y el lector de `.ctg` `src\ctg.rs`: contenedor sacado de `Ctg32.dll`,
contenido de los objetos deducido de páginas reales, ver su cabecera) y la referencia para el agente (`src\tools\reference.md`, embebida), PLC por Modbus TCP solo lectura (`src\tools\plc.rs`
sobre `src\modbus.rs`) y `po_status` + sondeo de puentes (`src\tools\status.rs`). `src\argerr.rs` arma los errores
de argumentos (campos válidos + esquema) desde `call_tool` en `src\server.rs`.

**Puentes (`src\bridges.rs`):** DISCO, PROCESOS, ESCRITURA, CTAPI, ODBC, GB, STUDIO, MODBUS. La tabla `TOOLS` dice
qué puentes necesita cada herramienta (el test `tabla_de_puentes_coincide_con_el_router` exige que coincida con los
routers). `list_tools` solo devuelve las visibles; `call_tool` rechaza las ocultas con el motivo (y re-sondea antes
los puentes periódicos) y traduce nombres viejos (`RENAMED`). CTAPI, ODBC y MODBUS se sondean cada
`POWEROP_PROBE_SECS`; si cambia la visibilidad se manda `tools/list_changed`. Al arrancar se sondea todo (hasta 8 s)
antes de servir. Herramienta nueva: agregarla al router, a `TOOLS` y, si reemplaza a otra, a `RENAMED`.


Las notas del entorno de desarrollo (VM con PO 2024, proyectos de prueba, verificaciones en vivo y pendientes) van en
`AGENT.md`, que es local y no se publica (`.gitignore`).

## Comandos

```
cargo build --release && cargo build --release --target i686-pc-windows-msvc
cargo test                                          # unitarios + tests\offline.rs + tests\runtime.rs (CtApi.dll falso)
cargo test --test live -- --ignored --nocapture     # contra el runtime real de MCP_Demo y Graphics Builder
                                                    # (gb_write escribe la página MCP_GB_Test; POWEROP_LIVE_OUT=<carpeta> guarda miniaturas)
target\release\poweroperation-mcp.exe --doctor
```

`tests\runtime.rs` compila `tests\fakectapi` (CtApi.dll falso que imita a PO 2024). Los e2e lanzan el exe por stdio
aislado de la instalación real (ProgramData/ProgramFiles falsos). `dist\`: exe x64 + `x86\` + README + `.env.example`,
comprimido como `dist\poweroperation-mcp-<versión>.zip`.

Release: `.\scripts\release.ps1` (x64 + x86, zip y SHA256 en `dist\`; con `-Publish` crea el release `v<versión>` en GitHub).

## Reglas

- Escrituras (po_tag_write, po_alarm_ack, po_cicode_run con efectos, po_cicode_write, po_gb_page_create/edit/delete,
  po_gb_genie_create/edit/delete (solo librerías .ctm del propio proyecto, nunca de incluidos),
  po_project_create, po_project_unregister, po_project_rows_add/update, po_oneline_clone, po_project_compile, po_runtime_control, po_studio_set_project) siempre detrás de
  `POWEROP_ALLOW_WRITE=1` (sin él ni se listan) y auditadas. No escribir DBF salvo en `MCP_Demo` o en proyectos creados por el MCP (archivo
  `_mcp_created`), siempre con respaldo; respaldar `MASTER.DBF` antes de tocarlo. Cambiar `Run=` del citect.ini solo
  con respaldo fechado y OK del usuario. Nunca escribir contraseñas.
- Es un SCADA eléctrico: nunca escribir tags ni reconocer alarmas sin confirmación explícita del usuario.
- PLC reales (Modbus TCP): solo lecturas (FC1–FC4); el MCP no tiene escrituras Modbus y no hay que agregarlas.
- No restaurar backups `.ctz` para crear proyectos. Preguntar antes de cerrar Studio o Graphics Builder.
- No cargar `ctExplor.dll` en ningún proceso (Assembly.LoadFile, etc.): Studio reescribió `Run=` del citect.ini
  (visto el 2026-10-16 06:41). Tampoco reiniciar Studio sin OK del usuario (`po_studio_set_project restart_studio=true`).
- No correr `CtCmp32.exe` a mano: reescribe `Run=`, `LASTDATABASE` y `LASTDATABASEPATH` del citect.ini con el
  proyecto compilado, sin respaldo (visto el 2026-10-16 08:10). Compilar con `po_project_compile` (ProjectManager).
- Lanzar procesos de larga vida (Studio, Citect32) con `procs::spawn_detached` (CreateProcessW sin heredar handles),
  nunca con `std::process::Command`: heredan los pipes stdio del MCP y el cliente queda colgado esperando EOF.
- stdout es el canal MCP: logs solo a stderr.
- `src\tools\reference.md` es lo que lee el agente: mantenerlo correcto y marcar lo verificado / no verificado.
