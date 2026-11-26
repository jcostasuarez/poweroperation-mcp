//! Tablas dBase (.dbf), el formato de la base de configuración de proyectos Citect / Plant SCADA /
//! Power Operation (variable.dbf, digalm.dbf, units.dbf, ...). Lectura y escritura de registros.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use chrono::Datelike;

use crate::text::decode;

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub kind: char,
    pub len: usize,
    /// Decimales (solo campos N).
    pub dec: usize,
    offset: usize,
}

pub struct Dbf {
    pub fields: Vec<Field>,
    data: Vec<u8>,
    header_len: usize,
    record_len: usize,
    /// Registros declarados en la cabecera (incluye los marcados como borrados).
    pub declared: usize,
}

impl Dbf {
    pub fn open(path: &Path) -> Result<Self> {
        let data = std::fs::read(path).with_context(|| format!("No se pudo leer {}", path.display()))?;
        Self::parse(data).with_context(|| format!("{} no es un DBF válido", path.display()))
    }

    pub fn parse(data: Vec<u8>) -> Result<Self> {
        if data.len() < 32 {
            bail!("archivo demasiado corto");
        }
        let declared = u32::from_le_bytes(data[4..8].try_into()?) as usize;
        let header_len = u16::from_le_bytes([data[8], data[9]]) as usize;
        let record_len = u16::from_le_bytes([data[10], data[11]]) as usize;
        if header_len < 33 || record_len == 0 || header_len > data.len() {
            bail!("cabecera inválida (header={header_len}, record={record_len})");
        }
        let mut fields = Vec::new();
        let mut pos = 32;
        let mut offset = 1; // byte 0 de cada registro = marca de borrado
        while pos + 32 <= header_len && data[pos] != 0x0D {
            let d = &data[pos..pos + 32];
            let name_end = d[..11].iter().position(|&b| b == 0).unwrap_or(11);
            let name = String::from_utf8_lossy(&d[..name_end]).trim().to_string();
            let kind = d[11] as char;
            let len = if kind == 'C' { d[16] as usize | ((d[17] as usize) << 8) } else { d[16] as usize };
            let dec = if kind == 'N' { d[17] as usize } else { 0 };
            fields.push(Field { name, kind, len, dec, offset });
            offset += len;
            pos += 32;
        }
        if offset > record_len {
            bail!("los campos ({offset} bytes) no entran en el registro ({record_len} bytes)");
        }
        Ok(Self { fields, data, header_len, record_len, declared })
    }

    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name.eq_ignore_ascii_case(name))
    }

    /// Nombres de los campos, para los mensajes de error.
    pub fn field_names(&self) -> String {
        self.fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", ")
    }

    /// Recorre los registros vigentes: `f(número_de_registro, valores)`; devolver `false` corta.
    pub fn for_each(&self, mut f: impl FnMut(usize, Vec<String>) -> bool) {
        for i in 0..self.declared {
            let start = self.header_len + i * self.record_len;
            let Some(rec) = self.data.get(start..start + self.record_len) else { break };
            match rec[0] {
                b'*' => continue,
                0x1A => break,
                _ => {}
            }
            let values = self.fields.iter().map(|fd| value(fd, &rec[fd.offset..fd.offset + fd.len])).collect();
            if !f(i + 1, values) {
                break;
            }
        }
    }

    pub fn count(&self) -> usize {
        let mut n = 0;
        self.for_each(|_, _| {
            n += 1;
            true
        });
        n
    }

    /// Todos los registros declarados, incluidos los borrados: así los guarda dBase en el .ndx (verificado con
    /// pgbutton.dbf de MCP_Demo: 48 registros, 40 borrados, 48 claves). (número desde 1, registro).
    pub fn all_raw_records(&self) -> Vec<(usize, &[u8])> {
        (0..self.declared)
            .map_while(|i| {
                let start = self.header_len + i * self.record_len;
                self.data.get(start..start + self.record_len).map(|r| (i + 1, r))
            })
            .collect()
    }

    /// Bytes crudos de un campo dentro de un registro de `raw_records`.
    pub fn field_bytes<'a>(&self, rec: &'a [u8], idx: usize) -> &'a [u8] {
        let f = &self.fields[idx];
        &rec[f.offset..f.offset + f.len]
    }

    /// Codifica valores por nombre de campo (sin distinguir mayúsculas) en celdas de ancho exacto.
    /// Campos desconocidos o repetidos son error; los valores vacíos quedan en blanco.
    pub fn encode_values(&self, values: &[(String, String)]) -> Result<Vec<(usize, Vec<u8>)>> {
        let mut out: Vec<(usize, Vec<u8>)> = Vec::with_capacity(values.len());
        for (name, value) in values {
            let idx = self
                .field_index(name)
                .ok_or_else(|| anyhow!("Campo desconocido '{name}'. Campos de la tabla: {}", self.field_names()))?;
            if out.iter().any(|(i, _)| *i == idx) {
                bail!("El campo {} aparece dos veces en la misma fila", self.fields[idx].name);
            }
            out.push((idx, encode_cell(&self.fields[idx], value)?));
        }
        Ok(out)
    }

    /// Bytes del archivo con cambios. `updates`: (registro desde 1, celdas) que reemplazan campos de
    /// registros existentes; `appends`: registros nuevos (los campos no indicados quedan en blanco).
    /// Los demás registros, incluidos los borrados, se copian tal cual. Actualiza fecha y conteo.
    pub fn rewrite(&self, updates: &[(usize, Vec<(usize, Vec<u8>)>)], appends: &[Vec<(usize, Vec<u8>)>]) -> Result<Vec<u8>> {
        let rl = self.record_len;
        let mut recs: Vec<Vec<u8>> = Vec::with_capacity(self.declared + appends.len());
        for i in 0..self.declared {
            let start = self.header_len + i * rl;
            let rec = self.data.get(start..start + rl).ok_or_else(|| anyhow!("el archivo está truncado: faltan registros"))?;
            recs.push(rec.to_vec());
        }
        for (n, cells) in updates {
            let rec = n
                .checked_sub(1)
                .and_then(|i| recs.get_mut(i))
                .ok_or_else(|| anyhow!("registro {n} fuera de rango (hay {})", self.declared))?;
            self.put(rec, cells)?;
        }
        for cells in appends {
            let mut rec = vec![b' '; rl];
            self.put(&mut rec, cells)?;
            recs.push(rec);
        }
        let mut out = self.data[..self.header_len].to_vec();
        let now = chrono::Local::now();
        out[1] = (now.year() - 1900).clamp(0, 255) as u8;
        out[2] = now.month() as u8;
        out[3] = now.day() as u8;
        out[4..8].copy_from_slice(&(recs.len() as u32).to_le_bytes());
        for r in &recs {
            out.extend_from_slice(r);
        }
        out.push(0x1A);
        Ok(out)
    }

    /// Bytes del archivo con los registros `recs` (desde 1) marcados como borrados (`*`, como hace dBase): siguen
    /// ocupando su lugar y su clave en los .ndx. Actualiza la fecha.
    pub fn mark_deleted(&self, recs: &[usize]) -> Result<Vec<u8>> {
        let mut out = self.rewrite(&[], &[])?;
        for &n in recs {
            if n == 0 || n > self.declared {
                bail!("registro {n} fuera de rango (hay {})", self.declared);
            }
            out[self.header_len + (n - 1) * self.record_len] = b'*';
        }
        Ok(out)
    }

    fn put(&self, rec: &mut [u8], cells: &[(usize, Vec<u8>)]) -> Result<()> {
        for (idx, bytes) in cells {
            let f = &self.fields[*idx];
            if bytes.len() != f.len {
                bail!("{}: celda de {} bytes para un campo de {}", f.name, bytes.len(), f.len);
            }
            rec[f.offset..f.offset + f.len].copy_from_slice(bytes);
        }
        Ok(())
    }
}

