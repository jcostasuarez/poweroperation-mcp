# Referencia rápida: Power Operation para agentes

EcoStruxure Power Operation (PO) corre sobre **AVEVA Plant SCADA** (ex Citect SCADA), con librerías propias
(proyectos incluidos `PLS_*`, Profile Editor, I/O Device Manager). Todo lo de Citect/Plant SCADA aplica:
Cicode, tablas DBF de proyecto, CtAPI, servidores de I/O, alarmas, tendencias y reportes.

Lo marcado **(verificado)** se probó contra PO 2024 (Plant SCADA 8.50) con un runtime real. Lo marcado
**(no verificado)** está implementado según la documentación o pruebas con datos falsos: confirmá el resultado.

Si los argumentos de una herramienta están mal (campo con otro nombre, falta uno obligatorio, tipo equivocado), el
error lista los campos válidos (obligatorios marcados), sugiere el nombre correcto y trae el esquema de entrada.
Los campos desconocidos se rechazan sin ejecutar nada, también dentro de `ops` de `po_gb_page_create`/`po_gb_page_edit`
(y de su `fill`/`colours`): el error dice dónde está (p. ej. `ops[1].fill`) y qué campos valen ahí.

## 0. Puentes: qué herramientas hay según lo disponible

Todas las herramientas se llaman `po_*`. Cada una depende de uno o más **puentes**; se lista solo si están
disponibles todos. `po_status` vuelve a sondear y muestra cada puente (DISPONIBLE / NO DISPONIBLE, motivo y qué
herramientas habilita u oculta). Si se llama a una oculta, no se ejecuta nada y el error dice qué puente falta y por qué.

| Puente | Disponible cuando | Herramientas |
|---|---|---|
| (siempre) | — | `po_status`, `po_reference` |
| DISCO | siempre (archivos del proyecto) | `po_project_list/tables/query/search/graphics`, `po_cicode_files/read/check`, `po_oneline_read/validate` |
| PROCESOS | siempre (Windows) | `po_runtime_procs`, `po_runtime_screenshot`, `po_studio_project` |
| CTAPI | CtApi.dll encontrado y el cliente del runtime responde (`Time(1)` en ≤10 s) | `po_tag_read`, `po_tag_browse`, `po_ctapi_find`, `po_trend_query`, `po_alarm_list`, `po_cicode_run` |
| ODBC | driver "Citect Alarm Driver" instalado y puerto del servidor de alarmas abierto | `po_alarm_sql` |
| GB | Graphics Builder registrado como servidor COM (no lo abre) | `po_gb_status/library/page_read/page_thumbnail/genie_read` |
| STUDIO | `PowerSCADA.Common.dll` y PowerShell de 32 bits encontrados | (con ESCRITURA) `po_project_compile`, `po_studio_set_project` |
| MODBUS | `POWEROP_MODBUS` configurado y puerto TCP abierto | `po_plc_read` |
| ESCRITURA | `POWEROP_ALLOW_WRITE=1` | combinada con otro puente: `po_tag_write`, `po_alarm_ack` (CTAPI); `po_cicode_write`, `po_project_create`, `po_project_unregister`, `po_project_rows_add/update`, `po_oneline_clone` (DISCO; con `apply=true` usa Graphics Builder); `po_runtime_control` (PROCESOS); `po_gb_page_create/edit/delete`, `po_gb_genie_create/edit/delete` (GB); `po_project_compile` (STUDIO) |

- CTAPI, ODBC y MODBUS se vuelven a sondear cada `POWEROP_PROBE_SECS` (30 s por defecto; 0 = nunca) y al llamar a
  una herramienta oculta por ellos. Cuando cambia la lista, el servidor manda `notifications/tools/list_changed`.
  **Verificado el 2026-10-15** con `po_runtime_control start` (Transferencia_Demo) y un sondeo cada 10 s:
  runtime arrancado en 20 s; a los ~34 s del arranque ODBC pasó a disponible (aviso) y enseguida CTAPI (otro
  aviso); el cliente CtAPI responde recién después de que el cliente lanza sus procesos CEF (~20 s). Con el runtime
  detenido CTAPI dice "runtime detenido" y ODBC "puerto cerrado". **(no verificado que Claude Code refresque la lista
  con el aviso: si no aparecen las herramientas, `po_status` o reiniciar el MCP)**.
- Con el runtime detenido CTAPI no está: `po_runtime_procs` muestra los procesos y `po_runtime_control` (escritura)
  lo arranca. Graphics Builder registrado no significa abierto: `po_gb_status` lo abre o se conecta.
- Nombres viejos (`tag_read`, `runtime_status`...) responden con el nombre nuevo. Fusionadas: `tag_watch` →
  `po_tag_read` con `samples`/`interval_secs`; `runtime_start`/`runtime_restart` → `po_runtime_control`
  (`action`); `page_display` → `po_cicode_run` con `PageDisplay("<página>")` o `refresh_page` de `po_project_compile`;
  `gb_library {}` → `po_project_graphics`.

## 1. Proyecto en disco (herramientas offline)

