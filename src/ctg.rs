//! Lectura offline (sin Graphics Builder) de páginas gráficas `.ctg`: genies colocados con sus propiedades y
//! referencias a tags del Advanced One-Line (`zOL\Bkr12`) en las expresiones ya expandidas.
//!
//! Dos niveles:
//! - **Contenedor** (`objects`): sacado de `Ctg32.dll` y verificado contra las 111 `.ctg` y 56 `.ctm` del equipo de
//!   desarrollo (2026-11-27). Enteros little-endian. Cabecera de 16 bytes (`u16` formato 7..9, `u16` versión, dos
//!   fechas, `u32` cantidad de entradas) y directorio desde 0x10 con entradas de 0x2A bytes (`u8` en uso, `u8`
//!   última, nombre de 32 bytes, `u32` offset, `u32` espacio reservado). Una `.ctg` tiene las páginas `page` y
//!   `attributes`; una `.ctm`, una por genie. Cada página: cabecera (0x34 bytes desde la versión 12, si no 0x24:
//!   `u32` tamaño de los objetos con sus cabeceras, `u32` cantidad, `u32` miniatura, extent, `u32` imports en
//!   unidades de 0x60, fecha y GUID), los imports (librerías que usa, `<lib>.<genie>.<ctm#fecha`, cadenas sin largo
//!   delante), la cadena de objetos (`u32` tamaño, `u32` siguiente relativo al inicio de la zona, payload) y la
//!   miniatura. Se leen solo los payloads de los objetos de páginas en uso: no se toman restos de páginas borradas
//!   ni de huecos (frecuentes en `.ctm`) y nada se lee cruzando de un objeto al siguiente. Si el archivo no cumple la
//!   estructura, se lee entero (cada paso de abajo funciona también sin el contenedor).
//! - **Payload** de cada objeto: lo interpreta Graphics Builder, no Ctg32, y no está documentado. Lo que se usa salió
//!   de páginas reales de PO 2024 (PLS_Include, PSO_Equipment, Library_Controls, MCP_Demo) y es solo esto:
//!   - Las cadenas van con su largo delante: `u16` little-endian + bytes en Windows-1252.
//!   - Un genie colocado es un objeto con la cadena `<librería>.<genie>.<ctm` (una por objeto en las 111 páginas),
//!     14 bytes de cabecera (el primer `u16` en 0 = genie sin propiedades) y bloques consecutivos `u16 tamaño` +
//!     (`u16 cantidad` + pares (nombre, valor) de cadenas): las propiedades del genie tal como se completaron al
//!     colocarlo (`Tag` = `Feeder1\XCBR1\Trip`). Ver `genie_properties`.
//!   - Las animaciones guardan las expresiones expandidas (`PLS_Bkr_Color_Bkr(12, zOL\Bus3, ...)`) como cadenas
//!     comunes.
//!
//! Lo que no se reconoce se cuenta en `unparsed` en vez de adivinarse.

use std::ops::Range;

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

fn u32_at(d: &[u8], o: usize) -> Option<usize> {
    d.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
}

/// Tamaño de una entrada del directorio del contenedor.
const DIR_ENTRY: usize = 0x2A;
/// Tamaño de una unidad de la zona de imports de una página.
const IMPORT_UNIT: usize = 0x60;

