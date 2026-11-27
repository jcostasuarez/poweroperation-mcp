//! Modelo del Advanced One-Line (AOL) de Power Operation, a partir de los genies de las páginas (`ctg.rs`).
//!
//! Cómo funciona el AOL (PO 2024, visto en PLS_Include y en `OneLineConfig.Business.dll`):
//! - Cada objeto del unifilar es un genie de las librerías de PLS_Include (`pls_ansi_*`, `pls_iec_*`, `pls_gen_*`,
//!   `pls_meter`) con propiedades numéricas que lo identifican: `BkrNum` (interruptor), `BusbarNum` (barra),
//!   `MtrNum` (medidor), `SrcNum` (fuente), `SrcBusbarNum`/`DestBusbarNum` (barras de origen y destino) y para
//!   transferencias `BkrNum1`/`BkrNum2`/`SrcBusbarNum1`/`SrcBusbarNum2`. `Equip` es el equipo y `Active` la
//!   condición de línea activa.
//! - Las animaciones usan tags del dispositivo `zOL`: `zOL\Bkr<BkrNum>`, `zOL\Bus<BusbarNum>`, `zOL\Mtr<N>`,
//!   `zOL\Src<N>`, que escribe `AdvOneLine.exe` según `AdvOneLine.csv` (lo genera la One-Line Configuration Utility:
//!   `Component,Condition,Bus1,Bus2`, con componentes `BKR<n>`, `SRC<n>`, `MTR<n>`, `XFR<n>` y colores `CLR<n>`).
//! - Las validaciones replican las de la utilidad (`DuplicateBreaker`, `InvalidEquipment`, `NoLineActive`,
//!   `SameSourceAndDestinationBus`, `InvalidSourceBus`...) más las de consistencia entre páginas.
//!
//! El tipo de cada genie se deduce de sus propiedades: ver `classify`. La utilidad usa dos archivos de PLS_Include:
//! `SupportedGenies.xml` (librería/genie → tipo: PwrBreaker, PwrBusbar, PwrMeter, PwrSource, PwrTransformer, PwrATS,
//! PwrMotor) y `GenieDefinitions.xml` (tipo → propiedades: ID, ID2, Node1..3). Las propiedades que usa `classify`
//! coinciden con esas definiciones (transformador: SimSrcNum/SimSrcNum2 + SrcBusbarNum/DestBusbarNum; ATS:
//! BkrNum1/2 + SrcBusbarNum1/2 + DestBusbarNum; los motores de pls_gen_eq_* son barras con BusbarNum).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde_json::{Value, json};

use crate::ctg::GenieInstance;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Breaker,
    Ats,
    Busbar,
    Meter,
    Source,
    Transformer,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Breaker => "breaker",
            Kind::Ats => "ats",
            Kind::Busbar => "busbar",
            Kind::Meter => "meter",
            Kind::Source => "source",
            Kind::Transformer => "transformer",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        [Kind::Breaker, Kind::Ats, Kind::Busbar, Kind::Meter, Kind::Source, Kind::Transformer].into_iter().find(|k| k.name().eq_ignore_ascii_case(s.trim()))
    }

    /// Prefijo del tag zOL y del componente de AdvOneLine.csv (vacío = no tiene). El transformador no tiene tag
    /// zOL propio (sus genies solo animan `zOL\Bus<origen/destino>`); en el csv es `XFR<SimSrcNum>`.
    fn zol(self) -> Option<(&'static str, &'static str)> {
        match self {
            Kind::Breaker => Some(("Bkr", "BKR")),
            Kind::Busbar => Some(("Bus", "")),
            Kind::Meter => Some(("Mtr", "MTR")),
            Kind::Source => Some(("Src", "SRC")),
            Kind::Transformer => Some(("", "XFR")),
            Kind::Ats => None,
        }
    }
}

/// Tipo de objeto del One-Line según las propiedades del genie; `None` si no es del One-Line.
pub fn classify(g: &GenieInstance) -> Option<Kind> {
    let has = |k: &str| g.prop(k).is_some();
    if has("BkrNum1") || has("BkrNum2") {
        Some(Kind::Ats)
    } else if has("BkrNum") {
        Some(Kind::Breaker)
    } else if has("MtrNum") {
        Some(Kind::Meter)
    } else if has("SrcNum") || has("SimSrcNum") {
        if has("SrcBusbarNum") && has("DestBusbarNum") { Some(Kind::Transformer) } else { Some(Kind::Source) }
    } else if has("BusbarNum") {
        Some(Kind::Busbar)
    } else if has("SrcBusbarNum") && has("DestBusbarNum") {
        Some(Kind::Transformer)
    } else {
        None
    }
}

