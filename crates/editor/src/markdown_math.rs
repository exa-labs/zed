use gpui::{
    AnyElement, FontWeight, Hsla, IntoElement, ParentElement, SharedString, Styled,
    prelude::FluentBuilder,
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
    Delimited {
        left: String,
        content: Box<MathNode>,
        right: String,
        scale: DelimiterScale,
    },
    SizedDelimiter {
        value: String,
        scale: DelimiterScale,
    },
    Spacing(f32),
    LineBreak,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DelimiterScale {
    Normal,
    Big,
    Bigg,
    Biggg,
    Bigggg,
    Content,
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
        if let Some(character) = self.chars.get(self.position).copied() {
            if !character.is_ascii_alphabetic() {
                self.position += 1;
                return match character {
                    ',' => MathNode::Spacing(0.17),
                    ':' => MathNode::Spacing(0.22),
                    ';' => MathNode::Spacing(0.28),
                    '!' => MathNode::Spacing(-0.18),
                    ' ' => MathNode::Spacing(0.25),
                    '\\' => MathNode::LineBreak,
                    '{' => MathNode::SizedDelimiter {
                        value: "{".to_owned(),
                        scale: DelimiterScale::Normal,
                    },
                    '}' => MathNode::SizedDelimiter {
                        value: "}".to_owned(),
                        scale: DelimiterScale::Normal,
                    },
                    '|' => MathNode::SizedDelimiter {
                        value: "|".to_owned(),
                        scale: DelimiterScale::Normal,
                    },
                    _ => MathNode::Text(character.to_string()),
                };
            }
        }
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
            "left" => self.parse_delimited(depth, DelimiterScale::Content),
            "right" => MathNode::SizedDelimiter {
                value: self.read_delimiter(),
                scale: DelimiterScale::Content,
            },
            "big" | "bigl" | "bigr" => MathNode::SizedDelimiter {
                value: self.read_delimiter(),
                scale: DelimiterScale::Big,
            },
            "Big" | "Bigl" | "Bigr" => MathNode::SizedDelimiter {
                value: self.read_delimiter(),
                scale: DelimiterScale::Bigg,
            },
            "bigg" | "biggl" | "biggr" => MathNode::SizedDelimiter {
                value: self.read_delimiter(),
                scale: DelimiterScale::Biggg,
            },
            "Bigg" | "Biggl" | "Biggr" => MathNode::SizedDelimiter {
                value: self.read_delimiter(),
                scale: DelimiterScale::Bigggg,
            },
            "mathbb" => self.style_argument(MathStyleKind::Blackboard, depth),
            "mathcal" => self.style_argument(MathStyleKind::Calligraphic, depth),
            "mathfrak" => self.style_argument(MathStyleKind::Fraktur, depth),
            "mathbf" => self.style_argument(MathStyleKind::Bold, depth),
            "mathrm" => self.style_argument(MathStyleKind::Roman, depth),
            "mathit" => self.style_argument(MathStyleKind::Italic, depth),
            "mathsf" => self.style_argument(MathStyleKind::Sans, depth),
            "mathtt" => self.style_argument(MathStyleKind::Monospace, depth),
            "text" | "textbf" => self.text_style_argument(
                if name == "text" {
                    MathStyleKind::Text
                } else {
                    MathStyleKind::Bold
                },
                depth,
            ),
            "operator" | "operatorname" => self.text_style_argument(MathStyleKind::Operator, depth),
            "quad" => MathNode::Spacing(1.0),
            "qquad" => MathNode::Spacing(2.0),
            "sin" | "cos" | "tan" | "cot" | "sec" | "csc" | "arcsin" | "arccos" | "arctan"
            | "sinh" | "cosh" | "tanh" | "log" | "ln" | "exp" | "lim" | "limsup" | "liminf"
            | "sup" | "inf" | "max" | "min" | "gcd" | "det" | "dim" | "ker" | "arg" | "Pr"
            | "deg" => MathNode::Styled {
                style: MathStyleKind::Operator,
                content: Box::new(MathNode::Text(name)),
            },
            _ => symbol_or_literal(&name),
        }
    }

    fn style_argument(&mut self, style: MathStyleKind, depth: usize) -> MathNode {
        MathNode::Styled {
            style,
            content: self.parse_argument(depth + 1),
        }
    }

    fn text_style_argument(&mut self, style: MathStyleKind, depth: usize) -> MathNode {
        MathNode::Styled {
            style,
            content: self.parse_text_argument(depth + 1),
        }
    }

    fn parse_text_argument(&mut self, depth: usize) -> Box<MathNode> {
        if depth >= MAX_PARSE_DEPTH || self.chars.get(self.position) != Some(&'{') {
            return Box::new(MathNode::Text(String::new()));
        }
        self.position += 1;
        let start = self.position;
        let mut nesting = 1;
        while let Some(character) = self.chars.get(self.position).copied() {
            self.position += 1;
            match character {
                '{' => nesting += 1,
                '}' => {
                    nesting -= 1;
                    if nesting == 0 {
                        let value: String = self.chars[start..self.position - 1].iter().collect();
                        return Box::new(MathNode::Text(value));
                    }
                }
                _ => {}
            }
        }
        Box::new(MathNode::Text(
            self.chars[start..self.position].iter().collect(),
        ))
    }

    fn parse_delimited(&mut self, depth: usize, scale: DelimiterScale) -> MathNode {
        let left = self.read_delimiter();
        let mut nodes = Vec::new();
        while self.position < self.chars.len() {
            if self.is_command("right") {
                self.consume_command_name();
                let right = self.read_delimiter();
                return MathNode::Delimited {
                    left,
                    content: Box::new(MathNode::Row(nodes)),
                    right,
                    scale,
                };
            }
            if self.chars.get(self.position) == Some(&'}') {
                break;
            }
            if self
                .chars
                .get(self.position)
                .is_some_and(|character| character.is_whitespace())
            {
                self.position += 1;
                continue;
            }
            let mut node = self.parse_atom(depth + 1);
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
        MathNode::Delimited {
            left,
            content: Box::new(MathNode::Row(nodes)),
            right: String::new(),
            scale,
        }
    }

    fn is_command(&self, name: &str) -> bool {
        let Some(command) = self
            .chars
            .get(self.position..self.position + name.len() + 1)
        else {
            return false;
        };
        command.first() == Some(&'\\')
            && command[1..]
                .iter()
                .take(name.len())
                .copied()
                .eq(name.chars())
            && command
                .get(name.len() + 1)
                .is_some_and(|character| !character.is_ascii_alphabetic())
    }

    fn consume_command_name(&mut self) {
        if self.chars.get(self.position) == Some(&'\\') {
            self.position += 1;
            while self
                .chars
                .get(self.position)
                .is_some_and(|character| character.is_ascii_alphabetic())
            {
                self.position += 1;
            }
        }
    }

    fn read_delimiter(&mut self) -> String {
        if self.chars.get(self.position) == Some(&'\\') {
            self.position += 1;
            if let Some(character) = self.chars.get(self.position).copied()
                && !character.is_ascii_alphabetic()
            {
                self.position += 1;
                return delimiter(Some(character));
            }
            let start = self.position;
            while self
                .chars
                .get(self.position)
                .is_some_and(|character| character.is_ascii_alphabetic())
            {
                self.position += 1;
            }
            let name: String = self.chars[start..self.position].iter().collect();
            return match name.as_str() {
                "langle" => "⟨".to_owned(),
                "rangle" => "⟩".to_owned(),
                "lfloor" => "⌊".to_owned(),
                "rfloor" => "⌋".to_owned(),
                "lceil" => "⌈".to_owned(),
                "rceil" => "⌉".to_owned(),
                "vert" | "lvert" | "rvert" => "|".to_owned(),
                _ => format!("\\{name}"),
            };
        }
        let character = self.chars.get(self.position).copied();
        if character.is_some() {
            self.position += 1;
        }
        delimiter(character)
    }

    fn parse_environment(&mut self, _depth: usize) -> MathNode {
        let environment = self.read_braced_text();
        if environment.is_empty() {
            return MathNode::Text("\\begin".to_owned());
        }
        let end_marker = format!("\\end{{{environment}}}");
        let marker: Vec<char> = end_marker.chars().collect();
        let Some(relative_end) = self.chars[self.position..]
            .windows(marker.len())
            .position(|window| window == marker.as_slice())
        else {
            return MathNode::Text(format!("\\begin{{{environment}}}"));
        };
        let end = self.position + relative_end;
        let body: String = self.chars[self.position..end].iter().collect();
        self.position = end + marker.len();
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
        "degree" => "°",
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
    render_node(
        node,
        style,
        display,
        style.base_font_size,
        MathStyleKind::Italic,
    )
    .into_any_element()
}

pub fn estimate_math_height(node: &MathNode, base_font_size: f32) -> f32 {
    fn height(node: &MathNode, base_font_size: f32) -> f32 {
        match node {
            MathNode::Row(nodes) => nodes
                .iter()
                .map(|node| height(node, base_font_size))
                .fold(base_font_size, f32::max),
            MathNode::Fraction {
                numerator,
                denominator,
            } => {
                height(numerator, base_font_size * 0.8)
                    + height(denominator, base_font_size * 0.8)
                    + base_font_size * 0.35
            }
            MathNode::Radical { index, radicand } => {
                height(radicand, base_font_size * 0.9)
                    + if index.is_some() {
                        base_font_size * 0.3
                    } else {
                        0.0
                    }
            }
            MathNode::Script {
                base,
                subscript,
                superscript,
            } => {
                height(base, base_font_size)
                    + superscript
                        .as_deref()
                        .map_or(0.0, |node| height(node, base_font_size * 0.6))
                    + subscript
                        .as_deref()
                        .map_or(0.0, |node| height(node, base_font_size * 0.6))
            }
            MathNode::Styled { content, .. } => height(content, base_font_size),
            MathNode::Matrix { rows, .. } => rows.len().max(1) as f32 * base_font_size * 1.25,
            MathNode::Delimited { content, .. } => height(content, base_font_size),
            MathNode::SizedDelimiter { .. }
            | MathNode::Spacing(_)
            | MathNode::LineBreak
            | MathNode::Text(_)
            | MathNode::Symbol(_) => base_font_size,
        }
    }
    height(node, base_font_size)
}

pub fn estimate_math_lines(node: &MathNode) -> u32 {
    fn lines(node: &MathNode) -> u32 {
        match node {
            MathNode::Row(nodes) => nodes.iter().map(lines).max().unwrap_or(1),
            MathNode::Fraction {
                numerator,
                denominator,
            } => lines(numerator) + lines(denominator) + 1,
            MathNode::Radical { radicand, index } => lines(radicand) + u32::from(index.is_some()),
            MathNode::Script {
                base,
                subscript,
                superscript,
            } => {
                let base_lines = lines(base);
                let script_lines =
                    subscript.as_deref().map_or(0, lines) + superscript.as_deref().map_or(0, lines);
                base_lines.max(script_lines + 1)
            }
            MathNode::Styled { content, .. } | MathNode::Delimited { content, .. } => {
                lines(content)
            }
            MathNode::Matrix { rows, .. } => rows.len().max(1) as u32,
            MathNode::SizedDelimiter { .. }
            | MathNode::Spacing(_)
            | MathNode::LineBreak
            | MathNode::Text(_)
            | MathNode::Symbol(_) => 1,
        }
    }
    lines(node)
}

fn render_node(
    node: &MathNode,
    style: &MathStyle,
    display: bool,
    font_size: f32,
    node_style: MathStyleKind,
) -> gpui::Div {
    match node {
        MathNode::Row(nodes) => {
            let mut row = gpui::div()
                .flex()
                .flex_row()
                .items_center()
                .font_family(style.font_family.clone())
                .text_size(gpui::px(font_size))
                .text_color(style.text_color);
            for (index, child) in nodes.iter().enumerate() {
                if index > 0 {
                    row = row.child(render_spacing(
                        node_spacing(nodes[index - 1].clone(), child),
                        style,
                        font_size,
                    ));
                }
                row = row.child(render_node(child, style, display, font_size, node_style));
            }
            row
        }
        MathNode::Text(text) | MathNode::Symbol(text) => {
            let italic = node_style == MathStyleKind::Italic
                && text.chars().count() == 1
                && text.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
            let mut element = gpui::div()
                .font_family(style.font_family.clone())
                .text_size(gpui::px(font_size))
                .text_color(style.text_color)
                .child(SharedString::from(styled_symbol(text, node_style, italic)));
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
            .child(render_node(
                numerator,
                style,
                display,
                font_size * 0.8,
                node_style,
            ))
            .child(
                gpui::div()
                    .h(gpui::px(1.0))
                    .w(gpui::px(
                        estimate_math_width(numerator, font_size * 0.8)
                            .max(estimate_math_width(denominator, font_size * 0.8)),
                    ))
                    .bg(style.text_color),
            )
            .child(render_node(
                denominator,
                style,
                display,
                font_size * 0.8,
                node_style,
            )),
        MathNode::Radical { index, radicand } => {
            let radicand_element = gpui::div().flex().flex_col().child(
                gpui::div()
                    .border_t_1()
                    .border_color(style.text_color)
                    .child(render_node(
                        radicand,
                        style,
                        display,
                        font_size * 0.9,
                        node_style,
                    )),
            );
            let mut root = gpui::div()
                .flex()
                .items_center()
                .font_family(style.font_family.clone())
                .text_color(style.text_color);
            if let Some(index) = index {
                root = root.child(
                    render_node(index, style, display, font_size * 0.45, node_style)
                        .relative()
                        .top(gpui::px(-font_size * 0.45)),
                );
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
            let base_size = if display && is_big_operator(base) {
                font_size * 1.6
            } else {
                font_size
            };
            if display && is_big_operator(base) {
                let mut limits = gpui::div().flex().flex_col().items_center();
                if let Some(superscript) = superscript {
                    limits = limits.child(render_node(
                        superscript,
                        style,
                        display,
                        font_size * 0.6,
                        node_style,
                    ));
                }
                limits = limits.child(render_node(base, style, display, base_size, node_style));
                if let Some(subscript) = subscript {
                    limits = limits.child(render_node(
                        subscript,
                        style,
                        display,
                        font_size * 0.6,
                        node_style,
                    ));
                }
                limits
            } else {
                let mut result = gpui::div()
                    .flex()
                    .items_center()
                    .child(render_node(base, style, display, base_size, node_style));
                if let Some(superscript) = superscript {
                    result = result.child(
                        render_node(superscript, style, display, font_size * 0.6, node_style)
                            .relative()
                            .top(gpui::px(-font_size * 0.35)),
                    );
                }
                if let Some(subscript) = subscript {
                    result = result.child(
                        render_node(subscript, style, display, font_size * 0.6, node_style)
                            .relative()
                            .top(gpui::px(font_size * 0.35)),
                    );
                }
                result
            }
        }
        MathNode::Styled {
            style: node_style,
            content,
        } => render_styled(content, *node_style, style, display, font_size),
        MathNode::Matrix { environment, rows } => {
            let aligned_left = matches!(
                environment.as_str(),
                "cases" | "aligned" | "align" | "array"
            );
            let mut grid = gpui::div()
                .flex()
                .flex_col()
                .when(aligned_left, |element| element.items_start())
                .when(!aligned_left, |element| element.items_center())
                .gap_1();
            for row in rows {
                let mut row_element = gpui::div().flex().flex_row().items_center().gap_2();
                for cell in row {
                    row_element = row_element.child(render_node(
                        cell,
                        style,
                        display,
                        font_size * 0.85,
                        node_style,
                    ));
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
        MathNode::Delimited {
            left,
            content,
            right,
            scale,
        } => {
            let content_height = estimate_math_height(content, font_size);
            let delimiter_size = delimiter_scale(*scale, font_size, content_height);
            gpui::div()
                .flex()
                .items_center()
                .child(delimiter_element(left, style, delimiter_size))
                .child(render_node(content, style, display, font_size, node_style))
                .child(delimiter_element(right, style, delimiter_size))
        }
        MathNode::SizedDelimiter { value, scale } => {
            delimiter_element(value, style, delimiter_scale(*scale, font_size, font_size))
        }
        MathNode::LineBreak => gpui::div().h(gpui::px(font_size * 1.2)),
    }
}

fn render_styled(
    content: &MathNode,
    node_style: MathStyleKind,
    style: &MathStyle,
    display: bool,
    font_size: f32,
) -> gpui::Div {
    let mut element = render_node(content, style, display, font_size, node_style);
    let family = match node_style {
        MathStyleKind::Sans => SharedString::new_static("sans-serif"),
        MathStyleKind::Monospace => SharedString::new_static("monospace"),
        _ => style.font_family.clone(),
    };
    element = element.font_family(family);
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

fn delimiter_scale(scale: DelimiterScale, font_size: f32, content_height: f32) -> f32 {
    match scale {
        DelimiterScale::Normal => font_size,
        DelimiterScale::Big => font_size * 1.2,
        DelimiterScale::Bigg => font_size * 1.5,
        DelimiterScale::Biggg => font_size * 1.8,
        DelimiterScale::Bigggg => font_size * 2.1,
        DelimiterScale::Content => content_height.max(font_size * 1.2),
    }
}

fn is_big_operator(node: &MathNode) -> bool {
    match node {
        MathNode::Symbol(value) => matches!(value.as_str(), "∑" | "∏" | "∫" | "∮" | "⋃" | "⋂"),
        MathNode::Styled { content, style } => {
            if *style != MathStyleKind::Operator {
                return false;
            }
            match content.as_ref() {
                MathNode::Text(value) => matches!(value.as_str(), "lim" | "limsup" | "liminf"),
                _ => is_big_operator(content),
            }
        }
        _ => false,
    }
}

fn estimate_math_width(node: &MathNode, font_size: f32) -> f32 {
    match node {
        MathNode::Row(nodes) => nodes
            .iter()
            .map(|node| estimate_math_width(node, font_size))
            .sum::<f32>()
            .max(font_size * 0.5),
        MathNode::Text(text) | MathNode::Symbol(text) => {
            text.chars().count() as f32 * font_size * 0.6
        }
        MathNode::Fraction {
            numerator,
            denominator,
        } => {
            estimate_math_width(numerator, font_size * 0.8)
                .max(estimate_math_width(denominator, font_size * 0.8))
                + font_size * 0.3
        }
        MathNode::Radical { radicand, .. } => {
            estimate_math_width(radicand, font_size * 0.9) + font_size * 0.8
        }
        MathNode::Script {
            base,
            superscript,
            subscript,
        } => {
            estimate_math_width(base, font_size)
                + superscript
                    .as_deref()
                    .map_or(0.0, |node| estimate_math_width(node, font_size * 0.6))
                    .max(
                        subscript
                            .as_deref()
                            .map_or(0.0, |node| estimate_math_width(node, font_size * 0.6)),
                    )
        }
        MathNode::Styled { content, .. } | MathNode::Delimited { content, .. } => {
            estimate_math_width(content, font_size)
        }
        MathNode::Matrix { rows, .. } => {
            let mut width = font_size;
            for row in rows {
                let row_width = row
                    .iter()
                    .map(|cell| estimate_math_width(cell, font_size * 0.85))
                    .sum::<f32>();
                width = width.max(row_width);
            }
            width
        }
        MathNode::SizedDelimiter { .. } => font_size,
        MathNode::Spacing(multiplier) => font_size * multiplier.max(0.0),
        MathNode::LineBreak => 0.0,
    }
}

fn node_spacing(previous: MathNode, current: &MathNode) -> f32 {
    if matches!(previous, MathNode::Symbol(ref value) if matches!(
        value.as_str(),
        "+" | "-" | "×" | "÷" | "±" | "∓" | "⋅" | "∘" | "⊕" | "⊗"
    )) {
        return 0.2;
    }
    if matches!(current, MathNode::Symbol(value) if matches!(
        value.as_str(),
        "+" | "-" | "×" | "÷" | "±" | "∓" | "⋅" | "∘" | "⊕" | "⊗"
    )) {
        return 0.2;
    }
    if matches!(previous, MathNode::Symbol(ref value) if matches!(
        value.as_str(),
        "=" | "≤" | "≥" | "≠" | "≡" | "≈" | "∼" | "≅" | "∝" | "⊥" | "∥" | "∈"
            | "∉" | "⊂" | "⊆" | "→" | "↦" | "⇒" | "⟹" | "⟺"
    )) || matches!(current, MathNode::Symbol(value) if matches!(
        value.as_str(),
        "=" | "≤" | "≥" | "≠" | "≡" | "≈" | "∼" | "≅" | "∝" | "⊥" | "∥" | "∈"
            | "∉" | "⊂" | "⊆" | "→" | "↦" | "⇒" | "⟹" | "⟺"
    )) {
        return 0.28;
    }
    if matches!(previous, MathNode::Text(ref value) if value.ends_with(',') || value.ends_with(';'))
    {
        return 0.25;
    }
    0.0
}

fn render_spacing(multiplier: f32, style: &MathStyle, font_size: f32) -> gpui::Div {
    gpui::div()
        .font_family(style.font_family.clone())
        .w(gpui::px(font_size * multiplier))
}

fn delimiter_element(value: &str, style: &MathStyle, font_size: f32) -> gpui::Div {
    gpui::div()
        .font_family(style.font_family.clone())
        .text_size(gpui::px(font_size))
        .text_color(style.text_color)
        .child(SharedString::from(value.to_owned()))
}

pub(crate) fn styled_symbol(text: &str, style: MathStyleKind, _italic: bool) -> String {
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
    let special = match (style, character) {
        (MathStyleKind::Blackboard, 'C') => Some('ℂ'),
        (MathStyleKind::Blackboard, 'H') => Some('ℍ'),
        (MathStyleKind::Blackboard, 'N') => Some('ℕ'),
        (MathStyleKind::Blackboard, 'P') => Some('ℙ'),
        (MathStyleKind::Blackboard, 'Q') => Some('ℚ'),
        (MathStyleKind::Blackboard, 'R') => Some('ℝ'),
        (MathStyleKind::Blackboard, 'Z') => Some('ℤ'),
        (MathStyleKind::Calligraphic, 'B') => Some('ℬ'),
        (MathStyleKind::Calligraphic, 'E') => Some('ℰ'),
        (MathStyleKind::Calligraphic, 'F') => Some('ℱ'),
        (MathStyleKind::Calligraphic, 'H') => Some('ℋ'),
        (MathStyleKind::Calligraphic, 'I') => Some('ℐ'),
        (MathStyleKind::Calligraphic, 'L') => Some('ℒ'),
        (MathStyleKind::Calligraphic, 'M') => Some('ℳ'),
        (MathStyleKind::Calligraphic, 'R') => Some('ℛ'),
        (MathStyleKind::Calligraphic, 'Z') => Some('ℨ'),
        (MathStyleKind::Fraktur, 'C') => Some('ℭ'),
        (MathStyleKind::Fraktur, 'H') => Some('ℌ'),
        (MathStyleKind::Fraktur, 'I') => Some('ℑ'),
        (MathStyleKind::Fraktur, 'R') => Some('ℜ'),
        (MathStyleKind::Fraktur, 'Z') => Some('ℨ'),
        _ => None,
    };
    if let Some(special) = special {
        return special.to_string();
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
        let MathNode::Row(ref nodes) = node else {
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
    fn consumes_sized_delimiters_once() {
        let node = parse_math(r"\big(x\big)");
        assert_eq!(format!("{node:?}").matches("SizedDelimiter").count(), 2);
        assert!(!format!("{node:?}").contains("Text(\"(\")"));
    }

    #[test]
    fn parses_control_sequence_spacing() {
        let node = parse_math(r"a\,b\;c\!d");
        let MathNode::Row(ref nodes) = node else {
            panic!("expected row");
        };
        assert_eq!(
            nodes
                .iter()
                .filter(|node| matches!(node, MathNode::Spacing(_)))
                .count(),
            3
        );
    }

    #[test]
    fn parses_upright_operators_and_text_spaces() {
        let node = parse_math(r"\operatorname{lcm}\sin\text{if and only if}");
        let MathNode::Row(ref nodes) = node else {
            panic!("expected row");
        };
        assert!(nodes.iter().any(|node| {
            matches!(
                node,
                MathNode::Styled {
                    style: MathStyleKind::Operator,
                    content,
                } if matches!(content.as_ref(), MathNode::Text(value) if value == "lcm")
            )
        }));
        assert!(format!("{node:?}").contains("if and only if"));
    }

    #[test]
    fn maps_blackboard_symbols() {
        assert_eq!(styled_symbol("R", MathStyleKind::Blackboard, false), "ℝ");
    }

    #[test]
    fn scans_unicode_environment_bodies_without_swallowing_trailing_text() {
        let node = parse_math(r"\begin{matrix}\alpha & β\end{matrix}+z");
        let MathNode::Row(ref nodes) = node else {
            panic!("expected row");
        };
        assert!(matches!(nodes.first(), Some(MathNode::Matrix { .. })));
        assert!(format!("{node:?}").contains("z"));
    }

    #[test]
    fn parses_display_limits_as_scripts() {
        let node = parse_math(r"\sum_{i=1}^{n}");
        assert!(format!("{node:?}").contains("Script"));
        assert!(format!("{node:?}").contains("∑"));
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
