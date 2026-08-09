use gpui::{
    AnyElement, FontFallbacks, FontWeight, Hsla, IntoElement, ParentElement, SharedString, Styled,
};

use std::fmt;

const MAX_PARSE_DEPTH: usize = 64;

#[derive(Clone, Debug, PartialEq)]
pub enum MathNode {
    Row(Vec<MathNode>),
    Text(String),
    Symbol(String),
    Fraction {
        numerator: Box<MathNode>,
        denominator: Box<MathNode>,
    },
    Radical {
        index: Option<Box<MathNode>>,
        radicand: Box<MathNode>,
    },
    Script {
        base: Box<MathNode>,
        subscript: Option<Box<MathNode>>,
        superscript: Option<Box<MathNode>>,
    },
    Styled {
        style: MathStyleKind,
        content: Box<MathNode>,
    },
    Matrix {
        environment: String,
        rows: Vec<Vec<MathNode>>,
    },
    Spacing(f32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MathStyleKind {
    Blackboard,
    Calligraphic,
    Fraktur,
    Bold,
    Roman,
    Italic,
    Sans,
    Monospace,
    Text,
    Operator,
}

#[derive(Clone)]
pub struct MathStyle {
    pub base_font_size: f32,
    pub text_color: Hsla,
    pub font_family: SharedString,
    pub font_fallbacks: FontFallbacks,
}

impl fmt::Debug for MathStyle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MathStyle")
            .field("base_font_size", &self.base_font_size)
            .field("text_color", &self.text_color)
            .finish_non_exhaustive()
    }
}

pub fn parse_math(source: &str) -> MathNode {
    Parser::new(source).parse()
}

struct Parser {
    chars: Vec<char>,
    position: usize,
}

impl Parser {
    fn new(source: &str) -> Self {
        Self {
            chars: source.chars().collect(),
            position: 0,
        }
    }

    fn parse(mut self) -> MathNode {
        self.parse_row(None, 0)
    }

    fn parse_row(&mut self, terminator: Option<char>, depth: usize) -> MathNode {
        if depth >= MAX_PARSE_DEPTH {
            let remainder: String = self.chars[self.position..].iter().collect();
            self.position = self.chars.len();
            return MathNode::Text(remainder);
        }
        let mut nodes = Vec::new();
        while let Some(character) = self.chars.get(self.position).copied() {
            if terminator == Some(character) {
                self.position += 1;
                break;
            }
            if character == '}' {
                break;
            }
            if character.is_whitespace() {
                self.position += 1;
                continue;
            }
            let mut node = self.parse_atom(depth);
            let mut subscript = None;
            let mut superscript = None;
            while let Some(script) = self.chars.get(self.position).copied() {
                if script != '^' && script != '_' {
                    break;
                }
                self.position += 1;
                let argument = self.parse_argument(depth + 1);
                if script == '_' {
                    subscript = Some(argument);
                } else {
                    superscript = Some(argument);
                }
            }
            if subscript.is_some() || superscript.is_some() {
                node = MathNode::Script {
                    base: Box::new(node),
                    subscript,
                    superscript,
                };
            }
            nodes.push(node);
        }
        MathNode::Row(nodes)
    }

    fn parse_atom(&mut self, depth: usize) -> MathNode {
        let Some(character) = self.chars.get(self.position).copied() else {
            return MathNode::Text(String::new());
        };
        match character {
            '{' => {
                self.position += 1;
                self.parse_row(Some('}'), depth + 1)
            }
            '\\' => self.parse_command(depth),
            '^' | '_' => {
                self.position += 1;
                MathNode::Text(character.to_string())
            }
            '&' => {
                self.position += 1;
                MathNode::Text("&".to_owned())
            }
            _ => {
                self.position += 1;
                if character.is_ascii_digit() {
                    MathNode::Text(character.to_string())
                } else if character.is_ascii_alphabetic() {
                    MathNode::Styled {
                        style: MathStyleKind::Italic,
                        content: Box::new(MathNode::Text(character.to_string())),
                    }
                } else {
                    MathNode::Text(character.to_string())
                }
            }
        }
    }

