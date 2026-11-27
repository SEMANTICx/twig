//! Clean-room structural inspection for binary artifacts (ELF, Wasm, PE, Mach-O,
//! ZIP/JAR, SQLite), paginated 16-byte Hexdump slices, and HTTP Archive (`.har`)
//! API contract distillation. Zero external crate dependencies; strictly read-only.

use serde_json::Value;

/// Render either a paginated 16-byte/row canonical hexdump (when `start_line` is
/// explicitly supplied) or a rich structural header + strings summary for binary bytes.
pub(super) fn format_binary(
    bytes: &[u8],
    start_line: Option<usize>,
    limit: Option<usize>,
    read_max_lines: usize,
) -> String {
    let kind = detect_binary_kind(bytes);
    let digest = blake3::hash(bytes);
    let short_hash = &digest.to_hex()[..16];
    let header = format!(
        "[binary file: {kind} · {} bytes · blake3:{short_hash} — raw binary text dump skipped]",
        bytes.len()
    );

    if start_line.is_some() {
        let dump = format_hexdump(bytes, start_line, limit, read_max_lines);
        return format!("{header}\n{dump}");
    }

    let mut details = Vec::new();
    if bytes.starts_with(b"\x7fELF") {
        details.extend(inspect_elf(bytes));
    } else if bytes.starts_with(b"\0asm") {
        details.extend(inspect_wasm(bytes));
    } else if bytes.starts_with(b"PK\x03\x04") {
        details.extend(inspect_zip(bytes));
    } else if bytes.starts_with(b"SQLite format 3\0") {
        details.extend(inspect_sqlite(bytes));
    }

    let strings = extract_printable_strings(bytes, 5, 16);
    if !strings.is_empty() {
        details.push(format!("notable_strings: {}", strings.join(" | ")));
    }
    details.push(
        "[tip: pass start_line=1, limit=32 to inspect 16-byte/row hexdump slices]".into(),
    );
    format!("{header}\n{}", details.join("\n"))
}

fn detect_binary_kind(bytes: &[u8]) -> String {
    if bytes.starts_with(b"\x7fELF") && bytes.len() >= 20 {
        let bits = match bytes[4] {
            1 => "32-bit",
            2 => "64-bit",
            _ => "unknown-class",
        };
        let endian = match bytes[5] {
            1 => "LSB",
            2 => "MSB",
            _ => "unknown-endian",
        };
        let le = bytes[5] != 2;
        let u16_at = |off: usize| -> u16 {
            if le {
                u16::from_le_bytes([bytes[off], bytes[off + 1]])
            } else {
                u16::from_be_bytes([bytes[off], bytes[off + 1]])
            }
        };
        let etype = match u16_at(16) {
            1 => "relocatable",
            2 => "executable",
            3 => "shared/PIE",
            4 => "core",
            _ => "object",
        };
        let arch = match u16_at(18) {
            0x3E => "x86_64",
            0xB7 => "aarch64",
            0xF3 => "riscv",
            0x03 => "x86",
            0x28 => "arm",
            _ => "other-arch",
        };
        format!("ELF {bits} {endian} {etype} ({arch})")
    } else if bytes.starts_with(b"\xfe\xed\xfa\xcf")
        || bytes.starts_with(b"\xcf\xfa\xed\xfe")
        || bytes.starts_with(b"\xfe\xed\xfa\xce")
        || bytes.starts_with(b"\xce\xfa\xed\xfe")
        || bytes.starts_with(b"\xca\xfe\xba\xbe")
    {
        "Mach-O binary".into()
    } else if bytes.starts_with(b"MZ") {
        "PE/COFF executable/library".into()
    } else if bytes.starts_with(b"\0asm") {
        "WebAssembly module (.wasm)".into()
    } else if bytes.starts_with(b"SQLite format 3\0") {
        "SQLite 3 database".into()
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "PNG image".into()
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "JPEG image".into()
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "GIF image".into()
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "WebP image".into()
    } else if bytes.starts_with(b"%PDF-") {
        "PDF document".into()
    } else if bytes.starts_with(b"PK\x03\x04") {
        "ZIP/JAR archive".into()
    } else if bytes.starts_with(b"\x1f\x8b") {
        "GZIP compressed archive".into()
    } else {
        "non-UTF8 binary data".into()
    }
}