/// Texto → bytes de un campo (ancho exacto, relleno con espacios). Tipos: C, N, L, D. M (memo) se rechaza.
pub fn encode_cell(fd: &Field, input: &str) -> Result<Vec<u8>> {
    let too_long = |n: usize| anyhow!("{}: '{input}' ocupa {n} caracteres y el campo admite {} como máximo", fd.name, fd.len);
    let mut out = match fd.kind {
        'C' => {
            let (bytes, _, unmappable) = encoding_rs::WINDOWS_1252.encode(input);
            if unmappable {
                bail!("{}: el texto tiene caracteres que no existen en ANSI (Windows-1252)", fd.name);
            }
            if bytes.len() > fd.len {
                return Err(too_long(bytes.len()));
            }
            bytes.into_owned()
        }
        'N' => {
            let t = input.trim();
            if t.is_empty() {
                Vec::new()
            } else {
                let v: f64 = t.parse().map_err(|_| anyhow!("{}: '{input}' no es un número", fd.name))?;
                if !v.is_finite() {
                    bail!("{}: '{input}' no es un número finito", fd.name);
                }
                let s = format!("{:.*}", fd.dec, v);
                let back: f64 = s.parse()?;
                if (back - v).abs() > 1e-9 * (1.0 + v.abs()) {
                    bail!("{}: '{input}' tiene más decimales que los {} que admite el campo", fd.name, fd.dec);
                }
                if s.len() > fd.len {
                    return Err(too_long(s.len()));
                }
                s.into_bytes()
            }
        }
        'L' => match input.trim().to_ascii_uppercase().as_str() {
            "" => vec![b' '],
            "T" | "TRUE" | "Y" | "YES" | "S" | "SI" | "1" => vec![b'T'],
            "F" | "FALSE" | "N" | "NO" | "0" => vec![b'F'],
            _ => bail!("{}: '{input}' no es lógico (usá true o false)", fd.name),
        },
        'D' => {
            let digits: String = input.trim().chars().filter(|c| *c != '-').collect();
            if digits.is_empty() {
                Vec::new()
            } else {
                if digits.len() != 8 || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    bail!("{}: '{input}' no es una fecha (usá AAAA-MM-DD)", fd.name);
                }
                chrono::NaiveDate::parse_from_str(&digits, "%Y%m%d")
                    .map_err(|_| anyhow!("{}: '{input}' no es una fecha válida (usá AAAA-MM-DD)", fd.name))?;
                digits.into_bytes()
            }
        }
        'M' => bail!("{}: los campos memo (M) no se escriben desde el MCP", fd.name),
        other => bail!("{}: tipo de campo '{other}' no soportado (solo C, N, L y D)", fd.name),
    };
    if out.len() > fd.len {
        return Err(too_long(out.len()));
    }
    let pad = fd.len - out.len();
    if fd.kind == 'N' {
        out.splice(0..0, std::iter::repeat_n(b' ', pad));
    } else {
        out.extend(std::iter::repeat_n(b' ', pad));
    }
    Ok(out)
}

