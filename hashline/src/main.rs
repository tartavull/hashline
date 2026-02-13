use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use xxhash_rust::xxh32::xxh32;

#[derive(Parser, Debug)]
#[command(name = "hashline")]
#[command(about = "Hashline read/edit tools (LINE:HASH anchors)")]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Read a text file and print hashline-prefixed output: LINE:HASH|content
    Read {
        path: PathBuf,
        /// Start line (1-indexed)
        #[arg(long)]
        offset: Option<usize>,
        /// Max lines
        #[arg(long)]
        limit: Option<usize>,
    },

    /// Apply hashline edits to a text file
    Edit {
        path: PathBuf,
        /// JSON edits payload (either a full object or just an array of edits)
        #[arg(long, conflicts_with = "edits_file")]
        edits_json: Option<String>,
        /// Read JSON edits payload from file
        #[arg(long)]
        edits_file: Option<PathBuf>,
        /// Print a unified diff-like preview (very basic) before applying
        #[arg(long)]
        preview: bool,
    },
}

#[derive(Debug, Deserialize, Clone)]
struct EditRequest {
    #[serde(default)]
    edits: Vec<HashlineEdit>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum HashlineEdit {
    SetLine { set_line: SetLine },
    ReplaceLines { replace_lines: ReplaceLines },
    InsertAfter { insert_after: InsertAfter },
    Replace { replace: ReplaceText },
}

#[derive(Debug, Deserialize, Clone)]
struct SetLine {
    anchor: String,
    new_text: String,
}

#[derive(Debug, Deserialize, Clone)]
struct ReplaceLines {
    start_anchor: String,
    end_anchor: String,
    new_text: String,
}

#[derive(Debug, Deserialize, Clone)]
struct InsertAfter {
    anchor: String,
    text: String,
}

#[derive(Debug, Deserialize, Clone)]
struct ReplaceText {
    old_text: String,
    new_text: String,
    #[serde(default)]
    all: Option<bool>,
}

#[derive(Debug, Clone)]
struct LineRef {
    line: usize,
    hash: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.cmd {
        Command::Read { path, offset, limit } => {
            let content = fs::read_to_string(&path)
                .with_context(|| format!("read: failed to read {}", path.display()))?;
            let normalized = normalize_to_lf(&content);
            let lines: Vec<&str> = split_preserve_last_empty(&normalized);

            let start = offset.unwrap_or(1);
            if start == 0 {
                bail!("--offset is 1-indexed (must be >= 1)");
            }
            if start > lines.len().max(1) {
                // Allow reading from a past-the-end offset on empty-ish files.
                bail!("offset {} out of range (file has {} lines)", start, lines.len());
            }

            let max_lines = limit.unwrap_or(lines.len());
            let mut printed = 0usize;

            for (i, line) in lines.iter().enumerate() {
                let line_no = i + 1;
                if line_no < start {
                    continue;
                }
                if printed >= max_lines {
                    break;
                }
                let hash = compute_line_hash(*line);
                println!("{}:{}|{}", line_no, hash, line);
                printed += 1;
            }
        }

        Command::Edit {
            path,
            edits_json,
            edits_file,
            preview,
        } => {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("edit: failed to read {}", path.display()))?;
            let line_ending = detect_line_ending(&raw);
            let had_final_newline = raw.ends_with('\n');
            let normalized = normalize_to_lf(&raw);

            let edits_payload = if let Some(p) = edits_file {
                fs::read_to_string(&p).with_context(|| format!("edit: failed to read edits file {}", p.display()))?
            } else if let Some(s) = edits_json {
                s
            } else {
                bail!("provide --edits-json or --edits-file");
            };

            let edits: Vec<HashlineEdit> = parse_edits_payload(&edits_payload)
                .context("edit: failed to parse edits JSON")?;

            let old_lines: Vec<String> = split_preserve_last_empty(&normalized)
                .into_iter()
                .map(|s| s.to_string())
                .collect();

            let new_lines = apply_hashline_edits(old_lines.clone(), &edits)
                .with_context(|| format!("edit: failed to apply edits to {}", path.display()))?;

            if preview {
                eprintln!("--- {}\n+++ {}\n", path.display(), path.display());
                render_basic_diff(&old_lines, &new_lines);
            }

            if old_lines == new_lines {
                bail!("no changes made (edits produced identical content)");
            }

            let mut out = new_lines.join("\n");
            if had_final_newline {
                out.push('\n');
            }
            out = restore_line_endings(&out, line_ending);

            fs::write(&path, out).with_context(|| format!("edit: failed to write {}", path.display()))?;
            eprintln!("updated {}", path.display());
        }
    }