/// Objeto del One-Line en una página.
#[derive(Debug, Clone)]
pub struct Item {
    pub project: String,
    pub page: String,
    pub kind: Kind,
    pub genie: GenieInstance,
}

impl Item {
    fn p(&self, k: &str) -> Option<&str> {
        self.genie.prop(k).map(str::trim).filter(|v| !v.is_empty())
    }

    /// Número que identifica al objeto (BkrNum, BusbarNum, MtrNum, SrcNum; SimSrcNum en transformadores).
    pub fn number(&self) -> Option<&str> {
        match self.kind {
            Kind::Breaker => self.p("BkrNum"),
            Kind::Busbar => self.p("BusbarNum"),
            Kind::Meter => self.p("MtrNum"),
            Kind::Source => self.p("SrcNum").or_else(|| self.p("SimSrcNum")),
            Kind::Transformer => self.p("SimSrcNum").or_else(|| self.p("SrcNum")),
            Kind::Ats => None,
        }
    }

    /// Tag zOL del objeto (`zOL\Bkr12`).
    pub fn zol_tag(&self) -> Option<String> {
        let (t, _) = self.kind.zol().filter(|(t, _)| !t.is_empty())?;
        self.number().map(|n| format!("zOL\\{t}{n}"))
    }

    /// Barras de origen y destino que referencia (SrcBusbarNum*, DestBusbarNum, y la monitoreada de medidores).
    pub fn buses(&self) -> Vec<(&'static str, &str)> {
        let keys: &[&'static str] = match self.kind {
            Kind::Breaker | Kind::Transformer => &["SrcBusbarNum", "DestBusbarNum"],
            Kind::Ats => &["SrcBusbarNum1", "SrcBusbarNum2", "SrcBusbarNum", "DestBusbarNum"],
            Kind::Meter | Kind::Source => &["BusbarNum", "SrcBusbarNum", "DestBusbarNum"],
            Kind::Busbar => &[],
        };
        keys.iter().filter_map(|k| self.p(k).map(|v| (*k, v))).collect()
    }

    pub fn an(&self) -> Option<&str> {
        self.p("AN")
    }

    pub fn json(&self, all_properties: bool) -> Value {
        let mut o = json!({
            "project": self.project, "page": self.page, "kind": self.kind.name(),
            "genie": format!("{}.{}", self.genie.library, self.genie.genie),
        });
        let set = |o: &mut Value, k: &str, v: Option<&str>| {
            if let Some(v) = v {
                o[k] = json!(v);
            }
        };
        set(&mut o, "an", self.an());
        set(&mut o, "number", self.number());
        if let Some(t) = self.zol_tag() {
            o["zol"] = json!(t);
        }
        set(&mut o, "equip", self.p("Equip"));
        set(&mut o, "active", self.p("Active"));
        let buses: BTreeMap<&str, &str> = self.buses().into_iter().collect();
        if !buses.is_empty() {
            o["buses"] = json!(buses);
        }
        if all_properties {
            o["properties"] = json!(self.genie.properties.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<BTreeMap<_, _>>());
        }
        o
    }
}

/// Fila de AdvOneLine.csv: componente (`BKR12`), condición y barras.
#[derive(Debug, Clone, PartialEq)]
pub struct CsvRow {
    pub component: String,
    pub condition: String,
    pub bus1: String,
    pub bus2: String,
}

/// Lee AdvOneLine.csv: salta comentarios (`//`), la cabecera y los colores (`CLR<n>`).
pub fn parse_csv(text: &str) -> Vec<CsvRow> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("//") && !l.to_ascii_lowercase().starts_with("component,"))
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').map(|s| s.trim().trim_matches('"')).collect();
            let c = f.first()?.to_string();
            if c.to_ascii_uppercase().starts_with("CLR") {
                return None;
            }
            Some(CsvRow {
                component: c,
                condition: f.get(1).unwrap_or(&"").to_string(),
                bus1: f.get(2).unwrap_or(&"").to_string(),
                bus2: f.get(3).unwrap_or(&"").to_string(),
            })
        })
        .collect()
}