/// Payloads de los objetos de las páginas en uso del contenedor (ver la cabecera del módulo), en el orden del
/// directorio y de la cadena. `None` si algo no cumple la estructura: formato fuera de 7..9, entradas o páginas que
/// se salen del archivo, una cadena de objetos que se sale de su zona, vuelve atrás o no tiene la cantidad de la
/// cabecera.
fn objects(d: &[u8]) -> Option<Vec<Range<usize>>> {
    if !(7..=9).contains(&u16_at(d, 0)?) {
        return None;
    }
    let header = if u16_at(d, 2)? & 0x7fff >= 12 { 0x34 } else { 0x24 };
    let entries = u32_at(d, 12)?;
    let dir_end = entries.checked_mul(DIR_ENTRY)?.checked_add(0x10)?;
    if entries == 0 || dir_end > d.len() {
        return None;
    }
    let mut out = Vec::new();
    for e in (0..entries).map(|k| 0x10 + k * DIR_ENTRY) {
        if d[e] == 0 {
            continue; // libre o borrada (queda con el nombre "deleted" y sus bytes)
        }
        let off = u32_at(d, e + 0x22)?;
        let page_end = off.checked_add(u32_at(d, e + 0x26)?)?;
        if off < dir_end || page_end > d.len() || off + header > page_end {
            return None;
        }
        let (size, count, thumb) = (u32_at(d, off)?, u32_at(d, off + 4)?, u32_at(d, off + 8)?);
        let start = off.checked_add(header)?.checked_add(u32_at(d, off + 0x1C)?.checked_mul(IMPORT_UNIT)?)?;
        let end = start.checked_add(size)?;
        if end.checked_add(thumb)? > page_end {
            return None;
        }
        let (mut rel, mut walked) = (0, 0);
        while walked < count {
            let at = start + rel;
            if at + 8 > end {
                return None;
            }
            let payload_end = (at + 8).checked_add(u32_at(d, at)?)?;
            if payload_end > end {
                return None;
            }
            out.push(at + 8..payload_end);
            walked += 1;
            let next = u32_at(d, at + 4)?;
            if next == 0 {
                break;
            }
            if next <= rel {
                return None;
            }
            rel = next;
        }
        if walked != count {
            return None;
        }
    }
    Some(out)
}