    Ok(())
}

fn parse_edits_payload(s: &str) -> Result<Vec<HashlineEdit>> {
    // Accept either:
    // - {"edits": [ ... ]}
    // - [ ... ]
    if s.trim_start().starts_with('[') {
        let edits: Vec<HashlineEdit> = serde_json::from_str(s)?;
        return Ok(edits);
    }
    let req: EditRequest = serde_json::from_str(s)?;
    Ok(req.edits)
}

fn detect_line_ending(s: &str) -> &'static str {
    if s.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn normalize_to_lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

fn restore_line_endings(s: &str, ending: &str) -> String {
    if ending == "\n" {
        s.to_string()
    } else {
        s.replace("\n", ending)
    }
}

fn split_preserve_last_empty(s: &str) -> Vec<&str> {
    // Like JS `content.split("\n")`: keeps trailing empty line if file ends with \n.
    // But we do NOT want to treat a trailing newline as an extra addressable empty line.
    // So we drop exactly one final empty segment if the file ends with "\n".
    let mut parts: Vec<&str> = s.split('\n').collect();
    if s.ends_with('\n') {
        if let Some(last) = parts.last() {
            if last.is_empty() {
                parts.pop();
            }
        }
    }
    parts
}

fn compute_line_hash(line: &str) -> String {
    let mut normalized = String::with_capacity(line.len());
    for ch in line.chars() {
        if ch == '\r' {
            continue;
        }
        if ch.is_whitespace() {
            continue;
        }
        normalized.push(ch);
    }

    let h = xxh32(normalized.as_bytes(), 0);
    let truncated = (h as u32) & 0xffff;
    format!("{:04x}", truncated)
}

fn parse_line_ref(s: &str) -> Result<LineRef> {
    let mut it = s.split(':');
    let line_s = it.next().ok_or_else(|| anyhow!("invalid anchor: {s}"))?;
    let hash_s = it.next().ok_or_else(|| anyhow!("invalid anchor: {s}"))?;
    if it.next().is_some() {
        bail!("invalid anchor (too many ':'): {s}");
    }

    let line: usize = line_s
        .parse()
        .map_err(|_| anyhow!("invalid line number in anchor: {s}"))?;
    if line == 0 {
        bail!("anchors are 1-indexed (line must be >= 1): {s}");
    }

    let hash = hash_s.trim().to_ascii_lowercase();
    if hash.is_empty() {
        bail!("invalid hash in anchor: {s}");
    }

    Ok(LineRef { line, hash })
}

