//! Clonado de páginas del Advanced One-Line (`po_oneline_clone`): reglas puras para renumerar los números zOL
//! (`BkrNum`, `BusbarNum`, `MtrNum`, `SrcNum` y las referencias a barras) y reemplazar textos en las demás
//! propiedades de los genies. `tools/oneline.rs` las aplica en Graphics Builder y después compara el `.ctg` guardado
//! con el mismo plan.
//!
//! Cuatro espacios de numeración, uno por tipo de tag zOL: `Bkr` (BkrNum, BkrNum1, BkrNum2), `Bus` (BusbarNum y
//! cualquier `*BusbarNum*`: SrcBusbarNum, DestBusbarNum, SrcBusbarNum1/2), `Mtr` (MtrNum) y `Src` (SrcNum,
//! SimSrcNum). El mismo número viejo va siempre al mismo número nuevo dentro de un espacio, así que las conexiones
//! (interruptor → barra) se mantienen. 0 y vacío no se tocan (sin número).

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::ctg::GenieInstance;
use crate::oneline::CsvRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Space {
    Bkr,
    Bus,
    Mtr,
    Src,
}

impl Space {
    pub const ALL: [Space; 4] = [Space::Bkr, Space::Bus, Space::Mtr, Space::Src];

    pub fn name(self) -> &'static str {
        match self {
            Space::Bkr => "bkr",
            Space::Bus => "bus",
            Space::Mtr => "mtr",
            Space::Src => "src",
        }
    }

    pub fn parse(s: &str) -> Option<Space> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bkr" | "breaker" => Some(Space::Bkr),
            "bus" | "busbar" => Some(Space::Bus),
            "mtr" | "meter" => Some(Space::Mtr),
            "src" | "source" => Some(Space::Src),
            _ => None,
        }
    }
}

/// Espacio de numeración de una propiedad (None si no numera nada).
pub fn space_of(prop: &str) -> Option<Space> {
    let p = prop.trim().to_ascii_lowercase();
    match p.as_str() {
        "bkrnum" | "bkrnum1" | "bkrnum2" => Some(Space::Bkr),
        "mtrnum" => Some(Space::Mtr),
        // SimSrcNum/SimSrcNum2: fuente simulada de un transformador, en el mismo espacio que SrcNum (la utilidad la
        // renumbera si choca con una fuente: FixTransformerInvalidIDs de OneLineConfig.Business.dll).
        "srcnum" | "simsrcnum" | "simsrcnum2" => Some(Space::Src),
        _ if p.contains("busbarnum") => Some(Space::Bus),
        _ => None,
    }
}

/// Número de una propiedad de numeración: 0 y vacío son "sin número".
fn num(v: &str) -> Option<u32> {
    v.trim().parse::<u32>().ok().filter(|n| *n > 0)
}

/// Números por espacio.
pub type Numbers = BTreeMap<Space, BTreeSet<u32>>;

/// Viejo → nuevo, por espacio.
pub type Maps = BTreeMap<Space, BTreeMap<u32, u32>>;

/// Agrega los números de las propiedades de un genie.
pub fn add_numbers(props: &[(String, String)], into: &mut Numbers) {
    for (k, v) in props {
        if let (Some(sp), Some(n)) = (space_of(k), num(v)) {
            into.entry(sp).or_default().insert(n);
        }
    }
}

/// Agrega los números de AdvOneLine.csv (`BKR12`, `SRC3`, `MTR4`, `XFR5` y las barras Bus1/Bus2). `XFR<n>` va al
/// espacio `src`: n es el `SimSrcNum` del transformador (OneLineConfig.Business.dll arma el componente con
/// "XFR" + el ID que GenieDefinitions.xml mapea a SimSrcNum).
pub fn add_csv(rows: &[CsvRow], into: &mut Numbers) {
    for r in rows {
        let c = r.component.trim().to_ascii_uppercase();
        let sp = match c.get(..3) {
            Some("BKR") => Some(Space::Bkr),
            Some("SRC") | Some("XFR") => Some(Space::Src),
            Some("MTR") => Some(Space::Mtr),
            _ => None,
        };
        if let (Some(sp), Some(n)) = (sp, c.get(3..).and_then(num)) {
            into.entry(sp).or_default().insert(n);
        }
        for b in [&r.bus1, &r.bus2] {
            if let Some(n) = num(b) {
                into.entry(Space::Bus).or_default().insert(n);
            }
        }
    }
}

