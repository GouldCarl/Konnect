use super::SexpNode;

/// KiCAD's own writer indents with tabs; this crate historically used two
/// spaces, so every typed-model write reindented the whole sheet and turned a
/// one-symbol edit into a whole-file diff (#210).
pub const KICAD_INDENT: &str = "\t";
/// What this crate emitted before [`write_with_indent`] existed.
pub const LEGACY_INDENT: &str = "  ";

pub fn write(node: &SexpNode) -> String {
    write_with_indent(node, LEGACY_INDENT)
}

/// As [`write`], but indenting each level with `indent`.
///
/// Callers that loaded a file should pass the indent that file already used,
/// so a targeted edit stays a targeted diff.
pub fn write_with_indent(node: &SexpNode, indent: &str) -> String {
    let mut buf = String::with_capacity(16384);
    write_node(node, &mut buf, 0, indent);
    buf.push('\n');
    buf
}

/// The indentation unit a KiCAD file already uses, from its first indented
/// line: a tab, or the run of spaces that opens it. Falls back to KiCAD's own
/// tab for a file with nothing to learn from.
pub fn detect_indent(source: &str) -> String {
    for line in source.lines() {
        let trimmed = line.trim_start_matches([' ', '\t']);
        if trimmed.is_empty() {
            continue;
        }
        let lead = &line[..line.len() - trimmed.len()];
        if lead.starts_with('\t') {
            return "\t".to_string();
        }
        if !lead.is_empty() {
            return lead.to_string();
        }
    }
    KICAD_INDENT.to_string()
}

/// This crate's writer always builds its buffer with bare `\n`; KiCAD itself
/// writes whatever the platform that saved the file used (`\r\n` on Windows,
/// `\n` elsewhere). Rewriting a CRLF file turned every line ending into an LF
/// one — a whole-file diff for no reason (#21) — so the line ending is
/// sniffed at load, the same way [`detect_indent`] sniffs the indent unit, and
/// carried through to every write.
pub const LF: &str = "\n";
pub const CRLF: &str = "\r\n";

/// The line ending a KiCAD file already uses: `\r\n` if any appears, `\n`
/// otherwise (including a file with no newlines at all).
pub fn detect_line_ending(source: &str) -> &'static str {
    if source.contains("\r\n") {
        CRLF
    } else {
        LF
    }
}

/// As [`write_with_indent`], but re-writing the trailing newline of every line
/// as `line_ending`.
///
/// Safe as a blanket replace: the only bare `\n` bytes the writer ever emits
/// are the structural separators it inserts itself — a quoted string's own
/// embedded newline is escaped to the two-character `\n` inside
/// [`write_node`]'s `SexpNode::Str` arm, never written as a raw byte.
pub fn write_with_indent_and_eol(node: &SexpNode, indent: &str, line_ending: &str) -> String {
    let text = write_with_indent(node, indent);
    if line_ending == LF {
        text
    } else {
        text.replace(LF, line_ending)
    }
}

fn write_node(node: &SexpNode, buf: &mut String, depth: usize, indent: &str) {
    match node {
        SexpNode::Atom(s) => buf.push_str(s),
        SexpNode::Str(s) => {
            buf.push('"');
            for c in s.chars() {
                match c {
                    '"' => buf.push_str("\\\""),
                    '\\' => buf.push_str("\\\\"),
                    '\n' => buf.push_str("\\n"),
                    '\t' => buf.push_str("\\t"),
                    '\r' => buf.push_str("\\r"),
                    c => buf.push(c),
                }
            }
            buf.push('"');
        }
        SexpNode::List(children) => {
            if children.is_empty() {
                buf.push_str("()");
                return;
            }

            let has_list_child = children.iter().skip(1).any(|c| c.is_list());
            // KiCAD never wraps a `(pts …)` polyline one point per line: every
            // `(xy …)` point it holds, however many, is packed onto the single
            // line that follows the `pts` tag, with the closing paren on its
            // own line at the node's own indent — `pts` on its own line, then
            // all the points together, then `)`. The general multi-line rule
            // below (one sub-list per line) would split each point onto its
            // own line instead — right for every other node with list
            // children, wrong for exactly this one (#21).
            let is_pts = matches!(children.first(), Some(SexpNode::Atom(s)) if s == "pts");

            buf.push('(');

            if depth == 0 {
                // Root: tag on same line, each child on its own indented line.
                for (i, child) in children.iter().enumerate() {
                    if i == 0 {
                        write_node(child, buf, 1, indent);
                    } else {
                        buf.push('\n');
                        write_indent(buf, 1, indent);
                        write_node(child, buf, 1, indent);
                    }
                }
                buf.push('\n');
            } else if is_pts {
                write_node(&children[0], buf, depth + 1, indent);
                buf.push('\n');
                write_indent(buf, depth + 1, indent);
                for (i, child) in children.iter().skip(1).enumerate() {
                    if i > 0 {
                        buf.push(' ');
                    }
                    write_node(child, buf, depth + 1, indent);
                }
                buf.push('\n');
                write_indent(buf, depth, indent);
            } else if has_list_child {
                // Multi-line: scalars inline after tag, sub-lists on new lines.
                for (i, child) in children.iter().enumerate() {
                    if i == 0 {
                        write_node(child, buf, depth + 1, indent);
                    } else if child.is_list() {
                        buf.push('\n');
                        write_indent(buf, depth + 1, indent);
                        write_node(child, buf, depth + 1, indent);
                    } else {
                        buf.push(' ');
                        write_node(child, buf, depth + 1, indent);
                    }
                }
                // KiCAD closes a node that has list children on its own line at
                // the node's own indent. Collapsing it onto the last child —
                // `(uuid "j1"))` — differs on every such node in the file, which
                // is most of them (#210).
                buf.push('\n');
                write_indent(buf, depth, indent);
            } else {
                // All scalars: single line.
                for (i, child) in children.iter().enumerate() {
                    if i > 0 {
                        buf.push(' ');
                    }
                    write_node(child, buf, depth + 1, indent);
                }
            }

            buf.push(')');
        }
    }
}

