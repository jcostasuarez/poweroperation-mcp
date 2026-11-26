//! Lectura offline (sin Graphics Builder) de páginas gráficas `.ctg`: genies colocados con sus propiedades y
//! referencias a tags del Advanced One-Line (`zOL\Bkr12`) en las expresiones ya expandidas.
//!
//! El formato no está documentado. Lo que se usa salió de páginas reales de PO 2024 (PLS_Include, PSO_Equipment,
//! MCP_Demo) y es solo esto:
//! - Las cadenas van con su largo delante: `u16` little-endian + bytes en Windows-1252.
//! - Un genie colocado es la cadena `<librería>.<genie>.<ctm` (sin `#...` detrás: esas son la tabla de librerías
//!   de la cabecera), 14 bytes de cabecera (el primer `u16` en 0 = genie sin propiedades) y bloques consecutivos
//!   `u16 tamaño` + (`u16 cantidad` + pares (nombre, valor) de cadenas): las propiedades del genie tal como se
//!   completaron al colocarlo (`Tag` = `Feeder1\XCBR1\Trip`). Ver `genie_properties`.
//! - Las animaciones guardan las expresiones expandidas (`PLS_Bkr_Color_Bkr(12, zOL\Bus3, ...)`) como cadenas
//!   comunes.
//!
//! Lo que no se reconoce se cuenta en `unparsed` en vez de adivinarse.

use encoding_rs::WINDOWS_1252;

/// Un genie colocado en una página.
#[derive(Debug, Clone, PartialEq)]
pub struct GenieInstance {
    /// Posición en el archivo (para diagnosticar y ordenar).
    pub offset: usize,
    pub library: String,
    pub genie: String,
    /// Propiedades en el orden del archivo (nombre, valor).
    pub properties: Vec<(String, String)>,
}

impl GenieInstance {
    /// Valor de una propiedad, sin distinguir mayúsculas en el nombre.
    pub fn prop(&self, name: &str) -> Option<&str> {
        self.properties.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Resultado de leer los genies de una página.
#[derive(Debug, Default)]
pub struct Genies {
    pub genies: Vec<GenieInstance>,
    /// Referencias a genies cuyas propiedades no se pudieron leer (formato no reconocido).
    pub unparsed: Vec<String>,
}

const CTM: &[u8] = b".<ctm";

fn decode(b: &[u8]) -> String {
    WINDOWS_1252.decode_without_bom_handling(b).0.into_owned()
}

fn u16_at(d: &[u8], o: usize) -> Option<usize> {
    d.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
}

fn printable(b: &[u8]) -> bool {
    b.iter().all(|&c| c >= 0x20 || c == b'\t')
}

/// Cadena con largo delante en `o`: (texto, posición siguiente).
fn lp_string(d: &[u8], o: usize) -> Option<(String, usize)> {
    let n = u16_at(d, o)?;
    let b = d.get(o + 2..o + 2 + n)?;
    printable(b).then(|| (decode(b), o + 2 + n))
}

/// Nombre de propiedad de genie: letra o `_` al principio; letras, dígitos, `_` y espacios (`Equip Name`).
fn prop_name(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(f) if f.is_ascii_alphabetic() || f == '_')
        && s.len() <= 64
        && s.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == ' ')
}

/// Pares (nombre, valor) de un bloque: `u16 cantidad` + pares en `o`, sin pasar de `limit`.
fn pairs_at(d: &[u8], o: usize, limit: usize) -> Option<Vec<(String, String)>> {
    let n = u16_at(d, o)?;
    if n > 500 {
        return None;
    }
    let mut pos = o + 2;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let (k, next) = lp_string(d, pos)?;
        if !prop_name(&k) {
            return None;
        }
        let (v, next) = lp_string(d, next)?;
        if next > limit {
            return None;
        }
        out.push((k, v));
        pos = next;
    }
    Some(out)
}

