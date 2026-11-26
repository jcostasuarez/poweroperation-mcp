# poweroperation-mcp

Servidor MCP (stdio) para **EcoStruxure Power Operation** (basado en AVEVA Plant SCADA / Citect).
Es un único `.exe` sin instalador ni dependencias: usa el `CtApi.dll` de la instalación de Power Operation
que ya está en la máquina.

## Qué puede hacer el agente

Todas las herramientas se llaman `po_*`. Cada una depende de un **puente** (fuente de datos); solo se listan las de
puentes disponibles en ese momento y `po_status` dice cuáles están y por qué no (ver "Puentes" abajo).

| Área | Herramientas | Puente |
|---|---|---|
| Diagnóstico | `po_status` (puentes, CtApi.dll, citect.ini, PLCs), `po_reference` | siempre |
| Procesos del runtime | `po_runtime_procs` (rol, PID, arranque, si responde, errores de Runtime Manager), `po_runtime_screenshot` (PNG del cliente), `po_runtime_control`* (`start` / `restart`) | PROCESOS (solo la máquina) |
| Tags en vivo | `po_tag_read` (con `samples`/`interval_secs`: N lecturas cada X s), `po_tag_browse`, `po_tag_write`* (uno o lote) | CTAPI |
| Cicode en runtime | `po_cicode_run` (solo lectura salvo *; `PageDisplay("Pagina")` abre o refresca una página) | CTAPI |
| Tendencias | `po_trend_query` (JSON/CSV + mín/máx/prom/desvío) | CTAPI |
| Alarmas | `po_alarm_list` (estado actual o resumen de activaciones), `po_alarm_ack`* | CTAPI |
| Alarmas por SQL | `po_alarm_sql` (histórico/journal) | ODBC |
| Acceso genérico | `po_ctapi_find` (cualquier tabla CtAPI, TRNQUERY...) | CTAPI |
| Proyecto offline | `po_project_list`, `po_project_tables`, `po_project_query`, `po_project_search`, `po_project_graphics` (páginas y librerías gráficas) | DISCO |
| Crear y configurar proyectos | `po_project_create`*, `po_project_unregister`*, `po_project_rows_add`*, `po_project_rows_update`* (solo `MCP_Demo` o proyectos creados por el MCP) | DISCO |
| Compilar | `po_project_compile`* (errores de `errlog.dbf`; `refresh_page`) | STUDIO |
| Proyecto activo de Studio | `po_studio_project` (lectura), `po_studio_set_project`* (con Studio abierto cambia en caliente por Graphics Builder si está abierto sin páginas; si no, `restart_studio=true`: ver `reference.md`, sección "Proyecto activo de Studio") | PROCESOS, STUDIO |
| Cicode offline | `po_cicode_files`, `po_cicode_read`, `po_cicode_write`*, `po_cicode_check` | DISCO |
| Páginas gráficas | `po_gb_status`, `po_gb_library`, `po_gb_page_read`, `po_gb_page_thumbnail` (PNG), `po_gb_page_create`*, `po_gb_page_edit`*, `po_gb_page_delete`*, `po_gb_genie_read`, `po_gb_genie_create`*, `po_gb_genie_edit`*, `po_gb_genie_delete`* | GB (Graphics Builder instalado; `po_gb_status` lo abre) |
| PLC | `po_plc_read` (Modbus TCP, **solo lectura** FC1–FC4, notación `%MW`/`%IW`/`%M`/`%I`) | MODBUS |

\* Escrituras: puente ESCRITURA (`POWEROP_ALLOW_WRITE=1`). Sin él **no se listan**. Quedan en el log de auditoría.

### Puentes

