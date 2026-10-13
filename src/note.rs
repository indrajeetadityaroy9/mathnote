use std::fmt;
use std::ops::Range;

#[derive(Debug, Clone)]
pub struct MathSpan {
    span: Range<usize>,
    source: String,
    latex: String,
}

impl MathSpan {
    pub fn span(&self) -> Range<usize> {
        self.span.clone()
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn latex(&self) -> &str {
        &self.latex
    }
}

#[derive(Debug, Clone)]
pub enum NoteSegment {
    Text(String),
    Math(MathSpan),
}

#[derive(Debug, Clone)]
pub struct NoteParagraph {
    segments: Vec<NoteSegment>,
}

impl NoteParagraph {
    /// Parses one mixed note paragraph, which may span several lines.
    /// Ordinary language remains prose while complete, unambiguous
    /// mathematical phrases are detected and rendered. Automatically inferred
    /// expressions are limited to 256 source bytes; longer expressions must use
    /// explicit `$...$` delimiters.
    pub fn parse(input: &str) -> Result<Self, NoteError> {
        let mut segments = Vec::new();
        let mut cursor = 0;

        for explicit in explicit_math_ranges(input)? {
            append_automatic_segments(&mut segments, input, cursor..explicit.full.start)?;
            segments.push(NoteSegment::Math(parse_math_span(
                explicit.content.clone(),
                &input[explicit.content],
            )?));
            cursor = explicit.full.end;
        }

        if cursor < input.len() {
            append_automatic_segments(&mut segments, input, cursor..input.len())?;
        }

        Ok(Self { segments })
    }

    pub fn segments(&self) -> &[NoteSegment] {
        &self.segments
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteError {
    InvalidMath { byte: usize, message: String },
}

impl fmt::Display for NoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMath { byte, message } => {
                write!(formatter, "mathematics at byte {byte}: {message}")
            }
        }
    }
}

impl std::error::Error for NoteError {}

#[derive(Debug, Clone)]
struct Token {
    span: Range<usize>,
}

#[derive(Debug, Clone)]
struct ExplicitMathRange {
    full: Range<usize>,
    content: Range<usize>,
}

/// Pairs the unescaped `$` delimiters of `input`, rejecting an empty pair or an
/// unclosed trailing delimiter.
fn explicit_math_ranges(input: &str) -> Result<Vec<ExplicitMathRange>, NoteError> {
    let mut ranges = Vec::new();
    let mut opening = None;

    for (byte, character) in input.char_indices() {
        if character != '$' || is_escaped_delimiter(input, byte) {
            continue;
        }

        if let Some(opening_byte) = opening.take() {
            let content = opening_byte + 1..byte;
            if input[content.clone()].trim().is_empty() {
                return Err(NoteError::InvalidMath {
                    byte: opening_byte,
                    message: String::from("explicit mathematics cannot be empty"),
                });
            }
            ranges.push(ExplicitMathRange {
                full: opening_byte..byte + 1,
                content,
            });
        } else {
            opening = Some(byte);
        }
    }

    if let Some(byte) = opening {
        return Err(NoteError::InvalidMath {
            byte,
            message: String::from("unclosed inline mathematics delimiter"),
        });
    }

    Ok(ranges)
}

fn is_escaped_delimiter(input: &str, byte: usize) -> bool {
    input[..byte]
        .chars()
        .rev()
        .take_while(|character| *character == '\\')
        .count()
        % 2
        == 1
}

fn append_automatic_segments(
    segments: &mut Vec<NoteSegment>,
    input: &str,
    region: Range<usize>,
) -> Result<(), NoteError> {
    let source = &input[region.clone()];
    let mut cursor = 0;

    for local_range in discover_math_ranges(source) {
        if cursor < local_range.start {
            push_text_segment(segments, &source[cursor..local_range.start]);
        }

        let absolute = region.start + local_range.start..region.start + local_range.end;
        segments.push(NoteSegment::Math(parse_math_span(
            absolute,
            &source[local_range.clone()],
        )?));
        cursor = local_range.end;
    }

    if cursor < source.len() {
        push_text_segment(segments, &source[cursor..]);
    }

    Ok(())
}

fn push_text_segment(segments: &mut Vec<NoteSegment>, text: &str) {
    if text.is_empty() {
        return;
    }

    if let Some(NoteSegment::Text(previous)) = segments.last_mut() {
        previous.push_str(text);
    } else {
        segments.push(NoteSegment::Text(text.to_owned()));
    }
}