/// Propiedades de un genie colocado: después de la referencia, `u16` (0 = sin propiedades), 12 bytes fijos y
/// bloques consecutivos `u16 tamaño` + (`u16 cantidad` + pares) de `tamaño` bytes. Hay páginas con un bloque vacío
/// y otro completo (CONTROLS.Toggle_Button_1) y con uno parcial y otro completo (pls_easergy): se juntan y, si una
/// propiedad se repite, queda el último valor. `None` si el formato no coincide.
fn genie_properties(d: &[u8], end: usize) -> Option<Vec<(String, String)>> {
    if u16_at(d, end)? == 0 {
        return Some(vec![]);
    }
    if let Some(p) = blocks_from(d, end + 14) {
        return Some(p);
    }
    // Variante (p. ej. vigilohmifl12, switchgear monitoring): antes van bloques de la hoja de propiedades
    // (`!*PropSheet.Focus`, `#1421.#1404`), cuyas claves no son nombres de propiedad. Se busca la primera cadena de
    // bloques válida que empiece con un bloque no vacío.
    (end + 6..(end + 600).min(d.len())).find_map(|pos| match u16_at(d, pos + 2) {
        Some(n) if n > 0 => blocks_from(d, pos).filter(|p| !p.is_empty()),
        _ => None,
    })
}

/// Bloques consecutivos de propiedades desde `pos` (ver `genie_properties`).
fn blocks_from(d: &[u8], mut pos: usize) -> Option<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut blocks = 0;
    while blocks < 8 {
        let Some(size) = u16_at(d, pos) else { break };
        let limit = pos + 2 + size;
        if size < 2 || limit > d.len() {
            break;
        }
        let Some(pairs) = pairs_at(d, pos + 2, limit) else { break };
        for (k, v) in pairs {
            match out.iter_mut().find(|(x, _)| x.eq_ignore_ascii_case(&k)) {
                Some(slot) => slot.1 = v,
                None => out.push((k, v)),
            }
        }
        pos = limit;
        blocks += 1;
    }
    (blocks > 0).then_some(out)
}

/// Referencia `<librería>.<genie>.<ctm` que termina en `end` (posición después de `.<ctm`): (inicio, texto).
fn genie_ref(d: &[u8], end: usize) -> Option<(usize, String)> {
    if d.get(end) == Some(&b'#') {
        return None; // tabla de librerías de la cabecera
    }
    let ok = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'!' | b'.' | b' ' | b'$');
    // Se agranda el nombre hacia atrás hasta que el `u16` anterior coincida con el largo. El byte alto del largo
    // (0) no es un carácter válido, así que el recorrido termina solo.
    for len in CTM.len() + 3..=260.min(end) {
        let start = end - len;
        if start < 2 || !ok(d[start]) {
            break;
        }
        if u16_at(d, start - 2) == Some(len) {
            return Some((start, decode(&d[start..end])));
        }
    }
    None
}

/// Genies colocados en una página (o en una librería `.ctm`, para los genies anidados).
pub fn genies(d: &[u8]) -> Genies {
    let mut out = Genies::default();
    let mut i = 0;
    while let Some(p) = d[i..].windows(CTM.len()).position(|w| w == CTM) {
        let end = i + p + CTM.len();
        i = end;
        let Some((start, text)) = genie_ref(d, end) else { continue };
        let name = &text[..text.len() - CTM.len()];
        let Some((library, genie)) = name.split_once('.') else { continue };
        match genie_properties(d, end) {
            Some(properties) => out.genies.push(GenieInstance { offset: start, library: library.to_string(), genie: genie.to_string(), properties }),
            None => out.unparsed.push(name.to_string()),
        }
    }
    out
}