| Puente | Disponible cuando |
|---|---|
| DISCO, PROCESOS | siempre |
| ESCRITURA | `POWEROP_ALLOW_WRITE=1` |
| CTAPI | se encontró `CtApi.dll` y el cliente del runtime responde |
| ODBC | driver "Citect Alarm Driver" instalado y el puerto del servidor de alarmas abierto |
| GB | Graphics Builder registrado como servidor COM |
| STUDIO | `PowerSCADA.Common.dll` y PowerShell de 32 bits encontrados |
| MODBUS | `POWEROP_MODBUS` configurado y el puerto del PLC abierto |

CTAPI, ODBC y MODBUS se vuelven a sondear cada `POWEROP_PROBE_SECS` (30 s) y, si cambia la lista, el servidor avisa al
cliente (`tools/list_changed`). Si el cliente no refresca la lista, reiniciar el MCP. Los nombres de antes de 0.3.0
(`tag_read`, `runtime_status`...) responden con el nombre nuevo; los permisos guardados en el agente
(`mcp__poweroperation__tag_read`...) hay que actualizarlos a `po_*`.

## Instalación en la VM

1. Copiar la carpeta (p. ej. a `C:\MCP\poweroperation-mcp\`). Usar `poweroperation-mcp.exe` (64 bits);
   la carpeta `x86\` solo sirve si el `CtApi.dll` instalado fuera de 32 bits.
2. (Opcional) copiar `.env.example` como `.env` en la misma carpeta y ajustar. Sin `.env`, busca todo solo.
3. Verificar con el runtime de Power Operation corriendo:
   ```
   C:\MCP\poweroperation-mcp\poweroperation-mcp.exe --doctor
   ```
   Tiene que mostrar el `CtApi.dll` encontrado, `Conexión ... OK`, los proyectos, el estado del driver ODBC y si
   Graphics Builder está abierto.

## Registrar en el agente

**Claude Code**
```
claude mcp add poweroperation --scope user -- C:\MCP\poweroperation-mcp\poweroperation-mcp.exe
```

**Claude Desktop** (`%APPDATA%\Claude\claude_desktop_config.json`) / **Antigravity** (`mcp_config.json`):
```json
{
  "mcpServers": {
    "poweroperation": {
      "command": "C:\\MCP\\poweroperation-mcp\\poweroperation-mcp.exe",
      "args": [],
      "env": { "POWEROP_ALLOW_WRITE": "0" }
    }
  }
}
```
Las variables pueden ir en `env` o en el `.env` junto al exe.

## Configuración

Ver [.env.example](.env.example). Lo principal:

| Variable | Para qué |
|---|---|
| `POWEROP_CTAPI` | Carpeta o ruta de `CtApi.dll` si la búsqueda automática no lo encuentra (normalmente `...\Power Operation\v20xx\Bin\Bin (x64)`). |
| `POWEROP_HOST`, `POWEROP_USER`, `POWEROP_PASSWORD` | Conexión a un SCADA remoto (requiere `[CtAPI] Remote=1` en su citect.ini). |
| `POWEROP_PROJECTS_DIR`, `POWEROP_PROJECT` | Dónde están los proyectos y cuál usar por defecto. |
| `POWEROP_ALARM_HOST`, `POWEROP_ALARM_PORT` | Servidor de alarmas para `po_alarm_sql`. Vacíos = el del proyecto (tablas `almsrv`/`netaddr`) o `localhost:5482`. |
| `POWEROP_ALLOW_WRITE` | `1` habilita escrituras. |
| `POWEROP_GB_ALLOW_PAGES` | Páginas exactas de librerías de PO (`PLS_Include/!equipment1;...`) que `po_gb_page_edit` puede modificar. Vacío = ninguna. |

## Cómo funciona

- **Runtime → CtAPI**: carga `CtApi.dll` de la instalación (no se distribuye), agrega su carpeta al PATH y llama a
  `ctSetManagedBinDirectory` para que encuentre sus dependencias nativas y .NET. Conexión con `ctOpen` (reconexión
  automática), todo serializado en un hilo con tiempo máximo. Los errores se informan como "error Citect N" o "error Windows N".
  El error 233 justo después de un reinicio del cliente se reintenta una vez con reconexión (en escrituras solo se
  reconecta, nunca se repite la escritura).
- **Procesos → Windows**: `po_runtime_procs` lista los `Citect32.exe` y `RuntimeManager.exe` y deduce el rol de cada uno
  de los logs `syslog*.dat`; los errores salen de `tracelog.RuntimeManager.BackEnd.dat`. `po_runtime_screenshot` captura
  solo la ventana del cliente (`PrintWindow`). `po_runtime_control` con `start` ejecuta `Citect32.exe` sin argumentos (lee `Run=` del
  citect.ini); cambiar `Run=` exige confirmación y deja `citect.ini.<fecha>.bak`.
- **Alarmas → ODBC**: driver `Citect Alarm Driver`, consultas SQL a `CiAlarmObject` (estado) y `CDBEventJournal` (histórico).
- **PLC → Modbus TCP**: cliente propio, solo funciones de lectura (FC1 coils, FC2 entradas, FC3 holding, FC4 input);
  no hay ningún código de escritura Modbus. Rangos largos se parten en pedidos de 125 registros / 2000 bits.
- **Proyecto → archivos**: lee las tablas `.dbf` (variable, digalm, anaalm, trend, units, equip...) y los `.ci`
  directamente, incluyendo proyectos incluidos. Escribe `.ci` (con copia en `_mcp_backup`) y tablas DBF solo en
  `MCP_Demo` o en proyectos creados por el MCP (archivo `_mcp_created`), con respaldo y reconstrucción de los `.ndx`.
  `po_project_create` arma un proyecto vacío copiando el esquema de tablas y lo registra en `MASTER.DBF` (con respaldo).
  `po_project_compile` compila con `PowerSCADA.Common.dll` en PowerShell de 32 bits y devuelve los errores de
  `errlog.dbf`. Si el proyecto es el que corre, Runtime Manager a veces reinicia el runtime al recompilar (no siempre: mirar `po_runtime_procs`).
- **Páginas → Graphics Builder (COM)**: se conecta al Graphics Builder abierto (`GraphicsBuilder.Application`;
  `po_gb_status` con `launch=true` lo abre) y lo maneja por su interfaz de automatización: lee páginas y librerías,
  crea páginas desde un template con genies/símbolos/textos/botones (relleno, visibilidad y texto por expresión;
  colores `#RRGGBB`) y las edita; `replace=true` recrea una página existente. Los diálogos que bloquean a Graphics Builder
  se cierran solos y se informan como `warnings`, salvo el aviso de actualización de versión ("Cannot convert ... to
  version ..."), que se deja abierto porque aceptarlo cierra Graphics Builder. Antes de guardar encima o borrar una
  página se copia el `.ctg` a `_mcp_backup`. Las páginas nuevas se ven en runtime después de compilar.
- **Solo lectura por defecto**: `po_cicode_run` solo admite funciones de consulta (TagRead, *Browse*, Str*, Time*...) salvo
  con `POWEROP_ALLOW_WRITE=1`. Toda escritura se registra en `poweroperation-mcp-audit.log`.

## Problemas comunes

| Síntoma | Causa probable |
|---|---|
| `No se encontró CtApi.dll` | Indicar `POWEROP_CTAPI` con la carpeta `Bin\Bin (x64)`. |
| `No se pudo cargar ... el DLL es x86` | Usar el exe de la carpeta `x86\` o el DLL de `Bin (x64)`. |
| `No se pudo cargar ... faltan dependencias` / excepción 0xe0434352 | Apuntar al DLL dentro de la instalación (no copiarlo suelto). |
| `ctOpen falló` | El runtime no está corriendo, o es remoto sin `[CtAPI] Remote=1` / usuario. |
| "error Windows 233" en todas las llamadas | CtAPI se conecta al proceso **cliente** del runtime: no arrancó o se cayó (ver `Logs\syslog.dat`; sin página de inicio válida el cliente se cae al arrancar). Los servidores pueden estar corriendo igual. |
| Tabla/propiedad en `unavailable_properties` | El nombre de campo no existe en esa versión: probar otros con `po_ctapi_find`. |
| `po_alarm_sql`: "The system is offline" | El driver no llega al servidor de alarmas. Por defecto se usa el definido en el proyecto (`almsrv` + `netaddr`); las direcciones locales se pasan a `localhost` porque PO 2024 escucha solo en IPv6 (`::1`). El mensaje dice si el puerto TCP está abierto o cerrado; si está cerrado, buscar en qué host/puerto escucha (ver abajo) y ajustar `POWEROP_ALARM_HOST` / `POWEROP_ALARM_PORT`. |
| `po_gb_*`: "Graphics Builder no está abierto" | Abrirlo desde Power Operation Studio o llamar `po_gb_status` con `launch=true`. |
| Graphics Builder muestra "Cannot convert '<proyecto>' to version ..." | Al proyecto le falta `GLOBAL.CTL` (Graphics Builder lo usa para saber la versión del proyecto). Copiar un `GLOBAL.CTL` vacío de otro proyecto (p. ej. `User\Library_Controls\GLOBAL.CTL`, 352 bytes) a la carpeta del proyecto. No aceptar el aviso: cierra Graphics Builder (las herramientas `po_gb_*` funcionan con el aviso abierto). `po_gb_status` lista los proyectos sin ese archivo. |
| `po_gb_*` termina por tiempo máximo | Graphics Builder quedó con un diálogo o ventana que el MCP no cierra (p. ej. un formulario de propiedades): cerrarlo a mano y repetir. |
| Ningún proyecto `[RUN]` | Normal en PO 2024 (`Run=` vacío en `Config\citect.ini`): el MCP le pregunta al runtime con `ProjectInfo(0)` (en PO 2024 `ProjectCurrentGet()` da error 292). Los `citect.ini` dentro de carpetas de proyecto son copias y se ignoran. |

Para ver en qué puertos escucha el runtime (correr en PowerShell en el servidor SCADA):
```
Get-NetTCPConnection -State Listen | Select-Object LocalAddress, LocalPort, @{n='Proceso';e={(Get-Process -Id $_.OwningProcess).ProcessName}} | Sort-Object Proceso, LocalPort | Format-Table
```

## Desarrollo

```
cargo build --release                                   # target\release\poweroperation-mcp.exe (x64)
cargo build --release --target i686-pc-windows-msvc     # versión x86
cargo test                                              # unitarios + e2e por stdio (tests\offline.rs, tests\runtime.rs)
cargo test --test live -- --ignored --nocapture         # contra un runtime real (proyecto MCP_Demo)
```
Requiere la toolchain MSVC (Visual Studio Build Tools con C++). El CRT se enlaza estático (`.cargo/config.toml`):
el exe no necesita el redistribuible de Visual C++. `tests\runtime.rs` compila y usa un `CtApi.dll` falso
(`tests/fakectapi`) que imita el comportamiento verificado de PO 2024; `tests\live.rs` verifica las herramientas contra el runtime y el
Graphics Builder reales (`gb_write` crea y edita la página `MCP_GB_Test` de MCP_Demo). El entorno de pruebas está
descrito en `AGENT.md` (local, no se publica).

### Release

```powershell
.\scripts\release.ps1                  # cargo test + build x64 y x86 + dist\poweroperation-mcp-<versión>.zip (+ .sha256)
.\scripts\release.ps1 -Publish         # además crea el tag v<versión> y el release en GitHub (requiere gh)
.\scripts\release.ps1 -Publish -Draft  # el release queda como borrador
```
La versión sale de `Cargo.toml`. Con `-Publish` exige el árbol limpio y la rama ya subida, y no pisa un release
existente. Si Rust no encuentra el linker de Visual Studio (VS sin registrar en vswhere), el script carga el
entorno desde `vcvarsall.bat` por su cuenta.
