use crate::{Result, engine::Candidate, source::ReadSource};

/// No shell, executable content, HTML renderer or document decoder is involved.
pub fn preview(source: &mut dyn ReadSource, item: &Candidate) -> Result<String> {
    item.validate(source.len())?;
    let mut data = vec![0; item.recovered_size.min(16384) as usize];
    item.read_at(source, 0, &mut data)?;
    if let Ok(text) = std::str::from_utf8(&data)
        && !data.contains(&0)
        && text
            .chars()
            .filter(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
            .count()
            < 3
    {
        return Ok(text
            .chars()
            .filter(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t'))
            .take(8192)
            .collect());
    }
    let mut out = String::new();
    use std::fmt::Write;
    for (index, row) in data.chunks(16).take(128).enumerate() {
        let _ = write!(out, "{:04X}  ", index * 16);
        for c in row {
            let _ = write!(out, "{c:02X} ");
        }
        for _ in row.len()..16 {
            out.push_str("   ");
        }
        out.push_str("  ");
        for c in row {
            out.push(if (32..127).contains(c) {
                *c as char
            } else {
                '.'
            });
        }
        out.push_str("\r\n");
    }
    Ok(out)
}
