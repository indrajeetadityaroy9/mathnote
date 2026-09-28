use mathnote::document::{Block, Document, Inline, TextBuffer, map_offset_across_edit};
use mathnote::latex::{emit_latex, escape_prose};
use mathnote::note::{NoteLine, NoteSegment};

#[test]
fn mixed_notes_preserve_english_and_emit_canonical_mathematics() {
    let input = "Proof: the words square root stay readable, while $root of 81$ is mathematics.";
    let document = Document::parse(input).expect("mixed note parses");
    let generated = emit_latex(&document);

    assert!(
        generated
            .body()
            .contains("the words square root stay readable")
    );
    assert!(generated.body().contains(r"\(\sqrt{81}\)"));
    assert!(
        generated
            .source()
            .contains(r"\documentclass[a4paper,11pt]{article}")
    );
    assert!(!generated.source().contains(r"\usepackage{unicode-math}"));
    assert!(
        generated
            .source()
            .contains(r"\usepackage[margin=20mm]{geometry}")
    );
    assert!(!generated.body().contains("$root of 81$"));
}

#[test]
fn incomplete_math_language_remains_prose() {
    let input = "The words square root remain readable, and a plus sign remains prose.";
    let note = NoteLine::parse(input).expect("ordinary language");
    assert!(matches!(
        note.segments(),
        [NoteSegment::Text(text)] if text == input
    ));
}

#[test]
fn long_trigger_free_clause_remains_prose() {
    let input = vec!["ordinary"; 10_000].join(" ");
    let note = NoteLine::parse(&input).expect("long prose clause parses");

    assert!(matches!(
        note.segments(),
        [NoteSegment::Text(text)] if text == &input
    ));
}

#[test]
fn long_clause_still_finds_a_short_automatic_expression() {
    let prose = vec!["ordinary"; 10_000].join(" ");
    let input = format!("{prose} x + y");
    let note = NoteLine::parse(&input).expect("long mixed clause parses");

    assert!(matches!(
        note.segments(),
        [NoteSegment::Text(text), NoteSegment::Math(math)]
            if text == &format!("{prose} ")
                && math.source() == "x + y"
                && math.latex() == "x + y"
    ));
}

#[test]
fn automatic_length_bound_is_fail_closed_without_limiting_explicit_math() {
    let digits = "1".repeat(300);
    let expression = format!("root of {digits}");

    let automatic = NoteLine::parse(&expression).expect("oversized automatic input stays prose");
    assert!(matches!(
        automatic.segments(),
        [NoteSegment::Text(text)] if text == &expression
    ));

    let explicit_input = format!("${expression}$");
    let explicit =
        NoteLine::parse(&explicit_input).expect("explicit math bypasses heuristic bound");
    assert!(matches!(
        explicit.segments(),
        [NoteSegment::Math(math)]
            if math.source() == expression
                && math.latex() == format!(r"\sqrt{{{digits}}}")
    ));
}

#[test]
fn display_math_and_source_spans_form_one_document_model() {
    let input = "A proof begins here.\n\n$$\nx squared plus y squared equals z squared\n$$\n";
    let document = Document::parse(input).expect("display math document");
    let display = document
        .blocks()
        .iter()
        .find_map(|block| match block {
            Block::DisplayMath { latex, span, .. } => Some((latex, span)),
            _ => None,
        })
        .expect("display block");

    assert_eq!(display.0, r"x^{2}+y^{2}=z^{2}");
    assert_eq!(
        &input[display.1.range()],
        "x squared plus y squared equals z squared"
    );
    assert!(
        emit_latex(&document)
            .body()
            .contains("\\[\nx^{2}+y^{2}=z^{2}\n\\]")
    );
}

#[test]
fn malformed_explicit_math_fails_closed() {
    let inline_error = Document::parse("Proof: $root of 81").expect_err("unclosed inline math");
    assert!(inline_error.to_string().contains("unclosed inline"));

    let display_error = Document::parse("$$\nroot of 81\n").expect_err("unclosed display math");
    assert!(display_error.to_string().contains("unclosed display"));
}

#[test]
fn prose_is_escaped_and_cannot_inject_latex() {
    let escaped = escape_prose(r"Text with \input{secret}, 50%, x_y, and $5.");
    assert_eq!(
        escaped,
        r"Text with \textbackslash{}input\{secret\}, 50\%, x\_y, and \$5."
    );
}