/// Hallazgo de la validación.
#[derive(Debug, Clone)]
pub struct Finding {
    pub severity: &'static str,
    pub rule: &'static str,
    pub message: String,
    pub at: Vec<Value>,
}

impl Finding {
    fn new(severity: &'static str, rule: &'static str, message: String, items: &[&Item]) -> Self {
        let at = items
            .iter()
            .map(|i| {
                let mut o = json!({"project": i.project, "page": i.page, "genie": format!("{}.{}", i.genie.library, i.genie.genie)});
                if let Some(a) = i.an() {
                    o["an"] = json!(a);
                }
                if let Some(e) = i.p("Equip") {
                    o["equip"] = json!(e);
                }
                o
            })
            .collect();
        Finding { severity, rule, message, at }
    }

    pub fn json(&self) -> Value {
        json!({"severity": self.severity, "rule": self.rule, "message": self.message, "at": self.at})
    }
}

/// Qué conoce la validación del proyecto, además de los objetos de las páginas.
#[derive(Default)]
pub struct Context {
    /// Nombres de equipos (equip.dbf del proyecto y sus incluidos), en minúsculas.
    pub equipment: HashSet<String>,
    /// Nombres de tags (variable.dbf), en minúsculas. Vacío = no se validan tags.
    pub tags: HashSet<String>,
    /// AdvOneLine.csv, si se encontró.
    pub csv: Option<Vec<CsvRow>>,
    /// Textos que no deberían aparecer (p. ej. el prefijo de otra estación), sin distinguir mayúsculas.
    pub foreign: Vec<String>,
    /// Prefijo que deberían tener los equipos (p. ej. la estación de estas páginas).
    pub equip_prefix: Option<String>,
}

fn is_number(s: &str) -> bool {
    s.parse::<u32>().is_ok_and(|n| n > 0)
}