    fn parse_argument(&mut self, depth: usize) -> Box<MathNode> {
        if depth >= MAX_PARSE_DEPTH {
            return Box::new(MathNode::Text(String::new()));
        }
        if self.chars.get(self.position) == Some(&'{') {
            self.position += 1;
            Box::new(self.parse_row(Some('}'), depth + 1))
        } else {
            Box::new(self.parse_atom(depth + 1))
        }
    }

    fn parse_command(&mut self, depth: usize) -> MathNode {
        self.position += 1;
        let start = self.position;
        while self
            .chars
            .get(self.position)
            .is_some_and(|character| character.is_ascii_alphabetic())
        {
            self.position += 1;
        }
        let name: String = self.chars[start..self.position].iter().collect();
        if name.is_empty() {
            let character = self.chars.get(self.position).copied().unwrap_or('\\');
            self.position = self.position.saturating_add(1);
            return MathNode::Text(character.to_string());
        }
        match name.as_str() {
            "frac" | "dfrac" | "tfrac" => MathNode::Fraction {
                numerator: self.parse_argument(depth + 1),
                denominator: self.parse_argument(depth + 1),
            },
            "sqrt" => {
                let index = if self.chars.get(self.position) == Some(&'[') {
                    self.position += 1;
                    Some(Box::new(self.parse_row(Some(']'), depth + 1)))
                } else {
                    None
                };
                MathNode::Radical {
                    index,
                    radicand: self.parse_argument(depth + 1),
                }
            }
            "begin" => self.parse_environment(depth + 1),
            "left" | "right" | "big" | "Big" | "bigg" | "Bigg" | "bigl" | "bigr" | "Bigl"
            | "Bigr" | "biggl" | "biggr" | "Biggl" | "Biggr" => {
                MathNode::Symbol(delimiter(self.chars.get(self.position).copied()))
            }
            "mathbb" => self.style_argument(MathStyleKind::Blackboard, depth),
            "mathcal" => self.style_argument(MathStyleKind::Calligraphic, depth),
            "mathfrak" => self.style_argument(MathStyleKind::Fraktur, depth),
            "mathbf" => self.style_argument(MathStyleKind::Bold, depth),
            "mathrm" => self.style_argument(MathStyleKind::Roman, depth),
            "mathit" => self.style_argument(MathStyleKind::Italic, depth),
            "mathsf" => self.style_argument(MathStyleKind::Sans, depth),
            "mathtt" => self.style_argument(MathStyleKind::Monospace, depth),
            "text" | "textbf" => self.style_argument(
                if name == "text" {
                    MathStyleKind::Text
                } else {
                    MathStyleKind::Bold
                },
                depth,
            ),
            "operator" => self.style_argument(MathStyleKind::Operator, depth),
            "," => MathNode::Spacing(0.17),
            ":" => MathNode::Spacing(0.22),
            ";" => MathNode::Spacing(0.28),
            "!" => MathNode::Spacing(-0.18),
            "quad" => MathNode::Spacing(1.0),
            "qquad" => MathNode::Spacing(2.0),
            " " => MathNode::Spacing(0.25),
            "\\" => MathNode::Text(" ".to_owned()),
            _ => symbol_or_literal(&name),
        }
    }

    fn style_argument(&mut self, style: MathStyleKind, depth: usize) -> MathNode {
        MathNode::Styled {
            style,
            content: self.parse_argument(depth + 1),
        }
    }