/// Cómo elegir los números nuevos.
#[derive(Debug, Clone, PartialEq)]
pub enum Numbering {
    /// Desde `start` (o el mayor usado + 1), saltando los usados, en el orden de los números viejos.
    NextFree { start: Option<u32> },
    /// Viejo + offset; error si alguno ya está usado.
    Offset(u32),
    /// No renumerar.
    Keep,
}

/// Asigna números nuevos a los `source` de cada espacio de `spaces`, sin repetir ninguno de `used`.
pub fn allocate(source: &Numbers, used: &Numbers, spaces: &[Space], how: &Numbering) -> Result<Maps> {
    let mut maps = Maps::new();
    for sp in spaces {
        let Some(src) = source.get(sp) else { continue };
        let empty = BTreeSet::new();
        let taken = used.get(sp).unwrap_or(&empty);
        let mut m = BTreeMap::new();
        match how {
            Numbering::Keep => {}
            Numbering::Offset(off) => {
                let mut clash = Vec::new();
                for &o in src {
                    let Some(n) = o.checked_add(*off) else { bail!("offset {off}: el número {} {o} se pasa de rango", sp.name()) };
                    if taken.contains(&n) {
                        clash.push(n);
                    }
                    m.insert(o, n);
                }
                if !clash.is_empty() {
                    bail!(
                        "offset {off}: los números {} {clash:?} ya están usados en el proyecto. Usá otro offset o renumber=\"next_free\".",
                        sp.name()
                    );
                }
            }
            Numbering::NextFree { start } => {
                let mut n = start.unwrap_or_else(|| taken.iter().next_back().map_or(1, |m| m + 1)).max(1);
                for &o in src {
                    while taken.contains(&n) {
                        n += 1;
                    }
                    m.insert(o, n);
                    n += 1;
                }
            }
        }
        if !m.is_empty() {
            maps.insert(*sp, m);
        }
    }
    Ok(maps)
}

/// Reemplaza cada `from` por `to` sin distinguir mayúsculas (ASCII), en orden: un reemplazo ve el resultado del
/// anterior.
pub fn replace_all(v: &str, reps: &[(String, String)]) -> String {
    let mut s = v.to_string();
    for (from, to) in reps {
        if from.is_empty() {
            continue;
        }
        // to_ascii_lowercase no cambia el largo en bytes: las posiciones valen para `s`.
        let lower = s.to_ascii_lowercase();
        let f = from.to_ascii_lowercase();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while let Some(p) = lower[i..].find(&f) {
            out.push_str(&s[i..i + p]);
            out.push_str(to);
            i += p + f.len();
        }
        out.push_str(&s[i..]);
        s = out;
    }
    s
}

/// Reglas de un clonado.
#[derive(Debug, Clone, Default)]
pub struct Rules {
    pub maps: Maps,
    pub replace: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub property: String,
    pub old: String,
    pub new: String,
}

impl Change {
    pub fn json(&self) -> Value {
        json!({"property": self.property, "old": self.old, "new": self.new})
    }
}

/// Cambios que las reglas hacen en las propiedades de un genie: números de los espacios renumerados y reemplazos de
/// texto en las demás (salvo `AN`).
pub fn changes(props: &[(String, String)], rules: &Rules) -> Vec<Change> {
    props
        .iter()
        .filter_map(|(k, v)| {
            let new = match space_of(k) {
                Some(sp) => rules.maps.get(&sp)?.get(&num(v)?)?.to_string(),
                None if k.trim().eq_ignore_ascii_case("AN") => return None,
                None => replace_all(v, &rules.replace),
            };
            (new != *v).then(|| Change { property: k.clone(), old: v.clone(), new })
        })
        .collect()
}

/// El genie como quedaría con las reglas aplicadas.
pub fn apply(g: &GenieInstance, rules: &Rules) -> GenieInstance {
    let mut out = g.clone();
    for c in changes(&g.properties, rules) {
        for (k, v) in out.properties.iter_mut() {
            if *k == c.property {
                *v = c.new.clone();
            }
        }
    }
    out
}