/// Zonas del archivo a leer: los payloads de los objetos o, si no es un contenedor reconocible, el archivo entero.
fn regions(d: &[u8]) -> Vec<Range<usize>> {
    objects(d).unwrap_or_else(|| std::iter::once(0..d.len()).collect())
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

/// Cuánto puede pasarse del fin del objeto el tamaño del último bloque de propiedades. En dos páginas reales
/// (`PLS_Include\PLSSOE_HD1080.ctg`, `PSO_Equipment\!PM2K_TOU.ctg`) se pasa hasta 9 bytes aunque los pares terminan
/// dentro del objeto: se acotan los pares, no el bloque.
const BLOCK_SLACK: usize = 16;

/// Bloques consecutivos de propiedades desde `pos` (ver `genie_properties`).
fn blocks_from(d: &[u8], mut pos: usize) -> Option<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut blocks = 0;
    while blocks < 8 {
        let Some(size) = u16_at(d, pos) else { break };
        let limit = pos + 2 + size;
        if size < 2 || limit > d.len() + BLOCK_SLACK {
            break;
        }
        let limit = limit.min(d.len());
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
        return None; // import de la página (librería usada, con su fecha): solo si se lee el archivo entero
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
    for r in regions(d) {
        genies_in(&d[r.clone()], r.start, &mut out);
    }
    out
}

/// Genies de una zona `d` que empieza en `base` del archivo.
fn genies_in(d: &[u8], base: usize, out: &mut Genies) {
    let mut i = 0;
    while let Some(p) = d[i..].windows(CTM.len()).position(|w| w == CTM) {
        let end = i + p + CTM.len();
        i = end;
        let Some((start, text)) = genie_ref(d, end) else { continue };
        let name = &text[..text.len() - CTM.len()];
        let Some((library, genie)) = name.split_once('.') else { continue };
        match genie_properties(d, end) {
            Some(properties) => out.genies.push(GenieInstance { offset: base + start, library: library.to_string(), genie: genie.to_string(), properties }),
            None => out.unparsed.push(name.to_string()),
        }
    }
}

/// Tags `zOL\<Tipo><N>` que aparecen en las cadenas de la página (expresiones de animación expandidas):
/// (tag tal como figura, expresión que lo contiene). Sin repetir la misma pareja.
pub fn zol_refs(d: &[u8]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for r in regions(d) {
        zol_refs_in(&d[r], &mut out);
    }
    out
}

fn zol_refs_in(d: &[u8], out: &mut Vec<(String, String)>) {
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
}

/// Cadenas de la página que contienen `needle` (sin distinguir mayúsculas ASCII), sin repetir, hasta `max`.
pub fn strings_containing(d: &[u8], needle: &str, max: usize) -> Vec<String> {
    let n = crate::text::encode(needle);
    let mut out: Vec<String> = Vec::new();
    if n.is_empty() {
        return out;
    }
    for r in regions(d) {
        let d = &d[r];
        let mut i = 0;
        while out.len() < max {
            let Some(p) = d[i..].windows(n.len()).position(|w| w.eq_ignore_ascii_case(&n)) else { break };
            let at = i + p;
            i = at + n.len();
            let s = enclosing_string(d, at, i);
            if !out.contains(&s) {
                out.push(s);
            }
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

    /// Una página del contenedor.
    pub struct Page<'a> {
        pub name: &'a str,
        /// `false` = borrada: queda en el archivo, con sus bytes, pero no se lee.
        pub in_use: bool,
        /// Librerías que usa la página (`lib.genie.<ctm#fecha`), sin largo delante como en los archivos reales.
        pub imports: &'a [&'a str],
        /// Payload de cada objeto.
        pub objects: &'a [Vec<u8>],
    }

    /// Contenedor como los de PO 2024 (formato 9, versión 12, 8 entradas en el directorio; ver la cabecera del
    /// módulo). Cada página deja 20 bytes de hueco con basura al final del espacio reservado.
    pub fn container(pages: &[Page]) -> Vec<u8> {
        const ENTRIES: usize = 8;
        let mut out = Vec::new();
        out.extend_from_slice(&9u16.to_le_bytes());
        out.extend_from_slice(&12u16.to_le_bytes());
        out.extend_from_slice(&[0; 8]);
        out.extend_from_slice(&(ENTRIES as u32).to_le_bytes());
        out.resize(0x10 + ENTRIES * super::DIR_ENTRY, 0);
        for (k, p) in pages.iter().enumerate() {
            let mut imports = Vec::new();
            if !p.imports.is_empty() {
                let mut data = Vec::new();
                let mut offsets = Vec::new();
                for s in p.imports {
                    offsets.extend_from_slice(&(data.len() as u32).to_le_bytes());
                    data.extend_from_slice(&[0x5a; 16]); // GUID de la página importada
                    data.extend_from_slice(s.as_bytes());
                    data.push(0);
                }
                imports.extend_from_slice(b"    ");
                imports.extend_from_slice(&(p.imports.len() as u32).to_le_bytes());
                imports.extend_from_slice(&(data.len() as u32).to_le_bytes());
                imports.extend_from_slice(&offsets);
                imports.extend_from_slice(&data);
                imports.resize(imports.len().div_ceil(super::IMPORT_UNIT) * super::IMPORT_UNIT, 0);
            }
            let mut objs = Vec::new();
            for (i, o) in p.objects.iter().enumerate() {
                let next = if i + 1 == p.objects.len() { 0 } else { objs.len() + 8 + o.len() };
                objs.extend_from_slice(&(o.len() as u32).to_le_bytes());
                objs.extend_from_slice(&(next as u32).to_le_bytes());
                objs.extend_from_slice(o);
            }
            let thumb = [0x11u8; 6];
            let mut page = Vec::new();
            for v in [objs.len(), p.objects.len(), thumb.len(), 0, 0, 1920, 1080, imports.len() / super::IMPORT_UNIT, 0xdd67_fb98] {
                page.extend_from_slice(&(v as u32).to_le_bytes());
            }
            page.extend_from_slice(&[0x29; 16]); // GUID de la página
            page.extend_from_slice(&imports);
            page.extend_from_slice(&objs);
            page.extend_from_slice(&thumb);
            page.extend_from_slice(&[0xFF; 20]);
            let e = 0x10 + k * super::DIR_ENTRY;
            out[e] = p.in_use as u8;
            out[e + 1] = (k + 1 == pages.len()) as u8;
            out[e + 2..e + 2 + p.name.len()].copy_from_slice(p.name.as_bytes());
            let off = out.len() as u32;
            out[e + 0x22..e + 0x26].copy_from_slice(&off.to_le_bytes());
            out[e + 0x26..e + 0x2a].copy_from_slice(&(page.len() as u32).to_le_bytes());
            out.extend_from_slice(&page);
        }
        out
    }

    /// Página `.ctg`: `page` con los imports de siempre y un objeto por parte, más `attributes` vacía.
    pub fn page(parts: &[Vec<u8>]) -> Vec<u8> {
        container(&[
            Page { name: "page", in_use: true, imports: &["standard.normal@-XGA.<ctt#dd67fb9e", "pls_ansi_cb_1.Breaker_1.<ctm#dd67fb98"], objects: parts },
            Page { name: "attributes", in_use: true, imports: &[], objects: &[] },
        ])
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

        // Sin contenedor (se lee el archivo entero): una referencia con `#` detrás es un import, no un genie.
        let mut d = b"\x00\x00attributes\x00\x25\x00pls_ansi_cb_1.Breaker_1.<ctm#dd67fb98".to_vec();
        d.extend_from_slice(&[0; 12]);
        d.extend_from_slice(&fake::genie("pls_ansi_cb_1", "Breaker_1", &[("BkrNum", "7")]));
        let g = genies(&d);
        assert!(objects(&d).is_none());
        assert_eq!((g.genies.len(), g.genies[0].prop("BkrNum")), (1, Some("7")), "{g:?}");
        assert!(g.unparsed.is_empty());
    }

    #[test]
    fn contenedor() {
        let parts = [fake::animation("A"), fake::genie("lib", "gen", &[("Tag", "x")]), fake::animation("B")];
        let d = fake::page(&parts);
        let r = objects(&d).expect("contenedor válido");
        assert_eq!(r.len(), 3, "{r:?}");
        for (r, p) in r.iter().zip(&parts) {
            assert_eq!(&d[r.clone()], p.as_slice());
        }
        let g = genies(&d);
        assert_eq!(g.genies[0].offset, r[1].start + 6, "posición en el archivo, no en el objeto");

        // Una cadena que vuelve atrás o que no tiene la cantidad de la cabecera: se lee el archivo entero.
        let page = u32::from_le_bytes(d[0x10 + 0x22..0x10 + 0x26].try_into().unwrap()) as usize;
        let mut roto = d.clone();
        roto[page + 4] = 4; // 4 objetos según la cabecera, la cadena tiene 3
        assert!(objects(&roto).is_none());
        assert_eq!(genies(&roto).genies.len(), 1);
        let mut roto = d.clone();
        let second = r[1].start - 8;
        roto[second + 4..second + 8].fill(0); // la cadena termina en el 2.º objeto
        assert!(objects(&roto).is_none(), "la cadena termina antes de la cantidad de la cabecera");
        let mut roto = d;
        roto[0] = 10; // formato que Ctg32 no acepta
        assert!(objects(&roto).is_none());
    }

    #[test]
    fn paginas_borradas_e_imports_no_se_leen() {
        let vivo = [fake::genie("lib", "vivo", &[("Tag", "1")])];
        let borrado = [fake::genie("lib", "borrado", &[("Tag", "2")]), fake::animation("f(zOL\\Bkr9)")];
        let d = fake::container(&[
            fake::Page { name: "deleted", in_use: false, imports: &[], objects: &borrado },
            // Import sin `#` (como los de MCP_OL_A.ctg cuando Graphics Builder no encontró la librería).
            fake::Page { name: "vivo", in_use: true, imports: &["pls_easergy.t300_hu250.<ctm", "\\\\Login"], objects: &vivo },
        ]);
        let g = genies(&d);
        assert_eq!(g.genies.iter().map(|x| x.genie.as_str()).collect::<Vec<_>>(), ["vivo"], "{g:?}");
        assert!(g.unparsed.is_empty(), "{g:?}");
        assert!(zol_refs(&d).is_empty());
        assert!(strings_containing(&d, "t300", 10).is_empty());
    }

    /// Objeto real (PSO_Equipment/!PM2K_TOU.ctg, genie pso_general.pso_dev_time): el tamaño del bloque con las
    /// propiedades se pasa 9 bytes del fin del objeto, aunque los pares terminan dentro.
    #[test]
    fn bloque_que_se_pasa_del_objeto() {
        let obj = b"\x01\x00\x08\x00\x00\x00\x09\x02\x00\x00\x01\x00\x30\x00\x00\x6e\x00\x00\x00\x01\x00\x00\x00\x00\x00\x38\x80\x40\x00\x00\x00\x00\x00\xf0\x83\x40\x01\x00\x00\x00\x00\x00\xc0\x6a\x40\x00\x00\x00\x00\x00\x40\x70\x40\x01\x00\x00\x00\x00\x00\xc0\x6a\x40\x00\x00\x00\x00\x00\x40\x70\x40\x0c\x02\x79\x02\x0c\x02\x6a\x02\x38\x02\x7a\x02\x0c\x02\x6a\x02\x38\x02\x7a\x02\x1d\x00pso_general.pso_dev_time.<ctm\x01\x00\x02\x00\x00\x00\x0c\x00\x00\x00\x02\x00\x00\x00\x02\x00\x00\x00\x32\x00\x02\x00\x02\x00AN\x03\x00111\x08\x00dev time\x02\x0026\x02\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
        let d = fake::page(&[obj.to_vec(), fake::genie("lib", "gen", &[("AN", "5")])]);
        let g = genies(&d);
        assert!(g.unparsed.is_empty(), "{g:?}");
        assert_eq!(g.genies[0].properties, vec![("AN".into(), "111".into()), ("dev time".into(), "26".into())]);
        assert_eq!(g.genies[1].prop("AN"), Some("5"));
    }

    /// La búsqueda de bloques de `genie_properties` (hasta 600 bytes adelante) queda dentro del objeto: un genie
    /// con propiedades que no se entienden no se queda con las del objeto siguiente.
    #[test]
    fn propiedades_no_se_toman_del_objeto_siguiente() {
        let mut raro = b"\x0c\x00lib.gen.<ctm\x01\x00".to_vec();
        raro.extend_from_slice(&[0xFF; 20]);
        let siguiente = b"\x00\x00\x00\x00\x0b\x00\x01\x00\x02\x00AN\x03\x00111\x00\x00".to_vec();
        let d = fake::page(&[raro, siguiente]);
        let g = genies(&d);
        assert!(g.genies.is_empty(), "{g:?}");
        assert_eq!(g.unparsed, ["lib.gen"]);

        // Leyendo el archivo entero sí se las habría quedado.
        let mut entero = Genies::default();
        genies_in(&d, 0, &mut entero);
        assert_eq!(entero.genies.first().and_then(|x| x.prop("AN")), Some("111"), "{entero:?}");
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

    #[test]
    fn cadenas_con_un_texto() {
        let d = fake::page(&[
            fake::genie("pls_ansi_cb_1", "Breaker_1", &[("Equip", "EstA.Q1")]),
            fake::animation("EquipGetProperty(\"ESTA.Q1\", \"x\")"),
            fake::animation("EquipGetProperty(\"ESTA.Q1\", \"x\")"),
        ]);
        let s = strings_containing(&d, "esta.", 10);
        assert_eq!(s, ["EstA.Q1", "EquipGetProperty(\"ESTA.Q1\", \"x\")"]);
        assert_eq!(strings_containing(&d, "esta.", 1).len(), 1);
        assert!(strings_containing(&d, "", 10).is_empty());
    }
}
