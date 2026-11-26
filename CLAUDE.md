# Power Operation MCP

Servidor MCP (Rust + rmcp 3.5, stdio) para EcoStruxure Power Operation. Mismo stack y estilo que `..\ignition-mcp`.
Se entrega como exe (`dist\poweroperation-mcp-<versión>.zip`) para usar en una VM con Power Operation instalado.

## Contexto importante

- **Power Operation NO está instalado en esta máquina.** El camino CtAPI se prueba con `tests\fakectapi`
  (un `CtApi.dll` falso con las mismas firmas); el proyecto offline, con DBF falsos generados por `tests\e2e\offline.py`.
- Firmas CtAPI, `DBTYPE_STR=129`, flags de `ctOpen`, sintaxis/propiedades de `TRNQUERY`, `AlarmAckTag` y el
  ODBC "Citect Alarm Driver" (tablas `CiAlarmObject`, `CDBEventJournal`) están verificados contra el wrapper
  open source github.com/estradege/citect y la doc de AVEVA.
- **Sin verificar contra un PO real**: nombres de propiedades de la tabla `Alarm` y `Tag` en `ctFindFirst`
  (por eso se informan `unavailable_properties`), que el driver ODBC venga con PO, y la convención de nombres
  de tags `Dispositivo\NodoLógico\Atributo`. Ajustar con lo que reporte el compañero desde la VM.

## Comandos

```
cargo build --release && cargo build --release --target i686-pc-windows-msvc
cargo test
cd tests\fakectapi && cargo build --release && cd ..\..
python -X utf8 tests\e2e\offline.py && python -X utf8 tests\e2e\runtime.py
```

Para regenerar `dist\`: copiar ambos exe + README.md + .env.example y comprimir (ver historial de la sesión inicial).

## Reglas

- Escrituras (tag_write, alarm_ack, cicode_run con efectos, cicode_write) siempre detrás de `POWEROP_ALLOW_WRITE=1`
  y auditadas. Nunca escribir DBF (Studio los gestiona; editar `.ci` sí, con backup).
- stdout es el canal MCP: logs solo a stderr.
- `src\tools\reference.md` es lo que lee el agente: mantenerlo correcto y marcar lo no verificado.
