//! Herramientas de Graphics Builder (páginas gráficas, genies, librerías) por automatización COM.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Result, anyhow, bail};
use base64::Engine as _;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::gb::{Arg, Session, Val};
use crate::project::{self, Catalog, Project};
use crate::server::{PowerOpServer, ToolResult, blocking, err};
use crate::text::truncate;

/// Proyectos de sistema y librerías de Power Operation: Graphics Builder no los modifica desde el MCP.
const PROTECTED: &[&str] = &["System", "Include", "Library_Controls", "Library_Equipment", "PLS_Include", "PSO_Equipment", "IndustrialGraphics_Include"];

/// Tipos de objeto de librería de Graphics Builder (LibraryObjectPlaceEx / LibraryFirstObject).
const LIB_SYMBOL: i16 = 0;
const LIB_GENIE: i16 = 1;

fn pretty(v: &Value, max: usize) -> ToolResult {
    let s = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    Ok(truncate(&s, max))
}

pub(crate) fn check_writable(project: &str) -> Result<()> {
    if PROTECTED.iter().any(|p| p.eq_ignore_ascii_case(project)) {
        bail!("{project} es un proyecto de sistema o librería de Power Operation: el MCP no modifica sus páginas.");
    }
    Ok(())
}

/// Como `check_writable`, pero deja editar una página de un proyecto protegido si figura en
/// `POWEROP_GB_ALLOW_PAGES` (página exacta). Solo la usa po_gb_page_edit: crear o borrar páginas y editar
/// genies de librerías sigue bloqueado.
fn check_page_writable(project: &str, page: &str, allowed: &[(String, String)]) -> Result<bool> {
    if !PROTECTED.iter().any(|p| p.eq_ignore_ascii_case(project)) {
        return Ok(false);
    }
    if allowed.iter().any(|(p, g)| p.eq_ignore_ascii_case(project) && g.eq_ignore_ascii_case(page)) {
        return Ok(true);
    }
    bail!(
        "{project} es un proyecto de sistema o librería de Power Operation: el MCP no modifica sus páginas. \
         Para editar solo esta página, agregá {project}/{page} a POWEROP_GB_ALLOW_PAGES (afecta a todos los \
         proyectos que incluyen {project}) y reiniciá el MCP."
    );
}

fn s(v: &str) -> Arg {
    Arg::Str(v.to_string())
}

/// Selecciona la página si ya está abierta; si no, la abre. Devuelve true si la abrió esta llamada.
fn open_page(gb: &mut Session, project: &str, page: &str) -> Result<bool> {
    if gb.m("PageSelect", &[s(project), s(page)]).is_ok() && gb.m("PageName", &[]).is_ok_and(|r| r.ret.text().eq_ignore_ascii_case(page)) {
        gb.warnings.retain(|w| !w.starts_with("PageSelect"));
        return Ok(false);
    }
    gb.warnings.retain(|w| !w.starts_with("PageSelect"));
    gb.m("PageOpen", &[s(project), s(page)]).map_err(|e| anyhow!("No se pudo abrir la página {project}/{page}: {e:#}"))?;
    Ok(true)
}

/// Cierra la página activa sin guardar (si pregunta, el vigilante responde No).
pub(crate) fn close_page(gb: &mut Session) {
    let _ = gb.m("PageClose", &[]);
}

/// Antes de recrear una página con replace: si está abierta, la cierra sin guardar. No se borra: `PageSaveAs` sobre el
/// mismo nombre la sobrescribe sin preguntar y reescribe sus registros de animación (verificado en PO 2024).
/// `PageDelete` no sirve: marca borrada la fila de pages.dbf y devuelve E_FAIL dejando el .ctg (verificado).
fn close_if_open(gb: &mut Session, project: &str, page: &str) {
    if is_open(gb, project, page) {
        close_page(gb);
    }
}

/// La página está abierta en Graphics Builder (queda seleccionada si lo está).
pub(crate) fn is_open(gb: &mut Session, project: &str, page: &str) -> bool {
    let open = gb.m("PageSelect", &[s(project), s(page)]).is_ok() && gb.m("PageName", &[]).is_ok_and(|r| r.ret.text().eq_ignore_ascii_case(page));
    gb.warnings.retain(|w| !w.starts_with("PageSelect"));
    open
}

/// Apariencia de la página activa.
fn appearance(gb: &mut Session) -> Value {
    let o = [Arg::OutStr, Arg::OutStr, Arg::OutStr, Arg::OutI2, Arg::OutBool, Arg::OutI2, Arg::OutI2, Arg::OutI4, Arg::OutI4];
    match gb.m("PageAppearanceGetEx", &o) {
        Ok(r) => json!({
            "project": r.outs[0].json(), "style": r.outs[1].json(), "template": r.outs[2].json(),
            "resolution": r.outs[3].json(), "titlebar": r.outs[4].json(), "width": r.outs[5].json(), "height": r.outs[6].json(),
            "background_on": r.outs[7].json(), "background_off": r.outs[8].json(),
        }),
        Err(e) => json!({"error": format!("{e:#}")}),
    }
}

/// Propiedades (nombre → valor) del genie activo.
fn genie_properties(gb: &mut Session, first: &str, first_args: &[Arg], next: &str) -> Vec<(String, String)> {
    let mut props = Vec::new();
    let mut r = gb.m(first, first_args);
    while let Ok(rep) = r {
        let name = rep.outs.first().map(Val::text).unwrap_or_default();
        if name.is_empty() || props.len() >= 200 {
            break;
        }
        props.push((name, rep.outs.get(1).map(Val::text).unwrap_or_default()));
        r = gb.m(next, &[Arg::OutStr, Arg::OutStr]);
    }
    props
}

/// Objeto activo: AN, clase, posición, texto, objeto de librería y propiedades de genie.
fn active_object(gb: &mut Session, with_props: bool) -> Value {
    let mut o = serde_json::Map::new();
    let int = |gb: &mut Session, m: &str| gb.m(m, &[]).ok().and_then(|r| r.ret.int());
    o.insert("an".into(), json!(int(gb, "AttributeAN")));
    let class = gb.m("AttributeClass", &[]).map(|r| r.ret.text()).unwrap_or_default();
    o.insert("class".into(), json!(class));
    o.insert("x".into(), json!(int(gb, "AttributeX")));
    o.insert("y".into(), json!(int(gb, "AttributeY")));
    // AttributeExtentX/Y: coordenada del extremo opuesto al origen (fin de una línea, esquina de un rectángulo).
    o.insert("x2".into(), json!(int(gb, "AttributeExtentX")));
    o.insert("y2".into(), json!(int(gb, "AttributeExtentY")));
    if class.to_lowercase().contains("text") {
        if let Ok(t) = gb.get("AttributeText") {
            o.insert("text".into(), json!(t.text()));
        }
    }
    if let Ok(r) = gb.m("LibraryObjectName", &[Arg::OutStr, Arg::OutStr, Arg::OutStr, Arg::OutI2]) {
        let element = r.outs[2].text();
        if !element.is_empty() {
            o.insert(
                "library_object".into(),
                json!({"project": r.outs[0].text(), "library": r.outs[1].text(), "element": element, "type": r.outs[3].json()}),
            );
            if with_props {
                let props = genie_properties(gb, "LibraryObjectFirstProperty", &[Arg::OutStr, Arg::OutStr], "LibraryObjectNextProperty");
                if !props.is_empty() {
                    o.insert("properties".into(), json!(props.into_iter().collect::<BTreeMap<_, _>>()));
                }
            }
        }
    }
    Value::Object(o)
}

/// Selecciona el objeto de `path`: índice en el recorrido de la página (`PageSelectFirstObjectEx`/`NextObjectEx`,
/// que no entra en grupos) y después, por cada nivel de grupo, índice dentro del grupo
/// (`PageSelectFirstObjectInGroup`/`NextObjectInGroup`, documentados en la ayuda de la automatización).
fn select_path(gb: &mut Session, path: &[usize]) -> Result<()> {
    let Some((&top, inner)) = path.split_first() else { bail!("ruta vacía") };
    gb.m("PageSelectFirstObjectEx", &[])?;
    for _ in 0..top {
        gb.m("PageSelectNextObjectEx", &[])?;
    }
    for &k in inner {
        gb.m("PageSelectFirstObjectInGroup", &[])?;
        for _ in 0..k {
            gb.m("PageSelectNextObjectInGroup", &[])?;
        }
    }
    Ok(())
}

/// Un grupo tiene AN propio y clase `Group` (verificado 2026-11-27 con la op `group`). También se prueban los
/// objetos sin AN ni clase (en `SingleLinesOverview_T300` hay dos; no son grupos: E_HANDLE al entrar).
fn maybe_group(o: &Value) -> bool {
    let class = o["class"].as_str().unwrap_or_default();
    class.eq_ignore_ascii_case("group") || (o["an"].is_null() && class.is_empty())
}

/// Recorre los objetos de la página activa, también los de dentro de grupos (a cualquier profundidad). Los grupos
/// salen como `{class: "Group", path, children}` y sus objetos con `group: <path del grupo>`; `path` son los índices
/// del recorrido (ver `select_path`).
pub(crate) fn page_objects(gb: &mut Session, max: usize, with_props: bool) -> (Vec<Value>, bool) {
    let mut objs = Vec::new();
    let mut seen = HashSet::new();
    let mut groups: Vec<(usize, Vec<usize>)> = Vec::new(); // (índice en objs, ruta)
    let mut r = gb.m("PageSelectFirstObjectEx", &[]);
    let mut i = 0;
    while r.is_ok() {
        if objs.len() >= max {
            return (objs, true);
        }
        let obj = active_object(gb, with_props);
        // Algunas versiones vuelven a empezar al llegar al final en vez de fallar.
        if let Some(an) = obj["an"].as_i64() {
            if an != 0 && !seen.insert(an) {
                break;
            }
        }
        if maybe_group(&obj) {
            groups.push((objs.len(), vec![i]));
        }
        objs.push(obj);
        i += 1;
        r = gb.m("PageSelectNextObjectEx", &[]);
    }
    // Cada grupo se vuelve a seleccionar por su ruta (salir de un grupo deja el recorrido en otro lugar).
    let mut k = 0;
    while k < groups.len() {
        let (gi, path) = groups[k].clone();
        k += 1;
        if let Err(e) = select_path(gb, &path).and_then(|_| gb.m("PageSelectFirstObjectInGroup", &[]).map(|_| ())) {
            // No es un grupo (o no se pudo entrar): se deja dicho, sin AN ni clase no hay más que mostrar.
            objs[gi]["note"] = json!(format!("Graphics Builder no da AN ni clase de este objeto y no se pudo recorrer como grupo: {e:#}"));
            continue;
        }
        let mut children = 0;
        let mut in_group = HashSet::new();
        loop {
            if objs.len() >= max {
                return (objs, true);
            }
            let mut obj = active_object(gb, with_props);
            if let Some(an) = obj["an"].as_i64() {
                if an != 0 && !in_group.insert(an) {
                    break;
                }
            }
            if maybe_group(&obj) {
                let mut p = path.clone();
                p.push(children);
                groups.push((objs.len(), p));
            }
            obj["group"] = json!(path);
            objs.push(obj);
            children += 1;
            if children > 5000 || gb.m("PageSelectNextObjectInGroup", &[]).is_err() {
                break;
            }
        }
        objs[gi]["class"] = json!("Group");
        objs[gi]["path"] = json!(path);
        objs[gi]["children"] = json!(children);
    }
    (objs, false)
}

/// Relleno por expresión (Fill | Color, `PropertiesFillColourPut`): el color del objeto depende del valor de una expresión.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FillSpec {
    /// "on_off" (por defecto: estado 0 = apagado, 1 = encendido) o "multi_state" (estados 0 a 31).
    #[serde(default)]
    pub kind: Option<String>,
    /// Expresión Cicode que decide el color. on_off: condición de encendido (p. ej. "TRF_Red1_Estado == 1").
    /// multi_state: condición A.
    pub expression: String,
    /// Solo multi_state: condiciones B a E (hasta 4 más).
    #[serde(default)]
    pub conditions: Vec<String>,
    /// Color por estado. on_off usa state 0 (apagado) y 1 (encendido); multi_state usa state 0 a 31.
    pub colours: Vec<FillColour>,
}

/// Color de un estado de un relleno por expresión.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FillColour {
    /// Estado (índice del color): on_off 0 o 1; multi_state 0 a 31.
    pub state: i16,
    /// Color "#RRGGBB".
    pub colour: String,
}

/// Apariencia fija de un objeto (General | Appearance). Colores "#RRGGBB". Cada campo vale solo para algunos objetos:
/// `fill_colour` en rectangle/ellipse; `gradient_*` y `corner_radius` en rectangle; `line_*` en rectangle, ellipse y
/// line; `text_colour`, `font`, `font_style` y `align` en text.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Look {
    /// Relleno fijo (la figura queda rellena).
    #[serde(default)]
    pub fill_colour: Option<String>,
    /// Segundo color del degradé (el primero es `fill_colour`). Solo rectangle.
    #[serde(default)]
    pub gradient_colour: Option<String>,
    /// Dirección del degradé: 1 izq→der, 2 der→izq, 3 arriba→abajo, 4 abajo→arriba, 5 bordes horizontales→centro,
    /// 6 centro→bordes horizontales, 7 bordes verticales→centro, 8 centro→bordes verticales (por defecto 3).
    #[serde(default)]
    pub gradient_mode: Option<i16>,
    /// Color del contorno o de la línea.
    #[serde(default)]
    pub line_colour: Option<String>,
    /// Grosor del contorno o de la línea en píxeles (1 a 32; Graphics Builder rechaza 0). Sin contorno visible:
    /// `line_colour` igual a `fill_colour`.
    #[serde(default)]
    pub line_width: Option<i16>,
    /// Radio de las esquinas en píxeles (0 a 32). Solo rectangle.
    #[serde(default)]
    pub corner_radius: Option<i16>,
    /// Color del texto.
    #[serde(default)]
    pub text_colour: Option<String>,
    /// Nombre de la fuente (p. ej. "Arial", "Segoe UI").
    #[serde(default)]
    pub font: Option<String>,
    /// Estilo: suma de 1 negrita, 2 cursiva, 4 subrayado, 8 tachado (0 normal).
    #[serde(default)]
    pub font_style: Option<i16>,
    /// Alineación respecto de x: "left" (por defecto, x es el borde izquierdo), "center" (x es el centro de `text`) o
    /// "right" (x es el borde derecho). Con `expression`, el valor mostrado crece desde ahí hacia ese lado.
    #[serde(default)]
    pub align: Option<String>,
}

/// Nivel de llenado por expresión (Fill | Level, `PropertiesFillLevelPutEx`): la parte llena usa `colour`, el resto
/// `background`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LevelSpec {
    /// Expresión Cicode del nivel (p. ej. un tag en %).
    pub expression: String,
    /// Valor de la expresión con la figura vacía (por defecto 0).
    #[serde(default)]
    pub min: Option<f64>,
    /// Valor de la expresión con la figura llena (por defecto 100).
    #[serde(default)]
    pub max: Option<f64>,
    /// Color de la parte llena "#RRGGBB".
    pub colour: String,
    /// Color de la parte vacía "#RRGGBB".
    pub background: String,
    /// Dirección del llenado: "up" (por defecto), "down", "left" o "right".
    #[serde(default)]
    pub direction: Option<String>,
}

/// Pluma de un objeto de tendencia.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TrendPen {
    /// Nombre del trend tag (tabla `trend`, campo NAME).
    pub expression: String,
    /// Color de la pluma "#RRGGBB".
    pub colour: String,
    /// Color "off" de la pluma (por defecto el mismo que `colour`).
    #[serde(default)]
    pub off_colour: Option<String>,
}