fn format_hexdump(
    bytes: &[u8],
    start_line: Option<usize>,
    limit: Option<usize>,
    read_max_lines: usize,
) -> String {
    let total_rows = bytes.len().div_ceil(16);
    if total_rows == 0 {
        return "[empty file]".into();
    }
    let skip_rows = start_line.unwrap_or(1).saturating_sub(1);
    let take_rows = limit.unwrap_or(64).clamp(1, read_max_lines);
    if skip_rows >= total_rows {
        return format!("[start_line={} beyond {total_rows} hex rows]", skip_rows + 1);
    }
    let end_row = (skip_rows + take_rows).min(total_rows);
    let mut lines = Vec::with_capacity(end_row - skip_rows + 1);
    for row in skip_rows..end_row {
        let off = row * 16;
        let slice = &bytes[off..bytes.len().min(off + 16)];
        let mut hex = String::with_capacity(50);
        for i in 0..16 {
            if i == 8 {
                hex.push(' ');
            }
            if let Some(&b) = slice.get(i) {
                hex.push_str(&format!("{b:02x} "));
            } else {
                hex.push_str("   ");
            }
        }
        let ascii: String = slice
            .iter()
            .map(|&b| {
                if (0x20..=0x7E).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        lines.push(format!("{off:08x}  {hex} |{ascii}|"));
    }
    if end_row < total_rows {
        lines.push(format!(
            "[truncated at row {end_row}/{total_rows}; pass start_line={} to continue]",
            end_row + 1
        ));
    }
    lines.join("\n")
}

fn inspect_elf(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    if bytes.len() < 64 || bytes[4] != 2 {
        return out;
    }
    let le = bytes[5] != 2;
    let u16_at = |o: usize| -> Option<u16> {
        let s = bytes.get(o..o + 2)?;
        Some(if le {
            u16::from_le_bytes([s[0], s[1]])
        } else {
            u16::from_be_bytes([s[0], s[1]])
        })
    };
    let u32_at = |o: usize| -> Option<u32> {
        let s = bytes.get(o..o + 4)?;
        Some(if le {
            u32::from_le_bytes([s[0], s[1], s[2], s[3]])
        } else {
            u32::from_be_bytes([s[0], s[1], s[2], s[3]])
        })
    };
    let u64_at = |o: usize| -> Option<u64> {
        let s = bytes.get(o..o + 8)?;
        let arr: [u8; 8] = s.try_into().ok()?;
        Some(if le {
            u64::from_le_bytes(arr)
        } else {
            u64::from_be_bytes(arr)
        })
    };

    if let Some(entry) = u64_at(24) {
        out.push(format!("entry_point: 0x{entry:x}"));
    }
    // Check PT_INTERP (type == 3) in program headers.
    if let (Some(phoff), Some(phentsize), Some(phnum)) = (u64_at(32), u16_at(54), u16_at(56)) {
        let phoff = phoff as usize;
        let phentsize = phentsize as usize;
        for i in 0..(phnum as usize).min(64) {
            let base = phoff.saturating_add(i.saturating_mul(phentsize));
            if u32_at(base) == Some(3) {
                if let (Some(off), Some(filesz)) = (u64_at(base + 8), u64_at(base + 32)) {
                    let s = off as usize;
                    let e = s.saturating_add((filesz as usize).min(256));
                    if let Some(raw) = bytes.get(s..e) {
                        let interp = String::from_utf8_lossy(raw)
                            .trim_end_matches('\0')
                            .to_owned();
                        if !interp.is_empty() {
                            out.push(format!("interpreter: {interp}"));
                        }
                    }
                }
            }
        }
    }
    // Parse section headers + `.shstrtab` names.
    if let (Some(shoff), Some(shentsize), Some(shnum), Some(shstrndx)) =
        (u64_at(40), u16_at(58), u16_at(60), u16_at(62))
    {
        let shoff = shoff as usize;
        let shentsize = shentsize as usize;
        let shnum = (shnum as usize).min(128);
        let str_hdr = shoff.saturating_add((shstrndx as usize).saturating_mul(shentsize));
        if let (Some(str_off), Some(str_sz)) = (u64_at(str_hdr + 24), u64_at(str_hdr + 32)) {
            let str_start = str_off as usize;
            let str_end = str_start.saturating_add((str_sz as usize).min(65_536));
            if let Some(strtab) = bytes.get(str_start..str_end) {
                let mut secs = Vec::new();
                let mut has_dwarf = false;
                for i in 0..shnum {
                    let b = shoff.saturating_add(i.saturating_mul(shentsize));
                    let Some(name_idx) = u32_at(b).map(|v| v as usize) else {
                        continue;
                    };
                    let Some(sz) = u64_at(b + 32) else { continue };
                    if sz == 0 || name_idx >= strtab.len() {
                        continue;
                    }
                    let tail = &strtab[name_idx..];
                    let len = tail.iter().position(|&c| c == 0).unwrap_or(tail.len());
                    let name = String::from_utf8_lossy(&tail[..len]).into_owned();
                    if name.starts_with(".debug_") {
                        has_dwarf = true;
                    }
                    if !name.is_empty() && secs.len() < 14 {
                        secs.push(format!("{name}({sz}B)"));
                    }
                }
                if !secs.is_empty() {
                    out.push(format!(
                        "sections: {} · dwarf_debug: {}",
                        secs.join(", "),
                        if has_dwarf { "present" } else { "stripped/none" }
                    ));
                }
            }
        }
    }
    out
}

fn read_leb128_u32(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    let mut result = 0u32;
    let mut shift = 0u32;
    for _ in 0..5 {
        let byte = *bytes.get(*pos)?;
        *pos += 1;
        result |= ((byte & 0x7F) as u32) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
    }
    None
}

fn read_wasm_name(bytes: &[u8], pos: &mut usize) -> Option<String> {
    let len = read_leb128_u32(bytes, pos)? as usize;
    let slice = bytes.get(*pos..pos.saturating_add(len))?;
    *pos += len;
    Some(String::from_utf8_lossy(slice).into_owned())
}

fn inspect_wasm(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    if bytes.len() < 8 {
        return out;
    }
    let mut pos = 8usize;
    let mut sec_summary = Vec::new();
    let mut imports = Vec::new();
    let mut exports = Vec::new();

    while pos < bytes.len() && sec_summary.len() < 32 {
        let id = bytes[pos];
        pos += 1;
        let Some(sec_len) = read_leb128_u32(bytes, &mut pos).map(|v| v as usize) else {
            break;
        };
        let end = pos.saturating_add(sec_len);
        if end > bytes.len() {
            break;
        }
        let sec_name = match id {
            0 => "custom",
            1 => "type",
            2 => "import",
            3 => "function",
            4 => "table",
            5 => "memory",
            6 => "global",
            7 => "export",
            8 => "start",
            9 => "element",
            10 => "code",
            11 => "data",
            _ => "other",
        };
        sec_summary.push(format!("{sec_name}({sec_len}B)"));
        if id == 2 {
            let mut p = pos;
            if let Some(cnt) = read_leb128_u32(bytes, &mut p) {
                for _ in 0..cnt.min(20) {
                    let (Some(m), Some(f)) = (read_wasm_name(bytes, &mut p), read_wasm_name(bytes, &mut p)) else {
                        break;
                    };
                    imports.push(format!("{m}::{f}"));
                    let Some(&kind) = bytes.get(p) else { break };
                    p += 1;
                    match kind {
                        0 => {
                            let _ = read_leb128_u32(bytes, &mut p);
                        }
                        1 => {
                            p += 1;
                            if let Some(flags) = read_leb128_u32(bytes, &mut p) {
                                let _ = read_leb128_u32(bytes, &mut p);
                                if flags & 1 != 0 {
                                    let _ = read_leb128_u32(bytes, &mut p);
                                }
                            }
                        }
                        2 => {
                            if let Some(flags) = read_leb128_u32(bytes, &mut p) {
                                let _ = read_leb128_u32(bytes, &mut p);
                                if flags & 1 != 0 {
                                    let _ = read_leb128_u32(bytes, &mut p);
                                }
                            }
                        }
                        3 => {
                            p += 2;
                        }
                        _ => break,
                    }
                }
            }
        } else if id == 7 {
            let mut p = pos;
            if let Some(cnt) = read_leb128_u32(bytes, &mut p) {
                for _ in 0..cnt.min(24) {
                    let Some(name) = read_wasm_name(bytes, &mut p) else {
                        break;
                    };
                    let Some(&kind) = bytes.get(p) else { break };
                    p += 1;
                    let _ = read_leb128_u32(bytes, &mut p);
                    let k = match kind {
                        0 => "fn",
                        1 => "table",
                        2 => "mem",
                        3 => "global",
                        _ => "sym",
                    };
                    exports.push(format!("{name}:{k}"));
                }
            }
        }
        pos = end;
    }
    if !sec_summary.is_empty() {
        out.push(format!("wasm_sections: {}", sec_summary.join(", ")));
    }
    if !imports.is_empty() {
        out.push(format!("wasm_imports: {}", imports.join(", ")));
    }
    if !exports.is_empty() {
        out.push(format!("wasm_exports: {}", exports.join(", ")));
    }
    out
}

fn inspect_zip(bytes: &[u8]) -> Vec<String> {
    let mut entries = Vec::new();
    let mut pos = 0usize;
    while pos + 30 <= bytes.len() && entries.len() < 25 {
        if &bytes[pos..pos + 4] != b"PK\x03\x04" {
            break;
        }
        let comp_sz = u32::from_le_bytes([
            bytes[pos + 18],
            bytes[pos + 19],
            bytes[pos + 20],
            bytes[pos + 21],
        ]) as usize;
        let uncomp_sz = u32::from_le_bytes([
            bytes[pos + 22],
            bytes[pos + 23],
            bytes[pos + 24],
            bytes[pos + 25],
        ]);
        let name_len = u16::from_le_bytes([bytes[pos + 26], bytes[pos + 27]]) as usize;
        let extra_len = u16::from_le_bytes([bytes[pos + 28], bytes[pos + 29]]) as usize;
        let name_end = pos + 30 + name_len;
        let Some(name_bytes) = bytes.get(pos + 30..name_end) else {
            break;
        };
        let name = String::from_utf8_lossy(name_bytes).into_owned();
        entries.push(format!("{name} ({uncomp_sz}B)"));
        let next = name_end.saturating_add(extra_len).saturating_add(comp_sz);
        if next <= pos || comp_sz == 0 && !name.ends_with('/') {
            break;
        }
        pos = next;
    }
    if entries.is_empty() {
        Vec::new()
    } else {
        vec![format!("archive_entries: {}", entries.join(", "))]
    }
}

fn inspect_sqlite(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    if bytes.len() >= 100 {
        let raw_ps = u16::from_be_bytes([bytes[16], bytes[17]]);
        let page_size = if raw_ps == 1 { 65_536u32 } else { raw_ps as u32 };
        let enc = match u32::from_be_bytes([bytes[56], bytes[57], bytes[58], bytes[59]]) {
            1 => "UTF-8",
            2 => "UTF-16le",
            3 => "UTF-16be",
            _ => "unknown",
        };
        out.push(format!("sqlite_header: page_size={page_size}B · encoding={enc}"));
        let first_page = &bytes[..bytes.len().min(page_size as usize)];
        let schema_hits: Vec<String> = extract_printable_strings(first_page, 12, 64)
            .into_iter()
            .filter(|s| {
                let u = s.to_ascii_uppercase();
                u.contains("CREATE TABLE") || u.contains("CREATE INDEX")
            })
            .take(10)
            .collect();
        if !schema_hits.is_empty() {
            out.push(format!("sqlite_schema_page1: {}", schema_hits.join(" ; ")));
        }
    }
    out
}

fn extract_printable_strings(bytes: &[u8], min_len: usize, max_items: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for &b in bytes.iter().take(262_144) {
        if (0x20..=0x7E).contains(&b) {
            if cur.len() < 96 {
                cur.push(b as char);
            }
        } else {
            let trimmed = cur.trim();
            if trimmed.len() >= min_len && !out.iter().any(|x| x == trimmed) {
                out.push(trimmed.to_owned());
                if out.len() >= max_items {
                    return out;
                }
            }
            cur.clear();
        }
    }
    let trimmed = cur.trim();
    if trimmed.len() >= min_len && out.len() < max_items && !out.iter().any(|x| x == trimmed) {
        out.push(trimmed.to_owned());
    }
    out
}

/// Distill a browser `.har` HTTP Archive into a compact, secret-redacted API specification.
pub(super) fn distill_har(text: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text).ok()?;
    let entries = v.get("log")?.get("entries")?.as_array()?;
    let mut lines = Vec::new();
    let mut skipped_static = 0usize;

    for entry in entries {
        let req = &entry["request"];
        let resp = &entry["response"];
        let method = req["method"].as_str().unwrap_or("GET");
        let url = req["url"].as_str().unwrap_or("");
        let status = resp["status"].as_u64().unwrap_or(0);
        let mime = resp["content"]["mimeType"]
            .as_str()
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let low_url = url.to_ascii_lowercase();
        let is_static = mime.starts_with("image/")
            || mime.starts_with("font/")
            || mime == "text/css"
            || low_url.ends_with(".png")
            || low_url.ends_with(".woff2")
            || low_url.ends_with(".svg")
            || low_url.ends_with(".ico");
        if is_static {
            skipped_static += 1;
            continue;
        }
        if lines.len() >= 80 {
            break;
        }
        let mut auth_hdrs = Vec::new();
        for h in req["headers"].as_array().into_iter().flatten() {
            let name = h["name"].as_str().unwrap_or("");
            let low = name.to_ascii_lowercase();
            if matches!(
                low.as_str(),
                "authorization" | "cookie" | "x-api-key" | "x-csrf-token" | "x-auth-token"
            ) && !auth_hdrs.iter().any(|x| x == name)
            {
                auth_hdrs.push(format!("{name}=<redacted>"));
            }
        }
        let req_schema = summarize_json_shape(req["postData"]["text"].as_str().unwrap_or(""));
        let resp_schema = summarize_json_shape(resp["content"]["text"].as_str().unwrap_or(""));
        let mut row = format!("#{:>2} {method:<6} {status:>3}  {url}", lines.len() + 1);
        if !mime.is_empty() {
            row.push_str(&format!("  [{mime}]"));
        }
        if !auth_hdrs.is_empty() {
            row.push_str(&format!("  auth:[{}]", auth_hdrs.join(", ")));
        }
        if let Some(rq) = req_schema {
            row.push_str(&format!("  req:{rq}"));
        }
        if let Some(rs) = resp_schema {
            row.push_str(&format!("  resp:{rs}"));
        }
        lines.push(row);
    }

    let mut header = format!(
        "[HAR API Contract Summary: {} API call(s), {} static asset(s) filtered — pass start_line=1 to inspect raw .har]",
        lines.len(),
        skipped_static
    );
    if !lines.is_empty() {
        header.push('\n');
        header.push_str(&lines.join("\n"));
    }
    Some(header)
}

fn summarize_json_shape(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let val: Value = serde_json::from_str(trimmed).ok()?;
    match val {
        Value::Object(map) => {
            let keys: Vec<&str> = map.keys().map(String::as_str).take(10).collect();
            Some(format!("{{{}}}", keys.join(", ")))
        }
        Value::Array(arr) => {
            let inner = arr
                .first()
                .and_then(|first| first.as_object())
                .map(|m| {
                    let keys: Vec<&str> = m.keys().map(String::as_str).take(8).collect();
                    format!("{{{}}}", keys.join(", "))
                })
                .unwrap_or_else(|| "item".into());
            Some(format!("[{inner}; len={}]", arr.len()))
        }
        _ => None,
    }
}
