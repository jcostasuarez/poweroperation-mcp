//! Índices .NDX (dBase III) de las tablas: lectura de la cabecera y reconstrucción completa a partir de los
//! registros vigentes de la tabla.
//!
//! Formato (verificado con los .NDX de MCP_Demo, ver `ndx_matches_mcp_demo`): bloques de 512 bytes. El bloque 0
//! es la cabecera: raíz (u32 @0, número de bloque), cantidad de bloques incluida la cabecera (u32 @4), longitud
//! de clave (u16 @12), claves por nodo (u16 @14), tipo (u16 @16, 0 = carácter), longitud de entrada (u16 @18 =
//! 8 + clave) y la expresión de clave en @24 (terminada en NUL). Un nodo empieza con su cantidad de claves n
//! (u32) y tiene entradas de 8 + clave bytes: hijo (u32), número de registro (u32) y clave. Las hojas tienen
//! hijo = 0 y listan todas las claves ordenadas por clave y, a igual clave, por registro. Los nodos internos
//! tienen n claves y n+1 hijos; la clave i es la máxima (clave y registro) del hijo i, como en Citect.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::dbf::{Dbf, write_atomic};
use crate::text::decode;

const BLOCK: usize = 512;

/// Cabecera de un .NDX: lo necesario para reconstruirlo.
#[derive(Debug, Clone)]
pub struct Header {
    /// Primer bloque original: se conserva tal cual salvo raíz y cantidad de bloques.
    raw: Vec<u8>,
    /// Longitud de la clave.
    pub key_len: usize,
    /// Claves por nodo.
    pub per_node: usize,
    /// Bytes por entrada (8 + longitud de clave).
    pub entry_len: usize,
    /// Expresión de clave (nombre de campo).
    pub expr: String,
}

pub fn read_header(data: &[u8]) -> Result<Header> {
    if data.len() < BLOCK {
        bail!("archivo .ndx demasiado corto");
    }
    let key_len = u16le(data, 12) as usize;
    let per_node = u16le(data, 14) as usize;
    let entry_len = u16le(data, 18) as usize;
    if key_len == 0 || per_node == 0 || entry_len != key_len + 8 {
        bail!("cabecera .ndx no reconocida (clave {key_len}, claves por nodo {per_node}, entrada {entry_len})");
    }
    let raw_expr = &data[24..256];
    let end = raw_expr.iter().position(|&b| b == 0).unwrap_or(raw_expr.len());
    let expr = decode(&raw_expr[..end]).trim().to_string();
    Ok(Header { raw: data[..BLOCK].to_vec(), key_len, per_node, entry_len, expr })
}

/// Índices (.ndx) de una tabla: los que se llaman como la tabla, sin distinguir mayúsculas.
pub fn indexes_for(dir: &Path, table: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("ndx"))
                && p.file_stem().is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case(table))
        })
        .collect();
    v.sort();
    v
}

/// Campo que indexa el .ndx. Solo se reconstruyen índices de un campo C o D (la clave son sus bytes).
pub fn key_field(h: &Header, dbf: &Dbf, file: &str) -> Result<usize> {
    let idx = dbf
        .field_index(&h.expr)
        .ok_or_else(|| anyhow!("{file}: la expresión de índice '{}' no es un campo de la tabla (solo se reconstruyen índices de un campo)", h.expr))?;
    match dbf.fields[idx].kind {
        'C' | 'D' => Ok(idx),
        k => bail!("{file}: índice sobre un campo {k} ('{}'): no soportado", h.expr),
    }
}

