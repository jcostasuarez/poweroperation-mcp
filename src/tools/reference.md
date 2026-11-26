# Referencia rápida: Power Operation para agentes

EcoStruxure Power Operation (PO) corre sobre **AVEVA Plant SCADA** (ex Citect SCADA), con librerías propias
(proyectos incluidos `PLS_*`, Profile Editor, I/O Device Manager). Todo lo de Citect/Plant SCADA aplica:
Cicode, tablas DBF de proyecto, CtAPI, servidores de I/O, alarmas, tendencias y reportes.

## 1. Proyecto en disco (herramientas offline)

- Carpeta típica: `C:\ProgramData\Schneider Electric\Power Operation\v20xx\User\<Proyecto>\`
  (el `citect.ini` de `...\Config\` indica `[CtEdit] User=` y `Run=` = proyecto en ejecución).
- Un proyecto PO **incluye** otros (include.dbf): las librerías de PO y a veces proyectos propios.
  Usá `include_included: true` para buscar en todos.
- La configuración está en tablas **dBase (.dbf)**. Las más usadas (los nombres de campo exactos
  dependen de la versión: verificá con `project_tables`):

| Tabla | Contenido | Campos habituales |
|---|---|---|
| `variable` | Variable tags | TAG, TYPE, UNIT (I/O device), ADDR, RAW_ZERO/RAW_FULL, ENG_ZERO/ENG_FULL, ENG_UNITS, FORMAT, COMMENT, CLUSTER, EQUIP, ITEM |
| `digalm` | Alarmas digitales | TAG, NAME, DESC, VAR_A, VAR_B, CATEGORY, AREA, PRIV, HELP |
| `anaalm` | Alarmas analógicas | TAG, NAME, DESC, VAR, HIGHHIGH, HIGH, LOW, LOWLOW, DEADBAND, CATEGORY |
| `advalm` | Alarmas avanzadas (expresión) | TAG, NAME, DESC, EXPR, CATEGORY |
| `tsdigalm`, `tsanaalm` | Alarmas con timestamp (SOE) | TAG, NAME, DESC, VAR... |
| `almcat` | Categorías de alarma (prioridad, formato) | CATEGORY, PRIORITY, ... |
| `trend` | Trend tags | NAME, EXPR, SAMPLEPER, TYPE, FILENAME, ENG_UNITS, COMMENT |
| `units` | Dispositivos de I/O | NAME, NUMBER, ADDRESS, PROTOCOL, PORT, COMMENT |
| `ports`, `boards` | Puertos y placas de comunicación | |
| `equip` | Modelo de equipos | NAME, TYPE, AREA, IODEVICE, COMMENT |
| `pages` | Propiedades de páginas gráficas | |
| `include` | Proyectos incluidos | NAME |

- Las **páginas gráficas** (.ctg) y los genies son binarios: este MCP no los edita.
- Nombres de tags en PO: normalmente `<Dispositivo>\<NodoLógico>\<Atributo>` al estilo IEC 61850,
  p. ej. `Feeder1\MMXU1\TotW` (potencia activa total), `Feeder1\XCBR1\Pos` (posición de interruptor).
  Confirmalo con `project_query` sobre `variable` antes de asumir nombres.
- Cicode: archivos `*.ci` en la carpeta del proyecto. Tras modificar algo **hay que compilar** en
  Power Operation Studio y recargar el runtime.

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

- Tipos: INT, REAL, STRING, OBJECT, LONG, QUALITY, TIMESTAMP. Sin tipo de retorno = VOID.
- Alcance de variables: `GLOBAL` (todo el proyecto), `MODULE` (el archivo), local (dentro de la función).
- Bloques: `IF ... THEN ... [ELSE ...] END`, `WHILE ... DO ... END`, `FOR i = 0 TO 9 DO ... END`,
  `SELECT CASE x` / `CASE 1` / `CASE 2, 3` / `CASE 4 TO 6` / `CASE IS > 10` / `CASE ELSE` / `END SELECT`,
  `REPEAT ... UNTIL cond`.
- Operadores: `+ - * / MOD`, `AND OR NOT`, `BITAND BITOR BITXOR`, `+` concatena cadenas.
- Escape en cadenas con `^`: `^"` comilla, `^n` salto, `^t` tab, `^^` circunflejo.
- Tags con `\` en el nombre no pueden usarse como identificador: usar `TagRead("...")` / `TagWrite("...", valor)`.
- Funciones frecuentes: TagRead, TagWrite, TagGetProperty, IntToStr, RealToStr, StrToInt, StrToReal,
  StrLeft/StrMid/StrRight/StrLength/StrSearch, TimeCurrent, TimeToStr, Sleep, SleepMS, TaskNew,
  ErrSet/IsError, Message, PageDisplay, AlarmAckTag, AlarmEnableTag, AlarmDisableTag,
  TagBrowseOpen/First/Next/GetField/Close, AlmBrowseOpen/..., TrnQuery.
- Las funciones bloqueantes (Sleep, I/O) deben ir en tareas (TaskNew) y no en animaciones de páginas.
- `cicode_check` hace solo un chequeo estructural; la validación real es compilar en Studio.

## 3. Runtime vía CtAPI

- `tag_read` / `tag_write`: valores en unidades de ingeniería, como texto. `Tag.Elemento` y `Tag[i]` admitidos.
- `cicode_run`: ejecuta en el runtime; los argumentos de cadena van entre comillas:
  `TagGetProperty("Feeder1\MMXU1\TotW", "EngUnits")`. El resultado vuelve como texto.
  En modo solo lectura solo se permiten funciones de consulta.
- `tag_browse` / `ctapi_find`: tablas `Tag`, `Alarm`, `Trend`... Filtro `"CAMPO=valor"` con comodines
  (p. ej. `TAG=Feeder1*`). Las propiedades que no existen en la versión instalada se informan en
  `unavailable_properties`.
- Errores: "error Citect N" = código de error de Cicode/CtAPI; "error Windows N" = sistema.

### Tendencias (TRNQUERY)

`trend_query` arma: `TRNQUERY,Endtime,EndtimeMs,Period,NumSamples,Tagname,DisplayMode,DataMode,InstantTrend,SamplePeriod`
(Endtime en segundos UTC desde 1970; el inicio sale de Period × NumSamples). Propiedades: DATETIME,
MSECONDS, VALUE, QUALITY.

- DataMode: 0 = una muestra por período, interpolando huecos; 1 = timestamps reales, sin interpolar.
- DisplayMode = suma de: orden (0 antiguo→nuevo, 1 nuevo→antiguo) + condensado (0 promedio, 4 mínimo,
  8 máximo, 12 más reciente) + estiramiento (0 escalón, 128 proporcional, 256 crudo) + relleno de huecos
  (n × 4096) + calidad mala (0 = cero, 2097152 = último valor válido) + 4194304 = datos totalmente crudos
  (solo con DataMode 1).
- QUALITY: bits 0-3 tipo (1 interpolado, 2/3 crudo), bits 4-7 calidad (1 = buena).
- El nombre es el del **trend tag** (tabla `trend`, campo NAME), que puede diferir del variable tag.

## 4. Alarmas

- `alarm_list`: alarmas del runtime vía CtAPI (tabla Alarm, filtro `TAG=...`).
- `alarm_sql`: SQL vía el driver ODBC **Citect Alarm Driver** contra el servidor de alarmas (puerto 5482).
  Fechas en **UTC** con sintaxis ODBC `{ts 'YYYY-MM-DD HH:MM:SS'}`.

```sql
-- Estado actual de las alarmas
select AlarmSource, DisplayName, Description, AlarmCategory, AlarmState, OnTime, OffTime, AckTime, Equipment
from CiAlarmObject

-- Alarmas que cambiaron en la última ventana (ejemplo con hora UTC)
select AlarmSource, AlarmState, OnTime, OffTime, AckTime from CiAlarmObject
where AlarmLastUpdateTime >= {ts '2026-10-14 10:00:00'}

-- Journal de eventos (histórico) entre dos fechas UTC
select Id, RecordTime, AlarmStateDesc, Message, Category, User, ClientName from CDBEventJournal
where RecordTime between {ts '2026-10-13 00:00:00'} and {ts '2026-10-14 00:00:00'}
```

Otros campos de CiAlarmObject: Id, HelpPage, Comment, CustomFilters[0..7], Historian, Name (item),
DisableTime, ConfigTime.

## 5. Seguridad

- Es un SCADA eléctrico: `tag_write`, `alarm_ack`, Cicode con efectos y `cicode_write` actúan sobre
  equipos o sobre el proyecto. Confirmá con el usuario, una operación por vez, y verificá el resultado.
- Por defecto el MCP está en solo lectura; las escrituras exigen `POWEROP_ALLOW_WRITE=1` y quedan
  en el log de auditoría.