/// Operación de edición de una página.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum PageOp {
    /// Texto en (x, y). Con `expression` el texto mostrado es esa expresión Cicode (p. ej. "\"Secuencia: \" + TRF_Seq");
    /// `text` queda como referencia en el editor.
    Text {
        text: String,
        x: i16,
        y: i16,
        #[serde(default)]
        font_size: Option<i16>,
        /// Expresión Cicode del texto mostrado.
        #[serde(default)]
        expression: Option<String>,
        /// Expresión Cicode: el objeto solo se ve cuando es verdadera (se guarda como "Hidden when" NOT (...)).
        #[serde(default)]
        visible_when: Option<String>,
        /// Color, fuente, estilo y alineación del texto.
        #[serde(default)]
        look: Option<Look>,
    },
    /// Rectángulo de (x, y) a (x2, y2).
    Rectangle {
        x: i16,
        y: i16,
        x2: i16,
        y2: i16,
        #[serde(default)]
        fill: Option<FillSpec>,
        #[serde(default)]
        visible_when: Option<String>,
        /// Relleno fijo, degradé, contorno y esquinas.
        #[serde(default)]
        look: Option<Look>,
        /// Nivel de llenado por expresión (p. ej. el nivel de un tanque).
        #[serde(default)]
        level: Option<LevelSpec>,
    },
    /// Elipse inscripta en (x, y)-(x2, y2).
    Ellipse {
        x: i16,
        y: i16,
        x2: i16,
        y2: i16,
        #[serde(default)]
        fill: Option<FillSpec>,
        #[serde(default)]
        visible_when: Option<String>,
        /// Relleno fijo y contorno.
        #[serde(default)]
        look: Option<Look>,
    },
    /// Línea de (x, y) a (x2, y2).
    Line {
        x: i16,
        y: i16,
        x2: i16,
        y2: i16,
        #[serde(default)]
        visible_when: Option<String>,
        /// Color y grosor de la línea.
        #[serde(default)]
        look: Option<Look>,
    },
    /// Botón de texto en (x, y)-(x2, y2). `command` es el comando Cicode de la acción "Down" (al presionar) y
    /// `up_command` el de "Up" (al soltar). `privilege` (1 a 255) es el privilegio que se pide para operarlo.
    Button {
        x: i16,
        y: i16,
        x2: i16,
        y2: i16,
        text: String,
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        up_command: Option<String>,
        #[serde(default)]
        privilege: Option<u8>,
        #[serde(default)]
        visible_when: Option<String>,
    },
    /// Objeto de tendencia (Trend) de (x, y) a (x2, y2) con 1 a 8 plumas. Cada pluma es un trend tag (tabla
    /// `trend`); el eje vertical de cada una va de ENG_ZERO a ENG_FULL del trend tag. El ancho mostrado es
    /// `samples` muestras de `pixels_per_sample` píxeles (por defecto 2 px y tantas muestras como entren).
    Trend {
        x: i16,
        y: i16,
        x2: i16,
        y2: i16,
        pens: Vec<TrendPen>,
        #[serde(default)]
        samples: Option<i16>,
        #[serde(default)]
        pixels_per_sample: Option<i16>,
        #[serde(default)]
        visible_when: Option<String>,
    },
    /// Genie de una librería (archivo .ctm) con sus propiedades (ver po_gb_library / po_gb_genie_read). (x, y) es donde
    /// queda la esquina superior izquierda del recuadro del genie (Graphics Builder ignora el hotspot al colocarlo).
    Genie {
        library: String,
        element: String,
        x: i16,
        y: i16,
        /// Proyecto que contiene la librería (por defecto se busca en el proyecto y sus incluidos).
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        properties: BTreeMap<String, String>,
        #[serde(default)]
        fill: Option<FillSpec>,
        #[serde(default)]
        visible_when: Option<String>,
    },
    /// Símbolo de una librería (archivo .ctl) en (x, y).
    Symbol {
        library: String,
        element: String,
        x: i16,
        y: i16,
        #[serde(default)]
        project: Option<String>,
        #[serde(default)]
        fill: Option<FillSpec>,
        #[serde(default)]
        visible_when: Option<String>,
    },
    /// Cambia propiedades de un genie ya colocado (por AN, ver po_gb_page_read).
    SetGenieProperties { an: i16, properties: BTreeMap<String, String> },
    /// Relleno, visibilidad o texto por expresión de un objeto ya colocado (por AN, ver po_gb_page_read).
    SetDynamic {
        an: i16,
        #[serde(default)]
        fill: Option<FillSpec>,
        #[serde(default)]
        visible_when: Option<String>,
        #[serde(default)]
        expression: Option<String>,
    },
    /// Cambia el comando Touch (Input | Touch) de un objeto ya colocado (por AN): lee lo actual y cambia solo el
    /// comando; el mensaje de log y la repetición se conservan salvo que se indiquen.
    SetTouch {
        an: i16,
        /// `down` (al presionar) o `up` (al soltar).
        action: String,
        command: String,
        #[serde(default)]
        log_message: Option<String>,
        /// Repetición en ms (por defecto la actual). Con down/up Graphics Builder no la guarda (`after` dice lo guardado).
        #[serde(default)]
        repeat_ms: Option<i16>,
    },
    /// Mueve un objeto (por AN).
    Move { an: i16, x: i16, y: i16 },
    /// Borra un objeto (por AN).
    Delete { an: i16 },
    /// Agrupa objetos ya colocados (por AN, 2 o más): `PageSelectObject` + `PageSelectObjectAdd` +
    /// `PageGroupSelectedObjects`. El grupo no tiene AN; `po_gb_page_read` lo muestra con sus objetos.
    Group { ans: Vec<i16> },
}

impl PageOp {
    fn name(&self) -> &'static str {
        match self {
            PageOp::Text { .. } => "text",
            PageOp::Rectangle { .. } => "rectangle",
            PageOp::Ellipse { .. } => "ellipse",
            PageOp::Line { .. } => "line",
            PageOp::Button { .. } => "button",
            PageOp::Trend { .. } => "trend",
            PageOp::Genie { .. } => "genie",
            PageOp::Symbol { .. } => "symbol",
            PageOp::SetGenieProperties { .. } => "set_genie_properties",
            PageOp::SetDynamic { .. } => "set_dynamic",
            PageOp::SetTouch { .. } => "set_touch",
            PageOp::Move { .. } => "move",
            PageOp::Delete { .. } => "delete",
            PageOp::Group { .. } => "group",
        }
    }
}

fn nonempty(what: &str, v: &str) -> Result<()> {
    if v.trim().is_empty() {
        bail!("{what} no puede estar vacío");
    }
    Ok(())
}

/// Tipo de relleno de PropertiesFillColourPut: 0 = On / Off, 1 = Multi-state.
fn fill_kind(f: &FillSpec) -> Result<i16> {
    match f.kind.as_deref().unwrap_or("on_off") {
        "on_off" => Ok(0),
        "multi_state" => Ok(1),
        other => bail!("kind '{other}' no soportado: usá on_off o multi_state"),
    }
}

/// Valida un relleno (sin tocar Graphics Builder): tipo, expresiones, estados y colores.
fn check_fill(f: &FillSpec) -> Result<()> {
    let kind = fill_kind(f)?;
    nonempty("fill.expression", &f.expression)?;
    if f.colours.is_empty() {
        bail!("El relleno necesita al menos un color en colours");
    }
    if kind == 0 && !f.conditions.is_empty() {
        bail!("fill.conditions solo aplica con kind multi_state");
    }
    let (max_state, max_conditions) = if kind == 0 { (1, 0) } else { (31, 4) };
    if f.conditions.len() > max_conditions {
        bail!("fill.conditions: como máximo {max_conditions} (con expression son {} condiciones)", max_conditions + 1);
    }
    for c in &f.colours {
        if !(0..=max_state).contains(&c.state) {
            bail!("fill.colours: state {} fuera de rango (0 a {max_state} para este kind)", c.state);
        }
        crate::gb::colour_value(&c.colour)?;
    }
    Ok(())
}

/// Alineación de `look.align` al valor de AttributeTextJustification (0 izquierda, 1 derecha, 2 centro).
fn align_value(a: &str) -> Result<i16> {
    match a {
        "left" => Ok(0),
        "right" => Ok(1),
        "center" => Ok(2),
        other => bail!("look.align '{other}' no soportado: usá left, right o center"),
    }
}

/// Dirección de `level.direction` al FillDirection de PropertiesFillLevelPutEx.
fn level_direction(d: Option<&str>) -> Result<i16> {
    match d.unwrap_or("up") {
        "up" => Ok(0),
        "down" => Ok(1),
        "left" => Ok(2),
        "right" => Ok(3),
        other => bail!("level.direction '{other}' no soportado: usá up, down, left o right"),
    }
}

/// Valida un `look` para el tipo de objeto `kind` (rectangle, ellipse, line o text): campos que aplican, rangos y colores.
fn check_look(l: &Look, kind: &str) -> Result<()> {
    let shape = matches!(kind, "rectangle" | "ellipse");
    let campos: [(&str, bool, bool); 10] = [
        ("fill_colour", l.fill_colour.is_some(), shape),
        ("gradient_colour", l.gradient_colour.is_some(), kind == "rectangle"),
        ("gradient_mode", l.gradient_mode.is_some(), kind == "rectangle"),
        ("corner_radius", l.corner_radius.is_some(), kind == "rectangle"),
        ("line_colour", l.line_colour.is_some(), shape || kind == "line"),
        ("line_width", l.line_width.is_some(), shape || kind == "line"),
        ("text_colour", l.text_colour.is_some(), kind == "text"),
        ("font", l.font.is_some(), kind == "text"),
        ("font_style", l.font_style.is_some(), kind == "text"),
        ("align", l.align.is_some(), kind == "text"),
    ];
    for (campo, puesto, vale) in campos {
        if puesto && !vale {
            bail!("look.{campo} no aplica a {kind}");
        }
    }
    for c in [&l.fill_colour, &l.gradient_colour, &l.line_colour, &l.text_colour].into_iter().flatten() {
        crate::gb::colour_value(c)?;
    }
    if l.gradient_colour.is_some() && l.fill_colour.is_none() {
        bail!("look.gradient_colour necesita fill_colour (el degradé va de fill_colour a gradient_colour)");
    }
    if l.gradient_mode.is_some() && l.gradient_colour.is_none() {
        bail!("look.gradient_mode necesita gradient_colour");
    }
    // line_width = 0 da E_INVALIDARG en Graphics Builder (verificado en PO 2024), aunque la doc dice 0 a 32.
    let rangos = [("gradient_mode", l.gradient_mode, 0, 8), ("line_width", l.line_width, 1, 32), ("corner_radius", l.corner_radius, 0, 32), ("font_style", l.font_style, 0, 15)];
    for (campo, v, min, max) in rangos {
        if let Some(v) = v {
            if !(min..=max).contains(&v) {
                bail!("look.{campo} = {v} fuera de rango ({min} a {max})");
            }
        }
    }
    if let Some(f) = &l.font {
        nonempty("look.font", f)?;
    }
    if let Some(a) = &l.align {
        align_value(a)?;
    }
    Ok(())
}

/// Valida un nivel de llenado: expresión, rango y colores.
fn check_level(l: &LevelSpec) -> Result<()> {
    nonempty("level.expression", &l.expression)?;
    let (min, max) = (l.min.unwrap_or(0.0), l.max.unwrap_or(100.0));
    if !(min < max) {
        bail!("level: min ({min}) tiene que ser menor que max ({max})");
    }
    crate::gb::colour_value(&l.colour)?;
    crate::gb::colour_value(&l.background)?;
    level_direction(l.direction.as_deref())?;
    Ok(())
}

/// Valida los campos nuevos de cada operación antes de abrir nada en Graphics Builder.
fn check_op(op: &PageOp) -> Result<()> {
    match op {
        PageOp::Text { look: Some(l), .. } => check_look(l, "text")?,
        PageOp::Rectangle { look, level, .. } => {
            if let Some(l) = look {
                check_look(l, "rectangle")?;
            }
            if let Some(l) = level {
                check_level(l)?;
            }
        }
        PageOp::Ellipse { look: Some(l), .. } => check_look(l, "ellipse")?,
        PageOp::Line { look: Some(l), .. } => check_look(l, "line")?,
        _ => {}
    }
    let (fill, visible, expr) = match op {
        PageOp::Text { expression, visible_when, .. } => (None, visible_when.as_deref(), expression.as_deref()),
        PageOp::Line { visible_when, .. } => (None, visible_when.as_deref(), None),
        PageOp::Rectangle { fill, visible_when, .. }
        | PageOp::Ellipse { fill, visible_when, .. }
        | PageOp::Genie { fill, visible_when, .. }
        | PageOp::Symbol { fill, visible_when, .. } => (fill.as_ref(), visible_when.as_deref(), None),
        PageOp::Button { text, command, up_command, privilege, visible_when, .. } => {
            nonempty("button.text", text)?;
            for (k, c) in [("command", command), ("up_command", up_command)] {
                if let Some(c) = c {
                    nonempty(&format!("button.{k}"), c)?;
                }
            }
            if privilege == &Some(0) {
                bail!("button.privilege va de 1 a 255 (0 no es un privilegio)");
            }
            (None, visible_when.as_deref(), None)
        }
        PageOp::Trend { x, x2, pens, samples, pixels_per_sample, visible_when, .. } => {
            check_trend(*x, *x2, pens, *samples, *pixels_per_sample)?;
            (None, visible_when.as_deref(), None)
        }
        PageOp::SetDynamic { fill, visible_when, expression, .. } => (fill.as_ref(), visible_when.as_deref(), expression.as_deref()),
        PageOp::SetTouch { action, command, repeat_ms, .. } => {
            touch_action(action)?;
            nonempty("command", command)?;
            if let Some(r) = repeat_ms {
                if *r < 1 {
                    bail!("repeat_ms = {r}: tiene que ser 1 o más (ms)");
                }
            }
            (None, None, None)
        }
        PageOp::Group { ans } => {
            let distinct: HashSet<_> = ans.iter().collect();
            if ans.len() < 2 || distinct.len() != ans.len() {
                bail!("ans: hacen falta 2 o más AN distintos");
            }
            (None, None, None)
        }
        PageOp::SetGenieProperties { .. } | PageOp::Move { .. } | PageOp::Delete { .. } => (None, None, None),
    };
    if let Some(v) = visible {
        nonempty("visible_when", v)?;
    }
    if let Some(e) = expr {
        nonempty("expression", e)?;
    }
    if let Some(f) = fill {
        check_fill(f)?;
    }
    Ok(())
}

/// Muestras y píxeles por muestra de un trend (ya validados): por defecto 2 px y tantas muestras como entren en el ancho.
fn trend_samples(x: i16, x2: i16, samples: Option<i16>, pixels: Option<i16>) -> (i16, i16) {
    let px = pixels.unwrap_or(2);
    let n = samples.unwrap_or_else(|| ((x2 - x).abs() / px).max(1));
    (n, px)
}

/// Valida un trend: 1 a 8 plumas con nombre y colores válidos, y rangos de PropertiesTrendPutEx.
fn check_trend(x: i16, x2: i16, pens: &[TrendPen], samples: Option<i16>, pixels: Option<i16>) -> Result<()> {
    if pens.is_empty() || pens.len() > 8 {
        bail!("trend.pens: de 1 a 8 plumas (hay {})", pens.len());
    }
    for (i, p) in pens.iter().enumerate() {
        nonempty(&format!("trend.pens[{i}].expression"), &p.expression)?;
        crate::gb::colour_value(&p.colour)?;
        if let Some(c) = &p.off_colour {
            crate::gb::colour_value(c)?;
        }
    }
    if let Some(px) = pixels {
        if !(1..=32).contains(&px) {
            bail!("trend.pixels_per_sample = {px} fuera de rango (1 a 32)");
        }
    }
    if let Some(n) = samples {
        if n < 1 {
            bail!("trend.samples = {n} fuera de rango (1 a 32767)");
        }
    }
    if x2 == x {
        bail!("trend: x2 tiene que ser distinto de x");
    }
    Ok(())
}