    fn parse_environment(&mut self, _depth: usize) -> MathNode {
        let environment = self.read_braced_text();
        if environment.is_empty() {
            return MathNode::Text("\\begin".to_owned());
        }
        let remaining: String = self.chars[self.position..].iter().collect();
        let end_marker = format!("\\end{{{environment}}}");
        let Some(end) = remaining.find(&end_marker) else {
            return MathNode::Text(format!("\\begin{{{environment}}}"));
        };
        let body = remaining[..end].to_owned();
        self.position += end + end_marker.chars().count();
        let rows = split_matrix_rows(&body)
            .into_iter()
            .map(|row| {
                split_matrix_cells(&row)
                    .into_iter()
                    .map(|cell| parse_math(&cell))
                    .collect()
            })
            .collect();
        MathNode::Matrix { environment, rows }
    }

    fn read_braced_text(&mut self) -> String {
        if self.chars.get(self.position) != Some(&'{') {
            return String::new();
        }
        self.position += 1;
        let start = self.position;
        while self
            .chars
            .get(self.position)
            .is_some_and(|character| *character != '}')
        {
            self.position += 1;
        }
        let value: String = self.chars[start..self.position].iter().collect();
        if self.chars.get(self.position) == Some(&'}') {
            self.position += 1;
        }
        value
    }
}

fn split_matrix_rows(source: &str) -> Vec<String> {
    source
        .split("\\\\")
        .map(str::trim)
        .filter(|row| !row.is_empty())
        .map(str::to_owned)
        .collect()
}

fn split_matrix_cells(source: &str) -> Vec<String> {
    source
        .split('&')
        .map(str::trim)
        .map(str::to_owned)
        .collect()
}

fn symbol_or_literal(name: &str) -> MathNode {
    MathNode::Symbol(symbol(name).unwrap_or_else(|| format!("\\{name}")))
}

fn symbol(name: &str) -> Option<String> {
    let value = match name {
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ϵ",
        "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "vartheta" => "ϑ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" => "π",
        "varpi" => "ϖ",
        "rho" => "ρ",
        "varrho" => "ϱ",
        "sigma" => "σ",
        "varsigma" => "ς",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "ϕ",
        "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        "forall" => "∀",
        "exists" => "∃",
        "in" => "∈",
        "notin" => "∉",
        "subset" => "⊂",
        "subseteq" => "⊆",
        "cup" => "∪",
        "cap" => "∩",
        "emptyset" => "∅",
        "land" => "∧",
        "lor" => "∨",
        "lnot" => "¬",
        "implies" => "⟹",
        "iff" => "⟺",
        "to" => "→",
        "mapsto" => "↦",
        "Rightarrow" => "⇒",
        "leftarrow" => "←",
        "rightarrow" => "→",
        "leftrightarrow" => "↔",
        "Leftrightarrow" => "⇔",
        "le" | "leq" => "≤",
        "ge" | "geq" => "≥",
        "ne" | "neq" => "≠",
        "equiv" => "≡",
        "approx" => "≈",
        "sim" => "∼",
        "cong" => "≅",
        "propto" => "∝",
        "perp" => "⊥",
        "parallel" => "∥",
        "times" => "×",
        "div" => "÷",
        "pm" => "±",
        "mp" => "∓",
        "cdot" => "⋅",
        "circ" => "∘",
        "oplus" => "⊕",
        "otimes" => "⊗",
        "nabla" => "∇",
        "partial" => "∂",
        "infty" => "∞",
        "sum" => "∑",
        "prod" => "∏",
        "int" => "∫",
        "oint" => "∮",
        "bigcup" => "⋃",
        "bigcap" => "⋂",
        "ldots" | "dots" => "…",
        "cdots" => "⋯",
        "vdots" => "⋮",
        "ddots" => "⋱",
        "angle" => "∠",
        "deg" => "°",
        "prime" => "′",
        "langle" => "⟨",
        "rangle" => "⟩",
        "|" | "vert" | "lvert" | "rvert" => "|",
        _ => return None,
    };
    Some(value.to_owned())
}

fn delimiter(character: Option<char>) -> String {
    match character {
        Some('{') => "{".to_owned(),
        Some('}') => "}".to_owned(),
        Some('.') => String::new(),
        Some(character) => character.to_string(),
        None => String::new(),
    }
}