#[test]
fn explicit_math_cannot_inject_raw_latex_commands() {
    for input in [r"$\input{file}$", r"$\a \b$"] {
        let error = Document::parse(input).expect_err("raw LaTeX must be rejected");
        assert!(
            error
                .to_string()
                .contains("not an unambiguous mathematical value")
        );
    }
}

#[test]
fn common_unicode_math_is_canonicalized_without_unicode_math() {
    let document = Document::parse("Let α satisfy $α squared is less than or equal to infinity$.")
        .expect("Unicode mathematics parses");
    let generated = emit_latex(&document);

    assert!(
        generated
            .body()
            .contains(r"Let \ensuremath{\alpha} satisfy")
    );
    assert!(generated.body().contains(r"\(\alpha^{2}\le{}\infty\)"));
}

#[test]
fn rope_editor_is_multiline_unicode_safe_and_clearable() {
    let mut buffer = TextBuffer::new("Proof: α\nroot of 81");
    buffer.move_home();
    buffer.insert_str("Given ");
    assert_eq!(buffer.text(), "Proof: α\nGiven root of 81");

    buffer.move_up();
    buffer.move_end();
    buffer.insert_char('.');
    assert_eq!(buffer.text(), "Proof: α.\nGiven root of 81");

    buffer.clear();
    assert!(buffer.is_empty());
    assert_eq!(buffer.cursor(), 0);
}

#[test]
fn inline_source_spans_remain_exact_after_hidden_delimiters() {
    let input = "before $root of 81$ after";
    let document = Document::parse(input).expect("document");
    let Block::Paragraph { inlines, .. } = &document.blocks()[0] else {
        panic!("paragraph expected");
    };
    let Inline::Math { span, .. } = &inlines[1] else {
        panic!("math expected");
    };
    let Inline::Text { source, .. } = &inlines[2] else {
        panic!("trailing text expected");
    };

    assert_eq!(&input[span.range()], "root of 81");
    assert_eq!(&input[source.range()], " after");
}

#[test]
fn compiled_lines_are_anchored_and_map_back_to_note_lines() {
    let latex = emit_latex(&Document::parse("alpha\nbeta\n\ngamma\n").expect("paragraphs parse"));
    let alpha = latex
        .output_line_for_source_byte(0)
        .expect("alpha is typeset");
    let beta = latex
        .output_line_for_source_byte(6)
        .expect("beta is typeset");
    let gamma = latex
        .output_line_for_source_byte(12)
        .expect("gamma is typeset");

    assert_eq!(beta, alpha + 1);
    assert_eq!(
        latex.source().lines().nth(beta - 1),
        Some(r"beta\hskip0pt{}")
    );
    assert!(!latex.body().contains("hskip"));
    assert_eq!(latex.source_byte_for_output_line(beta), Some(6));
    assert_eq!(latex.source_byte_for_output_line(gamma), Some(12));
    assert_eq!(latex.output_line_for_source_byte(11), None);
    assert!(!latex.is_content_line(beta + 1));
}

#[test]
fn content_lines_lists_exactly_the_content_lines() {
    let note = "alpha $x squared$\nbeta\n\n$$\nintegral of x\n$$\n\ngamma $root of 81$\ndelta\n";
    let latex = emit_latex(&Document::parse(note).expect("note parses"));
    let expected = (1..=latex.source().lines().count() + 1)
        .filter(|line| latex.is_content_line(*line))
        .collect::<Vec<_>>();

    assert!(expected.len() >= 5);
    assert_eq!(latex.content_lines(), expected);
}

#[test]
fn byte_cursor_lands_on_multibyte_character_boundaries() {
    let mut buffer = TextBuffer::new("α\nβ");
    buffer.set_cursor_byte(3);

    assert_eq!(buffer.cursor_line_column(), (1, 0));
    assert_eq!(buffer.cursor_byte(), 3);
}

#[test]
fn offsets_map_across_a_pending_edit() {
    let compiled = "alpha\nbeta\ngamma";
    let typed = "alpha\nbetaXY\ngamma";

    assert_eq!(map_offset_across_edit(typed, compiled, 2), 2);
    assert_eq!(map_offset_across_edit(typed, compiled, 11), 10);
    assert_eq!(map_offset_across_edit(typed, compiled, 12), 10);
    assert_eq!(map_offset_across_edit(typed, compiled, 14), 12);
    assert_eq!(map_offset_across_edit(compiled, typed, 12), 14);

    let greek = "λ\nα";
    let edited = "λ\nβα";
    assert_eq!(map_offset_across_edit(edited, greek, 5), 3);
}