/// Árbol completo del índice (bytes del archivo) para estas claves y números de registro. Las claves se
/// ajustan a la longitud de la clave (relleno con espacios), se ordenan por bytes y, a igual clave, por registro.
pub fn build(h: &Header, keys: Vec<(Vec<u8>, u32)>) -> Vec<u8> {
    let kl = h.key_len;
    let es = h.entry_len;
    let mut keys: Vec<(Vec<u8>, u32)> = keys
        .into_iter()
        .map(|(k, r)| {
            let mut kk = vec![b' '; kl];
            let n = k.len().min(kl);
            kk[..n].copy_from_slice(&k[..n]);
            (kk, r)
        })
        .collect();
    keys.sort();

    // Un nodo interno no puede pasar de 504 bytes de punteros (el último hijo ocupa 4 bytes al final del bloque).
    let max_children = (h.per_node + 1).min(504 / es + 1).max(2);
    let mut nodes: Vec<Vec<u8>> = Vec::new();

    // Hojas: todas las claves, repartidas parejo.
    // Cada elemento del nivel: (número de bloque, máxima clave del subárbol).
    let mut level: Vec<(u32, Option<(Vec<u8>, u32)>)> = Vec::new();
    let mut start = 0usize;
    for size in split_even(keys.len(), h.per_node) {
        let part = &keys[start..start + size];
        let mut node = vec![0u8; BLOCK];
        node[0..4].copy_from_slice(&(size as u32).to_le_bytes());
        for (i, (k, rec)) in part.iter().enumerate() {
            let e = 4 + i * es;
            node[e + 4..e + 8].copy_from_slice(&rec.to_le_bytes());
            node[e + 8..e + 8 + kl].copy_from_slice(k);
        }
        nodes.push(node);
        level.push((nodes.len() as u32, part.last().cloned()));
        start += size;
    }

    // Niveles internos hasta que quede una sola raíz.
    while level.len() > 1 {
        let mut next = Vec::new();
        let mut start = 0usize;
        for size in split_even(level.len(), max_children) {
            let group = &level[start..start + size];
            let mut node = vec![0u8; BLOCK];
            node[0..4].copy_from_slice(&((size - 1) as u32).to_le_bytes());
            for (i, (blk, max)) in group.iter().enumerate() {
                let e = 4 + i * es;
                node[e..e + 4].copy_from_slice(&blk.to_le_bytes());
                if i + 1 < size
                    && let Some((k, rec)) = max
                {
                    node[e + 4..e + 8].copy_from_slice(&rec.to_le_bytes());
                    node[e + 8..e + 8 + kl].copy_from_slice(k);
                }
            }
            nodes.push(node);
            next.push((nodes.len() as u32, group.last().and_then(|g| g.1.clone())));
            start += size;
        }
        level = next;
    }

    let root = level[0].0;
    let mut out = h.raw.clone();
    out[0..4].copy_from_slice(&root.to_le_bytes());
    out[4..8].copy_from_slice(&((nodes.len() + 1) as u32).to_le_bytes());
    for n in nodes {
        out.extend_from_slice(&n);
    }
    out
}

/// Reconstruye el .ndx `path` (existente, para conservar su cabecera) con los registros de `dbf`, incluidos los
/// borrados (dBase los deja en el índice). Escritura atómica. Devuelve la cantidad de claves.
pub fn rebuild(dbf: &Dbf, path: &Path) -> Result<usize> {
    let file = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let old = std::fs::read(path).with_context(|| format!("No se pudo leer {}", path.display()))?;
    let h = read_header(&old).with_context(|| file.clone())?;
    let field = key_field(&h, dbf, &file)?;
    let keys: Vec<(Vec<u8>, u32)> = dbf
        .all_raw_records()
        .into_iter()
        .map(|(rec, raw)| (dbf.field_bytes(raw, field).to_vec(), rec as u32))
        .collect();
    let n = keys.len();
    let bytes = build(&h, keys);
    write_atomic(path, &bytes)?;
    Ok(n)
}

/// Reparte `n` elementos en grupos de hasta `cap` (parejos). Siempre devuelve al menos un grupo.
fn split_even(n: usize, cap: usize) -> Vec<usize> {
    if n <= cap {
        return vec![n];
    }
    let k = n.div_ceil(cap);
    let (base, extra) = (n / k, n % k);
    (0..k).map(|i| base + usize::from(i < extra)).collect()
}

fn u16le(d: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([d[at], d[at + 1]])
}

#[cfg(test)]
fn u32le(d: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]])
}