// This keeps heuristic range discovery bounded per token. Explicit inline and
// display mathematics bypass automatic discovery and are intentionally uncapped.
const MAX_AUTOMATIC_MATH_BYTES: usize = 256;

fn discover_math_ranges(input: &str) -> Vec<Range<usize>> {
    let clauses = tokenize_clauses(input);
    let mut ranges = Vec::new();

    for tokens in clauses {
        let clause = tokens[0].span.start..tokens[tokens.len() - 1].span.end;
        if !contains_math_trigger(&input[clause]) || has_ambiguous_root_scope(&tokens, input) {
            continue;
        }
        let mut start = 0;
        let mut bounded_end = 0;
        while start < tokens.len() {
            bounded_end = bounded_end.max(start);
            while bounded_end < tokens.len()
                && tokens[bounded_end].span.end - tokens[start].span.start
                    <= MAX_AUTOMATIC_MATH_BYTES
            {
                bounded_end += 1;
            }

            let mut selected = None;
            for end in (start + 1..=bounded_end).rev() {
                let range = tokens[start].span.start..tokens[end - 1].span.end;
                let candidate = &input[range.clone()];
                if is_complete_math_candidate(candidate)
                    && candidate_has_safe_end(&tokens, end, input)
                {
                    selected = Some((end, range));
                    break;
                }
            }

            if let Some((end, range)) = selected {
                ranges.push(range);
                start = end;
            } else {
                start += 1;
            }
        }
    }

    ranges
}

fn has_ambiguous_root_scope(tokens: &[Token], input: &str) -> bool {
    tokens.windows(5).any(|window| {
        input[window[0].span.clone()].eq_ignore_ascii_case("square")
            && input[window[1].span.clone()].eq_ignore_ascii_case("root")
            && input[window[2].span.clone()].eq_ignore_ascii_case("of")
            && is_continuation_operator(&input[window[4].span.clone()].to_ascii_lowercase())
    }) || tokens.windows(4).any(|window| {
        input[window[0].span.clone()].eq_ignore_ascii_case("root")
            && input[window[1].span.clone()].eq_ignore_ascii_case("of")
            && is_continuation_operator(&input[window[3].span.clone()].to_ascii_lowercase())
    })
}

fn tokenize_clauses(input: &str) -> Vec<Vec<Token>> {
    let mut clauses = Vec::new();
    let mut clause = Vec::new();
    let mut token_start = None;
    let mut characters = input.char_indices().peekable();

    while let Some((byte, character)) = characters.next() {
        let decimal_point = character == '.'
            && token_start.is_some()
            && characters
                .peek()
                .is_some_and(|(_, next)| next.is_ascii_digit());
        if is_math_token_character(character) || decimal_point {
            token_start.get_or_insert(byte);
            continue;
        }

        if let Some(start) = token_start.take() {
            clause.push(Token { span: start..byte });
        }

        if is_clause_boundary(character) && !clause.is_empty() {
            clauses.push(std::mem::take(&mut clause));
        }
    }

    if let Some(start) = token_start {
        clause.push(Token {
            span: start..input.len(),
        });
    }
    if !clause.is_empty() {
        clauses.push(clause);
    }

    clauses
}

fn is_math_token_character(character: char) -> bool {
    character.is_alphanumeric()
        || character == '_'
        || is_operator_symbol(character)
        || character == '∞'
}

fn is_operator_symbol(character: char) -> bool {
    matches!(
        character,
        '+' | '-' | '=' | '<' | '>' | '/' | '^' | '×' | '≤' | '≥' | '≠' | '±'
    )
}

fn is_operator_word(word: &str) -> bool {
    matches!(
        word,
        "plus" | "minus" | "times" | "over" | "equals" | "less" | "greater" | "not"
    )
}

const NUMBER_WORDS: [&str; 11] = [
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
];

const GREEK_NAMES: [&str; 14] = [
    "alpha", "beta", "gamma", "delta", "epsilon", "theta", "lambda", "mu", "pi", "rho", "sigma",
    "phi", "psi", "omega",
];

fn is_clause_boundary(character: char) -> bool {
    matches!(character, ',' | ';' | ':' | '.' | '!' | '?' | '\n')
}

fn candidate_has_safe_end(tokens: &[Token], end: usize, input: &str) -> bool {
    tokens.get(end).is_none_or(|token| {
        !is_continuation_operator(&input[token.span.clone()].to_ascii_lowercase())
    })
}