fn apply_hashline_edits(mut lines: Vec<String>, edits: &[HashlineEdit]) -> Result<Vec<String>> {
    if edits.is_empty() {
        return Ok(lines);
    }

    // Build hash -> unique line map (1-indexed) using current file.
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut first_seen: HashMap<String, usize> = HashMap::new();
    for (i, line) in lines.iter().enumerate() {
        let ln = i + 1;
        let h = compute_line_hash(line);
        *counts.entry(h.clone()).or_insert(0) += 1;
        first_seen.entry(h).or_insert(ln);
    }
    let mut unique: HashMap<String, usize> = HashMap::new();
    for (h, c) in counts {
        if c == 1 {
            if let Some(ln) = first_seen.get(&h) {
                unique.insert(h, *ln);
            }
        }
    }

    // Parse and validate all anchors before mutating. Relocate if hash is uniquely found elsewhere.
    let mut mismatches: Vec<(usize, String, String)> = Vec::new();

    #[derive(Clone)]
    enum ParsedSpec {
        Single { r: LineRef, dst: String },
        Range { start: LineRef, end: LineRef, dst: String },
        InsertAfter { after: LineRef, dst: String },
        ReplaceText { old: String, new_: String, all: bool },
    }

    let mut parsed: Vec<(usize, ParsedSpec)> = Vec::new();

    for (idx, edit) in edits.iter().enumerate() {
        match edit {
            HashlineEdit::SetLine { set_line } => {
                let r = parse_line_ref(&set_line.anchor)?;
                parsed.push((idx, ParsedSpec::Single { r, dst: set_line.new_text.clone() }));
            }
            HashlineEdit::ReplaceLines { replace_lines } => {
                let start = parse_line_ref(&replace_lines.start_anchor)?;
                let end = parse_line_ref(&replace_lines.end_anchor)?;
                parsed.push((
                    idx,
                    ParsedSpec::Range { start, end, dst: replace_lines.new_text.clone() },
                ));
            }
            HashlineEdit::InsertAfter { insert_after } => {
                let after = parse_line_ref(&insert_after.anchor)?;
                if insert_after.text.is_empty() {
                    bail!("insert_after.text must be non-empty");
                }
                parsed.push((idx, ParsedSpec::InsertAfter { after, dst: insert_after.text.clone() }));
            }
            HashlineEdit::Replace { replace } => {
                if replace.old_text.is_empty() {
                    bail!("replace.old_text must be non-empty");
                }
                parsed.push((
                    idx,
                    ParsedSpec::ReplaceText {
                        old: replace.old_text.clone(),
                        new_: replace.new_text.clone(),
                        all: replace.all.unwrap_or(false),
                    },
                ));
            }
        }
    }

    // Validate and relocate
    for (_idx, spec) in parsed.iter_mut() {
        match spec {
            ParsedSpec::Single { r, .. } => validate_or_relocate(r, &lines, &unique, &mut mismatches)?,
            ParsedSpec::Range { start, end, .. } => {
                validate_or_relocate(start, &lines, &unique, &mut mismatches)?;
                validate_or_relocate(end, &lines, &unique, &mut mismatches)?;
                if start.line > end.line {
                    bail!("replace_lines.start_anchor line must be <= end_anchor line");
                }
            }
            ParsedSpec::InsertAfter { after, .. } => validate_or_relocate(after, &lines, &unique, &mut mismatches)?,
            ParsedSpec::ReplaceText { .. } => {}
        }
    }

    if !mismatches.is_empty() {
        bail!(render_mismatch_error(&lines, &mismatches));
    }

    // Sort bottom-up so earlier splices don't invalidate later line numbers.
    // ReplaceText operations run last (they don't use anchors).
    let sort_key = |spec: &ParsedSpec| -> (usize, usize) {
        match spec {
            ParsedSpec::Single { r, .. } => (r.line, 0),
            ParsedSpec::Range { end, .. } => (end.line, 0),
            ParsedSpec::InsertAfter { after, .. } => (after.line, 1),
            ParsedSpec::ReplaceText { .. } => (0, 9),
        }
    };
    parsed.sort_by(|a, b| {
        let a_key = sort_key(&a.1);
        let b_key = sort_key(&b.1);
        // descending by line, then precedence
        b_key.cmp(&a_key)
    });

    for (_idx, spec) in parsed {
        match spec {
            ParsedSpec::Single { r, dst } => {
                let dst_lines = split_dst_lines(&dst);
                let at = r.line - 1;
                if at >= lines.len() {
                    bail!("line {} does not exist (file has {} lines)", r.line, lines.len());
                }
                lines.splice(at..at + 1, dst_lines);
            }
            ParsedSpec::Range { start, end, dst } => {
                let dst_lines = split_dst_lines(&dst);
                let s = start.line - 1;
                let e = end.line - 1;
                if s >= lines.len() || e >= lines.len() {
                    bail!("range out of bounds (file has {} lines)", lines.len());
                }
                if s > e {
                    bail!("invalid range: start > end");
                }
                lines.splice(s..e + 1, dst_lines);
            }
            ParsedSpec::InsertAfter { after, dst } => {
                let dst_lines = split_dst_lines(&dst);
                let at = after.line; // insert after => index is line (1-indexed) as 0-index insert point
                if after.line > lines.len() {
                    bail!("line {} does not exist (file has {} lines)", after.line, lines.len());
                }
                lines.splice(at..at, dst_lines);
            }
            ParsedSpec::ReplaceText { old, new_, all } => {
                if all {
                    lines = lines.join("\n").replace(&old, &new_).split('\n').map(|s| s.to_string()).collect();
                } else {
                    let joined = lines.join("\n");
                    if let Some(pos) = joined.find(&old) {
                        let mut out = String::with_capacity(joined.len() - old.len() + new_.len());
                        out.push_str(&joined[..pos]);
                        out.push_str(&new_);
                        out.push_str(&joined[pos + old.len()..]);
                        lines = out.split('\n').map(|s| s.to_string()).collect();
                    } else {
                        bail!("replace.old_text not found");
                    }
                }
            }
        }
    }

    Ok(lines)
}