/// Claves en orden de hojas: (clave, registro). Recorre el árbol desde la raíz (solo para pruebas).
#[cfg(test)]
pub fn read_keys(data: &[u8]) -> Result<Vec<(Vec<u8>, u32)>> {
    fn walk(data: &[u8], h: &Header, blk: usize, depth: usize, out: &mut Vec<(Vec<u8>, u32)>) -> Result<()> {
        if depth > 8 || blk == 0 || (blk + 1) * BLOCK > data.len() {
            bail!("nodo {blk} fuera de rango");
        }
        let o = blk * BLOCK;
        let n = u32le(data, o) as usize;
        if n > h.per_node {
            bail!("nodo {blk} con {n} claves (máximo {})", h.per_node);
        }
        if u32le(data, o + 4) == 0 {
            for i in 0..n {
                let e = o + 4 + i * h.entry_len;
                out.push((data[e + 8..e + 8 + h.key_len].to_vec(), u32le(data, e + 4)));
            }
        } else {
            for i in 0..=n {
                let child = u32le(data, o + 4 + i * h.entry_len) as usize;
                walk(data, h, child, depth + 1, out)?;
            }
        }
        Ok(())
    }
    let h = read_header(data)?;
    let mut out = Vec::new();
    walk(data, &h, u32le(data, 0) as usize, 0, &mut out)?;
    Ok(out)
}

