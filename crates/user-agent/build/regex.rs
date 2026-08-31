pub(crate) fn rewrite_regex(regex: &str) -> String {
    let mut output = String::with_capacity(regex.len());
    let mut chars = regex.chars().peekable();
    let mut in_class = false;

    while let Some(character) = chars.next() {
        match character {
            '[' => {
                in_class = true;
                output.push(character);
            }
            ']' => {
                in_class = false;
                output.push(character);
            }
            '\\' => rewrite_escape(&mut output, chars.next()),
            '{' if !in_class => {
                let repetition = take_repetition(&mut chars);
                output.push_str(simplify_repetition(&repetition).unwrap_or(&repetition));
            }
            _ => output.push(character),
        }
    }
    output
}

fn rewrite_escape(output: &mut String, escaped: Option<char>) {
    match escaped {
        Some('d') => output.push_str("[0-9]"),
        Some('D') => output.push_str("[^0-9]"),
        Some('w') => output.push_str("[A-Za-z0-9_]"),
        Some('W') => output.push_str("[^A-Za-z0-9_]"),
        Some(character) => {
            output.push('\\');
            output.push(character);
        }
        None => output.push('\\'),
    }
}

fn take_repetition(chars: &mut std::iter::Peekable<impl Iterator<Item = char>>) -> String {
    let mut repetition = String::from("{");
    for character in chars.by_ref() {
        repetition.push(character);
        if character == '}' {
            break;
        }
    }
    repetition
}

fn simplify_repetition(repetition: &str) -> Option<&'static str> {
    let inner = repetition.strip_prefix('{')?.strip_suffix('}')?;
    let (minimum, maximum) = inner.split_once(',')?;
    if maximum.len() <= 2 || !maximum.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    match minimum {
        "0" => Some("*"),
        "1" => Some("+"),
        _ => None,
    }
}