/// Argumentos de PropertiesTrendPutEx: muestras, píxeles por muestra y 8 ternas (pluma, color on, color off);
/// las plumas que no se usan van vacías y en negro.
fn trend_put_args(pens: &[TrendPen], samples: i16, pixels: i16) -> Result<Vec<Arg>> {
    let mut args = vec![Arg::I2(samples), Arg::I2(pixels)];
    for i in 0..8 {
        match pens.get(i) {
            Some(p) => {
                let on = crate::gb::colour_value(&p.colour)?;
                let off = match &p.off_colour {
                    Some(c) => crate::gb::colour_value(c)?,
                    None => on,
                };
                args.extend([s(&p.expression), Arg::I4(on), Arg::I4(off)]);
            }
            None => args.extend([s(""), Arg::I4(0), Arg::I4(0)]),
        }
    }
    Ok(args)
}

/// Valida todas las operaciones; el mensaje indica cuál falla.
/// Acción de Input | Touch para PropertiesInputTouchGet/Put: 1 = Down (al presionar), 0 = Up (al soltar),
/// los mismos valores que usa la op `button` (verificado en pgbutton DOWN_CMD).
fn touch_action(a: &str) -> Result<i16> {
    match a {
        "down" => Ok(1),
        "up" => Ok(0),
        other => bail!("action '{other}' no soportada: usá down (al presionar) o up (al soltar)"),
    }
}

fn validate_ops(ops: &[PageOp]) -> Result<()> {
    for (i, op) in ops.iter().enumerate() {
        check_op(op).map_err(|e| anyhow!("Operación {i} ({}): {e:#}", op.name()))?;
    }
    Ok(())
}

/// Argumentos de PropertiesFillColourPut: tipo, cinco expresiones (A a E, vacías si no se usan), RangeFlag, min y max.
fn fill_put_args(f: &FillSpec) -> Result<Vec<Arg>> {
    let mut conds: Vec<String> = std::iter::once(f.expression.clone()).chain(f.conditions.iter().cloned()).collect();
    conds.resize(5, String::new());
    let mut args = vec![Arg::I2(fill_kind(f)?)];
    args.extend(conds.into_iter().map(Arg::Str));
    args.extend([Arg::Bool(false), Arg::R8(0.0), Arg::R8(0.0)]);
    Ok(args)
}

/// `PropertyVisibility` es el "Hidden when" de Appearance | Visibility (verificado en runtime: con la condición tal
/// cual el objeto se ocultaba justo cuando debía verse), así que `visible_when` se guarda negado.
fn hidden_when(visible_when: &str) -> String {
    format!("NOT ({visible_when})")
}

/// Relleno, visibilidad y texto por expresión del objeto seleccionado. Devuelve qué se aplicó.
fn apply_dynamic(gb: &mut Session, fill: Option<&FillSpec>, visible_when: Option<&str>, expression: Option<&str>) -> Result<Vec<&'static str>> {
    let mut set = Vec::new();
    if let Some(f) = fill {
        gb.m("PropertiesFillColourPut", &fill_put_args(f)?)?;
        // Los colores se fijan después de definir el tipo (el tipo puede reiniciar la lista de colores).
        // on/off: índice 1 = encendido, 0 = apagado; el mismo color para las dos columnas (On y Off).
        for c in &f.colours {
            let v = Arg::I4(crate::gb::colour_value(&c.colour)?);
            gb.m("PropertiesFillColourColourPutEx", &[Arg::I2(c.state), v.clone(), v, Arg::I2(0), Arg::I2(0)])?;
        }
        set.push("fill");
    }
    if let Some(v) = visible_when {
        gb.put("PropertyVisibility", s(&hidden_when(v)))?;
        set.push("visible_when");
    }
    if let Some(e) = expression {
        // Tipo 4 = String: el texto mostrado es la expresión.
        gb.m("PropertiesDisplayValuePut", &[Arg::I2(4), s(e), s(""), s(""), s(""), s("")])?;
        set.push("expression");
    }
    Ok(set)
}

/// Apariencia fija del objeto seleccionado (ya validada con `check_look`). Devuelve qué se aplicó.
/// Colores con las funciones "Ex" (0x00RRGGBB, verificado). El degradé (modo y luego AttributeGradientOn/OffColour)
/// queda guardado tal cual, pero la miniatura de Graphics Builder no lo dibuja: no verificado en runtime.
fn apply_look(gb: &mut Session, l: &Look) -> Result<Vec<&'static str>> {
    let col = |c: &String| crate::gb::colour_value(c).map(Arg::I4);
    let mut set = Vec::new();
    if let Some(c) = &l.fill_colour {
        gb.put("AttributeSetFill", Arg::Bool(true))?;
        gb.put("AttributeFillOnColourEx", col(c)?)?;
        gb.put("AttributeFillOffColourEx", col(c)?)?;
        set.push("fill_colour");
    }
    if let Some(c) = &l.gradient_colour {
        gb.put("AttributeGradientMode", Arg::I2(l.gradient_mode.unwrap_or(3)))?;
        gb.put("AttributeGradientOnColour", col(l.fill_colour.as_ref().unwrap_or(c))?)?;
        gb.put("AttributeGradientOffColour", col(c)?)?;
        set.push("gradient");
    }
    if let Some(c) = &l.line_colour {
        gb.put("AttributeLineOnColourEx", col(c)?)?;
        gb.put("AttributeLineOffColourEx", col(c)?)?;
        set.push("line_colour");
    }
    if let Some(w) = l.line_width {
        gb.put("AttributeLineWidth", Arg::I2(w))?;
        set.push("line_width");
    }
    if let Some(r) = l.corner_radius {
        gb.put("AttributeCornerRadius", Arg::I2(r))?;
        set.push("corner_radius");
    }
    if let Some(c) = &l.text_colour {
        gb.put("AttributeTextOnColourEx", col(c)?)?;
        gb.put("AttributeTextOffColourEx", col(c)?)?;
        set.push("text_colour");
    }
    if let Some(f) = &l.font {
        gb.put("AttributeTextFont", s(f))?;
        set.push("font");
    }
    if let Some(st) = l.font_style {
        gb.put("AttributeTextStyle", Arg::I2(st))?;
        set.push("font_style");
    }
    if let Some(a) = &l.align {
        gb.put("AttributeTextJustification", Arg::I2(align_value(a)?))?;
        set.push("align");
    }
    Ok(set)
}

/// Nivel de llenado del objeto seleccionado: la parte llena es el relleno del objeto (`colour`), el resto el fondo.
fn apply_level(gb: &mut Session, l: &LevelSpec) -> Result<()> {
    let colour = Arg::I4(crate::gb::colour_value(&l.colour)?);
    let bg = Arg::I4(crate::gb::colour_value(&l.background)?);
    gb.put("AttributeSetFill", Arg::Bool(true))?;
    gb.put("AttributeFillOnColourEx", colour.clone())?;
    gb.put("AttributeFillOffColourEx", colour)?;
    gb.m(
        "PropertiesFillLevelPutEx",
        &[
            s(&l.expression),
            Arg::Bool(true),
            Arg::R8(l.min.unwrap_or(0.0)),
            Arg::R8(l.max.unwrap_or(100.0)),
            Arg::I2(0),
            Arg::I2(100),
            Arg::I2(level_direction(l.direction.as_deref())?),
            bg.clone(),
            bg,
        ],
    )?;
    Ok(())
}

/// Proyecto que tiene la librería `library` con la extensión dada, buscando en `projects` (en orden).
pub(crate) fn library_project(projects: &[Project], library: &str, ext: &str) -> Option<String> {
    projects.iter().find(|p| project::find_file(&p.path, &format!("{library}.{ext}")).is_some()).map(|p| p.name.clone())
}

/// Aplica operaciones sobre la página activa. Devuelve un resumen por operación.
fn apply_ops(gb: &mut Session, ops: &[PageOp], libs: &[Project]) -> Result<Vec<Value>> {
    let mut done = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let r: Result<Value> = (|| {
            Ok(match op {
                PageOp::Text { text, x, y, font_size, expression, visible_when, look } => {
                    gb.m("DrawText", &[s(text), Arg::I2(*x), Arg::I2(*y)])?;
                    if let Some(sz) = font_size {
                        gb.put("AttributeTextFontSize", Arg::I2(*sz))?;
                    }
                    let mut set = match look {
                        Some(l) => apply_look(gb, l)?,
                        None => Vec::new(),
                    };
                    // La justificación solo dice hacia dónde crece el texto desde donde quedó dibujado (empieza en x):
                    // para que x sea el centro o el borde derecho se corre según el ancho de `text`, antes de la
                    // expresión (verificado 2026-10-16: sin esto, "center" quedaba con el borde izquierdo en x).
                    if let Some(a @ ("center" | "right")) = look.as_ref().and_then(|l| l.align.as_deref()) {
                        let int = |gb: &mut Session, m: &str| gb.m(m, &[]).ok().and_then(|r| r.ret.int());
                        if let (Some(x0), Some(x1), Some(y0)) = (int(gb, "AttributeX"), int(gb, "AttributeExtentX"), int(gb, "AttributeY")) {
                            let w = (x1 - x0).max(0);
                            let nx = if a == "center" { i64::from(*x) - w / 2 } else { i64::from(*x) - w };
                            gb.m("PositionAt", &[Arg::I2(nx as i16), Arg::I2(y0 as i16)])?;
                            set.push("anchored");
                        }
                    }
                    set.extend(apply_dynamic(gb, None, visible_when.as_deref(), expression.as_deref())?);
                    json!({"an": gb.m("AttributeAN", &[])?.ret.json(), "applied": set})
                }
                PageOp::Rectangle { x, y, x2, y2, fill, visible_when, look, .. } | PageOp::Ellipse { x, y, x2, y2, fill, visible_when, look } => {
                    let m = if matches!(op, PageOp::Rectangle { .. }) { "DrawRectangle" } else { "DrawEllipse" };
                    gb.m(m, &[Arg::I2(*x), Arg::I2(*y), Arg::I2(*x2), Arg::I2(*y2)])?;
                    let mut set = match look {
                        Some(l) => apply_look(gb, l)?,
                        None => Vec::new(),
                    };
                    // Sin AttributeSetFill la figura es solo contorno y el color del relleno no se ve (verificado en runtime).
                    if fill.is_some() {
                        gb.put("AttributeSetFill", Arg::Bool(true))?;
                    }
                    set.extend(apply_dynamic(gb, fill.as_ref(), visible_when.as_deref(), None)?);
                    if let PageOp::Rectangle { level: Some(l), .. } = op {
                        apply_level(gb, l)?;
                        set.push("level");
                    }
                    json!({"an": gb.m("AttributeAN", &[])?.ret.json(), "applied": set})
                }
                PageOp::Line { x, y, x2, y2, visible_when, look } => {
                    gb.m("DrawLine", &[Arg::I2(*x), Arg::I2(*y), Arg::I2(*x2), Arg::I2(*y2)])?;
                    let mut set = match look {
                        Some(l) => apply_look(gb, l)?,
                        None => Vec::new(),
                    };
                    set.extend(apply_dynamic(gb, None, visible_when.as_deref(), None)?);
                    json!({"an": gb.m("AttributeAN", &[])?.ret.json(), "applied": set})
                }
                PageOp::Button { x, y, x2, y2, text, command, up_command, privilege, visible_when } => {
                    gb.m("DrawButton", &[Arg::I2(*x), Arg::I2(*y), Arg::I2(*x2), Arg::I2(*y2)])?;
                    // Tipo 0 = texto. La fuente del botón se conserva (si no se puede leer, la por defecto).
                    let font = gb
                        .m("PropertiesButtonGet", &[Arg::OutI2, Arg::OutStr, Arg::OutStr, Arg::OutStr, Arg::OutStr])
                        .map(|r| r.outs[2].text())
                        .unwrap_or_default();
                    gb.m("PropertiesButtonPut", &[Arg::I2(0), s(text), s(&font), s(""), s("")])?;
                    let mut set = vec!["text"];
                    // Comandos Touch: acción 1 = Down (al presionar), 0 = Up (al soltar). Repetición mínima 1 ms.
                    if let Some(c) = command {
                        gb.m("PropertiesInputTouchPut", &[Arg::I2(1), s(c), s(""), Arg::I2(1)])?;
                        set.push("command");
                    }
                    if let Some(c) = up_command {
                        gb.m("PropertiesInputTouchPut", &[Arg::I2(0), s(c), s(""), Arg::I2(1)])?;
                        set.push("up_command");
                    }
                    // El privilegio está en Access | General (la API de Touch no lo tiene): se lee lo actual y se cambia solo eso.
                    if let Some(p) = privilege {
                        let g = gb.m("PropertiesAccessGeneralGet", &[Arg::OutStr, Arg::OutStr, Arg::OutStr, Arg::OutI2, Arg::OutI2, Arg::OutStr])?;
                        let area = g.outs[3].int().unwrap_or(0) as i16;
                        gb.m(
                            "PropertiesAccessGeneralPut",
                            &[s(&g.outs[0].text()), s(&g.outs[1].text()), s(&g.outs[2].text()), Arg::I2(area), Arg::I2(*p as i16), s(&g.outs[5].text())],
                        )?;
                        set.push("privilege");
                    }
                    let dynamic = apply_dynamic(gb, None, visible_when.as_deref(), None)?;
                    set.extend(dynamic);
                    json!({"an": gb.m("AttributeAN", &[])?.ret.json(), "applied": set})
                }
                PageOp::Trend { x, y, x2, y2, pens, samples, pixels_per_sample, visible_when } => {
                    gb.m("DrawTrend", &[Arg::I2(*x), Arg::I2(*y), Arg::I2(*x2), Arg::I2(*y2)])?;
                    let (n, px) = trend_samples(*x, *x2, *samples, *pixels_per_sample);
                    gb.m("PropertiesTrendPutEx", &trend_put_args(pens, n, px)?)?;
                    let mut set = vec!["pens"];
                    set.extend(apply_dynamic(gb, None, visible_when.as_deref(), None)?);
                    json!({"an": gb.m("AttributeAN", &[])?.ret.json(), "samples": n, "pixels_per_sample": px, "applied": set})
                }
                PageOp::Genie { library, element, x, y, fill, visible_when, .. } | PageOp::Symbol { library, element, x, y, fill, visible_when, .. } => {
                    let (lib_proj, kind) = op_library(op, libs).unwrap_or_else(|| Err(anyhow!("op sin librería")))?;
                    // Con la librería fuera del alcance del proyecto seleccionado falla con 0x80004003 (ver with_project).
                    gb.m("LibraryObjectPlaceEx", &[s(&lib_proj), s(library), s(element), Arg::I2(kind), Arg::Bool(true), Arg::I2(*x), Arg::I2(*y)])
                        .map_err(|e| {
                            let sel = gb.m("ProjectSelected", &[]).map(|r| r.ret.text()).unwrap_or_default();
                            anyhow!("{e:#} (librería {lib_proj}/{library}, proyecto seleccionado en Graphics Builder: {sel})")
                        })?;
                    let mut set = Vec::new();
                    if let PageOp::Genie { properties, .. } = op {
                        for (k, v) in properties {
                            gb.m("LibraryObjectPutProperty", &[s(k), s(v)])?;
                            set.push(k.clone());
                        }
                    }
                    let dynamic = apply_dynamic(gb, fill.as_ref(), visible_when.as_deref(), None)?;
                    json!({"an": gb.m("AttributeAN", &[])?.ret.json(), "library_project": lib_proj, "properties_set": set, "applied": dynamic})
                }
                PageOp::SetGenieProperties { an, properties } => {
                    gb.m("PageSelectObject", &[Arg::I2(*an)])?;
                    for (k, v) in properties {
                        gb.m("LibraryObjectPutProperty", &[s(k), s(v)])?;
                    }
                    json!({"an": an})
                }
                PageOp::SetDynamic { an, fill, visible_when, expression } => {
                    gb.m("PageSelectObject", &[Arg::I2(*an)])?;
                    // Rectángulos y elipses necesitan estar rellenos para que se vea el color; en genies/símbolos
                    // AttributeSetFill puede no aplicar, así que es a mejor esfuerzo.
                    if fill.is_some() {
                        let _ = gb.put("AttributeSetFill", Arg::Bool(true));
                    }
                    let set = apply_dynamic(gb, fill.as_ref(), visible_when.as_deref(), expression.as_deref())?;
                    json!({"an": an, "applied": set})
                }
                PageOp::SetTouch { an, action, command, log_message, repeat_ms } => {
                    gb.m("PageSelectObject", &[Arg::I2(*an)])?;
                    let a = touch_action(action)?;
                    // Se lee lo actual para no pisar el mensaje de log ni la repetición: si no se puede leer, no se escribe.
                    let cur = gb
                        .m("PropertiesInputTouchGet", &[Arg::I2(a), Arg::OutStr, Arg::OutStr, Arg::OutI2])
                        .map_err(|e| anyhow!("{e:#} (no se pudo leer el touch actual del AN {an}; no se cambió nada)"))?;
                    let (old_cmd, old_log) = (cur.outs[0].text(), cur.outs[1].text());
                    let old_rep = cur.outs[2].int().unwrap_or(1) as i16;
                    let log = log_message.clone().unwrap_or_else(|| old_log.clone());
                    // Repetición mínima 1 ms (como en `button`): un 0 leído se escribe como 1.
                    let rep = repeat_ms.unwrap_or(old_rep).max(1);
                    gb.m("PropertiesInputTouchPut", &[Arg::I2(a), s(command), s(&log), Arg::I2(rep)])?;
                    // `after` es lo que Graphics Builder devuelve después de escribir, no lo pedido: con Down/Up la
                    // repetición se lee 0 y no se guarda (RPT_RATE vacío en pgbutton/pgtouch, verificado 2026-11-26).
                    let after = match gb.m("PropertiesInputTouchGet", &[Arg::I2(a), Arg::OutStr, Arg::OutStr, Arg::OutI2]) {
                        Ok(r) => json!({"command": r.outs[0].text(), "log_message": r.outs[1].text(),
                                        "repeat_ms": r.outs[2].int().unwrap_or(0)}),
                        Err(e) => json!({"command": command, "log_message": log, "repeat_ms": rep,
                                         "unverified": format!("no se pudo releer: {e:#}")}),
                    };
                    let mut out = json!({"an": an, "action": action,
                        "before": {"command": old_cmd, "log_message": old_log, "repeat_ms": old_rep}, "after": after});
                    if after["command"].as_str().is_some_and(|c| c != command) {
                        out["warning"] = json!("Graphics Builder devolvió otro comando después de escribir: revisar after.command");
                    }
                    if repeat_ms.is_some() && after["repeat_ms"].as_i64() != Some(rep as i64) {
                        out["note"] = json!("repeat_ms no quedó guardado: con Down/Up Graphics Builder no guarda la repetición (ver after)");
                    }
                    out
                }
                PageOp::Move { an, x, y } => {
                    gb.m("PageSelectObject", &[Arg::I2(*an)])?;
                    gb.m("PositionAt", &[Arg::I2(*x), Arg::I2(*y)])?;
                    json!({"an": an})
                }
                PageOp::Delete { an } => {
                    gb.m("PageSelectObject", &[Arg::I2(*an)])?;
                    gb.m("PageDeleteObject", &[])?;
                    json!({"an": an, "deleted": true})
                }
                PageOp::Group { ans } => {
                    gb.m("PageSelectObject", &[Arg::I2(ans[0])])?;
                    for an in &ans[1..] {
                        gb.m("PageSelectObjectAdd", &[Arg::I2(*an)])?;
                    }
                    gb.m("PageGroupSelectedObjects", &[])?;
                    json!({"ans": ans, "grouped": true})
                }
            })
        })();
        match r {
            Ok(mut v) => {
                v["index"] = json!(i);
                v["op"] = json!(op.name());
                done.push(v);
            }
            Err(e) => bail!("Operación {i} ({}) falló: {e:#}. Las anteriores ya se aplicaron a la página (sin guardar).", op.name()),
        }
    }
    Ok(done)
}