- Carpeta típica: `C:\ProgramData\Schneider Electric\Power Operation\v20xx\User\<Proyecto>\`. Los proyectos
  registrados están en `User\MASTER.DBF` (verificado en PO 2024: NAME, TITLE, PATH terminado en `\`, MAJOR_VER,
  MINOR_VER, DATE, TIME, PRIVILEGE, STATUS; filas borradas marcadas con `*`). `po_project_list` usa MASTER como
  autoridad: las carpetas con forma de proyecto que no figuran ahí salen en `unregistered_folders` y los respaldos
  `_mcp_backup` se ignoran. El `citect.ini` de `...\Config\` indica `[CtEdit] User=` y `Run=`
  (carpeta completa del proyecto en ejecución; en PO 2024 puede estar vacío hasta que se arranca un proyecto).
  La carpeta de `Run=` **siempre** entra en `po_project_list` con `running: true`, aunque no figure en MASTER.DBF ni
  esté bajo `User` (verificado: un proyecto en ejecución puede no estar registrado).
- Un proyecto PO **incluye** otros (include.dbf): las librerías de PO (`PLS_Include`...) y a veces proyectos
  propios. El proyecto de sistema `Include` se incluye siempre. Usá `include_included: true` para buscar en todos.
- La configuración está en tablas **dBase (.dbf)**. Las más usadas (verificá los campos con `po_project_tables`):

| Tabla | Contenido | Campos habituales |
|---|---|---|
| `variable` | Variable tags | NAME (el tag), TYPE, UNIT (I/O device), ADDR, ENG_ZERO/ENG_FULL, ENG_UNITS, FORMAT, COMMENT, CLUSTER, EQUIP, ITEM |
| `digalm` | Alarmas digitales | TAG, NAME, DESC, VAR_A, VAR_B, CATEGORY, AREA, PRIV, HELP, EQUIP, ITEM |
| `anaalm` | Alarmas analógicas | TAG, NAME, VAR, HIGHHIGH, HIGH, LOW, LOWLOW, DEADBAND, CATEGORY |
| `advalm` | Alarmas avanzadas (expresión) | TAG, NAME, DESC, EXPR, CATEGORY |
| `tsdig`, `tsana` | Alarmas con timestamp (SOE) | TAG, NAME, DESC, VAR... |
| `category` | Categorías de alarma | CATEGORY, PRIORITY, colores, LOG_*, SHOW_* |
| `trend` | Trend tags | NAME, EXPR, SAMPLEPER (hh:mm:ss), TYPE (TRN_PERIODIC...), FILENAME, STORMETHOD, ENG_UNITS |
| `units` | Dispositivos de I/O | SERVER, NAME, NUMBER, ADDR, PROTOCOL, PORT, MEMORY, PERSIST |
| `ports`, `boards` | Puertos y placas de comunicación | |
| `equip` | Modelo de equipos | NAME (jerárquico con puntos), TYPE, IODEVICE, TAGPREFIX, CLUSTER |
| `cluster`, `netaddr`, `computer` | Red: clusters, direcciones (Address = IP) y equipos | netaddr: NAME, Address, COMMENT, COMPUTER; computer: NAME, COMMENT, DNSNAME (verificado) |
| `almsrv`, `trnsrv`, `repsrv`, `scanner` | Servidores de alarmas, tendencias, reportes y **I/O** (`scanner`) | NAME, MODE (vacío = Primary), CLUSTER, NETADDR, PORT; almsrv: DBPORT, PUBPORT; trnsrv/repsrv: PFPORT; scanner: PEERPORT (verificado). Puertos por defecto: alarmas 2080, BD 5482, I/O 2082, reportes 2084, tendencias 2085 |
| `pages` | Propiedades de páginas gráficas | |
| `include` | Proyectos incluidos | NAME |

- Puertos por defecto de los servidores (verificado): alarmas 2080 y base de datos de alarmas 5482, I/O 2082,
  reportes 2084, tendencias 2085.
- Las **páginas gráficas** (.ctg) y los genies son binarios: se editan solo a través de Graphics Builder (sección 5).
- **Nombres de tags en PO (verificado en los proyectos de ejemplo):** `<DispositivoIO>\<NodoLógico>\<Atributo>[\sub]`
  al estilo IEC 61850, p. ej. `S33K_X_GEN1\MMXU1\A\phsA` (corriente fase A), `...\MMXU1\Hz`,
  `...\XCBR1\Pos\ZCBCls\dchg` (cierre de interruptor), `...\LPHD1\EEHealth`. El prefijo es el nombre del
  dispositivo de I/O. La alarma suele llamarse igual que el tag que vigila y el trend igual que el tag que registra.
  Confirmalo con `po_project_query` sobre `variable` (campo NAME) antes de asumir nombres.
- Campos de contraseña (los que tienen PASS, PWD o SEED en el nombre, p. ej. PASSWORD y SEED de `users`):
  `po_project_query` muestra `***` en vez del hash (vacío si está vacío), no admite `where` sobre ellos y el texto libre
  no los mira; `po_project_search` tampoco los busca.
- Categorías de alarma de las librerías PLS: `_PLSALM_HIGH`, `_PLSALM_LOW`, `_PLSALM_EVENT`...
- Cicode: archivos `*.ci` en la carpeta del proyecto. Tras modificar algo **hay que compilar** (`po_project_compile`
  o Power Operation Studio). Los errores de compilación quedan en `<proyecto>\errlog.dbf` (E_TYPE, E_REC, E_PAGE,
  E_FIELD, E_TABLE, E_ERROR, E_CONTEXT, E_TAG; verificado leyendo proyectos reales).

### Crear, escribir y compilar proyectos (escrituras)

- `po_project_create`: proyecto vacío en la carpeta User con todas las tablas DBF vacías copiadas del esquema de otro
  proyecto (por defecto `MCP_Demo`), sus `.ndx`, `GLOBAL.CTL` válido, `include.dbf` (por defecto `PLS_Include`) y
  una fila en `MASTER.DBF` (con respaldo previo) y `PROJECT.CIT` (las propiedades que lee Studio: con el nombre, la
  revisión y la fecha; ver "Proyecto activo de Studio"). Lo marca con el archivo `_mcp_created`. Rechaza nombres que
  ya existan. `parent_dir` crea el proyecto en otra carpeta (fuera de `User\`; la fila de `MASTER.DBF` apunta ahí).
  **(verificado el 2026-10-16 con `Planta_Liquidos`, creado con `parent_dir` en `Desktop\prueba`: Studio lo lista y,
  con el `PROJECT.CIT` que escribe `write_project_cit` (agregado por `po_studio_set_project`, la misma función que usa
  esta herramienta), muestra revisión y fecha y lo abre como activo tras reiniciar Studio. Sin `PROJECT.CIT` lo lista
  sin revisión ni fecha. Desde `po_project_create`: verificado el 2026-11-27, `PROJECT.CIT OK` con GUID nuevo, y
  Graphics Builder lo lista y guarda páginas ahí después de reabrirlo.)** **Nunca**
  restaurar backups `.ctz` para crear proyectos. **(verificado: `MCP_Create_Test` abre en Studio y en Graphics Builder
  y compila)**
- `po_project_unregister`: quita proyectos del registro marcando sus filas de `MASTER.DBF` como borradas (`*`, como
  dBase; con respaldo) y reconstruye `master.ndx`. No borra carpetas. Rechaza sistema/librería, proyectos en ejecución
  y proyectos incluidos por otro que queda registrado; si algo falla no escribe nada. Si se quita `MCP_Demo`,
  `po_project_create` necesita `schema_from` explícito.
- `po_project_rows_add` / `po_project_rows_update`: escriben filas en una tabla DBF (tags en `variable`, dispositivos
  de memoria en `units`, alarmas digitales en `digalm`, usuarios/roles...) con respaldo en `_mcp_backup` y
  reconstrucción de los `.ndx` de la tabla. Solo en `MCP_Demo` o en proyectos con `_mcp_created`; nunca en
  proyectos de sistema/librería. **Nunca escribas contraseñas**: los campos con PASS/PWD/SEED se rechazan; los
  usuarios se crean sin contraseña y la cargan en Studio. Después hay que compilar. **(verificado** en `MCP_Create_Test`:
  cluster, netaddr, computer, servidores, units, variable, equip, roles, users...; el compilador las acepta)**. Un
  proyecto mínimo compilable necesita al menos un usuario (si no, E2117) y los `equip` que nombren los tags (E2123).
- Índices `.ndx` (dBase III): los que se llaman como la tabla. La clave es un campo C o D (NAME, SERVER, PAGE...)
  y el índice incluye los registros borrados. **Verificado:** los `.ndx` de MCP_Demo (variable, units, computer,
  pglang, locvar) se reconstruyen byte a byte; en otros (pgbutton, pgnum...) coinciden claves y orden pero no la
  forma del árbol. **No verificado:** que Studio/runtime acepten índices reconstruidos de tablas grandes.
- `po_project_compile`: compila con `CompileProject` de `bin\PowerSCADA.Common.dll` (PowerShell de 32 bits) y
  devuelve los errores y advertencias de `errlog.dbf` (solo si se reescribió durante la compilación). **Verificado:**
  con 0 errores y 0 advertencias el compilador **no** reescribe `errlog.dbf` (salvo que tuviera contenido: entonces lo
  deja vacío); `compilersummary.xml` sí se reescribe siempre, y se usa como señal de "compiló" (`summary_updated`).
  El compilador actualiza la fila del proyecto en `MASTER.DBF` (MINOR_VER, DATE, TIME). No usa
  `RunProject` (reescribe `Run=` con una ruta equivocada para proyectos fuera de User). Si el proyecto es el que
  corre, **Runtime Manager puede reiniciar el runtime al recompilar** (todos los Citect32 cambian de PID y el
  cliente se reinicia; no siempre pasa, ver sección 3): confirmalo con el usuario. Con `refresh_page` muestra esa página en el cliente al terminar.
  **(verificado** con errores, solo advertencias y limpio)**.
- Protocolo `GENERIC` (dispositivos en memoria o disco): direcciones `D<n>` digital, `I<n>` INT, `R<n>` REAL,
  `L<n>` LONG, `S<n>` STRING (verificado, `bin\GENERIC.DBF`).

### Proyecto activo de Studio (`po_studio_project`, `po_studio_set_project`)

- **Cómo lo guarda Studio:** `[CtEdit] LASTDATABASE` / `LASTDATABASEPATH` del citect.ini. Studio solo los lee al abrirse;
  con Studio abierto el cambio pasa por dentro ("Make Active" de la vista Projects → ConfigProvider
  `Project.SetActive` → `ctExplor.dll`). No hay API externa.
- **`po_studio_project`** (solo lectura): PID y título de la ventana de Studio, proyecto activo que muestra, y
  LASTDATABASE, LASTDATABASEPATH, LASTSETUPPATH y `Run=`, con `notes` si no coinciden. **(verificado** el 2026-10-16
  con Studio abierto: `studio_active_project` = `Transferencia_Demo`, `notes` vacío)**.
- **`po_studio_set_project`** (escritura, pide confirmación): `{project, restart_studio, wait_secs}`.
  - **Studio y Graphics Builder abiertos (camino en caliente, `"via": "graphics_builder"`):** respalda el citect.ini
    (`citect.ini.<fecha>.bak`) y hace `ProjectSelect(proyecto)` por COM de Graphics Builder, que cambia también el
    activo de Studio; espera hasta 20 s a que el título de Studio lo muestre. **Verificado el 2026-10-16**
    (Transferencia_Ejemplo → MCP_Demo → Transferencia_Ejemplo, ~370 ms la llamada, ~200 ms el `ProjectSelect`): título,
    `AID_ActiveProjectTextBlock`, insignia "Active Project" de la vista Projects, árbol de librerías de Visualization,
    grilla de System Model → Variables (los 7 tags de MCP_Demo) y Compile Messages (los del último compilado del
    proyecto nuevo) cambian solos; `LASTDATABASE`/`LASTDATABASEPATH` los escribe Studio; `Run=` no cambia (estaba vacío
    y siguió vacío). **No verificado:** que el Compile/Run de Studio usen el proyecto nuevo (Invoke de UI Automation sobre
    `AID_CompileButton` no hace nada, como con "Make Active"; un clic del usuario no dejó compilado). Condiciones (si
    no se cumplen, error claro sin cambiar nada, o el reinicio con `restart_studio=true`):
    - Graphics Builder **sin páginas abiertas** (incluidas las `Untitled`): con páginas abiertas `ProjectSelect` no
      hace nada. El error lista las páginas para que el usuario las cierre (verificado el 2026-10-16 con dos abiertas).
      El MCP no las cierra.
    - Que Graphics Builder liste el proyecto (`ProjectFirst/Next`; lo carga al abrirse: un proyecto recién creado
      necesita reabrir Graphics Builder).
    - Que el proyecto tenga `GLOBAL.CTL` (si no, Graphics Builder muestra "Cannot convert" y aceptarlo lo cierra).
    - Si Graphics Builder ya tenía seleccionado el proyecto pedido (no siempre sigue a Studio), selecciona antes el que
      muestra Studio y después el pedido.
  - Studio abierto sin Graphics Builder: selecciona la fila del proyecto en la vista Projects con las flechas (mensajes de teclado a la
    ventana, **sin mover el mouse**) y hace Invoke de "Make Active". **Verificado:** la selección de `Planta_Liquidos`
    por teclado y que aparece su botón "Make Active". **No verificado (falla):** el cambio del activo. El 2026-10-16
    "Make Active" no tuvo efecto por ninguna vía: Invoke de UI Automation, Espacio o Intro sobre el botón enfocado,
    mensajes de clic a la ventana y clic real del mouse (con la ventana al frente). Devuelve `NO_CHANGE` y no toca
    el citect.ini. Clics por mensaje sobre la fila no la seleccionaban; las flechas sí. Sin un log de Studio no se
    sabe por qué la acción no corre.
  - `restart_studio=true`: cierra Studio con WM_CLOSE (nunca a la fuerza), escribe LASTDATABASE/LASTDATABASEPATH con
    respaldo `citect.ini.<fecha>.bak`, lo vuelve a abrir y espera su ventana. **Verificado el 2026-10-16:** Studio
    cerró en orden y reabrió como "Planta_Liquidos - Power Operation Studio" con el proyecto activo cambiado. Cierra
    los documentos abiertos de Studio: confirmalo antes.
  - Studio cerrado: escribe el citect.ini y, con `restart_studio=true`, lo abre. (Mismo camino de escritura y
    arranque que el caso verificado; el caso "Studio ya cerrado" en sí no se probó aparte.)
  - Studio se lanza con `CreateProcessW` sin heredar handles (`procs::spawn_detached`): si heredara los pipes stdio
    del MCP, el cliente se quedaría esperando EOF mientras Studio siga abierto (visto el 2026-10-16).
  - **Al abrir, Studio vació `Run=` del citect.ini** (2026-10-16). Después de cambiar el proyecto de Studio, poner
    `Run=` con `po_runtime_control` antes de arrancar el runtime.
  - Si al proyecto le falta `PROJECT.CIT` y tiene `_mcp_created`, lo crea. Si no tiene el marcador, rechaza. Rechaza
    proyectos de sistema y de librería. No cambia `Run=` (eso es `po_runtime_control`) ni `LASTSETUPPATH`.
- **Efecto secundario de `ctExplor.dll`:** cargarlo en otro proceso (p. ej. `Assembly.LoadFile`) hizo que Studio
  reescribiera `Run=` del citect.ini con su proyecto activo (visto el 2026-10-16 a las 06:41). El MCP nunca lo carga.
  No se sabe si el "Make Active" de Studio reescribe `Run=`: no se logró que cambiara el activo, así que no se vio.
- **Runtime Manager "Setup required":** si `LASTSETUPPATH` (último Computer Setup Wizard) no coincide con `Run=`,
  Runtime Manager muestra ese diálogo y el runtime no arranca. Lo resuelve el usuario corriendo el Setup Wizard; el
  MCP no lo toca. Con `Planta_Liquidos` (fuera de `User\`) el wizard mostró "ctcmp32 /I… "Planta_Liquidos" -O Cannot
  Find File" (también lanzado desde `bin`), pero igual escribió `LASTSETUPPATH` y el runtime arrancó (2026-10-16 08:12).
  Causa del error sin encontrar: `CtCmp32.exe` está en `bin` y compila ese proyecto bien a mano.
- `CtCmp32.exe` a mano reescribe `Run=`, `LASTDATABASE` y `LASTDATABASEPATH` con el proyecto que compila, sin
  respaldo (2026-10-16). El MCP compila con ProjectManager (`po_project_compile`), que no lo hace.
- **Pendiente:** el cambio del activo con Studio abierto sin Graphics Builder ("Make Active" en caliente no tiene
  efecto): abrir Graphics Builder desde Studio (sin páginas) o usar `restart_studio=true`. Verificar que Compile/Run de
  Studio usen el proyecto cambiado por Graphics Builder.

## 2. Cicode

```cicode
// Comentarios: //, ! o /* ... */
INT FUNCTION BreakerStatus(STRING sDevice)          // [PUBLIC|PRIVATE] [TIPO] FUNCTION Nombre(...)
    INT    iPos;
    STRING sTag = sDevice + "\XCBR1\Pos";
    iPos = StrToInt(TagRead(sTag));
    IF iPos = 2 THEN                                 // '=' asigna y compara; distinto: <>
        RETURN 1;
    ELSE
        RETURN 0;
    END
