use crate::error::{Error, Result};
use crate::sexp::{atom, qstr, tagged, SexpNode};
use crate::types::{fmt_f64, At, Effects};

// ---- Junction ---------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Junction {
    pub x: f64,
    pub y: f64,
    pub diameter: f64,
    pub uuid: String,
    pub raw_color: Option<SexpNode>,
}

impl Junction {
    pub fn new(x: f64, y: f64) -> Self {
        Junction {
            x,
            y,
            diameter: 0.0,
            uuid: uuid::Uuid::new_v4().to_string(),
            raw_color: None,
        }
    }

    pub fn from_sexp(node: &SexpNode) -> Result<Self> {
        let at = node.find("at").ok_or(Error::MissingField("at"))?;
        let s = at.scalar_args();
        let x: f64 = s.first().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let y: f64 = s.get(1).and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let diameter = node.get_float("diameter").unwrap_or(0.0);
        let uuid = node.get_value("uuid").unwrap_or("").to_owned();
        let raw_color = node.find("color").cloned();
        Ok(Junction {
            x,
            y,
            diameter,
            uuid,
            raw_color,
        })
    }

    pub fn to_sexp(&self) -> SexpNode {
        let mut c = vec![
            atom("junction"),
            tagged("at", vec![atom(fmt_f64(self.x)), atom(fmt_f64(self.y))]),
            tagged("diameter", vec![atom(fmt_f64(self.diameter))]),
        ];
        if let Some(col) = &self.raw_color {
            c.push(col.clone());
        }
        c.push(tagged("uuid", vec![qstr(self.uuid.clone())]));
        SexpNode::List(c)
    }

    pub fn position(&self) -> (f64, f64) {
        (self.x, self.y)
    }
    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
    }
}

// ---- Text -------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Text {
    pub text: String,
    /// `(exclude_from_sim …)`, present in KiCAD 8+ files, written right after
    /// the text string and before `(at …)`. `None` for older files that omit
    /// it. This used to be silently dropped on every round-trip — `Text` had
    /// no unmodelled-child deny-list at all, unlike `Symbol`/`Sheet` (#143) —
    /// so any free-floating text block lost the token the moment a save
    /// touched the file (#21).
    pub exclude_from_sim: Option<bool>,
    pub at: At,
    pub uuid: String,
    pub effects: Option<Effects>,
}

impl Text {
    pub fn new(text: impl Into<String>, x: f64, y: f64) -> Self {
        Text {
            text: text.into(),
            exclude_from_sim: None,
            at: At::new(x, y),
            uuid: uuid::Uuid::new_v4().to_string(),
            effects: None,
        }
    }

    pub fn from_sexp(node: &SexpNode) -> Result<Self> {
        let text = node
            .value()
            .ok_or(Error::MissingField("text content"))?
            .to_owned();
        let exclude_from_sim = node.get_bool("exclude_from_sim");
        let at = node
            .find("at")
            .and_then(At::from_sexp)
            .ok_or(Error::MissingField("at"))?;
        let uuid = node.get_value("uuid").unwrap_or("").to_owned();
        let effects = node.find("effects").and_then(Effects::from_sexp);
        Ok(Text {
            text,
            exclude_from_sim,
            at,
            uuid,
            effects,
        })
    }

    pub fn to_sexp(&self) -> SexpNode {
        let mut c = vec![atom("text"), qstr(self.text.clone())];
        if let Some(x) = self.exclude_from_sim {
            c.push(tagged(
                "exclude_from_sim",
                vec![atom(if x { "yes" } else { "no" })],
            ));
        }
        c.push(self.at.to_sexp());
        if let Some(e) = &self.effects {
            c.push(e.to_sexp());
        }
        c.push(tagged("uuid", vec![qstr(self.uuid.clone())]));
        SexpNode::List(c)
    }

    pub fn position(&self) -> (f64, f64) {
        (self.at.x, self.at.y)
    }
    pub fn translate(&mut self, dx: f64, dy: f64) {
        self.at.translate(dx, dy);
    }
}

// ---- NoConnect --------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct NoConnect {
    pub x: f64,
    pub y: f64,
    pub uuid: String,
}

impl NoConnect {
    pub fn new(x: f64, y: f64) -> Self {
        NoConnect {
            x,
            y,
            uuid: uuid::Uuid::new_v4().to_string(),
        }
    }

    pub fn from_sexp(node: &SexpNode) -> Result<Self> {
        let at = node.find("at").ok_or(Error::MissingField("at"))?;
        let s = at.scalar_args();
        let x: f64 = s.first().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let y: f64 = s.get(1).and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let uuid = node.get_value("uuid").unwrap_or("").to_owned();
        Ok(NoConnect { x, y, uuid })
    }

    pub fn to_sexp(&self) -> SexpNode {
        SexpNode::List(vec![
            atom("no_connect"),
            tagged("at", vec![atom(fmt_f64(self.x)), atom(fmt_f64(self.y))]),
            tagged("uuid", vec![qstr(self.uuid.clone())]),
        ])
    }

    pub fn position(&self) -> (f64, f64) {
        (self.x, self.y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sexp::parser;

    /// A free-floating text block as eeschema writes one, carrying
    /// `(exclude_from_sim …)` right after the string, before `(at …)`. `Text`
    /// had no unmodelled-child preservation at all (unlike `Symbol`/`Sheet`),
    /// so this token was silently discarded on every round-trip rather than
    /// merely reordered (#21, the same class of loss #143 fixed elsewhere).
    #[test]
    fn exclude_from_sim_round_trips_and_keeps_its_position() {
        let src = "(text \"note\" (exclude_from_sim no) (at 5 5 0) (uuid \"t1\"))";
        let text = Text::from_sexp(&parser::parse(src).unwrap()).unwrap();
        assert_eq!(text.exclude_from_sim, Some(false));

        let out = crate::sexp::writer::write(&text.to_sexp());
        assert!(out.contains("(exclude_from_sim no)"), "{out}");
        let exclude_pos = out.find("(exclude_from_sim").unwrap();
        let at_pos = out.find("(at ").unwrap();
        assert!(
            exclude_pos < at_pos,
            "exclude_from_sim must precede at, matching KiCAD's own order:\n{out}"
        );
    }

    #[test]
    fn text_without_exclude_from_sim_does_not_invent_it() {
        let src = "(text \"note\" (at 5 5 0) (uuid \"t1\"))";
        let text = Text::from_sexp(&parser::parse(src).unwrap()).unwrap();
        assert_eq!(text.exclude_from_sim, None);
        let out = crate::sexp::writer::write(&text.to_sexp());
        assert!(!out.contains("exclude_from_sim"), "{out}");
    }
}