fn is_continuation_operator(word: &str) -> bool {
    is_operator_word(word)
        || matches!(
            word,
            "+" | "-" | "=" | "<" | ">" | "/" | "×" | "≤" | "≥" | "≠" | "±"
        )
}

fn is_complete_math_candidate(candidate: &str) -> bool {
    if !contains_math_trigger(candidate) || is_compact_numeric_range(candidate) {
        return false;
    }

    normalize_math(candidate).is_ok()
}

fn is_compact_numeric_range(candidate: &str) -> bool {
    !candidate.chars().any(char::is_whitespace)
        && candidate.matches('-').count() == 1
        && candidate
            .chars()
            .all(|character| character.is_ascii_digit() || character == '-')
}

fn contains_math_trigger(input: &str) -> bool {
    contains_natural_operator(input)
        || input
            .chars()
            .any(|character| is_operator_symbol(character) || character == '∞')
}

fn parse_math_span(span: Range<usize>, source: &str) -> Result<MathSpan, NoteError> {
    let source = source.to_owned();
    let cleaned = collapse_whitespace(&source);
    let latex = normalize_math(&cleaned).map_err(|message| NoteError::InvalidMath {
        byte: span.start,
        message,
    })?;
    Ok(MathSpan {
        span,
        source,
        latex,
    })
}

fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn normalize_math(source: &str) -> Result<String, String> {
    if contains_natural_operator(source) {
        if source
            .chars()
            .any(|character| matches!(character, '(' | ')' | '[' | ']' | '{' | '}'))
        {
            return Err(String::from(
                "grouped natural-language expressions are not supported",
            ));
        }
        return NaturalMathParser::new(source).parse();
    }

    if contains_symbolic_operator(source) {
        validate_symbolic_expression(source)?;
        return Ok(source.chars().fold(String::new(), |mut output, character| {
            if let Some(command) = unicode_math_command(character) {
                output.push_str(command);
            } else {
                output.push(character);
            }
            output
        }));
    }

    if source.trim() == "∞" {
        return Ok(String::from(r"\infty"));
    }

    let normalized = source
        .split_whitespace()
        .map(normalize_natural_atom)
        .collect::<Result<Vec<_>, _>>()?;
    if normalized.is_empty() {
        Err(String::from("not a complete mathematical expression"))
    } else {
        Ok(normalized.join(" "))
    }
}

fn contains_symbolic_operator(input: &str) -> bool {
    input.chars().any(is_operator_symbol)
}

fn validate_symbolic_expression(input: &str) -> Result<(), String> {
    let characters = input.chars().collect::<Vec<_>>();
    let mut position = 0;
    let mut expects_value = true;
    let mut operator_count = 0;

    while position < characters.len() {
        if characters[position].is_whitespace() {
            position += 1;
            continue;
        }

        if expects_value {
            if characters[position] == '-' {
                position += 1;
                continue;
            }
            let start = position;
            while position < characters.len()
                && (characters[position].is_alphanumeric()
                    || characters[position] == '_'
                    || characters[position] == '.')
            {
                position += 1;
            }
            if start == position {
                return Err(String::from("expected a mathematical value"));
            }
            let atom = characters[start..position].iter().collect::<String>();
            if atom.matches('.').count() > 1
                || (atom.contains('.')
                    && !atom
                        .chars()
                        .all(|character| character.is_ascii_digit() || character == '.'))
            {
                return Err(format!("invalid numeric value `{atom}`"));
            }
            normalize_natural_atom(&atom)?;
            expects_value = false;
            continue;
        }

        if is_operator_symbol(characters[position]) {
            operator_count += 1;
            position += 1;
            if position < characters.len()
                && matches!(
                    (characters[position - 1], characters[position]),
                    ('<', '=') | ('>', '=') | ('!', '=')
                )
            {
                position += 1;
            }
            expects_value = true;
        } else {
            return Err(String::from("expected a mathematical operator"));
        }
    }

    if expects_value || operator_count == 0 {
        Err(String::from("incomplete symbolic expression"))
    } else {
        Ok(())
    }
}

fn contains_natural_operator(input: &str) -> bool {
    input.split_whitespace().any(|word| {
        let word = word.to_ascii_lowercase();
        is_operator_word(&word)
            || matches!(
                word.as_str(),
                "squared" | "cubed" | "square" | "root" | "integral"
            )
    })
}