END
```

- Tipos: INT (32 bits), REAL, STRING, OBJECT, QUALITY, TIMESTAMP. Sin tipo de retorno = VOID.
  **LONG no es un tipo de Cicode** (verificado: el compilador lo toma como tag inexistente).
- Alcance de variables: `GLOBAL` (todo el proyecto), `MODULE` (el archivo), local (dentro de la función).
- Bloques: `IF ... THEN ... [ELSE ...] END`, `WHILE ... DO ... END`, `FOR i = 0 TO 9 DO ... END`,
  `SELECT CASE x` / `CASE 1` / `CASE 2, 3` / `CASE 4 TO 6` / `CASE IS > 10` / `CASE ELSE` / `END SELECT`,
  `REPEAT ... UNTIL cond`.
- Operadores: `+ - * / MOD`, `AND OR NOT`, `BITAND BITOR BITXOR`, `+` concatena cadenas.
- Escape en cadenas con `^`: `^"` comilla, `^n` salto, `^t` tab, `^^` circunflejo. La `\` no es escape.
- Tags con `\` en el nombre: en Cicode y en `po_cicode_run` no sirven como identificador (verificado: error
  Citect 292); usar `TagRead("...")` / `TagWrite("...", valor)`. En expresiones de configuración (EXPR de
  trends y alarmas avanzadas) sí se escriben directo.
- Funciones frecuentes: TagRead, TagWrite, TagGetProperty, IntToStr, RealToStr, StrToInt, StrToReal,
  StrLeft/StrMid/StrRight/StrLength/StrSearch, TimeCurrent, TimeToStr, Sleep, SleepMS, TaskNew,
  ErrSet/IsError, Message, PageDisplay, AlarmAckTag, AlarmEnableTag, AlarmDisableTag,
  TagBrowseOpen/First/Next/GetField/Close, AlmBrowseOpen/..., TrnInfo, ProjectInfo, ParameterGet.
- Las funciones bloqueantes (Sleep, I/O) deben ir en tareas (TaskNew) y no en animaciones de páginas.
- `po_cicode_check` hace solo un chequeo estructural; la validación real es compilar (`po_project_compile` o Studio).
- Dispositivos de memoria (I/O device en memoria, protocolo GENERIC): justo después de arrancar el IOServer
  no aceptan escrituras los primeros segundos. Código de arranque (`StartupCode`) que inicializa tags de memoria
  tiene que reintentar hasta leer el valor escrito (ver `TRF_SimLoop` en el proyecto de prueba
  Transferencia_Demo). **(verificado)**

## 3. Runtime vía CtAPI

- CtAPI se conecta al proceso **cliente** del runtime: si el cliente no arrancó o se cayó, todo da
  "error Windows 233" aunque los servidores estén corriendo.
- Proyecto en ejecución: `ProjectInfo(0)` (verificado; `ProjectCurrentGet()` da error 292 en PO 2024).
- `po_tag_read` / `po_tag_write`: valores en unidades de ingeniería, como texto. `Tag.Elemento` y `Tag[i]` admitidos.
  `po_tag_write` acepta un tag (`tag` + `value`) o un lote (`writes: [{tag, value}]`, hasta 50): el usuario confirma
  la lista completa, cada tag se escribe y relee por separado, hay una línea de auditoría por tag y si uno falla
  se siguen los demás e informa cada resultado.
- `po_tag_read` devuelve por tag `value`, `quality` y `timestamp` (hora local de la última actualización;
  `value_timestamp` si el valor cambió en otro momento). Calidad (ayuda: "The Quality Item", QualityGetPart):
  `GOOD`, `UNCERTAIN` o `BAD`, con el subestado si lo hay (`BAD/COMM_FAILURE`, `BAD/LAST_KNOWN_VALUE`,
  `BAD/WAITING_FOR_INITIAL_DATA`, `UNCERTAIN/LAST_USABLE_VALUE`, `GOOD/LOCAL_OVERRIDE`...). `quality_detail` aparece
  si hay algo más: `extended` (`COMM_DEV_BAD`, `COMM_IOSERVER_BAD`, `STALE`, `INVALID_TAG`, `NOT_REPLICATED`...),
  `limit` (LOW/HIGH/CONSTANT), `override` (el valor es el forzado, no el de campo), `control_inhibit` (escribir al
  campo está bloqueado), `datasource_error`. Un valor con calidad BAD **no es el valor real** del equipo: decirlo al
  usuario antes de sacar conclusiones. Los tags referenciados por ítem (`Tag.V`, `Tag.Field.T`) dan siempre GOOD y
  hora 0 (según la ayuda).
  Varios tags se leen en **un solo pedido** (ctListNew/ctListRead: todos del mismo instante); un tag que la lista no
  devuelve completo se relee solo (ctTagReadEx) para tener el error real. `details=true` agrega `units`,
  `description`, `eng_low`, `eng_high` y `type` (ctTagGetProperty). Un CtApi.dll sin estas funciones da solo `value`.
  Tag inexistente: ni ctTagReadEx ni la lista fallan (dan TRUE, valor vacío y `BAD` + `INVALID_TAG`); el MCP lo relee
  con ctTagRead y muestra su error, `error Citect 424 "Tag not found"`, igual que antes. **(verificado** contra el
  runtime de MCP_Demo el 2026-11-27: horas FILETIME en hora local con milisegundos, elementos numéricos de
  ctListItem desde la primera lectura, 4 tags en una lista con un solo releído (el inexistente), `details` con
  kW / rango 0–1000 / REAL**)**.
- `po_tag_read` con `samples` (hasta 120) e `interval_secs`: lee los mismos tags N veces cada X s (hasta 300 s en
  total) y resume por tag los valores distintos, cantidad de cambios y la hora del primer cambio. Sirve para
  verificar secuencias (p. ej. una transferencia) sin escribir nada. Cada muestra es una lectura por lista; si la
  calidad no es GOOD el valor sale como `"0 [BAD/COMM_FAILURE]"` (perder la comunicación cuenta como cambio).
  `po_tag_browse with_values=true` agrega `QUALITY` a cada fila.
- Error Citect/Windows **233** justo después de un reinicio del cliente es transitorio: en lecturas el MCP cierra la
  conexión, espera 1 s y reintenta una vez; si persiste, el error dice "el cliente se está reiniciando". En
  `po_tag_write` solo se reintenta la conexión, nunca la escritura (verificá con `po_tag_read`). **(verificado con un
  CtApi.dll falso, no tras un reinicio real del cliente)**. Con el runtime detenido el 233 persiste en todos los tags:
  mirá `po_runtime_procs`.
- Abrir o refrescar una página: `po_cicode_run` con `PageDisplay("<página>")` (escritura: cambia lo que ve el
  operador) o `refresh_page` de `po_project_compile`. Tras compilar, el cliente sigue mostrando la versión vieja de
  la página hasta volver a abrirla. ctCicode corre en el proceso cliente. **(verificado** contra el runtime real con
  `Unifilar`)**.
- Cliente al arrancar: abre en la página de `[Page] Startup` del citect.ini (en PO, `PageMenu`) y puede mostrar un
  error de `PLS_StartAdvOneLine()` (causa no verificada). `[Client] AutoLoginMode` intenta entrar con el usuario de
  Windows y da "invalid username or password" si no existe en el proyecto. **El agente nunca escribe contraseñas**:
  las carga el usuario en Studio. Con `AutoLoginMode = 5` (credenciales guardadas; el valor que dejó el Wizard) el
  cliente muestra "Log in to display client" y **Cancel cierra el cliente** (verificado el 2026-11-27, aunque la
  ayuda dice que solo el modo 4 cierra): Runtime Manager relanza los servidores pero no el cliente, y CtAPI da 233
  hasta que alguien entra.
- `po_cicode_run`: ejecuta en el runtime; los argumentos de cadena van entre comillas:
  `TagRead("Feeder1\MMXU1\TotW")`. El resultado vuelve como texto. En modo solo lectura solo se permiten
  funciones de consulta.
- `po_tag_browse` / `po_ctapi_find` con filtro `"CAMPO=valor"` y comodín `*` (p. ej. `TAG=Feeder1*`; varios
  campos separados por `;`). Las propiedades que no existen se informan en `unavailable_properties`.
  Tablas y propiedades (verificado):

| Tabla | Propiedades |
|---|---|
| `Tag` | TAG, TYPE (código: 0 DIGITAL, 1 INT, 2 REAL, 4 LONG, 7 STRING...; `po_tag_browse` agrega TYPE_NAME), CLUSTER, EQUIPMENT, ITEM, ADDR, ENG_UNITS, ENG_ZERO, ENG_FULL, RAW_ZERO, RAW_FULL, ARR_SIZE, FORMAT, COMMENT. **No** existen UNIT, EQUIP ni NAME. |
| `Alarm` | TAG, NAME, DESC, ALARMTYPE (Digital/Analog...), CATEGORY, PRIORITY, STATE, STATE_DESC, VALUE, HIGH, LOW, HIGHHIGH, LOWLOW, DEADBAND, TIME, DATE, AREA, EQUIPMENT, ALMCOMMENT, CLUSTER |
| `AlarmSummary` | TAG, NAME, SUMDESC, SUMSTATE, ONDATE, ONTIME, OFFDATE, OFFTIME, ACKDATE, ACKTIME, USERNAME, VALUE, COMMENT, CATEGORY, PRIORITY, EQUIPMENT (una fila por activación) |
| `Trend` | TAG (nombre del trend), EXPRESSION, SAMPLEPER (s), TYPE (1 periódico, 2 evento, 3 ambos), ENG_UNITS, COMMENT, CLUSTER, EQUIPMENT, STORMETHOD |
| `Equip` | NAME, CLUSTER, IODEVICE, COMMENT, PARENT, PAGE, CUSTOM1..8 |
| `DigAlm`, `AnaAlm`, `AdvAlm`, `LocalTag`, `Accum`, `Cluster` | según la ayuda de ctFindFirst (sin verificar) |

- STATE y las descripciones de estado llegan **traducidas al idioma del runtime** (p. ej. "ACTIVADO", "DESACT.",
  "ALTO"): no compararlas contra textos en inglés.
- Errores: "error Citect N" = código de Cicode/CtAPI, con su título en inglés de la tabla "Cicode and General
  Errors" de la ayuda (243 códigos; 1–31 son errores del dispositivo de I/O); "error Windows N" = sistema. Según la
  ayuda de CtAPI, un error Windows indica conexión perdida y uno Citect, un pedido inválido con la conexión sana.
  Frecuentes (verificado):
  424 tag no encontrado, 274 argumento inválido, 292 función inexistente (o identificador inválido),
  Windows 233 sin conexión con el cliente del runtime (transitorio tras un reinicio del cliente).

### Procesos del runtime (sin CtAPI)

- El runtime son varios `Citect32.exe` (uno por rol: IOServer, Alarm, Trend, Report y el Client) más
  `RuntimeManager.exe`. `po_runtime_procs` (y la cantidad en `po_status`) muestra por proceso PID, rol, hora de
  arranque, si responde y el título de ventana (en el cliente, la página mostrada). Si todos los PID cambiaron
  a la vez, el runtime se reinició.
- Rol: de la línea `Runtime (32 bit) (<Rol> [<cluster>.<servidor>]) (PID n) Startup` de `Logs\syslog.<Rol>.*.dat`;
  el cliente escribe `Runtime (32 bit)  (PID n) Startup` sin corchetes en `syslog.dat`. Líneas de comandos reales:
  `/R[C:Client] /C:0,1`, `/R[A:c1.AlarmServer1]`, `/R[T:…]`, `/R[R:…]`, `/R[I:c1.IOServer1]`. Unos 15 s después de
  arrancar, el cliente lanza Citect32 hijos de CEF (navegador embebido: `--type=gpu-process`, `--type=utility`...),
  que se muestran como `Client-CEF (<tipo>)`. **(verificado con el runtime corriendo)**. Logs en `C:\ProgramData\Schneider Electric\Power Operation\v20xx\Logs\`.
- Errores de Runtime Manager: `tracelog.RuntimeManager.BackEnd.dat` (separado por tabs, nivel en la 5.ª columna).
  `HangMonitor::timerElapsed() PID=<n>:C:Client; State=Not Responding` = el cliente no responde. **(verificado)**
- `po_runtime_screenshot`: PNG solo de la ventana del cliente, aunque otra ventana la tape (no si está minimizada).
  **(verificado** con PageMenu y Unifilar)**. El PNG va comprimido (deflate + filtro por fila; una pantalla sintética
  de 900×675 queda en ~13 KB y la abre System.Drawing; antes iba sin comprimir, ~2 MB). **(compresión no verificada
  sobre una captura real del cliente)**.
- `po_runtime_control` (escritura), `action`:
  - `start`: `Citect32.exe` sin argumentos (como el acceso "Power Operation Runtime"), que lee `[CtEdit] Run=` del
    citect.ini. `RuntimeManager.exe /r` sale enseguida y no sirve. **(verificado, también con cambio de `Run=`)**
  - `restart`: WM_CLOSE a los Citect32 y espera; con `force=true` los termina (TerminateProcess) y espera hasta 45 s
    a que salgan. Runtime Manager suele relanzarlos solo: si aparecen PIDs nuevos (espera hasta 20 s) no lanza otro.
    El 2026-10-15, con `force=true`, a los 31 s seguían 3 de los PIDs terminados y la versión anterior lo informó
    como fallo; no se confirmó si salían lento o si Runtime Manager intervino. **(la espera de 45 s y la detección de
    relanzamiento no están verificadas en vivo: confirmá con `po_runtime_procs`)**.
  - Cambiar `Run=` (`project_path`) exige el OK del usuario (`confirm_ini_change=true`) y deja respaldo
    `citect.ini.<yyyyMMdd-HHmmss>.bak`.
- Citect32 sin argumentos delega en Runtime Manager. Si el equipo quedó configurado para otro proyecto, Runtime
  Manager muestra **"Setup required"** (pide correr el Computer Setup Wizard) y no arranca nada: `start` lo
  detecta por el título de la ventana, deja de esperar y lo explica. El Wizard lo corre el usuario (o desactiva el
  chequeo en las opciones de Studio). **El Wizard reescribe el citect.ini y puede volver a poner `Run=` en otro
  proyecto** (verificado): revisá `Run=` después. `po_tag_read` con `samples` y `po_tag_write` en lote, verificados
  con el runtime real.
  En la página "Project Setup" (Local Engineering Environment) el Wizard **no lista todos los proyectos**: el
  2026-11-27 MCP_Demo no aparecía (su compilación era del 14/10 y tenía páginas cambiadas después) y apareció tras
  compilarlo con `po_project_compile`. Si el proyecto no está en la lista, compilarlo primero. Criterio exacto no
  verificado (Transferencia_Ejemplo, compilado al día, tampoco aparecía).
- `Shutdown()` por `po_cicode_run` devolvió 274 y no hizo nada **(verificado)**. Al recompilar el proyecto que corre,
  Runtime Manager **a veces** reinicia el runtime solo (todos los Citect32 con PID nuevo; visto el 2026-10-14). El
  2026-10-15 no lo hizo en tres compilaciones de `Transferencia_Demo` corriendo, con y sin Studio abierto (misma PID).
  Igual se vieron los cambios de la página con `refresh_page`. No supongas que hubo reinicio: mirá `po_runtime_procs`.
  Con Studio cerrado, la compilación tardó ~240 s en vez de ~6 s.

### Tendencias (TRNQUERY)

`po_trend_query` arma `TRNQUERY,Endtime,EndtimeMs,Period,NumSamples,Tagname,DisplayMode,DataMode`
(Endtime en segundos UTC desde 1970). Propiedades: DATETIME, MSECONDS, VALUE, QUALITY.

- **Verificado:** en PO 2024 la forma larga con `InstantTrend=0,SamplePeriod=0` al final da error 274; la de
  7 argumentos funciona. El inicio es `Endtime − Period × (NumSamples − 1)` y el servidor puede devolver hasta
  2 muestras extra (antes y después del rango).
- Las muestras llegan de la más nueva a la más vieja; `po_trend_query` las ordena cronológicamente.
- DataMode: 0 = una muestra por período, interpolando huecos; 1 = timestamps reales, sin interpolar.
- DisplayMode = suma de: orden + condensado (0 promedio, 4 mínimo, 8 máximo, 12 más reciente) + estiramiento
  (0 escalón, 128 proporcional, 256 crudo) + relleno de huecos (n × 4096) + calidad mala (0 = cero,
  2097152 = último valor válido) + 4194304 = datos totalmente crudos (solo con DataMode 1).
- QUALITY: bits 0-3 tipo (0 sin muestra, 1 interpolado, 2/3 crudo), bits 4-7 calidad (1 = buena),
  bit 12 = muestra parcial (período en curso).
- El nombre es el del **trend tag** (tabla `trend`, campo NAME; en CtAPI `Trend.TAG`); en PO suele coincidir
  con el variable tag. Para saber si un trend existe: `po_cicode_run` con `TrnInfo("<trend>", 1)`.

## 4. Alarmas

- `po_alarm_list`: estado actual de cada alarma (tabla Alarm); con `summary: true`, el resumen de activaciones
  (tabla AlarmSummary: horas de activación, desactivación y reconocimiento, usuario).
- `po_alarm_sql`: SQL vía el driver ODBC **Citect Alarm Driver** contra la base del servidor de alarmas.
  El servidor se toma del proyecto (tablas `almsrv` + `netaddr`, puerto DBPORT o 5482) o de
  POWEROP_ALARM_HOST/PORT. **Verificado:** en un equipo local los servidores de PO 2024 escuchan solo en `::1`;
  el MCP usa `localhost` con IPv6 habilitado.
- Fechas de las consultas en **UTC** con sintaxis ODBC `{ts 'YYYY-MM-DD HH:MM:SS'}`. Los resultados también
  vienen en UTC, con formato `dd/MM/yyyy HH:mm:ss.fffff`. AlarmState es numérico.

```sql
-- Estado actual de las alarmas
select AlarmSource, DisplayName, Description, AlarmCategory, AlarmState, OnTime, OffTime, AckTime, Equipment
from CiAlarmObject