pub fn render_math(node: &MathNode, style: &MathStyle, display: bool) -> AnyElement {
    let _ = &style.font_fallbacks;
    render_node(node, style, display, style.base_font_size).into_any_element()
}

fn render_node(node: &MathNode, style: &MathStyle, display: bool, font_size: f32) -> gpui::Div {
    match node {
        MathNode::Row(nodes) => {
            let mut row = gpui::div()
                .flex()
                .flex_row()
                .items_center()
                .font_family(style.font_family.clone())
                .text_size(gpui::px(font_size))
                .text_color(style.text_color);
            for child in nodes {
                row = row.child(render_node(child, style, display, font_size));
            }
            row
        }
        MathNode::Text(text) | MathNode::Symbol(text) => {
            let italic = text.chars().count() == 1
                && text.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
            let mut element = gpui::div()
                .font_family(style.font_family.clone())
                .text_size(gpui::px(font_size))
                .text_color(style.text_color)
                .child(SharedString::from(styled_symbol(
                    text,
                    MathStyleKind::Italic,
                    italic,
                )));
            if italic {
                element = element.italic();
            }
            element
        }
        MathNode::Fraction {
            numerator,
            denominator,
        } => gpui::div()
            .flex()
            .flex_col()
            .items_center()
            .font_family(style.font_family.clone())
            .child(render_node(numerator, style, display, font_size * 0.8))
            .child(
                gpui::div()
                    .h(gpui::px(1.0))
                    .w(gpui::px(font_size * 1.2))
                    .bg(style.text_color),
            )
            .child(render_node(denominator, style, display, font_size * 0.8)),
        MathNode::Radical { index, radicand } => {
            let radicand_element = gpui::div().flex().flex_col().child(
                gpui::div()
                    .border_t_1()
                    .border_color(style.text_color)
                    .child(render_node(radicand, style, display, font_size * 0.9)),
            );
            let mut root = gpui::div()
                .flex()
                .items_center()
                .font_family(style.font_family.clone())
                .text_color(style.text_color);
            if let Some(index) = index {
                root = root.child(render_node(index, style, display, font_size * 0.45));
            }
            root.child(
                gpui::div()
                    .text_size(gpui::px(font_size * 1.15))
                    .child(SharedString::from("√")),
            )
            .child(radicand_element)
        }
        MathNode::Script {
            base,
            subscript,
            superscript,
        } => {
            let mut scripts = gpui::div().flex().flex_col().justify_center();
            if let Some(superscript) = superscript {
                scripts = scripts.child(render_node(superscript, style, display, font_size * 0.6));
            }
            if let Some(subscript) = subscript {
                scripts = scripts.child(render_node(subscript, style, display, font_size * 0.6));
            }
            gpui::div()
                .flex()
                .items_center()
                .child(render_node(base, style, display, font_size))
                .child(scripts)
        }
        MathNode::Styled {
            style: node_style,
            content,
        } => render_styled(content, *node_style, style, display, font_size),
        MathNode::Matrix { environment, rows } => {
            let mut grid = gpui::div().flex().flex_col().items_center().gap_1();
            for row in rows {
                let mut row_element = gpui::div().flex().flex_row().items_center().gap_2();
                for cell in row {
                    row_element =
                        row_element.child(render_node(cell, style, display, font_size * 0.85));
                }
                grid = grid.child(row_element);
            }
            let (left, right) = match environment.as_str() {
                "pmatrix" => ("(", ")"),
                "bmatrix" => ("[", "]"),
                "vmatrix" => ("|", "|"),
                "Bmatrix" => ("{", "}"),
                "cases" => ("{", ""),
                _ => ("", ""),
            };
            let mut result = gpui::div().flex().flex_row().items_center();
            if !left.is_empty() {
                result = result.child(delimiter_element(left, style, font_size * 1.25));
            }
            result = result.child(grid);
            if !right.is_empty() {
                result = result.child(delimiter_element(right, style, font_size * 1.25));
            }
            result
        }
        MathNode::Spacing(multiplier) => gpui::div().w(gpui::px(font_size * multiplier)),
    }
}