/// Tags `zOL\<Tipo><N>` que aparecen en las cadenas de la página (expresiones de animación expandidas):
/// (tag tal como figura, expresión que lo contiene). Sin repetir la misma pareja.
pub fn zol_refs(d: &[u8]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while let Some(p) = d[i..].windows(4).position(|w| w.eq_ignore_ascii_case(b"zOL\\")) {
        let at = i + p;
        i = at + 4;
        // Nombre del tag: zOL\ + letras + dígitos.
        let mut e = at + 4;
        while e < d.len() && d[e].is_ascii_alphabetic() {
            e += 1;
        }
        let letters = e;
        while e < d.len() && d[e].is_ascii_digit() {
            e += 1;
        }
        if letters == at + 4 || e == letters {
            continue; // zOL\OffColor, zOL\Bus%DestBusbarNum% (sin expandir): no son de un objeto
        }
        let tag = decode(&d[at..e]);
        let expr = enclosing_string(d, at, e);
        if !out.iter().any(|(t, x)| t.eq_ignore_ascii_case(&tag) && x == &expr) {
            out.push((tag, expr));
        }
    }
    out
}

/// Cadena (con largo delante) que contiene los bytes `at..e`. Si no se encuentra el largo, la corrida imprimible
/// alrededor (hasta 400 caracteres por lado).
fn enclosing_string(d: &[u8], at: usize, e: usize) -> String {
    let isp = |c: u8| c >= 0x20 || c == b'\t';
    let mut s = at;
    while s > 0 && isp(d[s - 1]) && at - s < 400 {
        s -= 1;
    }
    // El byte bajo del largo puede ser imprimible ('[' = 91): el texto empieza en `s` o en `s + 1`.
    for cand in [s, s + 1] {
        if cand >= 2 && cand <= at {
            if let Some(n) = u16_at(d, cand - 2) {
                if cand + n >= e && cand + n <= d.len() && printable(&d[cand..cand + n]) {
                    return decode(&d[cand..cand + n]);
                }
            }
        }
    }
    let mut f = e;
    while f < d.len() && isp(d[f]) && f - e < 400 {
        f += 1;
    }
    decode(&d[s..f]).trim().to_string()
}

/// Arma bytes con el formato descripto arriba, para los tests de este módulo y de `oneline`.
#[cfg(test)]
pub mod fake {
    fn lp(out: &mut Vec<u8>, s: &str) {
        let b = crate::text::encode(s);
        out.extend_from_slice(&(b.len() as u16).to_le_bytes());
        out.extend_from_slice(&b);
    }

    /// Un genie colocado como los de PO 2024 (cabecera de 18 bytes, como `CONTROLS.Toggle_Button_1`).
    pub fn genie(library: &str, genie: &str, props: &[(&str, &str)]) -> Vec<u8> {
        let mut out = vec![0x78, 0x00, 0x18, 0x01];
        lp(&mut out, &format!("{library}.{genie}.<ctm"));
        out.extend_from_slice(&[0x01, 0x00, 0x02, 0, 0, 0, 0x0c, 0, 0, 0, 0x02, 0, 0, 0, 0x02, 0, 0, 0]);
        let mut block = Vec::new();
        block.extend_from_slice(&(props.len() as u16).to_le_bytes());
        for (k, v) in props {
            lp(&mut block, k);
            lp(&mut block, v);
        }
        out.extend_from_slice(&((block.len() + 11) as u16).to_le_bytes());
        out.extend_from_slice(&block);
        out.extend_from_slice(&[0x02, 0, 0, 0, 0x01, 0, 0, 0, 0x80, 0, 0]);
        out
    }

    /// Animación con una expresión expandida (par COL_A = expresión).
    pub fn animation(expr: &str) -> Vec<u8> {
        let mut out = vec![0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x49, 0x00, 0x01, 0x00];
        lp(&mut out, "COL_A");
        lp(&mut out, expr);
        out
    }