-- Alarmas que cambiaron en la última ventana (ejemplo con hora UTC)
select AlarmSource, AlarmState, OnTime, OffTime, AckTime from CiAlarmObject
where AlarmLastUpdateTime >= {ts '2026-10-14 10:00:00'}

-- Journal de eventos (histórico) entre dos fechas UTC
select RecordTime, Source, Message, Category, AlarmStateDesc, User, ClientName from CDBEventJournal
where RecordTime between {ts '2026-10-13 00:00:00'} and {ts '2026-10-14 00:00:00'}
```

Otros campos de CiAlarmObject: Id, HelpPage, Comment, CustomFilters[0..7], Historian, Name (item),
DisableTime, ConfigTime. CDBEventJournal (verificado): RecordId, Time, RecordTime, ReceiptTime, Severity,
SeverityDesc, SeverityValue, Id, Source, Message, MessageLong, User, ClientAddress, ClientName, Category,
AlarmStateDesc, NamedRegion, Deleted, SeqNo, CommentNo, Location, CustomStringField, AreaOfInterest.

## 5. Páginas gráficas (Graphics Builder)

Las herramientas `po_gb_*` manejan Graphics Builder (`CtDraw32.exe`) por su interfaz de automatización COM
(`GraphicsBuilder.Application`). Graphics Builder tiene que estar abierto (o `po_gb_status` con `launch=true`); el MCP
no lo cierra. Todo lo de esta sección está **(verificado)** en PO 2024, salvo lo marcado.
- **Graphics Builder necesita Studio abierto (verificado):** lanzado solo (`launch=true` sin Studio) muestra "Plant
  SCADA could not communicate with Project Editor" y no se registra por COM. Pedile al usuario que abra Studio
  primero. `po_gb_status launch=true` detecta ese aviso y devuelve el error enseguida (sin esperar 90 s); el aviso queda
  abierto para que el usuario lo cierre **(detección no verificada en vivo)**. `ProjectSelected` puede no seguir al proyecto que el usuario elige en Studio: el MCP siempre pasa el
  proyecto explícito. Las librerías sí dependen del seleccionado (ver "Proyecto seleccionado" en Genies propios).

- Archivos de un proyecto: `.ctg` = página, `.ctm` = librería de genies, `.ctl` = librería de símbolos,
  `.ctt` = estilo de templates. `po_project_graphics` los lista por proyecto (y sus incluidos, siempre
  con el proyecto de sistema `Include`) sin abrir Graphics Builder.
- Flujo típico: `po_project_graphics` → `po_gb_library {style}` (templates: nombre, resolución, titlebar) →
  `po_gb_library {library}` (genies; `kind="symbol"` para símbolos) → `po_gb_library {library, element}` (propiedades
  del genie, p. ej. `Tag`, `Button_Text`) → `po_gb_page_create` con `ops` → `po_gb_page_read` / `po_gb_page_thumbnail` (PNG).
- `po_gb_page_create` usa el template del proyecto que tiene el estilo (`standard`/`normal` está en `Include`; la
  página queda en el proyecto pedido). Resoluciones: 0 default, 1 VGA, 2 SVGA, 3 XGA (1024×768), 4 SXGA, 5 user,
  6 WUXGA, 7 HD1080.
- `ops`: `text`, `rectangle`, `ellipse`, `line` (coordenadas en píxeles de la página, origen arriba a la izquierda),
  `genie` / `symbol` (librería + elemento en x, y; el genie con sus `properties`), y sobre objetos existentes (por
  `an`, el número de animación que da `po_gb_page_read`): `set_genie_properties`, `move`, `delete` y `group`
  (`{op: "group", ans: [a, b, ...]}`: `PageSelectObject` + `PageSelectObjectAdd` + `PageGroupSelectedObjects`).
- **Aspecto (`look`) y nivel (`level`)** en `po_gb_page_create`/`po_gb_page_edit`: `line_width` va de **1 a 32**
  (Graphics Builder da E_INVALIDARG con 0, verificado; el MCP lo rechaza fuera de rango). `gradient_colour` /
  `gradient_mode` (0 a 8) se guardan en la figura, pero **la miniatura de Graphics Builder no los muestra
  (no verificado en runtime)**. `level` (nivel de llenado por expresión) **(no verificado en vivo)**.
- **Animaciones y botones** (en `ops` de `po_gb_page_create` y `po_gb_page_edit`). **Verificado en Graphics Builder**
  (`MCP_Create_Test/MCP_GB_Dyn`): los registros quedan en `pgdynobj` (COL_A/COL_B con COLMODE 0 = on_off,
  1 = multi_state; TXT_STR con TXTMODE 4 = expresión; VISIBLE = visible_when) y `pgbutton` (DOWN_CMD, PRIV), y el
  proyecto compila sin errores. **Verificado en runtime** (`Transferencia_Demo/MCP_GB_Live`, midiendo los píxeles
  de la captura): `fill` on_off (estado 1 = condición verdadera) y multi_state, colores exactos, `visible_when`,
  `expression` y el botón dibujado (el botón no se presionó).
  - `fill` en `rectangle`, `ellipse`, `genie`, `symbol` y `set_dynamic` (por `an`): relleno por expresión.
    `{kind: "on_off", expression: "<cond>", colours: [{state: 0, colour: "#808080"}, {state: 1, colour: "#00B000"}]}`
    (0 = apagado, 1 = encendido) o `{kind: "multi_state", expression: "<A>", conditions: ["<B>", ...] (hasta 4 más),
    colours: [{state: 0..31, colour}]}`. Usa `PropertiesFillColourPut` + `PropertiesFillColourColourPutEx`; en
    rectángulos y elipses antes pone `AttributeSetFill = TRUE` (sin eso la figura es solo contorno).
  - Colores siempre como `"#RRGGBB"`: el MCP los pasa como entero positivo **0x00RRGGBB** (las funciones "Ex" no
    usan OLE_COLOR: con 0x00BBGGRR un `#E00000` se veía azul en runtime, verificado). En VBScript
    `&h00B000` es un Integer negativo (-20480) y el color sale mal (verificado en la prueba Transferencia_Demo).
  - `visible_when` (cualquier objeto): el objeto se ve cuando la expresión es verdadera. `PropertyVisibility` es el
    **"Hidden when"** de Appearance | Visibility (documentación y runtime), así que el MCP guarda `NOT (<expr>)`.
  - `expression` en `text` o `set_dynamic`: texto mostrado = expresión Cicode, p. ej. `"\"Secuencia: \" + TRF_Seq"`
    (`PropertiesDisplayValuePut`, tipo String).
  - `button`: x, y, x2, y2, `text`, `command` (Cicode al presionar), `up_command` (al soltar), `privilege` (1 a 255).
  - `set_touch` (solo en `po_gb_page_edit`/`po_gb_genie_edit`): `{an, action: "down"|"up", command, log_message?,
    repeat_ms?}` cambia el comando de Input | Touch de un objeto ya colocado. Lee antes lo actual
    (`PropertiesInputTouchGet`) y conserva el mensaje de log y la repetición si no se indican; la respuesta trae
    `before` y `after` (`after` es lo que Graphics Builder devuelve al releer después de escribir). Si no puede
    leer, no escribe. **Verificado en Graphics Builder (2026-11-26, páginas de MCP_Demo con otro proyecto
    seleccionado en GB; `tests\live.rs gb_write`)**: `PropertiesInputTouchGet(acción, comando, log, repetición)`
    (acción de entrada, el resto de salida; por nombre sin W, como el Put) con 1 = down y 0 = up, igual que el Put.
    En un `button` queda en `pgbutton` (`DOWN_CMD` = al presionar, `CMD` = al soltar, un solo `MSG_LOG`); en otros
    objetos en `pgtouch` (`ACTION` `KEY_LBUTTON`, `DOWN_CMD`, `CMD`; el log del up quedó en `MSG_LOG`, el del down no se probó). La repetición con
    down/up se lee 0 y no se guarda (`RPT_RATE` vacío): si se pide `repeat_ms` y no queda, la respuesta trae `note`.
  - `trend`: x, y, x2, y2, `pens` (1 a 8: `{expression: "<trend tag>", colour, off_colour?}`), `samples`,
    `pixels_per_sample` (1 a 32; por defecto 2 px y tantas muestras como entren en el ancho). Usa `DrawTrend` +
    `PropertiesTrendPutEx` (documentados en la ayuda de la automatización). Cada pluma se escala con ENG_ZERO/ENG_FULL
    de su trend tag; la ventana de tiempo es `samples` × SAMPLEPER del trend tag. **Verificado en Graphics Builder
    (2026-10-16, `Planta_Liquidos/Tendencias`):** cada trend queda como una fila en `pgtrend` (PAGE, AN, PEN1..PEN8),
    la miniatura muestra las plumas con sus colores y el proyecto compila sin errores. **Verificado en runtime
    (2026-10-16, misma página, 300 muestras × 3 px, SAMPLEPER 1 s):** las plumas dibujan los 5 minutos completos con
    el historial del trend. Ojo: el trend tarda unos segundos en dibujar después de `PageDisplay` (una captura
    inmediata sale vacía) y después rellena toda la ventana desde el historial.
  - Se validan antes de tocar Graphics Builder (colores, estados, expresiones vacías); el error dice el número de
    operación. Revisá el resultado con `po_gb_page_thumbnail` y, compilado, con `po_runtime_screenshot`.