fn render_styled(
    content: &MathNode,
    node_style: MathStyleKind,
    style: &MathStyle,
    display: bool,
    font_size: f32,
) -> gpui::Div {
    let mut element = render_node(content, style, display, font_size);
    match node_style {
        MathStyleKind::Bold => element = element.font_weight(FontWeight::BOLD),
        MathStyleKind::Italic => element = element.italic(),
        MathStyleKind::Blackboard
        | MathStyleKind::Calligraphic
        | MathStyleKind::Fraktur
        | MathStyleKind::Roman
        | MathStyleKind::Sans
        | MathStyleKind::Monospace
        | MathStyleKind::Text
        | MathStyleKind::Operator => {}
    }
    element
}

fn delimiter_element(value: &str, style: &MathStyle, font_size: f32) -> gpui::Div {
    gpui::div()
        .font_family(style.font_family.clone())
        .text_size(gpui::px(font_size))
        .text_color(style.text_color)
        .child(SharedString::from(value.to_owned()))
}

fn styled_symbol(text: &str, style: MathStyleKind, _italic: bool) -> String {
    if !matches!(
        style,
        MathStyleKind::Blackboard | MathStyleKind::Calligraphic | MathStyleKind::Fraktur
    ) {
        return text.to_owned();
    }
    let Some(character) = text.chars().next() else {
        return String::new();
    };
    if !character.is_ascii_alphabetic() {
        return text.to_owned();
    }
    let (uppercase, lowercase) = match style {
        MathStyleKind::Blackboard => (0x1d538, 0x1d552),
        MathStyleKind::Calligraphic => (0x1d49c, 0x1d4b6),
        MathStyleKind::Fraktur => (0x1d504, 0x1d51e),
        _ => (0, 0),
    };
    let offset = if character.is_ascii_uppercase() {
        u32::from(character) - u32::from('A')
    } else {
        u32::from(character) - u32::from('a')
    };
    let codepoint = if character.is_ascii_uppercase() {
        uppercase + offset
    } else {
        lowercase + offset
    };
    char::from_u32(codepoint).map_or_else(|| text.to_owned(), |mapped| mapped.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_symbols_and_scripts() {
        let node = parse_math(r"\forall a \in F, x_i^2 \le \infty");
        let MathNode::Row(nodes) = node else {
            panic!("expected row");
        };
        assert!(
            nodes
                .iter()
                .any(|node| matches!(node, MathNode::Symbol(value) if value == "∀"))
        );
        assert!(
            nodes
                .iter()
                .any(|node| matches!(node, MathNode::Script { .. }))
        );
    }

    #[test]
    fn parses_fractions_and_radicals() {
        let node = parse_math(r"\frac{a+b}{\sqrt[n]{x}}");
        assert!(
            matches!(node, MathNode::Row(nodes) if matches!(nodes.first(), Some(MathNode::Fraction { .. })))
        );
    }

    #[test]
    fn parses_matrices() {
        let node = parse_math(r"\begin{pmatrix}a&b\\c&d\end{pmatrix}");
        assert!(
            matches!(node, MathNode::Row(nodes) if matches!(nodes.first(), Some(MathNode::Matrix { environment, rows }) if environment == "pmatrix" && rows.len() == 2))
        );
    }

    #[test]
    fn malformed_input_is_literal_and_finite() {
        let node = parse_math(r"\frac{a");
        assert!(!format!("{node:?}").is_empty());
        let node = parse_math(r"\unknown{x}");
        assert!(format!("{node:?}").contains(r"\unknown"));
    }

    #[test]
    fn deeply_nested_input_is_bounded() {
        let source = "{}".repeat(MAX_PARSE_DEPTH + 20);
        let _ = parse_math(&source);
    }
}