/// Propiedades de numeración con un valor que no es un número (p. ej. una expresión): no se renumeran.
pub fn non_numeric(props: &[(String, String)]) -> Vec<(String, String)> {
    props
        .iter()
        .filter(|(k, v)| space_of(k).is_some() && !v.trim().is_empty() && v.trim().parse::<u32>().is_err())
        .cloned()
        .collect()
}

/// Compara los genies del `.ctg` guardado con los de origen: cada cambio del plan tiene que estar en el genie de la
/// misma posición. Error si no tienen la misma cantidad o el mismo orden (no se puede comparar).
pub fn not_applied(source: &[GenieInstance], saved: &[GenieInstance], rules: &Rules) -> Result<Vec<Value>, String> {
    if source.len() != saved.len() {
        return Err(format!("la página guardada tiene {} genies y la de origen {}: no se pudo comparar genie por genie", saved.len(), source.len()));
    }
    let mut out = Vec::new();
    for (i, (a, b)) in source.iter().zip(saved).enumerate() {
        if !a.library.eq_ignore_ascii_case(&b.library) || !a.genie.eq_ignore_ascii_case(&b.genie) {
            return Err(format!(
                "el genie {i} de la página guardada es {}.{} y en la de origen {}.{}: no se pudo comparar genie por genie",
                b.library, b.genie, a.library, a.genie
            ));
        }
        for c in changes(&a.properties, rules) {
            let found = b.prop(&c.property);
            if found != Some(c.new.as_str()) {
                out.push(json!({
                    "index": i, "genie": format!("{}.{}", a.library, a.genie), "an": a.prop("AN"),
                    "property": c.property, "expected": c.new, "found": found,
                }));
            }
        }
    }
    Ok(out)
}