fn normalize_single_atom(atom: &str) -> String {
    let mut characters = atom.chars();
    if let (Some(character), None) = (characters.next(), characters.next())
        && let Some(command) = unicode_math_command(character)
    {
        return command.to_owned();
    }

    let lower = atom.to_ascii_lowercase();
    if let Some(value) = NUMBER_WORDS.iter().position(|word| *word == lower) {
        return value.to_string();
    }
    match lower.as_str() {
        word if GREEK_NAMES.contains(&word) => format!(r"\{word}"),
        "infinity" => String::from(r"\infty"),
        "sin" | "cos" | "tan" | "log" | "ln" | "exp" | "min" | "max" => format!(r"\{lower}"),
        _ => atom.to_string(),
    }
}

pub(crate) fn unicode_math_command(character: char) -> Option<&'static str> {
    Some(match character {
        'α' => r"\alpha",
        'β' => r"\beta",
        'γ' => r"\gamma",
        'δ' => r"\delta",
        'ε' => r"\epsilon",
        'ζ' => r"\zeta",
        'η' => r"\eta",
        'θ' => r"\theta",
        'ι' => r"\iota",
        'κ' => r"\kappa",
        'λ' => r"\lambda",
        'μ' => r"\mu",
        'ν' => r"\nu",
        'ξ' => r"\xi",
        'ο' => "o",
        'π' => r"\pi",
        'ρ' => r"\rho",
        'σ' | 'ς' => r"\sigma",
        'τ' => r"\tau",
        'υ' => r"\upsilon",
        'φ' => r"\phi",
        'χ' => r"\chi",
        'ψ' => r"\psi",
        'ω' => r"\omega",
        'Α' => "A",
        'Β' => "B",
        'Γ' => r"\Gamma",
        'Δ' => r"\Delta",
        'Ε' => "E",
        'Ζ' => "Z",
        'Η' => "H",
        'Θ' => r"\Theta",
        'Ι' => "I",
        'Κ' => "K",
        'Λ' => r"\Lambda",
        'Μ' => "M",
        'Ν' => "N",
        'Ξ' => r"\Xi",
        'Ο' => "O",
        'Π' => r"\Pi",
        'Ρ' => "P",
        'Σ' => r"\Sigma",
        'Τ' => "T",
        'Υ' => r"\Upsilon",
        'Φ' => r"\Phi",
        'Χ' => "X",
        'Ψ' => r"\Psi",
        'Ω' => r"\Omega",
        'ϵ' => r"\varepsilon",
        'ϑ' => r"\vartheta",
        'ϖ' => r"\varpi",
        'ϱ' => r"\varrho",
        '×' => r"\times{}",
        '≤' => r"\le{}",
        '≥' => r"\ge{}",
        '≠' => r"\ne{}",
        '±' => r"\pm{}",
        '∞' => r"\infty",
        _ => return None,
    })
}

fn normalize_natural_atom(atom: &str) -> Result<String, String> {
    let lower = atom.to_ascii_lowercase();
    let is_named_constant = NUMBER_WORDS.contains(&lower.as_str())
        || GREEK_NAMES.contains(&lower.as_str())
        || lower == "infinity";
    let is_number = atom.chars().all(|character| character.is_ascii_digit())
        || (atom.matches('.').count() == 1
            && atom
                .chars()
                .all(|character| character.is_ascii_digit() || character == '.')
            && atom.chars().any(|character| character.is_ascii_digit()));
    let mut characters = atom.chars();
    let is_variable = characters.next().is_some_and(|first| first.is_alphabetic())
        && characters.all(|character| character.is_ascii_digit() || character == '_')
        && atom
            .chars()
            .filter(|character| character.is_alphabetic())
            .count()
            == 1;

    if is_named_constant || is_number || is_variable {
        Ok(normalize_single_atom(atom))
    } else {
        Err(format!(
            "`{atom}` is prose, not an unambiguous mathematical value"
        ))
    }
}

struct NaturalMathParser<'a> {
    words: Vec<&'a str>,
    position: usize,
}