/// Campo de contraseña (PASSWORD, PWD..., SEED de `users.dbf`): el MCP no los escribe ni muestra su valor.
pub fn is_secret_field(name: &str) -> bool {
    let u = name.to_ascii_uppercase();
    ["PASS", "PWD", "SEED"].iter().any(|s| u.contains(s))
}

/// Lo que se muestra en lugar del valor de un campo de contraseña.
pub const MASKED: &str = "***";

/// Escribe `bytes` en `path` a través de `path.tmp` y lo reemplaza: nunca deja el archivo a medias.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = path.with_file_name(format!("{name}.tmp"));
    std::fs::write(&tmp, bytes).with_context(|| format!("No se pudo escribir {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("No se pudo reemplazar {}", path.display()));
    }
    Ok(())
}

fn value(fd: &Field, raw: &[u8]) -> String {
    match fd.kind {
        'L' => match raw.first() {
            Some(b'T' | b't' | b'Y' | b'y') => "TRUE".into(),
            Some(b'F' | b'f' | b'N' | b'n') => "FALSE".into(),
            _ => String::new(),
        },
        'D' => {
            let s = decode(raw);
            let s = s.trim();
            if s.len() == 8 { format!("{}-{}-{}", &s[..4], &s[4..6], &s[6..]) } else { s.to_string() }
        }
        _ => decode(raw).trim().to_string(),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// Construye un DBF mínimo de campos C (para pruebas).
    pub fn build(fields: &[(&str, u8)], rows: &[(&[&str], bool)]) -> Vec<u8> {
        let typed: Vec<(&str, char, u8, u8)> = fields.iter().map(|&(name, len)| (name, 'C', len, 0)).collect();
        build_typed(&typed, rows)
    }

    /// DBF con campos de cualquier tipo: (nombre, tipo, largo, decimales). Los valores van como texto
    /// (los N se alinean a la derecha). `true` en la segunda parte marca el registro como borrado.
    pub fn build_typed(fields: &[(&str, char, u8, u8)], rows: &[(&[&str], bool)]) -> Vec<u8> {
        let header_len = 32 + 32 * fields.len() + 1;
        let record_len = 1 + fields.iter().map(|f| f.2 as usize).sum::<usize>();
        let mut d = vec![0u8; 32];
        d[0] = 0x03;
        d[4..8].copy_from_slice(&(rows.len() as u32).to_le_bytes());
        d[8..10].copy_from_slice(&(header_len as u16).to_le_bytes());
        d[10..12].copy_from_slice(&(record_len as u16).to_le_bytes());
        for (name, kind, len, dec) in fields {
            let mut fd = [0u8; 32];
            fd[..name.len()].copy_from_slice(name.as_bytes());
            fd[11] = *kind as u8;
            fd[16] = *len;
            fd[17] = *dec;
            d.extend_from_slice(&fd);
        }
        d.push(0x0D);
        for (vals, deleted) in rows {
            d.push(if *deleted { b'*' } else { b' ' });
            for ((_, kind, len, _), v) in fields.iter().zip(vals.iter()) {
                let mut cell = crate::text::encode(v);
                cell.resize(*len as usize, b' ');
                if *kind == 'N' {
                    let s = String::from_utf8_lossy(&cell).trim().to_string();
                    cell = format!("{s:>w$}", w = *len as usize).into_bytes();
                }
                d.extend_from_slice(&cell);
            }
        }
        d.push(0x1A);
        d
    }

    #[test]
    fn parse_records() {
        let data = build(
            &[("TAG", 20), ("UNIT", 10), ("COMMENT", 30)],
            &[
                (&["Feeder1\\MMXU1\\TotW", "IODev1", "Potencia activa"], false),
                (&["Borrado", "IODev1", ""], true),
                (&["Feeder1\\XCBR1\\Pos", "IODev1", "Posición interruptor"], false),
            ],
        );
        let dbf = Dbf::parse(data).unwrap();
        assert_eq!(dbf.fields.len(), 3);
        assert_eq!(dbf.field_index("unit"), Some(1));
        let mut rows = vec![];
        dbf.for_each(|n, v| {
            rows.push((n, v));
            true
        });
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1[0], "Feeder1\\MMXU1\\TotW");
        assert_eq!(rows[1].0, 3);
        assert_eq!(rows[1].1[2], "Posición interruptor");
    }

    fn typed() -> Dbf {
        let data = build_typed(
            &[("NAME", 'C', 8, 0), ("VAL", 'N', 7, 2), ("ON", 'L', 1, 0), ("DAY", 'D', 8, 0), ("MEMO", 'M', 10, 0)],
            &[(&["A", "1.5", "T", "20260101", ""], false), (&["B", "2", "F", "", ""], true)],
        );
        Dbf::parse(data).unwrap()
    }

    #[test]
    fn encode_by_type() {
        let d = typed();
        let c = |name: &str, v: &str| encode_cell(&d.fields[d.field_index(name).unwrap()], v);
        assert_eq!(c("NAME", "ABCDEFGH").unwrap(), b"ABCDEFGH");
        assert_eq!(c("NAME", "ABCD").unwrap(), b"ABCD    ");
        let e = c("NAME", "ABCDEFGHI").unwrap_err().to_string();
        assert!(e.contains("admite 8"), "{e}");
        assert_eq!(c("VAL", "12.5").unwrap(), b"  12.50");
        assert!(c("VAL", "1.234").unwrap_err().to_string().contains("decimales"));
        assert!(c("VAL", "abc").is_err());
        assert_eq!(c("VAL", "").unwrap(), b"       ");
        assert_eq!(c("ON", "true").unwrap(), b"T");
        assert_eq!(c("ON", "F").unwrap(), b"F");
        assert!(c("ON", "tal vez").is_err());
        assert_eq!(c("DAY", "2026-10-14").unwrap(), b"20261014");
        assert!(c("DAY", "2026-13-01").is_err());
        assert!(c("DAY", "2026").is_err());
        assert!(c("MEMO", "x").unwrap_err().to_string().contains("memo"));
        assert!(c("NAME", "😀").is_err());
    }

    #[test]
    fn encode_values_unknown_field_lists_valid() {
        let d = typed();
        let e = d.encode_values(&[("NOPE".into(), "x".into())]).unwrap_err().to_string();
        assert!(e.contains("NOPE") && e.contains("NAME, VAL, ON, DAY, MEMO"), "{e}");
        let dup = d.encode_values(&[("name".into(), "a".into()), ("NAME".into(), "b".into())]).unwrap_err().to_string();
        assert!(dup.contains("El campo NAME aparece dos veces"), "{dup}");
    }

    #[test]
    fn rewrite_updates_and_appends() {
        let d = typed();
        let name = d.field_index("NAME").unwrap();
        let val = d.field_index("VAL").unwrap();
        let upd = vec![(1usize, vec![(name, b"NUEVO   ".to_vec())])];
        let app = vec![vec![(name, b"C       ".to_vec()), (val, b"   3.00".to_vec())]];
        let out = d.rewrite(&upd, &app).unwrap();
        assert_eq!(*out.last().unwrap(), 0x1A);
        let n = Dbf::parse(out).unwrap();
        assert_eq!(n.declared, 3);
        let mut rows = vec![];
        n.for_each(|r, v| {
            rows.push((r, v));
            true
        });
        // El registro 2 estaba borrado: sigue borrado y no aparece; el 3 es el nuevo.
        assert_eq!(rows.iter().map(|r| r.0).collect::<Vec<_>>(), vec![1, 3]);
        assert_eq!(rows[0].1[0], "NUEVO");
        assert_eq!(rows[1].1[0], "C");
        assert_eq!(rows[1].1[1], "3.00");
        assert!(rows[1].1[2].is_empty());
        // Registros fuera de rango (0 o más que los declarados) son error.
        let e9 = d.rewrite(&[(9usize, vec![])], &[]).unwrap_err().to_string();
        assert!(e9.contains("registro 9 fuera de rango"), "{e9}");
        let e0 = d.rewrite(&[(0usize, vec![])], &[]).unwrap_err().to_string();
        assert!(e0.contains("registro 0 fuera de rango"), "{e0}");
    }

    #[test]
    fn write_atomic_replaces() {
        let tmp = TempDir::new("atomic");
        let p = tmp.join("t.dbf");
        std::fs::write(&p, b"viejo").unwrap();
        write_atomic(&p, b"nuevo").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"nuevo");
        assert!(!tmp.join("t.dbf.tmp").exists());
    }
}
