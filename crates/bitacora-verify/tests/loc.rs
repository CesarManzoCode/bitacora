use bitacora_verify::criteria::c26::{Lang, count_lines};

#[test]
fn rust_blank_and_line_comments_do_not_count() {
    let text = "\n   \n// line\n    // indented\n/// doc\n//! inner doc\n\t\n";
    assert_eq!(count_lines(text, Lang::Rust), 0);
}

#[test]
fn rust_code_with_trailing_comment_counts() {
    let text = "fn main() {\n    let x = 1; // trailing\n    let y = 2; /* trailing */\n}\n";
    assert_eq!(count_lines(text, Lang::Rust), 4);
}

#[test]
fn rust_multiline_block_comment_does_not_count() {
    let text = "/*\n * first\n * second\n */\nfn f() {}\n    /* one line */\n  /* starts\n  ends */\n";
    assert_eq!(count_lines(text, Lang::Rust), 1);
}

#[test]
fn rust_code_after_a_block_comment_closes_counts() {
    let text = "/* start\nstill comment */ let x = 1;\n/* a */ let y = 2;\n";
    assert_eq!(count_lines(text, Lang::Rust), 2);
}

#[test]
fn rust_nested_block_comment_ends_at_the_outer_close() {
    let text = "/* outer\n/* inner */\nstill outer\n*/\nfn g() {}\n";
    assert_eq!(count_lines(text, Lang::Rust), 1);
}

#[test]
fn rust_block_delimiter_inside_code_is_not_a_comment() {
    let text = "let glob = \"src/**/*.rs\";\nlet z = 3;\n";
    assert_eq!(count_lines(text, Lang::Rust), 2);
}

#[test]
fn shell_hash_lines_and_blank_lines_do_not_count() {
    let text = "#!/usr/bin/env bash\n# comment\n    # indented comment\n\nset -eu\necho hi # trailing\n";
    assert_eq!(count_lines(text, Lang::Hash), 2);
}

#[test]
fn shell_rust_comment_markers_are_code() {
    let text = "// not a shell comment\n/* neither */\n";
    assert_eq!(count_lines(text, Lang::Hash), 2);
}