impl<'a> NaturalMathParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            words: input.split_whitespace().collect(),
            position: 0,
        }
    }

    fn parse(mut self) -> Result<String, String> {
        let expression = self.parse_relation()?;
        if self.position != self.words.len() {
            return Err(format!(
                "unsupported natural-language sequence near `{}`",
                self.words[self.position..].join(" ")
            ));
        }
        Ok(expression)
    }

    fn parse_relation(&mut self) -> Result<String, String> {
        let mut left = self.parse_sum()?;
        while let Some(operator) = self.consume_relation() {
            let right = self.parse_sum()?;
            left = format!("{left}{operator}{right}");
        }
        Ok(left)
    }

    fn parse_sum(&mut self) -> Result<String, String> {
        let mut left = self.parse_product()?;
        loop {
            let operator = if self.consume_phrase(&["plus", "or", "minus"]) {
                Some(r"\pm{}")
            } else if self.consume_word("plus") {
                Some("+")
            } else if self.consume_word("minus") {
                Some("-")
            } else {
                None
            };
            let Some(operator) = operator else {
                break;
            };
            let right = self.parse_product()?;
            left = format!("{left}{operator}{right}");
        }
        Ok(left)
    }

    fn parse_product(&mut self) -> Result<String, String> {
        let mut left = self.parse_power()?;
        loop {
            if self.consume_word("times") {
                let right = self.parse_power()?;
                left = format!(r"{left}\times {right}");
            } else if self.consume_word("over") {
                let right = self.parse_power()?;
                left = format!(r"\frac{{{left}}}{{{right}}}");
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_power(&mut self) -> Result<String, String> {
        let mut expression = self.parse_primary()?;
        loop {
            if self.consume_word("squared") {
                expression.push_str("^{2}");
            } else if self.consume_word("cubed") {
                expression.push_str("^{3}");
            } else {
                break;
            }
        }
        Ok(expression)
    }

    fn parse_primary(&mut self) -> Result<String, String> {
        if self.consume_phrase(&["integral", "of"]) {
            let integrand_start = self.position;
            let integrand = self.parse_sum()?;
            let variable = integration_variable(&self.words[integrand_start..self.position])
                .ok_or_else(|| {
                    String::from("an indefinite integral needs an unambiguous integration variable")
                })?;
            return Ok(format!(r"\int {integrand}\,\mathrm{{d}}{variable}"));
        }

        if self.consume_phrase(&["square", "root", "of"]) || self.consume_phrase(&["root", "of"]) {
            let radicand = self.parse_power()?;
            if self.next_word_is_operator() {
                return Err(String::from(
                    "ambiguous square-root scope; enter the radicand as one complete expression",
                ));
            }
            return Ok(format!(r"\sqrt{{{radicand}}}"));
        }

        let Some(word) = self.words.get(self.position) else {
            return Err(String::from("expected a mathematical value"));
        };
        self.position += 1;
        normalize_natural_atom(word)
    }

    fn consume_relation(&mut self) -> Option<&'static str> {
        let initial_position = self.position;
        self.consume_word("is");
        if self.consume_phrase(&["less", "than", "or", "equal", "to"]) {
            Some(r"\le{}")
        } else if self.consume_phrase(&["greater", "than", "or", "equal", "to"]) {
            Some(r"\ge{}")
        } else if self.consume_phrase(&["not", "equal", "to"]) {
            Some(r"\ne{}")
        } else if self.consume_phrase(&["less", "than"]) {
            Some("<")
        } else if self.consume_phrase(&["greater", "than"]) {
            Some(">")
        } else if self.consume_word("equals") {
            Some("=")
        } else {
            self.position = initial_position;
            None
        }
    }

    fn next_word_is_operator(&self) -> bool {
        self.words
            .get(self.position)
            .is_some_and(|word| is_operator_word(&word.to_ascii_lowercase()))
    }

    fn consume_word(&mut self, expected: &str) -> bool {
        if self
            .words
            .get(self.position)
            .is_some_and(|word| word.eq_ignore_ascii_case(expected))
        {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn consume_phrase(&mut self, expected: &[&str]) -> bool {
        if self.words[self.position..].len() < expected.len() {
            return false;
        }
        if self.words[self.position..self.position + expected.len()]
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.eq_ignore_ascii_case(expected))
        {
            self.position += expected.len();
            true
        } else {
            false
        }
    }
}

fn integration_variable(words: &[&str]) -> Option<String> {
    words.iter().find_map(|word| {
        let normalized = normalize_natural_atom(word).ok()?;
        let is_greek = GREEK_NAMES.contains(&word.to_ascii_lowercase().as_str());
        let alphabetic_count = word
            .chars()
            .filter(|character| character.is_alphabetic())
            .count();
        (is_greek || alphabetic_count == 1).then_some(normalized)
    })
}