- `po_gb_page_create` con `replace=true` recrea una página existente: copia el `.ctg` a `_mcp_backup`, la cierra si
  está abierta y guarda la nueva encima con `PageNew` + `PageSaveAs`. Sin `replace`, una página existente da error.
  **(verificado:** `PageSaveAs` sobre un nombre existente sobrescribe sin preguntar, deja una sola fila en
  `pages.dbf` y reescribe los registros `pg*` de la página)**.
- **`PageDelete` de la automatización no sirve en PO 2024 (verificado):** devuelve E_FAIL (E_ABORT si la página está
  abierta), con `True` o `False`, pero antes marca borrada la fila de `pages.dbf` y deja el `.ctg`/`.ctF` en disco:
  la página desaparece de Graphics Builder sin borrarse. Por eso `replace` ya no la usa. `po_gb_page_delete` todavía la
  usa, y **el 2026-10-16 borró bien dos páginas cerradas** (fila, `.ctg` y `.ctF`; Transferencia_Ejemplo y MCP_Demo),
  y el 2026-11-26 otras dos (páginas recién guardadas por `po_oneline_clone` en MCP_Demo y cerradas).
  Si falla como arriba, el error lo explica; para recuperar la página, `po_gb_page_create` con `replace=true` y el
  mismo nombre, o copiar el respaldo como `<página>.ctg`.