    /// Página: cabecera con la tabla de librerías (`...<ctm#...`, que no son genies colocados) + partes.
    pub fn page(parts: &[Vec<u8>]) -> Vec<u8> {
        let mut out = b"\x00\x00attributes\x00".to_vec();
        lp(&mut out, "standard.normal@-XGA.<ctt#dd67fb9e");
        lp(&mut out, "pls_ansi_cb_1.Breaker_1.<ctm#dd67fb98");
        for p in parts {
            out.extend_from_slice(&[0x00; 12]);
            out.extend_from_slice(p);
        }
        out.extend_from_slice(&[0x00; 16]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes reales de PO 2024 (MCP_Demo/MCP_GB_Test.ctg): genie CONTROLS.Toggle_Button_1 con dos propiedades.
    /// (un bloque vacío y otro con las propiedades).
    const REAL_TOGGLE: &[u8] = b"\x78\x00\x04\x01\xdc\x00\x1a\x01\x1d\x00CONTROLS.Toggle_Button_1.<ctm\x01\x00\x02\x00\x00\x00\x0c\x00\x00\x00\x02\x00\x00\x00\x02\x00\x00\x009\x00\x02\x00\x0b\x00Button_Text\x04\x00Trip\x03\x00Tag\x12\x00Feeder1\\XCBR1\\Trip\x02\x00\x00\x00\x01\x00\x00\x00\x80\x00\x00\x00\x80\x00\x00\x00";

    /// Bytes reales (PSO_Equipment/!GalaxyUPS.ctg): nombres de propiedad con espacios.
    const REAL_EQUIP_NAME: &[u8] = b"\x00\x00\x1f\x00pso_general.pso_equip_name.<ctm\x01\x00\x02\x00\x00\x00\x0c\x00\x00\x00\x0c\x00\x00\x00\x02\x00\x00\x00&\x00\x02\x00\x02\x00AN\x03\x00146\n\x00Equip Name\x02\x0095\x02\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00M\x01";

    /// Bytes reales (PLS_Include/!PLS_Splash.ctg): genie sin propiedades (primer u16 en 0).
    const REAL_SPLASH: &[u8] = b"\x00\x00\x17\x00pls_style_1.splash.<ctm\x00\x00\x02\x00\x00\x00\x0c\x00\x00\x00\x02\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00";

    /// Interruptor Easergy de una página One-Line real (PSO_Equipment/SingleLinesOverview_T300.ctg): un bloque con
    /// una propiedad y otro con todas. Mismos bytes que el archivo, armados por partes para que se lean.
    fn real_easergy() -> Vec<u8> {
        let lp = |o: &mut Vec<u8>, s: &[u8]| {
            o.extend_from_slice(&(s.len() as u16).to_le_bytes());
            o.extend_from_slice(s);
        };
        let mut d = vec![0x00, 0x00];
        lp(&mut d, b"pls_easergy.hv_sw_fd_nd_c_2.<ctm");
        d.extend_from_slice(b"\x01\x00\x02\x00\x00\x00\x0c\x00\x00\x00\x02\x00\x00\x00");
        d.extend_from_slice(b"\x12\x00\x01\x00\x0b\x00ValueMultip\x01\x001");
        let props: [(&[u8], &[u8]); 11] = [
            (b"ValueMultip", b"1"), (b"AN", b"809"), (b"Equip", b"MA_T300_SC01"), (b"BkrNum", b"2"), (b"SrcBusbarNum", b"1"),
            (b"DestBusbarNum", b"2"), (b"Txt1", b"MA"), (b"Txt2", b"SC01"), (b"DetailsFn", b"PLSSC150ShowDetail(\"MA_T300_SC01\",\"SW\")"),
            (b"ValueType", b"MMXU1\\A\\zgnd"), (b"ValueUnits", b"Amps"),
        ];
        let mut block = 11u16.to_le_bytes().to_vec();
        for (k, v) in props {
            lp(&mut block, k);
            lp(&mut block, v);
        }
        block.extend_from_slice(b"\x02\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00");
        assert_eq!(block.len(), 0xde, "el bloque real mide 0xde bytes");
        d.extend_from_slice(&(block.len() as u16).to_le_bytes());
        d.extend_from_slice(&block);
        d.extend_from_slice(b"\x00\x00\x00\x00\x00)\x02\x00\x00");
        d
    }

    #[test]
    fn genies_reales() {
        let g = genies(REAL_TOGGLE);
        assert!(g.unparsed.is_empty(), "{:?}", g.unparsed);
        assert_eq!(g.genies.len(), 1);
        assert_eq!((g.genies[0].library.as_str(), g.genies[0].genie.as_str()), ("CONTROLS", "Toggle_Button_1"));
        assert_eq!(g.genies[0].prop("tag"), Some("Feeder1\\XCBR1\\Trip"));
        assert_eq!(g.genies[0].prop("Button_Text"), Some("Trip"));

        let g = genies(REAL_EQUIP_NAME);
        assert_eq!(g.genies[0].properties, vec![("AN".into(), "146".into()), ("Equip Name".into(), "95".into())]);

        let g = genies(REAL_SPLASH);
        assert!(g.unparsed.is_empty() && g.genies.len() == 1 && g.genies[0].properties.is_empty(), "{g:?}");

        // Variante con bloques de la hoja de propiedades delante (PSO_Equipment/!VigilohmIFL12_Settings.ctg).
        let real_vigilohm: &[u8] = b"\x00\x00\x22\x00vigilohmifl12.pls_ifl_ntwfltr.<ctm\x01\x00\x02\x00\x00\x00L\x00\x03\x00\x11\x00!*PropSheet.Focus\x0b\x00#1421.#1404\x07\x00!*#1989\x05\x00#1404\x07\x00!*#1421\x05\x00#1404\x02\x00\x00\x00\x02\x00\x00\x00'\x00\x02\x00\x02\x00AN\x02\x0026\x0b\x00NtwrkFilter\x03\x00485\x02\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\xb3\x00";
        let g = genies(real_vigilohm);
        assert!(g.unparsed.is_empty(), "{g:?}");
        assert_eq!(g.genies[0].properties, vec![("AN".into(), "26".into()), ("NtwrkFilter".into(), "485".into())]);

        let g = genies(&real_easergy());
        assert!(g.unparsed.is_empty(), "{g:?}");
        let e = &g.genies[0];
        assert_eq!(e.properties.len(), 11);
        assert_eq!((e.prop("BkrNum"), e.prop("Equip"), e.prop("DestBusbarNum")), (Some("2"), Some("MA_T300_SC01"), Some("2")));
    }

    #[test]
    fn cabecera_de_libreria_no_es_genie() {
        let d = fake::page(&[fake::genie("pls_ansi_cb_1", "Breaker_1", &[("BkrNum", "12"), ("Equip", "EstA.Q1")])]);
        let g = genies(&d);
        assert_eq!(g.genies.len(), 1, "{g:?}");
        assert_eq!(g.genies[0].prop("BkrNum"), Some("12"));
        assert!(g.unparsed.is_empty());
    }

    #[test]
    fn formato_desconocido_se_informa() {
        let mut d = vec![0u8; 4];
        d.extend_from_slice(b"\x0c\x00lib.gen.<ctm");
        d.extend_from_slice(&[0xFF; 60]);
        let g = genies(&d);
        assert!(g.genies.is_empty());
        assert_eq!(g.unparsed, vec!["lib.gen".to_string()]);
    }

    #[test]
    fn referencias_zol() {
        let d = fake::page(&[
            fake::animation("PLS_Bkr_Color_Bkr(12, zOL\\Bus3, zOL\\Bus4, zOL\\Bkr12)"),
            fake::animation("zOL\\OffColor"),
            fake::animation("PLS_Bkr_Color_Bus(zOL\\Bus%SrcBusbarNum%)"),
        ]);
        let r = zol_refs(&d);
        let tags: Vec<&str> = r.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(tags, ["zOL\\Bus3", "zOL\\Bus4", "zOL\\Bkr12"]);
        assert!(r[0].1.starts_with("PLS_Bkr_Color_Bkr("), "{}", r[0].1);
    }
}
