use crate::Anyhow;

enum ParseMode {
    FREE,
    EXPRESSION,
}

pub struct Templater {
    input: Vec<char>,
    current_char: usize,
}

/// Don't do this at home
impl Templater {
    pub fn new(input: &str) -> Templater {
        let input = input.chars().collect();
        Templater {
            input,
            current_char: 0,
        }
    }

    fn next_char(&mut self) -> Option<char> {
        let char = self.input.get(self.current_char).copied();
        self.current_char += 1;
        char
    }

    fn peek_char(&self) -> Option<char> {
        self.input.get(self.current_char).copied()
    }

    pub fn template<R, F>(mut self, op: F) -> Anyhow<String>
    where
        R: Into<String>,
        F: Fn(&str) -> Anyhow<R>,
    {
        let mut output = String::with_capacity(self.input.len());
        let mut expression = String::new();
        let mut parse_mode = ParseMode::FREE;

        while let Some(current_char) = self.next_char() {
            match parse_mode {
                ParseMode::FREE => {
                    if current_char == '{' && self.peek_char() == Some('{') {
                        self.next_char();
                        parse_mode = ParseMode::EXPRESSION;
                    } else {
                        output.push(current_char);
                    }
                }
                ParseMode::EXPRESSION => {
                    if current_char == '}' && self.peek_char() == Some('}') {
                        self.next_char();
                        parse_mode = ParseMode::FREE;
                        let template_value = op(&expression)?.into();
                        output.push_str(&template_value);
                        expression.clear();
                    } else {
                        expression.push(current_char);
                    }
                }
            }
        }

        Ok(output)
    }
}

#[test]
fn test_templater() {
    let templater = Templater::new("Hello, my name is {{expr}}");
    let template_result = templater.template(|expr| {
        if expr == "expr" {
            Ok("PASSED")
        } else {
            Ok("FAILED")
        }
    });

    assert_eq!(template_result.unwrap(), "Hello, my name is PASSED");
}