- `look.align` **(verificado el 2026-10-16)**: la justificación de Graphics Builder solo dice hacia dónde crece el
  texto desde donde quedó dibujado (siempre empieza en x). El MCP mide el texto y lo corre para que x sea el centro
  ("center") o el borde derecho ("right"); con `expression`, el valor crece desde ahí. En el editor un texto con
  `expression` se ve como `=expresión` (largo): al diseñar genies conviene que esos textos vayan alineados a la
  izquierda, para que no agranden el recuadro hacia arriba o la izquierda.
- En `po_gb_page_read`, `x2`/`y2` son la coordenada del extremo opuesto (no el tamaño). Los objetos `Genie`/`Symbol`
  traen `library_object` (proyecto, librería, elemento) y los genies sus propiedades.
- **Grupos (verificado el 2026-11-27, `tests\live.rs gb_write`)**: un grupo tiene AN propio y clase `Group`;
  `po_gb_page_read` entra en él (`PageSelectFirstObjectInGroup`/`NextObjectInGroup`; los anidados también,
  verificado en `tests\live.rs oneline_clone_grupos`) y lo devuelve con `path` y `children`; sus objetos van a continuación con `group` = el `path` del grupo. Un genie
  agrupado se edita por su AN (`set_genie_properties`, verificado). Los objetos ocultos (`visible_when`) se listan
  igual. `visible_when` sobre un genie dentro de un grupo dio E_HANDLE (0x80070006); sobre el grupo y sobre un texto
  funciona. En `SingleLinesOverview_T300` hay dos objetos sin AN ni clase que no son grupos (E_HANDLE al entrar):
  salen con `note`.
- Una página nueva o editada **no se ve en runtime hasta compilar** el proyecto.
- Diálogos modales: Graphics Builder bloquea las llamadas COM mientras muestra uno. El MCP los cierra solo
  (No/OK/Cancelar) y los devuelve en `warnings`, salvo los avisos de actualización de versión
  ("Cannot convert '<proyecto>' to version …"): si se aceptan, **Graphics Builder se cierra**, así que se dejan
  abiertos (Graphics Builder sigue respondiendo por COM). Pedile al usuario que no los acepte.
- Causa de "Cannot convert '<proyecto>' to version 8.50 format. You MUST use version 5.31…": Graphics Builder
  deduce la versión del proyecto abriendo `<proyecto>\GLOBAL.CTL` (librería global de símbolos); si no existe
  (o su versión es < 6) lo trata como anterior a la 5.31. Pasa con proyectos armados a mano o copiados sin ese
  archivo. Arreglo: copiar un `GLOBAL.CTL` vacío (352 bytes, el de `Library_Controls` sirve) a la carpeta del
  proyecto. `po_gb_status` lista los proyectos sin `GLOBAL.CTL` y `po_gb_page_*` lo advierten. **(verificado)**
- Al abrirse, Graphics Builder crea una página `Untitled` en el proyecto seleccionado: no la guardes. Para ella
  Graphics Builder devuelve basura como proyecto (verificado): `po_gb_status` la lista con `project: null` y
  `unsaved: true`.
- **Error conocido:** el `project` de `open_pages` en `po_gb_status` sale de `PageAppearanceGetEx` y es el proyecto
  del template, no el de la página (2026-10-16: `MCP_Demo/MCP_GB_Test` salió como `Include` y
  `Transferencia_Ejemplo/Transferencia` como `PLS_Include`). No usarlo para saber dónde está la página.