fn split_dst_lines(dst: &str) -> Vec<String> {
    if dst.is_empty() {
        Vec::new()
    } else {
        dst.split('\n').map(|s| s.to_string()).collect()
    }
}

fn validate_or_relocate(
    r: &mut LineRef,
    lines: &[String],
    unique: &HashMap<String, usize>,
    mismatches: &mut Vec<(usize, String, String)>,
) -> Result<()> {
    if r.line < 1 || r.line > lines.len() {
        bail!("line {} does not exist (file has {} lines)", r.line, lines.len());
    }

    let actual = compute_line_hash(&lines[r.line - 1]);
    if actual == r.hash {
        return Ok(());
    }

    if let Some(relocated) = unique.get(&r.hash) {
        r.line = *relocated;
        return Ok(());
    }

    mismatches.push((r.line, r.hash.clone(), actual));
    Ok(())
}

fn render_mismatch_error(lines: &[String], mismatches: &[(usize, String, String)]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{} line(s) have changed since last read. Re-read the file and use updated LINE:HASH refs.\n\n",
        mismatches.len()
    ));

    for (line, expected, actual) in mismatches {
        let content = lines.get(line - 1).cloned().unwrap_or_default();
        out.push_str(&format!(
            ">>> {}:{}|{}\n    expected {}\n\n",
            line, actual, content, expected
        ));
    }

    out.push_str("Quick fix: replace stale refs:\n");
    for (line, expected, actual) in mismatches {
        out.push_str(&format!("  {}:{} -> {}:{}\n", line, expected, line, actual));
    }

    out
}

fn render_basic_diff(old_lines: &[String], new_lines: &[String]) {
    // Very basic: show removed/added lines if lengths differ, else show line-by-line changes.
    let max = old_lines.len().max(new_lines.len());
    for i in 0..max {
        let a = old_lines.get(i);
        let b = new_lines.get(i);
        match (a, b) {
            (Some(x), Some(y)) if x == y => {}
            (Some(x), Some(y)) => {
                eprintln!("-{}", x);
                eprintln!("+{}", y);
            }
            (Some(x), None) => eprintln!("-{}", x),
            (None, Some(y)) => eprintln!("+{}", y),
            (None, None) => {}
        }
    }
}