pub fn maps_json(maps: &Maps) -> Value {
    let o: serde_json::Map<String, Value> = maps
        .iter()
        .map(|(sp, m)| (sp.name().to_string(), json!(m.iter().map(|(a, b)| (a.to_string(), json!(b))).collect::<serde_json::Map<_, _>>())))
        .collect();
    Value::Object(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(p: &[(&str, &str)]) -> Vec<(String, String)> {
        p.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn genie(p: &[(&str, &str)]) -> GenieInstance {
        GenieInstance { offset: 0, library: "pls_ansi_cb_1".into(), genie: "Breaker_1".into(), properties: props(p) }
    }

    fn set(v: &[u32]) -> BTreeSet<u32> {
        v.iter().copied().collect()
    }

    #[test]
    fn espacios() {
        assert_eq!(space_of("BkrNum"), Some(Space::Bkr));
        assert_eq!(space_of("BkrNum2"), Some(Space::Bkr));
        assert_eq!(space_of("SrcBusbarNum1"), Some(Space::Bus));
        assert_eq!(space_of("DestBusbarNum"), Some(Space::Bus));
        assert_eq!(space_of("SimSrcNum"), Some(Space::Src));
        assert_eq!(space_of("SimSrcNum2"), Some(Space::Src));
        assert_eq!(space_of("Equip"), None);
        assert_eq!(Space::parse("Busbar"), Some(Space::Bus));
    }

    #[test]
    fn numeros_de_genies_y_csv() {
        let mut n = Numbers::new();
        add_numbers(&props(&[("BkrNum", "12"), ("SrcBusbarNum", "3"), ("DestBusbarNum", "0"), ("Equip", "EstA.Q1")]), &mut n);
        let csv = [
            CsvRow { component: "BKR40".into(), condition: "x".into(), bus1: "7".into(), bus2: "".into() },
            CsvRow { component: "XFR9".into(), condition: "".into(), bus1: "7".into(), bus2: "8".into() },
        ];
        add_csv(&csv, &mut n);
        assert_eq!(n[&Space::Bkr], set(&[12, 40]));
        assert_eq!(n[&Space::Bus], set(&[3, 7, 8]));
        assert_eq!(n[&Space::Src], set(&[9]));
    }

    #[test]
    fn siguiente_libre_salta_los_usados() {
        let source = Numbers::from([(Space::Bkr, set(&[1, 2, 5])), (Space::Bus, set(&[1, 2]))]);
        let used = Numbers::from([(Space::Bkr, set(&[1, 2, 5, 8])), (Space::Bus, set(&[1, 2, 3]))]);
        let m = allocate(&source, &used, &Space::ALL, &Numbering::NextFree { start: None }).unwrap();
        assert_eq!(m[&Space::Bkr], BTreeMap::from([(1, 9), (2, 10), (5, 11)]));
        assert_eq!(m[&Space::Bus], BTreeMap::from([(1, 4), (2, 5)]));
        let m = allocate(&source, &used, &[Space::Bkr], &Numbering::NextFree { start: Some(7) }).unwrap();
        assert_eq!(m[&Space::Bkr], BTreeMap::from([(1, 7), (2, 9), (5, 10)]));
        assert!(!m.contains_key(&Space::Bus));
    }

    #[test]
    fn offset_choca_con_usados() {
        let source = Numbers::from([(Space::Bkr, set(&[1, 2]))]);
        let used = Numbers::from([(Space::Bkr, set(&[1, 2, 102]))]);
        let e = allocate(&source, &used, &Space::ALL, &Numbering::Offset(100)).unwrap_err().to_string();
        assert!(e.contains("[102]"), "{e}");
        let m = allocate(&source, &used, &Space::ALL, &Numbering::Offset(200)).unwrap();
        assert_eq!(m[&Space::Bkr], BTreeMap::from([(1, 201), (2, 202)]));
        assert!(allocate(&source, &used, &Space::ALL, &Numbering::Keep).unwrap().is_empty());
    }

    #[test]
    fn reemplazos_sin_mayusculas_y_en_orden() {
        let r = vec![("esta_".to_string(), "EstB_".to_string()), ("EstB_PM4".to_string(), "EstB_PM5".to_string())];
        assert_eq!(replace_all("EstA_PM4\\MMXU1\\TotW > 0 AND ESTA_Q1", &r), "EstB_PM5\\MMXU1\\TotW > 0 AND EstB_Q1");
        assert_eq!(replace_all("sin cambios", &r), "sin cambios");
    }

    #[test]
    fn cambios_de_un_genie() {
        let rules = Rules {
            maps: Maps::from([(Space::Bkr, BTreeMap::from([(12, 40)])), (Space::Bus, BTreeMap::from([(3, 8), (4, 9)]))]),
            replace: vec![("EstA".into(), "EstB".into())],
        };
        let g = genie(&[("BkrNum", "12"), ("SrcBusbarNum", "3"), ("DestBusbarNum", "4"), ("Equip", "EstA.Q1"), ("AN", "12"), ("Active", "1")]);
        let ch = changes(&g.properties, &rules);
        let got: Vec<(String, String, String)> = ch.into_iter().map(|c| (c.property, c.old, c.new)).collect();
        assert_eq!(
            got,
            vec![
                ("BkrNum".into(), "12".into(), "40".into()),
                ("SrcBusbarNum".into(), "3".into(), "8".into()),
                ("DestBusbarNum".into(), "4".into(), "9".into()),
                ("Equip".into(), "EstA.Q1".into(), "EstB.Q1".into()),
            ]
        );
        let a = apply(&g, &rules);
        assert_eq!(a.prop("BkrNum"), Some("40"));
        assert_eq!(a.prop("AN"), Some("12"));
        assert!(changes(&a.properties, &Rules { maps: Maps::new(), replace: vec![] }).is_empty());
    }

    #[test]
    fn verificacion_contra_lo_guardado() {
        let rules = Rules { maps: Maps::from([(Space::Bkr, BTreeMap::from([(12, 40)]))]), replace: vec![] };
        let src = vec![genie(&[("BkrNum", "12"), ("AN", "7")])];
        assert!(not_applied(&src, &[apply(&src[0], &rules)], &rules).unwrap().is_empty());
        let left = not_applied(&src, &src, &rules).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0]["expected"], "40");
        assert_eq!(left[0]["found"], "12");
        assert!(not_applied(&src, &[], &rules).is_err());
    }

    #[test]
    fn numeros_que_no_son_numeros() {
        assert_eq!(non_numeric(&props(&[("BkrNum", "%X%"), ("MtrNum", " 4 "), ("BusbarNum", "")])), props(&[("BkrNum", "%X%")]));
    }
}
