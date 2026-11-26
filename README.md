# poweroperation-mcp

Servidor MCP (stdio) para **EcoStruxure Power Operation** (basado en AVEVA Plant SCADA / Citect).
Es un único `.exe` sin instalador ni dependencias: usa el `CtApi.dll` de la instalación de Power Operation
que ya está en la máquina.

## Qué puede hacer el agente

| Área | Herramientas | Necesita |
|---|---|---|
| Diagnóstico | `runtime_status`, `poweroperation_reference` | — |
| Tags en vivo | `tag_read`, `tag_browse`, `tag_write`* | runtime corriendo (CtAPI) |
| Cicode en runtime | `cicode_run` (solo lectura salvo *) | runtime |
| Tendencias | `trend_query` (JSON/CSV + mín/máx/prom/desvío) | runtime |
| Alarmas | `alarm_list`, `alarm_ack`*, `alarm_sql` (histórico/journal por SQL) | runtime / driver ODBC |
| Acceso genérico | `ctapi_find` (cualquier tabla CtAPI, TRNQUERY...) | runtime |
| Proyecto offline | `project_list`, `project_tables`, `project_query`, `project_search` | solo los archivos |
| Cicode offline | `cicode_files`, `cicode_read`, `cicode_write`*, `cicode_check` | solo los archivos |

\* Escrituras: deshabilitadas por defecto (`POWEROP_ALLOW_WRITE=1` para habilitarlas). Quedan en el log de auditoría.

## Instalación en la VM

1. Copiar la carpeta (p. ej. a `C:\MCP\poweroperation-mcp\`). Usar `poweroperation-mcp.exe` (64 bits);
   la carpeta `x86\` solo sirve si el `CtApi.dll` instalado fuera de 32 bits.
2. (Opcional) copiar `.env.example` como `.env` en la misma carpeta y ajustar. Sin `.env`, busca todo solo.
3. Verificar con el runtime de Power Operation corriendo:
   ```
   C:\MCP\poweroperation-mcp\poweroperation-mcp.exe --doctor
   ```
   Tiene que mostrar el `CtApi.dll` encontrado, `Conexión ... OK`, los proyectos y el estado del driver ODBC.

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
| `POWEROP_ALARM_HOST`, `POWEROP_ALARM_PORT` | Servidor de alarmas para `alarm_sql` (por defecto 127.0.0.1:5482). |
| `POWEROP_ALLOW_WRITE` | `1` habilita escrituras. |

## Cómo funciona

- **Runtime → CtAPI**: carga `CtApi.dll` de la instalación (no se distribuye), agrega su carpeta al PATH y llama a
  `ctSetManagedBinDirectory` para que encuentre sus dependencias nativas y .NET. Conexión con `ctOpen` (reconexión
  automática), todo serializado en un hilo con tiempo máximo. Los errores se informan como "error Citect N" o "error Windows N".
- **Alarmas → ODBC**: driver `Citect Alarm Driver`, consultas SQL a `CiAlarmObject` (estado) y `CDBEventJournal` (histórico).
- **Proyecto → archivos**: lee las tablas `.dbf` (variable, digalm, anaalm, trend, units, equip...) y los `.ci`
  directamente, incluyendo proyectos incluidos. No modifica DBF; solo puede escribir `.ci` (con copia en `_mcp_backup`).
  Después de cambiar Cicode hay que **compilar en Power Operation Studio**.
- **Solo lectura por defecto**: `cicode_run` solo admite funciones de consulta (TagRead, *Browse*, Str*, Time*...) salvo
  con `POWEROP_ALLOW_WRITE=1`. Toda escritura se registra en `poweroperation-mcp-audit.log`.

## Problemas comunes

| Síntoma | Causa probable |
|---|---|
| `No se encontró CtApi.dll` | Indicar `POWEROP_CTAPI` con la carpeta `Bin\Bin (x64)`. |
| `No se pudo cargar ... el DLL es x86` | Usar el exe de la carpeta `x86\` o el DLL de `Bin (x64)`. |
| `No se pudo cargar ... faltan dependencias` / excepción 0xe0434352 | Apuntar al DLL dentro de la instalación (no copiarlo suelto). |
| `ctOpen falló` | El runtime no está corriendo, o es remoto sin `[CtAPI] Remote=1` / usuario. |
| Tabla/propiedad en `unavailable_properties` | El nombre de campo no existe en esa versión: probar otros con `ctapi_find`. |
| `alarm_sql`: "The system is offline" | El driver no llega al servidor de alarmas. El mensaje dice si el puerto TCP está abierto o cerrado. Si está cerrado, buscar en qué host/puerto escucha (ver abajo) y ajustar `POWEROP_ALARM_HOST` / `POWEROP_ALARM_PORT`. |
| Ningún proyecto `[RUN]` | Normal en PO 2024 (`Run=` vacío en `Config\citect.ini`): el MCP le pregunta al runtime con `ProjectCurrentGet()`. Los `citect.ini` dentro de carpetas de proyecto son copias y se ignoran. |

Para ver en qué puertos escucha el runtime (correr en PowerShell en el servidor SCADA):
```
Get-NetTCPConnection -State Listen | Select-Object LocalAddress, LocalPort, @{n='Proceso';e={(Get-Process -Id $_.OwningProcess).ProcessName}} | Sort-Object Proceso, LocalPort | Format-Table
```

## Desarrollo

```
cargo build --release                                   # target\release\poweroperation-mcp.exe (x64)
cargo build --release --target i686-pc-windows-msvc     # versión x86
cargo test
```
El CRT se enlaza estático (`.cargo/config.toml`): el exe no necesita el redistribuible de Visual C++.
El camino CtAPI se probó contra un `CtApi.dll` falso con las mismas firmas (ver `tests/fakectapi`).