- Escrituras (`po_gb_page_create`, `po_gb_page_edit`, `po_gb_page_delete`): `POWEROP_ALLOW_WRITE=1`, auditadas, y nunca en
  proyectos de sistema/librerías (System, Include, Library_*, PLS_Include, PSO_Equipment, IndustrialGraphics_Include).
  Excepción: `po_gb_page_edit` sobre una página exacta listada en `POWEROP_GB_ALLOW_PAGES` (p. ej.
  `PLS_Include/!equipment1`; la respuesta trae `allowed_by`). Esas librerías las usan todos los proyectos del equipo:
  confirmá con el usuario. Crear o borrar páginas y editar genies de librerías sigue bloqueado. **Sin verificar**: con
  la página ya abierta, Graphics Builder puede informarla bajo otro proyecto (p. ej. `Include`, ver el error
  conocido de `open_pages`); si `PageSelect` no la encuentra, el MCP intenta `PageOpen`.
  Antes de guardar encima o borrar, el `.ctg` se copia a `<proyecto>\_mcp_backup\` (campo `backup` de la respuesta):
  para restaurar, copiar ese archivo de vuelta como `<página>.ctg`.

### Genies propios (`po_gb_genie_*`)

- `po_gb_genie_read {library, genie}`: propiedades (con valor por defecto), hotspot, objetos (como `po_gb_page_read`)
  y `place_op`, la op `genie` lista para `po_gb_page_create/edit`. Lee también librerías de proyectos incluidos.
- `po_gb_genie_create {library, genie, ops, hotspot?, replace?}`: dibuja con las mismas `ops` que una página, en
  píxeles desde (0, 0); el hotspot (punto que cae en x, y al colocarlo) por defecto queda donde lo ponga Graphics
  Builder: indicarlo. Cada `%Nombre%` en textos, expresiones, rellenos o comandos se vuelve propiedad del genie
  (p. ej. `fill.expression = "%Tag% = 1"`, `text = "%Label%"`). Si la librería no existe la crea.
- `po_gb_genie_edit {library, genie, ops?, hotspot?}`: ops de `po_gb_page_edit` (move/delete/set_dynamic por AN
  de `po_gb_genie_read`). `po_gb_genie_delete {library, genie}`.
- Escritura: solo librerías `.ctm` del propio proyecto (nunca de un incluido, como PLS_Include), copia del `.ctm`
  en `_mcp_backup` antes de cada cambio, y error si el genie está abierto en Graphics Builder.
- API de automatización **(verificado el 2026-10-16 en MCP_Demo)**: `PageNewEx(1)` abre un genie nuevo;
  `PageSaveAsEx(proyecto, librería, genie)` lo guarda (la página pasa a llamarse `librería.genie`);
  `PageOpenEx(proyecto, librería, genie, 1)` lo abre; `PageDeleteEx(proyecto, librería, genie, 1)` lo borra;
  `PageNewLibrary(proyecto, librería, 1)` solo crea el `.ctm` (no abre página: un `PageSaveAsEx` justo después
  guarda la página activa, que puede ser la `Untitled` de Graphics Builder); `LibraryObjectHotspotPut/Get(x, y)`.
  Los genies de PLS_Include tienen el hotspot en (100, 100) y los objetos dibujados desde ahí.
- **Colocación (verificado el 2026-10-16):** la op `genie` en (x, y) pone ahí la esquina superior izquierda del
  recuadro (`bbox`) del genie; el hotspot se ignora (probado con hotspot (0,0), (10,10) y sin fijar). `po_gb_genie_read`
  y `po_gb_genie_create` devuelven `bbox`, y create avisa si empieza antes de (0, 0): para que el (0, 0) del dibujo
  caiga en (X, Y), colocarlo en (X + bbox.x, Y + bbox.y). Sin `hotspot`, Graphics Builder guarda uno arbitrario.
- **Proyecto seleccionado (verificado el 2026-10-16):** Graphics Builder solo usa las librerías del proyecto
  seleccionado y de sus incluidos; con otro, `LibraryObjectPlaceEx` falla con 0x80004003 y `LibraryFirstObject` no
  lista nada (una librería que ya cargó antes la sigue listando, pero colocar falla igual). Por eso
  `po_gb_page_create/edit` (ops `genie`/`symbol`), `po_gb_genie_*` y `po_gb_library` comparan el proyecto de cada
  librería con el seleccionado y sus incluidos (include.dbf); si falta, seleccionan el proyecto de la página (o del
  genie) con `ProjectSelect` antes de abrirla y vuelven al anterior después de cerrarla. La respuesta trae
  `gb_project_switch {selected, previous, restored}`. `ProjectSelect` tarda ~200 ms, **cambia también el proyecto
  activo de Studio** (título de la ventana y `LASTDATABASE` del citect.ini) y **no hace nada si Graphics Builder tiene
  páginas abiertas** (ni la `Untitled`): en ese caso el error pide cerrarlas o elegir el proyecto en Studio
  (`po_studio_set_project` usa este mismo `ProjectSelect` para cambiar el activo de Studio en caliente). Si no se
  pudo restaurar (p. ej. la página quedó abierta porque falló el guardado), queda un aviso.
- Las páginas toman los cambios del genie al abrirse (genies vinculados): editar el genie actualiza las páginas.

### Advanced One-Line (`po_oneline_read`, `po_oneline_validate`: offline, solo lectura; `po_oneline_clone`)

- **Cómo está armado** (PO 2024, visto en PLS_Include y en `OneLineConfig.Business.dll`; **verificado** sobre la página
  de ejemplo `PSO_Equipment/SingleLinesOverview_T300`): cada objeto del unifilar es un genie de PLS_Include
  (`pls_ansi_*`, `pls_iec_*`, `pls_gen_*`, `pls_meter`, `pls_easergy`) con propiedades numéricas: `BkrNum`
  (interruptor), `BusbarNum` (barra), `MtrNum` (medidor), `SrcNum` (fuente), `SrcBusbarNum`/`DestBusbarNum` (barras de
  origen y destino); transferencias con `BkrNum1`/`BkrNum2`/`SrcBusbarNum1`/`SrcBusbarNum2`. `Equip` = equipo,
  `Active` = condición de línea activa. Las animaciones usan tags del dispositivo `zOL`: `zOL\Bkr<BkrNum>`,
  `zOL\Bus<N>`, `zOL\Mtr<N>`, `zOL\Src<N>` (p. ej. `PLS_Bkr_Color_Bkr(12, zOL\Bus3, zOL\Bus4, zOL\Bkr12)`), que escribe
  `AdvOneLine.exe` según `AdvOneLine.csv` (lo genera la One-Line Configuration Utility, `OneLineConfig.exe`:
  `Component,Condition,Bus1,Bus2` con `BKR<n>`, `SRC<n>`, `MTR<n>`, `XFR<n>` y colores `CLR<n>`).
- Las herramientas leen los `.ctg` del disco **sin Graphics Builder**: ven genies ocultos y dentro de grupos, y las
  expresiones ya expandidas. El lector (`src/ctg.rs`) recorre el contenedor de la página (directorio → páginas →
  objetos, sacado de `Ctg32.dll`) y lee solo los objetos de las páginas en uso: no toma los imports (librerías que
  usa la página), ni páginas borradas ni huecos con restos. Lo que hay dentro de cada objeto no está documentado y se
  dedujo de páginas reales. **Verificado** el 2026-11-27 en las 111 `.ctg` de una instalación de PO 2024 (Include,
  PLS_Include, PSO_Equipment, Library_Controls y proyectos de prueba): todas son contenedores válidos y se leen sus 1745 genies con
  propiedades, sin ninguno sin leer. Lo que no reconozca va en `unparsed_genies`: no se adivina.
- El tipo se deduce de las propiedades: `BkrNum1/2` → ats, `BkrNum` → breaker, `MtrNum` → meter, `SrcNum` →
  source, `SimSrcNum` (o `SrcNum`) con origen y destino → transformer, solo `BusbarNum` → busbar. **Coincide con la
  lista oficial (2026-11-26)**: la utilidad toma el tipo de cada genie de `PLS_Include\SupportedGenies.xml`
  (librería/genie → PwrBreaker, PwrBusbar, PwrMeter, PwrSource, PwrTransformer, PwrATS, PwrMotor) y sus propiedades
  de `GenieDefinitions.xml`. Transformador (`trans_nd/sd/ss_1/2` de `pls_gen_eq_1/2`): `SimSrcNum` (fuente
  simulada), `SimSrcNum2`, `SrcBusbarNum`, `DestBusbarNum`; no tiene tag zOL propio (anima con `zOL\Bus<origen>` y
  `zOL\Bus<destino>`) y en el csv es `XFR<SimSrcNum>`. ATS (`ats_nd`): `BkrNum1/2`, `SrcBusbarNum1/2`,
  `DestBusbarNum`. Los motores de `pls_gen_eq` son barras y los `pls_ansi/iec_mot_*` interruptores. El `an` es la
  propiedad `AN` del genie (los de PLS la tienen), no el AN del objeto en Graphics Builder.
- Recorre el proyecto y sus incluidos salvo los de sistema y librerías (como la utilidad: System, Include,
  PLS_Include, PSO_Equipment, Library_*...). `pages` filtra por nombre (`*`/`?`).
- `po_oneline_validate` (errores primero):
  - Repetidos (`DuplicateBreaker/Busbar/Meter/Source`): en la misma página = error; en varias páginas = error si el
    equipo cambia (el color de uno muestra el estado del otro: típico de copiar una página sin renumerar) y
    advertencia si es el mismo equipo (puede ser otra vista). Barras repetidas con el mismo equipo: info.
  - `MissingId`/`InvalidId`, `SameSourceAndDestinationBus`, `InvalidSourceBus`/`InvalidDestinationBus` (no numérico),
    `NoSourceOrDestinationBus`, `BusWithoutBusbar` (info: normal en una salida, como en la página de ejemplo).
  - `InvalidEquipment` (no está en equip.dbf del proyecto e incluidos), `NoEquipment`, `NoLineActive`.
  - `foreign: ["EstB_"]` → `ForeignReference` en cualquier propiedad que contenga ese texto (encuentra el `Active` o
    el `DetailsFn` que quedó apuntando a otra estación, también en genies ocultos). `equip_prefix` →
    `ForeignEquipment`. `UnknownTag`: tags con `\` de las propiedades que no están en variable.dbf.
  - `StaleZolReference`: un `zOL\Bkr<N>`/`Mtr`/`Src` en las expresiones de la página sin ningún objeto de esa página
    que lo defina (o un `zOL\Bus<N>` que no usa nadie): restos de una copia.
  - Con `AdvOneLine.csv` en la carpeta del proyecto: `NotInCsv` (en páginas, no en el csv: regenerar con la utilidad) y
    `CsvOrphan`. La utilidad lo guarda en la carpeta del proyecto (visto en `OneLineCSV.Save` de
    `OneLineConfig.Business.dll`, 2026-11-26), que es donde lo busca el MCP.
  - `TransformerSourceClash` (error): el `SimSrcNum`/`SimSrcNum2` de un transformador es el `SrcNum` de una fuente.
    La utilidad lo "arregla" sola dándole otro número al transformador (`FixTransformerInvalidIDs`): mejor
    corregirlo en la página. Los transformadores no entran en los `Duplicate*` (varios comparten fuente simulada).
- `po_oneline_clone` (escritura): página nueva a partir de otra, con números zOL nuevos.
  - **Plan** (`apply=false`, por defecto; offline, no necesita Graphics Builder): lee la página de origen (del
    proyecto, de un incluido o de `source_project`, p. ej. una página de ejemplo de PSO_Equipment) y devuelve los
    mapas viejo → nuevo por espacio (`bkr`: BkrNum/BkrNum1/2; `bus`: BusbarNum y todo `*BusbarNum*`; `mtr`: MtrNum;
    `src`: SrcNum/SimSrcNum/SimSrcNum2 y `SRC`/`XFR` del csv: el mismo espacio), los cambios por genie, `manual_zol_refs` (tags zOL escritos a mano en expresiones: no se
    renumeran) y `expected_findings` (lo que daría `po_oneline_validate` sobre la página nueva junto al resto del
    proyecto, con los `from` de `replace` como `foreign`).
  - Numeración: `next_free` (desde el mayor usado + 1 o desde `start`, saltando los usados en las páginas del
    proyecto e incluidos y en AdvOneLine.csv), `offset` (error si alguno choca) o `keep`. El mismo número viejo va
    al mismo nuevo dentro de un espacio: las conexiones interruptor → barra se mantienen. Sin `bus` en `spaces`, la
    copia queda colgada de las mismas barras. 0 y vacío no se tocan; `AN` nunca se cambia.
  - `replace` cambia texto en las demás propiedades (Equip, Active, DetailsFn, textos), sin distinguir mayúsculas y
    en orden. Conviene que sea específico: en `SingleLinesOverview_T300`, `T300` → `T301` también renombra la función
    `PLST300ShowDetail` del `DetailsFn` (visto en el plan; usar `MA_T300_` → `MA_T301_`). Revisar `changes`.
  - **Aplicar** (`apply=true`): abre la página de origen en Graphics Builder (no tiene que estar abierta), cambia
    las propiedades con `LibraryObjectPutProperty` y la guarda con otro nombre (`PageSaveAs`); la de origen no se
    guarda. El recorrido entra en grupos (ver Grupos más arriba). Si Graphics Builder ve menos cambios que el plan no
    guarda nada, salvo `allow_partial=true`. Después lee el `.ctg` guardado y compara genie por
    genie (`verification.not_applied`), busca los textos de `replace` que quedaron (`leftovers`) y valida la página
    nueva (`findings`, incluye `StaleZolReference` si alguna expresión siguió apuntando a un zOL viejo). No reemplaza
    páginas existentes. `verification.ok` = guardada y sin errores que el plan no esperaba; `new_findings` = lo
    que la validación encontró y no estaba en `expected_findings`; `applied_as_planned` = todos los cambios en el `.ctg`.
  - El destino tiene que ver las librerías de los genies (incluir PLS_Include): si no, Graphics Builder los abre
    como `DummyMarker` (genie sin resolver) y no se les pueden cambiar propiedades. El plan lo avisa
    (`missing_libraries`) y al aplicar, si faltan cambios y hay `DummyMarker`, no guarda nada y lo explica.
  - **Verificado en vivo (2026-11-26, `PSO_Equipment/SingleLinesOverview_T300` → página nueva de MCP_Demo, 16
    objetos, 39 cambios)**: `LibraryObjectPutProperty` vuelve a expandir las expresiones del genie (las animaciones
    del `.ctg` guardado usan los zOL nuevos, sin `StaleZolReference` ni `leftovers`); `PageSaveAs` guarda en el
    proyecto de destino (`.ctg`/`.ctF` en su carpeta y fila en su `pages.dbf`; la de origen no cambia); Graphics
    Builder vio los 39 cambios (esa página no tiene genies en grupos).
  - **Verificado en vivo dentro del mismo proyecto (2026-11-27, `tests\live.rs oneline_clone_mismo_proyecto`)**: en
    un proyecto creado con `po_project_create` que incluye PLS_Include y PSO_Equipment (`pls_easergy` está en
    PSO_Equipment, no en PLS_Include), una copia de T300 clonada con `next_free` toma números por encima de los de la
    página de origen; 18 genies y 39 propiedades editados, `ok`, y `po_oneline_validate` del proyecto sin errores
    ni advertencias. Graphics Builder carga la lista de proyectos al abrir: con un proyecto creado después hay que
    reabrirlo (el error lo dice). El plan (`apply=false`) también necesita `POWEROP_ALLOW_WRITE=1` (la herramienta
    es de escritura).
  - **Verificado en vivo con grupos (2026-11-27, `tests\live.rs oneline_clone_grupos`)**: en una copia de T300 con
    dos interruptores en un grupo y ese grupo junto con una barra dentro de otro (interruptores a profundidad 2),
    Graphics Builder vio y editó los 39 cambios del plan (18 genies), el `.ctg` guardado coincide con el plan
    (`applied_as_planned`, sin `leftovers`), la copia conserva los dos grupos y `po_oneline_validate` del proyecto
    queda sin errores ni advertencias. El lector offline del `.ctg` también ve los genies anidados.
    Después: compilar y regenerar AdvOneLine.csv con la One-Line Configuration Utility.

## 6. PLC por Modbus TCP (solo lectura)

- `POWEROP_MODBUS=nombre=host[:puerto][/unidad]`, varios separados por `;` (puerto 502 y unidad 255 por defecto;
  sin nombre: PLC1, PLC2...). `po_plc_read` usa el primero si no se indica `plc`.
- Solo funciones de lectura: FC1 coils, FC2 entradas discretas, FC3 holding, FC4 input registers. **No existe
  ninguna escritura al PLC desde el MCP**, ni hay que buscarla (los comandos van por tags del SCADA y con
  confirmación).
- Notación Schneider (Unity/Control Expert): `%MWn` holding, `%IWn` input register, `%Mn` coil, `%In` entrada
  discreta; dirección 0-based tal como va en la trama (`%MW4000` = registro 4000). Los registros ≥ 0x8000 traen
  también `signed`.
- Dos modos: `addresses` (lista suelta, hasta 200; una excepción Modbus queda solo en esa dirección) o `area` + `start` + `count`
  (hasta 10000; se parte en pedidos de 125 registros / 2000 bits) con `nonzero_only` para explorar.
- Para mapear un tag de Citect a una dirección del PLC: campo ADDR de `variable` y protocolo del `units`
  (`po_project_query`). Un dispositivo de memoria (protocolo GENERIC) **no** lee el PLC aunque los nombres coincidan.
- **(verificado** con un PLC falso en los tests; contra el PLC real se exploró antes con lecturas FC1–FC4 sin la
  herramienta: **`po_plc_read` no verificado contra el PLC real)**.

## 7. Seguridad

- Es un SCADA eléctrico: `po_tag_write`, `po_alarm_ack`, Cicode con efectos (incluido `PageDisplay`) y `po_cicode_write` actúan sobre
  equipos, el cliente o el proyecto; `po_project_create`, `po_project_rows_*`, `po_project_compile` y las herramientas `po_gb_*` de
  escritura cambian el proyecto; `po_runtime_control` arranca o reinicia el SCADA. Confirmá con el
  usuario, una operación por vez (un lote de `po_tag_write` se confirma completo), y verificá el resultado.
- Por defecto el MCP está en solo lectura; las escrituras exigen `POWEROP_ALLOW_WRITE=1` y quedan
  en el log de auditoría.
- Tablas DBF: solo en `MCP_Demo` o proyectos creados por el MCP, siempre con respaldo. `MASTER.DBF` y `citect.ini`
  se respaldan antes de tocarlos; cambiar `Run=` requiere el OK explícito del usuario. Nunca restaurar `.ctz`, nunca
  escribir contraseñas, nunca tocar proyectos de sistema/librería.