/// Cabecera de prueba: raíz 1, 2 bloques, clave `key_len`, `per_node` claves por nodo.
#[cfg(test)]
pub fn test_header(expr: &str, key_len: usize, per_node: usize) -> Vec<u8> {
    let mut h = vec![0u8; BLOCK];
    h[0..4].copy_from_slice(&1u32.to_le_bytes());
    h[4..8].copy_from_slice(&2u32.to_le_bytes());
    h[12..14].copy_from_slice(&(key_len as u16).to_le_bytes());
    h[14..16].copy_from_slice(&(per_node as u16).to_le_bytes());
    h[18..20].copy_from_slice(&((key_len + 8) as u16).to_le_bytes());
    h[24..24 + expr.len()].copy_from_slice(expr.as_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbf::tests::build_typed;
    use crate::testutil::TempDir;

    fn keys(words: &[&str]) -> Vec<(Vec<u8>, u32)> {
        words.iter().enumerate().map(|(i, w)| (w.as_bytes().to_vec(), i as u32 + 1)).collect()
    }

    #[test]
    fn split_even_balances() {
        assert_eq!(split_even(0, 5), vec![0]);
        assert_eq!(split_even(5, 5), vec![5]);
        assert_eq!(split_even(7, 5), vec![4, 3]);
        let parts = split_even(48, 7);
        assert_eq!(parts, vec![7, 7, 7, 7, 7, 7, 6]);
        assert!(parts.iter().max().unwrap() - parts.iter().min().unwrap() <= 1);
        assert!(split_even(13, 4).iter().all(|&s| s >= 3));
    }

    #[test]
    fn empty_index_has_one_empty_leaf() {
        let h = read_header(&test_header("NAME", 10, 4)).unwrap();
        let out = build(&h, vec![]);
        assert_eq!(out.len(), 2 * BLOCK);
        assert_eq!(u32le(&out, 0), 1);
        assert_eq!(u32le(&out, 4), 2);
        assert!(read_keys(&out).unwrap().is_empty());
        assert_eq!(read_header(&out).unwrap().expr, "NAME");
    }

    #[test]
    fn roundtrip_keeps_sorted_order_and_duplicates() {
        for n in [1usize, 4, 5, 6, 7, 21, 48, 200, 1000] {
            let mut words: Vec<String> = (0..n).map(|i| format!("Feeder{}\\MMXU{}\\TotW", i % 7, (i * 13) % 50)).collect();
            words.push("Dup".into());
            words.push("Dup".into());
            let input = keys(&words.iter().map(|s| s.as_str()).collect::<Vec<_>>());
            let h = read_header(&test_header("NAME", 40, 5)).unwrap();
            let out = build(&h, input.clone());
            let got = read_keys(&out).unwrap();
            let mut want: Vec<(Vec<u8>, u32)> = input
                .into_iter()
                .map(|(k, r)| {
                    let mut kk = vec![b' '; 40];
                    kk[..k.len()].copy_from_slice(&k);
                    (kk, r)
                })
                .collect();
            want.sort();
            assert_eq!(got, want, "n={n}");
            assert!(out.len() % BLOCK == 0);
            assert_eq!(u32le(&out, 4) as usize, out.len() / BLOCK, "bloques n={n}");
        }
    }

    #[test]
    fn rebuild_from_dbf_includes_deleted_records() {
        let data = build_typed(
            &[("NAME", 'C', 10, 0), ("VAL", 'N', 4, 0)],
            &[(&["bravo", "1"], false), (&["borrado", "2"], true), (&["alfa", "3"], false)],
        );
        let dbf = Dbf::parse(data).unwrap();
        let tmp = TempDir::new("ndx-rebuild");
        let path = tmp.join("alfa.ndx");
        std::fs::write(&path, test_header("NAME", 10, 4)).unwrap();
        assert_eq!(rebuild(&dbf, &path).unwrap(), 3);
        let got = read_keys(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].1, 3); // "alfa" (registro 3)
        assert_eq!(got[1].1, 2); // "borrado" (registro 2, borrado pero indexado)
        assert_eq!(got[2].1, 1); // "bravo" (registro 1)
    }

    #[test]
    fn rejects_bad_headers_and_numeric_keys() {
        assert!(read_header(&[0u8; 100]).is_err());
        let mut bad = test_header("NAME", 10, 4);
        bad[18] = 9; // entrada no coincide con la clave
        assert!(read_header(&bad).is_err());
        let data = build_typed(&[("VAL", 'N', 4, 0)], &[]);
        let dbf = Dbf::parse(data).unwrap();
        let h = read_header(&test_header("VAL", 4, 4)).unwrap();
        let e = key_field(&h, &dbf, "v.ndx").unwrap_err().to_string();
        assert!(e.contains("no soportado"), "{e}");
        let h2 = read_header(&test_header("NOEXISTE", 4, 4)).unwrap();
        let e2 = key_field(&h2, &dbf, "v.ndx").unwrap_err().to_string();
        assert!(e2.contains("no es un campo"), "{e2}");
    }

    /// Compara los .NDX reales de MCP_Demo con la reconstrucción desde sus .dbf (copiados a una carpeta temporal).
    /// Solo lectura del proyecto. Correr a mano: `cargo test -- --ignored ndx_matches_mcp_demo --nocapture`.
    #[test]
    #[ignore = "lee MCP_Demo en ProgramData; correr a mano con --ignored"]
    fn ndx_matches_mcp_demo() {
        let src = std::path::PathBuf::from(r"C:\ProgramData\Schneider Electric\Power Operation\v2024\User\MCP_Demo");
        let tmp = TempDir::new("ndx");
        let tables = ["variable", "units", "computer", "pgbutton", "pgnum", "pgansym", "pgkey", "pgdynobj", "English", "PGLANG", "locvar"];
        let mut checked = 0;
        for t in tables {
            let dbf_src = match crate::project::table_path(&src, t) {
                Some(p) => p,
                None => continue,
            };
            let dbf_dst = tmp.join(dbf_src.file_name().unwrap());
            std::fs::copy(&dbf_src, &dbf_dst).unwrap();
            for ndx_src in indexes_for(&src, t) {
                let ndx_dst = tmp.join(ndx_src.file_name().unwrap());
                std::fs::copy(&ndx_src, &ndx_dst).unwrap();
                let orig = std::fs::read(&ndx_src).unwrap();
                let orig_keys = read_keys(&orig).unwrap();
                let dbf = Dbf::open(&dbf_dst).unwrap();
                let n = rebuild(&dbf, &ndx_dst).unwrap();
                let rebuilt = std::fs::read(&ndx_dst).unwrap();
                let got = read_keys(&rebuilt).unwrap();
                let same_keys = got == orig_keys;
                let same_bytes = rebuilt == orig;
                println!(
                    "{}: claves={} (dbf vivos={}) mismas claves/recnos={} bytes_iguales={} tamaño {} vs {}",
                    ndx_src.file_name().unwrap().to_string_lossy(),
                    orig_keys.len(),
                    n,
                    same_keys,
                    same_bytes,
                    orig.len(),
                    rebuilt.len()
                );
                assert!(same_keys, "{t}: las claves o los registros no coinciden con el original");
                checked += 1;
            }
        }
        assert!(checked > 0, "no se encontró ningún .ndx");
    }
}