fn write_indent(buf: &mut String, depth: usize, indent: &str) {
    for _ in 0..depth {
        buf.push_str(indent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sexp::parser;

    #[test]
    fn detect_line_ending_finds_crlf() {
        assert_eq!(detect_line_ending("(a)\r\n(b)\r\n"), CRLF);
        assert_eq!(detect_line_ending("(a)\n(b)\n"), LF);
        assert_eq!(
            detect_line_ending("(a)"),
            LF,
            "no newline at all falls back to LF"
        );
    }

    #[test]
    fn write_with_indent_and_eol_preserves_crlf() {
        let node =
            parser::parse("(kicad_sch (uuid \"x\") (wire (pts (xy 1 1) (xy 2 2))))").unwrap();
        let out = write_with_indent_and_eol(&node, "\t", CRLF);
        assert!(
            out.contains("\r\n"),
            "must carry CRLF line endings:\n{out:?}"
        );
        assert!(
            !out.replace("\r\n", "").contains('\n'),
            "no bare LF left over:\n{out:?}"
        );
    }

    #[test]
    fn write_with_indent_and_eol_lf_is_unchanged() {
        let node = parser::parse("(kicad_sch (uuid \"x\"))").unwrap();
        let with_lf = write_with_indent(&node, "\t");
        let via_eol = write_with_indent_and_eol(&node, "\t", LF);
        assert_eq!(with_lf, via_eol);
    }

    /// KiCAD packs every `(xy …)` point of a `(pts …)` polyline onto the one
    /// line that follows the `pts` tag, however many there are (2 through 7
    /// observed in real files) — never one point per line, and never
    /// wrapped at a fixed width (#21).
    #[test]
    fn pts_polyline_packs_every_point_onto_one_line() {
        let node = parser::parse(
            "(polyline (pts (xy 0 0) (xy 0 1.27) (xy -1.016 1.905) (xy 0 2.54) (xy 1.016 1.905) (xy 0 1.27)) (stroke (width 0)))",
        )
        .unwrap();
        let out = write_with_indent(&node, KICAD_INDENT);
        assert!(
            out.contains("(pts\n\t\t(xy 0 0) (xy 0 1.27) (xy -1.016 1.905) (xy 0 2.54) (xy 1.016 1.905) (xy 0 1.27)\n\t)"),
            "all six points must share one line between the pts tag and its closing paren:\n{out}"
        );
    }

    #[test]
    fn pts_with_two_points_still_shares_one_line() {
        // Wrapped in a parent so `pts` is not itself the root: the root's own
        // formatting rule (one child per line, applied regardless of tag) is
        // deliberately different and would defeat this test if `pts` were
        // parsed and written as the top-level node.
        let node = parser::parse("(polyline (pts (xy -1.27 0) (xy -0.508 0)))").unwrap();
        let out = write_with_indent(&node, KICAD_INDENT);
        assert_eq!(
            out,
            "(polyline\n\t(pts\n\t\t(xy -1.27 0) (xy -0.508 0)\n\t)\n)\n"
        );
    }
}