/// Proyecto de la librería y tipo (LIB_GENIE / LIB_SYMBOL) de una op `genie` o `symbol`; None para las demás.
fn op_library(op: &PageOp, libs: &[Project]) -> Option<Result<(String, i16)>> {
    let (library, project, genie) = match op {
        PageOp::Genie { library, project, .. } => (library, project, true),
        PageOp::Symbol { library, project, .. } => (library, project, false),
        _ => return None,
    };
    let ext = if genie { "ctm" } else { "ctl" };
    let lib_proj = project
        .clone()
        .or_else(|| library_project(libs, library, ext))
        .ok_or_else(|| anyhow!("No se encontró la librería {library}.{ext} en el proyecto ni en sus incluidos"));
    Some(lib_proj.map(|p| (p, if genie { LIB_GENIE } else { LIB_SYMBOL })))
}

/// Proyectos de las librerías que usan las ops `genie`/`symbol`.
fn ops_lib_projects(ops: &[PageOp], libs: &[Project]) -> Vec<String> {
    ops.iter().filter_map(|op| op_library(op, libs)?.ok()).map(|(p, _)| p).collect()
}

/// Proyectos cuyas librerías ve Graphics Builder con `selected` seleccionado: él y sus incluidos (siempre `Include`).
fn visible_projects(cat: &Catalog, selected: &str) -> Vec<String> {
    match cat.projects.iter().find(|p| p.name.eq_ignore_ascii_case(selected)) {
        Some(p) => cat.with_includes(p).0.into_iter().map(|p| p.name).collect(),
        None => vec![selected.to_string(), "Include".into()],
    }
}

/// Graphics Builder solo usa las librerías (.ctm/.ctl) del proyecto seleccionado y de sus incluidos: con otro
/// seleccionado, `LibraryObjectPlaceEx` falla con 0x80004003 y `LibraryFirstObject` no lista nada (salvo librerías que
/// ya cargó antes: las lista igual, pero colocar sigue fallando; verificado el 2026-10-16). Si alguno de `needs`
/// (proyectos de librerías) no está entre el seleccionado y sus incluidos, selecciona `project`, corre `f` y vuelve a
/// seleccionar el anterior. `ProjectSelect` es asíncrono (~200 ms), cambia también el proyecto activo de Studio
/// (`LASTDATABASE` del citect.ini) y no hace nada si Graphics Builder tiene páginas abiertas: por eso `f` abre y
/// cierra la página.
pub(crate) fn with_project(gb: &mut Session, project: &str, cat: &Catalog, needs: &[String], f: impl FnOnce(&mut Session) -> Result<Value>) -> Result<Value> {
    let prev = gb.m("ProjectSelected", &[]).map(|r| r.ret.text()).unwrap_or_default();
    if prev.is_empty() || prev.eq_ignore_ascii_case(project) {
        return f(gb);
    }
    let visible = visible_projects(cat, &prev);
    if needs.iter().all(|n| visible.iter().any(|v| v.eq_ignore_ascii_case(n))) {
        return f(gb);
    }
    if let Err(e) = select_project(gb, project) {
        let _ = gb.m("ProjectSelect", &[s(&prev)]);
        bail!(
            "Graphics Builder tiene seleccionado {prev} y desde ahí no ve las librerías de {project}; {e:#}. Graphics Builder no \
             cambia de proyecto con páginas abiertas: cerralas (po_gb_status → open_pages, incluida la Untitled) o seleccioná \
             {project} en Studio, y reintentá."
        );
    }
    let r = f(gb);
    let restored = select_project(gb, &prev);
    let restore_msg = restored.as_ref().err().map(|e| {
        format!("No se pudo volver a seleccionar {prev} en Graphics Builder ({e:#}): quedó {project}, también como proyecto activo de Studio. Cerrá las páginas abiertas y elegí {prev} en Studio.")
    });
    let mut v = match r {
        Ok(v) => v,
        Err(e) => return Err(match restore_msg {
            Some(m) => e.context(m),
            None => e,
        }),
    };
    gb.warnings.extend(restore_msg);
    let mut warnings = v["warnings"].as_array().cloned().unwrap_or_default();
    warnings.extend(std::mem::take(&mut gb.warnings).into_iter().map(Value::String));
    v["warnings"] = json!(warnings);
    v["gb_project_switch"] = json!({"selected": project, "previous": prev, "restored": restored.is_ok()});
    Ok(v)
}

/// Páginas abiertas en Graphics Builder (`{page, project, modified, unsaved?}`) y la activa: recorre las ventanas y
/// vuelve a la activa.
pub(crate) fn open_pages(gb: &mut Session) -> (Vec<Value>, Option<String>) {
    let active = gb.m("PageName", &[]).ok().map(|r| r.ret.text());
    let active_project = gb.m("PageAppearanceGet", &[Arg::OutStr, Arg::OutStr, Arg::OutStr, Arg::OutI2, Arg::OutBool, Arg::OutI2, Arg::OutI2, Arg::OutI2]).ok().map(|r| r.outs[0].text());
    let mut pages: Vec<Value> = Vec::new();
    if active.as_deref().is_some_and(|a| !a.is_empty()) && gb.m("PageSelectFirst", &[]).is_ok() {
        let mut seen = HashSet::new();
        loop {
            let name = gb.m("PageName", &[]).map(|r| r.ret.text()).unwrap_or_default();
            let ap = appearance(gb);
            let key = format!("{}/{name}", ap["project"].as_str().unwrap_or(""));
            if !seen.insert(key) || pages.len() > 50 {
                break;
            }
            let dirty = gb.get("PageIsDirty").ok().map(|v| v.json());
            let mut page = json!({"page": name, "project": page_project(&name, &ap["project"]), "modified": dirty});
            if is_untitled(&name) {
                page["unsaved"] = json!(true);
            }
            pages.push(page);
            if gb.m("PageSelectNext", &[]).is_err() {
                break;
            }
        }
        if let (Some(a), Some(ap)) = (&active, &active_project) {
            let _ = gb.m("PageSelect", &[s(ap), s(a)]);
        }
    }
    (pages, active)
}

/// Proyectos que lista Graphics Builder (`ProjectFirst`/`ProjectNext`).
pub(crate) fn gb_projects(gb: &mut Session) -> Vec<String> {
    let mut projects = Vec::new();
    let mut r = gb.m("ProjectFirst", &[]);
    while let Ok(rep) = r {
        let n = rep.ret.text();
        if n.is_empty() || projects.len() > 500 {
            break;
        }
        projects.push(n);
        r = gb.m("ProjectNext", &[]);
    }
    projects
}