/// Tokens con forma de tag con `\` (`Dispositivo\Nodo\Atributo`) dentro de una expresión, sin los de zOL.
pub fn tag_tokens(expr: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        let t = cur.trim_matches('\\').to_string();
        if t.contains('\\') && !t.to_ascii_lowercase().starts_with("zol\\") && t.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') {
            out.push(t);
        }
        cur.clear();
    };
    for ch in expr.chars() {
        if ch == '"' {
            in_str = !in_str;
            flush(&mut cur, &mut out);
        } else if !in_str && (ch.is_ascii_alphanumeric() || ch == '_' || ch == '\\') {
            cur.push(ch);
        } else {
            flush(&mut cur, &mut out);
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// Valida los objetos del One-Line. Los hallazgos vienen ordenados: errores primero.
pub fn validate(items: &[Item], ctx: &Context) -> Vec<Finding> {
    let mut f = Vec::new();

    // Números: válidos y sin repetir. En la misma página = error (la utilidad: Duplicate*); en páginas distintas =
    // advertencia si es el mismo equipo (puede ser una vista general) y error si el equipo cambia.
    let mut by_id: BTreeMap<(Kind, String), Vec<&Item>> = BTreeMap::new();
    for it in items {
        match (it.kind, it.number()) {
            (Kind::Ats, _) => {}
            (_, None) => f.push(Finding::new("error", "MissingId", format!("{} sin número ({})", it.kind.name(), id_prop(it.kind)), &[it])),
            (_, Some(n)) if !is_number(n) => {
                f.push(Finding::new("error", "InvalidId", format!("{} con {} = '{n}': tiene que ser un entero mayor que 0", it.kind.name(), id_prop(it.kind)), &[it]))
            }
            // Varios transformadores pueden compartir la fuente simulada (la utilidad los renumera juntos): no
            // es un repetido. Ver abajo el choque con fuentes.
            (Kind::Transformer, Some(_)) => {}
            (k, Some(n)) => by_id.entry((k, n.to_string())).or_default().push(it),
        }
    }
    // Fuente simulada de un transformador (SimSrcNum/SimSrcNum2) igual al SrcNum de una fuente: la utilidad le
    // asigna otro número al guardar (FixTransformerInvalidIDs). Se compara el número, no el texto: `07` = `7`.
    let sources: HashSet<u32> =
        items.iter().filter(|i| i.kind == Kind::Source).filter_map(|i| i.number()?.parse().ok()).filter(|&n| n > 0).collect();
    for it in items.iter().filter(|i| i.kind == Kind::Transformer) {
        for k in ["SimSrcNum", "SimSrcNum2"] {
            match it.p(k) {
                // SimSrcNum inválido ya salió arriba como InvalidId (es el número del transformador).
                Some(v) if !is_number(v) => {
                    if k == "SimSrcNum2" {
                        f.push(Finding::new("error", "InvalidId", format!("transformer con {k} = '{v}': tiene que ser un entero mayor que 0"), &[it]))
                    }
                }
                Some(v) if v.parse().is_ok_and(|n: u32| sources.contains(&n)) => f.push(Finding::new(
                    "error",
                    "TransformerSourceClash",
                    format!("transformer con {k} = {v}, el SrcNum de una fuente: la One-Line Configuration Utility le va a asignar otro número"),
                    &[it],
                )),
                _ => {}
            }
        }
    }
    for ((kind, n), list) in &by_id {
        if list.len() < 2 {
            continue;
        }
        let tag = list[0].zol_tag().unwrap_or_default();
        let pages: BTreeSet<(&str, &str)> = list.iter().map(|i| (i.project.as_str(), i.page.as_str())).collect();
        let equips: BTreeSet<String> = list.iter().map(|i| i.p("Equip").unwrap_or("").to_lowercase()).collect();
        let rule = match kind {
            Kind::Breaker => "DuplicateBreaker",
            Kind::Busbar => "DuplicateBusbar",
            Kind::Meter => "DuplicateMeter",
            _ => "DuplicateSource",
        };
        if *kind == Kind::Busbar && equips.len() == 1 {
            // Una barra dibujada en tramos o repetida en varias páginas comparte el número: solo se informa.
            if pages.len() > 1 {
                f.push(Finding::new("info", rule, format!("{tag} aparece en {} páginas", pages.len()), list));
            }
        } else if pages.len() < list.len() {
            f.push(Finding::new("error", rule, format!("{tag} ({} {n}) está repetido en la misma página", kind.name()), list));
        } else if equips.len() > 1 {
            f.push(Finding::new("error", rule, format!("{tag} se usa en {} páginas con equipos distintos: el color de uno va a mostrar el estado del otro", pages.len()), list));
        } else {
            f.push(Finding::new("warning", rule, format!("{tag} se usa en {} páginas (mismo equipo): correcto solo si es la misma barra/equipo en otra vista", pages.len()), list));
        }
    }

    // Barras referenciadas que no existen como genie de barra.
    let busbars: HashSet<&str> = items.iter().filter(|i| i.kind == Kind::Busbar).filter_map(|i| i.number()).collect();
    for it in items {
        let buses = it.buses();
        if matches!(it.kind, Kind::Breaker | Kind::Transformer | Kind::Ats) && buses.is_empty() {
            f.push(Finding::new("warning", "NoSourceOrDestinationBus", format!("{} sin barras de origen ni destino", it.kind.name()), &[it]));
        }
        let src = it.p("SrcBusbarNum");
        let dst = it.p("DestBusbarNum");
        if let (Some(s), Some(d)) = (src, dst) {
            if s == d && s != "0" {
                f.push(Finding::new("error", "SameSourceAndDestinationBus", format!("{} con la misma barra de origen y destino ({s})", it.kind.name()), &[it]));
            }
        }
        for (k, v) in buses {
            if v == "0" {
                continue;
            }
            if !is_number(v) {
                let rule = if k.starts_with("Dest") { "InvalidDestinationBus" } else { "InvalidSourceBus" };
                f.push(Finding::new("error", rule, format!("{k} = '{v}': tiene que ser un número de barra"), &[it]));
            } else if !busbars.is_empty() && !busbars.contains(v) {
                // Normal en salidas (la línea después del interruptor no tiene genie de barra, como en
                // PSO_Equipment/SingleLinesOverview_T300): solo se informa.
                f.push(Finding::new("info", "BusWithoutBusbar", format!("{k} = {v}: no hay genie de barra con BusbarNum {v} en las páginas revisadas (normal en una salida)"), &[it]));
            }
        }
    }

    // Equipos: que existan y que sean de esta estación.
    for it in items {
        match it.p("Equip") {
            Some(e) => {
                if !ctx.equipment.is_empty() && !ctx.equipment.contains(&e.to_lowercase()) {
                    f.push(Finding::new("error", "InvalidEquipment", format!("El equipo '{e}' no existe en equip.dbf (proyecto e incluidos)"), &[it]));
                }
                if let Some(px) = ctx.equip_prefix.as_deref().filter(|p| !p.is_empty()) {
                    if !e.to_lowercase().starts_with(&px.to_lowercase()) {
                        f.push(Finding::new("warning", "ForeignEquipment", format!("El equipo '{e}' no empieza con '{px}'"), &[it]));
                    }
                }
            }
            None if matches!(it.kind, Kind::Breaker | Kind::Meter) && it.genie.prop("Equip").is_some() => {
                f.push(Finding::new("warning", "NoEquipment", format!("{} con Equip vacío", it.kind.name()), &[it]));
            }
            None => {}
        }
        if matches!(it.kind, Kind::Meter | Kind::Source) && it.genie.prop("Active").is_some() && it.p("Active").is_none() {
            f.push(Finding::new("warning", "NoLineActive", format!("{} con Active vacío: la línea no se colorea como activa", it.kind.name()), &[it]));
        }
    }

    // Propiedades que nombran a otra estación o tags que no existen (incluye genies ocultos: se lee el .ctg).
    for it in items {
        for (k, v) in &it.genie.properties {
            for fx in ctx.foreign.iter().filter(|s| !s.trim().is_empty()) {
                if v.to_lowercase().contains(&fx.to_lowercase()) {
                    f.push(Finding::new("warning", "ForeignReference", format!("La propiedad {k} = '{v}' contiene '{fx}'"), &[it]));
                }
            }
            if !ctx.tags.is_empty() {
                for t in tag_tokens(v) {
                    if !ctx.tags.contains(&t.to_lowercase()) {
                        f.push(Finding::new("warning", "UnknownTag", format!("La propiedad {k} usa el tag '{t}', que no está en variable.dbf"), &[it]));
                    }
                }
            }
        }
    }

    // AdvOneLine.csv contra las páginas.
    if let Some(rows) = &ctx.csv {
        let csv: HashMap<String, &CsvRow> = rows.iter().map(|r| (r.component.to_uppercase(), r)).collect();
        let mut on_pages: BTreeMap<String, Vec<&Item>> = BTreeMap::new();
        for it in items {
            if let (Some((_, c)), Some(n)) = (it.kind.zol(), it.number()) {
                if !c.is_empty() {
                    on_pages.entry(format!("{c}{n}")).or_default().push(it);
                }
            }
        }
        for (c, list) in &on_pages {
            if !csv.contains_key(c) {
                f.push(Finding::new("warning", "NotInCsv", format!("{c} está en las páginas pero no en AdvOneLine.csv: regenerar con la One-Line Configuration Utility"), list));
            }
        }
        for r in rows {
            let c = r.component.to_uppercase();
            let known = ["BKR", "SRC", "MTR", "XFR"].iter().any(|p| c.starts_with(p));
            if known && !on_pages.contains_key(&c) {
                f.push(Finding::new("warning", "CsvOrphan", format!("{} está en AdvOneLine.csv pero en ninguna página revisada", r.component), &[]));
            }
        }
    }

    let rank = |s: &str| match s {
        "error" => 0,
        "warning" => 1,
        _ => 2,
    };
    f.sort_by_key(|x| rank(x.severity));
    f
}

/// Tags `zOL\...` de las expresiones expandidas de una página que no corresponden a ningún objeto: restos de una
/// copia (la página quedó usando los objetos de otra). `Bkr`/`Mtr`/`Src` se buscan entre los genies de la misma
/// página; `Bus`, entre las barras que usa cualquier objeto de las páginas revisadas (`known_buses`).
pub fn stale_refs(project: &str, page: &str, page_items: &[&Item], all_busbars: &HashSet<String>, refs: &[(String, String)]) -> Vec<Finding> {
    let own: HashSet<String> = page_items.iter().filter_map(|i| i.zol_tag()).map(|t| t.to_lowercase()).collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (tag, expr) in refs {
        let low = tag.to_lowercase();
        let Some(rest) = low.strip_prefix("zol\\") else { continue };
        let known = if let Some(n) = rest.strip_prefix("bus") {
            all_busbars.is_empty() || all_busbars.contains(n)
        } else if ["bkr", "mtr", "src"].iter().any(|p| rest.starts_with(p)) {
            own.contains(&low)
        } else {
            true
        };
        if !known && seen.insert(low.clone()) {
            out.push(Finding {
                severity: "warning",
                rule: "StaleZolReference",
                message: format!("{tag} aparece en una expresión de la página pero ningún objeto del One-Line lo define: '{expr}'"),
                at: vec![json!({"project": project, "page": page})],
            });
        }
    }
    out
}

/// Números de barra que usa algún objeto: BusbarNum de las barras y las barras de origen/destino de los demás.
pub fn known_buses(items: &[Item]) -> HashSet<String> {
    let mut out = HashSet::new();
    for i in items {
        if i.kind == Kind::Busbar {
            if let Some(n) = i.number() {
                out.insert(n.to_string());
            }
        }
        for (_, v) in i.buses() {
            out.insert(v.to_string());
        }
    }
    out
}

fn id_prop(k: Kind) -> &'static str {
    match k {
        Kind::Breaker => "BkrNum",
        Kind::Busbar => "BusbarNum",
        Kind::Meter => "MtrNum",
        Kind::Source => "SrcNum",
        Kind::Transformer => "SimSrcNum",
        Kind::Ats => "BkrNum1/BkrNum2",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(page: &str, lib: &str, props: &[(&str, &str)]) -> Item {
        let genie = GenieInstance {
            offset: 0,
            library: lib.into(),
            genie: "g".into(),
            properties: props.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        };
        let kind = classify(&genie).expect("debería ser del One-Line");
        Item { project: "P".into(), page: page.into(), kind, genie }
    }

    fn rules(f: &[Finding]) -> Vec<(&str, &str)> {
        f.iter().map(|x| (x.severity, x.rule)).collect()
    }

    #[test]
    fn clasificacion() {
        let k = |props: &[(&str, &str)]| item("p", "l", props).kind;
        assert_eq!(k(&[("BkrNum", "1"), ("SrcBusbarNum", "1"), ("DestBusbarNum", "2")]), Kind::Breaker);
        assert_eq!(k(&[("BkrNum1", "1"), ("BkrNum2", "2")]), Kind::Ats);
        assert_eq!(k(&[("MtrNum", "3"), ("BusbarNum", "1")]), Kind::Meter);
        assert_eq!(k(&[("BusbarNum", "1")]), Kind::Busbar);
        assert_eq!(k(&[("SrcNum", "1"), ("BusbarNum", "1")]), Kind::Source);
        assert_eq!(k(&[("SrcNum", "2"), ("SrcBusbarNum", "1"), ("DestBusbarNum", "2")]), Kind::Transformer);
        let g = GenieInstance { offset: 0, library: "CONTROLS".into(), genie: "x".into(), properties: vec![("Tag".into(), "A".into())] };
        assert_eq!(classify(&g), None);
        assert_eq!(item("p", "l", &[("bkrnum", "12")]).zol_tag().as_deref(), Some("zOL\\Bkr12"));
    }

    /// Propiedades reales de PLS_Include (pls_gen_eq_1.trans_nd_1 y ats_nd, leídas con po_gb_library el 2026-11-26).
    #[test]
    fn transformadores_y_ats_de_pls_include() {
        let xfr = item("p", "pls_gen_eq_1", &[("SimSrcNum", "7"), ("SimSrcNum2", ""), ("SrcBusbarNum", "1"), ("DestBusbarNum", "2")]);
        assert_eq!(xfr.kind, Kind::Transformer);
        assert_eq!(xfr.number(), Some("7"));
        assert_eq!(xfr.zol_tag(), None, "el transformador no anima un zOL\\Src propio");
        let ats = item(
            "p",
            "pls_gen_eq_1",
            &[("BkrNum1", "3"), ("SrcBusbarNum1", "1"), ("Txt3", ""), ("ClosedTag1", ""), ("BkrNum2", "4"), ("SrcBusbarNum2", "5"), ("DestBusbarNum", "2")],
        );
        assert_eq!(ats.kind, Kind::Ats);
        let src = item("p", "pls_gen_eq_1", &[("SrcNum", "7"), ("BusbarNum", "1"), ("Active", "1")]);
        let bus = |n| item("p", "pls_gen_eq_1", &[("BusbarNum", n)]);
        let otro = item("q", "pls_gen_eq_1", &[("SimSrcNum", "7"), ("SrcBusbarNum", "5"), ("DestBusbarNum", "6")]);
        let f = validate(&[xfr.clone(), otro, bus("1"), bus("2"), bus("5"), bus("6")], &Context::default());
        assert!(!f.iter().any(|x| x.rule.starts_with("Duplicate")), "dos transformadores con la misma fuente simulada: {f:?}");
        let f = validate(&[xfr.clone(), src, bus("1"), bus("2")], &Context::default());
        assert_eq!(f.iter().filter(|x| x.rule == "TransformerSourceClash").count(), 1, "{f:?}");
        // Con ceros a la izquierda es el mismo número.
        let src07 = item("p", "pls_gen_eq_1", &[("SrcNum", "07"), ("BusbarNum", "1"), ("Active", "1")]);
        let f = validate(&[xfr.clone(), src07, bus("1"), bus("2")], &Context::default());
        assert_eq!(f.iter().filter(|x| x.rule == "TransformerSourceClash").count(), 1, "07 = 7: {f:?}");
        let mal = item("p", "pls_gen_eq_1", &[("SimSrcNum", "8"), ("SimSrcNum2", "x"), ("SrcBusbarNum", "1"), ("DestBusbarNum", "2")]);
        assert!(validate(&[mal], &Context::default()).iter().any(|x| x.rule == "InvalidId" && x.message.contains("SimSrcNum2")));
        let csv = Context { csv: Some(parse_csv("Component,Condition,Bus1,Bus2\nXFR7,,1,2\n")), ..Default::default() };
        let f = validate(&[xfr, bus("1"), bus("2")], &csv);
        assert!(!f.iter().any(|x| x.rule == "NotInCsv" || x.rule == "CsvOrphan"), "{f:?}");
    }

    #[test]
    fn repetidos_entre_paginas() {
        let a = item("EstA_Unifilar", "pls_ansi_cb_1", &[("BkrNum", "12"), ("Equip", "EstA.Q1"), ("SrcBusbarNum", "1"), ("DestBusbarNum", "2")]);
        let b = item("EstB_Unifilar", "pls_ansi_cb_1", &[("BkrNum", "12"), ("Equip", "EstB.Q1"), ("SrcBusbarNum", "1"), ("DestBusbarNum", "2")]);
        let c = item("EstA_Detalle", "pls_ansi_cb_1", &[("BkrNum", "12"), ("Equip", "EstA.Q1"), ("SrcBusbarNum", "1"), ("DestBusbarNum", "2")]);
        let f = validate(&[a.clone(), b], &Context::default());
        assert_eq!(rules(&f), [("error", "DuplicateBreaker")], "{f:?}");
        assert!(f[0].message.contains("equipos distintos"));
        let f = validate(&[a.clone(), c], &Context::default());
        assert_eq!(rules(&f), [("warning", "DuplicateBreaker")]);
        let f = validate(&[a.clone(), a], &Context::default());
        assert_eq!(rules(&f), [("error", "DuplicateBreaker")]);
        assert!(f[0].message.contains("misma página"));
    }

    #[test]
    fn barras_y_equipos() {
        let ctx = Context { equipment: HashSet::from(["esta.q1".to_string()]), equip_prefix: Some("EstA".into()), ..Default::default() };
        let items = [
            item("p", "pls_gen_base_1", &[("BusbarNum", "1")]),
            item("p", "pls_ansi_cb_1", &[("BkrNum", "1"), ("Equip", "EstA.Q1"), ("SrcBusbarNum", "1"), ("DestBusbarNum", "1")]),
            item("p", "pls_ansi_cb_1", &[("BkrNum", "2"), ("Equip", "EstB.Q9"), ("SrcBusbarNum", "1"), ("DestBusbarNum", "7")]),
            item("p", "pls_ansi_cb_1", &[("BkrNum", "x"), ("Equip", "")]),
        ];
        let f = validate(&items, &ctx);
        let r = rules(&f);
        for want in [
            ("error", "SameSourceAndDestinationBus"),
            ("info", "BusWithoutBusbar"),
            ("error", "InvalidEquipment"),
            ("warning", "ForeignEquipment"),
            ("error", "InvalidId"),
            ("warning", "NoSourceOrDestinationBus"),
            ("warning", "NoEquipment"),
        ] {
            assert!(r.contains(&want), "falta {want:?} en {r:?}");
        }
        assert_eq!(f[0].severity, "error");
    }

    #[test]
    fn referencias_a_otra_estacion_y_tags() {
        // El caso del feedback: un genie oculto con Active apuntando a un medidor de otra estación.
        let ctx = Context {
            foreign: vec!["EstB".into()],
            tags: HashSet::from(["esta_pm4\\mmxu1\\totw".to_string()]),
            ..Default::default()
        };
        let items = [item("p", "pls_gen_eq_1", &[("MtrNum", "4"), ("BusbarNum", "1"), ("Active", "EstB_PM4\\MMXU1\\TotW > 0 AND EstA_PM4\\MMXU1\\TotW > 0")])];
        let f = validate(&items, &ctx);
        let r = rules(&f);
        assert!(r.contains(&("warning", "ForeignReference")), "{r:?}");
        assert_eq!(f.iter().filter(|x| x.rule == "UnknownTag").count(), 1, "{f:?}");
        assert!(f.iter().any(|x| x.message.contains("EstB_PM4\\MMXU1\\TotW")));
    }

    #[test]
    fn csv_contra_paginas() {
        let csv = parse_csv("// Source Color Map\nCLR1,255\nComponent,Condition,Bus1,Bus2\n// Breakers\nBKR1,\"zOL\\Bkr1 = 1\",1,2\nBKR9,x,1,2\n");
        assert_eq!(csv.len(), 2);
        assert_eq!(csv[0], CsvRow { component: "BKR1".into(), condition: "zOL\\Bkr1 = 1".into(), bus1: "1".into(), bus2: "2".into() });
        let ctx = Context { csv: Some(csv), ..Default::default() };
        let items = [
            item("p", "l", &[("BkrNum", "1"), ("SrcBusbarNum", "1"), ("DestBusbarNum", "2")]),
            item("p", "l", &[("BkrNum", "2"), ("SrcBusbarNum", "1"), ("DestBusbarNum", "2")]),
        ];
        let r: Vec<_> = validate(&items, &ctx).into_iter().map(|x| (x.rule, x.message)).collect();
        assert!(r.iter().any(|(k, m)| *k == "NotInCsv" && m.starts_with("BKR2")), "{r:?}");
        assert!(r.iter().any(|(k, m)| *k == "CsvOrphan" && m.starts_with("BKR9")), "{r:?}");
    }

    #[test]
    fn restos_de_copia() {
        let a = item("EstB_Unifilar", "l", &[("BkrNum", "21"), ("SrcBusbarNum", "5"), ("DestBusbarNum", "6")]);
        let buses = HashSet::from(["5".to_string(), "6".to_string()]);
        let refs = vec![
            ("zOL\\Bkr21".to_string(), "PLS_Bkr_Color_Bkr(21, zOL\\Bus5, zOL\\Bus6, zOL\\Bkr21)".to_string()),
            ("zOL\\Bus5".to_string(), "x".to_string()),
            ("zOL\\Bkr1".to_string(), "PLS_Bkr_Color_Bkr(1, zOL\\Bus1, zOL\\Bus2, zOL\\Bkr1)".to_string()),
            ("zOL\\Bus1".to_string(), "PLS_Bkr_Color_Bkr(1, zOL\\Bus1, zOL\\Bus2, zOL\\Bkr1)".to_string()),
        ];
        let f = stale_refs("P", "EstB_Unifilar", &[&a], &buses, &refs);
        let msgs: Vec<&str> = f.iter().map(|x| x.message.as_str()).collect();
        assert_eq!(f.len(), 2, "{msgs:?}");
        assert!(msgs[0].starts_with("zOL\\Bkr1 ") && msgs[1].starts_with("zOL\\Bus1 "), "{msgs:?}");
    }

    #[test]
    fn tokens_de_tags() {
        assert_eq!(tag_tokens("A\\B\\C > 0 AND zOL\\Bkr1 = 1 OR \"x\\y\" = Foo(D\\E)"), ["A\\B\\C", "D\\E"]);
        assert!(tag_tokens("PLS_Bkr_Color_Bus(1)").is_empty());
    }
}