/// `ProjectSelect` y espera hasta 10 s a que `ProjectSelected` lo refleje.
pub(crate) fn select_project(gb: &mut Session, project: &str) -> Result<()> {
    gb.m("ProjectSelect", &[s(project)])?;
    let t0 = std::time::Instant::now();
    loop {
        let cur = gb.m("ProjectSelected", &[]).map(|r| r.ret.text()).unwrap_or_default();
        if cur.eq_ignore_ascii_case(project) {
            return Ok(());
        }
        if t0.elapsed() > std::time::Duration::from_secs(10) {
            bail!("ProjectSelect({project}) no lo seleccionó en 10 s (sigue {cur})");
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Nombre de librería o de genie: sin separadores de ruta ni puntos (Graphics Builder arma "<librería>.<genie>").
fn check_lib_name(what: &str, v: &str) -> Result<()> {
    nonempty(what, v)?;
    if v.len() > 64 || v.chars().any(|c| matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '.') || c.is_control()) {
        bail!("{what} inválido: \"{v}\" (sin \\ / : * ? \" < > | . y hasta 64 caracteres)");
    }
    Ok(())
}

/// Elementos (genies o símbolos) de una librería.
fn library_elements(gb: &mut Session, project: &str, library: &str, kind: i16) -> Vec<String> {
    let mut items = Vec::new();
    let mut r = gb.m("LibraryFirstObject", &[s(project), s(library), Arg::I2(kind), Arg::OutStr]);
    while let Ok(rep) = r {
        let n = rep.outs[0].text();
        if n.is_empty() || items.len() >= 1000 {
            break;
        }
        items.push(n);
        r = gb.m("LibraryNextObject", &[Arg::OutStr]);
    }
    items
}

fn genie_exists(gb: &mut Session, project: &str, library: &str, genie: &str) -> bool {
    library_elements(gb, project, library, LIB_GENIE).iter().any(|n| n.eq_ignore_ascii_case(genie))
}

/// Hotspot (punto de inserción) de la página de genie activa. El tipo de los parámetros de salida no está documentado:
/// se prueba I2 y después I4.
/// Recuadro que encierra los objetos (de `page_objects`): {x, y, x2, y2} o Null si no hay.
fn objects_bbox(objects: &[Value]) -> Value {
    let (mut x0, mut y0, mut x1, mut y1) = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
    for o in objects {
        for v in ["x", "x2"].iter().filter_map(|k| o[*k].as_i64()) {
            x0 = x0.min(v);
            x1 = x1.max(v);
        }
        for v in ["y", "y2"].iter().filter_map(|k| o[*k].as_i64()) {
            y0 = y0.min(v);
            y1 = y1.max(v);
        }
    }
    if x0 > x1 || y0 > y1 {
        return Value::Null;
    }
    json!({"x": x0, "y": y0, "x2": x1, "y2": y1})
}

fn hotspot(gb: &mut Session) -> Value {
    for args in [[Arg::OutI2, Arg::OutI2], [Arg::OutI4, Arg::OutI4]] {
        if let Ok(r) = gb.m("LibraryObjectHotspotGet", &args) {
            gb.warnings.retain(|w| !w.starts_with("LibraryObjectHotspotGet"));
            return json!({"x": r.outs[0].json(), "y": r.outs[1].json()});
        }
    }
    gb.warnings.retain(|w| !w.starts_with("LibraryObjectHotspotGet"));
    Value::Null
}

/// Propiedades (nombre → valor por defecto) de un genie, leídas de la librería.
fn library_genie_properties(gb: &mut Session, project: &str, library: &str, genie: &str) -> Vec<(String, String)> {
    genie_properties(gb, "LibraryObjectFirstPropertyEx", &[s(project), s(library), s(genie), Arg::OutStr, Arg::OutStr], "LibraryObjectNextPropertyEx")
}

/// Si el genie está abierto como página en Graphics Builder (se llama "<librería>.<genie>"), lo deja activo y devuelve
/// true. No distingue proyectos: ante la duda se trata como abierto.
fn genie_is_open(gb: &mut Session, library: &str, genie: &str) -> bool {
    let want = format!("{library}.{genie}");
    let mut found = false;
    if gb.m("PageSelectFirst", &[]).is_ok() {
        let mut seen = HashSet::new();
        loop {
            let name = gb.m("PageName", &[]).map(|r| r.ret.text()).unwrap_or_default();
            if name.eq_ignore_ascii_case(&want) {
                found = true;
                break;
            }
            if !seen.insert(name.to_ascii_lowercase()) || seen.len() > 50 || gb.m("PageSelectNext", &[]).is_err() {
                break;
            }
        }
    }
    gb.warnings.retain(|w| !w.starts_with("PageSelect") && !w.starts_with("PageName"));
    found
}

/// `.ctm` de la librería en el proyecto donde se escribe (`libs[0]`). Si la librería está solo en un proyecto incluido,
/// error: el MCP escribe únicamente librerías del proyecto indicado.
fn own_ctm(libs: &[Project], library: &str) -> Result<Option<std::path::PathBuf>> {
    let Some(proj) = libs.first() else { bail!("sin proyecto") };
    if let Some(f) = project::find_file(&proj.path, &format!("{library}.ctm")) {
        return Ok(Some(f));
    }
    if let Some(other) = library_project(&libs[1..], library, "ctm") {
        bail!(
            "La librería {library} es del proyecto incluido {other}, no de {}: el MCP solo escribe librerías del proyecto indicado \
             (las de sistema, como PLS_Include, nunca). Usá una librería propia del proyecto.",
            proj.name
        );
    }
    Ok(None)
}

const GENIE_OPEN: &str = "está abierto en Graphics Builder: guardalo o cerralo ahí y repetí (el MCP no toca un genie abierto para no perder cambios)";

/// Desactiva el formulario de propiedades al crear objetos y lo restaura al terminar.
fn with_quiet_new<T>(gb: &mut Session, f: impl FnOnce(&mut Session) -> Result<T>) -> Result<T> {
    let prev = gb.get("OptionDisplayPropertiesOnNew").ok();
    let _ = gb.put("OptionDisplayPropertiesOnNew", Arg::Bool(false));
    let r = f(gb);
    if let Some(Val::Bool(b)) = prev {
        let _ = gb.put("OptionDisplayPropertiesOnNew", Arg::Bool(b));
    }
    r
}

/// Templates (nombre, resolución, barra de título) de un estilo.
fn templates(gb: &mut Session, project: &str, style: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut r = gb.m("LibraryFirstTemplate", &[s(project), s(style), Arg::OutStr, Arg::OutI2, Arg::OutBool]);
    while let Ok(rep) = r {
        let name = rep.outs[0].text();
        if name.is_empty() || out.len() >= 200 {
            break;
        }
        out.push(json!({"template": name, "resolution": rep.outs[1].json(), "titlebar": rep.outs[2].json()}));
        r = gb.m("LibraryNextTemplate", &[Arg::OutStr, Arg::OutI2, Arg::OutBool]);
    }
    out
}

pub(crate) fn warnings_json(gb: &mut Session) -> Value {
    json!(std::mem::take(&mut gb.warnings))
}

/// Graphics Builder abre `<proyecto>\GLOBAL.CTL` (librería global de símbolos) para saber la versión del proyecto:
/// si no está, lo trata como anterior a la 5.31 y muestra "Cannot convert '<proyecto>' to version 8.50 format"
/// cada vez que se lo selecciona (verificado en CtDraw32.exe de PO 2024).
pub(crate) fn global_ctl_hint(proj: &Project) -> Option<String> {
    project::find_file(&proj.path, "GLOBAL.CTL").is_none().then(|| {
        format!(
            "{} no tiene GLOBAL.CTL: Graphics Builder lo toma como un proyecto anterior a la 5.31 y muestra \"Cannot convert '{}' to version 8.50 format\" \
             (no aceptarlo: cierra Graphics Builder). Se arregla copiando un GLOBAL.CTL vacío (352 bytes) de otro proyecto, p. ej. Library_Controls.",
            proj.name, proj.name
        )
    })
}

/// Página nueva sin guardar ("Untitled", "Untitled1"...).
fn is_untitled(page: &str) -> bool {
    page.trim().to_ascii_lowercase().starts_with("untitled")
}

/// Proyecto de una página abierta para `po_gb_status`. Para una `Untitled` Graphics Builder devuelve basura en el
/// proyecto de `PageAppearanceGetEx` (visto en PO 2024): no tiene proyecto todavía, así que va null. También null
/// si el texto no parece un nombre (vacío, caracteres de control o inválidos).
fn page_project(page: &str, raw: &Value) -> Value {
    match raw.as_str() {
        Some(p) if !is_untitled(page) && !p.trim().is_empty() && !p.chars().any(|c| c.is_control() || c == '\u{FFFD}') => json!(p),
        _ => Value::Null,
    }
}

/// Agrega la advertencia de `global_ctl_hint` a `v["warnings"]`.
fn add_hint(v: &mut Value, hint: Option<String>) {
    if let Some(h) = hint {
        match v["warnings"].as_array_mut() {
            Some(w) => w.push(json!(h)),
            None => v["warnings"] = json!([h]),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct GbStatusParams {
    /// Abrir Graphics Builder si no está abierto (tarda ~20 s; abre una página "Untitled" que no se guarda).
    #[serde(default)]
    pub launch: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct GbLibraryParams {
    /// Proyecto (vacío = POWEROP_PROJECT / el que corre en runtime).
    #[serde(default)]
    pub project: Option<String>,
    /// Estilo de templates (archivo .ctt, p. ej. "standard"): lista sus templates con resolución.
    #[serde(default)]
    pub style: Option<String>,
    /// Librería (archivo .ctm de genies o .ctl de símbolos, sin extensión): lista sus elementos.
    #[serde(default)]
    pub library: Option<String>,
    /// Con `library`: "genie" (por defecto) o "symbol".
    #[serde(default)]
    pub kind: Option<String>,
    /// Con `library`: genie del que se quieren las propiedades (nombre → valor por defecto).
    #[serde(default)]
    pub element: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GbPageParams {
    /// Proyecto de la página (vacío = POWEROP_PROJECT / el que corre en runtime).
    #[serde(default)]
    pub project: Option<String>,
    /// Nombre de la página (sin .ctg).
    pub page: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct GbPageReadParams {
    #[serde(default)]
    pub project: Option<String>,
    pub page: String,
    /// Máximo de objetos (por defecto 300).
    #[serde(default)]
    pub max_objects: Option<usize>,
    /// Incluir las propiedades de cada genie (por defecto true).
    #[serde(default)]
    pub genie_properties: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GbPageCreateParams {
    #[serde(default)]
    pub project: Option<String>,
    /// Nombre de la página nueva.
    pub page: String,
    /// Estilo de template (por defecto "standard"; ver po_gb_library con `style`).
    #[serde(default)]
    pub style: Option<String>,
    /// Template (por defecto "normal").
    #[serde(default)]
    pub template: Option<String>,
    /// Resolución del template (por defecto la primera disponible para ese template).
    #[serde(default)]
    pub resolution: Option<i16>,
    /// Barra de título (por defecto la del template elegido).
    #[serde(default)]
    pub titlebar: Option<bool>,
    /// Título de la página.
    #[serde(default)]
    pub title: Option<String>,
    /// Operaciones a aplicar antes de guardar (texto, rectángulos, botones, genies, relleno/visibilidad por expresión...).
    #[serde(default)]
    pub ops: Vec<PageOp>,
    /// Si la página ya existe, la recrea: copia el .ctg a _mcp_backup y guarda la nueva encima (PageSaveAs). Sin esto, error.
    #[serde(default)]
    pub replace: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct GbPageEditParams {
    #[serde(default)]
    pub project: Option<String>,
    pub page: String,
    /// Operaciones en orden. Coordenadas en píxeles de la página.
    pub ops: Vec<PageOp>,
    /// Guardar al terminar (por defecto true).
    #[serde(default)]
    pub save: Option<bool>,
}

/// Punto en píxeles de la página del genie.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub x: i16,
    pub y: i16,
}

#[derive(Deserialize, JsonSchema)]
pub struct GbGenieParams {
    /// Proyecto (vacío = POWEROP_PROJECT / el que corre en runtime). Para leer se busca la librería también en sus incluidos;
    /// para escribir o borrar, la librería tiene que ser de este proyecto.
    #[serde(default)]
    pub project: Option<String>,
    /// Librería de genies (archivo .ctm, sin extensión).
    pub library: String,
    /// Nombre del genie.
    pub genie: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct GbGenieReadParams {
    #[serde(default)]
    pub project: Option<String>,
    pub library: String,
    pub genie: String,
    /// Máximo de objetos (por defecto 300).
    #[serde(default)]
    pub max_objects: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GbGenieCreateParams {
    #[serde(default)]
    pub project: Option<String>,
    /// Librería de genies (archivo .ctm, sin extensión). Si no existe en el proyecto, se crea.
    pub library: String,
    /// Nombre del genie nuevo.
    pub genie: String,
    /// Objetos del genie, en píxeles desde (0, 0) (mismas operaciones que po_gb_page_create). Cada `%Nombre%` que
    /// aparezca en textos, expresiones, comandos o propiedades se vuelve una propiedad del genie, que se completa al
    /// colocarlo en una página (op `genie` con `properties`).
    pub ops: Vec<PageOp>,
    /// Punto de inserción guardado en el genie. Al colocarlo con la op `genie` Graphics Builder lo ignora (verificado
    /// 2026-10-16): (x, y) es la esquina superior izquierda del recuadro (`bbox`) del genie.
    #[serde(default)]
    pub hotspot: Option<Point>,
    /// Si el genie ya existe, lo reemplaza (copia previa del .ctm en _mcp_backup). Sin esto, error.
    #[serde(default)]
    pub replace: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct GbGenieEditParams {
    #[serde(default)]
    pub project: Option<String>,
    pub library: String,
    pub genie: String,
    /// Operaciones en orden (mismas que po_gb_page_edit; `an` según po_gb_genie_read).
    #[serde(default)]
    pub ops: Vec<PageOp>,
    /// Mover el punto de inserción del genie.
    #[serde(default)]
    pub hotspot: Option<Point>,
}

#[tool_router(router = graphics_router, vis = "pub(crate)")]
impl PowerOpServer {
    #[tool(name = "po_gb_status", description = "Graphics Builder (editor de páginas gráficas) por COM: conecta (o lo abre con launch=true) y muestra proyecto seleccionado, proyectos y páginas abiertas. Usalo antes de las demás herramientas gb_*.")]
    async fn gb_status(&self, Parameters(p): Parameters<GbStatusParams>) -> ToolResult {
        let exe = if p.launch { blocking(std::time::Duration::from_secs(60), || Ok(crate::gb::ctdraw_exe())).await.map_err(err)? } else { None };
        if p.launch && exe.is_none() {
            return Err("No se encontró CtDraw32.exe en la instalación de Power Operation".into());
        }
        // El proyecto de sistema `System` no tiene GLOBAL.CTL y no se abre en Graphics Builder.
        let without_global: Vec<String> = self
            .catalog(false)
            .await
            .map(|c| c.projects.iter().filter(|p| !p.name.eq_ignore_ascii_case("System") && global_ctl_hint(p).is_some()).map(|p| p.name.clone()).collect())
            .unwrap_or_default();
        let mut v = self
            .gb
            .call(move |gb| {
                gb.ensure(exe.as_ref())?;
                let selected = gb.m("ProjectSelected", &[]).map(|r| r.ret.text()).unwrap_or_default();
                let projects = gb_projects(gb);
                let (pages, active) = open_pages(gb);
                Ok(json!({
                    "connected": true, "pid": gb.pid(), "launched_by_mcp": gb.launched,
                    "selected_project": selected, "projects": projects, "open_pages": pages, "active_page": active,
                    "note": "Las páginas 'Untitled' (unsaved: true, project null) son páginas nuevas sin guardar: no las toca el MCP.",
                    "warnings": warnings_json(gb),
                }))
            })
            .await
            .map_err(err)?;
        if !without_global.is_empty() {
            v["projects_without_global_ctl"] = json!(without_global);
            add_hint(
                &mut v,
                Some("Proyectos sin GLOBAL.CTL: Graphics Builder muestra \"Cannot convert '<proyecto>' to version 8.50 format\" al seleccionarlos (no aceptarlo: cierra Graphics Builder). Se arregla copiando un GLOBAL.CTL vacío (352 bytes) de otro proyecto.".into()),
            );
        }
        pretty(&v, 60_000)
    }

    #[tool(name = "po_project_graphics", description = "Lista sin Graphics Builder (offline, archivos en disco) las páginas (.ctg), librerías de genies (.ctm), de símbolos (.ctl) y estilos de template (.ctt) de un proyecto y sus incluidos. El contenido de un estilo o una librería se ve con po_gb_library.")]
    async fn project_graphics(&self, Parameters(p): Parameters<super::project::ProjectParam>) -> ToolResult {
        let proj = self.project(p.project.as_deref()).await.map_err(err)?;
        let (projects, _) = self.catalog(false).await.map_err(err)?.with_includes(&proj);
        {
            let v = blocking(std::time::Duration::from_secs(60), move || {
                let names = |dir: &std::path::Path, ext: &str| -> Vec<String> {
                    project::files_with_ext(dir, ext).iter().filter_map(|f| f.file_stem().map(|s| s.to_string_lossy().to_string())).collect()
                };
                Ok(json!({
                    "project": proj.name,
                    "projects": projects.iter().map(|p| json!({
                        "project": p.name, "pages": names(&p.path, "ctg"), "genie_libraries": names(&p.path, "ctm"),
                        "symbol_libraries": names(&p.path, "ctl"), "template_styles": names(&p.path, "ctt"),
                    })).collect::<Vec<_>>(),
                }))
            })
            .await
            .map_err(err)?;
            pretty(&v, 120_000)
        }
    }

    #[tool(name = "po_gb_library", description = "Contenido de un estilo de templates o de una librería gráfica, leído con Graphics Builder. Con `style`: templates del estilo (con resolución). Con `library`: genies (o símbolos con kind=symbol) de la librería; con `element`, las propiedades de ese genie (lo que se completa al colocarlo). Qué estilos y librerías hay se ve sin Graphics Builder con po_project_graphics.")]
    async fn gb_library(&self, Parameters(p): Parameters<GbLibraryParams>) -> ToolResult {
        if p.style.is_none() && p.library.is_none() {
            return Err("Indicá style o library. La lista de páginas, librerías y estilos del proyecto la da po_project_graphics.".into());
        }
        let proj = self.project(p.project.as_deref()).await.map_err(err)?;
        let cat = self.catalog(false).await.map_err(err)?;
        let (projects, _) = cat.with_includes(&proj);
        let symbol = p.kind.as_deref().is_some_and(|k| k.eq_ignore_ascii_case("symbol"));
        let lib_proj = match (&p.library, &p.style) {
            (Some(l), _) => library_project(&projects, l, if symbol { "ctl" } else { "ctm" }),
            (None, Some(st)) => library_project(&projects, st, "ctt"),
            _ => None,
        }
        .unwrap_or_else(|| proj.name.clone());
        let pname = proj.name.clone();
        let v = self
            .gb
            .call(move |gb| {
                gb.ensure(None)?;
                let kind = if symbol { LIB_SYMBOL } else { LIB_GENIE };
                let needs: Vec<String> = p.library.iter().map(|_| lib_proj.clone()).collect();
                with_project(gb, &pname, &cat, &needs, |gb| {
                    let mut out = json!({"project": lib_proj});
                    if let Some(st) = &p.style {
                        out["style"] = json!(st);
                        out["templates"] = json!(templates(gb, &lib_proj, st));
                    }
                    if let Some(lib) = &p.library {
                        out["library"] = json!(lib);
                        if let Some(el) = &p.element {
                            let props = library_genie_properties(gb, &lib_proj, lib, el);
                            out["element"] = json!(el);
                            out["properties"] = json!(props.into_iter().map(|(k, v)| json!({"name": k, "default": v})).collect::<Vec<_>>());
                        } else {
                            let items = library_elements(gb, &lib_proj, lib, kind);
                            out[if symbol { "symbols" } else { "genies" }] = json!(items);
                        }
                    }
                    out["warnings"] = warnings_json(gb);
                    Ok(out)
                })
            })
            .await
            .map_err(err)?;
        pretty(&v, 120_000)
    }

    #[tool(name = "po_gb_page_read", description = "Lee una página gráfica con Graphics Builder: apariencia (estilo, template, tamaño), título y objetos (AN, clase, posición, texto, genie/símbolo de librería con sus propiedades: tags, equipos, textos). Si la página no estaba abierta la abre y la cierra sin guardar.")]
    async fn gb_page_read(&self, Parameters(p): Parameters<GbPageReadParams>) -> ToolResult {
        let full = self.project(p.project.as_deref()).await.map_err(err)?;
        let hint = global_ctl_hint(&full);
        let proj = full.name;
        let max = p.max_objects.unwrap_or(300).clamp(1, 5000);
        let with_props = p.genie_properties.unwrap_or(true);
        let mut v = self
            .gb
            .call(move |gb| {
                gb.ensure(None)?;
                let opened = open_page(gb, &proj, &p.page)?;
                let ap = appearance(gb);
                let title = gb.get("PageTitle").map(|v| v.text()).unwrap_or_default();
                let description = gb.get("PageDescription").map(|v| v.text()).unwrap_or_default();
                let (objects, truncated) = page_objects(gb, max, with_props);
                if opened {
                    close_page(gb);
                }
                Ok(json!({
                    "project": proj, "page": p.page, "appearance": ap, "title": title, "description": description,
                    "objects_count": objects.len(), "truncated": truncated, "objects": objects, "warnings": warnings_json(gb),
                }))
            })
            .await
            .map_err(err)?;
        add_hint(&mut v, hint);
        pretty(&v, 200_000)
    }

    #[tool(name = "po_gb_page_thumbnail", description = "Miniatura (imagen) de una página gráfica, generada por Graphics Builder.")]
    async fn gb_page_thumbnail(&self, Parameters(p): Parameters<GbPageParams>) -> Result<CallToolResult, String> {
        let proj = self.project(p.project.as_deref()).await.map_err(err)?.name;
        let page = p.page.clone();
        let bytes = self
            .gb
            .call(move |gb| {
                gb.ensure(None)?;
                match gb.m("PageGetThumbnail", &[s(&proj), s(&page)])?.ret {
                    Val::Bytes(b) if !b.is_empty() => Ok(b),
                    other => bail!("PageGetThumbnail no devolvió una imagen ({other:?})"),
                }
            })
            .await
            .map_err(err)?;
        // Graphics Builder devuelve BMP: se pasa a PNG para que el cliente lo muestre.
        let bytes = crate::png::bmp_to_png(&bytes).unwrap_or(bytes);
        let mime = match bytes.as_slice() {
            [0x89, b'P', b'N', b'G', ..] => "image/png",
            [0xFF, 0xD8, ..] => "image/jpeg",
            [b'G', b'I', b'F', ..] => "image/gif",
            [b'B', b'M', ..] => "image/bmp",
            _ => "application/octet-stream",
        };
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(CallToolResult::success(vec![
            ContentBlock::text(format!("Miniatura de {}: {} bytes, {mime}", p.page, bytes.len())),
            ContentBlock::image(b64, mime),
        ]))
    }

    #[tool(name = "po_gb_page_create", description = "Crea una página gráfica nueva con Graphics Builder a partir de un template (ver po_gb_library con `style`) y opcionalmente le agrega objetos (`ops`: text, rectangle, ellipse, line, button con comando, trend (plumas = trend tags), genie, symbol; relleno/visibilidad/texto por expresión; `look` con colores fijos, degradé, contorno, esquinas y fuente; `level` = nivel de llenado en rectangle). Con replace=true recrea una página existente (copia previa en _mcp_backup). Guarda la página. Confirmá con el usuario. Requiere POWEROP_ALLOW_WRITE=1.")]
    async fn gb_page_create(&self, Parameters(p): Parameters<GbPageCreateParams>) -> ToolResult {
        self.require_write("gb_page_create").map_err(err)?;
        validate_ops(&p.ops).map_err(err)?;
        let proj = self.project(p.project.as_deref()).await.map_err(err)?;
        check_writable(&proj.name).map_err(err)?;
        let hint = global_ctl_hint(&proj);
        let existing = project::find_file(&proj.path, &format!("{}.ctg", p.page));
        if existing.is_some() && !p.replace {
            return Err(format!(
                "La página {} ya existe en {}: usá po_gb_page_edit, replace=true (la recrea, con copia previa) o elegí otro nombre",
                p.page, proj.name
            ));
        }
        // Con replace, la copia del .ctg existente va a _mcp_backup antes de sobrescribirla con PageSaveAs.
        let backup = match &existing {
            Some(f) if p.replace => Some(super::project::backup(f).map_err(|e| format!("No se pudo respaldar la página: {e:#}"))?.display().to_string()),
            _ => None,
        };
        let replacing = backup.is_some();
        let cat = self.catalog(false).await.map_err(err)?;
        let (libs, _) = cat.with_includes(&proj);
        let style = p.style.clone().unwrap_or_else(|| "standard".into());
        let style_proj = library_project(&libs, &style, "ctt").unwrap_or_else(|| proj.name.clone());
        let pname = proj.name.clone();
        let detail = format!("{pname}/{} (estilo {style}, {} operaciones{})", p.page, p.ops.len(), if replacing { ", reemplaza" } else { "" });
        let r = self
            .gb
            .call(move |gb| {
                gb.ensure(None)?;
                if replacing {
                    close_if_open(gb, &pname, &p.page);
                }
                let project = pname.clone();
                with_project(gb, &project, &cat, &ops_lib_projects(&p.ops, &libs), |gb| {
                    let template = p.template.clone().unwrap_or_else(|| "normal".into());
                    let (resolution, titlebar) = match (p.resolution, p.titlebar) {
                        (Some(r), Some(t)) => (r, t),
                        (res, tb) => {
                            let list = templates(gb, &style_proj, &style);
                            let pick = list
                                .iter()
                                .find(|t| t["template"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(&template)) && res.is_none_or(|r| t["resolution"] == json!(r)))
                                .ok_or_else(|| anyhow!("El estilo {style} no tiene el template {template}{}. Templates: {}", res.map(|r| format!(" con resolución {r}")).unwrap_or_default(), json!(list)))?;
                            (res.unwrap_or(pick["resolution"].as_i64().unwrap_or(0) as i16), tb.unwrap_or(pick["titlebar"].as_bool().unwrap_or(false)))
                        }
                    };
                    with_quiet_new(gb, |gb| {
                        gb.m("PageNew", &[s(&style_proj), s(&style), s(&template), Arg::I2(resolution), Arg::Bool(titlebar), Arg::Bool(true)])?;
                        let result: Result<Value> = (|| {
                            if let Some(t) = &p.title {
                                gb.put("PageTitle", s(t))?;
                            }
                            let done = apply_ops(gb, &p.ops, &libs)?;
                            gb.m("PageSaveAs", &[s(&pname), s(&p.page)])?;
                            Ok(json!({"project": pname, "page": p.page, "style": style, "style_project": style_proj, "template": template, "resolution": resolution, "titlebar": titlebar, "ops": done}))
                        })();
                        // Siempre se cierra la página nueva (si falló, sin guardar).
                        close_page(gb);
                        let mut v = result?;
                        v["warnings"] = warnings_json(gb);
                        v["next_step"] = json!("Compilar el proyecto para usar la página en runtime.");
                        Ok(v)
                    })
                })
            })
            .await;
        match r {
            Ok(mut v) => {
                self.audit("gb_page_create", &detail, "OK");
                v["replaced"] = json!(replacing);
                v["backup"] = json!(backup);
                add_hint(&mut v, hint);
                pretty(&v, 60_000)
            }
            Err(e) => {
                let mut m = err(e);
                if let Some(b) = &backup {
                    m.push_str(&format!(" (la página original quedó respaldada en {b})"));
                }
                self.audit("gb_page_create", &detail, &format!("ERROR {m}"));
                Err(m)
            }
        }
    }

    #[tool(name = "po_gb_page_edit", description = "Edita una página gráfica existente con Graphics Builder: lista de operaciones en orden (text con expression, rectangle/ellipse con fill, line, `look` (colores fijos, degradé, contorno, esquinas, fuente), `level` en rectangle, button con command/up_command/privilege, trend (plumas = trend tags), genie/symbol, set_genie_properties, set_dynamic (fill, visible_when, expression por AN), set_touch (comando Touch por AN, conserva log y repetición), move/delete por AN de po_gb_page_read, group (agrupar por AN)) y guarda (copia previa del .ctg en _mcp_backup). Páginas de librerías de PO solo si están en POWEROP_GB_ALLOW_PAGES. Confirmá con el usuario. Requiere POWEROP_ALLOW_WRITE=1.")]
    async fn gb_page_edit(&self, Parameters(p): Parameters<GbPageEditParams>) -> ToolResult {
        self.require_write("gb_page_edit").map_err(err)?;
        if p.ops.is_empty() {
            return Err("Indicá al menos una operación".into());
        }
        validate_ops(&p.ops).map_err(err)?;
        let proj = self.project(p.project.as_deref()).await.map_err(err)?;
        let protected_page = check_page_writable(&proj.name, &p.page, &self.cfg.gb_allow_pages).map_err(err)?;
        let hint = global_ctl_hint(&proj);
        let cat = self.catalog(false).await.map_err(err)?;
        let (libs, _) = cat.with_includes(&proj);
        let pname = proj.name.clone();
        let save = p.save.unwrap_or(true);
        // Copia del .ctg antes de guardar encima (si la página ya está en disco).
        let backup = match project::find_file(&proj.path, &format!("{}.ctg", p.page)) {
            Some(f) if save => Some(super::project::backup(&f).map_err(|e| format!("No se pudo respaldar la página: {e:#}"))?.display().to_string()),
            _ => None,
        };
        let detail = format!(
            "{pname}/{} ({} operaciones, guardar={save}{})",
            p.page,
            p.ops.len(),
            if protected_page { ", proyecto protegido habilitado por POWEROP_GB_ALLOW_PAGES" } else { "" }
        );
        let r = self
            .gb
            .call(move |gb| {
                gb.ensure(None)?;
                with_project(gb, &pname, &cat, &ops_lib_projects(&p.ops, &libs), |gb| {
                    let opened = open_page(gb, &pname, &p.page)?;
                    let result = with_quiet_new(gb, |gb| apply_ops(gb, &p.ops, &libs));
                    let saved = match &result {
                        Ok(_) if save => gb.m("PageSave", &[]).map(|_| true),
                        _ => Ok(false),
                    };
                    if opened && (saved.is_ok() || !save) {
                        close_page(gb);
                    }
                    let done = result?;
                    let saved = saved?;
                    let mut v = json!({"project": pname, "page": p.page, "saved": saved, "backup": backup, "ops": done, "left_open": !opened || !saved && save, "warnings": warnings_json(gb)});
                    if protected_page {
                        v["allowed_by"] = json!("POWEROP_GB_ALLOW_PAGES");
                    }
                    Ok(v)
                })
            })
            .await;
        match r {
            Ok(mut v) => {
                self.audit("gb_page_edit", &detail, "OK");
                add_hint(&mut v, hint);
                pretty(&v, 60_000)
            }
            Err(e) => {
                let m = err(e);
                self.audit("gb_page_edit", &detail, &format!("ERROR {m}"));
                Err(m)
            }
        }
    }

    #[tool(name = "po_gb_page_delete", description = "Borra una página gráfica (y sus registros de animación) con Graphics Builder; antes copia el .ctg a _mcp_backup. Confirmá con el usuario. Requiere POWEROP_ALLOW_WRITE=1.")]
    async fn gb_page_delete(&self, Parameters(p): Parameters<GbPageParams>) -> ToolResult {
        self.require_write("gb_page_delete").map_err(err)?;
        let proj = self.project(p.project.as_deref()).await.map_err(err)?;
        check_writable(&proj.name).map_err(err)?;
        let Some(file) = project::find_file(&proj.path, &format!("{}.ctg", p.page)) else {
            return Err(format!("No existe la página {} en {}", p.page, proj.name));
        };
        // Copia previa del .ctg para poder restaurarla.
        let backup = super::project::backup(&file).map_err(|e| format!("No se pudo respaldar la página: {e:#}"))?.display().to_string();
        let pname = proj.name.clone();
        let detail = format!("{pname}/{}", p.page);
        let page = p.page.clone();
        let backup_txt = backup.clone();
        let r = self
            .gb
            .call(move |gb| {
                gb.ensure(None)?;
                close_if_open(gb, &pname, &page);
                gb.m("PageDelete", &[s(&pname), s(&page), Arg::Bool(true)])?;
                Ok(json!({"project": pname, "page": page, "deleted": true, "backup": backup, "warnings": warnings_json(gb)}))
            })
            .await;
        match r {
            Ok(v) => {
                self.audit("gb_page_delete", &detail, "OK");
                pretty(&v, 20_000)
            }
            Err(e) => {
                let mut m = err(e);
                // Visto en PO 2024 con po_gb_page_create replace (ver close_if_open); po_gb_page_delete en sí no se probó.
                if m.contains("0x80004005") {
                    m.push_str(&format!(
                        ". PageDelete de PO 2024 suele fallar así después de marcar borrada la fila de pages.dbf, dejando el .ctg en \
                         disco: la página desaparece de Graphics Builder sin borrarse. Para recuperarla, po_gb_page_create con \
                         replace=true y el mismo nombre (o copiar el respaldo {} como {}.ctg)",
                        backup_txt,
                        p.page
                    ));
                }
                self.audit("gb_page_delete", &detail, &format!("ERROR {m}"));
                Err(m)
            }
        }
    }

    #[tool(name = "po_gb_genie_read", description = "Lee un genie de una librería (.ctm) con Graphics Builder: propiedades (lo que se completa al colocarlo, con su valor por defecto), punto de inserción (hotspot) y objetos (AN, clase, posición, texto, genies anidados). Busca la librería en el proyecto y sus incluidos (solo lectura). Devuelve `place_op`: la operación para colocarlo en una página con po_gb_page_create/po_gb_page_edit.")]
    async fn gb_genie_read(&self, Parameters(p): Parameters<GbGenieReadParams>) -> ToolResult {
        check_lib_name("library", &p.library).map_err(err)?;
        check_lib_name("genie", &p.genie).map_err(err)?;
        let proj = self.project(p.project.as_deref()).await.map_err(err)?;
        let cat = self.catalog(false).await.map_err(err)?;
        let (libs, _) = cat.with_includes(&proj);
        let lib_proj = library_project(&libs, &p.library, "ctm")
            .ok_or_else(|| format!("No se encontró la librería {}.ctm en {} ni en sus incluidos (ver po_project_graphics)", p.library, proj.name))?;
        let max = p.max_objects.unwrap_or(300).clamp(1, 5000);
        let pname = proj.name.clone();
        let v = self
            .gb
            .call(move |gb| {
                gb.ensure(None)?;
                with_project(gb, &pname, &cat, &[lib_proj.clone()], |gb| {
                    if !genie_exists(gb, &lib_proj, &p.library, &p.genie) {
                        let list = library_elements(gb, &lib_proj, &p.library, LIB_GENIE);
                        bail!("La librería {lib_proj}/{} no tiene el genie {}. Genies: {}", p.library, p.genie, json!(list));
                    }
                    let props = library_genie_properties(gb, &lib_proj, &p.library, &p.genie);
                    // Si ya está abierto (por el usuario) se lee ahí y no se cierra.
                    let was_open = genie_is_open(gb, &p.library, &p.genie);
                    if !was_open {
                        gb.m("PageOpenEx", &[s(&lib_proj), s(&p.library), s(&p.genie), Arg::I2(LIB_GENIE)])
                            .map_err(|e| anyhow!("No se pudo abrir el genie {lib_proj}/{}.{}: {e:#}", p.library, p.genie))?;
                    }
                    let hs = hotspot(gb);
                    let (objects, truncated) = page_objects(gb, max, true);
                    if !was_open {
                        close_page(gb);
                    }
                    let defaults: serde_json::Map<String, Value> = props.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
                    Ok(json!({
                        "project": lib_proj, "library": p.library, "genie": p.genie,
                        "properties": props.iter().map(|(k, v)| json!({"name": k, "default": v})).collect::<Vec<_>>(),
                        "hotspot": hs, "bbox": objects_bbox(&objects),
                        "placement": "La op `genie` en (x, y) pone ahí la esquina (bbox.x, bbox.y) del recuadro, no el hotspot.",
                        "objects_count": objects.len(), "truncated": truncated, "objects": objects,
                        "place_op": {"op": "genie", "project": lib_proj, "library": p.library, "element": p.genie, "x": 0, "y": 0, "properties": defaults},
                        "warnings": warnings_json(gb),
                    }))
                })
            })
            .await
            .map_err(err)?;
        pretty(&v, 200_000)
    }

    #[tool(name = "po_gb_genie_create", description = "Crea un genie en una librería (.ctm) del proyecto con Graphics Builder, dibujado con las mismas `ops` que po_gb_page_create (text, rectangle, ellipse, line, button, genie anidado, fill/visible_when/expression, look, level) en píxeles desde (0, 0). Cada %Nombre% usado en textos, expresiones o comandos se vuelve una propiedad del genie (ver po_gb_genie_read). Si la librería no existe la crea. Con replace=true reemplaza un genie existente. Copia previa del .ctm en _mcp_backup. Nunca en proyectos de sistema/librería ni en librerías de proyectos incluidos. Confirmá con el usuario. Requiere POWEROP_ALLOW_WRITE=1.")]
    async fn gb_genie_create(&self, Parameters(p): Parameters<GbGenieCreateParams>) -> ToolResult {
        self.require_write("gb_genie_create").map_err(err)?;
        check_lib_name("library", &p.library).map_err(err)?;
        check_lib_name("genie", &p.genie).map_err(err)?;
        if p.ops.is_empty() {
            return Err("Indicá al menos una operación: el genie no puede quedar vacío".into());
        }
        validate_ops(&p.ops).map_err(err)?;
        let proj = self.project(p.project.as_deref()).await.map_err(err)?;
        check_writable(&proj.name).map_err(err)?;
        let hint = global_ctl_hint(&proj);
        let cat = self.catalog(false).await.map_err(err)?;
        let (libs, _) = cat.with_includes(&proj);
        let ctm = own_ctm(&libs, &p.library).map_err(err)?;
        let backup = match &ctm {
            Some(f) => Some(super::project::backup(f).map_err(|e| format!("No se pudo respaldar la librería: {e:#}"))?.display().to_string()),
            None => None,
        };
        let new_library = ctm.is_none();
        let pname = proj.name.clone();
        let detail = format!("{pname}/{}.{} ({} operaciones{})", p.library, p.genie, p.ops.len(), if p.replace { ", replace" } else { "" });
        let backup_txt = backup.clone();
        let r = self
            .gb
            .call(move |gb| {
                gb.ensure(None)?;
                let project = pname.clone();
                let needs: Vec<String> = std::iter::once(pname.clone()).chain(ops_lib_projects(&p.ops, &libs)).collect();
                with_project(gb, &project, &cat, &needs, |gb| {
                    let exists = !new_library && genie_exists(gb, &pname, &p.library, &p.genie);
                    if exists && !p.replace {
                        bail!(
                            "El genie {}.{} ya existe en {pname}: usá po_gb_genie_edit, replace=true (lo reemplaza, con copia previa del .ctm) u otro nombre",
                            p.library,
                            p.genie
                        );
                    }
                    if exists && genie_is_open(gb, &p.library, &p.genie) {
                        bail!("El genie {}.{} {GENIE_OPEN}", p.library, p.genie);
                    }
                    if new_library {
                        gb.m("PageNewLibrary", &[s(&pname), s(&p.library), Arg::I2(LIB_GENIE)])
                            .map_err(|e| anyhow!("No se pudo crear la librería {pname}/{}: {e:#}", p.library))?;
                    }
                    with_quiet_new(gb, |gb| {
                        gb.m("PageNewEx", &[Arg::I2(LIB_GENIE)])?;
                        let result: Result<(Vec<Value>, Value)> = (|| {
                            let done = apply_ops(gb, &p.ops, &libs)?;
                            if let Some(h) = p.hotspot {
                                gb.m("LibraryObjectHotspotPut", &[Arg::I2(h.x), Arg::I2(h.y)])?;
                            }
                            let bb = objects_bbox(&page_objects(gb, 5000, false).0);
                            // Guardar encima de un genie existente puede pedir confirmación (el vigilante contestaría No):
                            // se borra el viejo justo antes, con la librería ya respaldada.
                            if exists {
                                gb.m("PageDeleteEx", &[s(&pname), s(&p.library), s(&p.genie), Arg::I2(LIB_GENIE)])?;
                            }
                            gb.m("PageSaveAsEx", &[s(&pname), s(&p.library), s(&p.genie)])?;
                            Ok((done, bb))
                        })();
                        // Siempre se cierra la página del genie nuevo (si falló, sin guardar).
                        close_page(gb);
                        let (done, bb) = result?;
                        if bb["x"].as_i64().is_some_and(|v| v < 0) || bb["y"].as_i64().is_some_and(|v| v < 0) {
                            gb.warnings.push(format!(
                                "El recuadro del genie empieza en ({}, {}), no en (0, 0): al colocarlo en (x, y) el (0, 0) del dibujo cae corrido. \
                                 Suele ser un texto con align right/center o una expresión larga (en el editor se ve \"=expresión\").",
                                bb["x"], bb["y"]
                            ));
                        }
                        if !genie_exists(gb, &pname, &p.library, &p.genie) {
                            bail!("Graphics Builder no informó error, pero el genie {}.{} no aparece en la librería", p.library, p.genie);
                        }
                        let props = library_genie_properties(gb, &pname, &p.library, &p.genie);
                        Ok(json!({
                            "project": pname, "library": p.library, "genie": p.genie, "library_created": new_library, "replaced": exists,
                            "bbox": bb,
                            "properties": props.iter().map(|(k, v)| json!({"name": k, "default": v})).collect::<Vec<_>>(),
                            "ops": done, "warnings": warnings_json(gb),
                            "next_step": "Colocarlo con la op `genie` (library, element, properties) en po_gb_page_create/po_gb_page_edit y compilar.",
                        }))
                    })
                })
            })
            .await;
        match r {
            Ok(mut v) => {
                self.audit("gb_genie_create", &detail, "OK");
                v["backup"] = json!(backup);
                add_hint(&mut v, hint);
                pretty(&v, 60_000)
            }
            Err(e) => {
                let mut m = err(e);
                if let Some(b) = &backup_txt {
                    m.push_str(&format!(" (la librería quedó respaldada en {b})"));
                }
                self.audit("gb_genie_create", &detail, &format!("ERROR {m}"));
                Err(m)
            }
        }
    }

    #[tool(name = "po_gb_genie_edit", description = "Edita un genie existente de una librería (.ctm) del proyecto con Graphics Builder: `ops` como po_gb_page_edit (agregar objetos, move/delete/set_dynamic por AN de po_gb_genie_read) y/o `hotspot`, y guarda. Copia previa del .ctm en _mcp_backup. Las páginas que ya lo usan toman el cambio al compilar (si los genies están vinculados). Nunca en proyectos de sistema/librería ni en librerías de proyectos incluidos. Confirmá con el usuario. Requiere POWEROP_ALLOW_WRITE=1.")]
    async fn gb_genie_edit(&self, Parameters(p): Parameters<GbGenieEditParams>) -> ToolResult {
        self.require_write("gb_genie_edit").map_err(err)?;
        check_lib_name("library", &p.library).map_err(err)?;
        check_lib_name("genie", &p.genie).map_err(err)?;
        if p.ops.is_empty() && p.hotspot.is_none() {
            return Err("Indicá al menos una operación o un hotspot".into());
        }
        validate_ops(&p.ops).map_err(err)?;
        let proj = self.project(p.project.as_deref()).await.map_err(err)?;
        check_writable(&proj.name).map_err(err)?;
        let hint = global_ctl_hint(&proj);
        let cat = self.catalog(false).await.map_err(err)?;
        let (libs, _) = cat.with_includes(&proj);
        let ctm = own_ctm(&libs, &p.library).map_err(err)?.ok_or_else(|| format!("No existe la librería {}.ctm en {} (para crearla, po_gb_genie_create)", p.library, proj.name))?;
        let backup = super::project::backup(&ctm).map_err(|e| format!("No se pudo respaldar la librería: {e:#}"))?.display().to_string();
        let pname = proj.name.clone();
        let detail = format!("{pname}/{}.{} ({} operaciones)", p.library, p.genie, p.ops.len());
        let r = self
            .gb
            .call(move |gb| {
                gb.ensure(None)?;
                let project = pname.clone();
                let needs: Vec<String> = std::iter::once(pname.clone()).chain(ops_lib_projects(&p.ops, &libs)).collect();
                with_project(gb, &project, &cat, &needs, |gb| {
                    if !genie_exists(gb, &pname, &p.library, &p.genie) {
                        let list = library_elements(gb, &pname, &p.library, LIB_GENIE);
                        bail!("La librería {pname}/{} no tiene el genie {}. Genies: {}", p.library, p.genie, json!(list));
                    }
                    if genie_is_open(gb, &p.library, &p.genie) {
                        bail!("El genie {}.{} {GENIE_OPEN}", p.library, p.genie);
                    }
                    gb.m("PageOpenEx", &[s(&pname), s(&p.library), s(&p.genie), Arg::I2(LIB_GENIE)])
                        .map_err(|e| anyhow!("No se pudo abrir el genie {pname}/{}.{}: {e:#}", p.library, p.genie))?;
                    let result = with_quiet_new(gb, |gb| {
                        let done = apply_ops(gb, &p.ops, &libs)?;
                        if let Some(h) = p.hotspot {
                            gb.m("LibraryObjectHotspotPut", &[Arg::I2(h.x), Arg::I2(h.y)])?;
                        }
                        Ok(done)
                    });
                    let saved = match &result {
                        Ok(_) => gb.m("PageSave", &[]).map(|_| ()),
                        Err(_) => Ok(()),
                    };
                    let hs = hotspot(gb);
                    // Si no se pudo guardar queda abierto, con los cambios, para revisarlo en Graphics Builder.
                    if result.is_err() || saved.is_ok() {
                        close_page(gb);
                    }
                    let done = result?;
                    saved?;
                    let props = library_genie_properties(gb, &pname, &p.library, &p.genie);
                    Ok(json!({
                        "project": pname, "library": p.library, "genie": p.genie, "saved": true, "hotspot": hs,
                        "properties": props.iter().map(|(k, v)| json!({"name": k, "default": v})).collect::<Vec<_>>(),
                        "ops": done, "warnings": warnings_json(gb),
                    }))
                })
            })
            .await;
        match r {
            Ok(mut v) => {
                self.audit("gb_genie_edit", &detail, "OK");
                v["backup"] = json!(backup);
                add_hint(&mut v, hint);
                pretty(&v, 60_000)
            }
            Err(e) => {
                let m = format!("{} (la librería quedó respaldada en {backup})", err(e));
                self.audit("gb_genie_edit", &detail, &format!("ERROR {m}"));
                Err(m)
            }
        }
    }

    #[tool(name = "po_gb_genie_delete", description = "Borra un genie de una librería (.ctm) del proyecto con Graphics Builder; antes copia el .ctm a _mcp_backup. Las páginas que lo usan quedan con el genie roto hasta reemplazarlo. Nunca en proyectos de sistema/librería ni en librerías de proyectos incluidos. Confirmá con el usuario. Requiere POWEROP_ALLOW_WRITE=1.")]
    async fn gb_genie_delete(&self, Parameters(p): Parameters<GbGenieParams>) -> ToolResult {
        self.require_write("gb_genie_delete").map_err(err)?;
        check_lib_name("library", &p.library).map_err(err)?;
        check_lib_name("genie", &p.genie).map_err(err)?;
        let proj = self.project(p.project.as_deref()).await.map_err(err)?;
        check_writable(&proj.name).map_err(err)?;
        let cat = self.catalog(false).await.map_err(err)?;
        let (libs, _) = cat.with_includes(&proj);
        let ctm = own_ctm(&libs, &p.library).map_err(err)?.ok_or_else(|| format!("No existe la librería {}.ctm en {}", p.library, proj.name))?;
        let backup = super::project::backup(&ctm).map_err(|e| format!("No se pudo respaldar la librería: {e:#}"))?.display().to_string();
        let pname = proj.name.clone();
        let detail = format!("{pname}/{}.{}", p.library, p.genie);
        let r = self
            .gb
            .call(move |gb| {
                gb.ensure(None)?;
                let project = pname.clone();
                with_project(gb, &project, &cat, &[pname.clone()], |gb| {
                    if !genie_exists(gb, &pname, &p.library, &p.genie) {
                        let list = library_elements(gb, &pname, &p.library, LIB_GENIE);
                        bail!("La librería {pname}/{} no tiene el genie {}. Genies: {}", p.library, p.genie, json!(list));
                    }
                    if genie_is_open(gb, &p.library, &p.genie) {
                        bail!("El genie {}.{} {GENIE_OPEN}", p.library, p.genie);
                    }
                    gb.m("PageDeleteEx", &[s(&pname), s(&p.library), s(&p.genie), Arg::I2(LIB_GENIE)])?;
                    if genie_exists(gb, &pname, &p.library, &p.genie) {
                        bail!("Graphics Builder no informó error, pero el genie {}.{} sigue en la librería", p.library, p.genie);
                    }
                    let remaining = library_elements(gb, &pname, &p.library, LIB_GENIE);
                    Ok(json!({
                        "project": pname, "library": p.library, "genie": p.genie, "deleted": true, "backup": backup,
                        "remaining_genies": remaining, "warnings": warnings_json(gb),
                        "note": "Para recuperarlo: copiar el respaldo sobre el .ctm (con Graphics Builder cerrado) o recrearlo con po_gb_genie_create.",
                    }))
                })
            })
            .await;
        match r {
            Ok(v) => {
                self.audit("gb_genie_delete", &detail, "OK");
                pretty(&v, 20_000)
            }
            Err(e) => {
                let m = err(e);
                self.audit("gb_genie_delete", &detail, &format!("ERROR {m}"));
                Err(m)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_when_se_guarda_negado() {
        assert_eq!(hidden_when("Pos = 2"), "NOT (Pos = 2)");
    }

    #[test]
    fn global_ctl_hint_detects_missing_library() {
        let dir = crate::testutil::TempDir::new("globalctl");
        let proj = Project { name: "Demo".into(), path: dir.path().to_path_buf(), running: false };
        let hint = global_ctl_hint(&proj).unwrap();
        assert!(hint.contains("Demo no tiene GLOBAL.CTL") && hint.contains("Cannot convert 'Demo'"), "{hint}");
        let mut v = json!({"warnings": ["otro"]});
        add_hint(&mut v, Some(hint.clone()));
        assert_eq!(v["warnings"], json!(["otro", hint]));
        // El nombre del archivo no distingue mayúsculas.
        std::fs::write(dir.join("global.ctl"), [0u8; 4]).unwrap();
        assert!(global_ctl_hint(&proj).is_none());
    }

    fn ops(v: Value) -> Vec<PageOp> {
        serde_json::from_value(v).expect("ops válidas")
    }

    #[test]
    fn op_library_busca_el_proyecto_de_la_libreria() {
        let dir = crate::testutil::TempDir::new("oplib");
        std::fs::write(dir.join("mcp_lib.ctm"), [0u8; 4]).unwrap();
        let libs = [Project { name: "Demo".into(), path: dir.path().to_path_buf(), running: false }];
        let v = ops(json!([
            {"op": "genie", "library": "mcp_lib", "element": "g", "x": 0, "y": 0},
            {"op": "genie", "library": "otra", "element": "g", "x": 0, "y": 0, "project": "Include"},
            {"op": "symbol", "library": "mcp_lib", "element": "s", "x": 0, "y": 0},
            {"op": "move", "an": 1, "x": 0, "y": 0}
        ]));
        assert_eq!(op_library(&v[0], &libs).unwrap().unwrap(), ("Demo".to_string(), LIB_GENIE));
        assert_eq!(op_library(&v[1], &libs).unwrap().unwrap(), ("Include".to_string(), LIB_GENIE));
        assert!(op_library(&v[2], &libs).unwrap().is_err(), "no hay mcp_lib.ctl");
        assert!(op_library(&v[3], &libs).is_none());
        assert_eq!(ops_lib_projects(&v, &libs), ["Demo", "Include"]);
        // Sin include.dbf: el proyecto y el de sistema Include; uno fuera del catálogo, él e Include.
        let inc = dir.join("Include");
        std::fs::create_dir_all(&inc).unwrap();
        let cat = Catalog { projects: vec![libs[0].clone(), Project { name: "Include".into(), path: inc, running: false }], ..Default::default() };
        assert_eq!(visible_projects(&cat, "demo"), ["Demo", "Include"]);
        assert_eq!(visible_projects(&cat, "Otro"), ["Otro", "Include"]);
    }

    #[test]
    fn protected_pages_need_allow_list() {
        let allow = vec![("PLS_Include".to_string(), "!equipment1".to_string())];
        assert_eq!(check_page_writable("MCP_Demo", "P1", &[]).unwrap(), false);
        assert_eq!(check_page_writable("pls_include", "!Equipment1", &allow).unwrap(), true);
        let e = check_page_writable("PLS_Include", "!equipment2", &allow).unwrap_err().to_string();
        assert!(e.contains("POWEROP_GB_ALLOW_PAGES"), "{e}");
        assert!(check_page_writable("Include", "!equipment1", &allow).is_err());
        // El resto de las escrituras en librerías sigue bloqueado.
        assert!(check_writable("PLS_Include").is_err());
    }

    #[test]
    fn validate_set_touch() {
        let ok = ops(json!([
            {"op": "set_touch", "an": 43, "action": "up", "command": "IF Tag1 = 0 THEN TagWrite(\"Tag1\", 1); END"},
            {"op": "set_touch", "an": 44, "action": "down", "command": "Cmd()", "log_message": "Mando", "repeat_ms": 500}
        ]));
        assert!(validate_ops(&ok).is_ok(), "{:?}", validate_ops(&ok));
        for (bad, campo) in [
            (json!({"op": "set_touch", "an": 1, "action": "repeat", "command": "X()"}), "action"),
            (json!({"op": "set_touch", "an": 1, "action": "up", "command": " "}), "command"),
            (json!({"op": "set_touch", "an": 1, "action": "up", "command": "X()", "repeat_ms": 0}), "repeat_ms"),
        ] {
            let e = validate_ops(&ops(json!([bad]))).unwrap_err().to_string();
            assert!(e.contains(campo) && e.contains("set_touch"), "{e}");
        }
        assert!(serde_json::from_value::<PageOp>(json!({"op": "set_touch", "an": 1, "action": "up", "command": "X()", "repeat": 5})).is_err());
    }

    #[test]
    fn validate_group() {
        assert!(validate_ops(&ops(json!([{"op": "group", "ans": [3, 4, 9]}]))).is_ok());
        for bad in [json!({"op": "group", "ans": [3]}), json!({"op": "group", "ans": [3, 3]}), json!({"op": "group", "ans": []})] {
            let e = validate_ops(&ops(json!([bad]))).unwrap_err().to_string();
            assert!(e.contains("group") && e.contains("AN distintos"), "{e}");
        }
    }

    #[test]
    fn grupos_candidatos() {
        assert!(maybe_group(&json!({"an": null, "class": ""})));
        assert!(maybe_group(&json!({"an": 38, "class": "Group"})));
        assert!(!maybe_group(&json!({"an": 12, "class": ""})));
        assert!(!maybe_group(&json!({"an": null, "class": "Genie"})));
    }

    #[test]
    fn validate_accepts_dynamic_fields() {
        let v = ops(json!([
            {"op": "rectangle", "x": 0, "y": 0, "x2": 10, "y2": 10, "visible_when": "TRF_Vis == 1",
             "fill": {"expression": "TRF_Red1_Estado == 1", "colours": [{"state": 0, "colour": "#FF0000"}, {"state": 1, "colour": "#00B000"}]}},
            {"op": "text", "text": "Secuencia", "x": 1, "y": 2, "expression": "\"Secuencia: \" + TRF_Seq"},
            {"op": "button", "x": 0, "y": 0, "x2": 50, "y2": 20, "text": "Falla", "command": "TRF_Falla(1)", "up_command": "TRF_Falla(0)", "privilege": 3},
            {"op": "genie", "library": "lib", "element": "g", "x": 1, "y": 1, "fill": {"kind": "multi_state", "expression": "A == 1",
             "conditions": ["A == 2", "A == 3"], "colours": [{"state": 31, "colour": "#abc"}]}},
            {"op": "set_dynamic", "an": 7, "expression": "\"Modo: \" + TRF_Modo"},
            {"op": "set_dynamic", "an": 8, "visible_when": "TRF_Mostrar"}
        ]));
        assert!(validate_ops(&v).is_ok(), "{:?}", validate_ops(&v));
    }

    #[test]
    fn validate_look_and_level() {
        let v = ops(json!([
            {"op": "rectangle", "x": 0, "y": 0, "x2": 10, "y2": 10,
             "look": {"fill_colour": "#1E293B", "gradient_colour": "#0F172A", "gradient_mode": 4, "line_colour": "#334155", "line_width": 2, "corner_radius": 8},
             "level": {"expression": "PL_TK101_Nivel", "colour": "#0EA5E9", "background": "#0B1220"}},
            {"op": "ellipse", "x": 0, "y": 0, "x2": 10, "y2": 10, "look": {"fill_colour": "#22C55E", "line_width": 1}},
            {"op": "line", "x": 0, "y": 0, "x2": 10, "y2": 0, "look": {"line_colour": "#38BDF8", "line_width": 6}},
            {"op": "text", "text": "TK-101", "x": 1, "y": 2, "look": {"text_colour": "#E2E8F0", "font": "Segoe UI", "font_style": 1, "align": "center"}}
        ]));
        assert!(validate_ops(&v).is_ok(), "{:?}", validate_ops(&v));
        let bad = [
            (json!({"op": "line", "x": 0, "y": 0, "x2": 1, "y2": 1, "look": {"fill_colour": "#000000"}}), "no aplica a line"),
            (json!({"op": "ellipse", "x": 0, "y": 0, "x2": 1, "y2": 1, "look": {"corner_radius": 4}}), "no aplica a ellipse"),
            (json!({"op": "text", "text": "t", "x": 0, "y": 0, "look": {"line_width": 2}}), "no aplica a text"),
            (json!({"op": "rectangle", "x": 0, "y": 0, "x2": 1, "y2": 1, "look": {"line_width": 40}}), "fuera de rango"),
            (json!({"op": "rectangle", "x": 0, "y": 0, "x2": 1, "y2": 1, "look": {"line_width": 0}}), "(1 a 32)"),
            (json!({"op": "rectangle", "x": 0, "y": 0, "x2": 1, "y2": 1, "look": {"gradient_colour": "#000000"}}), "necesita fill_colour"),
            (json!({"op": "text", "text": "t", "x": 0, "y": 0, "look": {"align": "middle"}}), "align"),
            (json!({"op": "text", "text": "t", "x": 0, "y": 0, "look": {"text_colour": "azul"}}), "Color inválido"),
            (json!({"op": "rectangle", "x": 0, "y": 0, "x2": 1, "y2": 1, "level": {"expression": "N", "min": 10, "max": 5, "colour": "#000000", "background": "#FFFFFF"}}), "menor que max"),
            (json!({"op": "rectangle", "x": 0, "y": 0, "x2": 1, "y2": 1, "level": {"expression": "N", "colour": "#000000", "background": "#FFFFFF", "direction": "arriba"}}), "direction"),
        ];
        for (b, msg) in bad {
            let e = format!("{:#}", validate_ops(&ops(json!([b.clone()]))).unwrap_err());
            assert!(e.contains(msg), "{b}: {e}");
        }
        // Campos desconocidos dentro de look los rechaza serde.
        assert!(serde_json::from_value::<Vec<PageOp>>(json!([{"op": "text", "text": "t", "x": 0, "y": 0, "look": {"color": "#fff"}}])).is_err());
    }

    #[test]
    fn validate_rejects_bad_dynamic_fields() {
        let rect = |fill: Value| json!({"op": "rectangle", "x": 0, "y": 0, "x2": 1, "y2": 1, "fill": fill});
        let negro = json!([{"state": 0, "colour": "#000000"}]);
        let bad = [
            // on/off solo admite estados 0 y 1; multi_state hasta 31.
            (rect(json!({"expression": "A", "colours": [{"state": 2, "colour": "#000000"}]})), "state 2 fuera de rango (0 a 1"),
            (rect(json!({"kind": "multi_state", "expression": "A", "colours": [{"state": 32, "colour": "#000000"}]})), "state 32 fuera de rango (0 a 31"),
            (rect(json!({"expression": "A", "colours": [{"state": 0, "colour": "#GG0000"}]})), "usá #RRGGBB"),
            (json!({"op": "ellipse", "x": 0, "y": 0, "x2": 1, "y2": 1, "fill": {"expression": "A", "colours": []}}), "al menos un color"),
            (json!({"op": "symbol", "library": "l", "element": "e", "x": 0, "y": 0, "fill": {"expression": " ", "colours": negro}}), "fill.expression no puede estar vacío"),
            (rect(json!({"expression": "A", "conditions": ["B"], "colours": negro})), "solo aplica con kind multi_state"),
            (rect(json!({"kind": "multi_state", "expression": "A", "conditions": ["B", "C", "D", "E", "F"], "colours": negro})), "como máximo 4"),
            (rect(json!({"kind": "threshold", "expression": "A", "colours": negro})), "kind 'threshold' no soportado"),
            (json!({"op": "line", "x": 0, "y": 0, "x2": 1, "y2": 1, "visible_when": ""}), "visible_when no puede estar vacío"),
            (json!({"op": "text", "text": "x", "x": 0, "y": 0, "expression": "  "}), "expression no puede estar vacío"),
            (json!({"op": "button", "x": 0, "y": 0, "x2": 1, "y2": 1, "text": "ok", "privilege": 0}), "0 no es un privilegio"),
            (json!({"op": "button", "x": 0, "y": 0, "x2": 1, "y2": 1, "text": "ok", "command": ""}), "button.command no puede estar vacío"),
            (json!({"op": "button", "x": 0, "y": 0, "x2": 1, "y2": 1, "text": "ok", "up_command": " "}), "button.up_command no puede estar vacío"),
            (json!({"op": "button", "x": 0, "y": 0, "x2": 1, "y2": 1, "text": ""}), "button.text no puede estar vacío"),
        ];
        for (b, msg) in bad {
            let e = format!("{:#}", validate_ops(&ops(json!([b.clone()]))).unwrap_err());
            assert!(e.contains(msg), "{b}: {e}");
        }
    }

    #[test]
    fn trend_ops_validate_and_build_args() {
        let pen = |e: &str| json!({"expression": e, "colour": "#00A0E0"});
        let good = ops(json!([{"op": "trend", "x": 10, "y": 10, "x2": 610, "y2": 210, "pens": [pen("PL_TK101_Nivel"), pen("PL_TK102_Nivel")]}]));
        assert!(validate_ops(&good).is_ok());
        assert_eq!(trend_samples(10, 610, None, None), (300, 2));
        assert_eq!(trend_samples(10, 610, Some(120), Some(5)), (120, 5));
        // Ancho angosto: al menos una muestra.
        assert_eq!(trend_samples(10, 11, None, None), (1, 2));
        // Pluma 0 con color off propio, pluma 1 con off = on, y 6 vacías en negro.
        let p: Vec<TrendPen> = serde_json::from_value(json!([{"expression": "A", "colour": "#00B000", "off_colour": "#FF0000"}, pen("B")])).unwrap();
        let args = trend_put_args(&p, 300, 2).unwrap();
        assert_eq!(args.len(), 2 + 8 * 3);
        assert!(matches!(args[0], Arg::I2(300)) && matches!(args[1], Arg::I2(2)));
        assert!(matches!(&args[2], Arg::Str(s) if s == "A"));
        assert!(matches!((&args[3], &args[4]), (Arg::I4(0x00B000), Arg::I4(0xFF0000))));
        assert!(matches!(&args[5], Arg::Str(s) if s == "B"));
        assert!(matches!((&args[6], &args[7]), (Arg::I4(0x00A0E0), Arg::I4(0x00A0E0))));
        for i in 2..8 {
            let k = 2 + 3 * i;
            assert!(matches!((&args[k], &args[k + 1], &args[k + 2]), (Arg::Str(s), Arg::I4(0), Arg::I4(0)) if s.is_empty()), "pluma {i}");
        }
        let trend = |extra: Value| {
            let mut t = json!({"op": "trend", "x": 0, "y": 0, "x2": 100, "y2": 50, "pens": [pen("A")]});
            t.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            t
        };
        let bad = [
            (trend(json!({"pens": []})), "de 1 a 8 plumas (hay 0)"),
            (trend(json!({"pens": (0..9).map(|i| pen(&format!("T{i}"))).collect::<Vec<_>>()})), "de 1 a 8 plumas (hay 9)"),
            (trend(json!({"pens": [pen(""), pen("B")]})), "trend.pens[0].expression no puede estar vacío"),
            (trend(json!({"pens": [{"expression": "A", "colour": "rojo"}]})), "usá #RRGGBB"),
            (trend(json!({"pens": [{"expression": "A", "colour": "#000", "off_colour": "x"}]})), "usá #RRGGBB"),
            (trend(json!({"pixels_per_sample": 0})), "pixels_per_sample = 0 fuera de rango"),
            (trend(json!({"pixels_per_sample": 33})), "pixels_per_sample = 33 fuera de rango"),
            (trend(json!({"samples": 0})), "samples = 0 fuera de rango"),
            (trend(json!({"x2": 0})), "x2 tiene que ser distinto de x"),
        ];
        for (b, msg) in bad {
            let e = format!("{:#}", validate_ops(&ops(json!([b.clone()]))).unwrap_err());
            assert!(e.contains(msg), "{b}: {e}");
        }
    }

    #[test]
    fn untitled_pages_have_no_project() {
        assert_eq!(page_project("Untitled", &json!("\u{1}\u{fffd}x")), Value::Null);
        assert_eq!(page_project("untitled2", &json!("MCP_Demo")), Value::Null);
        assert_eq!(page_project("Unifilar", &json!("Transferencia_Demo")), json!("Transferencia_Demo"));
        assert_eq!(page_project("Unifilar", &json!("ab\u{7}")), Value::Null);
        assert_eq!(page_project("Unifilar", &json!("")), Value::Null);
        assert_eq!(page_project("Unifilar", &Value::Null), Value::Null);
        assert!(is_untitled(" Untitled ") && !is_untitled("MCP_GB_Test"));
    }

    #[test]
    fn unknown_fields_inside_ops_are_rejected() {
        let bad = [
            (json!({"op": "rectangle", "x": 0, "y": 0, "x2": 1, "y2": 1, "visible": "A"}), "visible"),
            (json!({"op": "button", "x": 0, "y": 0, "x2": 1, "y2": 1, "text": "ok", "cmd": "X()"}), "cmd"),
            (json!({"op": "move", "an": 1, "x": 0, "y": 0, "z": 3}), "z"),
            (json!({"op": "rectangle", "x": 0, "y": 0, "x2": 1, "y2": 1, "fill": {"expr": "A", "expression": "A", "colours": []}}), "expr"),
            (json!({"op": "rectangle", "x": 0, "y": 0, "x2": 1, "y2": 1, "fill": {"expression": "A", "colours": [{"state": 0, "color": "#000000"}]}}), "color"),
        ];
        for (b, field) in bad {
            let e = serde_json::from_value::<GbPageEditParams>(json!({"page": "P", "ops": [b.clone()]})).err().map(|e| e.to_string());
            let e = e.unwrap_or_else(|| panic!("debería rechazar {b}"));
            assert!(e.contains(&format!("unknown field `{field}`")), "{e}");
        }
        // Lo válido sigue pasando (el campo `op` del enum etiquetado no cuenta como desconocido).
        assert!(serde_json::from_value::<GbPageCreateParams>(json!({"page": "P", "ops": [{"op": "delete", "an": 3}]})).is_ok());
    }

    #[test]
    fn validate_reports_operation_index() {
        let v = ops(json!([
            {"op": "line", "x": 0, "y": 0, "x2": 1, "y2": 1},
            {"op": "button", "x": 0, "y": 0, "x2": 1, "y2": 1, "text": "ok", "privilege": 0}
        ]));
        let e = format!("{:#}", validate_ops(&v).unwrap_err());
        assert!(e.starts_with("Operación 1 (button)"), "{e}");
    }

    #[test]
    fn fill_put_args_pad_expressions_and_kind() {
        let f: FillSpec = serde_json::from_value(json!({"expression": "A == 1", "colours": [{"state": 1, "colour": "#00B000"}]})).unwrap();
        let args = fill_put_args(&f).unwrap();
        assert_eq!(args.len(), 9);
        assert!(matches!(args[0], Arg::I2(0)));
        assert!(matches!(&args[1], Arg::Str(s) if s == "A == 1"));
        assert!(matches!(&args[2], Arg::Str(s) if s.is_empty()));
        assert!(matches!(&args[5], Arg::Str(s) if s.is_empty()));
        assert!(matches!(args[6], Arg::Bool(false)));

        let m: FillSpec = serde_json::from_value(json!({"kind": "multi_state", "expression": "A", "conditions": ["B", "C"], "colours": [{"state": 3, "colour": "#fff"}]})).unwrap();
        let args = fill_put_args(&m).unwrap();
        assert!(matches!(args[0], Arg::I2(1)));
        assert!(matches!(&args[3], Arg::Str(s) if s == "C"));
        assert!(matches!(&args[4], Arg::Str(s) if s.is_empty()));
    }
}
